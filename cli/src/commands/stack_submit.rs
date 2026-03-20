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

        let (suggested_title, suggested_body) =
            build_cr_suggestion(env.repo.as_ref(), bookmark_node.commits())?;

        let title_prompt = if suggested_title.is_empty() {
            "Title".to_string()
        } else {
            format!("Title [{}]", suggested_title)
        };
        let title = env.ui.prompt_choice_with(
            &title_prompt,
            if suggested_title.is_empty() {
                None
            } else {
                Some(suggested_title.as_str())
            },
            |input| -> Result<String, &'static str> { Ok(input.to_string()) },
        )?;
        let description = text_editor.edit_str(&suggested_body, Some(".md"))?;
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

/// Build a suggested title and body for a change request from commit
/// descriptions.
///
/// The title is the first line of the first commit's description. The body
/// contains the full descriptions of every commit in the bookmark, separated
/// by blank lines when there are multiple commits. The title line is stripped
/// from the first commit's contribution to the body to avoid duplication.
fn build_cr_suggestion(
    repo: &dyn jj_lib::repo::Repo,
    commit_ids: &[CommitId],
) -> Result<(String, String), Box<dyn std::error::Error>> {
    if commit_ids.is_empty() {
        return Ok((String::new(), String::new()));
    }

    let mut descriptions: Vec<String> = Vec::with_capacity(commit_ids.len());
    for id in commit_ids {
        let commit = repo.store().get_commit(id)?;
        let desc = commit.description().trim().to_string();
        if !desc.is_empty() {
            descriptions.push(desc);
        }
    }

    if descriptions.is_empty() {
        return Ok((String::new(), String::new()));
    }

    // Title: first line of the first commit description, trimmed.
    let title = descriptions[0]
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();

    // Body: strip the title line from the first description, then concatenate
    // all descriptions separated by blank lines.
    let first_remainder = descriptions[0]
        .strip_prefix(&title)
        .unwrap_or(&descriptions[0])
        .trim_start_matches('\n')
        .trim()
        .to_string();

    let mut body_parts: Vec<&str> = Vec::new();
    if !first_remainder.is_empty() {
        body_parts.push(&first_remainder);
    }
    for desc in &descriptions[1..] {
        body_parts.push(desc);
    }

    let body = body_parts.join("\n\n");

    Ok((title, body))
}

#[cfg(test)]
mod tests {
    /// Helper that exercises the suggestion-building logic without a real repo
    /// by testing the pure string-processing portion directly.
    fn suggestion_from_descriptions(descriptions: &[&str]) -> (String, String) {
        // Re-implement the pure logic portion so we can test without jj_lib
        // infrastructure. The real function fetches commits from the repo; here
        // we inline the string processing.
        if descriptions.is_empty() {
            return (String::new(), String::new());
        }

        let descriptions: Vec<String> = descriptions
            .iter()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .collect();

        if descriptions.is_empty() {
            return (String::new(), String::new());
        }

        let title = descriptions[0]
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();

        let first_remainder = descriptions[0]
            .strip_prefix(title.as_str())
            .unwrap_or(&descriptions[0])
            .trim_start_matches('\n')
            .trim()
            .to_string();

        let mut body_parts: Vec<&str> = Vec::new();
        if !first_remainder.is_empty() {
            body_parts.push(&first_remainder);
        }
        for desc in &descriptions[1..] {
            body_parts.push(desc);
        }

        let body = body_parts.join("\n\n");
        (title, body)
    }

    #[test]
    fn single_commit_single_line() {
        let (title, body) = suggestion_from_descriptions(&["Add user authentication"]);
        assert_eq!(title, "Add user authentication");
        assert_eq!(body, "");
    }

    #[test]
    fn single_commit_multi_line() {
        let (title, body) = suggestion_from_descriptions(&[
            "Add user authentication\n\nThis implements OAuth2 flow\nwith token refresh.",
        ]);
        assert_eq!(title, "Add user authentication");
        assert_eq!(body, "This implements OAuth2 flow\nwith token refresh.");
    }

    #[test]
    fn multiple_commits() {
        let (title, body) = suggestion_from_descriptions(&[
            "Add login endpoint",
            "Add logout endpoint\n\nClears the session cookie.",
        ]);
        assert_eq!(title, "Add login endpoint");
        assert_eq!(body, "Add logout endpoint\n\nClears the session cookie.");
    }

    #[test]
    fn multiple_commits_first_has_body() {
        let (title, body) =
            suggestion_from_descriptions(&["Add login\n\nWith rate limiting.", "Add logout"]);
        assert_eq!(title, "Add login");
        assert_eq!(body, "With rate limiting.\n\nAdd logout");
    }

    #[test]
    fn empty_descriptions() {
        let (title, body) = suggestion_from_descriptions(&["", "  ", ""]);
        assert_eq!(title, "");
        assert_eq!(body, "");
    }

    #[test]
    fn no_commits() {
        let (title, body) = suggestion_from_descriptions(&[]);
        assert_eq!(title, "");
        assert_eq!(body, "");
    }

    #[test]
    fn whitespace_trimmed() {
        let (title, body) = suggestion_from_descriptions(&["  Fix the bug  \n\n  Details here  "]);
        assert_eq!(title, "Fix the bug");
        assert_eq!(body, "Details here");
    }
}
