use std::fmt;
use std::process::Command;

use octocrab::Octocrab;
use octocrab::models::IssueState;
use octocrab::models::pulls::PullRequest;
use url::Url;

use crate::forge::{
    BoxFuture, ChangeRequest, ChangeStatus, CreateParams, Forge, ForgeResult, ForgeResults,
};
use crate::protos::change_request::forge_meta::Forge as ForgeOneof;
use crate::protos::change_request::{ForgeMeta, GitHubMeta};

/// Resolve a GitHub personal access token from the environment or `gh` CLI.
///
/// Lookup order:
/// 1. `GH_TOKEN` env var (what `gh` CLI itself respects)
/// 2. `GITHUB_TOKEN` env var (widely used in CI)
/// 3. `gh auth token` subprocess (uses `gh`'s stored credentials)
pub fn resolve_github_token() -> Option<String> {
    std::env::var("GH_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()
        .or_else(|| {
            Command::new("gh")
                .args(["auth", "token"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
}

/// Errors specific to the GitHub forge backend.
#[derive(Debug)]
pub enum GitHubError {
    /// Error returned by the GitHub API via octocrab.
    Api(octocrab::Error),
    /// The provided `ForgeMeta` is not a GitHub variant.
    WrongForge,
    /// No GitHub token could be resolved.
    MissingToken,
}

impl fmt::Display for GitHubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitHubError::Api(e) => write!(f, "GitHub API error: {e}"),
            GitHubError::WrongForge => write!(f, "expected GitHub metadata, got a different forge"),
            GitHubError::MissingToken => write!(
                f,
                "no GitHub token found (checked GH_TOKEN, GITHUB_TOKEN, gh auth token)"
            ),
        }
    }
}

impl std::error::Error for GitHubError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GitHubError::Api(e) => Some(e),
            _ => None,
        }
    }
}

impl From<octocrab::Error> for GitHubError {
    fn from(e: octocrab::Error) -> Self {
        GitHubError::Api(e)
    }
}

/// A GitHub Pull Request — combines stored identity with live API data.
#[derive(Debug)]
pub struct GitHubChangeRequest {
    pub(crate) meta: GitHubMeta,
    pub(crate) host: String,
    pub(crate) title: String,
    pub(crate) body: Option<String>,
    pub(crate) status: ChangeStatus,
    pub(crate) is_draft: bool,
    pub(crate) url: String,
}

impl ChangeRequest for GitHubChangeRequest {
    fn to_forge_meta(&self) -> ForgeMeta {
        ForgeMeta {
            forge: Some(ForgeOneof::Github(self.meta.clone())),
        }
    }

    fn id(&self) -> String {
        self.meta.number.to_string()
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

    fn is_draft(&self) -> bool {
        self.is_draft
    }

    fn link_label(&self) -> String {
        let repo = if self.meta.target_repo.is_empty() {
            format!("{}?", self.meta.number)
        } else {
            format!("{}#{}", self.meta.target_repo, self.meta.number)
        };
        format!("{}:{repo}", self.host)
    }
}

/// GitHub / GitHub Enterprise forge backend backed by [`Octocrab`].
pub struct GitHubForge {
    client: Octocrab,
    owner: String,
    repo: String,
    /// Hostname used for display in link labels (e.g. `"github.com"`).
    host: String,
}

impl GitHubForge {
    /// Create a new GitHub forge client.
    ///
    /// Resolves a personal access token via [`resolve_github_token`] and builds
    /// an HTTP client internally. Pass `base_url` to target a GitHub Enterprise
    /// instance; `None` uses the public GitHub API.
    pub fn new(
        owner: impl Into<String>,
        repo: impl Into<String>,
        base_url: Option<Url>,
    ) -> Result<Self, GitHubError> {
        let token = resolve_github_token().ok_or(GitHubError::MissingToken)?;
        let mut builder = Octocrab::builder().personal_token(token);
        if let Some(ref url) = base_url {
            builder = builder.base_uri(url.as_str())?;
        }
        let client = builder.build()?;
        let host = base_url
            .as_ref()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_else(|| "github.com".to_string());
        Ok(Self {
            client,
            owner: owner.into(),
            repo: repo.into(),
            host,
        })
    }

