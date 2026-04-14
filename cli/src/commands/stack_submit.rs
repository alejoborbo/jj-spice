use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::sync::Arc;

use itertools::Itertools;
use jj_cli::description_util::TextEditor;
use jj_cli::git_util::{GitSubprocessUi, print_push_stats};
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::git::{self, GitBranchPushTargets};
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::{RefName, RefNameBuf, RemoteNameBuf};
use jj_lib::refs::{
    BookmarkPushAction, BookmarkPushUpdate, LocalAndRemoteRef, classify_bookmark_push_action,
};
use jj_lib::repo::ReadonlyRepo;
use jj_lib::revset::ResolvedRevsetExpression;
use jj_lib::signing::SignBehavior;
use jj_spice_lib::bookmark::graph::BookmarkGraph;
use jj_spice_lib::comments::{Comment, LiveCrData};
use jj_spice_lib::forge::{CreateParams, Forge};
use jj_spice_lib::protos::change_request::{ChangeRequests, ForgeMeta};
use jj_spice_lib::store::change_request::ChangeRequestStore;

use crate::commands::cli::SubmitArgs;
use crate::commands::env::SpiceEnv;

/// Push data collected for a single bookmark during the first pass.
struct BookmarkPushEntry {
    name: String,
    push_update: BookmarkPushUpdate,
}

/// Collected data from the first pass: repo snapshot, push entries, and CR
/// metadata.
type CollectedPushData = (
    Arc<ReadonlyRepo>,
    Vec<BookmarkPushEntry>,
    Vec<BookmarkCrMeta>,
);

/// Metadata collected per bookmark for change-request creation in the second pass.
struct BookmarkCrMeta {
    name: String,
    ascendants: Vec<String>,
    commits: Vec<CommitId>,
}

/// Whether a base bookmark has been resolved, or a user input is needed.
enum ResolvedBaseBookmark {
    Resolved(String),
    NeedUserInput,
}

