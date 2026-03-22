use std::fmt;
use std::process::Command;

use http_body_util::BodyExt;
use octocrab::Octocrab;
use octocrab::models::pulls::PullRequest;
use octocrab::models::{CommentId, IssueState};
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
    pub meta: GitHubMeta,
    pub host: String,
    pub title: String,
    pub body: Option<String>,
    pub status: ChangeStatus,
    pub url: String,
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

    fn link_label(&self) -> String {
        let repo = if self.meta.target_repo.is_empty() {
            format!("{}?", self.meta.number)
        } else {
            format!("{}#{}", self.meta.target_repo, self.meta.number)
        };
        format!("{}:{repo}", self.host)
    }
}

// ---------------------------------------------------------------------------
// GraphQL response types for batch PR fetching
// ---------------------------------------------------------------------------

/// Top-level GraphQL response wrapper.
#[derive(Debug, serde::Deserialize)]
struct GraphQlResponse {
    data: Option<serde_json::Value>,
    errors: Option<Vec<GraphQlError>>,
}

/// A single error entry from a GraphQL response.
#[derive(Debug, serde::Deserialize)]
struct GraphQlError {
    message: String,
}

/// A pull request node as returned by the GraphQL API.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlPullRequest {
    number: u64,
    title: String,
    body: Option<String>,
    /// One of `"OPEN"`, `"CLOSED"`, `"MERGED"`.
    state: String,
    is_draft: bool,
    url: String,
    head_ref_name: String,
    base_ref_name: String,
    head_repository: Option<GraphQlRepo>,
    base_repository: Option<GraphQlRepo>,
}

/// Repository identity inside a GraphQL pull request node.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlRepo {
    name_with_owner: String,
}

/// Convert a GraphQL pull request to a [`GitHubChangeRequest`].
fn graphql_pr_to_cr(pr: &GraphQlPullRequest, host: &str) -> GitHubChangeRequest {
    let status = match (pr.state.as_str(), pr.is_draft) {
        ("MERGED", _) => ChangeStatus::Merged,
        ("CLOSED", _) => ChangeStatus::Closed,
        ("OPEN", true) => ChangeStatus::Draft,
        _ => ChangeStatus::Open,
    };

    let meta = GitHubMeta {
        number: pr.number,
        source_branch: pr.head_ref_name.clone(),
        target_branch: pr.base_ref_name.clone(),
        source_repo: pr
            .head_repository
            .as_ref()
            .map(|r| r.name_with_owner.clone())
            .unwrap_or_default(),
        target_repo: pr
            .base_repository
            .as_ref()
            .map(|r| r.name_with_owner.clone())
            .unwrap_or_default(),
        graphql_id: String::new(),
        comment_id: None,
    };

    GitHubChangeRequest {
        meta,
        host: host.to_string(),
        title: pr.title.clone(),
        body: pr.body.clone(),
        status,
        url: pr.url.clone(),
    }
}

/// Build a GraphQL query that fetches multiple PRs in a single request.
///
/// Each PR is aliased as `pr0`, `pr1`, ... so the response can be
/// indexed by position.
fn build_graphql_batch_query(owner: &str, repo: &str, numbers: &[u64]) -> serde_json::Value {
    const FIELDS: &str = "\
        number title body state isDraft url \
        headRefName baseRefName \
        headRepository { nameWithOwner } \
        baseRepository { nameWithOwner }";

    let aliases: Vec<String> = numbers
        .iter()
        .enumerate()
        .map(|(i, n)| format!("pr{i}: pullRequest(number: {n}) {{ {FIELDS} }}"))
        .collect();

    let query = format!(
        "query {{ repository(owner: \"{owner}\", name: \"{repo}\") {{ {} }} }}",
        aliases.join(" ")
    );

    serde_json::json!({ "query": query })
}

/// GitHub / GitHub Enterprise forge backend backed by [`Octocrab`].
pub struct GitHubForge {
    client: Octocrab,
    owner: String,
    repo: String,
    /// Hostname used for display in link labels (e.g. `"github.com"`).
    host: String,
    /// Full URL for the GraphQL endpoint.
    ///
    /// Stored at construction time because octocrab's `graphql()` method
    /// resolves `/graphql` relative to `base_uri`, which produces the wrong
    /// path for GitHub Enterprise (`/api/v3/graphql` instead of the correct
    /// `/api/graphql`). We use `_post()` with this URL directly to bypass
    /// that issue.
    graphql_url: String,
}

