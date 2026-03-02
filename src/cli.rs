use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
pub struct Cli {
    #[arg(short, long)]
    verbose: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Submit,
}
