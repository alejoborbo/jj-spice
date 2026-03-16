use itertools::Itertools;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use jj_lib::{
    backend::CommitId,
    dag_walk::topo_order_forward,
    graph::{GraphEdge, GraphNode, reverse_graph},
    repo::Repo,
    revset::{RevsetEvaluationError, RevsetExpression},
};
use thiserror::Error;

use super::Bookmark;

/// Nodes keyed by bookmark name and their outgoing edges, as built by
/// [`BookmarkGraph::build_bookmark_graph`].
type BookmarkGraphParts<'a> = (
    HashMap<String, BookmarkNode<'a>>,
    HashMap<String, Vec<GraphEdge<String>>>,
);

/// A node in the bookmark DAG, wrapping a [`Bookmark`] with ancestry info.
#[derive(Clone, Debug)]
pub struct BookmarkNode<'a> {
    bookmark: Bookmark<'a>,
    ascendants: Vec<String>,
    commits: Vec<CommitId>,
}

impl<'a> BookmarkNode<'a> {
    /// Wrap a bookmark as a graph node with no ascendants.
    pub fn new(bookmark: Bookmark<'a>) -> Self {
        Self {
            bookmark,
            ascendants: Vec::new(),
            commits: Vec::new(),
        }
    }

    /// The underlying bookmark.
    pub fn bookmark(&self) -> &Bookmark<'_> {
        &self.bookmark
    }

    /// Shorthand for `self.bookmark().name()`.
    pub fn name(&self) -> &str {
        self.bookmark.name()
    }

    pub fn ascendants(&self) -> &[String] {
        &self.ascendants
    }

    pub fn commits(&self) -> &[CommitId] {
        &self.commits
    }

    fn add_ascendant(&mut self, ascendant: String) {
        self.ascendants.push(ascendant);
    }

    fn add_commit(&mut self, commit: CommitId) {
        self.commits.push(commit);
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
pub struct BookmarkGraph<'a> {
    nodes: HashMap<String, BookmarkNode<'a>>,
    edges: HashMap<String, Vec<GraphEdge<String>>>,
    head_bookmarks: HashSet<String>,
}

impl<'a> BookmarkGraph<'a> {
    /// Build a bookmark graph from commits between `trunk` and `head`.
    ///
    /// Both should be pre-resolved commit IDs. Typically `trunk` comes from
    /// evaluating the `trunk()` revset alias, and `head` is the working-copy
    /// commit (`@`).
    pub fn new(
        repo: &'a (dyn Repo + 'a),
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

    /// Return the edges (child → parent) for a bookmark, or an empty vec if
    /// the name is not in the graph.
    pub fn edges_for(&self, name: &str) -> Vec<GraphEdge<String>> {
        self.edges.get(name).cloned().unwrap_or_default()
    }

    /// Iterate bookmarks in topological order (roots first).
    pub fn iter_graph(&self) -> Result<impl Iterator<Item = &BookmarkNode<'a>>, BookmarkGraphError> {
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

