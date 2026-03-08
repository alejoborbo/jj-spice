use std::collections::HashMap;

use gix::remote::Direction;
use jj_lib::config::StackedConfig;
use jj_lib::git::{get_git_repo, UnexpectedGitBackendError};
use jj_lib::store::Store;
use thiserror::Error;

/// Detected forge type for a remote.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForgeKind {
    /// A GitHub or GitHub Enterprise remote.
    GitHub {
        /// Repository owner (user or organisation).
        owner: String,
        /// Repository name (without `.git` suffix).
        repo: String,
        /// Base URI for the API. `None` means public GitHub.
        base_uri: Option<String>,
    },
}

/// Errors that can occur when detecting forges from git remotes.
#[derive(Debug, Error)]
pub enum ForgeDetectionError {
    #[error("repository is not backed by git")]
    NotGitBacked(#[from] UnexpectedGitBackendError),
}

/// Detect the forge type for each git remote in the repository.
///
/// Returns a map from remote name → [`ForgeKind`]. Remotes whose URLs cannot
/// be parsed or matched to a known forge are silently skipped.
///
/// GitHub Enterprise hosts are detected via the jj config key
/// `spice.forges.<hostname>.type = "github"`.
pub fn detect_forges(
    store: &Store,
    config: &StackedConfig,
) -> Result<HashMap<String, ForgeKind>, ForgeDetectionError> {
    let git_repo = get_git_repo(store)?;
    let mut result = HashMap::new();

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

        if let Some(kind) = detect_forge_from_host(host, url.path.as_ref(), config) {
            result.insert(name_str.to_string(), kind);
        }
    }

    Ok(result)
}

/// Match a hostname + path to a forge type.
fn detect_forge_from_host(
    host: &str,
    path: &gix::bstr::BStr,
    config: &StackedConfig,
) -> Option<ForgeKind> {
    let path_str = std::str::from_utf8(path.as_ref()).ok()?;

    // Check if this host is a known GitHub instance.
    let (is_github, base_uri) = if host == "github.com" {
        (true, None)
    } else {
        // Check jj config for GHE: spice.forges.<hostname>.type = "github"
        let key: &[&str] = &["spice", "forges", host, "type"];
        match config.get::<String>(key) {
            Ok(ref forge_type) if forge_type == "github" => {
                (true, Some(format!("https://{host}/api/v3")))
            }
            _ => (false, None),
        }
    };

    if is_github {
        let (owner, repo) = parse_owner_repo(path_str)?;
        return Some(ForgeKind::GitHub {
            owner,
            repo,
            base_uri,
        });
    }

    None
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
            Some(ForgeKind::GitHub {
                owner: "acme".into(),
                repo: "widget".into(),
                base_uri: None,
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
}
