use jj_lib::backend::CommitId;
use jj_lib::op_store::LocalRemoteRefTarget;
use jj_lib::ref_name::RemoteNameBuf;
use jj_lib::refs::LocalAndRemoteRef;

/// DAG of bookmarks between trunk and head for stack operations.
pub mod graph;

/// Resolve the commit a bookmark points to, preferring the local target but
/// falling back to remote refs.
///
/// jj's `trunk()` revset resolves via `remote_bookmarks()` so a local
/// bookmark is never required.  This helper follows the same semantics:
/// when the local target is absent it returns the first available remote
/// ref's commit instead.
pub fn resolve_commit_id<'a>(target: &'a LocalRemoteRefTarget<'_>) -> Option<&'a CommitId> {
    if let Some(id) = target.local_target.as_normal() {
        return Some(id);
    }
    target
        .remote_refs
        .iter()
        .find_map(|(_, remote_ref)| remote_ref.target.as_normal())
}

/// A remote that tracks this bookmark.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteTracking {
    /// Name of the remote (e.g. `"origin"`, `"upstream"`).
    pub remote_name: String,
    /// Whether this remote ref is tracked (merged into the local ref).
    pub is_tracked: bool,
}

/// A local bookmark enriched with its remote tracking state.
///
/// `Hash` and `Eq` are derived from `name` only so that a bookmark's identity
/// in sets and maps is unaffected by its remote refs.
#[derive(Clone, Debug)]
pub struct Bookmark<'a> {
    name: String,
    ref_target: LocalRemoteRefTarget<'a>,
    remotes: Vec<RemoteTracking>,
}

impl PartialEq for Bookmark<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Bookmark<'_> {}

impl std::hash::Hash for Bookmark<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl<'a> Bookmark<'a> {
    /// Create a bookmark with the given name and no remote tracking refs.
    pub fn new(name: String, ref_target: LocalRemoteRefTarget<'a>) -> Self {
        let remotes = ref_target
            .clone()
            .remote_refs
            .iter()
            .map(|(remote_name, remote_ref)| RemoteTracking {
                remote_name: remote_name.as_str().to_string(),
                is_tracked: remote_ref.is_tracked(),
            })
            .collect();

        Self {
            name,
            ref_target,
            remotes,
        }
    }

