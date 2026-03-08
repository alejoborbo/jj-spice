use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use jj_cli::ui::Ui;
use jj_lib::config::StackedConfig;
use jj_lib::repo::{ReadonlyRepo, Repo};

use crate::bookmark::Bookmark;
use crate::bookmark_graph::{BookmarkGraph, BookmarkGraphError};
use crate::forge::detect::{ForgeDetectionError, ForgeKind, detect_forges};
use crate::forge::github::{GitHubError, GitHubForge};
use crate::forge::{ChangeRequest, Forge};
use crate::protos::change_request::ForgeMeta;
use crate::store::SpiceStore;
use crate::store::change_request::ChangeRequestStore;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("bookmark graph: {0}")]
    BookmarkGraph(#[from] BookmarkGraphError),
    #[error("forge detection: {0}")]
    ForgeDetection(#[from] ForgeDetectionError),
    #[error("spice store: {0}")]
    SpiceStore(#[from] crate::store::SpiceStoreError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Per-bookmark error (non-fatal, printed as a warning).
#[derive(Debug, thiserror::Error)]
enum BookmarkSyncError {
    #[error("no tracked remotes")]
    NoTrackedRemotes,
    #[error("no forge detected for any tracked remote")]
    NoForgeDetected,
    #[error("GitHub API error: {0}")]
    GitHub(#[from] GitHubError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Run the `stack sync` operation.
///
/// For each bookmark in the stack (between trunk and working copy), discovers
/// existing change requests on the detected forges and persists their identity
/// metadata locally.
pub async fn run(
    ui: &Ui,
    repo: &Arc<ReadonlyRepo>,
    repo_path: &Path,
    graph: &BookmarkGraph,
    config: &StackedConfig,
    force: bool,
) -> Result<(), SyncError> {
    let forge_map = detect_forges(repo.store(), config)?;
    let spice_store = SpiceStore::init_at(repo_path)?;
    let cr_store = ChangeRequestStore::new(&spice_store);
    let mut state = cr_store.load()?;

    let bookmarks: Vec<&Bookmark> = graph.iter_graph()?.collect();

    for bookmark in &bookmarks {
        let name = bookmark.name();

        // Skip bookmarks that already have a tracked CR (unless --force).
        if !force && state.get(name).is_some() {
            writeln!(
                ui.warning_default(),
                "{name}: already tracked, skipping (use --force to re-sync)"
            )?;
            continue;
        }

        match sync_bookmark(ui, bookmark, &forge_map).await {
            Ok(Some(meta)) => {
                state.set(name.to_string(), meta);
                writeln!(ui.status(), "{name}: tracked")?;
            }
            Ok(None) => {
                writeln!(ui.status(), "{name}: no change request found")?;
            }
            Err(e) => {
                writeln!(ui.warning_default(), "{name}: {e}")?;
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

        match forge_kind {
            ForgeKind::GitHub {
                owner,
                repo,
                base_uri,
            } => {
                let forge = GitHubForge::new(owner, repo, base_uri.as_deref())?;
                let crs = forge.find(Some(bookmark.name()), None).await?;
                for cr in &crs {
                    all_crs.push(cr.to_forge_meta());
                }
            }
        }
    }

    if !found_forge {
        return Err(BookmarkSyncError::NoForgeDetected);
    }

    // Dedup by PR identity.
    all_crs.dedup();

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
    use crate::protos::change_request::forge_meta::Forge;
    match &meta.forge {
        Some(Forge::Github(gh)) => {
            format!(
                "GitHub PR #{} ({} → {})",
                gh.number, gh.source_branch, gh.target_branch
            )
        }
        None => "unknown forge".to_string(),
    }
}
