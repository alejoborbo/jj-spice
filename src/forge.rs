pub mod github;

use crate::protos::change_request::ForgeMeta;

/// Status of a change request on a forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeStatus {
    /// Active and accepting updates.
    Open,
    /// Closed without merging.
    Closed,
    /// Successfully merged into the target branch.
    Merged,
}

/// A change request on a forge.
///
/// Each forge backend implements this on its own type that combines persisted
/// identity (from the proto metadata) with volatile data fetched from the API.
/// The trait provides common accessors for display and a method to extract the
/// persistable [`ForgeMeta`] for the store.
pub trait ChangeRequest {
    /// Persistable metadata for the store.
    fn to_forge_meta(&self) -> ForgeMeta;

    /// Forge-specific identifier as a display string (e.g. `"42"` for a
    /// GitHub PR number, `"I8473b..."` for a Gerrit Change-Id).
    fn id(&self) -> String;

    /// Current status on the forge.
    fn status(&self) -> ChangeStatus;

    /// Web URL to view in a browser.
    fn url(&self) -> &str;

    /// Short summary of the change.
    fn title(&self) -> &str;

    /// Longer description. `None` when the forge has no description set.
    fn body(&self) -> Option<&str>;

    /// Whether the CR is a draft / work-in-progress.
    fn is_draft(&self) -> bool;
}

/// Input parameters for creating a change request on a forge.
pub struct CreateParams<'a> {
    pub source_branch: &'a str,
    pub target_branch: &'a str,
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub is_draft: bool,
}

/// Trait implemented by each forge backend (GitHub, GitLab, Bitbucket, Gitea, Gerrit).
#[async_trait::async_trait]
pub trait Forge {
    type Error;
    type CR: ChangeRequest;

    /// Create a new change request on the forge.
    async fn create(&self, params: CreateParams<'_>) -> Result<Self::CR, Self::Error>;

    /// Fetch a change request by its stored metadata.
    async fn get(&self, meta: &ForgeMeta) -> Result<Self::CR, Self::Error>;

    /// Find change requests by source and/or target branch.
    ///
    /// Useful for discovering existing CRs on the forge that are not yet
    /// tracked locally.
    async fn find(
        &self,
        source_branch: Option<&str>,
        target_branch: Option<&str>,
    ) -> Result<Vec<Self::CR>, Self::Error>;

    /// Update the title and/or body of an existing change request.
    async fn update(
        &self,
        meta: &ForgeMeta,
        title: Option<&str>,
        body: Option<&str>,
    ) -> Result<Self::CR, Self::Error>;

    /// Close a change request without merging.
    async fn close(&self, meta: &ForgeMeta) -> Result<Self::CR, Self::Error>;
}
