use std::collections::HashMap;
use std::io::Write;

use jj_cli::formatter::Formatter;
use jj_cli::graphlog::{GraphStyle, get_graphlog};
use jj_lib::backend::CommitId;
use jj_lib::config::{ConfigLayer, ConfigSource};
use jj_lib::graph::GraphEdge;
use jj_lib::repo::Repo;

use crate::commands::env::SpiceEnv;
use jj_spice_lib::bookmark::graph::{BookmarkGraph, BookmarkNode};
use jj_spice_lib::forge::detect::{DetectionResult, detect_forges};
use jj_spice_lib::forge::{ChangeRequest, ChangeStatus, Forge};
use jj_spice_lib::protos::change_request::ForgeMeta;
use jj_spice_lib::protos::change_request::forge_meta::Forge as ForgeOneof;
use jj_spice_lib::store::SpiceStore;
use jj_spice_lib::store::change_request::ChangeRequestStore;

/// Default color rules for `stack log` output.
///
/// Injected at [`ConfigSource::Default`] priority so users can override
/// any of these in their own jj config under `[colors]`.
const SPICE_COLOR_DEFAULTS: &str = r##"
[colors]
"spice bookmark" = "magenta"
"spice cr_id" = { fg = "yellow" }
"spice cr_title" = { fg = "default" }
"spice url" = { fg = "bright blue", underline = true }
"spice trunk" = "cyan"
"spice no_cr" = { fg = "bright black" }
"spice status_open" = { fg = "#ffffff", bg = "#238636", bold = false }
"spice status_open cap" = { fg = "#238636", bg = "default", bold = false }
"spice status_draft" = { fg = "#ffffff", bg = "#555555", bold = false }
"spice status_draft cap" = { fg = "#555555", bg = "default", bold = false }
"spice status_closed" = { fg = "#ffffff", bg = "#DA3743", bold = false }
"spice status_closed cap" = { fg = "#DA3743", bg = "default", bold = false }
"spice status_merged" = { fg = "#ffffff", bg = "#A371F7", bold = false }
"spice status_merged cap" = { fg = "#A371F7", bg = "default", bold = false }
"spice status_unknown" = { fg = "#ffffff", bg = "bright black", bold = false }
"spice status_unknown cap" = { fg = "bright black", bg = "default", bold = false }
"##;

/// Left cap of a pill-shaped status badge (Powerline rounded glyph).
const PILL_LEFT: &str = "\u{e0b6}";
/// Right cap of a pill-shaped status badge (Powerline rounded glyph).
const PILL_RIGHT: &str = "\u{e0b4}";

/// Node symbol used for every bookmark in the graph.
const NODE_SYMBOL: &str = "\u{25cb}";

/// Node symbol used for the trunk (immutable) bookmark.
const TRUNK_SYMBOL: &str = "\u{25c6}";

/// Show the bookmark DAG with change request status.
///
/// Builds a bookmark graph covering either all local bookmarks (default)
/// or the set of commits matching the given revset expression, loads any
/// tracked change request metadata, queries forges for live status, and
/// renders the result as a graphlog.
pub async fn run(
    env: &SpiceEnv,
    trunk: &CommitId,
    revisions: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let graph = if let Some(rev_str) = revisions {
        let resolved = env.resolve_revset(rev_str)?;
        BookmarkGraph::from_revset(env.repo.as_ref(), resolved)?
    } else {
        BookmarkGraph::all_local(env.repo.as_ref(), trunk)?
    };

    // Resolve the trunk bookmark name so we can show it at the bottom.
    let trunk_name = resolve_trunk_bookmark_name(env, trunk);

    // Load tracked change requests.
    let cr_state = load_change_requests(env);

    // Detect forges (read-only, no prompting for unmatched remotes).
    let forge_map = detect_forge_map(env);

    // Collect nodes and fetch live CR data.
    let nodes: Vec<&BookmarkNode> = graph.iter_graph()?.collect();
    let live_crs = fetch_live_crs(&nodes, &cr_state, &forge_map).await;

    // Render the graph.
    render_graph(
        env,
        &nodes,
        &graph,
        trunk_name.as_deref(),
        &cr_state,
        &live_crs,
    )?;

    Ok(())
}

