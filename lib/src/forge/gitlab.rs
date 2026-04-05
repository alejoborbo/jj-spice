use std::fmt;

use reqwest::Client;
use serde::Deserialize;

use crate::forge::{
    BoxFuture, ChangeRequest, ChangeStatus, CreateParams, Forge, ForgeResult, ForgeResults,
};
use crate::protos::change_request::forge_meta::Forge as ForgeOneof;
use crate::protos::change_request::{ForgeMeta, GitLabMeta};

/// Resolve a GitLab personal access token from the `GITLAB_TOKEN` env var.
pub fn resolve_gitlab_token() -> Option<String> {
    std::env::var("GITLAB_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Errors specific to the GitLab forge backend.
#[derive(Debug)]
pub enum GitLabError {
    /// Error returned by the GitLab API via reqwest.
    Api {
        context: String,
        source: reqwest::Error,
    },
    /// The API returned a non-success status code.
    ApiStatus {
        status: u16,
        url: String,
        message: String,
    },
    /// The provided `ForgeMeta` is not a GitLab variant.
    WrongForge,
    /// No GitLab token could be resolved.
    MissingToken,
}

impl GitLabError {
    fn api(context: impl Into<String>) -> impl FnOnce(reqwest::Error) -> Self {
        let context = context.into();
        move |source| GitLabError::Api { context, source }
    }
}

impl fmt::Display for GitLabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitLabError::Api { context, source } => {
                write!(f, "GitLab API error ({context}): {source}")?;
                // Walk the error chain so the root cause is visible.
                let mut current: &dyn std::error::Error = source;
                while let Some(cause) = current.source() {
                    write!(f, "\n  caused by: {cause}")?;
                    current = cause;
                }
                Ok(())
            }
            GitLabError::ApiStatus {
                status,
                url,
                message,
            } => {
                write!(f, "GitLab API {status} on {url}: {message}")
            }
            GitLabError::WrongForge => {
                write!(f, "expected GitLab metadata, got a different forge")
            }
            GitLabError::MissingToken => {
                write!(f, "no GitLab token found (set the GITLAB_TOKEN env var)")
            }
        }
    }
}

impl std::error::Error for GitLabError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GitLabError::Api { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// GitLab API response types
// ---------------------------------------------------------------------------

/// Merge request as returned by the GitLab REST API.
#[derive(Debug, Deserialize)]
struct MergeRequestResponse {
    id: u64,
    iid: u64,
    title: String,
    description: Option<String>,
    /// One of `"opened"`, `"closed"`, `"locked"`, `"merged"`.
    state: String,
    #[serde(default)]
    draft: bool,
    web_url: String,
    source_branch: String,
    target_branch: String,
    source_project_id: u64,
    target_project_id: u64,
}

/// Note (comment) as returned by the GitLab REST API.
#[derive(Debug, Deserialize)]
struct NoteResponse {
    id: u64,
}

// ---------------------------------------------------------------------------
// GitLabChangeRequest
// ---------------------------------------------------------------------------

/// A GitLab Merge Request — combines stored identity with live API data.
#[derive(Debug)]
pub struct GitLabChangeRequest {
    pub meta: GitLabMeta,
    pub host: String,
    pub project: String,
    pub title: String,
    pub body: Option<String>,
    pub status: ChangeStatus,
    pub url: String,
}

impl ChangeRequest for GitLabChangeRequest {
    fn to_forge_meta(&self) -> ForgeMeta {
        ForgeMeta {
            forge: Some(ForgeOneof::Gitlab(self.meta.clone())),
        }
    }

    fn id(&self) -> String {
        format!("!{}", self.meta.iid)
    }

    fn status(&self) -> ChangeStatus {
        self.status
    }