    /// The local bookmark name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The local and remote ref targets for this bookmark.
    pub fn ref_target(&self) -> &LocalRemoteRefTarget<'a> {
        &self.ref_target
    }

    /// Remote tracking refs for this bookmark (excluding the synthetic `"git"` remote).
    pub fn remotes(&self) -> &[RemoteTracking] {
        &self.remotes
    }

    /// Tracked remote names only.
    pub fn tracked_remotes(&self) -> impl Iterator<Item = &str> {
        self.remotes
            .iter()
            .filter(|r| r.is_tracked)
            .map(|r| r.remote_name.as_str())
    }

    /// Ref target for a given remote.
    pub fn remote_ref(&self, remote_name: &RemoteNameBuf) -> Option<LocalAndRemoteRef<'a>> {
        self.ref_target
            .remote_refs
            .iter()
            .find_map(|(r_name, r_ref)| {
                (r_name == remote_name).then_some(LocalAndRemoteRef {
                    local_target: self.ref_target.local_target,
                    remote_ref: r_ref,
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use jj_lib::op_store::{LocalRemoteRefTarget, RefTarget, RemoteRef, RemoteRefState};
    use jj_lib::ref_name::RemoteName;

    use super::*;

    fn absent_target() -> LocalRemoteRefTarget<'static> {
        LocalRemoteRefTarget {
            local_target: RefTarget::absent_ref(),
            remote_refs: vec![],
        }
    }

    fn commit_id(byte: u8) -> CommitId {
        CommitId::new(vec![byte])
    }

    fn make_remote_ref(tracked: bool) -> RemoteRef {
        RemoteRef {
            target: RefTarget::absent(),
            state: if tracked {
                RemoteRefState::Tracked
            } else {
                RemoteRefState::New
            },
        }
    }

    fn make_remote_ref_at(id: &CommitId, tracked: bool) -> RemoteRef {
        RemoteRef {
            target: RefTarget::normal(id.clone()),
            state: if tracked {
                RemoteRefState::Tracked
            } else {
                RemoteRefState::New
            },
        }
    }

    #[test]
    fn new_creates_bookmark_with_empty_remotes() {
        let b = Bookmark::new("feat".into(), absent_target());
        assert_eq!(b.name(), "feat");
        assert!(b.remotes().is_empty());
    }

    #[test]
    fn new_populates_remotes_from_ref_target() {
        let origin_ref = make_remote_ref(true);
        let upstream_ref = make_remote_ref(false);
        let target = LocalRemoteRefTarget {
            local_target: RefTarget::absent_ref(),
            remote_refs: vec![
                (RemoteName::new("origin"), &origin_ref),
                (RemoteName::new("upstream"), &upstream_ref),
            ],
        };
        let b = Bookmark::new("feat".into(), target);
        assert_eq!(b.remotes().len(), 2);
        assert_eq!(b.remotes()[0].remote_name, "origin");
        assert!(b.remotes()[0].is_tracked);
        assert_eq!(b.remotes()[1].remote_name, "upstream");
        assert!(!b.remotes()[1].is_tracked);
    }

    #[test]
    fn tracked_remotes_filters_to_tracked_only() {
        let origin_ref = make_remote_ref(true);
        let upstream_ref = make_remote_ref(false);
        let fork_ref = make_remote_ref(true);
        let target = LocalRemoteRefTarget {
            local_target: RefTarget::absent_ref(),
            remote_refs: vec![
                (RemoteName::new("origin"), &origin_ref),
                (RemoteName::new("upstream"), &upstream_ref),
                (RemoteName::new("fork"), &fork_ref),
            ],
        };
        let b = Bookmark::new("feat".into(), target);
        let tracked: Vec<&str> = b.tracked_remotes().collect();
        assert_eq!(tracked, vec!["origin", "fork"]);
    }

    #[test]
    fn tracked_remotes_empty_when_none_tracked() {
        let origin_ref = make_remote_ref(false);
        let upstream_ref = make_remote_ref(false);
        let target = LocalRemoteRefTarget {
            local_target: RefTarget::absent_ref(),
            remote_refs: vec![
                (RemoteName::new("origin"), &origin_ref),
                (RemoteName::new("upstream"), &upstream_ref),
            ],
        };
        let b = Bookmark::new("feat".into(), target);
        assert_eq!(b.tracked_remotes().count(), 0);
    }

    #[test]
    fn tracked_remotes_empty_when_no_remotes() {
        let b = Bookmark::new("feat".into(), absent_target());
        assert_eq!(b.tracked_remotes().count(), 0);
    }

    #[test]
    fn equality_is_name_only() {
        let origin_ref = make_remote_ref(true);
        let upstream_ref = make_remote_ref(false);
        let a = Bookmark::new(
            "feat".into(),
            LocalRemoteRefTarget {
                local_target: RefTarget::absent_ref(),
                remote_refs: vec![(RemoteName::new("origin"), &origin_ref)],
            },
        );
        let b = Bookmark::new(
            "feat".into(),
            LocalRemoteRefTarget {
                local_target: RefTarget::absent_ref(),
                remote_refs: vec![(RemoteName::new("upstream"), &upstream_ref)],
            },
        );
        assert_eq!(a, b);
    }

    #[test]
    fn inequality_when_names_differ() {
        let a = Bookmark::new("feat-a".into(), absent_target());
        let b = Bookmark::new("feat-b".into(), absent_target());
        assert_ne!(a, b);
    }

    #[test]
    fn hash_is_name_only() {
        let origin_ref = make_remote_ref(true);
        let upstream_ref = make_remote_ref(false);
        let a = Bookmark::new(
            "feat".into(),
            LocalRemoteRefTarget {
                local_target: RefTarget::absent_ref(),
                remote_refs: vec![(RemoteName::new("origin"), &origin_ref)],
            },
        );
        let b = Bookmark::new(
            "feat".into(),
            LocalRemoteRefTarget {
                local_target: RefTarget::absent_ref(),
                remote_refs: vec![(RemoteName::new("upstream"), &upstream_ref)],
            },
        );
        let mut set = HashSet::new();
        set.insert(a);
        set.insert(b);
        assert_eq!(set.len(), 1);
    }

    // -- resolve_commit_id tests --

    #[test]
    fn resolve_commit_id_returns_local_target_when_present() {
        let id = commit_id(1);
        let local = RefTarget::normal(id.clone());
        let target = LocalRemoteRefTarget {
            local_target: &local,
            remote_refs: vec![],
        };
        assert_eq!(resolve_commit_id(&target), Some(&id));
    }

    #[test]
    fn resolve_commit_id_falls_back_to_remote_ref() {
        let id = commit_id(2);
        let remote = make_remote_ref_at(&id, true);
        let target = LocalRemoteRefTarget {
            local_target: RefTarget::absent_ref(),
            remote_refs: vec![(RemoteName::new("origin"), &remote)],
        };
        assert_eq!(resolve_commit_id(&target), Some(&id));
    }

    #[test]
    fn resolve_commit_id_falls_back_to_untracked_remote_ref() {
        let id = commit_id(3);
        let remote = make_remote_ref_at(&id, false);
        let target = LocalRemoteRefTarget {
            local_target: RefTarget::absent_ref(),
            remote_refs: vec![(RemoteName::new("origin"), &remote)],
        };
        assert_eq!(resolve_commit_id(&target), Some(&id));
    }

    #[test]
    fn resolve_commit_id_prefers_local_over_remote() {
        let local_id = commit_id(10);
        let remote_id = commit_id(20);
        let local = RefTarget::normal(local_id.clone());
        let remote = make_remote_ref_at(&remote_id, true);
        let target = LocalRemoteRefTarget {
            local_target: &local,
            remote_refs: vec![(RemoteName::new("origin"), &remote)],
        };
        assert_eq!(resolve_commit_id(&target), Some(&local_id));
    }

    #[test]
    fn resolve_commit_id_returns_none_when_all_absent() {
        assert_eq!(resolve_commit_id(&absent_target()), None);
    }

    #[test]
    fn resolve_commit_id_skips_absent_remote_refs() {
        let id = commit_id(5);
        let absent_remote = make_remote_ref(true); // absent target
        let present_remote = make_remote_ref_at(&id, true);
        let target = LocalRemoteRefTarget {
            local_target: RefTarget::absent_ref(),
            remote_refs: vec![
                (RemoteName::new("origin"), &absent_remote),
                (RemoteName::new("upstream"), &present_remote),
            ],
        };
        assert_eq!(resolve_commit_id(&target), Some(&id));
    }
}
