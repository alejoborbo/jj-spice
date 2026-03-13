use std::io::Write as _;

use jj_cli::description_util::TextEditor;
use jj_lib::backend::CommitId;

use crate::bookmark::graph::BookmarkGraph;
use crate::commands::env::SpiceEnv;
use crate::forge::{CreateParams, Forge};
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

        // Skip bookmarks that already have a tracked CR (unless --force).
        if state.get(bookmark.name()).is_some() {
            writeln!(
                env.ui.warning_default(),
                "{}: already tracked, skipping",
                bookmark.name()
            )?;
            continue;
        }

        writeln!(
            env.ui.stdout_formatter(),
            "Creating change request for: {}",
            bookmark.name()
        )?;

        let base_bookmark = match ascendants.len() {
            0 => trunk_name,
            1 => ascendants.first().unwrap().as_str(),
            _ => {
                let choices: Vec<String> = (1..=ascendants.len()).map(|i| i.to_string()).collect();
                let index = env
                    .ui
                    .prompt_choice("Select base bookmark", &choices, None)?;
                ascendants[index].as_str()
            }
        };

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
