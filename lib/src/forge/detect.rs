use std::collections::HashMap;

use gix::remote::Direction;
use jj_lib::config::StackedConfig;
use jj_lib::git::{UnexpectedGitBackendError, get_git_repo};
use jj_lib::store::Store;
use thiserror::Error;
use url::Url;

use crate::protos::change_request::ForgeMeta;

use super::Forge;
use super::github::{GitHubForge, build_octocrab_for_github};

/// Supported forge type identifiers for interactive selection and config
/// persistence.
pub const FORGE_TYPES: &[&str] = &["github"];

/// Intermediate detection result before constructing a forge client.
#[derive(Clone, Debug, Eq, PartialEq)]
enum DetectedForge {
    GitHub {
        owner: String,
        repo: String,
        base_url: Option<Url>,
    },
}

/// A remote whose hostname did not match any known forge but whose URL path
/// could be parsed into an owner/repo pair.
#[derive(Clone, Debug)]
pub struct UnmatchedRemote {
    /// Git remote name (e.g. `"origin"`).
    pub remote_name: String,
    /// Hostname extracted from the remote URL.
    pub hostname: String,
    /// Repository owner parsed from the URL path.
    pub owner: String,
    /// Repository name parsed from the URL path.
    pub repo: String,
}

/// Result of [`detect_forges`], separating matched and unmatched remotes.
pub struct DetectionResult {
    /// Remotes successfully matched to a forge backend.
    pub forges: HashMap<String, Box<dyn Forge>>,
    /// Remotes with a parseable owner/repo but no recognised forge hostname.
    pub unmatched: Vec<UnmatchedRemote>,
}

impl DetectionResult {
    /// Resolve the forge instance that owns a change request.
    ///
    /// For cross-repo (fork) PRs the `target_repo` stored in [`ForgeMeta`]
    /// identifies where the CR lives (`"upstream-org/repo"`). This method
    /// looks through the detected forges for an instance whose
    /// [`Forge::repo_id`] matches that target.
    ///
    /// When `target_repo` is empty (same-repo CR) or no match is found,
    /// returns `None` so the caller can fall back to the default forge
    /// selection (e.g. by tracked remote).
    pub fn resolve_forge_for_meta(&self, meta: &ForgeMeta) -> Option<&dyn Forge> {
        let target = meta.target_repo()?;
        self.forges
            .values()
            .find(|f| f.repo_id() == target)
            .map(|f| f.as_ref())
    }
}

