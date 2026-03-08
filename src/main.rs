mod bookmark;
mod bookmark_graph;
mod cli;
mod forge;
mod protos;
mod store;

use jj_cli::cli_util::{CliRunner, CommandHelper, RevisionArg};
use jj_cli::command_error::{CommandError, CommandErrorKind};
use jj_cli::ui::Ui;

use cli::SpiceCommand;

fn run(ui: &mut Ui, command: &CommandHelper, args: SpiceCommand) -> Result<(), CommandError> {
    match args {
        SpiceCommand::Submit => {
            let workspace_command = command.workspace_helper(ui)?;

            // Resolve trunk() and @ through the full alias system.
            let trunk_revset = RevisionArg::from("trunk()".to_string());
            let trunk_commit = workspace_command.resolve_single_rev(ui, &trunk_revset)?;

            let wc_revset = RevisionArg::from("@".to_string());
            let wc_commit = workspace_command.resolve_single_rev(ui, &wc_revset)?;

            let repo = workspace_command.repo().clone();

            let graph = bookmark_graph::BookmarkGraph::new(
                repo.as_ref(),
                trunk_commit.id(),
                wc_commit.id(),
            )
            .map_err(|e| CommandError::new(CommandErrorKind::User, e))?;

            graph
                .iter_graph()
                .map_err(|e| CommandError::new(CommandErrorKind::User, e))?
                .for_each(|b| {
                    println!("{}", b.name());
                });

            Ok(())
        }
    }
}

fn main() -> std::process::ExitCode {
    CliRunner::init().add_subcommand(run).run().into()
}
