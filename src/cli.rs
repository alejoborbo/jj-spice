use clap::Subcommand;

/// Custom subcommands provided by jj-spice.
///
/// Registered via [`CliRunner::add_subcommand`] so that the full jj CLI
/// (config, revset aliases, workspace loading) is available.
#[derive(Subcommand, Clone, Debug)]
pub enum SpiceCommand {
    /// Submit the current stack of bookmarks for review.
    Submit,
}
