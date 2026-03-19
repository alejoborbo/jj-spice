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
    revset::{ResolvedRevsetExpression, RevsetEvaluationError, RevsetExpression},
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
    ///
    /// Used by `stack submit` and `stack sync` which operate on the single
    /// stack under the working copy.
    pub fn new(
        repo: &'a (dyn Repo + 'a),
        trunk: &CommitId,
        head: &CommitId,
    ) -> Result<Self, BookmarkGraphError> {
        let heads_expr = RevsetExpression::commit(head.clone());
        Self::build(repo, trunk, &heads_expr)
    }

    /// Build a bookmark graph covering **all** local bookmarks between `trunk`
    /// and the heads of every local bookmark.
    ///
    /// This produces the union of all in-flight stacks the user has locally,
    /// matching the spirit of `jj log` which shows all visible history.
    /// Remote-only bookmarks (from other contributors) are excluded.
    pub fn all_local(
        repo: &'a (dyn Repo + 'a),
        trunk: &CommitId,
    ) -> Result<Self, BookmarkGraphError> {
        let bookmarks_per_commit = Self::build_bookmark_commit_map(repo);
        let commit_ids: Vec<CommitId> = bookmarks_per_commit.keys().cloned().collect();
        if commit_ids.is_empty() {
            return Ok(Self {
                nodes: HashMap::new(),
                edges: HashMap::new(),
                head_bookmarks: HashSet::new(),
            });
        }
        let heads_expr = RevsetExpression::commits(commit_ids);
        let reversed = Self::evaluate_branch_commits(repo, trunk, &heads_expr)?;
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
    pub fn iter_graph(
        &self,
    ) -> Result<impl Iterator<Item = &BookmarkNode<'a>>, BookmarkGraphError> {
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

    /// Shared constructor logic: evaluate `trunk..heads` and build the graph.
    fn build(
        repo: &'a (dyn Repo + 'a),
        trunk: &CommitId,
        heads: &Arc<ResolvedRevsetExpression>,
    ) -> Result<Self, BookmarkGraphError> {
        let bookmarks_per_commit = Self::build_bookmark_commit_map(repo);
        let reversed = Self::evaluate_branch_commits(repo, trunk, heads)?;
        let (nodes, edges) = Self::build_bookmark_graph(&reversed, &bookmarks_per_commit);
        let head_bookmarks = Self::find_head_bookmarks(&edges);
        Ok(Self {
            nodes,
            edges,
            head_bookmarks,
        })
    }

    /// Evaluate all commits between `trunk` (exclusive) and `heads` (inclusive).
    ///
    /// Uses the `trunk..heads` revset which naturally captures every root and
    /// every path, avoiding the previous bug where only the first root was
    /// used and descendants were unbounded.
    fn evaluate_branch_commits(
        repo: &dyn Repo,
        trunk: &CommitId,
        heads: &Arc<ResolvedRevsetExpression>,
    ) -> Result<Vec<GraphNode<CommitId>>, BookmarkGraphError> {
        let trunk_expr = RevsetExpression::commit(trunk.clone());
        let range = trunk_expr.range(heads);
        let revset = range.evaluate(repo)?;
        Ok(reverse_graph(revset.iter_graph(), |id| id).expect("commit graph should be acyclic"))
    }

    fn build_bookmark_commit_map(
        repo: &'a (dyn Repo + 'a),
    ) -> HashMap<CommitId, Vec<Arc<Bookmark<'a>>>> {
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

        let bookmarks = bookmark_map(vec![(a.clone(), vec!["head"]), (d.clone(), vec!["base"])]);

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

    #[test]
    fn build_bookmark_graph_parent_with_multiple_children() {
        // Regression test modelling the dd-go scenario where a single parent
        // bookmark fans out into several children:
        //
        //        A (scheduler-cdk8s-library)
        //       /|\ \
        //      B  C  D  E
        //  (deployment) (monitoring) (networking) (storage)
        //
        // The reversed graph (parent → children) has A as the single head and
        // B, C, D, E as leaves. All children must list A as their ascendant.
        let a = commit_id(1);
        let b = commit_id(2);
        let c = commit_id(3);
        let d = commit_id(4);
        let e = commit_id(5);

        let reversed: Vec<GraphNode<CommitId>> = vec![
            (
                a.clone(),
                vec![
                    GraphEdge::direct(b.clone()),
                    GraphEdge::direct(c.clone()),
                    GraphEdge::direct(d.clone()),
                    GraphEdge::direct(e.clone()),
                ],
            ),
            (b.clone(), vec![]),
            (c.clone(), vec![]),
            (d.clone(), vec![]),
            (e.clone(), vec![]),
        ];

        let bookmarks = bookmark_map(vec![
            (a.clone(), vec!["scheduler-cdk8s-library"]),
            (b.clone(), vec!["scheduler-cdk8s-deployment"]),
            (c.clone(), vec!["scheduler-cdk8s-monitoring"]),
            (d.clone(), vec!["scheduler-cdk8s-networking"]),
            (e.clone(), vec!["scheduler-cdk8s-storage"]),
        ]);

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        // All five bookmarks must be discovered.
        assert_eq!(
            nodes.len(),
            5,
            "expected 5 bookmark nodes, got: {:?}",
            nodes.keys().collect::<Vec<_>>()
        );

        // The library bookmark is the root (no ascendants).
        assert!(
            nodes["scheduler-cdk8s-library"].ascendants().is_empty(),
            "library should be a root, got ascendants: {:?}",
            nodes["scheduler-cdk8s-library"].ascendants()
        );

        // Every child bookmark must have exactly the library as ascendant.
        for child in [
            "scheduler-cdk8s-deployment",
            "scheduler-cdk8s-monitoring",
            "scheduler-cdk8s-networking",
            "scheduler-cdk8s-storage",
        ] {
            assert_eq!(
                nodes[child].ascendants(),
                &["scheduler-cdk8s-library"],
                "{child} should descend from scheduler-cdk8s-library, got: {:?}",
                nodes[child].ascendants()
            );

            // Edge should point back to library.
            let targets: Vec<&str> = edges[child].iter().map(|e| e.target.as_str()).collect();
            assert_eq!(
                targets,
                vec!["scheduler-cdk8s-library"],
                "{child} edge should point to scheduler-cdk8s-library, got: {targets:?}"
            );
        }

        // The library bookmark should have no edges (it is a head in the
        // reversed graph, meaning a root in the original DAG).
        assert!(
            edges["scheduler-cdk8s-library"].is_empty(),
            "library should have no parent edges, got: {:?}",
            edges["scheduler-cdk8s-library"]
        );
    }

    #[test]
    fn build_bookmark_graph_multiple_roots_converge() {
        // Regression test: two independent paths from trunk converge at a
        // shared descendant. The old implementation only followed one root
        // (via find_root_commit().next()), missing the other sub-stack.
        //
        // The commit graph between trunk and head looks like:
        //
        //   R1 (library)        R2 (other-base)
        //    |                   |
        //    A (library-impl)    B (other-impl)
        //     \                 /
        //      C (deployment)  ← head / @
        //
        // With trunk..head, both R1 and R2 are included as roots.
        // Previously, only one root was picked, losing the other branch.
        let r1 = commit_id(1);
        let r2 = commit_id(2);
        let a = commit_id(3);
        let b = commit_id(4);
        let c = commit_id(5);

        // Reversed graph: heads (no incoming edges) are R1 and R2.
        let reversed: Vec<GraphNode<CommitId>> = vec![
            (r1.clone(), vec![GraphEdge::direct(a.clone())]),
            (r2.clone(), vec![GraphEdge::direct(b.clone())]),
            (a.clone(), vec![GraphEdge::direct(c.clone())]),
            (b.clone(), vec![GraphEdge::direct(c.clone())]),
            (c.clone(), vec![]),
        ];

        let bookmarks = bookmark_map(vec![
            (r1.clone(), vec!["library"]),
            (a.clone(), vec!["library-impl"]),
            (r2.clone(), vec!["other-base"]),
            (b.clone(), vec!["other-impl"]),
            (c.clone(), vec!["deployment"]),
        ]);

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        // All five bookmarks must be discovered.
        assert_eq!(
            nodes.len(),
            5,
            "expected 5 bookmark nodes, got: {:?}",
            nodes.keys().collect::<Vec<_>>()
        );
        assert!(nodes.contains_key("library"));
        assert!(nodes.contains_key("library-impl"));
        assert!(nodes.contains_key("other-base"));
        assert!(nodes.contains_key("other-impl"));
        assert!(nodes.contains_key("deployment"));

        // library and other-base are root bookmarks (no ascendants).
        assert!(
            nodes["library"].ascendants().is_empty(),
            "library should be a root, got ascendants: {:?}",
            nodes["library"].ascendants()
        );
        assert!(
            nodes["other-base"].ascendants().is_empty(),
            "other-base should be a root, got ascendants: {:?}",
            nodes["other-base"].ascendants()
        );

        // library-impl descends from library.
        assert_eq!(nodes["library-impl"].ascendants(), &["library"]);

        // other-impl descends from other-base.
        assert_eq!(nodes["other-impl"].ascendants(), &["other-base"]);

        // deployment descends from both library-impl and other-impl.
        let deploy_ascendants = nodes["deployment"].ascendants();
        assert_eq!(
            deploy_ascendants.len(),
            2,
            "deployment should have 2 ascendants, got: {deploy_ascendants:?}"
        );
        assert!(deploy_ascendants.contains(&"library-impl".to_string()));
        assert!(deploy_ascendants.contains(&"other-impl".to_string()));

        // Edges should mirror the ascendant relationships.
        let deploy_edge_targets: HashSet<&str> = edges["deployment"]
            .iter()
            .map(|e| e.target.as_str())
            .collect();
        assert!(deploy_edge_targets.contains("library-impl"));
        assert!(deploy_edge_targets.contains("other-impl"));
    }

    #[test]
    fn build_bookmark_graph_disjoint_stacks_all_discovered() {
        // Models the `all_local` scenario: two completely independent stacks
        // off trunk with no shared commits. Using all local bookmark commit
        // IDs as heads for `trunk..heads` produces a reversed graph containing
        // both stacks, and build_bookmark_graph must discover all of them.
        //
        //  Stack 1:        Stack 2:
        //  A (feat-a)      C (feat-c)
        //  |               |
        //  B (feat-b)      D (feat-d)
        //
        // No edges between the two stacks.
        let a = commit_id(1);
        let b = commit_id(2);
        let c = commit_id(3);
        let d = commit_id(4);

        let reversed: Vec<GraphNode<CommitId>> = vec![
            (a.clone(), vec![GraphEdge::direct(b.clone())]),
            (b.clone(), vec![]),
            (c.clone(), vec![GraphEdge::direct(d.clone())]),
            (d.clone(), vec![]),
        ];

        let bookmarks = bookmark_map(vec![
            (a.clone(), vec!["feat-a"]),
            (b.clone(), vec!["feat-b"]),
            (c.clone(), vec!["feat-c"]),
            (d.clone(), vec!["feat-d"]),
        ]);

        let (nodes, edges) = BookmarkGraph::build_bookmark_graph(&reversed, &bookmarks);

        // All four bookmarks across both stacks must be present.
        assert_eq!(
            nodes.len(),
            4,
            "expected 4 bookmark nodes, got: {:?}",
            nodes.keys().collect::<Vec<_>>()
        );

        // Stack 1: feat-a is head, feat-b descends from feat-a.
        assert!(nodes["feat-a"].ascendants().is_empty());
        assert_eq!(nodes["feat-b"].ascendants(), &["feat-a"]);
        let b_targets: Vec<&str> = edges["feat-b"].iter().map(|e| e.target.as_str()).collect();
        assert_eq!(b_targets, vec!["feat-a"]);

        // Stack 2: feat-c is head, feat-d descends from feat-c.
        assert!(nodes["feat-c"].ascendants().is_empty());
        assert_eq!(nodes["feat-d"].ascendants(), &["feat-c"]);
        let d_targets: Vec<&str> = edges["feat-d"].iter().map(|e| e.target.as_str()).collect();
        assert_eq!(d_targets, vec!["feat-c"]);

        // The two stacks are independent — no cross-edges.
        assert!(edges["feat-a"].is_empty());
        assert!(edges["feat-c"].is_empty());
    }
}
