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
pub struct Bookmark {
    name: String,
    remotes: Vec<RemoteTracking>,
}

impl PartialEq for Bookmark {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Bookmark {}

impl std::hash::Hash for Bookmark {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl Bookmark {
    /// Create a bookmark with the given name and no remote tracking refs.
    pub fn new(name: String) -> Self {
        Self {
            name,
            remotes: Vec::new(),
        }
    }

    /// Create a bookmark with the given name and pre-populated remote tracking refs.
    pub fn with_remotes(name: String, remotes: Vec<RemoteTracking>) -> Self {
        Self { name, remotes }
    }

    /// The local bookmark name.
    pub fn name(&self) -> &str {
        &self.name
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn remote(name: &str, tracked: bool) -> RemoteTracking {
        RemoteTracking {
            remote_name: name.to_string(),
            is_tracked: tracked,
        }
    }

    #[test]
    fn new_creates_bookmark_with_empty_remotes() {
        let b = Bookmark::new("feat".into());
        assert_eq!(b.name(), "feat");
        assert!(b.remotes().is_empty());
    }

    #[test]
    fn with_remotes_stores_provided_remotes() {
        let remotes = vec![remote("origin", true), remote("upstream", false)];
        let b = Bookmark::with_remotes("feat".into(), remotes);
        assert_eq!(b.remotes().len(), 2);
        assert_eq!(b.remotes()[0].remote_name, "origin");
        assert!(b.remotes()[0].is_tracked);
        assert_eq!(b.remotes()[1].remote_name, "upstream");
        assert!(!b.remotes()[1].is_tracked);
    }

    #[test]
    fn tracked_remotes_filters_to_tracked_only() {
        let remotes = vec![
            remote("origin", true),
            remote("upstream", false),
            remote("fork", true),
        ];
        let b = Bookmark::with_remotes("feat".into(), remotes);
        let tracked: Vec<&str> = b.tracked_remotes().collect();
        assert_eq!(tracked, vec!["origin", "fork"]);
    }

    #[test]
    fn tracked_remotes_empty_when_none_tracked() {
        let remotes = vec![remote("origin", false), remote("upstream", false)];
        let b = Bookmark::with_remotes("feat".into(), remotes);
        assert_eq!(b.tracked_remotes().count(), 0);
    }

    #[test]
    fn tracked_remotes_empty_when_no_remotes() {
        let b = Bookmark::new("feat".into());
        assert_eq!(b.tracked_remotes().count(), 0);
    }

    #[test]
    fn equality_is_name_only() {
        let a = Bookmark::with_remotes("feat".into(), vec![remote("origin", true)]);
        let b = Bookmark::with_remotes("feat".into(), vec![remote("upstream", false)]);
        assert_eq!(a, b);
    }

    #[test]
    fn inequality_when_names_differ() {
        let a = Bookmark::new("feat-a".into());
        let b = Bookmark::new("feat-b".into());
        assert_ne!(a, b);
    }

    #[test]
    fn hash_is_name_only() {
        let a = Bookmark::with_remotes("feat".into(), vec![remote("origin", true)]);
        let b = Bookmark::with_remotes("feat".into(), vec![remote("upstream", false)]);
        let mut set = HashSet::new();
        set.insert(a);
        set.insert(b);
        assert_eq!(set.len(), 1);
    }
}
