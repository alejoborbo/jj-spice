use std::collections::{HashMap, HashSet};

use jj_lib::{
    backend::CommitId,
    dag_walk::topo_order_forward,
    git::REMOTE_NAME_FOR_LOCAL_GIT_REPO,
    graph::{GraphEdge, GraphNode, reverse_graph},
    repo::Repo,
    revset::{RevsetEvaluationError, RevsetExpression},
};
use thiserror::Error;

use crate::bookmark::{Bookmark, RemoteTracking};

/// A node in the bookmark DAG, wrapping a [`Bookmark`] with ancestry info.
#[derive(Clone, Debug)]
pub struct BookmarkNode {
    bookmark: Bookmark,
    ascendants: Vec<String>,
}

impl BookmarkNode {
    /// Wrap a bookmark as a graph node with no ascendants.
    pub fn new(bookmark: Bookmark) -> Self {
        Self {
            bookmark,
            ascendants: Vec::new(),
        }
    }

    /// The underlying bookmark.
    pub fn bookmark(&self) -> &Bookmark {
        &self.bookmark
    }

    /// Shorthand for `self.bookmark().name()`.
    pub fn name(&self) -> &str {
        self.bookmark.name()
    }

    fn ascendants(&self) -> &[String] {
        &self.ascendants
    }

    fn add_ascendant(&mut self, ascendant: String) {
        self.ascendants.push(ascendant);
    }
}

/// Errors that can occur when building or traversing the bookmark graph.
#[derive(Debug, Error)]
pub enum BookmarkGraphError {
    #[error("revset evaluation failed")]
    RevsetEvaluation(#[from] RevsetEvaluationError),
    #[error("no root commit found in branch")]
    NoRootCommit,
    #[error("cycle detected in bookmark graph")]
    Cycle,
}

/// DAG of bookmarks between trunk and head, used for stack operations.
#[derive(Debug)]
pub struct BookmarkGraph {
    nodes: HashMap<String, BookmarkNode>,
    edges: HashMap<String, Vec<GraphEdge<String>>>,
    head_bookmarks: HashSet<String>,
}

impl BookmarkGraph {
    /// Build a bookmark graph from commits between `trunk` and `head`.
    ///
    /// Both should be pre-resolved commit IDs. Typically `trunk` comes from
    /// evaluating the `trunk()` revset alias, and `head` is the working-copy
    /// commit (`@`).
    pub fn new(
        repo: &dyn Repo,
        trunk: &CommitId,
        head: &CommitId,
    ) -> Result<Self, BookmarkGraphError> {
        let bookmarks_per_commit = Self::build_bookmark_commit_map(repo);
        let reversed = Self::evaluate_branch_commits(repo, trunk, head)?;
        let (nodes, edges) = Self::build_bookmark_graph(&reversed, &bookmarks_per_commit);
        let head_bookmarks = Self::find_head_bookmarks(&edges);
        Ok(Self {
            nodes,
            edges,
            head_bookmarks,
        })
    }

    /// Iterate bookmarks in topological order (roots first).
    pub fn iter_graph(&self) -> Result<impl Iterator<Item = &BookmarkNode>, BookmarkGraphError> {
        let result = topo_order_forward(
            self.head_bookmarks.iter().map(|name| &self.nodes[name]),
            |node| node.name(),
            |&node| {
                self.edges[node.name()]
                    .iter()
                    .map(|e| &self.nodes[e.target.as_str()])
            },
            |_| BookmarkGraphError::Cycle,
        )?;
        Ok(result.into_iter())
    }


    fn find_root_commit(
        repo: &dyn Repo,
        trunk: &CommitId,
        head: &CommitId,
    ) -> Result<CommitId, BookmarkGraphError> {
        let trunk_expr = RevsetExpression::commit(trunk.clone());
        let head_expr = RevsetExpression::commit(head.clone());
        let roots = trunk_expr.range(&head_expr).roots();
        let expression = roots.evaluate(repo)?;
        expression
            .iter()
            .next()
            .and_then(|r| r.ok())
            .ok_or(BookmarkGraphError::NoRootCommit)
    }

