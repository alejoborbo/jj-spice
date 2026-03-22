use std::io::Write as _;

use itertools::Itertools;
use jj_cli::description_util::TextEditor;
use jj_cli::git_util::{GitSubprocessUi, print_push_stats};
use jj_lib::backend::CommitId;
use jj_lib::git::{self, GitBranchPushTargets};
use jj_lib::ref_name::{RefNameBuf, RemoteNameBuf};
use jj_lib::refs::{BookmarkPushAction, BookmarkPushUpdate, classify_bookmark_push_action};
use jj_spice_lib::bookmark::Bookmark;
use jj_spice_lib::bookmark::graph::BookmarkGraph;
use jj_spice_lib::comments::Comment;
use jj_spice_lib::forge::{CreateParams, Forge};
use jj_spice_lib::protos::change_request::{ChangeRequests, ForgeMeta};
use jj_spice_lib::store::SpiceStore;
use jj_spice_lib::store::change_request::ChangeRequestStore;

use crate::commands::env::SpiceEnv;

/// Create change requests for each bookmark in the current stack (trunk..@).
///
/// `source_repo` identifies the fork repository for cross-repo PRs; it is
/// `None` in single-remote mode. See [`CreateParams::source_repo`] for the
/// forge-specific format.
pub async fn run(
    env: &SpiceEnv,
    forge: &dyn Forge,
    source_repo: Option<&str>,
    store: &SpiceStore,
    trunk: &CommitId,
    head: &CommitId,
    trunk_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cr_store = ChangeRequestStore::new(store);
    let graph = BookmarkGraph::new(env.repo.as_ref(), trunk, head)?;
    let text_editor = TextEditor::from_settings(&env.settings)?;
    let mut state = cr_store.load()?;

    // Iter on the graphs to create the change requests.
    for bookmark_node in graph.iter_graph()? {
        let bookmark = bookmark_node.bookmark();
        let ascendants = bookmark_node.ascendants();

        // Check for untracked changes in the bookmark and push them if the user agrees.
        check_untracked_changes(&env.ui, env, bookmark)?;

        // If the change request already exists, retarget if needed.
        let existing =
            get_existing_change_request(&env.ui, &state, forge, bookmark.name(), source_repo)
                .await?;

        let base_bookmark = match ascendants.len() {
            0 => trunk_name,
            1 => ascendants.first().unwrap().as_str(),
            _ => {
                writeln!(env.ui.stdout_formatter(), "Multiple base bookmarks found:")?;
                for (i, a) in ascendants.iter().enumerate() {
                    writeln!(env.ui.stdout_formatter(), "  {}: {}", i, a)?;
                }

                let choices: Vec<String> = (0..ascendants.len()).map(|i| i.to_string()).collect();
                let index = env
                    .ui
                    .prompt_choice("Select base bookmark", &choices, Some(0))?;

                ascendants[index].as_str()
            }
        };

        if let Some(meta) = existing {
            match meta.target_branch() {
                Some(tb) if tb != base_bookmark => {
                    let cr = forge.update_base(&meta, base_bookmark).await?;
                    state.set(bookmark.name().to_string(), cr.to_forge_meta());
                    writeln!(
                        env.ui.stdout_formatter(),
                        "Base branch has been retargeted to {}, updating change request: {}",
                        base_bookmark,
                        cr.id(),
                    )?;
                }
                _ => {
                    writeln!(
                        env.ui.warning_default(),
                        "{}: already tracked, skipping",
                        bookmark.name(),
                    )?;
                }
            }
            continue;
        }

        writeln!(
            env.ui.stdout_formatter(),
            "Creating change request for: {}",
            bookmark.name()
        )?;

        writeln!(
            env.ui.stdout_formatter(),
            "Base bookmark: {}",
            base_bookmark
        )?;

        let title = env.ui.prompt("Title")?;
        let description = text_editor.edit_str("", Some(".md"))?;
        let is_draft = env.ui.prompt_yes_no("Draft?", Some(false))?;

        let params = CreateParams {
            source_branch: bookmark.name(),
            target_branch: base_bookmark,
            title: &title,
            body: Some(&description),
            is_draft,
            source_repo,
        };

        let cr = forge.create(params).await?;
        state.set(bookmark.name().to_string(), cr.to_forge_meta());

        writeln!(
            env.ui.stdout_formatter(),
            "Created change request: {}",
            cr.url()
        )?;
    }

    // Post stack-trace comments on all CRs concurrently.
    post_stack_comments(forge, &graph, &mut state).await?;

    // Persist CRs and comment IDs to disk.
    cr_store.save(&state)?;

    Ok(())
}

