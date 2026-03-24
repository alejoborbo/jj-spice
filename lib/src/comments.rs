use std::collections::BTreeMap;

use const_format::formatcp;
use thiserror::Error;

use crate::bookmark::Bookmark;
use crate::bookmark::graph::BookmarkGraph;
use crate::forge::ChangeStatus;
use crate::protos::change_request::forge_meta::Forge;
use crate::protos::change_request::{ChangeRequests, ForgeMeta};

const JJ_SPICE_URL: &str = "https://github.com/alejoborbo/jj-spice";
const MANAGED_BY_HTML: &str = formatcp!(
    "\n<sub>Change managed by <a href=\"{}\">jj-spice</a>.</sub>",
    JJ_SPICE_URL
);

/// Node symbol used for every bookmark in the graph.
const NODE_SYMBOL: &str = "○";

/// Node symbol used for the trunk (immutable) bookmark.
const TRUNK_SYMBOL: &str = "◆";

/// Live data for a single change request, used to enrich graph comments.
#[derive(Debug, Clone)]
pub struct LiveCrData {
    pub status: ChangeStatus,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Error)]
pub enum CommentError {
    #[error("No change request found for bookmark {0}")]
    NoChangeRequestFound(String),
    #[error("No forge metadata found for bookmark {0}")]
    NoForgeMetadataFound(String),
    #[error("No base branch found)")]
    NoBaseBranchFound,
    #[error("No target branch found)")]
    NoTargetBranchFound,
}

/// Renders a stack-trace comment for a change request.
///
/// The comment is an ASCII-art graph inside `<pre>` with `<a>` hyperlinks,
/// visually matching the `jj-spice stack log` output with emoji status
/// badges and clickable CR links.
///
/// Graph rendering is forge-agnostic: forge-specific data (CR labels and
/// fallback URLs) is extracted once into [`CommentNode`] so the rendering
/// logic is shared across GitHub PRs and GitLab MRs.
pub struct Comment<'a> {
    current_bookmark: &'a Bookmark<'a>,
    graph: &'a BookmarkGraph<'a>,
    change_requests: &'a ChangeRequests,
    trunk_name: Option<&'a str>,
    live_data: Option<&'a BTreeMap<String, LiveCrData>>,
}

