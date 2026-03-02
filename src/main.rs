mod bookmark;
mod bookmark_graph;
mod cli;

use clap::Parser;
use jj_cli::{cli_util::find_workspace_dir, config};
use jj_lib::{
    config::{ConfigGetError, StackedConfig},
    repo::StoreFactories,
    settings::UserSettings,
    workspace::{
        DefaultWorkspaceLoaderFactory, WorkspaceLoaderFactory, default_working_copy_factories,
    },
};
use std::env;

use cli::Cli;

fn main() {
    let config = setup_config().expect("Failed to load config");
    let cli = Cli::parse();

    match cli.command {
        cli::Commands::Submit => println!("Submit"),
    };
}

fn setup_config() -> Result<StackedConfig, ConfigGetError> {
    let mut config_layers = config::default_config_layers();
    let raw_config = config::config_from_environment(config_layers.drain(..));
    let config_env = config::ConfigEnv::from_environment();
    config_env.resolve_config(&raw_config)
}