    fn evaluate_branch_commits(
        repo: &dyn Repo,
        trunk: &CommitId,
        head: &CommitId,
    ) -> Result<Vec<GraphNode<CommitId>>, BookmarkGraphError> {
        let first_commit = Self::find_root_commit(repo, trunk, head)?;
        let expression = RevsetExpression::commit(first_commit).descendants();
        let revset = expression.evaluate(repo)?;
        Ok(reverse_graph(revset.iter_graph(), |id| id).expect("commit graph should be acyclic"))
    }

    fn build_bookmark_commit_map(repo: &dyn Repo) -> HashMap<CommitId, Bookmark> {
        let mut map = HashMap::new();
        repo.view().bookmarks().for_each(|(ref_name, ref_target)| {
            if let Some(commit_id) = ref_target.local_target.as_normal() {
                let remotes: Vec<RemoteTracking> = ref_target
                    .remote_refs
                    .iter()
                    .filter(|(remote_name, _)| *remote_name != REMOTE_NAME_FOR_LOCAL_GIT_REPO)
                    .map(|(remote_name, remote_ref)| RemoteTracking {
                        remote_name: remote_name.as_str().to_string(),
                        is_tracked: remote_ref.is_tracked(),
                    })
                    .collect();

                map.entry(commit_id.clone()).or_insert_with(|| {
                    Bookmark::with_remotes(ref_name.as_str().to_string(), remotes)
                });
            }
        });
        map
    }

    fn find_head_commits(reversed: &[GraphNode<CommitId>]) -> Vec<&CommitId> {
        let all_edge_targets: HashSet<&CommitId> = reversed
            .iter()
            .flat_map(|(_, edges)| edges.iter().map(|e| &e.target))
            .collect();

        reversed
            .iter()
            .map(|(id, _)| id)
            .filter(|id| !all_edge_targets.contains(id))
            .collect()
    }

    fn build_bookmark_graph(
        reversed: &[GraphNode<CommitId>],
        bookmarks_per_commit: &HashMap<CommitId, Bookmark>,
    ) -> (
        HashMap<String, BookmarkNode>,
        HashMap<String, Vec<GraphEdge<String>>>,
    ) {
        let commit_index: HashMap<&CommitId, &GraphNode<CommitId>> =
            reversed.iter().map(|node| (&node.0, node)).collect();

        let head_commits = Self::find_head_commits(reversed);

        let mut nodes: HashMap<String, BookmarkNode> = HashMap::new();
        let mut edges: HashMap<String, Vec<GraphEdge<String>>> = HashMap::new();
        let mut visited: HashSet<&CommitId> = HashSet::new();

        let mut stack: Vec<(&CommitId, Option<&str>)> =
            head_commits.into_iter().map(|c| (c, None)).collect();

        while let Some((commit_id, parent_name)) = stack.pop() {
            if !visited.insert(commit_id) {
                continue;
            }

            let maybe_bookmark = bookmarks_per_commit.get(commit_id);

            if let Some(bookmark) = maybe_bookmark {
                let name = bookmark.name().to_string();

                if !nodes.contains_key(&name) {
                    let mut node = BookmarkNode::new(bookmark.clone());

                    // Ascendants = parent's ascendants + parent
                    if let Some(pn) = parent_name {
                        if let Some(parent_node) = nodes.get(pn) {
                            for asc in parent_node.ascendants() {
                                node.add_ascendant(asc.clone());
                            }
                        }
                        node.add_ascendant(pn.to_string());
                    }

                    nodes.insert(name.clone(), node);
                }

                let edge_list = edges.entry(name.clone()).or_default();
                if let Some(pn) = parent_name
                    && pn != name
                {
                    edge_list.push(GraphEdge::direct(pn.to_string()));
                }
            }

            let next_name = maybe_bookmark.map(|b| b.name()).or(parent_name);

            if let Some(node) = commit_index.get(commit_id) {
                for edge in &node.1 {
                    stack.push((&edge.target, next_name));
                }
            }
        }

        (nodes, edges)
    }