/// Errors that can occur when detecting forges from git remotes.
#[derive(Debug, Error)]
pub enum ForgeDetectionError {
    #[error("repository is not backed by git")]
    NotGitBacked(#[from] UnexpectedGitBackendError),

    #[error("failed to create {forge_type} forge for remote `{remote}`: {source}")]
    ForgeCreation {
        remote: String,
        forge_type: &'static str,
        source: Box<dyn std::error::Error>,
    },
}

/// Detect the forge type for each git remote and construct a client.
///
/// Returns a [`DetectionResult`] containing:
/// - `forges`: remote name → [`Forge`] for remotes matched to a known forge.
/// - `unmatched`: remotes with a parseable owner/repo but no recognised host.
///
/// GitHub Enterprise hosts are detected via the jj config key
/// `spice.forges.<hostname>.type = "github"`.
pub fn detect_forges(
    store: &Store,
    config: &StackedConfig,
) -> Result<DetectionResult, ForgeDetectionError> {
    let git_repo = get_git_repo(store)?;
    let mut forges = HashMap::new();
    let mut unmatched = Vec::new();

    for name in git_repo.remote_names() {
        let name_str = match std::str::from_utf8(name.as_ref()) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let remote = match git_repo.try_find_remote(name.as_ref()) {
            Some(Ok(r)) => r,
            _ => continue,
        };

        let url = match remote.url(Direction::Fetch) {
            Some(u) => u,
            None => continue,
        };

        let host = match url.host() {
            Some(h) => h,
            None => continue,
        };

        match detect_forge_from_host(host, url.path.as_ref(), config) {
            Some(detected) => {
                let forge = build_forge(name_str, detected)?;
                forges.insert(name_str.to_string(), forge);
            }
            None => {
                // Host unrecognised — record for potential interactive prompt
                // if the URL path yields a valid owner/repo.
                let path_str = match std::str::from_utf8(url.path.as_ref()) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if let Some((owner, repo)) = parse_owner_repo(path_str) {
                    unmatched.push(UnmatchedRemote {
                        remote_name: name_str.to_string(),
                        hostname: host.to_string(),
                        owner,
                        repo,
                    });
                }
            }
        }
    }

    Ok(DetectionResult { forges, unmatched })
}

/// Construct a [`Forge`] implementation from a [`DetectedForge`].
fn build_forge(
    remote: &str,
    detected: DetectedForge,
) -> Result<Box<dyn Forge>, ForgeDetectionError> {
    match detected {
        DetectedForge::GitHub {
            owner,
            repo,
            base_url,
        } => {
            let client = build_octocrab_for_github(base_url.as_ref()).map_err(|e| {
                ForgeDetectionError::ForgeCreation {
                    remote: remote.to_string(),
                    forge_type: "GitHub",
                    source: Box::new(e),
                }
            })?;
            let host = base_url
                .as_ref()
                .and_then(|u| u.host_str().map(String::from))
                .unwrap_or_else(|| "github.com".to_string());
            let graphql_url = graphql_endpoint_url(&host);
            Ok(Box::new(GitHubForge::new(
                client,
                owner,
                repo,
                host,
                graphql_url,
            )))
        }
    }
}

/// Construct a [`Forge`] from a user-selected forge type string and remote
/// metadata.
///
/// This is the public counterpart of [`build_forge`], used after interactive
/// prompts to construct a forge client for a previously-unmatched remote.
pub fn build_forge_for_type(
    remote: &str,
    forge_type: &str,
    owner: &str,
    repo: &str,
    hostname: &str,
) -> Result<Box<dyn Forge>, ForgeDetectionError> {
    match forge_type {
        "github" => {
            let base_url = if hostname == "github.com" {
                None
            } else {
                Some(ghe_api_url(hostname))
            };
            let client = build_octocrab_for_github(base_url.as_ref()).map_err(|e| {
                ForgeDetectionError::ForgeCreation {
                    remote: remote.to_string(),
                    forge_type: "GitHub",
                    source: Box::new(e),
                }
            })?;
            let graphql_url = graphql_endpoint_url(hostname);
            Ok(Box::new(GitHubForge::new(
                client,
                owner,
                repo,
                hostname,
                graphql_url,
            )))
        }
        other => Err(ForgeDetectionError::ForgeCreation {
            remote: remote.to_string(),
            forge_type: "unknown",
            source: format!("unsupported forge type: {other}").into(),
        }),
    }
}

/// Match a hostname + path to a detected forge type.
fn detect_forge_from_host(
    host: &str,
    path: &gix::bstr::BStr,
    config: &StackedConfig,
) -> Option<DetectedForge> {
    let path_str = std::str::from_utf8(path.as_ref()).ok()?;

    // Check if this host is a known GitHub instance.
    let (is_github, base_url) = if host == "github.com" {
        (true, None)
    } else {
        // Check jj config for GHE: spice.forges.<hostname>.type = "github"
        let key: &[&str] = &["spice", "forges", host, "type"];
        match config.get::<String>(key) {
            Ok(ref forge_type) if forge_type == "github" => (true, Some(ghe_api_url(host))),
            _ => (false, None),
        }
    };

    if is_github {
        let (owner, repo) = parse_owner_repo(path_str)?;
        return Some(DetectedForge::GitHub {
            owner,
            repo,
            base_url,
        });
    }

    None
}

/// Build the GitHub Enterprise API base URL for a given hostname.
fn ghe_api_url(hostname: &str) -> Url {
    Url::parse(&format!("https://{hostname}/api/v3")).expect("hostname should form a valid URL")
}

/// Build the full GraphQL endpoint URL for a given hostname.
///
/// Follows the same convention as git-spice: the API base is
/// `https://api.github.com` for github.com or `https://{host}/api` for GHE,
/// and the GraphQL endpoint is `{api_base}/graphql`.
pub(crate) fn graphql_endpoint_url(hostname: &str) -> String {
    if hostname == "github.com" {
        "https://api.github.com/graphql".to_string()
    } else {
        format!("https://{hostname}/api/graphql")
    }
}

/// Extract `(owner, repo)` from a URL path component.
///
/// Handles both formats:
/// - HTTPS: `/owner/repo.git` or `/owner/repo`
/// - SSH:   `owner/repo.git` or `owner/repo`
fn parse_owner_repo(path: &str) -> Option<(String, String)> {
    // Strip leading `/` if present (HTTPS URLs).
    let path = path.strip_prefix('/').unwrap_or(path);
    // Strip `.git` suffix if present.
    let path = path.strip_suffix(".git").unwrap_or(path);

    let mut parts = path.splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();

    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }

