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
    pub fn new(name: String) -> Self {
        Self {
            name,
            remotes: Vec::new(),
        }
    }

    pub fn with_remotes(name: String, remotes: Vec<RemoteTracking>) -> Self {
        Self { name, remotes }
    }

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