    fn find_head_bookmarks(edges: &HashMap<String, Vec<GraphEdge<String>>>) -> HashSet<String> {
        let all_edge_targets: HashSet<&str> = edges
            .values()
            .flatten()
            .map(|e| e.target.as_str())
            .collect();

        edges
            .keys()
            .filter(|name| !all_edge_targets.contains(name.as_str()))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit_id(byte: u8) -> CommitId {
        CommitId::new(vec![byte])
    }

    // -- BookmarkNode tests --

    #[test]
    fn bookmark_node_new_has_empty_ascendants() {
        let node = BookmarkNode::new(Bookmark::new("feat".into()));
        assert_eq!(node.name(), "feat");
        assert!(node.ascendants().is_empty());
    }

    #[test]
    fn bookmark_node_accessors() {
        let bookmark = Bookmark::new("my-branch".into());
        let node = BookmarkNode::new(bookmark.clone());
        assert_eq!(node.bookmark(), &bookmark);
        assert_eq!(node.name(), "my-branch");
    }

    #[test]
    fn bookmark_node_add_ascendant() {
        let mut node = BookmarkNode::new(Bookmark::new("child".into()));
        node.add_ascendant("parent".into());
        node.add_ascendant("grandparent".into());
        assert_eq!(node.ascendants(), &["parent", "grandparent"]);
    }

    // -- find_head_commits tests --

    #[test]
    fn find_head_commits_linear_chain() {
        // A -> B -> C, head is A (not targeted by any edge)
        let a = commit_id(1);
        let b = commit_id(2);
        let c = commit_id(3);
        let reversed: Vec<GraphNode<CommitId>> = vec![
            (a.clone(), vec![GraphEdge::direct(b.clone())]),
            (b.clone(), vec![GraphEdge::direct(c.clone())]),
            (c.clone(), vec![]),
        ];

        let heads = BookmarkGraph::find_head_commits(&reversed);
        assert_eq!(heads, vec![&a]);
    }

    #[test]
    fn find_head_commits_multiple_heads() {
        // A -> [], B -> [] — both are heads
        let a = commit_id(1);
        let b = commit_id(2);
        let reversed: Vec<GraphNode<CommitId>> = vec![
            (a.clone(), vec![]),
            (b.clone(), vec![]),
        ];

        let heads = BookmarkGraph::find_head_commits(&reversed);
        assert_eq!(heads.len(), 2);
        assert!(heads.contains(&&a));
        assert!(heads.contains(&&b));
    }

    #[test]
    fn find_head_commits_single_node() {
        let a = commit_id(1);
        let reversed: Vec<GraphNode<CommitId>> = vec![(a.clone(), vec![])];

        let heads = BookmarkGraph::find_head_commits(&reversed);
        assert_eq!(heads, vec![&a]);
    }

    // -- find_head_bookmarks tests --

    #[test]
    fn find_head_bookmarks_linear() {
        // "feat" -> "base", head is "feat"
        let mut edges = HashMap::new();
        edges.insert("feat".to_string(), vec![GraphEdge::direct("base".to_string())]);
        edges.insert("base".to_string(), vec![]);

        let heads = BookmarkGraph::find_head_bookmarks(&edges);
        assert_eq!(heads, HashSet::from(["feat".to_string()]));
    }

    #[test]
    fn find_head_bookmarks_multiple_heads() {
        // "a" -> [], "b" -> [] — both are heads
        let mut edges = HashMap::new();
        edges.insert("a".to_string(), vec![]);
        edges.insert("b".to_string(), vec![]);

        let heads = BookmarkGraph::find_head_bookmarks(&edges);
        assert_eq!(heads, HashSet::from(["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn find_head_bookmarks_diamond() {
        // "top" -> ["left", "right"], "left" -> ["base"], "right" -> ["base"], "base" -> []
        let mut edges = HashMap::new();
        edges.insert(
            "top".to_string(),
            vec![
                GraphEdge::direct("left".to_string()),
                GraphEdge::direct("right".to_string()),
            ],
        );
        edges.insert("left".to_string(), vec![GraphEdge::direct("base".to_string())]);
        edges.insert("right".to_string(), vec![GraphEdge::direct("base".to_string())]);
        edges.insert("base".to_string(), vec![]);

        let heads = BookmarkGraph::find_head_bookmarks(&edges);
        assert_eq!(heads, HashSet::from(["top".to_string()]));
    }

    // -- build_bookmark_graph tests --

    #[test]
    fn build_bookmark_graph_linear_chain() {
        // Commits: A -> B -> C
        // Bookmarks: A = "feat-a", C = "feat-c" (B has no bookmark)
        // Expected: feat-a -> feat-c (edge from head to tail through unbookmarked B)
        let a = commit_id(1);
        let b = commit_id(2);
        let c = commit_id(3);

        let reversed: Vec<GraphNode<CommitId>> = vec![
            (a.clone(), vec![GraphEdge::direct(b.clone())]),
            (b.clone(), vec![GraphEdge::direct(c.clone())]),
            (c.clone(), vec![]),
        ];

        let mut bookmarks = HashMap::new();
        bookmarks.insert(a.clone(), Bookmark::new("feat-a".into()));
        bookmarks.insert(c.clone(), Bookmark::new("feat-c".into()));

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        assert_eq!(nodes.len(), 2);
        assert!(nodes.contains_key("feat-a"));
        assert!(nodes.contains_key("feat-c"));

        // feat-a is head, has no outgoing edges to parents
        // (it IS the parent from feat-c's perspective)
        assert!(edges["feat-a"].is_empty() || edges["feat-a"].iter().all(|e| e.target != "feat-a"));

        // feat-c should have an edge to feat-a
        let feat_c_targets: Vec<&str> = edges["feat-c"]
            .iter()
            .map(|e| e.target.as_str())
            .collect();
        assert!(feat_c_targets.contains(&"feat-a"));
    }

    #[test]
    fn build_bookmark_graph_all_commits_bookmarked() {
        // A -> B -> C, all with bookmarks
        let a = commit_id(1);
        let b = commit_id(2);
        let c = commit_id(3);

        let reversed: Vec<GraphNode<CommitId>> = vec![
            (a.clone(), vec![GraphEdge::direct(b.clone())]),
            (b.clone(), vec![GraphEdge::direct(c.clone())]),
            (c.clone(), vec![]),
        ];

        let mut bookmarks = HashMap::new();
        bookmarks.insert(a.clone(), Bookmark::new("top".into()));
        bookmarks.insert(b.clone(), Bookmark::new("mid".into()));
        bookmarks.insert(c.clone(), Bookmark::new("base".into()));

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        assert_eq!(nodes.len(), 3);
        // top -> mid -> base
        let top_targets: Vec<&str> = edges["top"].iter().map(|e| e.target.as_str()).collect();
        assert!(top_targets.is_empty()); // top is head, no parent bookmark above it

        let mid_targets: Vec<&str> = edges["mid"].iter().map(|e| e.target.as_str()).collect();
        assert!(mid_targets.contains(&"top"));

        let base_targets: Vec<&str> = edges["base"].iter().map(|e| e.target.as_str()).collect();
        assert!(base_targets.contains(&"mid"));
    }

    #[test]
    fn build_bookmark_graph_no_bookmarks_produces_empty() {
        let a = commit_id(1);
        let reversed: Vec<GraphNode<CommitId>> = vec![(a.clone(), vec![])];
        let bookmarks = HashMap::new();

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);
        assert!(nodes.is_empty());
        assert!(edges.is_empty());
    }
}