/// Post stack-trace comments on all change requests concurrently.
async fn post_stack_comments(
    forge: &dyn Forge,
    graph: &BookmarkGraph<'_>,
    state: &mut ChangeRequests,
) -> Result<(), Box<dyn std::error::Error>> {
    // Build comment text + metadata for each bookmark in one pass.
    let comment_tasks: Vec<(String, ForgeMeta, String)> = graph
        .iter_graph()
        .unwrap()
        .unique_by(|n| n.bookmark().name())
        .map(|n| {
            let bookmark = n.bookmark();
            let meta = state.get(bookmark.name()).unwrap().clone();
            let comment_text = Comment::new(bookmark, graph, state)
                .to_string()
                .expect("Failed to serialize comment");
            (bookmark.name().to_string(), meta, comment_text)
        })
        .collect();

    // Fire all comment API calls concurrently.
    let futures: Vec<_> = comment_tasks
        .iter()
        .map(|(_, meta, comment_text)| forge.update_or_create_comment(meta, comment_text))
        .collect();
    let results = futures::future::join_all(futures).await;

    // Collect results and update state with comment IDs.
    for (result, (name, _, _)) in results.into_iter().zip(&comment_tasks) {
        let comment_id = result?;
        let mut updated_meta = state.get(name).unwrap().clone();
        updated_meta.set_comment_id(comment_id);
        state.set(name.clone(), updated_meta);
    }

    Ok(())
}

/// Check for untracked changes in the bookmark and push them if the user agrees.
fn check_untracked_changes(
    ui: &jj_cli::ui::Ui,
    env: &SpiceEnv,
    bookmark: &Bookmark,
) -> Result<(), Box<dyn std::error::Error>> {
    let remote = env.get_default_remote();
    let local_remote_target = bookmark.remote_ref(&remote).ok_or_else(|| {
        let _ = writeln!(
            ui.hint_default(),
            "No remote ref found for bookmark {name}. Run `jj bookmark track {name} --remote={remote}` to \
             track it.",
            name = bookmark.name(),
            remote = remote.as_symbol(),
        );
        format!("No remote ref found for bookmark {}", bookmark.name())
    })?;
    match classify_bookmark_push_action(local_remote_target) {
        BookmarkPushAction::AlreadyMatches => {}
        BookmarkPushAction::Update(push_update) => {
            writeln!(
                ui.warning_default(),
                "Untracked changes have been detected. Do you want to push them?",
            )?;
            if ui.prompt_yes_no("Push changes?", Some(true))? {
                push_bookmarks(env, &remote, bookmark, push_update)?;
                writeln!(
                    ui.stdout_formatter(),
                    "Pushed {} to {}",
                    bookmark.name(),
                    remote.as_str(),
                )?;
            }
        }
        action => {
            writeln!(
                ui.warning_default(),
                "Bookmark {} has unexpected state: {:?}",
                bookmark.name(),
                action,
            )?;
        }
    }
    Ok(())
}

/// Look up an existing change request for a bookmark.
///
/// 1. Check local state first — if already tracked, return it.
/// 2. Query the forge for CRs matching source/target branches.
/// 3. If multiple CRs are found, prompt the user to pick one.
///
/// `source_repo` is forwarded to [`Forge::find_change_requests`] for
/// cross-repo PR discovery; pass `None` in single-remote mode.
async fn get_existing_change_request(
    ui: &jj_cli::ui::Ui,
    state: &ChangeRequests,
    forge: &dyn Forge,
    bookmark: &str,
    source_repo: Option<&str>,
) -> Result<Option<ForgeMeta>, Box<dyn std::error::Error>> {
    // Check local state first.
    if let Some(meta) = state.get(bookmark) {
        return Ok(Some(meta.clone()));
    }

    // Query the forge.
    let metas = forge.find_change_requests(bookmark, source_repo).await?;

    match metas.len() {
        0 => Ok(None),
        1 => Ok(Some(metas.into_iter().next().unwrap())),
        _ => {
            writeln!(
                ui.warning_default(),
                "{bookmark}: found {} change requests on the forge",
                metas.len()
            )?;
            for (i, meta) in metas.iter().enumerate() {
                writeln!(ui.stdout_formatter(), "  {i}: {meta}")?;
            }
            writeln!(ui.stdout_formatter(), "  n: Create a new change request")?;

            let choices: Vec<String> = (0..metas.len())
                .map(|i| i.to_string())
                .chain(std::iter::once("n".into()))
                .collect();

            let index = ui.prompt_choice("Select change request", &choices, Some(0))?;

            // If the user selected "n", create a new change request.
            // Pretending no change request was found.
            if index == metas.len() {
                return Ok(None);
            }

            Ok(Some(metas.into_iter().nth(index).unwrap()))
        }
    }
}

fn push_bookmarks(
    env: &SpiceEnv,
    remote_name: &RemoteNameBuf,
    bookmark: &Bookmark,
    push_update: BookmarkPushUpdate,
) -> Result<(), Box<dyn std::error::Error>> {
    let targets = GitBranchPushTargets {
        branch_updates: vec![(RefNameBuf::from(bookmark.name()), push_update)],
    };

    let mut tx = env.repo.start_transaction();
    let push_stats = git::push_branches(
        tx.repo_mut(),
        env.git_settings.to_subprocess_options(),
        remote_name.as_ref(),
        &targets,
        &mut GitSubprocessUi::new(&env.ui),
    )?;

    print_push_stats(&env.ui, &push_stats)?;
    if push_stats.all_ok() {
        Ok(())
    } else {
        Err("Failed to push some bookmarks".into())
    }
}