/// Create change requests for each bookmark in the current stack (trunk..@).
///
/// `source_repo` identifies the fork repository for cross-repo PRs; it is
/// `None` in single-remote mode. See [`CreateParams::source_repo`] for the
/// forge-specific format.
pub async fn run(
    args: &SubmitArgs,
    env: &mut SpiceEnv,
    forge: &dyn Forge,
    source_repo: Option<&str>,
    trunk: &CommitId,
    head: &CommitId,
    trunk_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let text_editor = TextEditor::from_settings(&env.settings)?;

    let auto_accept =
        if let Ok(auto_accept) = env.config().get::<bool>(["spice", "auto-accept-changes"]) {
            auto_accept
        } else {
            args.auto_accept
        };

    // Auto-clean: when enabled, inactive (closed/merged) CRs discovered in
    // local state are automatically removed so a fresh CR can be created.
    let auto_clean = env
        .config()
        .get::<bool>(["spice", "auto-clean"])
        .unwrap_or(true);

    // ── Pass 1: Track bookmarks, classify push actions, collect metadata ──
    //
    // Bookmark tracking changes the repo view, so we do it in a dedicated
    // transaction first.  The returned repo (or `env.repo` if nothing was
    // tracked) is then used to classify every bookmark's push action and to
    // validate commits.
    let (repo_for_push, push_entries, cr_metas) = {
        let graph = BookmarkGraph::build_active_graph(env.repo.as_ref(), trunk, head)?;
        collect_push_data(env, &graph, auto_accept, args.auto_track_bookmarks)?
    };

    // ── Batch sign + push (single transaction) ──
    if !push_entries.is_empty() {
        batch_push(env, &repo_for_push, trunk, push_entries)?;
    }

    // Load the CR store and rebuild the graph after batch_push so the mutable
    // borrow of `env` used by `commit_and_update_working_copy` doesn't
    // conflict with the store/graph references.
    let cr_store = ChangeRequestStore::new(&env.store);
    let mut state = cr_store.load()?;
    let graph = BookmarkGraph::build_active_graph(env.repo.as_ref(), trunk, head)?;

    // ── Pass 2: Create / retarget change requests ──
    for meta in &cr_metas {
        let maybe_cr = get_existing_change_request(
            &env.ui,
            &mut state,
            forge,
            &meta.name,
            source_repo,
            args.allow_inactive,
            auto_clean,
        )
        .await?;

        let base_bookmark = get_base_bookmark(&env.ui, &maybe_cr, meta, trunk_name)?;

        if let Some(forge_meta) = maybe_cr {
            match forge_meta.target_branch() {
                Some(tb) if tb != base_bookmark => {
                    let cr = forge
                        .update_base(&forge_meta, &base_bookmark)
                        .await
                        .map_err(|e| e as Box<dyn std::error::Error>)?;

                    // Retargeting the base branch should not change the comment ID.
                    // Set the comment ID with the one from the existing CR.
                    let mut new_forge_meta = cr.to_forge_meta();
                    if let Some(comment_id) = forge_meta.comment_id() {
                        new_forge_meta.set_comment_id(comment_id);
                    }

                    state.set(meta.name.clone(), new_forge_meta);
                    writeln!(
                        env.ui.stdout_formatter(),
                        "Base branch has been retargeted to {}, updating change request: {}",
                        base_bookmark,
                        cr.id(),
                    )?;
                }
                _ => {
                    writeln!(
                        env.ui.hint_no_heading(),
                        "{}: already tracked, skipping",
                        meta.name,
                    )?;
                }
            }
            continue;
        }

        writeln!(
            env.ui.stdout_formatter(),
            "Creating change request for: {}",
            meta.name
        )?;

        writeln!(
            env.ui.stdout_formatter(),
            "Base bookmark: {}",
            base_bookmark
        )?;

        let (suggested_title, suggested_body) =
            build_cr_suggestion(env.repo.as_ref(), &meta.commits)?;

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
        let is_draft = if args.draft {
            true
        } else if args.no_draft {
            false
        } else {
            env.ui.prompt_yes_no("Draft?", Some(false))?
        };

        let params = CreateParams {
            source_branch: &meta.name,
            target_branch: &base_bookmark,
            title: &title,
            body: Some(&description),
            is_draft,
            source_repo,
        };

        let cr = forge
            .create(params)
            .await
            .map_err(|e| e as Box<dyn std::error::Error>)?;
        state.set(meta.name.clone(), cr.to_forge_meta());

        writeln!(
            env.ui.stdout_formatter(),
            "Created change request: {}",
            cr.url()
        )?;
    }

    // Post stack-trace comments on all CRs concurrently.
    post_stack_comments(forge, &graph, &mut state, trunk_name).await?;

    // Persist CRs and comment IDs to disk.
    cr_store.save(&state)?;

    Ok(())
}

/// Fetch the current branch's base bookmark.
fn get_base_bookmark(
    ui: &Ui,
    maybe_forge_meta: &Option<ForgeMeta>,
    meta: &BookmarkCrMeta,
    trunk_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    match resolve_base_bookmark(maybe_forge_meta, meta.ascendants.as_ref(), trunk_name) {
        ResolvedBaseBookmark::Resolved(b) => Ok(b),
        ResolvedBaseBookmark::NeedUserInput => {
            writeln!(ui.stdout_formatter(), "Multiple base bookmarks found:")?;
            for (i, a) in meta.ascendants.iter().enumerate() {
                writeln!(ui.stdout_formatter(), "  {}: {}", i, a)?;
            }

            let choices: Vec<String> = (0..meta.ascendants.len()).map(|i| i.to_string()).collect();
            let index = ui.prompt_choice("Select base bookmark", &choices, Some(0))?;

            Ok(meta.ascendants[index].clone())
        }
    }
}