    fn url(&self) -> &str {
        &self.url
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    fn link_label(&self) -> String {
        format!("{}:{}!{}", self.host, self.project, self.meta.iid)
    }
}

// ---------------------------------------------------------------------------
// GitLabForge
// ---------------------------------------------------------------------------

/// GitLab forge backend using the GitLab REST API v4 via [`reqwest`].
pub struct GitLabForge {
    client: Client,
    /// Hostname (e.g. `"gitlab.com"` or a self-hosted instance).
    host: String,
    /// Personal access token for the `PRIVATE-TOKEN` header.
    token: String,
    /// Full project path (e.g. `"group/project"`).
    project: String,
}

/// Build a [`reqwest::Client`] for GitLab using a resolved personal token.
///
/// Resolves the token via [`resolve_gitlab_token`], then returns the client
/// and token. The caller stores both in [`GitLabForge`].
pub fn build_gitlab_client() -> Result<(Client, String), GitLabError> {
    let token = resolve_gitlab_token().ok_or(GitLabError::MissingToken)?;
    let client = Client::new();
    Ok((client, token))
}

impl GitLabForge {
    /// Create a new GitLab forge from explicit parameters.
    pub fn new(
        client: Client,
        host: impl Into<String>,
        token: impl Into<String>,
        project: impl Into<String>,
    ) -> Self {
        Self {
            client,
            host: host.into(),
            token: token.into(),
            project: project.into(),
        }
    }

    /// Build the full API URL for a project-scoped endpoint.
    ///
    /// `suffix` is appended after `/projects/{encoded_project}/`.
    /// Pass an empty string for the project root endpoint.
    fn project_url(&self, suffix: &str) -> String {
        let encoded = urlencoded(&self.project);
        if suffix.is_empty() {
            format!("https://{}/api/v4/projects/{encoded}", self.host)
        } else {
            format!("https://{}/api/v4/projects/{encoded}/{suffix}", self.host)
        }
    }

    /// Extract the [`GitLabMeta`] from a [`ForgeMeta`], returning an error if
    /// the metadata belongs to a different forge.
    pub(crate) fn extract_meta(meta: &ForgeMeta) -> Result<&GitLabMeta, GitLabError> {
        match &meta.forge {
            Some(ForgeOneof::Gitlab(gl)) => Ok(gl),
            _ => Err(GitLabError::WrongForge),
        }
    }

    /// Check a response status and return an error for non-success codes.
    async fn check_response(resp: reqwest::Response) -> Result<reqwest::Response, GitLabError> {
        if resp.status().is_success() {
            Ok(resp)
        } else {
            let status = resp.status().as_u16();
            let url = resp.url().to_string();
            let message = resp.text().await.unwrap_or_default();
            Err(GitLabError::ApiStatus {
                status,
                url,
                message,
            })
        }
    }
}

/// Convert an API merge request response into a [`GitLabChangeRequest`].
fn gitlab_cr_from_mr(mr: &MergeRequestResponse, host: &str, project: &str) -> GitLabChangeRequest {
    let status = match (mr.state.as_str(), mr.draft) {
        ("merged", _) => ChangeStatus::Merged,
        ("closed", _) => ChangeStatus::Closed,
        ("locked", _) => ChangeStatus::Closed,
        ("opened", true) => ChangeStatus::Draft,
        _ => ChangeStatus::Open,
    };

    let meta = GitLabMeta {
        id: mr.id,
        iid: mr.iid,
        source_branch: mr.source_branch.clone(),
        target_branch: mr.target_branch.clone(),
        source_project_id: if mr.source_project_id != mr.target_project_id {
            Some(mr.source_project_id)
        } else {
            None
        },
        comment_id: None,
    };

    GitLabChangeRequest {
        meta,
        host: host.to_string(),
        project: project.to_string(),
        title: mr.title.clone(),
        body: mr.description.clone(),
        status,
        url: mr.web_url.clone(),
    }
}

impl Forge for GitLabForge {
    fn repo_id(&self) -> String {
        self.project.clone()
    }

    fn create<'a>(&'a self, params: CreateParams<'a>) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let url = self.project_url("merge_requests");

            let mut body = serde_json::json!({
                "source_branch": params.source_branch,
                "target_branch": params.target_branch,
                "title": if params.is_draft {
                    format!("Draft: {}", params.title)
                } else {
                    params.title.to_string()
                },
            });

            if let Some(desc) = params.body {
                body["description"] = serde_json::Value::String(desc.to_string());
            }

            let resp = self
                .client
                .post(&url)
                .header("PRIVATE-TOKEN", &self.token)
                .json(&body)
                .send()
                .await
                .map_err(GitLabError::api(format!("POST {url}")))?;

