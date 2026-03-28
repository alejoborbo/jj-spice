/// CLI definitions (clap structs and enums).
pub mod cli;
/// Shell completion script generation.
mod completion;
/// Shared environment bootstrapped from the jj config and workspace.
pub(crate) mod env;
/// `util install-aliases` command implementation.
mod install_aliases;
/// `stack log` command implementation.
mod stack_log;
/// `stack submit` command implementation.
pub mod stack_submit;
/// `stack sync` command implementation.
pub mod stack_sync;

use cli::{Cli, SpiceCommand, StackCommand, UtilCommand};
use env::SpiceEnv;
use jj_cli::cli_util::RevisionArg;
use jj_lib::repo::Repo as _;
use jj_spice_lib::forge::detect::detect_forges;

/// Dispatch to the appropriate command.
///
/// Commands that don't need a workspace (e.g. `util completion`) are handled
/// directly. Commands that do need one lazily initialise [`SpiceEnv`].
pub(crate) fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let Cli {
        global_args,
        command,
    } = cli;

    match command {
        SpiceCommand::Util(util_args) => match util_args.command {
            UtilCommand::Completion(args) => completion::run(args.shell),
            UtilCommand::InstallAliases(args) => install_aliases::run(args.print),
        },

        SpiceCommand::Stack(stack_args) => {
            let mut env = SpiceEnv::init(&global_args)?;

            let trunk_rev = RevisionArg::from("trunk()".to_string());
            let trunk = env.resolve_single_rev(&trunk_rev).map_err(|_| {
                "could not resolve trunk()\n\n\
                 Set the trunk bookmark in your jj config:\n  \
                 [revset-aliases]\n  \
                 'trunk()' = 'main@origin'"
            })?;

            let trunk_name = env
                .repo
                .view()
                .bookmarks()
                .find(|(_, target)| target.local_target.as_normal() == Some(&trunk))
                .map(|(name, _)| name.as_str().to_string())
                .ok_or("no bookmark found at trunk commit")?;

            let rt = tokio::runtime::Runtime::new()?;

            match stack_args.command {
                StackCommand::Log(log_args) => {
                    env.ui.request_pager();
                    let result =
                        rt.block_on(stack_log::run(&env, &trunk, log_args.revisions.as_deref()));
                    env.ui.finalize_pager();
                    result
                }
                StackCommand::Submit(submit_args) => {
                    let head = env
                        .resolve_single_rev(&RevisionArg::AT)
                        .map_err(|e| format!("failed to resolve @: {e}"))?;
                    rt.block_on(async {
                        let detection = detect_forges(env.repo.store(), env.config())?;

                        let (forge, source_repo) = env.resolve_forge(detection.forges)?;

                        stack_submit::run(
                            &submit_args,
                            &env,
                            forge.as_ref(),
                            source_repo.as_deref(),
                            &trunk,
                            &head,
                            &trunk_name,
                        )
                        .await
                    })
                }
                StackCommand::Sync(sync_args) => {
                    let head = env
                        .resolve_single_rev(&RevisionArg::AT)
                        .map_err(|e| format!("failed to resolve @: {e}"))?;
                    rt.block_on(stack_sync::run(
                        &sync_args,
                        &env,
                        &trunk,
                        &head,
                        &trunk_name,
                    ))
                }
            }
        }
    }
}
