use reqwest::header::{AUTHORIZATION, HeaderValue, InvalidHeaderValue};
use reqwest::{Client, Method};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use thiserror::Error;
use url::Url;

use super::{
    BoxFuture, ChangeRequest, ChangeStatus, CreateParams, Forge, ForgeResult, ForgeResults,
};
use crate::protos::change_request::forge_meta::Forge as ForgeOneof;
use crate::protos::change_request::{ForgeMeta, GiteaMeta};

/// Errors from Gitea client setup and API requests.
#[derive(Debug, Error)]
pub enum GiteaError {
    #[error("no Gitea token found (set the GITEA_TOKEN environment variable)")]
    MissingToken,
    #[error("invalid GITEA_TOKEN header value")]
    InvalidToken(#[from] InvalidHeaderValue),
    #[error("invalid Gitea API URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("Gitea API error: {0}")]
    Api(#[from] reqwest::Error),
    #[error("expected Gitea pull request metadata")]
    WrongForge,
    #[error("invalid Gitea source repository: {0}")]
    InvalidSourceRepo(String),
}

#[derive(Deserialize)]
struct PullRequest {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    draft: bool,
    html_url: String,
    head: Branch,
    base: Branch,
}

#[derive(Deserialize)]
struct Branch {
    #[serde(rename = "ref")]
    name: String,
    repo: Option<Repository>,
}

#[derive(Deserialize)]
struct Repository {
    full_name: String,
}

impl Branch {
    fn repo(&self) -> Option<&str> {
        self.repo.as_ref().map(|repo| repo.full_name.as_str())
    }
}

#[derive(Deserialize)]
struct Comment {
    id: u64,
}

/// A Gitea pull request with stored identity and live review status.
struct GiteaChangeRequest {
    meta: GiteaMeta,
    label: String,
    pr: PullRequest,
}

impl ChangeRequest for GiteaChangeRequest {
    fn to_forge_meta(&self) -> ForgeMeta {
        ForgeMeta {
            forge: Some(ForgeOneof::Gitea(self.meta.clone())),
        }
    }

    fn id(&self) -> String {
        self.meta.number.to_string()
    }

    fn status(&self) -> ChangeStatus {
        match (self.pr.merged, self.pr.state.as_str(), self.pr.draft) {
            (true, _, _) => ChangeStatus::Merged,
            (_, "closed", _) => ChangeStatus::Closed,
            (_, _, true) => ChangeStatus::Draft,
            _ => ChangeStatus::Open,
        }
    }

    fn url(&self) -> &str {
        &self.pr.html_url
    }

    fn link_label(&self) -> String {
        self.label.clone()
    }

    fn title(&self) -> &str {
        &self.pr.title
    }

    fn body(&self) -> Option<&str> {
        self.pr.body.as_deref()
    }
}

/// Gitea forge backend for a single repository.
pub struct GiteaForge {
    client: Client,
    token: HeaderValue,
    repo_url: Url,
    repo: String,
    host: String,
}

impl GiteaForge {
    /// Connect to a Gitea host using the `GITEA_TOKEN` environment variable.
    pub(super) fn from_env(host: &str, repo: (&str, &str)) -> Result<Self, GiteaError> {
        let token = std::env::var("GITEA_TOKEN").map_err(|_| GiteaError::MissingToken)?;
        Self::new(Url::parse(&format!("https://{host}"))?, repo, &token)
    }

    fn new(mut base: Url, (owner, repo): (&str, &str), token: &str) -> Result<Self, GiteaError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(GiteaError::MissingToken);
        }
        let mut token = HeaderValue::from_str(&format!("token {token}"))?;
        token.set_sensitive(true);
        let host = base.host_str().unwrap_or_default().to_owned();
        base.path_segments_mut()
            .expect("Gitea base is an HTTP URL")
            .pop_if_empty()
            .extend(["api", "v1", "repos", owner, repo]);
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            token,
            repo_url: base,
            repo: format!("{owner}/{repo}"),
            host,
        })
    }

    fn request(&self, method: Method, path: &[&str]) -> reqwest::RequestBuilder {
        let mut url = self.repo_url.clone();
        url.path_segments_mut()
            .expect("Gitea base is an HTTP URL")
            .extend(path.iter().copied());
        self.client
            .request(method, url)
            .header(AUTHORIZATION, self.token.clone())
    }

    async fn send<T: DeserializeOwned>(request: reqwest::RequestBuilder) -> Result<T, GiteaError> {
        Ok(request.send().await?.error_for_status()?.json().await?)
    }

    fn meta(meta: &ForgeMeta) -> Result<&GiteaMeta, GiteaError> {
        match &meta.forge {
            Some(ForgeOneof::Gitea(meta)) => Ok(meta),
            _ => Err(GiteaError::WrongForge),
        }
    }

    fn change_request(&self, pr: PullRequest, comment_id: Option<u64>) -> Box<dyn ChangeRequest> {
        let meta = GiteaMeta {
            number: pr.number,
            source_branch: pr.head.name.clone(),
            target_branch: pr.base.name.clone(),
            source_repo: pr.head.repo().unwrap_or_default().to_owned(),
            target_repo: pr.base.repo().unwrap_or(&self.repo).to_owned(),
            comment_id,
        };
        let label = format!("{}:{}#{}", self.host, meta.target_repo, pr.number);
        Box::new(GiteaChangeRequest { meta, label, pr })
    }

    async fn list_page(
        &self,
        page: u64,
        target_branch: Option<&str>,
    ) -> Result<Vec<PullRequest>, GiteaError> {
        let mut request = self
            .request(Method::GET, &["pulls"])
            .query(&[("state", "all"), ("limit", "50")])
            .query(&[("page", page)]);
        if let Some(base) = target_branch {
            request = request.query(&[("base_branch", base)]);
        }
        Self::send(request).await
    }

    async fn edit(&self, meta: &ForgeMeta, payload: Value) -> ForgeResult {
        let meta = Self::meta(meta)?;
        let request = self.request(Method::PATCH, &["pulls", &meta.number.to_string()]);
        let pr = Self::send(request.json(&payload)).await?;
        Ok(self.change_request(pr, meta.comment_id))
    }
}

