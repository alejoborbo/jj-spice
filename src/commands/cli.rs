use clap::builder::styling::AnsiColor;
use clap::builder::Styles;
use clap::{Args, Parser, Subcommand};

/// Colour theme matching jj-cli's help output.
///
/// Copied from jj-cli (where the constant is crate-private) so that
/// `jj-spice --help` looks visually consistent with `jj --help`.
const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Yellow.on_default().bold())
    .usage(AnsiColor::Yellow.on_default().bold())
    .literal(AnsiColor::Green.on_default().bold())
    .placeholder(AnsiColor::Green.on_default());

/// jj-spice: forge integration for jj.
#[derive(Parser, Clone, Debug)]
#[command(name = "jj-spice", styles = STYLES)]
pub struct Cli {
    #[command(subcommand)]
    pub command: SpiceCommand,
}

/// Top-level subcommands exposed by jj-spice.
#[derive(Subcommand, Clone, Debug)]
pub enum SpiceCommand {
    /// Manage the bookmark stack.
    Stack(StackArgs),
}

/// Arguments for the `stack` subcommand group.
#[derive(Args, Clone, Debug)]
pub struct StackArgs {
    /// The stack operation to perform.
    #[command(subcommand)]
    pub command: StackCommand,
}

/// Operations available under `jj-spice stack`.
#[derive(Subcommand, Clone, Debug)]
pub enum StackCommand {
    /// Submit the current stack of bookmarks for review.
    Submit,
    /// Discover and track existing change requests for bookmarks in the stack.
    Sync(SyncArgs),
}

/// Arguments for `jj-spice stack sync`.
#[derive(Args, Clone, Debug)]
pub struct SyncArgs {
    /// Re-discover change requests even for bookmarks that are already tracked.
    #[arg(long)]
    pub force: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_stack_submit() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "submit"]).unwrap();
        assert!(matches!(
            cli.command,
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Submit
            })
        ));
    }

    #[test]
    fn parse_stack_sync_without_force() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "sync"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Sync(args),
            }) => assert!(!args.force),
            _ => panic!("expected Sync"),
        }
    }

    #[test]
    fn parse_stack_sync_with_force() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "sync", "--force"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Sync(args),
            }) => assert!(args.force),
            _ => panic!("expected Sync"),
        }
    }

    #[test]
    fn parse_no_args_fails() {
        assert!(Cli::try_parse_from(["jj-spice"]).is_err());
    }

    #[test]
    fn parse_unknown_subcommand_fails() {
        assert!(Cli::try_parse_from(["jj-spice", "stack", "unknown"]).is_err());
    }
}
