/// Forge detection from git remote URLs and jj config.
pub mod detect;
/// GitHub / GitHub Enterprise backend.
pub mod github;

use crate::protos::change_request::ForgeMeta;

use self::detect::ForgeKind;
use self::github::GitHubForge;

/// Status of a change request on a forge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Branch (bookmark) that contains the changes.
    pub source_branch: &'a str,
    /// Branch the change request targets for merging.
    pub target_branch: &'a str,
    /// One-line summary of the change.
    pub title: &'a str,
    /// Optional longer description.
    pub body: Option<&'a str>,
    /// Whether to create the change request as a draft.
    pub is_draft: bool,
}

/// Trait implemented by each forge backend (GitHub, GitLab, Bitbucket, Gitea, Gerrit).
#[async_trait::async_trait]
pub trait Forge {
    /// Backend-specific error type.
    type Error;
    /// Concrete change request type returned by this backend.
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

/// Find change requests on a forge for the given source branch.
///
/// Instantiates the appropriate forge backend based on [`ForgeKind`] and
/// queries for CRs matching `source_branch`. Returns persistable metadata
/// for each match.
pub async fn find_change_requests(
    kind: &ForgeKind,
    source_branch: &str,
) -> Result<Vec<ForgeMeta>, Box<dyn std::error::Error>> {
    match kind {
        ForgeKind::GitHub {
            owner,
            repo,
            base_uri,
        } => {
            let forge = GitHubForge::new(owner, repo, base_uri.as_deref())?;
            let crs = forge.find(Some(source_branch), None).await?;
            Ok(crs.iter().map(|cr| cr.to_forge_meta()).collect())
        }
    }
}
