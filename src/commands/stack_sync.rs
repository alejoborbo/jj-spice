use std::collections::{HashMap, HashSet};
use std::io::Write;

use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::repo::Repo;

use crate::bookmark::Bookmark;
use crate::bookmark_graph::{BookmarkGraph, BookmarkNode};
use crate::commands::env::SpiceEnv;
use crate::forge::detect::{ForgeKind, detect_forges};
use crate::protos::change_request::ForgeMeta;
use crate::protos::change_request::forge_meta::Forge as ForgeOneof;
use crate::store::SpiceStore;
use crate::store::change_request::ChangeRequestStore;

/// Per-bookmark error (non-fatal, printed as a warning).
#[derive(Debug, thiserror::Error)]
enum BookmarkSyncError {
    #[error("no tracked remotes")]
    NoTrackedRemotes,
    #[error("no forge detected for any tracked remote")]
    NoForgeDetected,
    #[error("forge error: {0}")]
    Forge(#[from] Box<dyn std::error::Error>),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Discover and track change requests for each bookmark in the stack.
///
/// For each bookmark between trunk and the working copy, queries the detected
/// forges for existing change requests and persists their identity metadata
/// locally.
pub async fn run(
    env: &SpiceEnv,
    trunk: &CommitId,
    head: &CommitId,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let graph = BookmarkGraph::new(env.repo.as_ref(), trunk, head)?;
    let forge_map = detect_forges(env.repo.store(), env.config())?;
    let spice_store = SpiceStore::init_at(env.workspace.repo_path())?;
    let cr_store = ChangeRequestStore::new(&spice_store);
    let mut state = cr_store.load()?;

    let nodes: Vec<&BookmarkNode> = graph.iter_graph()?.collect();

    for node in &nodes {
        let bookmark = node.bookmark();
        let name = node.name();

        // Skip bookmarks that already have a tracked CR (unless --force).
        if !force && state.get(name).is_some() {
            writeln!(
                env.ui.warning_default(),
                "{name}: already tracked, skipping (use --force to re-sync)"
            )?;
            continue;
        }

        match sync_bookmark(&env.ui, bookmark, &forge_map).await {
            Ok(Some(meta)) => {
                state.set(name.to_string(), meta);
                writeln!(env.ui.status(), "{name}: tracked")?;
            }
            Ok(None) => {
                writeln!(env.ui.status(), "{name}: no change request found")?;
            }
            Err(e) => {
                writeln!(env.ui.warning_default(), "{name}: {e}")?;
            }
        }
    }

    cr_store.save(&state)?;
    Ok(())
}

/// Try to find and select a change request for a single bookmark.
///
/// Returns `Some(ForgeMeta)` if a CR was found and selected, `None` if no CR
/// exists on any forge.
async fn sync_bookmark(
    ui: &Ui,
    bookmark: &Bookmark,
    forge_map: &HashMap<String, ForgeKind>,
) -> Result<Option<ForgeMeta>, BookmarkSyncError> {
    let tracked_remotes: Vec<&str> = bookmark.tracked_remotes().collect();
    if tracked_remotes.is_empty() {
        return Err(BookmarkSyncError::NoTrackedRemotes);
    }

    // Collect all CRs across all tracked remotes.
    let mut all_crs: Vec<ForgeMeta> = Vec::new();

    let mut found_forge = false;
    for remote_name in &tracked_remotes {
        let forge_kind = match forge_map.get(*remote_name) {
            Some(k) => k,
            None => continue,
        };
        found_forge = true;

        let crs = crate::forge::find_change_requests(forge_kind, bookmark.name()).await?;
        all_crs.extend(crs);
    }

    if !found_forge {
        return Err(BookmarkSyncError::NoForgeDetected);
    }

    // Dedup by PR identity (multiple remotes may point to the same forge).
    let mut seen = HashSet::new();
    all_crs.retain(|cr| seen.insert(cr.clone()));

    match all_crs.len() {
        0 => Ok(None),
        1 => Ok(Some(all_crs.into_iter().next().unwrap())),
        _ => {
            // Multiple CRs found — prompt user to select one.
            let labels: Vec<String> = all_crs.iter().map(format_forge_meta).collect();

            let index = ui.prompt_choice(
                &format!(
                    "{}: found {} change requests, which should be tracked?",
                    bookmark.name(),
                    all_crs.len()
                ),
                &labels,
                Some(0),
            )?;

            Ok(Some(all_crs.into_iter().nth(index).unwrap()))
        }
    }
}

/// Format a `ForgeMeta` for display in a selection prompt.
fn format_forge_meta(meta: &ForgeMeta) -> String {
    match &meta.forge {
        Some(ForgeOneof::Github(gh)) => {
            format!(
                "GitHub PR #{} ({} → {})",
                gh.number, gh.source_branch, gh.target_branch
            )
        }
        None => "unknown forge".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protos::change_request::GitHubMeta;

    #[test]
    fn format_forge_meta_github_variant() {
        let meta = ForgeMeta {
            forge: Some(ForgeOneof::Github(GitHubMeta {
                number: 42,
                source_branch: "feat".into(),
                target_branch: "main".into(),
                source_repo: String::new(),
                target_repo: String::new(),
                graphql_id: String::new(),
            })),
        };
        assert_eq!(format_forge_meta(&meta), "GitHub PR #42 (feat → main)");
    }

    #[test]
    fn format_forge_meta_none_variant() {
        let meta = ForgeMeta { forge: None };
        assert_eq!(format_forge_meta(&meta), "unknown forge");
    }
}