    /// Build a forge from a pre-constructed [`Octocrab`] client.
    ///
    /// Used in tests to inject a client pointed at a mock server.
    #[cfg(test)]
    fn with_client(client: Octocrab, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            client,
            owner: owner.into(),
            repo: repo.into(),
            host: "github.com".into(),
        }
    }

    /// Extract the [`GitHubMeta`] from a [`ForgeMeta`], returning an error if
    /// the metadata belongs to a different forge.
    pub(crate) fn extract_meta(meta: &ForgeMeta) -> Result<&GitHubMeta, GitHubError> {
        match &meta.forge {
            Some(ForgeOneof::Github(gh)) => Ok(gh),
            _ => Err(GitHubError::WrongForge),
        }
    }
}

/// Build a [`GitHubChangeRequest`] from an octocrab [`PullRequest`] response.
fn github_cr_from_pr(pr: &PullRequest, host: &str) -> GitHubChangeRequest {
    let status = match (&pr.state, pr.merged_at.is_some()) {
        (_, true) => ChangeStatus::Merged,
        (Some(IssueState::Closed), false) => ChangeStatus::Closed,
        _ => ChangeStatus::Open,
    };

    let meta = GitHubMeta {
        number: pr.number,
        source_branch: pr.head.ref_field.clone(),
        target_branch: pr.base.ref_field.clone(),
        source_repo: pr
            .head
            .repo
            .as_ref()
            .and_then(|r| r.full_name.clone())
            .unwrap_or_default(),
        target_repo: pr
            .base
            .repo
            .as_ref()
            .and_then(|r| r.full_name.clone())
            .unwrap_or_default(),
        graphql_id: String::new(),
    };

    GitHubChangeRequest {
        meta,
        host: host.to_string(),
        title: pr.title.clone().unwrap_or_default(),
        body: pr.body.clone(),
        status,
        is_draft: pr.draft.unwrap_or(false),
        url: pr
            .html_url
            .as_ref()
            .map(|u| u.to_string())
            .unwrap_or_default(),
    }
}

impl Forge for GitHubForge {
    fn create<'a>(&'a self, params: CreateParams<'a>) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let pulls = self.client.pulls(&self.owner, &self.repo);
            let builder = pulls
                .create(params.title, params.source_branch, params.target_branch)
                .draft(Some(params.is_draft))
                .body::<String>(params.body.map(String::from));

