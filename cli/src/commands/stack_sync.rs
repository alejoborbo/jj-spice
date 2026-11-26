use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;

use futures::TryStreamExt as _;
use jj_cli::git_util::{GitSubprocessUi, load_git_import_options};
use jj_cli::revset_util::parse_remote_fetch_bookmarks;
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::config::{ConfigFile, ConfigSource};
use jj_lib::git::{GitFetch, GitFetchRefExpression, REMOTE_NAME_FOR_LOCAL_GIT_REPO};
use jj_lib::ref_name::{RefName, RemoteName};
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::revset::ResolvedRevsetExpression;
use jj_lib::rewrite::{
    MoveCommitsLocation, MoveCommitsStats, MoveCommitsTarget, RebaseOptions, move_commits,
};
use jj_lib::settings::RemoteSettingsMap;
use jj_lib::str_util::{StringExpression, StringPattern};
use jj_spice_lib::bookmark::Bookmark;
use jj_spice_lib::bookmark::graph::{BookmarkGraph, BookmarkNode};
use jj_spice_lib::forge::Forge;
use jj_spice_lib::forge::detect::{
    DetectionResult, FORGE_TYPES, UnmatchedRemote, build_forge_for_type, detect_forges,
};
use jj_spice_lib::protos::change_request::ForgeMeta;
use jj_spice_lib::store::SpiceStore;
use jj_spice_lib::store::change_request::ChangeRequestStore;

use crate::commands::cli::SyncArgs;
use crate::commands::env::{SpiceEnv, cmd_err};