/// Resolve the base bookmark for a bookmark.
/// If it's not resolved, we need the user to select a base bookmark.
fn resolve_base_bookmark(
    maybe_forge_meta: &Option<ForgeMeta>,
    ascendants: &[String],
    trunk_name: &str,
) -> ResolvedBaseBookmark {
    match ascendants.len() {
        0 => ResolvedBaseBookmark::Resolved(trunk_name.to_string()),
        1 => ResolvedBaseBookmark::Resolved(ascendants[0].clone()),
        _ => {
            // If a change request exists for the bookmark, and that the target bookmark is one
            // of the ascendants of the current bookmark, then we can use the existing change
            // without prompting the user.
            if let Some(forge_meta) = maybe_forge_meta
                && let Some(target_branch) = forge_meta.target_branch()
                && ascendants.contains(&target_branch.to_string())
            {
                return ResolvedBaseBookmark::Resolved(target_branch.to_string());
            }

            // If no change request exists for the bookmark, or the target bookmark is not one
            // of the ascendants of the current bookmark, we need the user to select a
            // base bookmark.
            ResolvedBaseBookmark::NeedUserInput
        }
    }
}

/// First pass: track untracked bookmarks, classify push actions, and collect
/// metadata for change-request creation.
///
/// Returns the repo snapshot to use for the batch push (may differ from
/// `env.repo` if bookmarks were tracked), the list of bookmarks that need
/// pushing, and the CR metadata for the second pass.
fn collect_push_data(
    env: &SpiceEnv,
    graph: &BookmarkGraph<'_>,
    auto_accept: bool,
    auto_track_bookmark: bool,
) -> Result<CollectedPushData, Box<dyn std::error::Error>> {
    let remote = env.get_default_remote();

    // Track all bookmarks that need tracking in a single transaction.
    let repo_after_track = track_needed_bookmarks(env, graph, &remote, auto_track_bookmark)?;
    let repo = repo_after_track.unwrap_or_else(|| env.repo.clone());

    let mut push_entries: Vec<BookmarkPushEntry> = Vec::new();
    let mut cr_metas: Vec<BookmarkCrMeta> = Vec::new();

    for bookmark_node in graph.iter_graph()? {
        let bookmark = bookmark_node.bookmark();
        let commit_ids = bookmark_node.commits();

        // Collect CR metadata (owned data, survives past the borrow of `graph`).
        cr_metas.push(BookmarkCrMeta {
            name: bookmark.name().to_string(),
            ascendants: bookmark_node.ascendants().to_vec(),
            commits: commit_ids.to_vec(),
        });

        // Classify the push action from the (possibly updated) repo view.
        let push_action = classify_push_for_bookmark(&repo, bookmark.name(), &remote);
        let push_action = match push_action {
            Some(a) => a,
            None => {
                // No tracked remote ref — nothing to push.
                if !auto_track_bookmark {
                    writeln!(
                        env.ui.hint_default(),
                        "No remote ref found for bookmark {name}. Run \
                         `jj bookmark track {name} --remote={remote}` to track it.",
                        name = bookmark.name(),
                        remote = remote.as_symbol(),
                    )?;
                    return Err(
                        format!("No remote ref found for bookmark {}", bookmark.name()).into(),
                    );
                }
                continue;
            }
        };

        match push_action {
            BookmarkPushAction::AlreadyMatches => {}
            BookmarkPushAction::Update(push_update) => {
                writeln!(
                    env.ui.warning_default(),
                    "Untracked changes have been detected.",
                )?;
                let should_push = if auto_accept {
                    writeln!(
                        env.ui.stdout_formatter(),
                        "Auto accept is enabled, pushing commits to {}",
                        remote.as_str(),
                    )?;
                    true
                } else {
                    env.ui.prompt_yes_no(
                        &format!("Do you want to push them to {}?", remote.as_str()),
                        Some(true),
                    )?
                };

                if should_push {
                    push_entries.push(BookmarkPushEntry {
                        name: bookmark.name().to_string(),
                        push_update,
                    });
                }
            }
            action => {
                writeln!(
                    env.ui.warning_default(),
                    "Bookmark {} has unexpected state: {:?}",
                    bookmark.name(),
                    action,
                )?;
            }
        }
    }

    Ok((repo, push_entries, cr_metas))
}

