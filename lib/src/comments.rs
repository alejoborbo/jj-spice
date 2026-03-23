use std::collections::BTreeMap;

use const_format::formatcp;
use thiserror::Error;

use crate::bookmark::Bookmark;
use crate::bookmark::graph::BookmarkGraph;
use crate::protos::change_request::forge_meta::Forge;
use crate::protos::change_request::{ChangeRequests, ForgeMeta};

const INDENT: &str = "  ";
const JJ_SPICE_URL: &str = "https://github.com/alejoborbo/jj-spice";
const HEADER_LINE: &str = "This change belongs to the following stack:\n";
const MANAGED_BY_COMMENT_LINE: &str = formatcp!(
    "\n<sub>Change managed by [jj-spice]({}).</sub>",
    JJ_SPICE_URL
);

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

/// Struct for creating change request comments.
/// Those comments would be added to the change request to vizualize the stack trace.
pub struct Comment<'a> {
    current_bookmark: &'a Bookmark<'a>,
    graph: &'a BookmarkGraph<'a>,
    change_requests: &'a ChangeRequests,
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
        }
    }

    pub fn to_string(&self) -> Result<String, CommentError> {
        let mut comment = String::from(HEADER_LINE);

        // Map from ascendant bookmark name (source_branch) to its descendant change requests.
        // Uses each CR's target_branch to find its parent CR.
        let ascendant_to_crs: BTreeMap<String, Vec<&ForgeMeta>> = {
            let mut map: BTreeMap<String, Vec<&ForgeMeta>> = BTreeMap::new();
            for meta in self.change_requests.by_bookmark.values() {
                let target_branch = meta
                    .target_branch()
                    .ok_or(CommentError::NoBaseBranchFound)?;

                if self.change_requests.get(target_branch).is_some() {
                    map.entry(target_branch.to_string()).or_default().push(meta);
                }
            }
            map
        };

        // A queue of bookmark, and the depth of the bookmark in the stack trace
        let mut queue: Vec<(&ForgeMeta, usize)> = self
            .graph
            .root_bookmarks
            .iter()
            .map(|b| {
                self.change_requests
                    .get(b)
                    .ok_or_else(|| CommentError::NoChangeRequestFound(b.clone()))
                    .map(|meta| (meta, 0))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut visited = Vec::new();

        // Go through the bookmarks in a depth first order, and add comments to the change request
        while let Some((meta, depth)) = queue.pop() {
            if visited.contains(&meta) {
                continue;
            }
            visited.push(meta);

            // Fetch source branch, which is the current bookmark
            let source_branch = meta
                .source_branch()
                .ok_or(CommentError::NoTargetBranchFound)?;

            // Add the comment to the change request
            let indent = INDENT.repeat(depth);
            let id = self
                .forge_meta_id(meta)
                .ok_or_else(|| CommentError::NoForgeMetadataFound(source_branch.to_string()))?;

            // Add the change request to the comment
            comment.push_str(format!("{}- {}", indent, id).as_str());
            if source_branch == self.current_bookmark.name() {
                comment.push_str(" 👈 you are here!");
            }
            comment.push('\n');

            // Add the next bookmarks of the bookmark to the queue
            for next in ascendant_to_crs.get(source_branch).unwrap_or(&vec![]) {
                queue.push((next, depth + 1));
            }
        }

        // Add a link to the repo URL
        comment.push_str(MANAGED_BY_COMMENT_LINE);

        Ok(comment)
    }

    fn forge_meta_id(&self, meta: &ForgeMeta) -> Option<String> {
        match &meta.forge {
            Some(Forge::Github(gh)) => Some(format!("#{}", gh.number)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use jj_lib::op_store::{LocalRemoteRefTarget, RefTarget};

    use super::*;
    use crate::bookmark::Bookmark;
    use crate::bookmark::graph::BookmarkGraph;
    use crate::protos::change_request::forge_meta::Forge as ForgeOneof;
    use crate::protos::change_request::{ChangeRequests, GitHubMeta};

    fn make_bookmark(name: &str) -> Bookmark<'static> {
        Bookmark::new(
            name.to_string(),
            LocalRemoteRefTarget {
                local_target: RefTarget::absent_ref(),
                remote_refs: vec![],
            },
        )
    }

    /// Build a ForgeMeta where `source_branch` = bookmark name, `target_branch` = parent bookmark.
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

    /// Entries: (bookmark_name, pr_number, target_branch).
    fn make_change_requests(entries: Vec<(&str, u64, &str)>) -> ChangeRequests {
        let mut crs = ChangeRequests::default();
        for (name, number, target) in entries {
            crs.set(name.to_string(), github_meta(number, name, target));
        }
        crs
    }

    // -- to_string tests --

    #[test]
    fn single_bookmark_stack() {
        let bookmark = make_bookmark("feat-a");
        let graph = BookmarkGraph::for_testing(vec!["feat-a".into()], BTreeMap::new());
        let crs = make_change_requests(vec![("feat-a", 1, "main")]);

        let comment = Comment::new(&bookmark, &graph, &crs);
        let output = comment.to_string().unwrap();

        assert!(output.contains("- #1 👈 you are here!"));
        assert!(output.contains("This change belongs to the following stack:"));
        assert!(output.contains("jj-spice"));
    }

    #[test]
    fn linear_stack_marks_current_bookmark() {
        // root -> mid -> leaf (target_branch chains: leaf→mid, mid→root, root→main)
        let current = make_bookmark("mid");
        let graph = BookmarkGraph::for_testing(vec!["root".into()], BTreeMap::new());
        let crs = make_change_requests(vec![
            ("root", 10, "main"),
            ("mid", 11, "root"),
            ("leaf", 12, "mid"),
        ]);

        let comment = Comment::new(&current, &graph, &crs);
        let output = comment.to_string().unwrap();

        // root at depth 0, mid at depth 1, leaf at depth 2
        assert!(output.contains("- #10\n"));
        assert!(output.contains("  - #11 👈 you are here!\n"));
        assert!(output.contains("    - #12\n"));
    }

    #[test]
    fn current_bookmark_is_root() {
        let current = make_bookmark("root");
        let graph = BookmarkGraph::for_testing(vec!["root".into()], BTreeMap::new());
        let crs = make_change_requests(vec![("root", 1, "main"), ("child", 2, "root")]);

        let comment = Comment::new(&current, &graph, &crs);
        let output = comment.to_string().unwrap();

        assert!(output.contains("- #1 👈 you are here!\n"));
        assert!(output.contains("  - #2\n"));
    }

    #[test]
    fn missing_change_request_for_root_returns_error() {
        // root_bookmarks references "feat-a" but it's not in change_requests
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

        // ForgeMeta with no forge variant → target_branch() returns None
        let mut crs = ChangeRequests::default();
        crs.set("feat-a".to_string(), ForgeMeta { forge: None });

        let comment = Comment::new(&bookmark, &graph, &crs);
        let err = comment.to_string().unwrap_err();

        assert!(matches!(err, CommentError::NoBaseBranchFound));
    }

    #[test]
    fn comment_includes_header_and_footer() {
        let bookmark = make_bookmark("feat");
        let graph = BookmarkGraph::for_testing(vec!["feat".into()], BTreeMap::new());
        let crs = make_change_requests(vec![("feat", 42, "main")]);

        let comment = Comment::new(&bookmark, &graph, &crs);
        let output = comment.to_string().unwrap();

        assert!(output.starts_with("This change belongs to the following stack:\n"));
        assert!(output.ends_with(
            "Change managed by [jj-spice](https://github.com/alejoborbo/jj-spice).</sub>"
        ));
    }

    #[test]
    fn forking_stack_shows_both_branches() {
        // root -> child-a, root -> child-b (both target "root")
        let current = make_bookmark("root");
        let graph = BookmarkGraph::for_testing(vec!["root".into()], BTreeMap::new());
        let crs = make_change_requests(vec![
            ("root", 1, "main"),
            ("child-a", 2, "root"),
            ("child-b", 3, "root"),
        ]);

        let comment = Comment::new(&current, &graph, &crs);
        let output = comment.to_string().unwrap();

        assert!(output.contains("- #1 👈 you are here!\n"));
        // Both children at depth 1
        assert!(output.contains("  - #2\n"));
        assert!(output.contains("  - #3\n"));
    }
}