/// Per-bookmark error (non-fatal, printed as a warning).
#[derive(Debug, thiserror::Error)]
enum BookmarkSyncError {
    #[error("no tracked remotes")]
    NoTrackedRemotes,
    #[error("no forge detected for any tracked remote")]
    NoForgeDetected,
    #[error("forge error: {0}")]
    Forge(#[from] Box<dyn std::error::Error>),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Discover and track change requests for each bookmark in the stack.
///
/// For each bookmark between trunk and the working copy, queries the detected
/// forges for existing change requests and persists their identity metadata
/// locally.
///
/// When remotes are found with parseable URLs but no recognised forge hostname,
/// the user is prompted (once per unique hostname) to select a forge type.
/// The choice is persisted to the jj repo config so future runs skip the prompt.
pub async fn run(
    args: &SyncArgs,
    env: &SpiceEnv,
    trunk: &CommitId,
    head: &CommitId,
    trunk_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let DetectionResult {
        mut forges,
        unmatched,
    } = detect_forges(env.repo.store(), env.config())?;

    // Prompt for unmatched remotes (grouped by hostname).
    resolve_unmatched_remotes(env, &unmatched, &mut forges)?;

    // Fetch the latest remote changes for the trunk branch.
    // The returned repo reflects the fetched state so subsequent operations
    // see the up-to-date trunk.
    let repo_after_fetch = fetch_trunk(env, trunk_name).await?;
    let head = repo_after_fetch
        .view()
        .get_wc_commit_id(env.workspace.workspace_name())
        .unwrap_or(head)
        .clone();

    // Re-resolve trunk from the updated view — the bookmark may have advanced
    // after the fetch (e.g. new merge commits on main).
    let trunk = repo_after_fetch
        .view()
        .get_local_bookmark(RefName::new(trunk_name))
        .as_normal()
        .cloned()
        .unwrap_or_else(|| trunk.clone());

    // Build the bookmark graph. If the `--restack` flag is specified, rebase the
    // stack onto trunk first, then build the graph from the updated repo state.
    let active_repo = if args.restack {
        // Build a temporary graph to discover root bookmarks, then rebase.
        let initial_graph =
            BookmarkGraph::build_active_graph(repo_after_fetch.as_ref(), &trunk, &head)?;
        let root_bookmarks = initial_graph.root_bookmarks.clone();
        restack_graph(env, &repo_after_fetch, &root_bookmarks, &trunk).await?
    } else {
        repo_after_fetch
    };
    let graph = BookmarkGraph::build_active_graph(active_repo.as_ref(), &trunk, &head)?;

    let spice_store = SpiceStore::init_at(env.workspace.repo_path())?;
    let cr_store = ChangeRequestStore::new(&spice_store);
    let mut state = cr_store.load()?;

    let nodes: Vec<&BookmarkNode> = graph.iter_graph()?.collect();
    for node in &nodes {
        let bookmark = node.bookmark();
        let name = node.name();

        // Skip bookmarks that already have a tracked CR (unless --force).
        if !args.force && state.get(name).is_some() {
            writeln!(
                env.ui.warning_default(),
                "{name}: already tracked, skipping (use --force to re-sync)"
            )?;
            continue;
        }

        match sync_bookmark(&env.ui, bookmark, &forges, args.allow_inactive).await {
            Ok(Some(meta)) => {
                state.set(name.to_string(), meta);
                writeln!(env.ui.status(), "{name}: tracked")?;
            }
            Ok(None) => {
                writeln!(env.ui.status(), "{name}: no change request found")?;
            }
            Err(e) => {
                writeln!(env.ui.warning_default(), "{name}: {e}")?;
            }
        }
    }

    // Auto-clean: remove stale and inactive entries when enabled.
    let auto_clean = env
        .config()
        .get::<bool>(["spice", "auto-clean"])
        .unwrap_or(true);

    if auto_clean {
        auto_clean_entries(&env.ui, &mut state, &forges, &active_repo).await?;
    }

    cr_store.save(&state)?;
    Ok(())
}

/// Remove stale and inactive entries from the CR store during sync.
///
/// Closed CRs are always removed. Merged CRs are only removed when the
/// bookmark no longer exists locally. Uses the first available forge for
/// status queries.
async fn auto_clean_entries(
    ui: &Ui,
    state: &mut jj_spice_lib::protos::change_request::ChangeRequests,
    forges: &HashMap<String, Box<dyn Forge>>,
    repo: &ReadonlyRepo,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(forge) = forges.values().next() else {
        return Ok(());
    };

    let local_bookmarks = repo
        .view()
        .bookmarks()
        .map(|(name, _)| name.as_str().to_owned())
        .collect();
    let inactive =
        jj_spice_lib::clean::find_inactive_entries(state, forge.as_ref(), &local_bookmarks).await;

    if inactive.is_empty() {
        return Ok(());
    }

    for entry in &inactive {
        writeln!(
            ui.status(),
            "Auto-clean: removing {}: {} ({})",
            entry.bookmark,
            entry.meta,
            entry.reason,
        )?;
    }

    let result = jj_spice_lib::clean::CleanResult { entries: inactive };
    jj_spice_lib::clean::apply_clean(state, &result);

    writeln!(
        ui.status(),
        "Auto-clean: removed {} inactive entry(ies)",
        result.total(),
    )?;

    Ok(())
}

/// Prompt the user once per unique hostname and construct forge clients for
/// unmatched remotes.
///
/// Persists the selected forge type to the jj repo config at
/// `spice.forges.<hostname>.type` so subsequent runs auto-detect.
fn resolve_unmatched_remotes(
    env: &SpiceEnv,
    unmatched: &[UnmatchedRemote],
    forges: &mut HashMap<String, Box<dyn Forge>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if unmatched.is_empty() {
        return Ok(());
    }

    // Group by hostname so we only prompt once per unique host.
    let mut by_host: HashMap<&str, Vec<&UnmatchedRemote>> = HashMap::new();
    for remote in unmatched {
        by_host.entry(&remote.hostname).or_default().push(remote);
    }

    // Build choice labels: forge types + a skip sentinel.
    let mut choices: Vec<&str> = FORGE_TYPES.to_vec();
    choices.push("(skip)");
    let skip_index = choices.len() - 1;

    for (hostname, remotes) in &by_host {
        let remote_names: Vec<&str> = remotes.iter().map(|r| r.remote_name.as_str()).collect();
        let prompt_msg = format!(
            "Remote{} {} ({hostname}): select forge type",
            if remote_names.len() > 1 { "s" } else { "" },
            remote_names.join(", "),
        );

        let selected = match env
            .ui
            .prompt_choice(&prompt_msg, &choices, Some(skip_index))
        {
            Ok(idx) => idx,
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                // Non-interactive terminal — skip gracefully.
                writeln!(
                    env.ui.warning_default(),
                    "{hostname}: skipping forge selection (non-interactive terminal)"
                )?;
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // EOF on stdin — skip gracefully.
                writeln!(
                    env.ui.warning_default(),
                    "{hostname}: skipping forge selection (EOF)"
                )?;
                continue;
            }
            Err(e) => return Err(e.into()),
        };

        if selected == skip_index {
            continue;
        }

        let forge_type = choices[selected];

        // Persist the choice to jj repo config.
        persist_forge_config(env, hostname, forge_type)?;

        // Construct a forge client for each remote with this hostname.
        for remote in remotes {
            let forge = build_forge_for_type(
                &remote.remote_name,
                forge_type,
                &remote.owner,
                &remote.repo,
                hostname,
            )?;
            forges.insert(remote.remote_name.clone(), forge);
        }
    }

    Ok(())
}

/// Fetch the latest remote changes for trunk.
///
/// Returns the committed repo snapshot so callers see the updated trunk
/// position instead of the stale `env.repo`.
async fn fetch_trunk(
    env: &SpiceEnv,
    trunk_name: &str,
) -> Result<Arc<ReadonlyRepo>, Box<dyn std::error::Error>> {
    let remote_settings = env.settings.remote_settings()?;
    let fetch_remotes = trunk_fetch_remotes(env, trunk_name, &remote_settings)?;
    if fetch_remotes.is_empty() {
        return Ok(env.repo.clone());
    }

    let expression = GitFetchRefExpression {
        bookmark: StringExpression::Pattern(Box::new(StringPattern::Exact(trunk_name.to_owned()))),
        tag: StringExpression::none(),
    };
    let mut tx = env.repo.start_transaction();
    let import_options =
        load_git_import_options(&env.ui, &env.git_settings, &remote_settings).map_err(cmd_err)?;
    let mut git_fetch = GitFetch::new(
        tx.repo_mut(),
        env.git_settings.to_subprocess_options(),
        &import_options,
    )?;

    // Fetch changes from each remote
    for remote in fetch_remotes {
        let mut callback = GitSubprocessUi::new(&env.ui);
        git_fetch.fetch(remote, &expression, &mut callback, None)?;
    }
    git_fetch.import_refs().await?;
    tx.repo_mut().rebase_descendants().await?;

    if tx.repo().has_changes() {
        Ok(tx.commit("fetch trunk").await?)
    } else {
        Ok(env.repo.clone())
    }
}

fn trunk_fetch_remotes<'a>(
    env: &'a SpiceEnv,
    trunk_name: &str,
    settings: &RemoteSettingsMap,
) -> Result<Vec<&'a RemoteName>, Box<dyn std::error::Error>> {
    let mut remotes = Vec::new();
    for (symbol, target) in env.repo.view().all_remote_bookmarks() {
        if symbol.name.as_str() != trunk_name
            || symbol.remote == REMOTE_NAME_FOR_LOCAL_GIT_REPO
            || !target.is_tracked()
        {
            continue;
        }
        let configured =
            parse_remote_fetch_bookmarks(&env.ui, settings, symbol.remote).map_err(cmd_err)?;
        if configured.is_none_or(|expr| expr.to_matcher().is_match(trunk_name)) {
            remotes.push(symbol.remote);
        }
    }
    Ok(remotes)
}