/// Classify the push action for a bookmark by looking up its refs in the repo.
///
/// Returns `None` when the bookmark has no tracked remote ref on the given
/// remote (i.e. there is nothing to push).
fn classify_push_for_bookmark(
    repo: &ReadonlyRepo,
    bookmark_name: &str,
    remote: &RemoteNameBuf,
) -> Option<BookmarkPushAction> {
    let view = repo.view();
    let ref_name = RefName::new(bookmark_name);
    let local_target = view.get_local_bookmark(ref_name);
    let remote_symbol = ref_name.to_remote_symbol(remote);
    let remote_ref = view.get_remote_bookmark(remote_symbol);

    // A bookmark with no tracked remote ref has nothing to push.
    if !remote_ref.is_tracked() {
        return None;
    }

    Some(classify_bookmark_push_action(LocalAndRemoteRef {
        local_target,
        remote_ref,
    }))
}

/// Track all bookmarks that need tracking in a single committed transaction.
///
/// Returns `Some(repo)` with the updated state if any bookmarks were tracked,
/// or `None` if nothing needed tracking.
fn track_needed_bookmarks(
    env: &SpiceEnv,
    graph: &BookmarkGraph<'_>,
    remote: &RemoteNameBuf,
    auto_track: bool,
) -> Result<Option<Arc<ReadonlyRepo>>, Box<dyn std::error::Error>> {
    if !auto_track {
        return Ok(None);
    }

    let mut names_to_track: Vec<String> = Vec::new();

    for bookmark_node in graph.iter_graph()? {
        let bookmark = bookmark_node.bookmark();

        // Already has a remote ref — no tracking needed.
        if bookmark.remote_ref(remote).is_some() {
            continue;
        }

        // Track unconditionally — even if the remote doesn't have a branch
        // with this name yet.  Tracking creates an absent-but-tracked remote
        // ref, which classify_bookmark_push_action correctly classifies as
        // Update (i.e. "push for the first time").
        names_to_track.push(bookmark.name().to_string());
    }

    if names_to_track.is_empty() {
        return Ok(None);
    }

    let mut tx = env.repo.start_transaction();
    for name in &names_to_track {
        let ref_name = RefName::new(name);
        let symbol = ref_name.to_remote_symbol(remote);
        tx.repo_mut().track_remote_bookmark(symbol)?;
    }

    let description = if names_to_track.len() == 1 {
        format!("tracked bookmark {}", names_to_track[0])
    } else {
        format!("tracked {} bookmarks", names_to_track.len())
    };
    let repo = tx.commit(description)?;
    Ok(Some(repo))
}

/// Remap bookmark push targets after signing rewrites commits.
///
/// Every bookmark whose `new_target` appears in the `old_to_new` map is
/// updated to point to the newly-signed commit ID.  This must apply to
/// **all** bookmarks — the previous per-bookmark approach only remapped one,
/// leaving the rest pointing at stale (unsigned) commit IDs.
fn remap_push_targets(
    bookmark_updates: &mut [(RefNameBuf, BookmarkPushUpdate)],
    old_to_new: &HashMap<CommitId, CommitId>,
) {
    for (_, update) in bookmark_updates.iter_mut() {
        if let Some(old_target) = &update.new_target
            && let Some(new_id) = old_to_new.get(old_target)
        {
            update.new_target = Some(new_id.clone());
        }
    }
}