            let resp = Self::check_response(resp).await?;
            let mr: MergeRequestResponse = resp
                .json()
                .await
                .map_err(GitLabError::api("deserialize create MR response"))?;
            Ok(Box::new(gitlab_cr_from_mr(&mr, &self.host, &self.project))
                as Box<dyn ChangeRequest>)
        })
    }

    fn get<'a>(&'a self, meta: &'a ForgeMeta) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let gl = Self::extract_meta(meta)?;
            let url = self.project_url(&format!("merge_requests/{}", gl.iid));

            let resp = self
                .client
                .get(&url)
                .header("PRIVATE-TOKEN", &self.token)
                .send()
                .await
                .map_err(GitLabError::api(format!("GET {url}")))?;

            let resp = Self::check_response(resp).await?;
            let mr: MergeRequestResponse = resp
                .json()
                .await
                .map_err(GitLabError::api(format!("deserialize MR !{}", gl.iid)))?;
            Ok(Box::new(gitlab_cr_from_mr(&mr, &self.host, &self.project))
                as Box<dyn ChangeRequest>)
        })
    }

    fn find<'a>(
        &'a self,
        source_branch: Option<&'a str>,
        target_branch: Option<&'a str>,
        _source_repo: Option<&'a str>,
    ) -> BoxFuture<'a, ForgeResults> {
        Box::pin(async move {
            let url = self.project_url("merge_requests");

            let mut query: Vec<(&str, &str)> = vec![("state", "all"), ("per_page", "100")];
            if let Some(sb) = source_branch {
                query.push(("source_branch", sb));
            }
            if let Some(tb) = target_branch {
                query.push(("target_branch", tb));
            }
            let resp = self
                .client
                .get(&url)
                .header("PRIVATE-TOKEN", &self.token)
                .query(&query)
                .send()
                .await
                .map_err(GitLabError::api(format!("GET {url} (find MRs)")))?;

            let resp = Self::check_response(resp).await?;
            let mrs: Vec<MergeRequestResponse> = resp
                .json()
                .await
                .map_err(GitLabError::api("deserialize find MRs response"))?;

            Ok(mrs
                .iter()
                .map(|mr| {
                    Box::new(gitlab_cr_from_mr(mr, &self.host, &self.project))
                        as Box<dyn ChangeRequest>
                })
                .collect())
        })
    }

    fn update<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        title: Option<&'a str>,
        body: Option<&'a str>,
    ) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let gl = Self::extract_meta(meta)?;
            let url = self.project_url(&format!("merge_requests/{}", gl.iid));

            let mut payload = serde_json::Map::new();
            if let Some(t) = title {
                payload.insert("title".into(), serde_json::Value::String(t.to_string()));
            }
            if let Some(b) = body {
                payload.insert(
                    "description".into(),
                    serde_json::Value::String(b.to_string()),
                );
            }

            let resp = self
                .client
                .put(&url)
                .header("PRIVATE-TOKEN", &self.token)
                .json(&payload)
                .send()
                .await
                .map_err(GitLabError::api(format!("PUT {url} (update MR)")))?;

            let resp = Self::check_response(resp).await?;
            let mr: MergeRequestResponse = resp.json().await.map_err(GitLabError::api(format!(
                "deserialize update MR !{}",
                gl.iid
            )))?;
            Ok(Box::new(gitlab_cr_from_mr(&mr, &self.host, &self.project))
                as Box<dyn ChangeRequest>)
        })
    }

    fn update_base<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        base_branch: &'a str,
    ) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let gl = Self::extract_meta(meta)?;
            let url = self.project_url(&format!("merge_requests/{}", gl.iid));

            let payload = serde_json::json!({
                "target_branch": base_branch,
            });

            let resp = self
                .client
                .put(&url)
                .header("PRIVATE-TOKEN", &self.token)
                .json(&payload)
                .send()
                .await
                .map_err(GitLabError::api(format!("PUT {url} (update base)")))?;

            let resp = Self::check_response(resp).await?;
            let mr: MergeRequestResponse = resp.json().await.map_err(GitLabError::api(format!(
                "deserialize update_base MR !{}",
                gl.iid
            )))?;
            Ok(Box::new(gitlab_cr_from_mr(&mr, &self.host, &self.project))
                as Box<dyn ChangeRequest>)
        })
    }

    fn close<'a>(&'a self, meta: &'a ForgeMeta) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let gl = Self::extract_meta(meta)?;
            let url = self.project_url(&format!("merge_requests/{}", gl.iid));

            let payload = serde_json::json!({
                "state_event": "close",
            });

            let resp = self
                .client
                .put(&url)
                .header("PRIVATE-TOKEN", &self.token)
                .json(&payload)
                .send()
                .await
                .map_err(GitLabError::api(format!("PUT {url} (close MR)")))?;

            let resp = Self::check_response(resp).await?;
            let mr: MergeRequestResponse = resp.json().await.map_err(GitLabError::api(format!(
                "deserialize close MR !{}",
                gl.iid
            )))?;
            Ok(Box::new(gitlab_cr_from_mr(&mr, &self.host, &self.project))
                as Box<dyn ChangeRequest>)
        })
    }

    fn update_or_create_comment<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        comment: &'a str,
    ) -> BoxFuture<'a, Result<u64, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async move {
            let gl = Self::extract_meta(meta)?;

            // If a comment ID is present, update the existing note.
            if let Some(note_id) = gl.comment_id {
                let url = self.project_url(&format!("merge_requests/{}/notes/{note_id}", gl.iid));

                let resp = self
                    .client
                    .put(&url)
                    .header("PRIVATE-TOKEN", &self.token)
                    .query(&[("body", comment)])
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;

                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let msg = resp.text().await.unwrap_or_default();
                    return Err(format!("GitLab API {status}: {msg}").into());
                }
                return Ok(note_id);
            }

            // Otherwise, create a new note.
            let url = self.project_url(&format!("merge_requests/{}/notes", gl.iid));

            let resp = self
                .client
                .post(&url)
                .header("PRIVATE-TOKEN", &self.token)
                .query(&[("body", comment)])
                .send()
                .await
                .map_err(|e| e.to_string())?;

            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let msg = resp.text().await.unwrap_or_default();
                return Err(format!("GitLab API {status}: {msg}").into());
            }

            let note: NoteResponse = resp.json().await.map_err(|e| e.to_string())?;
            Ok(note.id)
        })
    }
}