/// Restack the bookmark graph on top of the trunk branch.
///
/// Uses the provided `repo` snapshot (instead of the potentially stale
/// `env.repo`) so that the rebase sees the freshly-fetched trunk position.
/// Returns the committed repo for downstream graph construction.
async fn restack_graph(
    env: &SpiceEnv,
    repo: &Arc<ReadonlyRepo>,
    root_bookmarks: &[String],
    trunk: &CommitId,
) -> Result<Arc<ReadonlyRepo>, Box<dyn std::error::Error>> {
    // Collect the tip commit IDs of all root bookmarks.
    let view = repo.view();
    let mut root_tip_ids = Vec::with_capacity(root_bookmarks.len());
    for bookmark in root_bookmarks {
        let commit_id = view
            .get_local_bookmark(RefName::new(bookmark.as_str()))
            .as_normal()
            .ok_or_else(|| format!("could not find root bookmark: {bookmark}"))?
            .clone();
        root_tip_ids.push(commit_id);
    }

    // Compute roots relative to the destination, matching jj's rebase --source logic.
    // This excludes commits that are already ancestors of trunk.
    let roots_expression: Arc<ResolvedRevsetExpression> =
        ResolvedRevsetExpression::commits(vec![trunk.clone()])
            .range(&ResolvedRevsetExpression::commits(root_tip_ids))
            .roots();
    let root_commit_ids: Vec<CommitId> = roots_expression
        .evaluate(repo.as_ref())?
        .stream()
        .try_collect()
        .await?;

    // Perform the rebase.
    let loc = MoveCommitsLocation {
        new_parent_ids: vec![trunk.clone()],
        new_child_ids: Vec::new(),
        target: MoveCommitsTarget::Roots(root_commit_ids),
    };
    let mut tx = repo.start_transaction();
    let stats = move_commits(tx.repo_mut(), &loc, &RebaseOptions::default()).await?;
    tx.repo_mut().rebase_descendants().await?;

    print_move_commits_stats(&env.ui, &stats)?;
    Ok(tx.commit("restack bookmarks on trunk").await?)
}