impl<'a> Comment<'a> {
    pub fn new(
        current_bookmark: &'a Bookmark<'a>,
        graph: &'a BookmarkGraph<'a>,
        change_requests: &'a ChangeRequests,
    ) -> Comment<'a> {
        Comment {
            current_bookmark,
            graph,
            change_requests,
            trunk_name: None,
            live_data: None,
        }
    }

    /// Set the trunk bookmark name (shown at the bottom of the graph).
    pub fn with_trunk(mut self, trunk_name: &'a str) -> Self {
        self.trunk_name = Some(trunk_name);
        self
    }

    /// Provide live change request data for richer graph output.
    pub fn with_live_data(mut self, data: &'a BTreeMap<String, LiveCrData>) -> Self {
        self.live_data = Some(data);
        self
    }

    /// Render the comment as an ASCII-art graph inside `<pre>` with `<a>` hyperlinks.
    ///
    /// Produces output in the same visual style as `jj-spice stack log`:
    /// vertical graph with Unicode box-drawing characters, bookmark nodes
    /// rendered top-to-bottom (leaf first, root last, trunk at bottom).
    pub fn to_string(&self) -> Result<String, CommentError> {
        let ascendant_to_crs = self.build_ascendant_map()?;
        let ordered = self.collect_ordered_nodes(&ascendant_to_crs)?;

        let mut output = String::from("This change belongs to the following stack:\n<pre>\n");

        for (i, node) in ordered.iter().enumerate() {
            let is_current = node.source_branch == self.current_bookmark.name();
            let here_marker = if is_current { "  👈" } else { "" };

            let link = self.format_cr_link(node);
            let status = self.format_status_emoji(node);

            output.push_str(&format!(
                "{}  {}{}{}\n",
                NODE_SYMBOL, link, status, here_marker,
            ));

            // Title line (when live data is available).
            if let Some(title) = self.node_title(node)
                && !title.is_empty()
            {
                output.push_str(&format!("│  {}\n", html_escape(title)));
            }

            // Connector to the next node.
            if i < ordered.len() - 1 {
                output.push_str("│\n");
            }
        }

        // Trunk node at the bottom.
        if let Some(trunk) = self.trunk_name {
            if !ordered.is_empty() {
                output.push_str("│\n");
            }
            output.push_str(&format!("{}  {}\n", TRUNK_SYMBOL, html_escape(trunk)));
        }

        output.push_str("</pre>\n");
        output.push_str(MANAGED_BY_HTML);
        Ok(output)
    }

    /// Collect nodes in reversed topological order (leaf-first, root-last).
    fn collect_ordered_nodes(
        &self,
        ascendant_to_crs: &BTreeMap<String, Vec<&ForgeMeta>>,
    ) -> Result<Vec<CommentNode>, CommentError> {
        let mut nodes = Vec::new();
        let mut visited = Vec::new();

        let mut stack: Vec<&ForgeMeta> = self
            .graph
            .root_bookmarks
            .iter()
            .map(|b| {
                self.change_requests
                    .get(b)
                    .ok_or_else(|| CommentError::NoChangeRequestFound(b.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?;

        while let Some(meta) = stack.pop() {
            if visited.contains(&meta) {
                continue;
            }
            visited.push(meta);

            let source_branch = meta
                .source_branch()
                .ok_or(CommentError::NoTargetBranchFound)?;

            let (cr_label, cr_url) = Self::extract_cr_info(meta);

            nodes.push(CommentNode {
                source_branch: source_branch.to_string(),
                cr_label,
                cr_url,
            });

            for next in ascendant_to_crs.get(source_branch).unwrap_or(&vec![]) {
                stack.push(next);
            }
        }

        // Reverse: DFS from roots produces root-first, we want leaf-first.
        nodes.reverse();
        Ok(nodes)
    }

    /// Build a map from ascendant bookmark name to its descendant ForgeMeta entries.
    fn build_ascendant_map(&self) -> Result<BTreeMap<String, Vec<&ForgeMeta>>, CommentError> {
        let mut map: BTreeMap<String, Vec<&ForgeMeta>> = BTreeMap::new();
        for meta in self.change_requests.by_bookmark.values() {
            let target_branch = meta
                .target_branch()
                .ok_or(CommentError::NoBaseBranchFound)?;

            if self.change_requests.get(target_branch).is_some() {
                map.entry(target_branch.to_string()).or_default().push(meta);
            }
        }
        Ok(map)
    }

    /// Format a CR reference as a clickable `<a>` tag.
    ///
    /// When live data is available, uses the authoritative URL from the forge
    /// API. Otherwise falls back to the URL built from stored metadata (only
    /// available for forges that carry enough info, e.g. GitHub's `target_repo`).
    fn format_cr_link(&self, node: &CommentNode) -> String {
        let bookmark = html_escape(&node.source_branch);

        if let Some(live) = self.live_data.and_then(|d| d.get(&node.source_branch)) {
            let label = match &node.cr_label {
                Some(l) => format!("{} {}", bookmark, l),
                None => bookmark.to_string(),
            };
            format!("<a href=\"{}\">{}</a>", html_escape(&live.url), label)
        } else if let (Some(label), Some(url)) = (&node.cr_label, &node.cr_url) {
            format!(
                "<a href=\"{}\">{} {}</a>",
                html_escape(url),
                bookmark,
                label,
            )
        } else if let Some(label) = &node.cr_label {
            format!("{} {}", bookmark, label)
        } else {
            bookmark.to_string()
        }
    }

    /// Format an emoji status badge for the node.
    fn format_status_emoji(&self, node: &CommentNode) -> String {
        if let Some(live) = self.live_data.and_then(|d| d.get(&node.source_branch)) {
            let emoji = match live.status {
                ChangeStatus::Open => "🟢 Open",
                ChangeStatus::Draft => "🟡 Draft",
                ChangeStatus::Merged => "🟣 Merged",
                ChangeStatus::Closed => "🔴 Closed",
            };
            format!(" {}", emoji)
        } else {
            String::new()
        }
    }

    /// Get the title for a node from live data, if available.
    fn node_title<'b>(&'b self, node: &CommentNode) -> Option<&'b str> {
        self.live_data
            .and_then(|d| d.get(&node.source_branch))
            .map(|live| live.title.as_str())
    }

    /// Extract forge-agnostic CR display label and fallback URL from metadata.
    ///
    /// Returns `(cr_label, cr_url)` where `cr_label` is the human-readable
    /// identifier (e.g. `"#42"` for GitHub, `"!42"` for GitLab) and `cr_url`
    /// is a best-effort direct URL when enough info is stored in the proto.
    fn extract_cr_info(meta: &ForgeMeta) -> (Option<String>, Option<String>) {
        match &meta.forge {
            Some(Forge::Github(gh)) => {
                let label = format!("#{}", gh.number);
                let url = if !gh.target_repo.is_empty() {
                    Some(format!(
                        "https://github.com/{}/pull/{}",
                        gh.target_repo, gh.number,
                    ))
                } else {
                    None
                };
                (Some(label), url)
            }
            Some(Forge::Gitlab(gl)) => {
                let label = format!("!{}", gl.iid);
                // GitLab proto stores project IDs rather than path strings,
                // so we cannot construct a fallback URL from metadata alone.
                (Some(label), None)
            }
            None => (None, None),
        }
    }
}

/// Forge-agnostic intermediate representation of a graph node.
struct CommentNode {
    source_branch: String,
    /// Human-readable CR identifier (e.g. `"#42"` for GitHub, `"!42"` for GitLab).
    cr_label: Option<String>,
    /// Best-effort direct URL to the CR, built from stored metadata.
    cr_url: Option<String>,
}

/// Escape HTML special characters.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use jj_lib::op_store::{LocalRemoteRefTarget, RefTarget};

    use super::*;
    use crate::bookmark::Bookmark;
    use crate::bookmark::graph::BookmarkGraph;
    use crate::protos::change_request::forge_meta::Forge as ForgeOneof;
    use crate::protos::change_request::{ChangeRequests, GitHubMeta, GitLabMeta};

    fn make_bookmark(name: &str) -> Bookmark<'static> {
        Bookmark::new(
            name.to_string(),
            LocalRemoteRefTarget {
                local_target: RefTarget::absent_ref(),
                remote_refs: vec![],
            },
        )
    }

    fn github_meta(number: u64, source_branch: &str, target_branch: &str) -> ForgeMeta {
        ForgeMeta {
            forge: Some(ForgeOneof::Github(GitHubMeta {
                number,
                source_branch: source_branch.into(),
                target_branch: target_branch.into(),
                source_repo: "owner/repo".into(),
                target_repo: "owner/repo".into(),
                graphql_id: String::new(),
                comment_id: None,
            })),
        }
    }

    fn gitlab_meta(iid: u64, source_branch: &str, target_branch: &str) -> ForgeMeta {
        ForgeMeta {
            forge: Some(ForgeOneof::Gitlab(GitLabMeta {
                id: iid * 100,
                iid,
                source_branch: source_branch.into(),
                target_branch: target_branch.into(),
                source_project_id: None,
                comment_id: None,
            })),
        }
    }

    fn make_change_requests(entries: Vec<(&str, u64, &str)>) -> ChangeRequests {
        let mut crs = ChangeRequests::default();
        for (name, number, target) in entries {
            crs.set(name.to_string(), github_meta(number, name, target));
        }
        crs
    }

    fn make_gitlab_change_requests(entries: Vec<(&str, u64, &str)>) -> ChangeRequests {
        let mut crs = ChangeRequests::default();
        for (name, iid, target) in entries {
            crs.set(name.to_string(), gitlab_meta(iid, name, target));
        }
        crs
    }

    fn make_live_data(
        entries: Vec<(&str, ChangeStatus, &str, &str)>,
    ) -> BTreeMap<String, LiveCrData> {
        entries
            .into_iter()
            .map(|(name, status, title, url)| {
                (
                    name.to_string(),
                    LiveCrData {
                        status,
                        title: title.to_string(),
                        url: url.to_string(),
                    },
                )
            })
            .collect()
    }

    // -- Structure tests (GitHub) --

    #[test]
    fn single_bookmark_graph() {
        let bookmark = make_bookmark("feat-a");
        let graph = BookmarkGraph::for_testing(vec!["feat-a".into()], BTreeMap::new());
        let crs = make_change_requests(vec![("feat-a", 1, "main")]);

        let comment = Comment::new(&bookmark, &graph, &crs).with_trunk("main");
        let output = comment.to_string().unwrap();

        assert!(output.contains("<pre>"));
        assert!(output.contains("</pre>"));
        assert!(output.contains("○"));
        assert!(output.contains("◆  main"));
        assert!(output.contains("feat-a #1"));
        assert!(output.contains("👈"));
        assert!(output.contains("<a href="));
        assert!(output.contains("jj-spice</a>"));
    }

    #[test]
    fn linear_stack_ordering() {
        let current = make_bookmark("mid");
        let graph = BookmarkGraph::for_testing(vec!["root".into()], BTreeMap::new());
        let crs = make_change_requests(vec![
            ("root", 10, "main"),
            ("mid", 11, "root"),
            ("leaf", 12, "mid"),
        ]);

        let comment = Comment::new(&current, &graph, &crs).with_trunk("main");
        let output = comment.to_string().unwrap();

        // Leaf at top, root at bottom before trunk.
        let leaf_pos = output.find("leaf #12").unwrap();
        let mid_pos = output.find("mid #11").unwrap();
        let root_pos = output.find("root #10").unwrap();
        let trunk_pos = output.find("◆  main").unwrap();

        assert!(leaf_pos < mid_pos, "leaf should be above mid");
        assert!(mid_pos < root_pos, "mid should be above root");
        assert!(root_pos < trunk_pos, "root should be above trunk");

        // Current bookmark marked.
        assert!(output.contains("mid #11</a>  👈"));
    }

    #[test]
    fn current_bookmark_is_root() {
        let current = make_bookmark("root");
        let graph = BookmarkGraph::for_testing(vec!["root".into()], BTreeMap::new());
        let crs = make_change_requests(vec![("root", 1, "main"), ("child", 2, "root")]);

        let comment = Comment::new(&current, &graph, &crs).with_trunk("main");
        let output = comment.to_string().unwrap();

        assert!(output.contains("root #1</a>  👈"));
        // Child above root.
        let child_pos = output.find("child #2").unwrap();
        let root_pos = output.find("root #1").unwrap();
        assert!(child_pos < root_pos);
    }

    // -- Live data tests --

    #[test]
    fn live_data_shows_status_and_title() {
        let current = make_bookmark("feat-a");
        let graph = BookmarkGraph::for_testing(vec!["feat-a".into()], BTreeMap::new());
        let crs = make_change_requests(vec![("feat-a", 42, "main")]);
        let live = make_live_data(vec![(
            "feat-a",
            ChangeStatus::Open,
            "Add cool feature",
            "https://github.com/owner/repo/pull/42",
        )]);

        let comment = Comment::new(&current, &graph, &crs)
            .with_trunk("main")
            .with_live_data(&live);
        let output = comment.to_string().unwrap();

        assert!(output.contains("🟢 Open"));
        assert!(output.contains("Add cool feature"));
        assert!(output.contains("https://github.com/owner/repo/pull/42"));
    }

    #[test]
    fn live_data_draft_and_merged() {
        let current = make_bookmark("feat-b");
        let graph = BookmarkGraph::for_testing(vec!["feat-a".into()], BTreeMap::new());
        let crs = make_change_requests(vec![("feat-a", 10, "main"), ("feat-b", 11, "feat-a")]);
        let live = make_live_data(vec![
            (
                "feat-a",
                ChangeStatus::Merged,
                "Base feature",
                "https://github.com/owner/repo/pull/10",
            ),
            (
                "feat-b",
                ChangeStatus::Draft,
                "WIP feature",
                "https://github.com/owner/repo/pull/11",
            ),
        ]);

        let comment = Comment::new(&current, &graph, &crs).with_live_data(&live);
        let output = comment.to_string().unwrap();

        assert!(output.contains("🟣 Merged"));
        assert!(output.contains("🟡 Draft"));
    }

    #[test]
    fn live_data_closed() {
        let current = make_bookmark("feat");
        let graph = BookmarkGraph::for_testing(vec!["feat".into()], BTreeMap::new());
        let crs = make_change_requests(vec![("feat", 5, "main")]);
        let live = make_live_data(vec![(
            "feat",
            ChangeStatus::Closed,
            "Abandoned",
            "https://github.com/owner/repo/pull/5",
        )]);

        let comment = Comment::new(&current, &graph, &crs).with_live_data(&live);
        let output = comment.to_string().unwrap();

        assert!(output.contains("🔴 Closed"));
    }

    // -- Graph structure tests --

    #[test]
    fn without_trunk_omits_diamond() {
        let bookmark = make_bookmark("feat-a");
        let graph = BookmarkGraph::for_testing(vec!["feat-a".into()], BTreeMap::new());
        let crs = make_change_requests(vec![("feat-a", 1, "main")]);

        let comment = Comment::new(&bookmark, &graph, &crs);
        let output = comment.to_string().unwrap();

        assert!(output.contains("○"));
        assert!(!output.contains("◆"));
    }

    #[test]
    fn connector_lines_between_nodes() {
        let bookmark = make_bookmark("root");
        let graph = BookmarkGraph::for_testing(vec!["root".into()], BTreeMap::new());
        let crs = make_change_requests(vec![("root", 1, "main"), ("child", 2, "root")]);

        let comment = Comment::new(&bookmark, &graph, &crs).with_trunk("main");
        let output = comment.to_string().unwrap();

        assert!(output.contains("│\n"));
    }

    #[test]
    fn forking_stack_both_children_above_root() {
        let current = make_bookmark("root");
        let graph = BookmarkGraph::for_testing(vec!["root".into()], BTreeMap::new());
        let crs = make_change_requests(vec![
            ("root", 1, "main"),
            ("child-a", 2, "root"),
            ("child-b", 3, "root"),
        ]);

        let comment = Comment::new(&current, &graph, &crs).with_trunk("main");
        let output = comment.to_string().unwrap();

        assert!(output.contains("child-a #2"));
        assert!(output.contains("child-b #3"));
        assert!(output.contains("root #1"));

        let child_a_pos = output.find("child-a").unwrap();
        let child_b_pos = output.find("child-b").unwrap();
        let root_pos = output.find("root #1").unwrap();

        assert!(child_a_pos < root_pos, "child-a should be above root");
        assert!(child_b_pos < root_pos, "child-b should be above root");
    }

    // -- GitLab tests --

    #[test]
    fn single_gitlab_mr_graph() {
        let bookmark = make_bookmark("feat-a");
        let graph = BookmarkGraph::for_testing(vec!["feat-a".into()], BTreeMap::new());
        let crs = make_gitlab_change_requests(vec![("feat-a", 1, "main")]);

        let comment = Comment::new(&bookmark, &graph, &crs).with_trunk("main");
        let output = comment.to_string().unwrap();

        assert!(output.contains("feat-a !1"));
        assert!(output.contains("○"));
        assert!(output.contains("◆  main"));
        assert!(output.contains("👈"));
        // GitLab has no fallback URL from metadata alone, so no CR link
        // inside the graph (the footer still has the jj-spice link).
        let pre_block = output.split("</pre>").next().unwrap();
        assert!(!pre_block.contains("<a href="));
    }

    #[test]
    fn linear_gitlab_stack_ordering() {
        let current = make_bookmark("mid");
        let graph = BookmarkGraph::for_testing(vec!["root".into()], BTreeMap::new());
        let crs = make_gitlab_change_requests(vec![
            ("root", 10, "main"),
            ("mid", 11, "root"),
            ("leaf", 12, "mid"),
        ]);

        let comment = Comment::new(&current, &graph, &crs).with_trunk("main");
        let output = comment.to_string().unwrap();

        let leaf_pos = output.find("leaf !12").unwrap();
        let mid_pos = output.find("mid !11").unwrap();
        let root_pos = output.find("root !10").unwrap();
        let trunk_pos = output.find("◆  main").unwrap();

        assert!(leaf_pos < mid_pos, "leaf should be above mid");
        assert!(mid_pos < root_pos, "mid should be above root");
        assert!(root_pos < trunk_pos, "root should be above trunk");

        // Current bookmark marked.
        assert!(output.contains("mid !11  👈"));
    }

    #[test]
    fn gitlab_live_data_produces_clickable_links() {
        let current = make_bookmark("feat-a");
        let graph = BookmarkGraph::for_testing(vec!["feat-a".into()], BTreeMap::new());
        let crs = make_gitlab_change_requests(vec![("feat-a", 42, "main")]);
        let live = make_live_data(vec![(
            "feat-a",
            ChangeStatus::Open,
            "Add cool feature",
            "https://gitlab.com/g/p/-/merge_requests/42",
        )]);

        let comment = Comment::new(&current, &graph, &crs)
            .with_trunk("main")
            .with_live_data(&live);
        let output = comment.to_string().unwrap();

        assert!(output.contains("<a href=\"https://gitlab.com/g/p/-/merge_requests/42\">"));
        assert!(output.contains("feat-a !42"));
        assert!(output.contains("🟢 Open"));
        assert!(output.contains("Add cool feature"));
    }

    // -- Security tests --

    #[test]
    fn html_escapes_special_chars() {
        let bookmark = make_bookmark("feat<xss>");
        let graph = BookmarkGraph::for_testing(vec!["feat<xss>".into()], BTreeMap::new());

        let mut crs = ChangeRequests::default();
        crs.set(
            "feat<xss>".to_string(),
            ForgeMeta {
                forge: Some(ForgeOneof::Github(GitHubMeta {
                    number: 1,
                    source_branch: "feat<xss>".into(),
                    target_branch: "main".into(),
                    source_repo: "owner/repo".into(),
                    target_repo: "owner/repo".into(),
                    graphql_id: String::new(),
                    comment_id: None,
                })),
            },
        );

        let comment = Comment::new(&bookmark, &graph, &crs);
        let output = comment.to_string().unwrap();

        assert!(output.contains("feat&lt;xss&gt;"));
        assert!(!output.contains("feat<xss>"));
    }

    // -- Error tests --

    #[test]
    fn missing_change_request_for_root_returns_error() {
        let bookmark = make_bookmark("feat-a");
        let graph = BookmarkGraph::for_testing(vec!["feat-a".into()], BTreeMap::new());
        let crs = ChangeRequests::default();

        let comment = Comment::new(&bookmark, &graph, &crs);
        let err = comment.to_string().unwrap_err();

        assert!(matches!(err, CommentError::NoChangeRequestFound(ref name) if name == "feat-a"));
    }

    #[test]
    fn missing_forge_variant_returns_no_base_branch() {
        let bookmark = make_bookmark("feat-a");
        let graph = BookmarkGraph::for_testing(vec!["feat-a".into()], BTreeMap::new());

        let mut crs = ChangeRequests::default();
        crs.set("feat-a".to_string(), ForgeMeta { forge: None });

        let comment = Comment::new(&bookmark, &graph, &crs);
        let err = comment.to_string().unwrap_err();

        assert!(matches!(err, CommentError::NoBaseBranchFound));
    }

    // -- Footer test --

    #[test]
    fn includes_jj_spice_footer() {
        let bookmark = make_bookmark("feat");
        let graph = BookmarkGraph::for_testing(vec!["feat".into()], BTreeMap::new());
        let crs = make_change_requests(vec![("feat", 42, "main")]);

        let comment = Comment::new(&bookmark, &graph, &crs);
        let output = comment.to_string().unwrap();

        assert!(output.contains("<pre>"));
        assert!(output.ends_with(
            "Change managed by <a href=\"https://github.com/alejoborbo/jj-spice\">jj-spice</a>.</sub>"
        ));
    }
}