/// Percent-encode a project path for use in GitLab API URLs.
///
/// GitLab expects `group%2Fproject` when the project path is used as a URL
/// segment.
fn urlencoded(s: &str) -> String {
    s.replace('/', "%2F")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoded_encodes_slash() {
        assert_eq!(urlencoded("group/project"), "group%2Fproject");
    }

    #[test]
    fn urlencoded_no_slash() {
        assert_eq!(urlencoded("project"), "project");
    }

    #[test]
    fn status_mapping_merged() {
        let mr = MergeRequestResponse {
            id: 1,
            iid: 10,
            title: "t".into(),
            description: None,
            state: "merged".into(),
            draft: false,
            web_url: "https://gitlab.com/g/p/-/merge_requests/10".into(),
            source_branch: "feat".into(),
            target_branch: "main".into(),
            source_project_id: 1,
            target_project_id: 1,
        };
        let cr = gitlab_cr_from_mr(&mr, "gitlab.com", "g/p");
        assert_eq!(cr.status, ChangeStatus::Merged);
    }

    #[test]
    fn status_mapping_draft() {
        let mr = MergeRequestResponse {
            id: 2,
            iid: 20,
            title: "Draft: wip".into(),
            description: None,
            state: "opened".into(),
            draft: true,
            web_url: "https://gitlab.com/g/p/-/merge_requests/20".into(),
            source_branch: "wip".into(),
            target_branch: "main".into(),
            source_project_id: 1,
            target_project_id: 1,
        };
        let cr = gitlab_cr_from_mr(&mr, "gitlab.com", "g/p");
        assert_eq!(cr.status, ChangeStatus::Draft);
    }

    #[test]
    fn status_mapping_open() {
        let mr = MergeRequestResponse {
            id: 3,
            iid: 30,
            title: "feat".into(),
            description: Some("desc".into()),
            state: "opened".into(),
            draft: false,
            web_url: "https://gitlab.com/g/p/-/merge_requests/30".into(),
            source_branch: "feat".into(),
            target_branch: "main".into(),
            source_project_id: 1,
            target_project_id: 1,
        };
        let cr = gitlab_cr_from_mr(&mr, "gitlab.com", "g/p");
        assert_eq!(cr.status, ChangeStatus::Open);
    }

    #[test]
    fn status_mapping_closed() {
        let mr = MergeRequestResponse {
            id: 4,
            iid: 40,
            title: "old".into(),
            description: None,
            state: "closed".into(),
            draft: false,
            web_url: "https://gitlab.com/g/p/-/merge_requests/40".into(),
            source_branch: "old".into(),
            target_branch: "main".into(),
            source_project_id: 1,
            target_project_id: 1,
        };
        let cr = gitlab_cr_from_mr(&mr, "gitlab.com", "g/p");
        assert_eq!(cr.status, ChangeStatus::Closed);
    }

    #[test]
    fn change_request_id_format() {
        let mr = MergeRequestResponse {
            id: 1,
            iid: 42,
            title: "t".into(),
            description: None,
            state: "opened".into(),
            draft: false,
            web_url: "https://gitlab.com/g/p/-/merge_requests/42".into(),
            source_branch: "feat".into(),
            target_branch: "main".into(),
            source_project_id: 1,
            target_project_id: 1,
        };
        let cr = gitlab_cr_from_mr(&mr, "gitlab.com", "g/p");
        assert_eq!(cr.id(), "!42");
    }

    #[test]
    fn change_request_link_label() {
        let mr = MergeRequestResponse {
            id: 1,
            iid: 42,
            title: "t".into(),
            description: None,
            state: "opened".into(),
            draft: false,
            web_url: "https://gitlab.com/g/p/-/merge_requests/42".into(),
            source_branch: "feat".into(),
            target_branch: "main".into(),
            source_project_id: 1,
            target_project_id: 1,
        };
        let cr = gitlab_cr_from_mr(&mr, "gitlab.com", "g/p");
        assert_eq!(cr.link_label(), "gitlab.com:g/p!42");
    }

    #[test]
    fn forge_meta_round_trip() {
        let mr = MergeRequestResponse {
            id: 100,
            iid: 7,
            title: "feat".into(),
            description: Some("body".into()),
            state: "opened".into(),
            draft: false,
            web_url: "https://gitlab.com/g/p/-/merge_requests/7".into(),
            source_branch: "feat".into(),
            target_branch: "main".into(),
            source_project_id: 1,
            target_project_id: 1,
        };
        let cr = gitlab_cr_from_mr(&mr, "gitlab.com", "g/p");
        let meta = cr.to_forge_meta();
        let gl = GitLabForge::extract_meta(&meta).unwrap();
        assert_eq!(gl.iid, 7);
        assert_eq!(gl.source_branch, "feat");
        assert_eq!(gl.target_branch, "main");
    }

    #[test]
    fn cross_repo_sets_source_project_id() {
        let mr = MergeRequestResponse {
            id: 100,
            iid: 7,
            title: "feat".into(),
            description: None,
            state: "opened".into(),
            draft: false,
            web_url: "url".into(),
            source_branch: "feat".into(),
            target_branch: "main".into(),
            source_project_id: 99,
            target_project_id: 1,
        };
        let cr = gitlab_cr_from_mr(&mr, "gitlab.com", "g/p");
        assert_eq!(cr.meta.source_project_id, Some(99));
    }

    #[test]
    fn same_repo_omits_source_project_id() {
        let mr = MergeRequestResponse {
            id: 100,
            iid: 7,
            title: "feat".into(),
            description: None,
            state: "opened".into(),
            draft: false,
            web_url: "url".into(),
            source_branch: "feat".into(),
            target_branch: "main".into(),
            source_project_id: 1,
            target_project_id: 1,
        };
        let cr = gitlab_cr_from_mr(&mr, "gitlab.com", "g/p");
        assert_eq!(cr.meta.source_project_id, None);
    }
}
