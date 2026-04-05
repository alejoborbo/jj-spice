//! Shared logic for cleaning stale and inactive CR entries from the store.
//!
//! Used by `stack clean`, `stack untrack --all-inactive`, and the auto-clean
//! passes in `stack submit` and `stack sync`.

use std::collections::HashSet;

use crate::forge::{ChangeStatus, Forge};
use crate::protos::change_request::{ChangeRequests, ForgeMeta};

/// A single entry that was (or will be) removed during cleaning.
#[derive(Debug, Clone)]
pub struct CleanedEntry {
    /// The bookmark name that was removed.
    pub bookmark: String,
    /// The forge metadata that was stored for this bookmark.
    pub meta: ForgeMeta,
    /// Why this entry was removed.
    pub reason: CleanReason,
}

/// Why a CR entry was removed during cleaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanReason {
    /// The bookmark no longer exists in the local repository.
    Stale,
    /// The CR was closed without merging on the forge.
    Closed,
    /// The CR was merged and the bookmark no longer exists locally.
    Merged,
}

impl CleanReason {
    /// Whether this reason counts as "inactive" (closed or merged).
    pub fn is_inactive(self) -> bool {
        matches!(self, CleanReason::Closed | CleanReason::Merged)
    }
}

impl std::fmt::Display for CleanReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CleanReason::Stale => write!(f, "bookmark no longer exists"),
            CleanReason::Closed => write!(f, "change request is closed"),
            CleanReason::Merged => write!(f, "change request is merged and bookmark removed"),
        }
    }
}

/// Result of a clean operation.
#[derive(Debug, Clone, Default)]
pub struct CleanResult {
    /// Entries that were identified for removal.
    pub entries: Vec<CleanedEntry>,
}

impl CleanResult {
    /// Number of stale entries removed.
    pub fn stale_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.reason == CleanReason::Stale)
            .count()
    }

    /// Number of inactive (closed or merged) entries removed.
    pub fn inactive_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.reason.is_inactive())
            .count()
    }

    /// Total number of entries removed.
    pub fn total(&self) -> usize {
        self.entries.len()
    }
}

/// Identify stale entries: bookmarks that no longer exist in the repo.
///
/// Returns the bookmark names that should be removed.
pub fn find_stale_entries(
    state: &ChangeRequests,
    local_bookmarks: &HashSet<String>,
) -> Vec<CleanedEntry> {
    state
        .iter()
        .filter(|(name, _)| !local_bookmarks.contains(name.as_str()))
        .map(|(name, meta)| CleanedEntry {
            bookmark: name.clone(),
            meta: meta.clone(),
            reason: CleanReason::Stale,
        })
        .collect()
}

/// Check whether a single CR should be cleaned based on forge status.
///
/// A CR is cleanable if it is closed (always) or merged and the bookmark
/// no longer exists locally. Returns `None` if the CR should be kept or
/// if the forge query fails.
pub async fn check_cleanable(
    forge: &dyn Forge,
    bookmark: &str,
    meta: &ForgeMeta,
    bookmark_exists: bool,
) -> Option<CleanedEntry> {
    let cr = forge.get(meta).await.ok()?;
    match cr.status() {
        ChangeStatus::Closed => Some(CleanedEntry {
            bookmark: bookmark.to_string(),
            meta: meta.clone(),
            reason: CleanReason::Closed,
        }),
        ChangeStatus::Merged if !bookmark_exists => Some(CleanedEntry {
            bookmark: bookmark.to_string(),
            meta: meta.clone(),
            reason: CleanReason::Merged,
        }),
        _ => None,
    }
}

/// Check the forge status of a single locally-tracked CR.
///
/// Returns the live [`ChangeStatus`] if the forge query succeeds, `None`
/// on error (callers should treat this as "unknown, keep the entry").
pub async fn check_status(forge: &dyn Forge, meta: &ForgeMeta) -> Option<ChangeStatus> {
    forge.get(meta).await.ok().map(|cr| cr.status())
}

/// Identify cleanable entries by querying the forge for live status.
///
/// Closed CRs are always flagged for removal. Merged CRs are only flagged
/// when the bookmark no longer exists locally. Entries whose forge queries
/// fail are silently kept (not flagged for removal).
pub async fn find_inactive_entries(
    state: &ChangeRequests,
    forge: &dyn Forge,
    local_bookmarks: &HashSet<String>,
) -> Vec<CleanedEntry> {
    let entries: Vec<_> = state.iter().collect();
    let metas: Vec<&ForgeMeta> = entries.iter().map(|(_, m)| *m).collect();

    let results = forge.get_batch(metas).await;
    let mut cleanable = Vec::new();

    for (result, (name, meta)) in results.into_iter().zip(entries) {
        let Ok(cr) = result else { continue };
        let bookmark_exists = local_bookmarks.contains(name.as_str());
        match cr.status() {
            ChangeStatus::Closed => {
                cleanable.push(CleanedEntry {
                    bookmark: name.clone(),
                    meta: meta.clone(),
                    reason: CleanReason::Closed,
                });
            }
            ChangeStatus::Merged if !bookmark_exists => {
                cleanable.push(CleanedEntry {
                    bookmark: name.clone(),
                    meta: meta.clone(),
                    reason: CleanReason::Merged,
                });
            }
            _ => {}
        }
    }

    cleanable
}