/// Print details about the provided [`MoveCommitsStats`].
fn print_move_commits_stats(ui: &Ui, stats: &MoveCommitsStats) -> std::io::Result<()> {
    let Some(mut formatter) = ui.status_formatter() else {
        return Ok(());
    };
    let &MoveCommitsStats {
        num_rebased_targets,
        num_rebased_descendants,
        num_skipped_rebases,
        num_abandoned_empty,
        rebased_commits: _,
    } = stats;
    if num_skipped_rebases > 0 {
        writeln!(
            formatter,
            "Skipped rebase of {num_skipped_rebases} commits that were already in place"
        )?;
    }
    if num_rebased_targets > 0 {
        writeln!(
            formatter,
            "Rebased {num_rebased_targets} commits to destination"
        )?;
    }
    if num_rebased_descendants > 0 {
        writeln!(
            formatter,
            "Rebased {num_rebased_descendants} descendant commits"
        )?;
    }
    if num_abandoned_empty > 0 {
        writeln!(
            formatter,
            "Abandoned {num_abandoned_empty} newly emptied commits"
        )?;
    }
    Ok(())
}

/// Write `spice.forges.<hostname>.type = <forge_type>` to the jj repo config.
fn persist_forge_config(
    env: &SpiceEnv,
    hostname: &str,
    forge_type: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = match env.config_env.repo_config_path(&env.ui).map_err(cmd_err)? {
        Some(p) => p,
        None => {
            writeln!(
                env.ui.warning_default(),
                "{hostname}: could not determine repo config path, forge choice not persisted"
            )?;
            return Ok(());
        }
    };

    let mut config_file = ConfigFile::load_or_empty(ConfigSource::Repo, config_path)?;
    config_file.set_value(&["spice", "forges", hostname, "type"][..], forge_type)?;
    config_file.save()?;

    writeln!(
        env.ui.status(),
        "{hostname}: saved as {forge_type} in repo config"
    )?;

    Ok(())
}