/// Find the bookmark name pointing at the trunk commit.
///
/// Scans all bookmarks in the repo for one whose local target matches
/// `trunk_id`. Returns `None` if no bookmark is found (shouldn't happen
/// in practice since `trunk()` resolves from a bookmark).
fn resolve_trunk_bookmark_name(env: &SpiceEnv, trunk_id: &CommitId) -> Option<String> {
    env.repo.view().bookmarks().find_map(|(name, target)| {
        if target.local_target.as_normal() == Some(trunk_id) {
            Some(name.as_str().to_string())
        } else {
            None
        }
    })
}

/// Load the change request store, returning an empty state on failure.
fn load_change_requests(env: &SpiceEnv) -> jj_spice_lib::protos::change_request::ChangeRequests {
    SpiceStore::init_at(env.workspace.repo_path())
        .ok()
        .and_then(|store| ChangeRequestStore::new(&store).load().ok())
        .unwrap_or_default()
}

/// Build a forge map from auto-detected forges only (no interactive prompts).
fn detect_forge_map(env: &SpiceEnv) -> HashMap<String, Box<dyn Forge>> {
    detect_forges(env.repo.store(), env.config())
        .map(|DetectionResult { forges, .. }| forges)
        .unwrap_or_default()
}

/// Fetch live change request data for each bookmark that has stored metadata.
///
/// Returns a map from bookmark name to the live CR result. Forge API errors
/// are captured per-bookmark rather than failing the entire command.
async fn fetch_live_crs(
    nodes: &[&BookmarkNode<'_>],
    cr_state: &jj_spice_lib::protos::change_request::ChangeRequests,
    forge_map: &HashMap<String, Box<dyn Forge>>,
) -> HashMap<String, Result<Box<dyn ChangeRequest>, String>> {
    let mut results = HashMap::new();

    // Group bookmarks by forge identity so we can batch per forge.
    type ForgeGroup<'a> = (
        &'a dyn Forge,
        Vec<(&'a str, &'a jj_spice_lib::protos::change_request::ForgeMeta)>,
    );
    let mut forge_groups: HashMap<*const dyn Forge, ForgeGroup<'_>> = HashMap::new();

    for node in nodes {
        let name = node.name();
        let meta = match cr_state.get(name) {
            Some(m) => m,
            None => continue,
        };

        let forge = match find_forge_for_bookmark(node, forge_map) {
            Some(f) => f,
            None => {
                results.insert(name.to_string(), Err("no forge detected".to_string()));
                continue;
            }
        };

        let key = forge as *const dyn Forge;
        forge_groups
            .entry(key)
            .or_insert_with(|| (forge, Vec::new()))
            .1
            .push((name, meta));
    }

    // Batch-fetch per forge.
    for (_, (forge, items)) in forge_groups {
        let metas: Vec<&jj_spice_lib::protos::change_request::ForgeMeta> =
            items.iter().map(|(_, m)| *m).collect();
        let batch_results = forge.get_batch(metas).await;

        for ((name, _), result) in items.into_iter().zip(batch_results) {
            match result {
                Ok(cr) => {
                    results.insert(name.to_string(), Ok(cr));
                }
                Err(e) => {
                    results.insert(name.to_string(), Err(e.to_string()));
                }
            }
        }
    }

    results
}

/// Find a forge instance for a bookmark by checking its tracked remotes.
fn find_forge_for_bookmark<'a>(
    node: &BookmarkNode,
    forge_map: &'a HashMap<String, Box<dyn Forge>>,
) -> Option<&'a dyn Forge> {
    node.bookmark()
        .tracked_remotes()
        .find_map(|remote| forge_map.get(remote).map(|f| f.as_ref()))
}

