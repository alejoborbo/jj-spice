use std::fmt;

use crate::protos::change_request::{forge_meta::Forge as ForgeOneof, ChangeRequests, ForgeMeta};

use super::{SpiceStore, SpiceStoreError};

const FILENAME: &str = "change_requests.pb";

impl ForgeMeta {
    /// Return the target (base) branch stored in the forge-specific metadata.
    pub fn target_branch(&self) -> Option<&str> {
        match &self.forge {
            Some(ForgeOneof::Github(gh)) => Some(&gh.target_branch),
            None => None,
        }
    }
}

impl fmt::Display for ForgeMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.forge {
            Some(ForgeOneof::Github(gh)) => {
                write!(
                    f,
                    "GitHub PR #{} ({} → {})",
                    gh.number, gh.source_branch, gh.target_branch
                )
            }
            None => write!(f, "unknown forge"),
        }
    }
}

impl ChangeRequests {
    /// Look up a mapping by bookmark name.
    pub fn get(&self, bookmark: &str) -> Option<&ForgeMeta> {
        self.by_bookmark.get(bookmark)
    }

    /// Insert or replace the mapping for a bookmark.
    pub fn set(&mut self, bookmark: String, meta: ForgeMeta) {
        self.by_bookmark.insert(bookmark, meta);
    }

    /// Remove a mapping by bookmark name. Returns `true` if it existed.
    pub fn remove(&mut self, bookmark: &str) -> bool {
        self.by_bookmark.remove(bookmark).is_some()
    }
}

/// Handles persistence of [`ChangeRequests`] to disk.
///
/// Delegates file I/O to [`SpiceStore`]. Query and mutation are done directly
/// on [`ChangeRequests`] via its own methods.
pub struct ChangeRequestStore<'a> {
    store: &'a SpiceStore,
}

impl<'a> ChangeRequestStore<'a> {
    /// Create a new handle backed by the given [`SpiceStore`].
    pub fn new(store: &'a SpiceStore) -> Self {
        Self { store }
    }

    /// Load the current state from disk.
    ///
    /// Returns an empty [`ChangeRequests`] if the file does not exist yet.
    pub fn load(&self) -> Result<ChangeRequests, SpiceStoreError> {
        self.store.load(FILENAME)
    }

    /// Atomically save state to disk.
    pub fn save(&self, state: &ChangeRequests) -> Result<(), SpiceStoreError> {
        self.store.save(FILENAME, state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protos::change_request::{forge_meta::Forge as ForgeOneof, GitHubMeta};
    use tempfile::TempDir;

    /// Build a sample [`ForgeMeta`] for testing.
    fn sample_meta(number: u64) -> ForgeMeta {
        ForgeMeta {
            forge: Some(ForgeOneof::Github(GitHubMeta {
                number,
                source_branch: "feat".into(),
                target_branch: "main".into(),
                source_repo: "owner/repo".into(),
                target_repo: "owner/repo".into(),
                graphql_id: String::new(),
            })),
        }
    }

    fn temp_cr_store() -> (TempDir, SpiceStore) {
        let tmp = TempDir::new().unwrap();
        let store = SpiceStore::init_at(tmp.path()).unwrap();
        (tmp, store)
    }

    #[test]
    fn get_returns_none_for_missing_bookmark() {
        let state = ChangeRequests::default();
        assert!(state.get("nonexistent").is_none());
    }

    #[test]
    fn set_then_get_retrieves_mapping() {
        let mut state = ChangeRequests::default();
        let meta = sample_meta(1);

        state.set("feat-branch".into(), meta.clone());
        let got = state.get("feat-branch");

        assert_eq!(got, Some(&meta));
    }

    #[test]
    fn set_replaces_existing_mapping() {
        let mut state = ChangeRequests::default();
        state.set("feat".into(), sample_meta(1));
        state.set("feat".into(), sample_meta(2));

        let got = state.get("feat").unwrap();
        match &got.forge {
            Some(ForgeOneof::Github(gh)) => assert_eq!(gh.number, 2),
            _ => panic!("expected GitHub variant"),
        }
    }

    #[test]
    fn remove_returns_true_when_found() {
        let mut state = ChangeRequests::default();
        state.set("feat".into(), sample_meta(1));

        assert!(state.remove("feat"));
        assert!(state.get("feat").is_none());
    }

    #[test]
    fn remove_returns_false_when_not_found() {
        let mut state = ChangeRequests::default();
        assert!(!state.remove("missing"));
    }

    #[test]
    fn load_save_round_trip_through_store() {
        let (_tmp, spice) = temp_cr_store();
        let cr_store = ChangeRequestStore::new(&spice);

        let mut state = cr_store.load().unwrap();
        state.set("branch-a".into(), sample_meta(10));
        state.set("branch-b".into(), sample_meta(20));
        cr_store.save(&state).unwrap();

        let reloaded = cr_store.load().unwrap();
        assert_eq!(state, reloaded);
        assert_eq!(reloaded.by_bookmark.len(), 2);
    }
}