    fn build_bookmark_commit_map(repo: &'a (dyn Repo + 'a)) -> HashMap<CommitId, Vec<Arc<Bookmark<'a>>>> {
        repo.view()
            .bookmarks()
            .filter_map(|(ref_name, ref_target)| {
                // Ignore unresolved/conflicted commits
                // TODO: Improve this behavior
                if let Some(commit_id) = ref_target.local_target.as_normal() {
                    return Some((
                        commit_id.to_owned(),
                        Arc::new(Bookmark::new(ref_name.as_str().to_string(), ref_target)),
                    ));
                }
                None
            })
            .into_group_map()
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
        bookmarks_per_commit: &HashMap<CommitId, Vec<Arc<Bookmark<'a>>>>,
    ) -> BookmarkGraphParts<'a> {
        let commit_index: HashMap<&CommitId, &GraphNode<CommitId>> =
            reversed.iter().map(|node| (&node.0, node)).collect();

        let head_commits = Self::find_head_commits(reversed);

        let mut nodes: HashMap<String, BookmarkNode> = HashMap::new();
        let mut edges: HashMap<String, Vec<GraphEdge<String>>> = HashMap::new();
        let mut visited: HashSet<&CommitId> = HashSet::new();

        let mut stack: Vec<(&CommitId, Option<&str>)> =
            head_commits.into_iter().map(|c| (c, None)).collect();

        while let Some((commit_id, parent_name)) = stack.pop() {
            let already_visited = !visited.insert(commit_id);

            let maybe_bookmarks = bookmarks_per_commit.get(commit_id);

            if let Some(bookmarks) = maybe_bookmarks {
                for bookmark in bookmarks {
                    let name = bookmark.name().to_string();

                    let node = nodes
                        .entry(name.clone())
                        .or_insert_with(|| BookmarkNode::new(bookmark.as_ref().clone()));

                    // Add commit to current node
                    if !node.commits.contains(commit_id) {
                        node.add_commit(commit_id.clone());
                    }

                    // Add parent bookmark as ascendant of current node
                    if let Some(pn) = parent_name
                        && !node.ascendants.contains(&pn.to_string())
                    {
                        node.add_ascendant(pn.to_string());
                    }

                    let edge_list = edges.entry(name.clone()).or_default();
                    if let Some(pn) = parent_name
                        && pn != name
                        && !edge_list.iter().any(|e| e.target == pn)
                    {
                        edge_list.push(GraphEdge::direct(pn.to_string()));
                    }
                }
            }

            // Only traverse children on first visit to avoid infinite loops.
            if already_visited {
                continue;
            }

            if let Some(graph_node) = commit_index.get(commit_id) {
                if let Some(bookmarks) = maybe_bookmarks {
                    // Push children once per bookmark on this commit,
                    // so each child discovers all parent bookmarks.
                    for bookmark in bookmarks {
                        for edge in &graph_node.1 {
                            stack.push((&edge.target, Some(bookmark.name())));
                        }
                    }
                } else {
                    // No bookmarks on this commit — pass through the parent name.
                    for edge in &graph_node.1 {
                        stack.push((&edge.target, parent_name));
                    }
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
    use jj_lib::op_store::{LocalRemoteRefTarget, RefTarget};

    fn commit_id(byte: u8) -> CommitId {
        CommitId::new(vec![byte])
    }

    fn make_bookmark(name: &str) -> Bookmark<'static> {
        Bookmark::new(
            name.to_string(),
            LocalRemoteRefTarget {
                local_target: RefTarget::absent_ref(),
                remote_refs: vec![],
            },
        )
    }

    fn bookmark_map(
        entries: Vec<(CommitId, Vec<&str>)>,
    ) -> HashMap<CommitId, Vec<Arc<Bookmark<'static>>>> {
        entries
            .into_iter()
            .map(|(id, names)| {
                (
                    id,
                    names
                        .into_iter()
                        .map(|n| Arc::new(make_bookmark(n)))
                        .collect(),
                )
            })
            .collect()
    }

    // -- BookmarkNode tests --

    #[test]
    fn bookmark_node_new_has_empty_ascendants() {
        let node = BookmarkNode::new(make_bookmark("feat"));
        assert_eq!(node.name(), "feat");
        assert!(node.ascendants().is_empty());
    }

    #[test]
    fn bookmark_node_accessors() {
        let node = BookmarkNode::new(make_bookmark("my-branch"));
        assert_eq!(node.bookmark(), &make_bookmark("my-branch"));
        assert_eq!(node.name(), "my-branch");
    }

    #[test]
    fn bookmark_node_add_ascendant() {
        let mut node = BookmarkNode::new(make_bookmark("child"));
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
        let reversed: Vec<GraphNode<CommitId>> = vec![(a.clone(), vec![]), (b.clone(), vec![])];

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
        edges.insert(
            "feat".to_string(),
            vec![GraphEdge::direct("base".to_string())],
        );
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
        edges.insert(
            "left".to_string(),
            vec![GraphEdge::direct("base".to_string())],
        );
        edges.insert(
            "right".to_string(),
            vec![GraphEdge::direct("base".to_string())],
        );
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

        let bookmarks = bookmark_map(vec![
            (a.clone(), vec!["feat-a"]),
            (c.clone(), vec!["feat-c"]),
        ]);

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        assert_eq!(nodes.len(), 2);
        assert!(nodes.contains_key("feat-a"));
        assert!(nodes.contains_key("feat-c"));

        // feat-a is head, has no outgoing edges to parents
        // (it IS the parent from feat-c's perspective)
        assert!(edges["feat-a"].is_empty() || edges["feat-a"].iter().all(|e| e.target != "feat-a"));

        // feat-c should have an edge to feat-a
        let feat_c_targets: Vec<&str> = edges["feat-c"].iter().map(|e| e.target.as_str()).collect();
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

        let bookmarks = bookmark_map(vec![
            (a.clone(), vec!["top"]),
            (b.clone(), vec!["mid"]),
            (c.clone(), vec!["base"]),
        ]);

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
    fn build_bookmark_graph_diamond_records_both_ascendants() {
        //   A (top)
        //  / \
        // B   C  (left, right)
        //  \ /
        //   D    (base)
        let a = commit_id(1);
        let b = commit_id(2);
        let c = commit_id(3);
        let d = commit_id(4);

        let reversed: Vec<GraphNode<CommitId>> = vec![
            (
                a.clone(),
                vec![GraphEdge::direct(b.clone()), GraphEdge::direct(c.clone())],
            ),
            (b.clone(), vec![GraphEdge::direct(d.clone())]),
            (c.clone(), vec![GraphEdge::direct(d.clone())]),
            (d.clone(), vec![]),
        ];

        let bookmarks = bookmark_map(vec![
            (a.clone(), vec!["top"]),
            (b.clone(), vec!["left"]),
            (c.clone(), vec!["right"]),
            (d.clone(), vec!["base"]),
        ]);

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        assert_eq!(nodes.len(), 4);

        // base should have both left and right as ascendants
        let base_ascendants = nodes["base"].ascendants();
        assert_eq!(
            base_ascendants.len(),
            2,
            "base should have 2 ascendants, got: {base_ascendants:?}"
        );
        assert!(base_ascendants.contains(&"left".to_string()));
        assert!(base_ascendants.contains(&"right".to_string()));

        // base edges should point to both left and right
        let base_edge_targets: HashSet<&str> =
            edges["base"].iter().map(|e| e.target.as_str()).collect();
        assert!(base_edge_targets.contains("left"));
        assert!(base_edge_targets.contains("right"));
    }

    #[test]
    fn build_bookmark_graph_single_bookmark() {
        // One commit with a bookmark, no edges.
        let a = commit_id(1);
        let reversed: Vec<GraphNode<CommitId>> = vec![(a.clone(), vec![])];

        let bookmarks = bookmark_map(vec![(a.clone(), vec!["only"])]);

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        assert_eq!(nodes.len(), 1);
        assert!(nodes["only"].ascendants().is_empty());
        assert!(edges["only"].is_empty());
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

    #[test]
    fn build_bookmark_graph_skips_unbookmarked_commits() {
        // A -> B -> C -> D, only A and D have bookmarks.
        // Should produce: head -> base, skipping B and C.
        let a = commit_id(1);
        let b = commit_id(2);
        let c = commit_id(3);
        let d = commit_id(4);

        let reversed: Vec<GraphNode<CommitId>> = vec![
            (a.clone(), vec![GraphEdge::direct(b.clone())]),
            (b.clone(), vec![GraphEdge::direct(c.clone())]),
            (c.clone(), vec![GraphEdge::direct(d.clone())]),
            (d.clone(), vec![]),
        ];

        let bookmarks = bookmark_map(vec![
            (a.clone(), vec!["head"]),
            (d.clone(), vec!["base"]),
        ]);

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        assert_eq!(nodes.len(), 2);
        assert!(nodes["head"].ascendants().is_empty());
        assert_eq!(nodes["base"].ascendants(), &["head"]);

        let base_targets: Vec<&str> = edges["base"].iter().map(|e| e.target.as_str()).collect();
        assert_eq!(base_targets, vec!["head"]);
        assert!(edges["head"].is_empty());
    }

    #[test]
    fn build_bookmark_graph_fork_two_branches() {
        //   A (root)
        //  / \
        // B   C  (left, right) — no shared descendant
        let a = commit_id(1);
        let b = commit_id(2);
        let c = commit_id(3);

        let reversed: Vec<GraphNode<CommitId>> = vec![
            (
                a.clone(),
                vec![GraphEdge::direct(b.clone()), GraphEdge::direct(c.clone())],
            ),
            (b.clone(), vec![]),
            (c.clone(), vec![]),
        ];

        let bookmarks = bookmark_map(vec![
            (a.clone(), vec!["root"]),
            (b.clone(), vec!["left"]),
            (c.clone(), vec!["right"]),
        ]);

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        assert_eq!(nodes.len(), 3);

        // Both left and right should have root as their only ascendant.
        assert_eq!(nodes["left"].ascendants(), &["root"]);
        assert_eq!(nodes["right"].ascendants(), &["root"]);
        assert!(nodes["root"].ascendants().is_empty());

        let left_targets: Vec<&str> = edges["left"].iter().map(|e| e.target.as_str()).collect();
        assert_eq!(left_targets, vec!["root"]);

        let right_targets: Vec<&str> = edges["right"].iter().map(|e| e.target.as_str()).collect();
        assert_eq!(right_targets, vec!["root"]);
    }

    #[test]
    fn build_bookmark_graph_ascendants_not_duplicated() {
        // A -> B, B -> C, A -> C  (two paths to C from A, through B and direct)
        // All bookmarked. C should have A as ascendant only once.
        let a = commit_id(1);
        let b = commit_id(2);
        let c = commit_id(3);

        let reversed: Vec<GraphNode<CommitId>> = vec![
            (
                a.clone(),
                vec![GraphEdge::direct(b.clone()), GraphEdge::direct(c.clone())],
            ),
            (b.clone(), vec![GraphEdge::direct(c.clone())]),
            (c.clone(), vec![]),
        ];

        let bookmarks = bookmark_map(vec![
            (a.clone(), vec!["top"]),
            (b.clone(), vec!["mid"]),
            (c.clone(), vec!["base"]),
        ]);

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        // base is reachable from top (directly) and from mid.
        // ascendants should contain both "top" and "mid", each once.
        let base_ascendants = nodes["base"].ascendants();
        assert_eq!(base_ascendants.len(), 2, "got: {base_ascendants:?}");
        assert!(base_ascendants.contains(&"top".to_string()));
        assert!(base_ascendants.contains(&"mid".to_string()));

        // edges should also have both, no duplicates
        let base_edge_targets: HashSet<&str> =
            edges["base"].iter().map(|e| e.target.as_str()).collect();
        assert_eq!(base_edge_targets.len(), 2);
        assert!(base_edge_targets.contains("top"));
        assert!(base_edge_targets.contains("mid"));
    }

    #[test]
    fn build_bookmark_graph_multiple_bookmarks_on_same_commit() {
        // A -> B, commit A has two bookmarks "feat-1" and "feat-2", B has "base"
        // Both feat-1 and feat-2 should be head nodes, base should have both as ascendants.
        let a = commit_id(1);
        let b = commit_id(2);

        let reversed: Vec<GraphNode<CommitId>> = vec![
            (a.clone(), vec![GraphEdge::direct(b.clone())]),
            (b.clone(), vec![]),
        ];

        let bookmarks = bookmark_map(vec![
            (a.clone(), vec!["feat-1", "feat-2"]),
            (b.clone(), vec!["base"]),
        ]);

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        assert_eq!(nodes.len(), 3);
        assert!(nodes.contains_key("feat-1"));
        assert!(nodes.contains_key("feat-2"));
        assert!(nodes.contains_key("base"));

        // base should have both feat-1 and feat-2 as ascendants
        let base_ascendants = nodes["base"].ascendants();
        assert_eq!(
            base_ascendants.len(),
            2,
            "base should have 2 ascendants, got: {base_ascendants:?}"
        );
        assert!(base_ascendants.contains(&"feat-1".to_string()));
        assert!(base_ascendants.contains(&"feat-2".to_string()));

        // base edges should point to both
        let base_edge_targets: HashSet<&str> =
            edges["base"].iter().map(|e| e.target.as_str()).collect();
        assert!(base_edge_targets.contains("feat-1"));
        assert!(base_edge_targets.contains("feat-2"));

        // feat-1 and feat-2 should have no ascendants (they are heads)
        assert!(nodes["feat-1"].ascendants().is_empty());
        assert!(nodes["feat-2"].ascendants().is_empty());
    }
}