/// Try to find and select a change request for a single bookmark.
///
/// Returns `Some(ForgeMeta)` if a CR was found and selected, `None` if no CR
/// exists on any forge.
async fn sync_bookmark(
    ui: &Ui,
    bookmark: &Bookmark<'_>,
    forge_map: &HashMap<String, Box<dyn Forge>>,
    allow_inactive: bool,
) -> Result<Option<ForgeMeta>, BookmarkSyncError> {
    let tracked_remotes: Vec<&str> = bookmark.tracked_remotes().collect();
    if tracked_remotes.is_empty() {
        return Err(BookmarkSyncError::NoTrackedRemotes);
    }

    // Collect CRs across all forges reachable from tracked remotes, plus
    // any other forge in the map that may host cross-repo (fork) PRs.
    // This ensures PRs opened against an upstream repo are discovered even
    // when the bookmark is only tracked on the fork remote.
    let mut all_crs: Vec<ForgeMeta> = Vec::new();
    let mut queried_repos: HashSet<String> = HashSet::new();

    // 1. Query forges matching tracked remotes (primary lookup).
    //    Also collect their repo_ids — these are the potential fork
    //    identities used to search for cross-repo PRs in step 2.
    let mut found_forge = false;
    let mut tracked_repo_ids: Vec<String> = Vec::new();
    for remote_name in &tracked_remotes {
        let forge_instance = match forge_map.get(*remote_name) {
            Some(f) => f,
            None => continue,
        };
        found_forge = true;
        let repo_id = forge_instance.repo_id();
        queried_repos.insert(repo_id.clone());
        tracked_repo_ids.push(repo_id);

        let crs: Vec<_> = forge_instance
            .find_change_requests(bookmark.name(), None)
            .await
            .map_err(|e| e as Box<dyn std::error::Error>)?
            .iter()
            .filter_map(|cr| {
                // If --allow-inactive is not set, we remote change request being
                // closed or merged.
                if !allow_inactive && cr.as_ref().status().is_inactive() {
                    return None;
                }
                Some(cr.to_forge_meta())
            })
            .collect();

        all_crs.extend(crs);
    }

    // 2. Query remaining forges that weren't reached via tracked remotes.
    //    For each, try with every tracked forge as a potential source (fork)
    //    so cross-repo PRs are found via the correct head filter
    //    (e.g. "fork-owner:branch" instead of "upstream-owner:branch").
    for forge_instance in forge_map.values() {
        if queried_repos.contains(&forge_instance.repo_id()) {
            continue;
        }
        for source_id in &tracked_repo_ids {
            let crs: Vec<_> = forge_instance
                .find_change_requests(bookmark.name(), Some(source_id))
                .await
                .map_err(|e| e as Box<dyn std::error::Error>)?
                .iter()
                .map(|cr| cr.to_forge_meta())
                .collect();
            if !crs.is_empty() {
                found_forge = true;
                all_crs.extend(crs);
            }
        }
    }

    if !found_forge {
        return Err(BookmarkSyncError::NoForgeDetected);
    }

    // Dedup by PR identity (multiple remotes may point to the same forge).
    let mut seen = HashSet::new();
    all_crs.retain(|cr| seen.insert(cr.clone()));

    match all_crs.len() {
        0 => Ok(None),
        1 => Ok(Some(all_crs.into_iter().next().unwrap())),
        _ => {
            // Multiple CRs found — prompt user to select one.
            writeln!(
                ui.status(),
                "{}: found {} change requests, which should be tracked?",
                bookmark.name(),
                all_crs.len()
            )?;
            for (i, cr) in all_crs.iter().enumerate() {
                writeln!(ui.status(), "  {i}: {cr}")?;
            }

            let choices: Vec<String> = (0..all_crs.len()).map(|i| i.to_string()).collect();
            let index = ui.prompt_choice("Select", &choices, Some(0))?;

            Ok(Some(all_crs.into_iter().nth(index).unwrap()))
        }
    }
}