/// Build an [`Octocrab`] client for GitHub using a resolved personal token.
///
/// Resolves the token via [`resolve_github_token`], then builds an
/// authenticated client. Pass `base_url` to target a GitHub Enterprise
/// instance; `None` uses the public GitHub API.
pub fn build_octocrab_for_github(base_url: Option<&Url>) -> Result<Octocrab, GitHubError> {
    let token = resolve_github_token().ok_or(GitHubError::MissingToken)?;
    let mut builder = Octocrab::builder().personal_token(token);
    if let Some(url) = base_url {
        builder = builder.base_uri(url.as_str())?;
    }
    Ok(builder.build()?)
}

impl GitHubForge {
    /// Create a new GitHub forge from a pre-built [`Octocrab`] client.
    ///
    /// The caller is responsible for constructing the client with the desired
    /// authentication strategy (personal token, OAuth, GitHub App, etc.).
    /// Use [`build_octocrab_for_github`] for the common personal-token path.
    ///
    /// `graphql_url` is the full URL for the GraphQL endpoint:
    /// - github.com: `"https://api.github.com/graphql"`
    /// - GHE: `"https://{host}/api/graphql"`
    pub fn new(
        client: Octocrab,
        owner: impl Into<String>,
        repo: impl Into<String>,
        host: impl Into<String>,
        graphql_url: impl Into<String>,
    ) -> Self {
        Self {
            client,
            owner: owner.into(),
            repo: repo.into(),
            host: host.into(),
            graphql_url: graphql_url.into(),
        }
    }

    /// Execute a GraphQL request and return the raw response bytes.
    ///
    /// Separated into its own function so that the single `.await` in
    /// `get_batch` only spans `Send`-safe types (no `Box<dyn ChangeRequest>`
    /// held across the await boundary).
    async fn execute_graphql(
        client: &Octocrab,
        uri: http::Uri,
        payload: &serde_json::Value,
    ) -> Result<Vec<u8>, String> {
        let response = client
            ._post(uri, Some(payload))
            .await
            .map_err(|e| format!("GraphQL request failed: {e}"))?;

        if !response.status().is_success() {
            return Err(format!("GraphQL HTTP error: {}", response.status()));
        }

        let collected = response
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("failed to read GraphQL response: {e}"))?;

        Ok(collected.to_bytes().to_vec())
    }

    /// Extract the [`GitHubMeta`] from a [`ForgeMeta`], returning an error if
    /// the metadata belongs to a different forge.
    pub(crate) fn extract_meta(meta: &ForgeMeta) -> Result<&GitHubMeta, GitHubError> {
        match &meta.forge {
            Some(ForgeOneof::Github(gh)) => Ok(gh),
            _ => Err(GitHubError::WrongForge),
        }
    }

    /// Format a branch reference for the GitHub API `head` parameter.
    ///
    /// GitHub requires `"owner:branch"` format for pull request head filters.
    /// `source_repo` carries the full [`Forge::repo_id`] (`"owner/repo"`);
    /// only the owner prefix is extracted. When `source_repo` is `None`,
    /// falls back to this forge's own owner (same-repo PR).
    fn format_head_ref(&self, branch: &str, source_repo: Option<&str>) -> String {
        let owner = source_repo
            .and_then(|r| r.split('/').next())
            .unwrap_or(&self.owner);
        format!("{owner}:{branch}")
    }
}

/// Build a [`GitHubChangeRequest`] from an octocrab [`PullRequest`] response.
fn github_cr_from_pr(pr: &PullRequest, host: &str) -> GitHubChangeRequest {
    let status = match (&pr.state, pr.merged_at.is_some(), pr.draft) {
        (_, true, _) => ChangeStatus::Merged,
        (Some(IssueState::Closed), false, _) => ChangeStatus::Closed,
        (Some(IssueState::Open), false, Some(true)) => ChangeStatus::Draft,
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
        comment_id: None,
    };

    GitHubChangeRequest {
        meta,
        host: host.to_string(),
        title: pr.title.clone().unwrap_or_default(),
        body: pr.body.clone(),
        status,
        url: pr
            .html_url
            .as_ref()
            .map(|u| u.to_string())
            .unwrap_or_default(),
    }
}

