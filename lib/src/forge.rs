/// Forge detection from git remote URLs and jj config.
pub mod detect;
/// GitHub / GitHub Enterprise backend.
pub mod github;
/// GitLab backend.
pub mod gitlab;

use std::future::Future;
use std::pin::Pin;

use crate::protos::change_request::ForgeMeta;

/// Boxed future type used in [`Forge`] method signatures for dyn-safety.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Shorthand for a single change-request result returned by [`Forge`] methods.
type ForgeResult = Result<Box<dyn ChangeRequest>, Box<dyn std::error::Error + Send + Sync>>;

/// Shorthand for a multi change-request result returned by [`Forge::find`].
type ForgeResults = Result<Vec<Box<dyn ChangeRequest>>, Box<dyn std::error::Error + Send + Sync>>;

/// Status of a change request on a forge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    /// Draft, not yet ready for review.
    Draft,
    /// Active and accepting updates.
    Open,
    /// Closed without merging.
    Closed,
    /// Successfully merged into the target branch.
    Merged,
}

impl ChangeStatus {
    pub fn is_inactive(&self) -> bool {
        matches!(self, ChangeStatus::Closed | ChangeStatus::Merged)
    }
}

/// A change request on a forge.
///
/// Each forge backend implements this on its own type that combines persisted
/// identity (from the proto metadata) with volatile data fetched from the API.
/// The trait provides common accessors for display and a method to extract the
/// persistable [`ForgeMeta`] for the store.
pub trait ChangeRequest: Send {
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
    /// Identity of the repository where the source branch lives, for
    /// cross-repo (fork-to-upstream) change requests.
    ///
    /// When `None`, the source is assumed to be the same repository as the
    /// target (no fork). When `Some`, the value is the fork's
    /// [`Forge::repo_id`].
    pub source_repo: Option<&'a str>,
}

/// Object-safe forge backend interface.
///
/// Each forge (GitHub, GitLab, Gerrit, ...) implements this trait directly.
/// Callers work with `&dyn Forge` or `Box<dyn Forge>` — no enums needed.
///
/// Methods return boxed futures to ensure dyn-compatibility. Implementations
/// use `Box::pin(async move { ... })` to build the future.
pub trait Forge: Send + Sync {
    /// Opaque identity string for this forge's repository.
    ///
    /// Used to match a forge instance against the `target_repo` stored in
    /// [`ForgeMeta`] when routing cross-repo (fork) operations to the correct
    /// forge instance. The format is forge-specific and must not be parsed
    /// by callers.
    fn repo_id(&self) -> String;

    /// Create a new change request on the forge.
    fn create<'a>(&'a self, params: CreateParams<'a>) -> BoxFuture<'a, ForgeResult>;

    /// Fetch a change request by its stored metadata.
    fn get<'a>(&'a self, meta: &'a ForgeMeta) -> BoxFuture<'a, ForgeResult>;

    /// Fetch multiple change requests in a single operation.
    ///
    /// Forges that support batching (e.g. GitHub via GraphQL) override this
    /// for efficiency. The default calls [`Self::get`] sequentially.
    ///
    /// Results are returned in the same order as the input `metas`.
    fn get_batch<'a>(&'a self, metas: Vec<&'a ForgeMeta>) -> BoxFuture<'a, Vec<ForgeResult>> {
        // Default: sequential fetches. Each result is produced and consumed
        // one-at-a-time so no non-Send values are held across await points.
        Box::pin(sequential_get_batch(self, metas))
    }

    /// Find change requests by source and/or target branch.
    ///
    /// Useful for discovering existing CRs on the forge that are not yet
    /// tracked locally.
    ///
    /// `source_repo` narrows the search to cross-repo (fork) change requests
    /// whose source branch lives in the specified repository. The format is
    /// forge-specific and matches [`CreateParams::source_repo`]. When `None`,
    /// the search is limited to same-repo change requests.
    fn find<'a>(
        &'a self,
        source_branch: Option<&'a str>,
        target_branch: Option<&'a str>,
        source_repo: Option<&'a str>,
    ) -> BoxFuture<'a, ForgeResults>;

    /// Update the title and/or body of an existing change request.
    fn update<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        title: Option<&'a str>,
        body: Option<&'a str>,
    ) -> BoxFuture<'a, ForgeResult>;

    /// Update the target (base) branch of an existing change request.
    ///
    /// Not all forges support this — the default implementation returns an
    /// "unsupported" error.
    fn update_base<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        base_branch: &'a str,
    ) -> BoxFuture<'a, ForgeResult> {
        let _ = (meta, base_branch);
        Box::pin(async { Err("this forge does not support updating the base branch".into()) })
    }

    /// Close a change request without merging.
    fn close<'a>(&'a self, meta: &'a ForgeMeta) -> BoxFuture<'a, ForgeResult>;

    /// Update or create a comment on a change request.
    ///
    /// Return the ID of the comment if it was created, or the ID of the
    fn update_or_create_comment<'a>(
        &'a self,
        meta: &'a ForgeMeta,
        comment: &'a str,
    ) -> BoxFuture<'a, Result<u64, Box<dyn std::error::Error + Send + Sync>>>;

    /// Find change requests matching `source_branch` and return persistable metadata.
    ///
    /// This is a convenience wrapper around [`Forge::find`] that extracts
    /// [`ForgeMeta`] from each result.
    /// `source_repo` is forwarded to [`Forge::find`] unchanged; see its
    /// documentation for the forge-specific format.
    fn find_change_requests<'a>(
        &'a self,
        source_branch: &'a str,
        source_repo: Option<&'a str>,
    ) -> BoxFuture<'a, ForgeResults> {
        Box::pin(async move {
            let crs = self.find(Some(source_branch), None, source_repo).await?;
            Ok(crs)
        })
    }
}

/// Default sequential implementation of [`Forge::get_batch`].
async fn sequential_get_batch<'a, F: Forge + ?Sized>(
    forge: &'a F,
    metas: Vec<&'a ForgeMeta>,
) -> Vec<ForgeResult> {
    let mut results: Vec<ForgeResult> = Vec::with_capacity(metas.len());
    for meta in metas {
        results.push(forge.get(meta).await);
    }
    results
}