    Some((owner, repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_url_with_git_suffix() {
        assert_eq!(
            parse_owner_repo("/owner/repo.git"),
            Some(("owner".into(), "repo".into()))
        );
    }

    #[test]
    fn parse_https_url_without_git_suffix() {
        assert_eq!(
            parse_owner_repo("/owner/repo"),
            Some(("owner".into(), "repo".into()))
        );
    }

    #[test]
    fn parse_ssh_url_with_git_suffix() {
        assert_eq!(
            parse_owner_repo("owner/repo.git"),
            Some(("owner".into(), "repo".into()))
        );
    }

    #[test]
    fn parse_ssh_url_without_git_suffix() {
        assert_eq!(
            parse_owner_repo("owner/repo"),
            Some(("owner".into(), "repo".into()))
        );
    }

    #[test]
    fn parse_empty_owner_returns_none() {
        assert_eq!(parse_owner_repo("/repo"), None);
    }

    #[test]
    fn parse_no_repo_returns_none() {
        assert_eq!(parse_owner_repo("owner"), None);
    }

    #[test]
    fn parse_nested_path_strips_to_two_components() {
        // We only take the first two components; "extra/deep" becomes the repo name.
        // This should fail because repo contains '/'.
        assert_eq!(parse_owner_repo("/owner/repo/extra/deep"), None);
    }

    #[test]
    fn detect_github_com() {
        let config = StackedConfig::with_defaults();
        let result = detect_forge_from_host("github.com", "/acme/widget.git".into(), &config);
        assert_eq!(
            result,
            Some(DetectedForge::GitHub {
                owner: "acme".into(),
                repo: "widget".into(),
                base_url: None,
            })
        );
    }

    #[test]
    fn detect_unknown_host_returns_none() {
        let config = StackedConfig::with_defaults();
        let result = detect_forge_from_host("gitlab.com", "/acme/widget.git".into(), &config);
        assert_eq!(result, None);
    }

    #[test]
    fn detect_github_com_with_invalid_path_returns_none() {
        let config = StackedConfig::with_defaults();
        assert_eq!(
            detect_forge_from_host("github.com", "/".into(), &config),
            None
        );
    }

    #[test]
    fn detect_github_com_with_bare_owner_returns_none() {
        let config = StackedConfig::with_defaults();
        assert_eq!(
            detect_forge_from_host("github.com", "/owner".into(), &config),
            None
        );
    }

    // -- Additional parse_owner_repo edge cases --

    #[test]
    fn parse_empty_string_returns_none() {
        assert_eq!(parse_owner_repo(""), None);
    }

    #[test]
    fn parse_just_slash_returns_none() {
        assert_eq!(parse_owner_repo("/"), None);
    }

    #[test]
    fn parse_trailing_slash_returns_none() {
        // "/owner/repo/" -> splitn gives ("owner", "repo/"), and "repo/"
        // contains '/' so it is rejected.
        assert_eq!(parse_owner_repo("/owner/repo/"), None);
    }

    #[test]
    fn parse_dot_git_in_middle_is_not_stripped() {
        // ".git" suffix stripping only removes the last ".git"
        assert_eq!(
            parse_owner_repo("/owner/repo.git.bak"),
            Some(("owner".into(), "repo.git.bak".into()))
        );
    }

    // -- FORGE_TYPES --

    #[test]
    fn forge_types_contains_github() {
        assert!(FORGE_TYPES.contains(&"github"));
    }

    // -- GitHubForge construction --

    #[tokio::test]
    async fn github_forge_new_for_github_dot_com() {
        let client = octocrab::Octocrab::builder().build().unwrap();
        let graphql_url = graphql_endpoint_url("github.com");
        let _forge = GitHubForge::new(client, "acme", "widget", "github.com", graphql_url);
    }

    #[tokio::test]
    async fn github_forge_new_for_github_enterprise() {
        let client = octocrab::Octocrab::builder()
            .base_uri("https://git.corp.example.com/api/v3")
            .unwrap()
            .build()
            .unwrap();
        let graphql_url = graphql_endpoint_url("git.corp.example.com");
        let _forge = GitHubForge::new(
            client,
            "acme",
            "widget",
            "git.corp.example.com",
            graphql_url,
        );
    }

    #[test]
    fn graphql_url_for_github_dot_com() {
        assert_eq!(
            graphql_endpoint_url("github.com"),
            "https://api.github.com/graphql"
        );
    }

    #[test]
    fn graphql_url_for_github_enterprise() {
        assert_eq!(
            graphql_endpoint_url("git.corp.example.com"),
            "https://git.corp.example.com/api/graphql"
        );
    }

    // -- build_forge_for_type routing --

    #[test]
    fn build_forge_for_type_unsupported() {
        let result = build_forge_for_type("origin", "gitlab", "acme", "widget", "gitlab.com");
        let Err(err) = result else {
            panic!("expected an error for unsupported forge type");
        };
        assert!(
            err.to_string().contains("unsupported forge type"),
            "unexpected error: {err}"
        );
    }
}