#[cfg(test)]
mod tests {
    use jj_lib::git;
    use jj_lib::op_store::RefTarget;
    use jj_spice_lib::protos::change_request::forge_meta::Forge as ForgeOneof;
    use jj_spice_lib::protos::change_request::{ForgeMeta, GitHubMeta};

    use super::*;
    use crate::commands::env::test_repo;

    #[tokio::test]
    async fn fetch_trunk_honors_patterns_and_rebases_descendants_of_rewritten_trunk() {
        let origin_path = tempfile::tempdir().unwrap();
        let local_path = tempfile::tempdir().unwrap();
        let origin = test_repo::init(origin_path.path(), true).await;
        let local = test_repo::init(local_path.path(), false).await;
        let trunk = RefName::new("main");
        let origin_name = RemoteName::new("origin");
        let root = origin.repo.store().root_commit_id();

        let mut tx = origin.repo.start_transaction();
        let first = test_repo::commit(tx.repo_mut(), root, "Initial trunk").await;
        tx.repo_mut()
            .set_local_bookmark_target(trunk, RefTarget::normal(first.id().clone()));
        git::export_refs(tx.repo_mut()).unwrap();
        let origin_repo = tx.commit("Set trunk").await.unwrap();

        let mut tx = local.repo.start_transaction();
        test_repo::add_origin(tx.repo_mut(), &origin);
        tx.commit("Add origin").await.unwrap();
        let local = test_repo::open(local_path.path(), &[]);
        let mut tx = local.repo.start_transaction();
        let import_options = load_git_import_options(
            &local.ui,
            &local.git_settings,
            &local.settings.remote_settings().unwrap(),
        )
        .unwrap();
        let mut fetch = GitFetch::new(
            tx.repo_mut(),
            local.git_settings.to_subprocess_options(),
            &import_options,
        )
        .unwrap();
        fetch
            .fetch(
                origin_name,
                &GitFetchRefExpression {
                    bookmark: StringExpression::Pattern(Box::new(StringPattern::Exact(
                        trunk.as_str().to_owned(),
                    ))),
                    tag: StringExpression::none(),
                },
                &mut GitSubprocessUi::new(&local.ui),
                None,
            )
            .unwrap();
        fetch.import_refs().await.unwrap();
        tx.repo_mut()
            .track_remote_bookmark(trunk.to_remote_symbol(origin_name))
            .await
            .unwrap();
        let repo = tx.commit("Track trunk").await.unwrap();
        assert_eq!(
            repo.view().get_local_bookmark(trunk).as_normal(),
            Some(first.id())
        );

        let mut tx = repo.start_transaction();
        let child = test_repo::commit(tx.repo_mut(), first.id(), "Local stack").await;
        let topic = RefName::new("topic");
        tx.repo_mut()
            .set_local_bookmark_target(topic, RefTarget::normal(child.id().clone()));
        tx.commit("Build local stack").await.unwrap();

        let mut tx = origin_repo.start_transaction();
        let rewritten = tx
            .repo_mut()
            .rewrite_commit(&first)
            .set_description("Rewritten trunk")
            .write()
            .await
            .unwrap();
        tx.repo_mut()
            .set_local_bookmark_target(trunk, RefTarget::normal(rewritten.id().clone()));
        let side = RefName::new("side");
        tx.repo_mut()
            .set_local_bookmark_target(side, RefTarget::normal(rewritten.id().clone()));
        tx.repo_mut().rebase_descendants().await.unwrap();
        git::export_refs(tx.repo_mut()).unwrap();
        tx.commit("Rewrite trunk").await.unwrap();

        let excluded = test_repo::open(
            local_path.path(),
            &["remotes.origin.fetch-bookmarks='~main'"],
        );
        let repo = fetch_trunk(&excluded, trunk.as_str()).await.unwrap();
        assert_eq!(
            repo.view().get_local_bookmark(trunk).as_normal(),
            Some(first.id())
        );

        let included = test_repo::open(
            local_path.path(),
            &["remotes.origin.fetch-bookmarks='main | side'"],
        );
        let repo = fetch_trunk(&included, trunk.as_str()).await.unwrap();
        assert_eq!(
            repo.view().get_local_bookmark(trunk).as_normal(),
            Some(rewritten.id())
        );
        assert!(
            repo.view()
                .get_remote_bookmark(side.to_remote_symbol(origin_name))
                .target
                .is_absent()
        );
        let tip = repo.view().get_local_bookmark(topic).as_normal().unwrap();
        let rebased = repo.store().get_commit(tip).unwrap();
        assert_eq!(rebased.change_id(), child.change_id());
        assert_eq!(rebased.parent_ids(), [rewritten.id().clone()]);
    }

