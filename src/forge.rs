/// Forge detection from git remote URLs and jj config.
pub mod detect;
/// GitHub / GitHub Enterprise backend.
pub mod github;

use std::future::Future;
use std::pin::Pin;

use crate::protos::change_request::ForgeMeta;

/// Boxed future type used in [`Forge`] method signatures for dyn-safety.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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

    /// Human-readable label for the change request link.
    ///
    /// Used as the visible text in hyperlinks and plain-text output.
    /// Each forge formats this according to its conventions, e.g.
    /// `"github.com:owner/repo#42"` for GitHub.
    fn link_label(&self) -> String;

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

/// Object-safe forge backend interface.
///
/// Each forge (GitHub, GitLab, Gerrit, ...) implements this trait directly.
/// Callers work with `&dyn Forge` or `Box<dyn Forge>` — no enums needed.
///
/// Methods return boxed futures to ensure dyn-compatibility. Implementations
/// use `Box::pin(async move { ... })` to build the future.
pub trait Forge: Send + Sync {
    /// Create a new change request on the forge.
    fn create<'a>(
        &'a self,
        params: CreateParams<'a>,
    ) -> BoxFuture<'a, Result<Box<dyn ChangeRequest>, Box<dyn std::error::Error>>>;

    /// Fetch a change request by its stored metadata.
    fn get<'a>(
        &'a self,
        meta: &'a ForgeMeta,
    ) -> BoxFuture<'a, Result<Box<dyn ChangeRequest>, Box<dyn std::error::Error>>>;

    /// Find change requests by source and/or target branch.
    ///
    /// Useful for discovering existing CRs on the forge that are not yet
    /// tracked locally.
    fn find<'a>(
        &'a self,
        source_branch: Option<&'a str>,
        target_branch: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<Box<dyn ChangeRequest>>, Box<dyn std::error::Error>>>;

    /// Update the title and/or body of an existing change request.
    fn update<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        title: Option<&'a str>,
        body: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Box<dyn ChangeRequest>, Box<dyn std::error::Error>>>;

    /// Update the target (base) branch of an existing change request.
    ///
    /// Not all forges support this — the default implementation returns an
    /// "unsupported" error.
    fn update_base<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        base_branch: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn ChangeRequest>, Box<dyn std::error::Error>>> {
        let _ = (meta, base_branch);
        Box::pin(async { Err("this forge does not support updating the base branch".into()) })
    }

    /// Close a change request without merging.
    fn close<'a>(
        &'a self,
        meta: &'a ForgeMeta,
    ) -> BoxFuture<'a, Result<Box<dyn ChangeRequest>, Box<dyn std::error::Error>>>;

    /// Find change requests matching `source_branch` and return persistable metadata.
    ///
    /// This is a convenience wrapper around [`Forge::find`] that extracts
    /// [`ForgeMeta`] from each result.
    fn find_change_requests<'a>(
        &'a self,
        source_branch: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ForgeMeta>, Box<dyn std::error::Error>>> {
        Box::pin(async move {
            let crs = self.find(Some(source_branch), None).await?;
            Ok(crs.iter().map(|cr| cr.to_forge_meta()).collect())
        })
    }
}