impl Forge for GiteaForge {
    fn repo_id(&self) -> String {
        self.repo.clone()
    }

    fn create<'a>(&'a self, params: CreateParams<'a>) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let head = match params.source_repo {
                Some(repo) if repo != self.repo => {
                    let (owner, _) = repo
                        .split_once('/')
                        .filter(|(owner, name)| !owner.is_empty() && !name.is_empty())
                        .ok_or_else(|| GiteaError::InvalidSourceRepo(repo.to_owned()))?;
                    format!("{owner}:{}", params.source_branch)
                }
                _ => params.source_branch.to_owned(),
            };
            let lower_title = params.title.to_ascii_lowercase();
            let title = if params.is_draft
                && !lower_title.starts_with("wip:")
                && !lower_title.starts_with("[wip]")
            {
                format!("WIP: {}", params.title)
            } else {
                params.title.to_owned()
            };
            let mut payload = json!({"head": head, "base": params.target_branch, "title": title});
            if let Some(body) = params.body {
                payload["body"] = json!(body);
            }
            let pr = Self::send(self.request(Method::POST, &["pulls"]).json(&payload)).await?;
            Ok(self.change_request(pr, None))
        })
    }

    fn get<'a>(&'a self, meta: &'a ForgeMeta) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let meta = Self::meta(meta)?;
            let request = self.request(Method::GET, &["pulls", &meta.number.to_string()]);
            let pr = Self::send(request).await?;
            Ok(self.change_request(pr, meta.comment_id))
        })
    }

    fn find<'a>(
        &'a self,
        source_branch: Option<&'a str>,
        target_branch: Option<&'a str>,
        source_repo: Option<&'a str>,
    ) -> BoxFuture<'a, ForgeResults> {
        Box::pin(async move {
            let source_repo = source_repo.unwrap_or(&self.repo);
            let mut matches = Vec::new();
            for page in 1.. {
                let prs = self.list_page(page, target_branch).await?;
                // An instance can cap the page size below the requested limit.
                if prs.is_empty() {
                    break;
                }
                matches.extend(
                    prs.into_iter()
                        .filter(|pr| {
                            source_branch.is_none_or(|branch| pr.head.name == branch)
                                && target_branch.is_none_or(|branch| pr.base.name == branch)
                                && pr
                                    .head
                                    .repo()
                                    .is_some_and(|repo| repo.eq_ignore_ascii_case(source_repo))
                        })
                        .map(|pr| self.change_request(pr, None)),
                );
            }
            Ok(matches)
        })
    }

    fn update<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        title: Option<&'a str>,
        body: Option<&'a str>,
    ) -> BoxFuture<'a, ForgeResult> {
        let payload = [("title", title), ("body", body)]
            .into_iter()
            .filter_map(|(key, value)| value.map(|v| (key.to_owned(), json!(v))))
            .collect::<Map<_, _>>();
        Box::pin(self.edit(meta, Value::Object(payload)))
    }

    fn update_base<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        base_branch: &'a str,
    ) -> BoxFuture<'a, ForgeResult> {
        Box::pin(self.edit(meta, json!({"base": base_branch})))
    }

    fn close<'a>(&'a self, meta: &'a ForgeMeta) -> BoxFuture<'a, ForgeResult> {
        Box::pin(self.edit(meta, json!({"state": "closed"})))
    }

    fn update_or_create_comment<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        comment: &'a str,
    ) -> BoxFuture<'a, Result<u64, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async move {
            let meta = Self::meta(meta)?;
            let request = match meta.comment_id {
                Some(id) => self.request(Method::PATCH, &["issues", "comments", &id.to_string()]),
                None => self.request(
                    Method::POST,
                    &["issues", &meta.number.to_string(), "comments"],
                ),
            };
            let comment: Comment = Self::send(request.json(&json!({"body": comment}))).await?;
            Ok(comment.id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const REPO_PATH: &str = "/api/v1/repos/acme/widget";

    fn forge(server: &MockServer) -> GiteaForge {
        GiteaForge::new(
            Url::parse(&server.uri()).unwrap(),
            ("acme", "widget"),
            "test-token",
        )
        .unwrap()
    }

    fn pr(number: u64, branch: &str, source_repo: &str) -> Value {
        json!({
            "number": number, "title": "Feature", "body": "Description", "state": "open",
            "html_url": format!("https://git.example.com/acme/widget/pulls/{number}"),
            "head": {"ref": branch, "repo": {"full_name": source_repo}},
            "base": {"ref": "main", "repo": {"full_name": "acme/widget"}}
        })
    }

    #[tokio::test]
    async fn creates_draft_from_fork_and_reuses_comment_id() {
        let server = MockServer::start().await;
        let forge = forge(&server);
        let mut draft = pr(7, "feat", "fork/widget");
        draft["draft"] = json!(true);
        Mock::given(method("POST"))
            .and(path(format!("{REPO_PATH}/pulls")))
            .and(header("authorization", "token test-token"))
            .and(body_json(json!({"head":"fork:feat", "base":"main", "title":"WIP: Feature", "body":"Description"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(&draft))
            .expect(1).mount(&server).await;
        let created = forge
            .create(CreateParams {
                source_branch: "feat",
                target_branch: "main",
                title: "Feature",
                body: Some("Description"),
                is_draft: true,
                source_repo: Some("fork/widget"),
            })
            .await
            .unwrap();
        assert_eq!(created.status(), ChangeStatus::Draft);
        let mut meta = created.to_forge_meta();
        for (verb, suffix) in [
            ("POST", "/issues/7/comments"),
            ("PATCH", "/issues/comments/22"),
        ] {
            Mock::given(method(verb))
                .and(path(format!("{REPO_PATH}{suffix}")))
                .and(body_json(json!({"body": "Stack graph"})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 22})))
                .expect(1)
                .mount(&server)
                .await;
            let id = forge
                .update_or_create_comment(&meta, "Stack graph")
                .await
                .unwrap();
            assert_eq!(id, 22);
            meta.set_comment_id(id);
        }

        let mut merged = draft;
        merged["state"] = json!("closed");
        merged["merged"] = json!(true);
        Mock::given(method("GET"))
            .and(path(format!("{REPO_PATH}/pulls/7")))
            .respond_with(ResponseTemplate::new(200).set_body_json(merged))
            .expect(1)
            .mount(&server)
            .await;
        let fetched = forge.get(&meta).await.unwrap();
        assert_eq!(fetched.status(), ChangeStatus::Merged);
        assert_eq!(fetched.to_forge_meta().comment_id(), Some(22));
    }

    #[tokio::test]
    async fn finds_exact_branch_and_repository_across_pages() {
        let server = MockServer::start().await;
        let forge = forge(&server);
        for (page, prs) in [
            (
                "1",
                vec![pr(1, "feat", "fork/widget"), pr(2, "other", "acme/widget")],
            ),
            ("2", vec![pr(3, "feat", "acme/widget")]),
            ("3", vec![]),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("{REPO_PATH}/pulls")))
                .and(query_param("state", "all"))
                .and(query_param("base_branch", "main"))
                .and(query_param("limit", "50"))
                .and(query_param("page", page))
                .respond_with(ResponseTemplate::new(200).set_body_json(prs))
                .expect(1)
                .mount(&server)
                .await;
        }
        let found = forge.find(Some("feat"), Some("main"), None).await.unwrap();
        assert_eq!(found.iter().map(|pr| pr.id()).collect::<Vec<_>>(), ["3"]);
    }

    #[tokio::test]
    async fn patches_title_base_and_state() {
        let server = MockServer::start().await;
        let forge = forge(&server);
        let meta = ForgeMeta {
            forge: Some(ForgeOneof::Gitea(GiteaMeta {
                number: 7,
                ..Default::default()
            })),
        };
        for (payload, response) in [
            (
                json!({"title":"New title","body":""}),
                pr(7, "feat", "acme/widget"),
            ),
            (json!({"base":"other"}), pr(7, "feat", "acme/widget")),
            (json!({"state":"closed"}), {
                let mut response = pr(7, "feat", "acme/widget");
                response["state"] = json!("closed");
                response
            }),
        ] {
            Mock::given(method("PATCH"))
                .and(path(format!("{REPO_PATH}/pulls/7")))
                .and(body_json(payload))
                .respond_with(ResponseTemplate::new(200).set_body_json(response))
                .expect(1)
                .mount(&server)
                .await;
        }
        forge
            .update(&meta, Some("New title"), Some(""))
            .await
            .unwrap();
        forge.update_base(&meta, "other").await.unwrap();
        assert_eq!(
            forge.close(&meta).await.unwrap().status(),
            ChangeStatus::Closed
        );
    }
}
