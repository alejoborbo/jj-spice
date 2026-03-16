mod commands;

use clap::{CommandFactory, Parser};

use commands::cli::Cli;

fn main() {
    if let Err(e) = try_main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<(), Box<dyn std::error::Error>> {
    // When the COMPLETE env var is set, act as a dynamic shell completion
    // engine and exit. Otherwise this is a no-op and execution continues.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();
    commands::run(cli)
}
