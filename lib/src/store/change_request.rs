use std::fmt;

use super::{SpiceStore, SpiceStoreError};
use crate::protos::change_request::forge_meta::Forge as ForgeOneof;
use crate::protos::change_request::{ChangeRequests, ForgeMeta};

const FILENAME: &str = "change_requests.pb";

impl ForgeMeta {
    /// Return the target (base) branch stored in the forge-specific metadata.
    pub fn target_branch(&self) -> Option<&str> {
        match &self.forge {
            Some(ForgeOneof::Github(gh)) => Some(&gh.target_branch),
            Some(ForgeOneof::Gitlab(gl)) => Some(&gl.target_branch),
            None => None,
        }
    }

    /// Return the source (head) branch stored in the forge-specific metadata.
    pub fn source_branch(&self) -> Option<&str> {
        match &self.forge {
            Some(ForgeOneof::Github(gh)) => Some(&gh.source_branch),
            Some(ForgeOneof::Gitlab(gl)) => Some(&gl.source_branch),
            None => None,
        }
    }

    /// Return the target repository identity stored in the metadata.
    ///
    /// For cross-repo (fork) change requests this identifies where the CR
    /// lives (e.g. `"upstream-org/repo"`). Returns `None` when the field is
    /// empty (same-repo CR) or the forge variant is absent.
    pub fn target_repo(&self) -> Option<&str> {
        match &self.forge {
            Some(ForgeOneof::Github(gh)) if !gh.target_repo.is_empty() => Some(&gh.target_repo),
            // GitLab uses project IDs, not repo path strings for cross-repo.
            _ => None,
        }
    }

    /// Return the comment ID stored in the forge-specific metadata, if any.
    pub fn comment_id(&self) -> Option<u64> {
        match &self.forge {
            Some(ForgeOneof::Github(gh)) => gh.comment_id,
            Some(ForgeOneof::Gitlab(gl)) => gl.comment_id,
            None => None,
        }
    }

    /// Set the comment ID in the forge-specific metadata.
    pub fn set_comment_id(&mut self, id: u64) {
        match &mut self.forge {
            Some(ForgeOneof::Github(gh)) => gh.comment_id = Some(id),
            Some(ForgeOneof::Gitlab(gl)) => gl.comment_id = Some(id),
            None => {}
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
            Some(ForgeOneof::Gitlab(gl)) => {
                write!(
                    f,
                    "GitLab MR !{} ({} → {})",
                    gl.iid, gl.source_branch, gl.target_branch
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

    /// Iterate over all `(bookmark_name, forge_meta)` entries.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &ForgeMeta)> {
        self.by_bookmark.iter()
    }

    /// Retain only entries for which the predicate returns `true`.
    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&String, &mut ForgeMeta) -> bool,
    {
        self.by_bookmark.retain(f);
    }

    /// Return the number of tracked entries.
    pub fn len(&self) -> usize {
        self.by_bookmark.len()
    }

    /// Return `true` if there are no tracked entries.
    pub fn is_empty(&self) -> bool {
        self.by_bookmark.is_empty()
    }

    /// Return a list of all tracked bookmark names.
    pub fn bookmark_names(&self) -> Vec<&String> {
        self.by_bookmark.keys().collect()
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
    use tempfile::TempDir;

    use super::*;
    use crate::protos::change_request::GitHubMeta;
    use crate::protos::change_request::forge_meta::Forge as ForgeOneof;

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
                comment_id: None,
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

    #[test]
    fn set_comment_id_updates_github_variant() {
        let mut meta = sample_meta(1);
        assert_eq!(meta.comment_id(), None);

        meta.set_comment_id(12345);
        assert_eq!(meta.comment_id(), Some(12345));
    }

    #[test]
    fn set_comment_id_noop_for_none_forge() {
        let mut meta = ForgeMeta { forge: None };
        meta.set_comment_id(999);
        assert_eq!(meta.comment_id(), None);
    }

    #[test]
    fn iter_returns_all_entries() {
        let mut state = ChangeRequests::default();
        state.set("a".into(), sample_meta(1));
        state.set("b".into(), sample_meta(2));

        let mut entries: Vec<_> = state.iter().map(|(k, _)| k.clone()).collect();
        entries.sort();
        assert_eq!(entries, vec!["a", "b"]);
    }

    #[test]
    fn retain_keeps_matching_entries() {
        let mut state = ChangeRequests::default();
        state.set("keep".into(), sample_meta(1));
        state.set("drop".into(), sample_meta(2));

        state.retain(|name, _| name == "keep");

        assert!(state.get("keep").is_some());
        assert!(state.get("drop").is_none());
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn len_and_is_empty() {
        let mut state = ChangeRequests::default();
        assert!(state.is_empty());
        assert_eq!(state.len(), 0);

        state.set("a".into(), sample_meta(1));
        assert!(!state.is_empty());
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn bookmark_names_returns_all_keys() {
        let mut state = ChangeRequests::default();
        state.set("x".into(), sample_meta(1));
        state.set("y".into(), sample_meta(2));

        let mut names: Vec<_> = state.bookmark_names().into_iter().cloned().collect();
        names.sort();
        assert_eq!(names, vec!["x", "y"]);
    }

    #[test]
    fn comment_id_round_trips_through_store() {
        let (_tmp, spice) = temp_cr_store();
        let cr_store = ChangeRequestStore::new(&spice);

        let mut meta = sample_meta(42);
        meta.set_comment_id(7890);

        let mut state = cr_store.load().unwrap();
        state.set("branch-with-comment".into(), meta);
        cr_store.save(&state).unwrap();

        let reloaded = cr_store.load().unwrap();
        let got = reloaded.get("branch-with-comment").unwrap();
        assert_eq!(got.comment_id(), Some(7890));
    }
}