/// Run a full clean: find stale + inactive entries and return them.
///
/// Does NOT mutate `state` — call [`apply_clean`] to actually remove entries.
pub async fn identify_cleanable(
    state: &ChangeRequests,
    local_bookmarks: &HashSet<String>,
    forge: &dyn Forge,
) -> CleanResult {
    let mut entries = find_stale_entries(state, local_bookmarks);

    // Only query forge for entries that aren't already flagged as stale.
    let stale_names: HashSet<&str> = entries.iter().map(|e| e.bookmark.as_str()).collect();
    let non_stale: ChangeRequests = {
        let mut filtered = ChangeRequests::default();
        for (name, meta) in state.iter() {
            if !stale_names.contains(name.as_str()) {
                filtered.set(name.clone(), meta.clone());
            }
        }
        filtered
    };

    let inactive = find_inactive_entries(&non_stale, forge, local_bookmarks).await;
    entries.extend(inactive);

    CleanResult { entries }
}

/// Remove all entries listed in `result` from `state`.
pub fn apply_clean(state: &mut ChangeRequests, result: &CleanResult) {
    for entry in &result.entries {
        state.remove(&entry.bookmark);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::protos::change_request::GitHubMeta;
    use crate::protos::change_request::forge_meta::Forge as ForgeOneof;

    fn sample_meta(number: u64) -> ForgeMeta {
        ForgeMeta {
            forge: Some(ForgeOneof::Github(GitHubMeta {
                number,
                source_branch: "feat".into(),
                target_branch: "main".into(),
                source_repo: "owner/repo".into(),
                target_repo: "owner/repo".into(),
                graphql_id: String::new(),
                comment_id: None,
            })),
        }
    }

    #[test]
    fn find_stale_detects_missing_bookmarks() {
        let mut state = ChangeRequests::default();
        state.set("exists".into(), sample_meta(1));
        state.set("gone".into(), sample_meta(2));

        let local = HashSet::from(["exists".to_string()]);
        let stale = find_stale_entries(&state, &local);

        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].bookmark, "gone");
        assert_eq!(stale[0].reason, CleanReason::Stale);
    }

    #[test]
    fn find_stale_empty_when_all_present() {
        let mut state = ChangeRequests::default();
        state.set("a".into(), sample_meta(1));

        let local = HashSet::from(["a".to_string()]);
        let stale = find_stale_entries(&state, &local);

        assert!(stale.is_empty());
    }

    #[test]
    fn apply_clean_removes_entries() {
        let mut state = ChangeRequests::default();
        state.set("a".into(), sample_meta(1));
        state.set("b".into(), sample_meta(2));
        state.set("c".into(), sample_meta(3));

        let result = CleanResult {
            entries: vec![
                CleanedEntry {
                    bookmark: "a".into(),
                    meta: sample_meta(1),
                    reason: CleanReason::Stale,
                },
                CleanedEntry {
                    bookmark: "c".into(),
                    meta: sample_meta(3),
                    reason: CleanReason::Closed,
                },
            ],
        };

        apply_clean(&mut state, &result);
        assert!(state.get("a").is_none());
        assert!(state.get("b").is_some());
        assert!(state.get("c").is_none());
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn clean_result_counts() {
        let result = CleanResult {
            entries: vec![
                CleanedEntry {
                    bookmark: "a".into(),
                    meta: sample_meta(1),
                    reason: CleanReason::Stale,
                },
                CleanedEntry {
                    bookmark: "b".into(),
                    meta: sample_meta(2),
                    reason: CleanReason::Stale,
                },
                CleanedEntry {
                    bookmark: "c".into(),
                    meta: sample_meta(3),
                    reason: CleanReason::Closed,
                },
                CleanedEntry {
                    bookmark: "d".into(),
                    meta: sample_meta(4),
                    reason: CleanReason::Merged,
                },
            ],
        };

        assert_eq!(result.stale_count(), 2);
        assert_eq!(result.inactive_count(), 2);
        assert_eq!(result.total(), 4);
    }

    #[test]
    fn clean_reason_display() {
        assert_eq!(CleanReason::Stale.to_string(), "bookmark no longer exists");
        assert_eq!(CleanReason::Closed.to_string(), "change request is closed");
        assert_eq!(
            CleanReason::Merged.to_string(),
            "change request is merged and bookmark removed"
        );
    }

    #[test]
    fn clean_reason_is_inactive() {
        assert!(!CleanReason::Stale.is_inactive());
        assert!(CleanReason::Closed.is_inactive());
        assert!(CleanReason::Merged.is_inactive());
    }
}
