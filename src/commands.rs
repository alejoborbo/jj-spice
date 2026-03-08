/// CLI definitions (clap structs and enums).
pub mod cli;
/// Shared environment bootstrapped from the jj config and workspace.
pub(crate) mod env;
/// `stack submit` command implementation.
pub mod stack_submit;
/// `stack sync` command implementation.
pub mod stack_sync;

use jj_cli::cli_util::RevisionArg;

use cli::{Cli, SpiceCommand, StackCommand};
use env::SpiceEnv;

/// Resolve trunk and head revisions, then dispatch to the appropriate command.
pub(crate) fn run(cli: Cli, env: &SpiceEnv) -> Result<(), Box<dyn std::error::Error>> {
    let trunk_rev = RevisionArg::from("trunk()".to_string());
    let trunk = env.resolve_single_rev(&trunk_rev).map_err(|_| {
        "could not resolve trunk()\n\n\
         Set the trunk bookmark in your jj config:\n  \
         [revset-aliases]\n  \
         'trunk()' = 'main@origin'"
    })?;

    let head = env.resolve_single_rev(&RevisionArg::AT).map_err(|e| {
        format!("failed to resolve @: {e}")
    })?;

    let rt = tokio::runtime::Runtime::new()?;

    match cli.command {
        SpiceCommand::Stack(stack_args) => match stack_args.command {
            StackCommand::Submit => stack_submit::run(env, &trunk, &head),
            StackCommand::Sync(sync_args) => {
                rt.block_on(stack_sync::run(env, &trunk, &head, sync_args.force))
            }
        },
    }
}