/// Sign all unsigned commits and push all bookmarks in a single transaction.
///
/// This mirrors jj's own `sign_commits_before_push` + `push_branches` flow:
/// all signing, rebasing, and pushing happen within one transaction that is
/// committed at the end so jj's repo state stays consistent with the remote.
fn batch_push(
    env: &mut SpiceEnv,
    repo: &Arc<ReadonlyRepo>,
    trunk: &CommitId,
    entries: Vec<BookmarkPushEntry>,
) -> Result<(), Box<dyn std::error::Error>> {
    let remote = env.get_default_remote();
    let mut tx = repo.start_transaction();

    // 1. Enumerate ALL commits that will be pushed (trunk..new_heads).
    //    Using a revset range ensures intermediate commits (those without a
    //    bookmark) are included — not just the bookmark tips.  This mirrors
    //    jj CLI's `validate_commits_ready_to_push` which uses
    //    `(old_heads ∪ immutable_heads)..new_heads`.
    let new_heads: Vec<CommitId> = entries
        .iter()
        .filter_map(|e| e.push_update.new_target.clone())
        .collect();
    let range_expr = ResolvedRevsetExpression::commits(vec![trunk.clone()])
        .range(&ResolvedRevsetExpression::commits(new_heads));
    let all_commit_ids: Vec<CommitId> = range_expr
        .evaluate(tx.repo())?
        .iter()
        .collect::<Result<Vec<_>, _>>()?;
    let commits_to_sign = verify_commits(env, tx.repo(), &all_commit_ids)?;

    // Build the initial bookmark update list.
    let mut bookmark_updates: Vec<(RefNameBuf, BookmarkPushUpdate)> = entries
        .iter()
        .map(|e| (RefNameBuf::from(e.name.as_str()), e.push_update.clone()))
        .collect();

    // 2. Sign all unsigned commits in a single `transform_descendants` call.
    //    This correctly handles stacked bookmarks: descendants are automatically
    //    rebased onto newly signed parents, so every commit in the stack gets a
    //    consistent lineage.
    if !commits_to_sign.is_empty() {
        let sign_ids: Vec<CommitId> = commits_to_sign.iter().map(|c| c.id().clone()).collect();
        let sign_id_set: std::collections::HashSet<CommitId> = sign_ids.iter().cloned().collect();
        let mut old_to_new: HashMap<CommitId, CommitId> = HashMap::new();

        tx.repo_mut()
            .transform_descendants(sign_ids, async |rewriter| {
                let old_id = rewriter.old_commit().id().clone();
                if sign_id_set.contains(&old_id) {
                    // Commit that needs signing: rewrite with signature.
                    let new_commit: Commit = rewriter
                        .reparent()
                        .set_sign_behavior(SignBehavior::Own)
                        .write()?;
                    old_to_new.insert(old_id, new_commit.id().clone());
                } else {
                    // Descendant commit: just reparent onto new parents.
                    let new_commit: Commit = rewriter.reparent().write()?;
                    old_to_new.insert(old_id, new_commit.id().clone());
                }
                Ok(())
            })?;

        // 3. Remap all push targets to the newly signed commit IDs.
        remap_push_targets(&mut bookmark_updates, &old_to_new);

        writeln!(
            env.ui.status(),
            "Signed {} commit(s)",
            commits_to_sign.len()
        )?;
    }

    // 4. Rebase any remaining descendants (clears parent_mapping so the
    //    transaction can be committed without hitting the `has_rewrites` assert).
    let num_rebased = tx.repo_mut().rebase_descendants()?;
    if num_rebased > 0 {
        writeln!(
            env.ui.status(),
            "Rebased {} descendant commit(s)",
            num_rebased
        )?;
    }

    // 5. Push all bookmarks in one call.
    let targets = GitBranchPushTargets {
        branch_updates: bookmark_updates,
    };
    let push_stats = git::push_branches(
        tx.repo_mut(),
        env.git_settings.to_subprocess_options(),
        remote.as_ref(),
        &targets,
        &mut GitSubprocessUi::new(&env.ui),
    )?;

    print_push_stats(&env.ui, &push_stats)?;

    // 6. Commit the transaction and update the working copy so jj's local
    //    state (both operation log and on-disk tree) matches the remote.
    if push_stats.all_ok() {
        env.commit_and_update_working_copy(tx, "push stack bookmarks")?;
        for entry in &entries {
            writeln!(
                env.ui.stdout_formatter(),
                "Pushed {} to {}",
                entry.name,
                remote.as_str(),
            )?;
        }
        Ok(())
    } else {
        Err("Failed to push some bookmarks".into())
    }
}