    #[test]
    fn forge_meta_display_github_variant() {
        let meta = ForgeMeta {
            forge: Some(ForgeOneof::Github(GitHubMeta {
                number: 42,
                source_branch: "feat".into(),
                target_branch: "main".into(),
                source_repo: String::new(),
                target_repo: String::new(),
                graphql_id: String::new(),
                comment_id: None,
            })),
        };
        assert_eq!(meta.to_string(), "GitHub PR #42 (feat → main)");
    }

    async fn restack_working_copy(colocated: bool) -> bool {
        let path = tempfile::tempdir().unwrap();
        let mut env = test_repo::init(path.path(), colocated).await;
        let root = env.repo.store().root_commit_id().clone();
        let mut tx = env.repo.start_transaction();
        use jj_lib::backend::{CopyId, TreeValue};
        use jj_lib::merged_tree_builder::MergedTreeBuilder;
        use jj_lib::repo_path::RepoPath;

        let store = tx.repo().store();
        let mut tree = MergedTreeBuilder::new(store.empty_merged_tree());
        let file = RepoPath::from_internal_string("trunk-file").unwrap();
        let id = store
            .write_file(file, &mut &b"new trunk content"[..])
            .await
            .unwrap();
        tree.set_or_remove(
            file.to_owned(),
            Some(TreeValue::File {
                id,
                executable: false,
                copy_id: CopyId::placeholder(),
            }),
        );
        let tree = tree.write_tree().await.unwrap();
        let trunk = tx
            .repo_mut()
            .new_commit(vec![root.clone()], tree)
            .set_description("New trunk")
            .write()
            .await
            .unwrap();
        let topic = test_repo::commit(tx.repo_mut(), &root, "Topic").await;
        tx.repo_mut().set_local_bookmark_target(
            RefName::new("topic"),
            RefTarget::normal(topic.id().clone()),
        );
        tx.repo_mut()
            .set_wc_commit(
                env.workspace.workspace_name().to_owned(),
                topic.id().clone(),
            )
            .unwrap();
        let repo = tx.commit("Build stack").await.unwrap();
        env.workspace
            .check_out(repo.op_id().clone(), None, &topic)
            .await
            .unwrap();

        let repo = restack_graph(&env, &repo, &["topic".to_owned()], trunk.id())
            .await
            .unwrap();
        let wc_id = repo
            .view()
            .get_wc_commit_id(env.workspace.workspace_name())
            .unwrap();
        let rebased = repo.store().get_commit(wc_id).unwrap();
        assert_eq!(rebased.change_id(), topic.change_id());
        assert_eq!(rebased.parent_ids(), [trunk.id().clone()]);
        test_repo::is_fresh(&env, &repo, &rebased).await
    }

    #[tokio::test]
    #[ignore = "Blocked by PR #87: restack does not update the on-disk working copy"]
    async fn restack_working_copy_freshness() {
        let colocated = restack_working_copy(true).await;
        let internal_git = restack_working_copy(false).await;
        assert_eq!(
            (colocated, internal_git),
            (true, true),
            "fresh working copy (colocated, non-colocated)"
        );
    }

    #[test]
    fn forge_meta_display_none_variant() {
        let meta = ForgeMeta { forge: None };
        assert_eq!(meta.to_string(), "unknown forge");
    }
}