            let pr = builder.send().await.map_err(GitHubError::Api)?;
            Ok(Box::new(github_cr_from_pr(&pr, &self.host)) as Box<dyn ChangeRequest>)
        })
    }

    fn get<'a>(&'a self, meta: &'a ForgeMeta) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let gh = Self::extract_meta(meta)?;
            let pr = self
                .client
                .pulls(&self.owner, &self.repo)
                .get(gh.number)
                .await
                .map_err(GitHubError::Api)?;
            Ok(Box::new(github_cr_from_pr(&pr, &self.host)) as Box<dyn ChangeRequest>)
        })
    }

    fn find<'a>(
        &'a self,
        source_branch: Option<&'a str>,
        target_branch: Option<&'a str>,
    ) -> BoxFuture<'a, ForgeResults> {
        Box::pin(async move {
            let pulls = self.client.pulls(&self.owner, &self.repo);
            let mut builder = pulls
                .list()
                .state(octocrab::params::State::All)
                .per_page(100);

            if let Some(head) = source_branch {
                // GitHub requires "owner:branch" format for the head filter.
                builder = builder.head(format!("{}:{head}", self.owner));
            }
            if let Some(base) = target_branch {
                builder = builder.base(base);
            }

            let page = builder.send().await.map_err(GitHubError::Api)?;
            let all_prs = self
                .client
                .all_pages(page)
                .await
                .map_err(GitHubError::Api)?;

            Ok(all_prs
                .iter()
                .map(|pr| Box::new(github_cr_from_pr(pr, &self.host)) as Box<dyn ChangeRequest>)
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
            let gh = Self::extract_meta(meta)?;
            let pulls = self.client.pulls(&self.owner, &self.repo);
            let mut builder = pulls.update(gh.number);

            if let Some(title) = title {
                builder = builder.title(title);
            }
            if let Some(body) = body {
                builder = builder.body(body);
            }

            let pr = builder.send().await.map_err(GitHubError::Api)?;
            Ok(Box::new(github_cr_from_pr(&pr, &self.host)) as Box<dyn ChangeRequest>)
        })
    }

    fn update_base<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        base_branch: &'a str,
    ) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let gh = Self::extract_meta(meta)?;
            let pr = self
                .client
                .pulls(&self.owner, &self.repo)
                .update(gh.number)
                .base(base_branch)
                .send()
                .await
                .map_err(GitHubError::Api)?;
            Ok(Box::new(github_cr_from_pr(&pr, &self.host)) as Box<dyn ChangeRequest>)
        })
    }

    fn close<'a>(&'a self, meta: &'a ForgeMeta) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let gh = Self::extract_meta(meta)?;
            let pr = self
                .client
                .pulls(&self.owner, &self.repo)
                .update(gh.number)
                .state(octocrab::params::pulls::State::Closed)
                .send()
                .await
                .map_err(GitHubError::Api)?;
            Ok(Box::new(github_cr_from_pr(&pr, &self.host)) as Box<dyn ChangeRequest>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a sample [`GitHubChangeRequest`] for testing.
    fn sample_cr() -> GitHubChangeRequest {
        GitHubChangeRequest {
            meta: GitHubMeta {
                number: 42,
                source_branch: "feat-branch".into(),
                target_branch: "main".into(),
                source_repo: "owner/repo".into(),
                target_repo: "owner/repo".into(),
                graphql_id: "PR_abc123".into(),
            },
            host: "github.com".into(),
            title: "Add feature X".into(),
            body: Some("Detailed description".into()),
            status: ChangeStatus::Open,
            is_draft: false,
            url: "https://github.com/owner/repo/pull/42".into(),
        }
    }

    // -- ChangeRequest trait tests --

    #[test]
    fn to_forge_meta_produces_github_variant() {
        let cr = sample_cr();
        let meta = cr.to_forge_meta();

        match &meta.forge {
            Some(ForgeOneof::Github(gh)) => {
                assert_eq!(gh.number, 42);
                assert_eq!(gh.source_branch, "feat-branch");
                assert_eq!(gh.target_branch, "main");
                assert_eq!(gh.graphql_id, "PR_abc123");
            }
            _ => panic!("expected Github variant"),
        }
    }

    #[test]
    fn id_returns_pr_number_as_string() {
        assert_eq!(sample_cr().id(), "42");
    }

    #[test]
    fn status_returns_expected_value() {
        assert_eq!(sample_cr().status(), ChangeStatus::Open);

        let mut cr = sample_cr();
        cr.status = ChangeStatus::Merged;
        assert_eq!(cr.status(), ChangeStatus::Merged);

        cr.status = ChangeStatus::Closed;
        assert_eq!(cr.status(), ChangeStatus::Closed);
    }

    #[test]
    fn url_returns_html_url() {
        assert_eq!(sample_cr().url(), "https://github.com/owner/repo/pull/42");
    }

    #[test]
    fn title_returns_title() {
        assert_eq!(sample_cr().title(), "Add feature X");
    }

    #[test]
    fn body_returns_some_when_present() {
        assert_eq!(sample_cr().body(), Some("Detailed description"));
    }

    #[test]
    fn body_returns_none_when_absent() {
        let mut cr = sample_cr();
        cr.body = None;
        assert_eq!(cr.body(), None);
    }

    #[test]
    fn is_draft_returns_false_by_default() {
        assert!(!sample_cr().is_draft());
    }

    #[test]
    fn is_draft_returns_true_when_set() {
        let mut cr = sample_cr();
        cr.is_draft = true;
        assert!(cr.is_draft());
    }

    // -- extract_meta tests --

    #[test]
    fn extract_meta_ok_for_github_variant() {
        let meta = ForgeMeta {
            forge: Some(ForgeOneof::Github(GitHubMeta {
                number: 99,
                source_branch: "branch".into(),
                target_branch: "main".into(),
                source_repo: String::new(),
                target_repo: String::new(),
                graphql_id: String::new(),
            })),
        };

        let gh = GitHubForge::extract_meta(&meta).unwrap();
        assert_eq!(gh.number, 99);
        assert_eq!(gh.source_branch, "branch");
    }

    #[test]
    fn extract_meta_err_for_none_forge() {
        let meta = ForgeMeta { forge: None };
        let err = GitHubForge::extract_meta(&meta).unwrap_err();
        assert!(matches!(err, GitHubError::WrongForge));
    }

    // -- GitHubForge (Forge trait) tests with wiremock --

    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const OWNER: &str = "test-owner";
    const REPO: &str = "test-repo";

    /// Build an [`Octocrab`] client pointed at the mock server.
    fn mock_octocrab(uri: &str) -> Octocrab {
        Octocrab::builder().base_uri(uri).unwrap().build().unwrap()
    }

    /// Build a [`GitHubForge`] backed by the mock server.
    fn mock_forge(uri: &str) -> GitHubForge {
        GitHubForge::with_client(mock_octocrab(uri), OWNER, REPO)
    }

    /// Minimal GitHub PR JSON response with the fields our code reads.
    fn pr_json(number: u64, state: &str, draft: bool, merged: bool) -> serde_json::Value {
        let mut v = json!({
            "url": format!("https://api.github.com/repos/{OWNER}/{REPO}/pulls/{number}"),
            "id": number,
            "number": number,
            "state": state,
            "title": format!("PR #{number}"),
            "body": "A test pull request",
            "html_url": format!("https://github.com/{OWNER}/{REPO}/pull/{number}"),
            "draft": draft,
            "head": {
                "ref": "feature-branch",
                "sha": "abc1234abc1234abc1234abc1234abc1234abc123",
                "repo": {
                    "id": 1,
                    "name": REPO,
                    "url": format!("https://api.github.com/repos/{OWNER}/{REPO}"),
                    "full_name": format!("{OWNER}/{REPO}")
                }
            },
            "base": {
                "ref": "main",
                "sha": "def5678def5678def5678def5678def5678def567",
                "repo": {
                    "id": 1,
                    "name": REPO,
                    "url": format!("https://api.github.com/repos/{OWNER}/{REPO}"),
                    "full_name": format!("{OWNER}/{REPO}")
                }
            }
        });
        if merged {
            v["merged_at"] = json!("2025-01-01T00:00:00Z");
        }
        v
    }

    fn github_meta(number: u64) -> ForgeMeta {
        ForgeMeta {
            forge: Some(ForgeOneof::Github(GitHubMeta {
                number,
                source_branch: "feature-branch".into(),
                target_branch: "main".into(),
                source_repo: format!("{OWNER}/{REPO}"),
                target_repo: format!("{OWNER}/{REPO}"),
                graphql_id: String::new(),
            })),
        }
    }

    #[tokio::test]
    async fn forge_get_returns_open_pr() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls/42")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(pr_json(42, "open", false, false)),
            )
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let cr = forge.get(&github_meta(42)).await.unwrap();

        assert_eq!(cr.id(), "42");
        assert_eq!(cr.title(), "PR #42");
        assert_eq!(cr.body(), Some("A test pull request"));
        assert_eq!(cr.status(), ChangeStatus::Open);
        assert!(!cr.is_draft());
        assert!(cr.url().contains("/pull/42"));
    }

    #[tokio::test]
    async fn forge_get_returns_merged_pr() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls/10")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(pr_json(10, "closed", false, true)),
            )
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let cr = forge.get(&github_meta(10)).await.unwrap();

        assert_eq!(cr.status(), ChangeStatus::Merged);
    }

    #[tokio::test]
    async fn forge_get_returns_closed_pr() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls/11")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(pr_json(11, "closed", false, false)),
            )
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let cr = forge.get(&github_meta(11)).await.unwrap();

        assert_eq!(cr.status(), ChangeStatus::Closed);
    }

    #[tokio::test]
    async fn forge_get_returns_draft_pr() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls/7")))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr_json(7, "open", true, false)))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let cr = forge.get(&github_meta(7)).await.unwrap();

        assert!(cr.is_draft());
    }

    #[tokio::test]
    async fn forge_get_propagates_api_error() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls/999")))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "message": "Not Found",
                "documentation_url": "https://docs.github.com/rest"
            })))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let Err(err) = forge.get(&github_meta(999)).await else {
            panic!("expected API error");
        };

        assert!(err.downcast_ref::<GitHubError>().is_some());
    }

    #[tokio::test]
    async fn forge_create_sends_post_and_returns_cr() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(pr_json(55, "open", false, false)),
            )
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let cr = forge
            .create(CreateParams {
                source_branch: "feature-branch",
                target_branch: "main",
                title: "New PR",
                body: Some("body text"),
                is_draft: false,
            })
            .await
            .unwrap();

        assert_eq!(cr.id(), "55");
        assert_eq!(cr.status(), ChangeStatus::Open);
    }

    #[tokio::test]
    async fn forge_find_returns_matching_prs() {
        let mock_server = MockServer::start().await;
        let response = json!([
            pr_json(1, "open", false, false),
            pr_json(2, "closed", false, true)
        ]);
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let results = forge.find(None, None).await.unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id(), "1");
        assert_eq!(results[0].status(), ChangeStatus::Open);
        assert_eq!(results[1].id(), "2");
        assert_eq!(results[1].status(), ChangeStatus::Merged);
    }

    #[tokio::test]
    async fn forge_find_returns_empty_list() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let results = forge.find(None, None).await.unwrap();

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn forge_update_sends_patch_and_returns_cr() {
        let mock_server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls/42")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(pr_json(42, "open", false, false)),
            )
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let cr = forge
            .update(&github_meta(42), Some("New title"), Some("New body"))
            .await
            .unwrap();

        assert_eq!(cr.id(), "42");
    }

    #[tokio::test]
    async fn forge_close_sends_patch_with_closed_state() {
        let mock_server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls/42")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(pr_json(42, "closed", false, false)),
            )
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let cr = forge.close(&github_meta(42)).await.unwrap();

        assert_eq!(cr.status(), ChangeStatus::Closed);
    }

    #[tokio::test]
    async fn forge_get_rejects_wrong_forge_meta() {
        let meta = ForgeMeta { forge: None };
        let mock_server = MockServer::start().await;
        let forge = mock_forge(&mock_server.uri());

        let Err(err) = forge.get(&meta).await else {
            panic!("expected WrongForge error");
        };
        assert!(
            err.downcast_ref::<GitHubError>()
                .is_some_and(|e| matches!(e, GitHubError::WrongForge))
        );
    }

    // -- GitHubError Display tests --

    #[test]
    fn github_error_display_wrong_forge() {
        let err = GitHubError::WrongForge;
        assert_eq!(
            err.to_string(),
            "expected GitHub metadata, got a different forge"
        );
    }

    #[test]
    fn github_error_display_missing_token() {
        let err = GitHubError::MissingToken;
        assert_eq!(
            err.to_string(),
            "no GitHub token found (checked GH_TOKEN, GITHUB_TOKEN, gh auth token)"
        );
    }

    #[test]
    fn github_error_source_is_none_for_non_api_variants() {
        use std::error::Error;
        assert!(GitHubError::WrongForge.source().is_none());
        assert!(GitHubError::MissingToken.source().is_none());
    }

    // -- link_label tests --

    #[test]
    fn link_label_formats_github_dot_com() {
        let cr = sample_cr();
        assert_eq!(cr.link_label(), "github.com:owner/repo#42");
    }

    #[test]
    fn link_label_formats_ghe_host() {
        let mut cr = sample_cr();
        cr.host = "git.corp.example.com".into();
        assert_eq!(cr.link_label(), "git.corp.example.com:owner/repo#42");
    }

    #[test]
    fn link_label_fallback_when_target_repo_empty() {
        let mut cr = sample_cr();
        cr.meta.target_repo = String::new();
        assert_eq!(cr.link_label(), "github.com:42?");
    }
}