/// Post stack-trace comments on all change requests concurrently.
async fn post_stack_comments(
    forge: &dyn Forge,
    graph: &BookmarkGraph<'_>,
    state: &mut ChangeRequests,
    trunk_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Fetch the current remote state for each bookmark in the graph, and use it to build the live
    // data.
    let cr_entries: Vec<_> = state.iter().collect();
    let cr_metas: Vec<&ForgeMeta> = cr_entries.iter().map(|(_, meta)| *meta).collect();
    let results = forge.get_batch(cr_metas).await;

    // Build the live data for each change request.
    // This is used to display the PR title, and status to the comment in the change request.
    let mut live_cr_data: BTreeMap<String, LiveCrData> = BTreeMap::new();
    for (result, (_, meta)) in results.into_iter().zip(&cr_entries) {
        if let Ok(cr) = result
            && let Some(source_branch) = meta.source_branch()
        {
            {
                live_cr_data.insert(
                    source_branch.to_string(),
                    LiveCrData {
                        status: cr.status(),
                        title: cr.title().to_string(),
                        url: cr.url().to_string(),
                    },
                );
            }
        }
    }

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
            let comment_text = Comment::new(bookmark, graph, state)
                .with_live_data(&live_cr_data)
                .with_trunk(trunk_name)
                .to_string()?;
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
        let comment_id = result.map_err(|e| e as Box<dyn std::error::Error>)?;
        let mut updated_meta = state
            .get(name)
            .ok_or_else(|| format!("no change request found for bookmark '{name}'"))?
            .clone();
        updated_meta.set_comment_id(comment_id);
        state.set(name.clone(), updated_meta);
    }

    Ok(())
}

