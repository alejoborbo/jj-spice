mod bookmark;
mod bookmark_graph;
mod cli;
mod forge;
mod protos;
mod stack_sync;
mod store;

use jj_cli::cli_util::{CliRunner, CommandHelper, RevisionArg};
use jj_cli::command_error::{CommandError, CommandErrorKind};
use jj_cli::ui::Ui;

use cli::SpiceCommand;

fn run(ui: &mut Ui, command: &CommandHelper, args: SpiceCommand) -> Result<(), CommandError> {
    match args {
        SpiceCommand::Submit => cmd_submit(ui, command),
        SpiceCommand::Stack(stack_args) => match stack_args.command {
            cli::StackCommand::Sync(sync_args) => cmd_stack_sync(ui, command, sync_args.force),
        },
    }
}

fn cmd_submit(ui: &mut Ui, command: &CommandHelper) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui)?;

    let trunk_revset = RevisionArg::from("trunk()".to_string());
    let trunk_commit = workspace_command.resolve_single_rev(ui, &trunk_revset)?;

    let wc_revset = RevisionArg::from("@".to_string());
    let wc_commit = workspace_command.resolve_single_rev(ui, &wc_revset)?;

    let repo = workspace_command.repo().clone();

    let graph =
        bookmark_graph::BookmarkGraph::new(repo.as_ref(), trunk_commit.id(), wc_commit.id())
            .map_err(|e| CommandError::new(CommandErrorKind::User, e))?;

    graph
        .iter_graph()
        .map_err(|e| CommandError::new(CommandErrorKind::User, e))?
        .for_each(|b| {
            println!("{}", b.name());
        });

    Ok(())
}

fn cmd_stack_sync(ui: &mut Ui, command: &CommandHelper, force: bool) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui)?;

    let trunk_revset = RevisionArg::from("trunk()".to_string());
    let trunk_commit = workspace_command.resolve_single_rev(ui, &trunk_revset)?;

    let wc_revset = RevisionArg::from("@".to_string());
    let wc_commit = workspace_command.resolve_single_rev(ui, &wc_revset)?;

    let repo = workspace_command.repo().clone();
    let repo_path = workspace_command.workspace().repo_path().to_owned();
    let config = command.settings().config().clone();

    let graph =
        bookmark_graph::BookmarkGraph::new(repo.as_ref(), trunk_commit.id(), wc_commit.id())
            .map_err(|e| CommandError::new(CommandErrorKind::User, e))?;

    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| CommandError::new(CommandErrorKind::Internal, e))?;

    rt.block_on(stack_sync::run(
        ui, &repo, &repo_path, &graph, &config, force,
    ))
    .map_err(|e| CommandError::new(CommandErrorKind::User, e))
}

fn main() -> std::process::ExitCode {
    CliRunner::init().add_subcommand(run).run().into()
}
