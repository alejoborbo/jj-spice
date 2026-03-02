use std::collections::{HashMap, HashSet};

use jj_lib::{
    backend::CommitId,
    dag_walk::topo_order_forward,
    graph::{GraphEdge, GraphNode, reverse_graph},
    repo::Repo,
    revset::{
        Revset, RevsetEvaluationError, RevsetExpression, RevsetExtensions, RevsetResolutionError,
        SymbolResolver, UserRevsetExpression,
    },
    workspace::Workspace,
};
use thiserror::Error;

use crate::bookmark::Bookmark;

#[derive(Debug, Error)]
pub enum BookmarkGraphError {
    #[error("revset evaluation failed")]
    RevsetEvaluation(#[from] RevsetEvaluationError),
    #[error("revset resolution failed")]
    RevsetResolution(#[from] RevsetResolutionError),
    #[error("no root commit found in branch")]
    NoRootCommit,
    #[error("cycle detected in bookmark graph")]
    Cycle,
}

#[derive(Debug)]
pub struct BookmarkGraph {
    nodes: HashMap<Bookmark, Vec<GraphEdge<String>>>,
    head_bookmarks: HashSet<Bookmark>,
}

impl BookmarkGraph {
    pub fn new(
        repo: &dyn Repo,
        workspace: &Workspace,
        trunk_name: &str,
    ) -> Result<Self, BookmarkGraphError> {
        let bookmarks_per_commit = Self::build_bookmark_commit_map(repo);
        let reversed = Self::build_reversed_commit_graph(repo, workspace, trunk_name)?;
        let nodes = Self::build_bookmark_graph(&reversed, &bookmarks_per_commit);
        let head_bookmarks = Self::find_head_bookmarks(&nodes);
        Ok(Self {
            nodes,
            head_bookmarks,
        })
    }

    pub fn iter_graph(&self) -> Result<impl Iterator<Item = &Bookmark>, BookmarkGraphError> {
        let string_to_bookmark: HashMap<&str, &Bookmark> =
            self.nodes.keys().map(|b| (b.name(), b)).collect();
        let result = topo_order_forward(
            self.heads().iter(),
            |b| b.name(),
            |&b| {
                self.edges(b)
                    .unwrap_or_default()
                    .iter()
                    .map(|e| *string_to_bookmark.get(e.target.as_str()).unwrap())
            },
            |_| BookmarkGraphError::Cycle,
        )?;
        Ok(result.into_iter())
    }

    pub fn edges(&self, bookmark: &Bookmark) -> Option<&[GraphEdge<String>]> {
        self.nodes.get(bookmark).map(Vec::as_slice)
    }

    pub fn heads(&self) -> &HashSet<Bookmark> {
        &self.head_bookmarks
    }

    pub fn bookmarks(&self) -> impl Iterator<Item = &Bookmark> {
        self.nodes.keys()
    }

    fn symbol_resolver(repo: &dyn Repo) -> SymbolResolver<'_> {
        SymbolResolver::new(repo, RevsetExtensions::default().symbol_resolvers())
    }

    fn find_root_commit(
        repo: &dyn Repo,
        workspace: &Workspace,
        trunk_name: &str,
    ) -> Result<CommitId, BookmarkGraphError> {
        let trunk = UserRevsetExpression::symbol(trunk_name.to_string());
        let wc = RevsetExpression::working_copy(workspace.workspace_name().to_owned());
        let branch_commits = trunk.range(&wc);
        let first_mutable = branch_commits
            .roots()
            .resolve_user_expression(repo, &Self::symbol_resolver(repo))?;
        let expression = first_mutable.evaluate(repo)?;
        expression
            .iter()
            .next()
            .and_then(|r| r.ok())
            .ok_or(BookmarkGraphError::NoRootCommit)
    }

    fn evaluate_branch_commits<'a>(
        repo: &'a dyn Repo,
        workspace: &Workspace,
        trunk_name: &str,
    ) -> Result<Box<dyn Revset + 'a>, BookmarkGraphError> {
        let first_commit = Self::find_root_commit(repo, workspace, trunk_name)?;
        let expression = RevsetExpression::commit(first_commit).descendants();
        Ok(expression.evaluate(repo)?)
    }

    fn build_bookmark_commit_map(repo: &dyn Repo) -> HashMap<CommitId, Bookmark> {
        let mut map = HashMap::new();
        repo.view().bookmarks().for_each(|(ref_name, ref_target)| {
            if let Some(commit_id) = ref_target.local_target.as_normal() {
                map.entry(commit_id.clone())
                    .or_insert_with(|| Bookmark::new(ref_name.as_str().to_string()));
            }
        });
        map
    }

    fn build_reversed_commit_graph(
        repo: &dyn Repo,
        workspace: &Workspace,
        trunk_name: &str,
    ) -> Result<Vec<GraphNode<CommitId>>, BookmarkGraphError> {
        let revset = Self::evaluate_branch_commits(repo, workspace, trunk_name)?;
        Ok(reverse_graph(revset.iter_graph(), |id| id).expect("commit graph should be acyclic"))
    }

    fn build_bookmark_graph(
        reversed: &[GraphNode<CommitId>],
        bookmarks_per_commit: &HashMap<CommitId, Bookmark>,
    ) -> HashMap<Bookmark, Vec<GraphEdge<String>>> {
        let commit_index: HashMap<&CommitId, &GraphNode<CommitId>> =
            reversed.iter().map(|node| (&node.0, node)).collect();

        // Find head commits (roots of the reversed/parent→child graph)
        let all_edge_targets: HashSet<&CommitId> = reversed
            .iter()
            .flat_map(|(_, edges)| edges.iter().map(|e| &e.target))
            .collect();

        let head_commits: Vec<&CommitId> = reversed
            .iter()
            .map(|(id, _)| id)
            .filter(|id| !all_edge_targets.contains(id))
            .collect();

        let mut registered: HashMap<Bookmark, Vec<GraphEdge<String>>> = HashMap::new();
        let mut visited: HashSet<&CommitId> = HashSet::new();

        let mut stack: Vec<(&CommitId, Option<&Bookmark>)> =
            head_commits.into_iter().map(|c| (c, None)).collect();

        while let Some((commit_id, parent_bookmark)) = stack.pop() {
            if !visited.insert(commit_id) {
                continue;
            }

            let maybe_bookmark = bookmarks_per_commit.get(commit_id);

            if let Some(bookmark) = maybe_bookmark {
                registered.entry(bookmark.clone()).or_default();

                if let Some(pb) = parent_bookmark
                    && pb != bookmark
                    && let Some(edges) = registered.get_mut(bookmark)
                {
                    edges.push(GraphEdge::direct(pb.name().to_string()));
                }
            }

            let next_bookmark = maybe_bookmark.or(parent_bookmark);

            if let Some(node) = commit_index.get(commit_id) {
                for edge in &node.1 {
                    stack.push((&edge.target, next_bookmark));
                }
            }
        }

        registered
    }

    fn find_head_bookmarks(nodes: &HashMap<Bookmark, Vec<GraphEdge<String>>>) -> HashSet<Bookmark> {
        let all_edge_targets: HashSet<&str> = nodes
            .values()
            .flatten()
            .map(|e| e.target.as_str())
            .collect();

        nodes
            .keys()
            .filter(|b| !all_edge_targets.contains(b.name()))
            .cloned()
            .collect()
    }
}
