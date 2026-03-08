use jj_lib::{
    config::{ConfigNamePathBuf, StackedConfig},
    repo::ReadonlyRepo,
    workspace::Workspace,
};

use crate::bookmark_graph::BookmarkGraph;

pub fn run(repo: &ReadonlyRepo, workspace: &Workspace, config: &StackedConfig) {
    let trunk_symbol = resolve_trunk_symbol(config).expect("trunk() alias not found in config");
    let graph = BookmarkGraph::new(repo, workspace, &trunk_symbol)
        .expect("Failed to build bookmark graph");
    graph.iter_graph().unwrap().for_each(|node| {
        println!("{} (ascendants: {:?})", node.name(), node.ascendants());
    });
}

fn resolve_trunk_symbol(config: &StackedConfig) -> Option<String> {
    let name = ConfigNamePathBuf::from_iter(["revset-aliases", "trunk()"]);
    let raw: String = config.get(name).ok()?;
    let stripped = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(&raw);
    Some(stripped.to_string())
}
