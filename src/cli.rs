use clap::{Args, Subcommand};

/// Custom subcommands provided by jj-spice.
///
/// Registered via [`CliRunner::add_subcommand`] so that the full jj CLI
/// (config, revset aliases, workspace loading) is available.
#[derive(Subcommand, Clone, Debug)]
pub enum SpiceCommand {
    /// Manage the bookmark stack.
    Stack(StackArgs),
}

#[derive(Args, Clone, Debug)]
pub struct StackArgs {
    #[command(subcommand)]
    pub command: StackCommand,
}

#[derive(Subcommand, Clone, Debug)]
pub enum StackCommand {
    /// Submit the current stack of bookmarks for review.
    Submit,
    /// Discover and track existing change requests for bookmarks in the stack.
    Sync(SyncArgs),
}

#[derive(Args, Clone, Debug)]
pub struct SyncArgs {
    /// Re-discover change requests even for bookmarks that are already tracked.
    #[arg(long)]
    pub force: bool,
}