/// Render the bookmark graph to stdout using `jj_cli::graphlog`.
///
/// Nodes are rendered in reverse topological order (children first, roots
/// last) so the graphlog reads top-to-bottom with leaf bookmarks at the top.
/// When a trunk bookmark name is provided, it is appended as a terminal
/// node at the very bottom with the `◆` symbol.
///
/// The [`BookmarkGraph`] stores edges as child → parent (e.g.
/// `feature-d → [feature-b, feature-c]`). This is the same direction
/// graphlog expects: each node's edges point to nodes that will be rendered
/// **later** (below) in the output.
fn render_graph(
    env: &SpiceEnv,
    nodes: &[&BookmarkNode<'_>],
    graph: &BookmarkGraph,
    trunk_name: Option<&str>,
    cr_state: &jj_spice_lib::protos::change_request::ChangeRequests,
    live_crs: &HashMap<String, Result<Box<dyn ChangeRequest>, String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Build a formatter factory that includes spice color defaults.
    // Respects --color=never by falling back to plain text.
    let factory = if env.ui.color() {
        let config = inject_color_defaults(env)?;
        jj_cli::formatter::FormatterFactory::color(&config, false)?
    } else {
        jj_cli::formatter::FormatterFactory::plain_text()
    };

    let graph_style = GraphStyle::from_settings(&env.settings).unwrap_or(GraphStyle::Curved);

    let mut stdout_buf: Vec<u8> = Vec::new();
    {
        let mut graphlog = get_graphlog::<String>(graph_style, &mut stdout_buf);

        // Reverse: iter_graph() yields roots-first, graphlog wants children-first.
        let reversed: Vec<&&BookmarkNode> = nodes.iter().rev().collect();

        for node in &reversed {
            let name = node.name();
            // Edges already point child → parent, matching graphlog's
            // expectation of pointing to nodes rendered later (below).
            let mut edges = graph.edges_for(name);

            // Root bookmarks (no edges to parents) get an edge to trunk.
            if edges.is_empty()
                && let Some(trunk) = trunk_name
            {
                edges.push(GraphEdge::direct(trunk.to_string()));
            }

            // Render the node text (2 lines) into a buffer with color labels.
            let mut text_buf: Vec<u8> = Vec::new();
            {
                let mut fmt = factory.new_formatter(&mut text_buf);
                render_node_text(&mut *fmt, node, cr_state, live_crs)?;
            }
            let text = String::from_utf8_lossy(&text_buf);

            graphlog.add_node(&name.to_string(), &edges, NODE_SYMBOL, &text)?;
        }

        // Render trunk as the terminal node at the bottom.
        if let Some(trunk) = trunk_name {
            let mut text_buf: Vec<u8> = Vec::new();
            {
                let mut fmt = factory.new_formatter(&mut text_buf);
                fmt.push_label("spice");
                fmt.push_label("trunk");
                write!(fmt, "{trunk}")?;
                fmt.pop_label();
                fmt.pop_label();
                writeln!(fmt)?;
            }
            let text = String::from_utf8_lossy(&text_buf);
            graphlog.add_node(&trunk.to_string(), &[], TRUNK_SYMBOL, &text)?;
        }
    }

    // Write the fully rendered graph to actual stdout.
    std::io::stdout().write_all(&stdout_buf)?;
    std::io::stdout().flush()?;
    Ok(())
}

/// Build a config stack with spice color defaults injected.
///
/// The defaults are added at [`ConfigSource::Default`] priority so any
/// user-defined `[colors]` rules take precedence.
fn inject_color_defaults(
    env: &SpiceEnv,
) -> Result<jj_lib::config::StackedConfig, Box<dyn std::error::Error>> {
    let mut config = env.config().clone();
    let layer = ConfigLayer::parse(ConfigSource::Default, SPICE_COLOR_DEFAULTS)?;
    config.add_layer(layer);
    Ok(config)
}

/// Render the node text for a single bookmark (always 2 lines).
///
/// Line 1: bookmark name + status pill + link (or placeholder)
/// Line 2: CR title, or empty when no title is available
fn render_node_text(
    fmt: &mut dyn Formatter,
    node: &BookmarkNode,
    cr_state: &jj_spice_lib::protos::change_request::ChangeRequests,
    live_crs: &HashMap<String, Result<Box<dyn ChangeRequest>, String>>,
) -> std::io::Result<()> {
    let name = node.name();
    let meta = cr_state.get(name);
    let live = live_crs.get(name);

    // Line 1: bookmark name + CR status/link.
    fmt.push_label("spice");
    fmt.push_label("bookmark");
    write!(fmt, "{name}")?;
    fmt.pop_label();

    match (meta, live) {
        // Live CR data available.
        (Some(_), Some(Ok(cr))) => {
            write!(fmt, " ")?;
            render_status_pill(fmt, Some(cr.status()))?;
            write!(fmt, " ")?;
            write_hyperlink(fmt, cr.url(), &cr.link_label())?;
        }
        // Stored metadata but forge API failed — show stored ID + unknown pill.
        (Some(meta), Some(Err(_))) => {
            let id = format_meta_id(meta);
            write!(fmt, " ")?;
            render_status_pill(fmt, None)?;
            write!(fmt, " ")?;
            fmt.push_label("cr_id");
            write!(fmt, "#{id}")?;
            fmt.pop_label();
        }
        // Stored metadata but no forge detected — show stored ID + unknown pill.
        (Some(meta), None) => {
            let id = format_meta_id(meta);
            write!(fmt, " ")?;
            render_status_pill(fmt, None)?;
            write!(fmt, " ")?;
            fmt.push_label("cr_id");
            write!(fmt, "#{id}")?;
            fmt.pop_label();
        }
        // No CR tracked at all.
        (None, _) => {
            write!(fmt, " ")?;
            fmt.push_label("no_cr");
            write!(fmt, "(no change request)")?;
            fmt.pop_label();
        }
    }

    fmt.pop_label();
    writeln!(fmt)?;

    // Line 2: CR title or empty line for consistent spacing.
    if let Some(Ok(cr)) = live {
        let title = cr.title();
        if !title.is_empty() {
            fmt.push_label("spice");
            fmt.push_label("cr_title");
            write!(fmt, "{title}")?;
            fmt.pop_label();
            fmt.pop_label();
        }
    }
    writeln!(fmt)?;

    Ok(())
}

/// Write a clickable OSC 8 hyperlink to the formatter.
///
/// In color mode, emits `ESC]8;;URL ST <visible_text> ESC]8;; ST` which
/// modern terminals render as a clickable link. The visible text is styled
/// with the `"url"` label.
///
/// In plain-text mode (or when the formatter doesn't support color), writes
/// the visible text followed by the URL in parentheses as a fallback.
fn write_hyperlink(fmt: &mut dyn Formatter, url: &str, text: &str) -> std::io::Result<()> {
    if fmt.maybe_color() {
        // OSC 8 opener: ESC ] 8 ; ; URL ST
        // ST (String Terminator) = ESC backslash
        {
            let mut raw = fmt.raw()?;
            write!(raw, "\x1b]8;;{url}\x1b\\")?;
        }
        // Visible text with label styling.
        fmt.push_label("url");
        write!(fmt, "{text}")?;
        fmt.pop_label();
        // OSC 8 closer: ESC ] 8 ; ; ST
        {
            let mut raw = fmt.raw()?;
            write!(raw, "\x1b]8;;\x1b\\")?;
        }
    } else {
        // Plain-text fallback: visible text (URL)
        write!(fmt, "{text} ({url})")?;
    }
    Ok(())
}

/// Render a pill-shaped status badge using Powerline rounded glyphs.
///
/// The pill has three parts with distinct labels:
/// - Left cap: fg = pill color, default bg → `"status_<x> cap"` label
/// - Inner text: fg = white, bg = pill color, bold → `"status_<x>"` label
/// - Right cap: same as left cap
fn render_status_pill(
    fmt: &mut dyn Formatter,
    status: Option<ChangeStatus>,
) -> std::io::Result<()> {
    let (label, text) = match status {
        Some(ChangeStatus::Open) => ("status_open", "Open"),
        Some(ChangeStatus::Closed) => ("status_closed", "Closed"),
        Some(ChangeStatus::Merged) => ("status_merged", "Merged"),
        Some(ChangeStatus::Draft) => ("status_draft", "Draft"),
        None => ("status_unknown", "?"),
    };

    // Left cap: colored glyph on default background.
    fmt.push_label(label);
    fmt.push_label("cap");
    write!(fmt, "{PILL_LEFT}")?;
    fmt.pop_label();

    // Inner text: white on colored background.
    write!(fmt, " {text} ")?;

    // Right cap: colored glyph on default background.
    fmt.push_label("cap");
    write!(fmt, "{PILL_RIGHT}")?;
    fmt.pop_label();

    fmt.pop_label();
    Ok(())
}

/// Extract a display ID from stored [`ForgeMeta`].
fn format_meta_id(meta: &ForgeMeta) -> String {
    match &meta.forge {
        Some(ForgeOneof::Github(gh)) => gh.number.to_string(),
        None => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jj_lib::op_store::{LocalRemoteRefTarget, RefTarget};
    use jj_spice_lib::forge::github::GitHubChangeRequest;
    use jj_spice_lib::protos::change_request::{ChangeRequests, GitHubMeta};

    fn make_node(name: &str) -> BookmarkNode<'static> {
        BookmarkNode::new(jj_spice_lib::bookmark::Bookmark::new(
            name.into(),
            LocalRemoteRefTarget {
                local_target: RefTarget::absent_ref(),
                remote_refs: vec![],
            },
        ))
    }

    // -- format_meta_id tests --

    #[test]
    fn format_meta_id_github() {
        let meta = ForgeMeta {
            forge: Some(ForgeOneof::Github(GitHubMeta {
                number: 42,
                source_branch: String::new(),
                target_branch: String::new(),
                source_repo: String::new(),
                target_repo: String::new(),
                graphql_id: String::new(),
                comment_id: None,
            })),
        };
        assert_eq!(format_meta_id(&meta), "42");
    }

    #[test]
    fn format_meta_id_none() {
        let meta = ForgeMeta { forge: None };
        assert_eq!(format_meta_id(&meta), "?");
    }

    // -- render_status_pill tests (plain text, no color) --

    fn render_pill_plain(status: Option<ChangeStatus>) -> String {
        let factory = jj_cli::formatter::FormatterFactory::plain_text();
        let mut buf = Vec::new();
        {
            let mut fmt = factory.new_formatter(&mut buf);
            render_status_pill(&mut *fmt, status).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn pill_open_plain_text() {
        let text = render_pill_plain(Some(ChangeStatus::Open));
        assert_eq!(text, format!("{PILL_LEFT} Open {PILL_RIGHT}"));
    }

    #[test]
    fn pill_draft_plain_text() {
        let text = render_pill_plain(Some(ChangeStatus::Draft));
        assert_eq!(text, format!("{PILL_LEFT} Draft {PILL_RIGHT}"));
    }

    #[test]
    fn pill_closed_plain_text() {
        let text = render_pill_plain(Some(ChangeStatus::Closed));
        assert_eq!(text, format!("{PILL_LEFT} Closed {PILL_RIGHT}"));
    }

    #[test]
    fn pill_merged_plain_text() {
        let text = render_pill_plain(Some(ChangeStatus::Merged));
        assert_eq!(text, format!("{PILL_LEFT} Merged {PILL_RIGHT}"));
    }

    #[test]
    fn pill_unknown_plain_text() {
        let text = render_pill_plain(None);
        assert_eq!(text, format!("{PILL_LEFT} ? {PILL_RIGHT}"));
    }

    // -- render_node_text tests (plain text) --

    fn render_node_plain(
        node: &BookmarkNode,
        cr_state: &ChangeRequests,
        live_crs: &HashMap<String, Result<Box<dyn ChangeRequest>, String>>,
    ) -> String {
        let factory = jj_cli::formatter::FormatterFactory::plain_text();
        let mut buf = Vec::new();
        {
            let mut fmt = factory.new_formatter(&mut buf);
            render_node_text(&mut *fmt, node, cr_state, live_crs).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn node_text_no_cr() {
        let node = make_node("feature-a");
        let cr_state = ChangeRequests::default();
        let live_crs = HashMap::new();

        let text = render_node_plain(&node, &cr_state, &live_crs);
        // Two lines: bookmark + placeholder, then empty line.
        assert_eq!(text, "feature-a (no change request)\n\n");
    }

    #[test]
    fn node_text_with_live_cr_layout() {
        let node = make_node("feature-a");
        let mut cr_state = ChangeRequests::default();
        cr_state.set(
            "feature-a".into(),
            ForgeMeta {
                forge: Some(ForgeOneof::Github(GitHubMeta {
                    number: 1,
                    source_branch: String::new(),
                    target_branch: String::new(),
                    source_repo: String::new(),
                    target_repo: String::new(),
                    graphql_id: String::new(),
                    comment_id: None,
                })),
            },
        );
        let mut live_crs: HashMap<String, Result<Box<dyn ChangeRequest>, String>> = HashMap::new();
        live_crs.insert(
            "feature-a".into(),
            Ok(Box::new(GitHubChangeRequest {
                meta: GitHubMeta {
                    number: 1,
                    source_branch: "feature-a".into(),
                    target_branch: "main".into(),
                    source_repo: "owner/repo".into(),
                    target_repo: "owner/repo".into(),
                    graphql_id: String::new(),
                    comment_id: None,
                },
                host: "github.com".into(),
                title: "Add cool feature".into(),
                body: None,
                status: ChangeStatus::Open,
                url: "https://github.com/owner/repo/pull/1".into(),
            })),
        );

        let text = render_node_plain(&node, &cr_state, &live_crs);
        let lines: Vec<&str> = text.lines().collect();
        // Line 1: bookmark + pill + link.
        assert_eq!(
            lines[0],
            format!(
                "feature-a {PILL_LEFT} Open {PILL_RIGHT} github.com:owner/repo#1 (https://github.com/owner/repo/pull/1)"
            )
        );
        // Line 2: CR title.
        assert_eq!(lines[1], "Add cool feature");
    }

    #[test]
    fn node_text_with_live_draft_cr() {
        let node = make_node("feat");
        let mut cr_state = ChangeRequests::default();
        cr_state.set(
            "feat".into(),
            ForgeMeta {
                forge: Some(ForgeOneof::Github(GitHubMeta {
                    number: 5,
                    source_branch: String::new(),
                    target_branch: String::new(),
                    source_repo: String::new(),
                    target_repo: String::new(),
                    graphql_id: String::new(),
                    comment_id: None,
                })),
            },
        );
        let mut live_crs: HashMap<String, Result<Box<dyn ChangeRequest>, String>> = HashMap::new();
        live_crs.insert(
            "feat".into(),
            Ok(Box::new(GitHubChangeRequest {
                meta: GitHubMeta {
                    number: 5,
                    source_branch: "feat".into(),
                    target_branch: "main".into(),
                    source_repo: "o/r".into(),
                    target_repo: "o/r".into(),
                    graphql_id: String::new(),
                    comment_id: None,
                },
                host: "github.com".into(),
                title: "WIP thing".into(),
                body: None,
                status: ChangeStatus::Draft,
                url: "https://github.com/o/r/pull/5".into(),
            })),
        );

        let text = render_node_plain(&node, &cr_state, &live_crs);
        let first_line = text.lines().next().unwrap();
        // Draft pill should appear instead of Open.
        assert!(first_line.contains("Draft"));
        assert!(!first_line.contains("Open"));
    }

    #[test]
    fn node_text_with_stored_meta_but_no_forge() {
        let node = make_node("feature-b");
        let mut cr_state = ChangeRequests::default();
        cr_state.set(
            "feature-b".into(),
            ForgeMeta {
                forge: Some(ForgeOneof::Github(GitHubMeta {
                    number: 10,
                    source_branch: String::new(),
                    target_branch: String::new(),
                    source_repo: String::new(),
                    target_repo: String::new(),
                    graphql_id: String::new(),
                    comment_id: None,
                })),
            },
        );
        let live_crs = HashMap::new();

        let text = render_node_plain(&node, &cr_state, &live_crs);
        // Two lines: bookmark + unknown pill + stored ID, then empty line.
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("feature-b"));
        assert!(lines[0].contains("#10"));
        assert!(lines[0].contains("?"));
        assert_eq!(text.matches('\n').count(), 2);
    }
}
