use jj_lib::backend::CommitId;

use crate::bookmark_graph;
use crate::commands::env::SpiceEnv;

/// Print the bookmark names in the current stack (trunk..@) in topological order.
pub fn run(
    env: &SpiceEnv,
    trunk: &CommitId,
    head: &CommitId,
) -> Result<(), Box<dyn std::error::Error>> {
    let graph = bookmark_graph::BookmarkGraph::new(env.repo.as_ref(), trunk, head)?;

    graph.iter_graph()?.for_each(|b| {
        println!("{}", b.name());
    });

    Ok(())
}
