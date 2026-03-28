use std::collections::{HashMap, HashSet};
use std::io::Write;

use jj_cli::git_util::GitSubprocessUi;
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::config::{ConfigFile, ConfigSource};
use jj_lib::git::{GitFetch, GitFetchRefExpression, GitImportOptions, expand_fetch_refspecs};
use jj_lib::ref_name::RemoteName;
use jj_lib::repo::Repo;
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
    let graph = BookmarkGraph::build_active_graph(env.repo.as_ref(), trunk, head)?;
    let DetectionResult {
        mut forges,
        unmatched,
    } = detect_forges(env.repo.store(), env.config())?;

    // Prompt for unmatched remotes (grouped by hostname).
    resolve_unmatched_remotes(env, &unmatched, &mut forges)?;

    // Fetch the latest remote changes for the trunk branch.
    fetch_trunk(env, trunk_name)?;

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

    cr_store.save(&state)?;
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

/// Fetch the latest remote changes for trunk
fn fetch_trunk(env: &SpiceEnv, trunk_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Get all trunk remotes.
    let trunk_remotes: Vec<&RemoteName> = env
        .repo
        .view()
        .bookmarks()
        .filter(|(ref_name, _)| ref_name.as_str() == trunk_name)
        .flat_map(|(_, ref_target)| {
            ref_target
                .remote_refs
                .iter()
                .filter_map(|(remote_name, remote_ref)| {
                    // Exclude the "git" remote, which is used for internal tracking.
                    if !remote_ref.is_tracked() || *remote_name == "git" {
                        return None;
                    }
                    Some(*remote_name)
                })
                .collect::<Vec<_>>()
        })
        .collect();

    // Build the fetch refspecs that are used to fetch the latest changes
    let mut fetch_refs = HashMap::new();
    for remote_name in &trunk_remotes {
        let expr = GitFetchRefExpression {
            bookmark: StringExpression::Pattern(Box::new(StringPattern::Exact(
                trunk_name.to_string(),
            ))),
            tag: StringExpression::none(),
        };
        fetch_refs.insert(*remote_name, expand_fetch_refspecs(remote_name, expr)?);
    }

    let mut tx = env.repo.start_transaction();
    let import_options = GitImportOptions {
        auto_local_bookmark: env.git_settings.auto_local_bookmark,
        abandon_unreachable_commits: env.git_settings.abandon_unreachable_commits,
        remote_auto_track_bookmarks: HashMap::new(),
    };
    let mut git_fetch = GitFetch::new(
        tx.repo_mut(),
        env.git_settings.to_subprocess_options(),
        &import_options,
    )?;

    // Fetch changes from each remote
    for (remote_name, expanded_fetch_refspecs) in fetch_refs {
        let mut callback = GitSubprocessUi::new(&env.ui);
        git_fetch.fetch(
            remote_name,
            expanded_fetch_refspecs,
            &mut callback,
            None,
            None,
        )?;
    }

    tx.commit("fetch trunk")?;

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
            .await?
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
                .await?
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
    use jj_spice_lib::protos::change_request::forge_meta::Forge as ForgeOneof;
    use jj_spice_lib::protos::change_request::{ForgeMeta, GitHubMeta};

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

    #[test]
    fn forge_meta_display_none_variant() {
        let meta = ForgeMeta { forge: None };
        assert_eq!(meta.to_string(), "unknown forge");
    }
}
