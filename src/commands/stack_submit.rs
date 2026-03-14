use std::io::Write as _;

use jj_cli::description_util::TextEditor;
use jj_lib::backend::CommitId;

use crate::bookmark::graph::BookmarkGraph;
use crate::commands::env::SpiceEnv;
use crate::forge::{CreateParams, Forge};
use crate::protos::change_request::{ChangeRequests, ForgeMeta};
use crate::store::SpiceStore;
use crate::store::change_request::ChangeRequestStore;

/// Create change requests for each bookmark in the current stack (trunk..@).
pub async fn run(
    env: &SpiceEnv,
    forge: &dyn Forge,
    store: &SpiceStore,
    trunk: &CommitId,
    head: &CommitId,
    trunk_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cr_store = ChangeRequestStore::new(store);
    let graph = BookmarkGraph::new(env.repo.as_ref(), trunk, head)?;
    let iter_graph = graph.iter_graph()?;
    let text_editor = TextEditor::from_settings(&env.settings)?;
    let mut state = cr_store.load()?;

    for bookmark_node in iter_graph {
        let bookmark = bookmark_node.bookmark();
        let ascendants = bookmark_node.ascendants();

        // If the change request already exists, retarget if needed.
        let existing =
            get_existing_change_request(&env.ui, &state, forge, bookmark.name()).await?;

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
        };

        let cr = forge.create(params).await?;
        state.set(bookmark.name().to_string(), cr.to_forge_meta());

        writeln!(
            env.ui.stdout_formatter(),
            "Created change request: {}",
            cr.url()
        )?;
    }

    // Save the CRs to the store.
    cr_store.save(&state)?;

    Ok(())
}

/// Look up an existing change request for a bookmark.
///
/// 1. Check local state first — if already tracked, return it.
/// 2. Query the forge for CRs matching source/target branches.
/// 3. If multiple CRs are found, prompt the user to pick one.
async fn get_existing_change_request(
    ui: &jj_cli::ui::Ui,
    state: &ChangeRequests,
    forge: &dyn Forge,
    bookmark: &str,
) -> Result<Option<ForgeMeta>, Box<dyn std::error::Error>> {
    // Check local state first.
    if let Some(meta) = state.get(bookmark) {
        return Ok(Some(meta.clone()));
    }

    // Query the forge.
    let metas = forge.find_change_requests(bookmark).await?;

    match metas.len() {
        0 => Ok(None),
        1 => Ok(Some(metas.into_iter().next().unwrap())),
        _ => {
            writeln!(
                ui.warning_default(),
                "{}: found {} change requests on the forge",
                bookmark,
                metas.len()
            )?;
            for (i, meta) in metas.iter().enumerate() {
                writeln!(ui.stdout_formatter(), "  {i}: {meta}")?;
            }

            let choices: Vec<String> = (0..metas.len()).map(|i| i.to_string()).collect();
            let index = ui.prompt_choice("Select change request", &choices, Some(0))?;

            Ok(Some(metas.into_iter().nth(index).unwrap()))
        }
    }
}
