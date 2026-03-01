mod bookmark;
mod bookmark_graph;
mod cli;
mod forge;
mod protos;
mod store;

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
    let settings = UserSettings::from_config(config).expect("Failed to load user settings");
    let cli = Cli::parse();

    let cwd = env::current_dir().and_then(dunce::canonicalize).unwrap();
    let workspace_loader = DefaultWorkspaceLoaderFactory
        .create(find_workspace_dir(&cwd))
        .expect("Failed to find workspace");

    let workspace = workspace_loader
        .load(
            &settings,
            &StoreFactories::default(),
            &default_working_copy_factories(),
        )
        .expect("Failed to load workspace");

    let repo_loader = workspace.repo_loader();
    let repo = repo_loader.load_at_head().expect("Failed to load repo");

    match cli.command {
        cli::Commands::Submit => {
            let graph = bookmark_graph::BookmarkGraph::new(repo.as_ref(), &workspace, "main")
                .expect("Failed to build bookmark graph");
            graph.iter_graph().unwrap().for_each(|b| {
                println!("{}", b.name());
            });
        }
    };
}

fn setup_config() -> Result<StackedConfig, ConfigGetError> {
    let mut config_layers = config::default_config_layers();
    let raw_config = config::config_from_environment(config_layers.drain(..));
    let config_env = config::ConfigEnv::from_environment();
    config_env.resolve_config(&raw_config)
}
