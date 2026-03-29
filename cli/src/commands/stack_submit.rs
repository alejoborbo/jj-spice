use std::collections::HashMap;
use std::io::Write as _;

use itertools::Itertools;
use jj_cli::description_util::TextEditor;
use jj_cli::git_util::{GitSubprocessUi, print_push_stats};
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::git::{self, GitBranchPushTargets};
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::{RefNameBuf, RemoteNameBuf};
use jj_lib::refs::{BookmarkPushAction, BookmarkPushUpdate, classify_bookmark_push_action};
use jj_lib::signing::SignBehavior;
use jj_spice_lib::bookmark::Bookmark;
use jj_spice_lib::bookmark::graph::BookmarkGraph;
use jj_spice_lib::comments::Comment;
use jj_spice_lib::forge::{CreateParams, Forge};
use jj_spice_lib::protos::change_request::{ChangeRequests, ForgeMeta};
use jj_spice_lib::store::change_request::ChangeRequestStore;

use crate::commands::cli::SubmitArgs;
use crate::commands::env::SpiceEnv;

/// Create change requests for each bookmark in the current stack (trunk..@).
///
/// `source_repo` identifies the fork repository for cross-repo PRs; it is
/// `None` in single-remote mode. See [`CreateParams::source_repo`] for the
/// forge-specific format.
pub async fn run(
    args: &SubmitArgs,
    env: &SpiceEnv,
    forge: &dyn Forge,
    source_repo: Option<&str>,
    trunk: &CommitId,
    head: &CommitId,
    trunk_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = &env.store;
    let cr_store = ChangeRequestStore::new(store);
    let graph = BookmarkGraph::build_active_graph(env.repo.as_ref(), trunk, head)?;
    let text_editor = TextEditor::from_settings(&env.settings)?;
    let mut state = cr_store.load()?;

    // Iter on the graphs to create the change requests.
    for bookmark_node in graph.iter_graph()? {
        let bookmark = bookmark_node.bookmark();
        let ascendants = bookmark_node.ascendants();

        // Check for untracked changes in the bookmark and push them if the user agrees.
        check_untracked_changes(
            &env.ui,
            env,
            bookmark,
            bookmark_node.commits(),
            args.auto_accept,
        )?;

        // If the change request already exists, retarget if needed.
        let existing = get_existing_change_request(
            &env.ui,
            &state,
            forge,
            bookmark.name(),
            source_repo,
            args.allow_inactive,
        )
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
        .iter_graph()?
        .unique_by(|n| n.bookmark().name())
        .map(|n| {
            let bookmark = n.bookmark();
            let name = bookmark.name();
            let meta = state
                .get(name)
                .ok_or_else(|| format!("no change request found for bookmark '{name}'"))?
                .clone();
            let comment_text = Comment::new(bookmark, graph, state).to_string()?;
            Ok((name.to_string(), meta, comment_text))
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;

    // Fire all comment API calls concurrently.
    let futures: Vec<_> = comment_tasks
        .iter()
        .map(|(_, meta, comment_text)| forge.update_or_create_comment(meta, comment_text))
        .collect();
    let results = futures::future::join_all(futures).await;

    // Collect results and update state with comment IDs.
    for (result, (name, _, _)) in results.into_iter().zip(&comment_tasks) {
        let comment_id = result?;
        let mut updated_meta = state
            .get(name)
            .ok_or_else(|| format!("no change request found for bookmark '{name}'"))?
            .clone();
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
    commit_ids: &[CommitId],
    auto_accept: bool,
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
            // Validate commits are ready to push (description, author, conflicts).
            // Returns commits that need signing.
            let commits_to_sign = verify_commits(env, env.repo.as_ref(), commit_ids)?;

            writeln!(
                ui.warning_default(),
                "Untracked changes have been detected.",
            )?;
            let should_push = if auto_accept {
                writeln!(
                    ui.stdout_formatter(),
                    "Auto accept is enabled, pushing commits to {}",
                    remote.as_str(),
                )?;
                true
            } else {
                ui.prompt_yes_no(
                    &format!("Do you want to push them to {}?", remote.as_str(),),
                    Some(true),
                )?
            };

            if should_push {
                push_bookmarks(env, &remote, bookmark, push_update, commits_to_sign)?;
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
    allow_inactive: bool,
) -> Result<Option<ForgeMeta>, Box<dyn std::error::Error>> {
    // Check local state first.
    if let Some(meta) = state.get(bookmark) {
        return Ok(Some(meta.clone()));
    }

    // Query the forge.
    let metas = forge
        .find_change_requests(bookmark, source_repo)
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
        .collect::<Vec<_>>();

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

/// Push a bookmark to the remote, signing unsigned commits first if needed.
///
/// Signing rewrites commits (producing new IDs), so the push target is
/// remapped after signing. Both signing and pushing happen in the same
/// transaction to keep the repo state consistent.
///
/// Mirrors jj's `sign_commits_before_push` + `push_branches` flow.
fn push_bookmarks(
    env: &SpiceEnv,
    remote_name: &RemoteNameBuf,
    bookmark: &Bookmark,
    mut push_update: BookmarkPushUpdate,
    commits_to_sign: Vec<Commit>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = env.repo.start_transaction();

    // Sign unsigned commits before pushing (if any).
    if !commits_to_sign.is_empty() {
        let commit_ids: Vec<CommitId> = commits_to_sign.iter().map(|c| c.id().clone()).collect();
        let mut old_to_new: HashMap<CommitId, CommitId> = HashMap::new();

        tx.repo_mut()
            .transform_descendants(commit_ids.clone(), async |rewriter| {
                let old_id = rewriter.old_commit().id().clone();
                let new_commit: Commit = rewriter
                    .reparent()
                    .set_sign_behavior(SignBehavior::Own)
                    .write()?;
                old_to_new.insert(old_id, new_commit.id().clone());
                Ok(())
            })?;

        // Remap push target to the newly signed commit ID.
        if let Some(old_target) = &push_update.new_target
            && let Some(new_id) = old_to_new.get(old_target)
        {
            push_update.new_target = Some(new_id.clone());
        }

        writeln!(
            env.ui.status(),
            "Signed {} commit(s)",
            commits_to_sign.len()
        )?;
    }

    let targets = GitBranchPushTargets {
        branch_updates: vec![(RefNameBuf::from(bookmark.name()), push_update)],
    };
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

/// Verify commits before pushing them.
///
/// Checks that all commits are ready to push: non-empty description,
/// author/committer set, and no unresolved conflicts. If `git.sign-on-push`
/// is configured, collects unsigned commits that need signing.
///
/// Mirrors jj's `validate_commits_ready_to_push`.
fn verify_commits(
    env: &SpiceEnv,
    repo: &dyn jj_lib::repo::Repo,
    commit_ids: &[CommitId],
) -> Result<Vec<Commit>, Box<dyn std::error::Error>> {
    let sign_on_push = env
        .settings
        .get_bool(["git", "sign-on-push"])
        .unwrap_or(false);
    // Override behavior to Own so commits authored by us are signed before
    // push, regardless of the configured signing.behavior (which may be Drop
    // for normal operations). Mirrors jj's own git push flow.
    let sign_settings = if sign_on_push {
        let mut settings = env.settings.sign_settings();
        settings.behavior = SignBehavior::Own;
        Some(settings)
    } else {
        None
    };

    let mut commits_to_sign = vec![];

    for id in commit_ids {
        let commit = repo.store().get_commit(id)?;
        let mut reasons = vec![];

        if commit.description().is_empty() {
            reasons.push("it has no description");
        }
        if commit.author().name.is_empty()
            || commit.author().email.is_empty()
            || commit.committer().name.is_empty()
            || commit.committer().email.is_empty()
        {
            reasons.push("it has no author and/or committer set");
        }
        if commit.has_conflict() {
            reasons.push("it has conflicts");
        }

        if !reasons.is_empty() {
            return Err(format!(
                "Won't push commit {} since {}",
                &id.hex()[..12],
                reasons.join(" and ")
            )
            .into());
        }

        // Collect commits that need signing before push.
        if let Some(ref settings) = sign_settings
            && !commit.is_signed()
            && settings.should_sign(commit.store_commit())
        {
            commits_to_sign.push(commit);
        }
    }

    Ok(commits_to_sign)
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
    use jj_lib::backend::{
        ChangeId, Commit, MillisSinceEpoch, SecureSig, Signature, Timestamp, TreeId,
    };
    use jj_lib::merge::Merge;
    use jj_lib::settings::SignSettings;
    use jj_lib::signing::SignBehavior;

    /// Build a minimal `backend::Commit` for sign-settings tests.
    fn make_commit(email: &str, signed: bool) -> Commit {
        let ts = Timestamp {
            timestamp: MillisSinceEpoch(0),
            tz_offset: 0,
        };
        let sig = Signature {
            name: "Test".into(),
            email: email.into(),
            timestamp: ts,
        };
        Commit {
            parents: vec![],
            predecessors: vec![],
            root_tree: Merge::resolved(TreeId::new(vec![0])),
            conflict_labels: Merge::resolved(String::new()),
            change_id: ChangeId::new(vec![0]),
            description: "test commit".into(),
            author: sig.clone(),
            committer: sig,
            secure_sig: if signed {
                Some(SecureSig {
                    data: vec![],
                    sig: vec![],
                })
            } else {
                None
            },
        }
    }

    fn sign_settings(behavior: SignBehavior, email: &str) -> SignSettings {
        SignSettings {
            behavior,
            user_email: email.into(),
            key: None,
        }
    }

    // -- sign-on-push behavior override tests --------------------------------

    #[test]
    fn drop_behavior_never_signs() {
        let settings = sign_settings(SignBehavior::Drop, "me@example.com");
        let commit = make_commit("me@example.com", false);
        assert!(!settings.should_sign(&commit));
    }

    #[test]
    fn own_behavior_signs_own_unsigned_commit() {
        let settings = sign_settings(SignBehavior::Own, "me@example.com");
        let commit = make_commit("me@example.com", false);
        assert!(settings.should_sign(&commit));
    }

    #[test]
    fn own_behavior_skips_other_author() {
        let settings = sign_settings(SignBehavior::Own, "me@example.com");
        let commit = make_commit("other@example.com", false);
        assert!(!settings.should_sign(&commit));
    }

    #[test]
    fn own_behavior_re_signs_already_signed_own_commit() {
        let settings = sign_settings(SignBehavior::Own, "me@example.com");
        let commit = make_commit("me@example.com", true);
        assert!(settings.should_sign(&commit));
    }

    #[test]
    fn keep_behavior_only_signs_already_signed_own_commit() {
        let settings = sign_settings(SignBehavior::Keep, "me@example.com");
        // Unsigned own commit — should NOT sign.
        assert!(!settings.should_sign(&make_commit("me@example.com", false)));
        // Signed own commit — should sign (preserve).
        assert!(settings.should_sign(&make_commit("me@example.com", true)));
        // Signed other commit — should NOT sign.
        assert!(!settings.should_sign(&make_commit("other@example.com", true)));
    }

    #[test]
    fn force_behavior_always_signs() {
        let settings = sign_settings(SignBehavior::Force, "me@example.com");
        assert!(settings.should_sign(&make_commit("me@example.com", false)));
        assert!(settings.should_sign(&make_commit("other@example.com", false)));
        assert!(settings.should_sign(&make_commit("other@example.com", true)));
    }

    // -- suggestion tests ----------------------------------------------------

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