impl Forge for GitHubForge {
    fn repo_id(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn create<'a>(&'a self, params: CreateParams<'a>) -> BoxFuture<'a, ForgeResult> {
        Box::pin(async move {
            let pulls = self.client.pulls(&self.owner, &self.repo);
            let head = self.format_head_ref(params.source_branch, params.source_repo);

            let builder = pulls
                .create(params.title, head, params.target_branch)
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
        source_repo: Option<&'a str>,
    ) -> BoxFuture<'a, ForgeResults> {
        Box::pin(async move {
            let pulls = self.client.pulls(&self.owner, &self.repo);
            let mut builder = pulls
                .list()
                .state(octocrab::params::State::All)
                .per_page(100);

            if let Some(head) = source_branch {
                builder = builder.head(self.format_head_ref(head, source_repo));
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

    fn update_or_create_comment<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        comment: &'a str,
    ) -> BoxFuture<'a, Result<u64, Box<dyn std::error::Error>>> {
        Box::pin(async move {
            let gh = Self::extract_meta(meta)?;
            let issues_handler = self.client.issues(&self.owner, &self.repo);

            // If a comment ID is present, update the comment.
            if let Some(comment_id) = gh.comment_id {
                issues_handler
                    .update_comment(CommentId(comment_id), comment)
                    .await
                    .map_err(|e| e.to_string())?;
                return Ok(comment_id);
            }

            // Otherwise, create a new comment.
            let pr_comment = issues_handler
                .create_comment(gh.number, comment)
                .await
                .map_err(|e| e.to_string())?;
            Ok(pr_comment.id.0)
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

    fn get_batch<'a>(&'a self, metas: Vec<&'a ForgeMeta>) -> BoxFuture<'a, Vec<ForgeResult>> {
        Box::pin(async move {
            if metas.is_empty() {
                return Vec::new();
            }

            let len = metas.len();

            // Extract PR numbers, tracking which input positions are valid.
            // Use Send-safe types across await points.
            let mut numbers = Vec::with_capacity(len);
            let mut index_map: Vec<Option<usize>> = Vec::with_capacity(len);
            let mut early_errors: Vec<Option<String>> = vec![None; len];

            for (i, meta) in metas.iter().enumerate() {
                match Self::extract_meta(meta) {
                    Ok(gh) => {
                        index_map.push(Some(numbers.len()));
                        numbers.push(gh.number);
                    }
                    Err(e) => {
                        index_map.push(None);
                        early_errors[i] = Some(e.to_string());
                    }
                }
            }

            if numbers.is_empty() {
                return early_errors
                    .into_iter()
                    .map(|e| Err(e.unwrap_or_else(|| "not fetched".to_string()).into()))
                    .collect();
            }

            let payload = build_graphql_batch_query(&self.owner, &self.repo, &numbers);

            // POST directly to the stored GraphQL URL, bypassing octocrab's
            // BaseUri middleware which produces wrong paths for GHE.
            let uri: http::Uri = match self.graphql_url.parse() {
                Ok(u) => u,
                Err(e) => {
                    let err_msg = format!("invalid GraphQL URL: {e}");
                    return (0..len)
                        .map(|i| {
                            Err(early_errors[i]
                                .clone()
                                .unwrap_or_else(|| err_msg.clone())
                                .into())
                        })
                        .collect();
                }
            };

            // Single HTTP request — the only await in this function.
            let gql_body = match Self::execute_graphql(&self.client, uri, &payload).await {
                Ok(body) => body,
                Err(err_msg) => {
                    return (0..len)
                        .map(|i| {
                            Err(early_errors[i]
                                .clone()
                                .unwrap_or_else(|| err_msg.clone())
                                .into())
                        })
                        .collect();
                }
            };

            // Everything below is synchronous — safe to build ForgeResult values.
            let gql_response: GraphQlResponse = match serde_json::from_slice(&gql_body) {
                Ok(r) => r,
                Err(e) => {
                    let err_msg = format!("failed to parse GraphQL response: {e}");
                    return (0..len)
                        .map(|i| {
                            Err(early_errors[i]
                                .clone()
                                .unwrap_or_else(|| err_msg.clone())
                                .into())
                        })
                        .collect();
                }
            };

            let repo_data = gql_response.data.as_ref().and_then(|d| d.get("repository"));

            // Build final results — no await points after this.
            (0..len)
                .map(|i| {
                    if let Some(err) = &early_errors[i] {
                        return Err(err.clone().into());
                    }

                    let num_idx = index_map[i].unwrap();
                    let alias = format!("pr{num_idx}");
                    let pr_value = repo_data.and_then(|r| r.get(&alias));

                    match pr_value {
                        Some(serde_json::Value::Null) | None => {
                            let err_msg = gql_response
                                .errors
                                .as_ref()
                                .and_then(|errs| {
                                    errs.iter().find_map(|e| {
                                        if e.message.contains(&numbers[num_idx].to_string()) {
                                            Some(e.message.clone())
                                        } else {
                                            None
                                        }
                                    })
                                })
                                .unwrap_or_else(|| format!("PR #{} not found", numbers[num_idx]));
                            Err(err_msg.into())
                        }
                        Some(value) => {
                            match serde_json::from_value::<GraphQlPullRequest>(value.clone()) {
                                Ok(pr) => Ok(Box::new(graphql_pr_to_cr(&pr, &self.host))
                                    as Box<dyn ChangeRequest>),
                                Err(e) => {
                                    Err(format!("failed to parse PR #{}: {e}", numbers[num_idx])
                                        .into())
                                }
                            }
                        }
                    }
                })
                .collect()
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
                comment_id: None,
            },
            host: "github.com".into(),
            title: "Add feature X".into(),
            body: Some("Detailed description".into()),
            status: ChangeStatus::Open,
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
                comment_id: None,
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
        let graphql_url = format!("{uri}/graphql");
        GitHubForge::new(mock_octocrab(uri), OWNER, REPO, "github.com", graphql_url)
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
                comment_id: None,
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

        assert_eq!(cr.status(), ChangeStatus::Draft);
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
                source_repo: None,
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
        let results = forge.find(None, None, None).await.unwrap();

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
        let results = forge.find(None, None, None).await.unwrap();

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

    // -- GraphQL batch tests --

    /// Build a GraphQL PR response node matching the fields in our query.
    fn graphql_pr_node(number: u64, state: &str, is_draft: bool) -> serde_json::Value {
        json!({
            "number": number,
            "title": format!("PR #{number}"),
            "body": "A test pull request",
            "state": state,
            "isDraft": is_draft,
            "url": format!("https://github.com/{OWNER}/{REPO}/pull/{number}"),
            "headRefName": "feature-branch",
            "baseRefName": "main",
            "headRepository": { "nameWithOwner": format!("{OWNER}/{REPO}") },
            "baseRepository": { "nameWithOwner": format!("{OWNER}/{REPO}") },
        })
    }

    #[tokio::test]
    async fn get_batch_returns_multiple_prs() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "pr0": graphql_pr_node(42, "OPEN", false),
                        "pr1": graphql_pr_node(43, "MERGED", false),
                        "pr2": graphql_pr_node(44, "OPEN", true),
                    }
                }
            })))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let m1 = github_meta(42);
        let m2 = github_meta(43);
        let m3 = github_meta(44);
        let results = forge.get_batch(vec![&m1, &m2, &m3]).await;

        assert_eq!(results.len(), 3);
        let cr0 = results[0].as_ref().ok().unwrap();
        assert_eq!(cr0.id(), "42");
        assert_eq!(cr0.status(), ChangeStatus::Open);

        let cr1 = results[1].as_ref().ok().unwrap();
        assert_eq!(cr1.id(), "43");
        assert_eq!(cr1.status(), ChangeStatus::Merged);

        let cr2 = results[2].as_ref().ok().unwrap();
        assert_eq!(cr2.id(), "44");
        assert_eq!(cr2.status(), ChangeStatus::Draft);
    }

    #[tokio::test]
    async fn get_batch_handles_single_pr() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "pr0": graphql_pr_node(42, "OPEN", false),
                    }
                }
            })))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let m1 = github_meta(42);
        let results = forge.get_batch(vec![&m1]).await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().ok().unwrap().id(), "42");
    }

    #[tokio::test]
    async fn get_batch_empty_input() {
        let mock_server = MockServer::start().await;
        // No mocks mounted — should not make any HTTP calls.
        let forge = mock_forge(&mock_server.uri());
        let results = forge.get_batch(vec![]).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn get_batch_partial_failure_null_pr() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "pr0": graphql_pr_node(42, "OPEN", false),
                        "pr1": null,
                    }
                },
                "errors": [{
                    "message": "Could not resolve to a PullRequest with the number of 999."
                }]
            })))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let m1 = github_meta(42);
        let m2 = github_meta(999);
        let results = forge.get_batch(vec![&m1, &m2]).await;

        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert_eq!(results[0].as_ref().ok().unwrap().id(), "42");
        assert!(results[1].is_err());
        let err_msg = match &results[1] {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error"),
        };
        assert!(
            err_msg.contains("999"),
            "error should mention PR number: {err_msg}"
        );
    }

    #[tokio::test]
    async fn get_batch_graphql_http_error() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let m1 = github_meta(42);
        let results = forge.get_batch(vec![&m1]).await;

        assert_eq!(results.len(), 1);
        let err_msg = match &results[0] {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error"),
        };
        assert!(
            err_msg.contains("500"),
            "error should mention 500: {err_msg}"
        );
    }

    // -- update_or_create_comment tests --

    fn github_meta_with_comment(number: u64, comment_id: Option<u64>) -> ForgeMeta {
        ForgeMeta {
            forge: Some(ForgeOneof::Github(GitHubMeta {
                number,
                source_branch: "feature-branch".into(),
                target_branch: "main".into(),
                source_repo: format!("{OWNER}/{REPO}"),
                target_repo: format!("{OWNER}/{REPO}"),
                graphql_id: String::new(),
                comment_id,
            })),
        }
    }

    /// Minimal GitHub issue-comment JSON matching octocrab's `Comment` struct.
    fn comment_json(id: u64) -> serde_json::Value {
        json!({
            "id": id,
            "node_id": format!("IC_{id}"),
            "url": format!("https://api.github.com/repos/{OWNER}/{REPO}/issues/comments/{id}"),
            "html_url": format!("https://github.com/{OWNER}/{REPO}/issues/42#issuecomment-{id}"),
            "body": "stack trace",
            "author_association": "MEMBER",
            "user": {
                "login": "bot",
                "id": 1,
                "node_id": "U_bot",
                "avatar_url": "https://example.com/avatar",
                "gravatar_id": "",
                "url": "https://api.github.com/users/bot",
                "html_url": "https://github.com/bot",
                "followers_url": "https://api.github.com/users/bot/followers",
                "following_url": "https://api.github.com/users/bot/following{/other_user}",
                "gists_url": "https://api.github.com/users/bot/gists{/gist_id}",
                "starred_url": "https://api.github.com/users/bot/starred{/owner}{/repo}",
                "subscriptions_url": "https://api.github.com/users/bot/subscriptions",
                "organizations_url": "https://api.github.com/users/bot/orgs",
                "repos_url": "https://api.github.com/users/bot/repos",
                "events_url": "https://api.github.com/users/bot/events{/privacy}",
                "received_events_url": "https://api.github.com/users/bot/received_events",
                "type": "User",
                "site_admin": false
            },
            "created_at": "2025-01-01T00:00:00Z"
        })
    }

    #[tokio::test]
    async fn create_comment_when_no_comment_id() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/repos/{OWNER}/{REPO}/issues/42/comments")))
            .respond_with(ResponseTemplate::new(201).set_body_json(comment_json(555)))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let meta = github_meta_with_comment(42, None);
        let id = forge
            .update_or_create_comment(&meta, "stack trace")
            .await
            .unwrap();

        assert_eq!(id, 555);
    }

    #[tokio::test]
    async fn update_comment_when_comment_id_present() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/repos/{OWNER}/{REPO}/issues/comments/999")))
            .respond_with(ResponseTemplate::new(200).set_body_json(comment_json(999)))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let meta = github_meta_with_comment(42, Some(999));
        let id = forge
            .update_or_create_comment(&meta, "updated stack trace")
            .await
            .unwrap();

        assert_eq!(id, 999);
    }

    #[tokio::test]
    async fn update_or_create_comment_rejects_wrong_forge() {
        let mock_server = MockServer::start().await;
        let forge = mock_forge(&mock_server.uri());
        let meta = ForgeMeta { forge: None };

        let result = forge.update_or_create_comment(&meta, "body").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn create_comment_propagates_api_error() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/repos/{OWNER}/{REPO}/issues/42/comments")))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "message": "Not Found",
                "documentation_url": "https://docs.github.com/rest"
            })))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let meta = github_meta_with_comment(42, None);
        let result = forge.update_or_create_comment(&meta, "body").await;
        assert!(result.is_err());
    }

    #[test]
    fn build_graphql_batch_query_structure() {
        let query = build_graphql_batch_query("owner", "repo", &[42, 43]);
        let query_str = query["query"].as_str().unwrap();
        assert!(query_str.contains("repository(owner: \"owner\", name: \"repo\")"));
        assert!(query_str.contains("pr0: pullRequest(number: 42)"));
        assert!(query_str.contains("pr1: pullRequest(number: 43)"));
    }

    // -- repo_id() --

    #[tokio::test]
    async fn repo_id_returns_owner_slash_repo() {
        let forge = mock_forge("http://localhost");
        assert_eq!(forge.repo_id(), format!("{OWNER}/{REPO}"));
    }

    // -- create() with source_repo (cross-repo PR) --

    #[tokio::test]
    async fn create_cross_repo_pr_formats_head_as_owner_colon_branch() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
            .and(wiremock::matchers::body_partial_json(json!({
                "head": "fork-user:feature-branch"
            })))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(pr_json(77, "open", false, false)),
            )
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        // source_repo is the fork's full repo_id; create() extracts the owner.
        let cr = forge
            .create(CreateParams {
                source_branch: "feature-branch",
                target_branch: "main",
                title: "Cross-repo PR",
                body: None,
                is_draft: false,
                source_repo: Some("fork-user/some-repo"),
            })
            .await
            .unwrap();

        assert_eq!(cr.id(), "77");
    }

    #[tokio::test]
    async fn create_same_repo_pr_uses_owner_colon_branch() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
            .and(wiremock::matchers::body_partial_json(json!({
                "head": format!("{OWNER}:feature-branch")
            })))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(pr_json(88, "open", false, false)),
            )
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let cr = forge
            .create(CreateParams {
                source_branch: "feature-branch",
                target_branch: "main",
                title: "Same-repo PR",
                body: None,
                is_draft: false,
                source_repo: None,
            })
            .await
            .unwrap();

        assert_eq!(cr.id(), "88");
    }

    // -- find() with source_repo --

    #[tokio::test]
    async fn find_with_source_repo_uses_fork_owner_in_head_filter() {
        let mock_server = MockServer::start().await;
        // source_repo is "fork-user/some-repo"; owner "fork-user" is extracted.
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
            .and(wiremock::matchers::query_param(
                "head",
                "fork-user:feature-branch",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let results = forge
            .find(Some("feature-branch"), None, Some("fork-user/some-repo"))
            .await
            .unwrap();

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn find_without_source_repo_uses_forge_owner_in_head_filter() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
            .and(wiremock::matchers::query_param(
                "head",
                format!("{OWNER}:feature-branch"),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&mock_server)
            .await;

        let forge = mock_forge(&mock_server.uri());
        let results = forge
            .find(Some("feature-branch"), None, None)
            .await
            .unwrap();

        assert!(results.is_empty());
    }

    // -- format_head_ref tests --

    #[tokio::test]
    async fn format_head_ref_with_source_repo_extracts_owner() {
        let forge = mock_forge("http://unused");
        let head = forge.format_head_ref("feat", Some("fork-user/some-repo"));
        assert_eq!(head, "fork-user:feat");
    }

    #[tokio::test]
    async fn format_head_ref_without_source_repo_uses_forge_owner() {
        let forge = mock_forge("http://unused");
        let head = forge.format_head_ref("feat", None);
        assert_eq!(head, format!("{OWNER}:feat"));
    }
}