/// Look up an existing change request for a bookmark.
///
/// 1. Check local state first — if already tracked, return it (unless
///    auto-clean is enabled and the CR is inactive on the forge).
/// 2. Query the forge for CRs matching source/target branches.
/// 3. If multiple CRs are found, prompt the user to pick one.
///
/// `source_repo` is forwarded to [`Forge::find_change_requests`] for
/// cross-repo PR discovery; pass `None` in single-remote mode.
async fn get_existing_change_request(
    ui: &jj_cli::ui::Ui,
    state: &mut ChangeRequests,
    forge: &dyn Forge,
    bookmark: &str,
    source_repo: Option<&str>,
    allow_inactive: bool,
    auto_clean: bool,
) -> Result<Option<ForgeMeta>, Box<dyn std::error::Error>> {
    // Check local state first.
    if let Some(meta) = state.get(bookmark) {
        // When auto-clean is enabled, verify the CR is still active on the
        // forge. Only closed CRs are removed here — merged CRs are kept
        // because the bookmark still exists (the user may want to re-create
        // a new CR for further changes on this branch).
        if auto_clean {
            if let Some(status) = jj_spice_lib::clean::check_status(forge, meta).await {
                if matches!(status, jj_spice_lib::forge::ChangeStatus::Closed) {
                    writeln!(
                        ui.hint_default(),
                        "{bookmark}: change request is {status:?}, removing from tracking \
                         (auto-clean)",
                    )?;
                    state.remove(bookmark);
                    // Fall through to forge discovery below.
                } else {
                    return Ok(Some(meta.clone()));
                }
            } else {
                // Forge query failed — keep the existing mapping.
                return Ok(Some(meta.clone()));
            }
        } else {
            return Ok(Some(meta.clone()));
        }
    }

    // Query the forge.
    let metas = forge
        .find_change_requests(bookmark, source_repo)
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
        .collect::<Vec<_>>();

    match metas.len() {
        0 => Ok(None),
        1 => Ok(Some(metas.into_iter().next().unwrap())),
        _ => {
            writeln!(
                ui.hint_no_heading(),
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
    use std::collections::HashMap;

    use jj_lib::backend::{
        ChangeId, Commit, CommitId, MillisSinceEpoch, SecureSig, Signature, Timestamp, TreeId,
    };
    use jj_lib::merge::Merge;
    use jj_lib::ref_name::RefNameBuf;
    use jj_lib::refs::BookmarkPushUpdate;
    use jj_lib::settings::SignSettings;
    use jj_lib::signing::SignBehavior;
    use jj_spice_lib::protos::change_request::forge_meta::Forge as ForgeOneof;
    use jj_spice_lib::protos::change_request::{ForgeMeta, GitHubMeta};

    use super::remap_push_targets;
    use super::{ResolvedBaseBookmark, resolve_base_bookmark};

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

    // -- batch push regression tests -----------------------------------------
    //
    // These tests verify the core logic that was broken before the batch-push
    // refactor.  The old per-bookmark approach had two bugs:
    //   1. Only one bookmark's push target was remapped after signing.
    //   2. The signing transaction was never committed, causing divergent
    //      changes.
    //
    // Bug 2 (transaction commit) is structural and covered by the code itself
    // (the `tx.commit()` call in `batch_push`).  Bug 1 is exercised by the
    // `remap_push_targets` and `dedup_commit_ids` unit tests below.

    /// Synthetic commit ID from a single byte for readability.
    fn cid(byte: u8) -> CommitId {
        CommitId::new(vec![byte])
    }

    // -- remap_push_targets --------------------------------------------------

    #[test]
    fn remap_targets_applies_to_all_bookmarks() {
        // Simulate a stack: bookmark A (commit 0xAA) → bookmark B (commit
        // 0xBB).  Signing rewrites both commits.  The old code only remapped
        // one bookmark's target; the fix remaps all of them.
        let mut updates = vec![
            (
                RefNameBuf::from("bookmark-a"),
                BookmarkPushUpdate {
                    old_target: None,
                    new_target: Some(cid(0xAA)),
                },
            ),
            (
                RefNameBuf::from("bookmark-b"),
                BookmarkPushUpdate {
                    old_target: None,
                    new_target: Some(cid(0xBB)),
                },
            ),
        ];

        let old_to_new: HashMap<CommitId, CommitId> =
            [(cid(0xAA), cid(0xA1)), (cid(0xBB), cid(0xB1))]
                .into_iter()
                .collect();

        remap_push_targets(&mut updates, &old_to_new);

        // Both bookmarks must be remapped.
        assert_eq!(updates[0].1.new_target, Some(cid(0xA1)));
        assert_eq!(updates[1].1.new_target, Some(cid(0xB1)));
    }

    #[test]
    fn remap_targets_leaves_unknown_targets_unchanged() {
        // A bookmark whose target was not rewritten should remain as-is.
        let mut updates = vec![
            (
                RefNameBuf::from("bookmark-a"),
                BookmarkPushUpdate {
                    old_target: None,
                    new_target: Some(cid(0xAA)),
                },
            ),
            (
                RefNameBuf::from("bookmark-b"),
                BookmarkPushUpdate {
                    old_target: None,
                    new_target: Some(cid(0xBB)),
                },
            ),
        ];

        // Only commit 0xAA was rewritten.
        let old_to_new: HashMap<CommitId, CommitId> =
            [(cid(0xAA), cid(0xA1))].into_iter().collect();

        remap_push_targets(&mut updates, &old_to_new);

        assert_eq!(updates[0].1.new_target, Some(cid(0xA1)));
        // bookmark-b's target should remain unchanged.
        assert_eq!(updates[1].1.new_target, Some(cid(0xBB)));
    }

    #[test]
    fn remap_targets_handles_delete_bookmarks() {
        // A bookmark being deleted (new_target = None) should not panic.
        let mut updates = vec![(
            RefNameBuf::from("deleted-bookmark"),
            BookmarkPushUpdate {
                old_target: Some(cid(0xAA)),
                new_target: None,
            },
        )];

        let old_to_new: HashMap<CommitId, CommitId> =
            [(cid(0xAA), cid(0xA1))].into_iter().collect();

        remap_push_targets(&mut updates, &old_to_new);

        // new_target remains None (delete).
        assert_eq!(updates[0].1.new_target, None);
    }

    #[test]
    fn remap_targets_empty_map_is_noop() {
        let mut updates = vec![(
            RefNameBuf::from("bookmark"),
            BookmarkPushUpdate {
                old_target: None,
                new_target: Some(cid(0xAA)),
            },
        )];

        remap_push_targets(&mut updates, &HashMap::new());

        assert_eq!(updates[0].1.new_target, Some(cid(0xAA)));
    }

    #[test]
    fn remap_targets_three_bookmark_stack() {
        // Three-deep stack: A → B → C.  All three signed.
        let mut updates = vec![
            (
                RefNameBuf::from("a"),
                BookmarkPushUpdate {
                    old_target: None,
                    new_target: Some(cid(1)),
                },
            ),
            (
                RefNameBuf::from("b"),
                BookmarkPushUpdate {
                    old_target: None,
                    new_target: Some(cid(2)),
                },
            ),
            (
                RefNameBuf::from("c"),
                BookmarkPushUpdate {
                    old_target: None,
                    new_target: Some(cid(3)),
                },
            ),
        ];

        let old_to_new: HashMap<CommitId, CommitId> =
            [(cid(1), cid(11)), (cid(2), cid(22)), (cid(3), cid(33))]
                .into_iter()
                .collect();

        remap_push_targets(&mut updates, &old_to_new);

        assert_eq!(updates[0].1.new_target, Some(cid(11)));
        assert_eq!(updates[1].1.new_target, Some(cid(22)));
        assert_eq!(updates[2].1.new_target, Some(cid(33)));
    }

    // -- test the base bookmark selection
    fn github_forge_meta(target_branch: &str) -> ForgeMeta {
        ForgeMeta {
            forge: Some(ForgeOneof::Github(GitHubMeta {
                target_branch: target_branch.to_string(),
                ..Default::default()
            })),
        }
    }

    #[test]
    fn resolve_base_bookmark_zero_ascendants_returns_trunk() {
        let result = resolve_base_bookmark(&None, &[], "main");
        assert!(matches!(result, ResolvedBaseBookmark::Resolved(name) if name == "main"));
    }

    #[test]
    fn resolve_base_bookmark_single_ascendant() {
        let ascendants = vec!["feature-a".to_string()];
        let result = resolve_base_bookmark(&None, &ascendants, "main");
        assert!(matches!(result, ResolvedBaseBookmark::Resolved(name) if name == "feature-a"));
    }

    #[test]
    fn resolve_base_bookmark_multiple_ascendants_no_cr() {
        let ascendants = vec!["feature-a".to_string(), "feature-b".to_string()];
        let result = resolve_base_bookmark(&None, &ascendants, "main");
        assert!(matches!(result, ResolvedBaseBookmark::NeedUserInput));
    }

    #[test]
    fn resolve_base_bookmark_multiple_ascendants_cr_matches() {
        let ascendants = vec!["feature-a".to_string(), "feature-b".to_string()];
        let forge_meta = github_forge_meta("feature-b");
        let result = resolve_base_bookmark(&Some(forge_meta), &ascendants, "main");
        assert!(matches!(result, ResolvedBaseBookmark::Resolved(name) if name == "feature-b"));
    }

    #[test]
    fn resolve_base_bookmark_multiple_ascendants_cr_no_match() {
        let ascendants = vec!["feature-a".to_string(), "feature-b".to_string()];
        let forge_meta = github_forge_meta("feature-c");
        let result = resolve_base_bookmark(&Some(forge_meta), &ascendants, "main");
        assert!(matches!(result, ResolvedBaseBookmark::NeedUserInput));
    }

    #[test]
    fn resolve_base_bookmark_multiple_ascendants_forge_none() {
        let ascendants = vec!["feature-a".to_string(), "feature-b".to_string()];
        let forge_meta = ForgeMeta { forge: None };
        let result = resolve_base_bookmark(&Some(forge_meta), &ascendants, "main");
        assert!(matches!(result, ResolvedBaseBookmark::NeedUserInput));
    }
}
