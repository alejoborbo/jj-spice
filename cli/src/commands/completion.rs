use std::io::Write as _;

use clap::CommandFactory;

use super::cli::{Cli, ShellCompletion};

/// Generate a shell completion script and write it to stdout.
pub(crate) fn run(shell: ShellCompletion) -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Cli::command();
    let buf = shell.generate(&mut cmd);
    std::io::stdout().write_all(&buf)?;
    Ok(())
}
