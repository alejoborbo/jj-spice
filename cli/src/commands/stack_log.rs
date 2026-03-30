use std::collections::{HashMap, HashSet};
use std::io::Write;

use jj_cli::cli_util::RevisionArg;
use jj_cli::formatter::Formatter;
use jj_cli::graphlog::{GraphStyle, get_graphlog};
use jj_lib::backend::CommitId;
use jj_lib::config::{ConfigLayer, ConfigSource};
use jj_lib::graph::GraphEdge;
use jj_lib::repo::Repo;

use crate::commands::env::{OutputMode, SpiceEnv};
use jj_spice_lib::bookmark::graph::{BookmarkGraph, BookmarkNode};
use jj_spice_lib::bookmark::resolve_commit_id;
use jj_spice_lib::forge::detect::{DetectionResult, detect_forges};
use jj_spice_lib::forge::{ChangeRequest, ChangeStatus, Forge};
use jj_spice_lib::protos::change_request::ForgeMeta;
use jj_spice_lib::protos::change_request::forge_meta::Forge as ForgeOneof;
use jj_spice_lib::store::SpiceStore;
use jj_spice_lib::store::change_request::ChangeRequestStore;

/// Default color rules for `stack log` output (modern mode).
///
/// Injected at [`ConfigSource::Default`] priority so users can override
/// any of these in their own jj config under `[colors]`.
///
/// Modern mode uses Powerline pill caps with background-colored inner text,
/// requiring the separate `"cap"` sub-label for the glyph transitions.
const SPICE_COLOR_DEFAULTS_MODERN: &str = r##"
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
"spice current" = { fg = "green", bold = true }
"spice nearby" = { fg = "green", bold = true }
"spice rebase_warning" = { fg = "yellow", bold = true }
"##;

/// Default color rules for `stack log` output (classic mode).
///
/// Classic mode renders status as `[Open]` with foreground-only coloring,
/// so no `"cap"` sub-label or background colors are needed.
const SPICE_COLOR_DEFAULTS_CLASSIC: &str = r##"
[colors]
"spice bookmark" = "magenta"
"spice cr_id" = { fg = "yellow" }
"spice cr_title" = { fg = "default" }
"spice url" = { fg = "bright blue", underline = true }
"spice trunk" = "cyan"
"spice no_cr" = { fg = "bright black" }
"spice status_open" = { fg = "#238636", bold = true }
"spice status_draft" = { fg = "#555555", bold = true }
"spice status_closed" = { fg = "#DA3743", bold = true }
"spice status_merged" = { fg = "#A371F7", bold = true }
"spice status_unknown" = { fg = "bright black", bold = true }
"spice current" = { fg = "green", bold = true }
"spice nearby" = { fg = "green", bold = true }
"spice rebase_warning" = { fg = "yellow", bold = true }
"##;

/// Left cap of a pill-shaped status badge (Powerline rounded glyph, modern mode).
const PILL_LEFT_MODERN: &str = "\u{e0b6}";
/// Right cap of a pill-shaped status badge (Powerline rounded glyph, modern mode).
const PILL_RIGHT_MODERN: &str = "\u{e0b4}";
/// Left bracket for status badge (classic mode).
const PILL_LEFT_CLASSIC: &str = "[";
/// Right bracket for status badge (classic mode).
const PILL_RIGHT_CLASSIC: &str = "]";

/// Node symbol used for every bookmark in the graph.
const NODE_SYMBOL: &str = "\u{25cb}";

/// Node symbol for a bookmark whose commit is exactly the working copy (`@`).
const CURRENT_SYMBOL: &str = "@";

/// Node symbol for a bookmark whose segment contains the working copy
/// (i.e. `@` sits between this bookmark's parents and the bookmark itself).
const NEARBY_SYMBOL: &str = "\u{25c9}";

/// Node symbol used for the trunk (immutable) bookmark.
const TRUNK_SYMBOL: &str = "\u{25c6}";

/// Relationship between a bookmark node and the working copy commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WcPosition {
    /// The working copy commit is exactly on this bookmark's commit.
    Current,
    /// The working copy commit is between this bookmark's parent bookmarks
    /// and this bookmark (belongs to this bookmark's segment).
    Nearby,
}

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
    let detection = detect_forge_result(env);

    // Resolve the working copy commit (if available) for position marking.
    let wc_commit = env.resolve_single_rev(&RevisionArg::AT).ok();

    // Collect nodes and fetch live CR data.
    let nodes: Vec<&BookmarkNode> = graph.iter_graph()?.collect();
    let live_crs = fetch_live_crs(&nodes, &cr_state, &detection).await;

    // Find root bookmarks not parented on current trunk.
    let stale_roots = find_stale_roots(env.repo.as_ref(), &graph, trunk);

    // Render the graph.
    render_graph(
        env,
        &nodes,
        &graph,
        trunk_name.as_deref(),
        &cr_state,
        &live_crs,
        wc_commit.as_ref(),
        &stale_roots,
    )?;

    Ok(())
}

/// Collect root bookmarks whose commits are not parented on trunk.
///
/// Each returned name is a root bookmark (bottom of a stack) whose commit
/// has no parent matching `trunk_id`, meaning `jj rebase` is needed to
/// bring that stack up to date with trunk.
fn find_stale_roots(
    repo: &dyn Repo,
    graph: &BookmarkGraph,
    trunk_id: &CommitId,
) -> HashSet<String> {
    let mut stale = HashSet::new();
    for root_name in graph.root_bookmarks() {
        let Some(node) = graph.get_node(root_name) else {
            continue;
        };
        for commit_id in node.commits() {
            let Ok(commit) = repo.store().get_commit(commit_id) else {
                continue;
            };
            if !commit.parent_ids().contains(trunk_id) {
                stale.insert(root_name.clone());
            }
        }
    }
    stale
}

/// Find the bookmark name pointing at the trunk commit.
///
/// Scans all bookmarks in the repo for one whose commit (local or remote)
/// matches `trunk_id`. Returns `None` if no bookmark is found (shouldn't
/// happen in practice since `trunk()` resolves from a bookmark).
fn resolve_trunk_bookmark_name(env: &SpiceEnv, trunk_id: &CommitId) -> Option<String> {
    env.repo.view().bookmarks().find_map(|(name, target)| {
        if resolve_commit_id(&target) == Some(trunk_id) {
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

/// Auto-detect forges from git remotes (no interactive prompts).
fn detect_forge_result(env: &SpiceEnv) -> DetectionResult {
    detect_forges(env.repo.store(), env.config()).unwrap_or(DetectionResult {
        forges: HashMap::new(),
        unmatched: Vec::new(),
    })
}

/// Determine which bookmark (if any) contains the working copy commit and how.
///
/// Returns a map from bookmark name to its [`WcPosition`]:
/// - [`WcPosition::Current`]: `wc_id` is exactly one of the bookmark's commit IDs.
/// - [`WcPosition::Nearby`]: `wc_id` is an ancestor of the bookmark's commit and
///   a descendant of every parent bookmark's commit — meaning `@` sits inside
///   this bookmark's segment of the graph.
///
/// At most one bookmark is returned; direct matches take priority over indirect.
fn find_wc_bookmark(
    nodes: &[&BookmarkNode],
    graph: &BookmarkGraph,
    repo: &dyn Repo,
    wc_id: &CommitId,
) -> HashMap<String, WcPosition> {
    let mut result = HashMap::new();

    // 1. Direct match: @ is exactly on a bookmark's commit.
    for node in nodes {
        if node.commits().contains(wc_id) {
            result.insert(node.name().to_string(), WcPosition::Current);
            return result;
        }
    }

    // 2. Indirect match: @ is between a bookmark's parents and the bookmark.
    //    A bookmark qualifies when:
    //    - @ is a strict ancestor of the bookmark's commit, AND
    //    - @ is a strict descendant of every parent bookmark's commit
    //      (or the bookmark has no parents, i.e. it is a root of the stack).
    let index = repo.index();
    for node in nodes {
        let Some(node_commit) = node.commits().first() else {
            continue;
        };
        // @ must be a strict ancestor of this bookmark's commit.
        let Ok(is_anc) = index.is_ancestor(wc_id, node_commit) else {
            continue;
        };
        if !is_anc || wc_id == node_commit {
            continue;
        }

        // @ must be a strict descendant of every parent bookmark's commit.
        let parent_edges = graph.edges_for(node.name());
        let all_parents_below = parent_edges.iter().all(|edge| {
            // Find the parent node's commit.
            nodes
                .iter()
                .find(|n| n.name() == edge.target)
                .and_then(|parent| parent.commits().first())
                .and_then(|parent_commit| index.is_ancestor(parent_commit, wc_id).ok())
                .is_some_and(|is_anc| is_anc && edge.target != node.name())
        });

        if all_parents_below || parent_edges.is_empty() {
            result.insert(node.name().to_string(), WcPosition::Nearby);
            return result;
        }
    }

    result
}

/// Fetch live change request data for each bookmark that has stored metadata.
///
/// Returns a map from bookmark name to the live CR result. Forge API errors
/// are captured per-bookmark rather than failing the entire command.
async fn fetch_live_crs(
    nodes: &[&BookmarkNode<'_>],
    cr_state: &jj_spice_lib::protos::change_request::ChangeRequests,
    detection: &DetectionResult,
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

        // For cross-repo (fork) PRs the CR lives on the upstream repo,
        // not the fork the bookmark is tracked on. Try matching by
        // target_repo first, then fall back to the tracked remote.
        let forge = detection
            .resolve_forge_for_meta(meta)
            .or_else(|| find_forge_for_bookmark(node, &detection.forges));
        let Some(forge) = forge else {
            results.insert(name.to_string(), Err("no forge detected".to_string()));
            continue;
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
#[allow(clippy::too_many_arguments)]
fn render_graph(
    env: &SpiceEnv,
    nodes: &[&BookmarkNode<'_>],
    graph: &BookmarkGraph,
    trunk_name: Option<&str>,
    cr_state: &jj_spice_lib::protos::change_request::ChangeRequests,
    live_crs: &HashMap<String, Result<Box<dyn ChangeRequest>, String>>,
    wc_commit: Option<&CommitId>,
    stale_roots: &HashSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mode = env.output_mode;

    // Determine which bookmark (if any) holds the working copy.
    let wc_positions = wc_commit
        .map(|wc| find_wc_bookmark(nodes, graph, env.repo.as_ref(), wc))
        .unwrap_or_default();
    // Build a formatter factory that includes spice color defaults.
    // Respects --color=never by falling back to plain text.
    let factory = if env.ui.color() {
        let config = inject_color_defaults(env, mode)?;
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
            let wc_pos = wc_positions.get(name).copied();

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
                let needs_rebase = stale_roots.contains(name);
                render_node_text(&mut *fmt, node, cr_state, live_crs, mode, needs_rebase)?;
            }
            let text = String::from_utf8_lossy(&text_buf);

            // Render the node symbol with positional color when applicable.
            let symbol = render_node_symbol(&factory, wc_pos);
            graphlog.add_node(&name.to_string(), &edges, &symbol, &text)?;
        }

        // Render trunk as the terminal node at the bottom.
        if let Some(trunk) = trunk_name {
            let mut text_buf: Vec<u8> = Vec::new();
            {
                let mut fmt = factory.new_formatter(&mut text_buf);
                render_trunk_text(&mut *fmt, trunk)?;
            }
            let text = String::from_utf8_lossy(&text_buf);
            graphlog.add_node(&trunk.to_string(), &[], TRUNK_SYMBOL, &text)?;
        }
    }

    // Write through the Ui's pager-aware stdout so the output is
    // displayed in the configured pager when running interactively.
    let mut stdout = env.ui.stdout();
    stdout.write_all(&stdout_buf)?;
    stdout.flush()?;
    Ok(())
}

/// Build a config stack with spice color defaults injected.
///
/// The defaults are added at [`ConfigSource::Default`] priority so any
/// user-defined `[colors]` rules take precedence. The set of defaults
/// depends on the active [`OutputMode`].
fn inject_color_defaults(
    env: &SpiceEnv,
    mode: OutputMode,
) -> Result<jj_lib::config::StackedConfig, Box<dyn std::error::Error>> {
    let mut config = env.config().clone();
    let defaults = match mode {
        OutputMode::Modern => SPICE_COLOR_DEFAULTS_MODERN,
        OutputMode::Classic => SPICE_COLOR_DEFAULTS_CLASSIC,
    };
    let layer = ConfigLayer::parse(ConfigSource::Default, defaults)?;
    config.add_layer(layer);
    Ok(config)
}

/// Render a colored node symbol for the graphlog.
///
/// When `wc_pos` is set the symbol glyph is changed (`@` for current,
/// `◉` for nearby) and wrapped in the corresponding `"spice current"` or
/// `"spice nearby"` color label. Otherwise the default `◯` symbol is
/// returned without any color markup.
fn render_node_symbol(
    factory: &jj_cli::formatter::FormatterFactory,
    wc_pos: Option<WcPosition>,
) -> String {
    let (glyph, label) = match wc_pos {
        Some(WcPosition::Current) => (CURRENT_SYMBOL, Some("current")),
        Some(WcPosition::Nearby) => (NEARBY_SYMBOL, Some("nearby")),
        None => (NODE_SYMBOL, None),
    };

    if let Some(lbl) = label {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut fmt = factory.new_formatter(&mut buf);
            fmt.push_label("spice");
            fmt.push_label(lbl);
            // write! error is non-fatal — fall back to plain glyph.
            let _ = write!(fmt, "{glyph}");
            fmt.pop_label();
            fmt.pop_label();
        }
        String::from_utf8(buf).unwrap_or_else(|_| glyph.to_string())
    } else {
        glyph.to_string()
    }
}

/// Render the node text for a single bookmark (always 2 lines).
///
/// Line 1: bookmark name + status pill + link (or placeholder) [+ rebase warning]
/// Line 2: CR title, or empty when no title is available
fn render_node_text(
    fmt: &mut dyn Formatter,
    node: &BookmarkNode,
    cr_state: &jj_spice_lib::protos::change_request::ChangeRequests,
    live_crs: &HashMap<String, Result<Box<dyn ChangeRequest>, String>>,
    mode: OutputMode,
    needs_rebase: bool,
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
            render_status_pill(fmt, Some(cr.status()), mode)?;
            write!(fmt, " ")?;
            write_hyperlink(fmt, cr.url(), &cr.link_label(), mode)?;
        }
        // Stored metadata but forge API failed — show stored ID + unknown pill.
        (Some(meta), Some(Err(_))) => {
            let id = format_meta_id(meta);
            write!(fmt, " ")?;
            render_status_pill(fmt, None, mode)?;
            write!(fmt, " ")?;
            fmt.push_label("cr_id");
            write!(fmt, "#{id}")?;
            fmt.pop_label();
        }
        // Stored metadata but no forge detected — show stored ID + unknown pill.
        (Some(meta), None) => {
            let id = format_meta_id(meta);
            write!(fmt, " ")?;
            render_status_pill(fmt, None, mode)?;
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

    if needs_rebase {
        write!(fmt, " ")?;
        fmt.push_label("rebase_warning");
        write!(fmt, "(needs rebase)")?;
        fmt.pop_label();
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

/// Render the trunk node text.
fn render_trunk_text(fmt: &mut dyn Formatter, trunk_name: &str) -> std::io::Result<()> {
    fmt.push_label("spice");
    fmt.push_label("trunk");
    write!(fmt, "{trunk_name}")?;
    fmt.pop_label();
    fmt.pop_label();
    writeln!(fmt)?;
    Ok(())
}

/// Write a hyperlink to the formatter.
///
/// In **modern** color mode, emits OSC 8 terminal hyperlinks
/// (`ESC]8;;URL ST <visible_text> ESC]8;; ST`) that modern terminals
/// render as clickable links. The visible text is styled with the `"url"`
/// label.
///
/// In **classic** mode (or plain-text mode), writes the visible text
/// followed by the URL in parentheses as a readable fallback.
fn write_hyperlink(
    fmt: &mut dyn Formatter,
    url: &str,
    text: &str,
    mode: OutputMode,
) -> std::io::Result<()> {
    if mode == OutputMode::Modern && fmt.maybe_color() {
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
        // Classic / plain-text fallback: visible text (URL)
        write!(fmt, "{text} ({url})")?;
    }
    Ok(())
}

/// Render a status badge.
///
/// In **modern** mode, renders a pill-shaped badge using Powerline rounded
/// glyphs with background-colored inner text and `"cap"` sub-labels for
/// the glyph transitions.
///
/// In **classic** mode, renders `[Status]` using foreground-only color
/// and ASCII brackets.
fn render_status_pill(
    fmt: &mut dyn Formatter,
    status: Option<ChangeStatus>,
    mode: OutputMode,
) -> std::io::Result<()> {
    let (label, text) = match status {
        Some(ChangeStatus::Open) => ("status_open", "Open"),
        Some(ChangeStatus::Closed) => ("status_closed", "Closed"),
        Some(ChangeStatus::Merged) => ("status_merged", "Merged"),
        Some(ChangeStatus::Draft) => ("status_draft", "Draft"),
        None => ("status_unknown", "?"),
    };

    fmt.push_label(label);

    match mode {
        OutputMode::Modern => {
            // Left cap: colored glyph on default background.
            fmt.push_label("cap");
            write!(fmt, "{PILL_LEFT_MODERN}")?;
            fmt.pop_label();

            // Inner text: white on colored background.
            write!(fmt, " {text} ")?;

            // Right cap: colored glyph on default background.
            fmt.push_label("cap");
            write!(fmt, "{PILL_RIGHT_MODERN}")?;
            fmt.pop_label();
        }
        OutputMode::Classic => {
            // Simple bracketed badge with fg-only color.
            write!(fmt, "{PILL_LEFT_CLASSIC}{text}{PILL_RIGHT_CLASSIC}")?;
        }
    }

    fmt.pop_label();
    Ok(())
}

/// Extract a display ID from stored [`ForgeMeta`].
fn format_meta_id(meta: &ForgeMeta) -> String {
    match &meta.forge {
        Some(ForgeOneof::Github(gh)) => gh.number.to_string(),
        Some(ForgeOneof::Gitlab(gl)) => format!("!{}", gl.iid),
        None => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jj_lib::op_store::{LocalRemoteRefTarget, RefTarget};
    use jj_spice_lib::forge::github::GitHubChangeRequest;
    use jj_spice_lib::protos::change_request::{ChangeRequests, GitHubMeta};

    /// Map from bookmark name to its live change request fetch result.
    type LiveCrMap = HashMap<String, Result<Box<dyn ChangeRequest>, String>>;

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

    // -- render_status_pill tests (plain text) --

    fn render_pill_plain(status: Option<ChangeStatus>, mode: OutputMode) -> String {
        let factory = jj_cli::formatter::FormatterFactory::plain_text();
        let mut buf = Vec::new();
        {
            let mut fmt = factory.new_formatter(&mut buf);
            render_status_pill(&mut *fmt, status, mode).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn pill_open_modern() {
        let text = render_pill_plain(Some(ChangeStatus::Open), OutputMode::Modern);
        assert_eq!(text, format!("{PILL_LEFT_MODERN} Open {PILL_RIGHT_MODERN}"));
    }

    #[test]
    fn pill_draft_modern() {
        let text = render_pill_plain(Some(ChangeStatus::Draft), OutputMode::Modern);
        assert_eq!(
            text,
            format!("{PILL_LEFT_MODERN} Draft {PILL_RIGHT_MODERN}")
        );
    }

    #[test]
    fn pill_closed_modern() {
        let text = render_pill_plain(Some(ChangeStatus::Closed), OutputMode::Modern);
        assert_eq!(
            text,
            format!("{PILL_LEFT_MODERN} Closed {PILL_RIGHT_MODERN}")
        );
    }

    #[test]
    fn pill_merged_modern() {
        let text = render_pill_plain(Some(ChangeStatus::Merged), OutputMode::Modern);
        assert_eq!(
            text,
            format!("{PILL_LEFT_MODERN} Merged {PILL_RIGHT_MODERN}")
        );
    }

    #[test]
    fn pill_unknown_modern() {
        let text = render_pill_plain(None, OutputMode::Modern);
        assert_eq!(text, format!("{PILL_LEFT_MODERN} ? {PILL_RIGHT_MODERN}"));
    }

    #[test]
    fn pill_open_classic() {
        let text = render_pill_plain(Some(ChangeStatus::Open), OutputMode::Classic);
        assert_eq!(text, "[Open]");
    }

    #[test]
    fn pill_draft_classic() {
        let text = render_pill_plain(Some(ChangeStatus::Draft), OutputMode::Classic);
        assert_eq!(text, "[Draft]");
    }

    #[test]
    fn pill_closed_classic() {
        let text = render_pill_plain(Some(ChangeStatus::Closed), OutputMode::Classic);
        assert_eq!(text, "[Closed]");
    }

    #[test]
    fn pill_merged_classic() {
        let text = render_pill_plain(Some(ChangeStatus::Merged), OutputMode::Classic);
        assert_eq!(text, "[Merged]");
    }

    #[test]
    fn pill_unknown_classic() {
        let text = render_pill_plain(None, OutputMode::Classic);
        assert_eq!(text, "[?]");
    }

    // -- render_node_text tests (plain text) --

    fn render_node_plain(
        node: &BookmarkNode,
        cr_state: &ChangeRequests,
        live_crs: &LiveCrMap,
        mode: OutputMode,
    ) -> String {
        render_node_plain_with_rebase(node, cr_state, live_crs, mode, false)
    }

    fn render_node_plain_with_rebase(
        node: &BookmarkNode,
        cr_state: &ChangeRequests,
        live_crs: &HashMap<String, Result<Box<dyn ChangeRequest>, String>>,
        mode: OutputMode,
        needs_rebase: bool,
    ) -> String {
        let factory = jj_cli::formatter::FormatterFactory::plain_text();
        let mut buf = Vec::new();
        {
            let mut fmt = factory.new_formatter(&mut buf);
            render_node_text(&mut *fmt, node, cr_state, live_crs, mode, needs_rebase).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn node_text_no_cr() {
        // Output is mode-independent when there is no CR.
        for mode in [OutputMode::Modern, OutputMode::Classic] {
            let node = make_node("feature-a");
            let cr_state = ChangeRequests::default();
            let live_crs = HashMap::new();

            let text = render_node_plain(&node, &cr_state, &live_crs, mode);
            assert_eq!(text, "feature-a (no change request)\n\n");
        }
    }

    fn make_live_cr_fixtures() -> (BookmarkNode<'static>, ChangeRequests, LiveCrMap) {
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
        let mut live_crs: LiveCrMap = HashMap::new();
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
        (node, cr_state, live_crs)
    }

    #[test]
    fn node_text_with_live_cr_modern() {
        let (node, cr_state, live_crs) = make_live_cr_fixtures();

        let text = render_node_plain(&node, &cr_state, &live_crs, OutputMode::Modern);
        let lines: Vec<&str> = text.lines().collect();
        // Modern: Powerline pill + plain-text link fallback (no color formatter).
        assert_eq!(
            lines[0],
            format!(
                "feature-a {PILL_LEFT_MODERN} Open {PILL_RIGHT_MODERN} github.com:owner/repo#1 (https://github.com/owner/repo/pull/1)"
            )
        );
        assert_eq!(lines[1], "Add cool feature");
    }

    #[test]
    fn node_text_with_live_cr_classic() {
        let (node, cr_state, live_crs) = make_live_cr_fixtures();

        let text = render_node_plain(&node, &cr_state, &live_crs, OutputMode::Classic);
        let lines: Vec<&str> = text.lines().collect();
        // Classic: ASCII brackets + plain-text link.
        assert_eq!(
            lines[0],
            "feature-a [Open] github.com:owner/repo#1 (https://github.com/owner/repo/pull/1)"
        );
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
        let mut live_crs: LiveCrMap = HashMap::new();
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

        for mode in [OutputMode::Modern, OutputMode::Classic] {
            let text = render_node_plain(&node, &cr_state, &live_crs, mode);
            let first_line = text.lines().next().unwrap();
            assert!(first_line.contains("Draft"));
            assert!(!first_line.contains("Open"));
        }
    }

    // -- render_trunk_text tests (plain text) --

    fn render_trunk_plain(trunk_name: &str) -> String {
        let factory = jj_cli::formatter::FormatterFactory::plain_text();
        let mut buf = Vec::new();
        {
            let mut fmt = factory.new_formatter(&mut buf);
            render_trunk_text(&mut *fmt, trunk_name).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn trunk_text_plain() {
        let text = render_trunk_plain("main");
        assert_eq!(text, "main\n");
    }

    // -- rebase warning on root bookmark tests --

    #[test]
    fn node_text_no_cr_with_rebase_warning() {
        let node = make_node("feature-a");
        let cr_state = ChangeRequests::default();
        let live_crs = HashMap::new();

        let text =
            render_node_plain_with_rebase(&node, &cr_state, &live_crs, OutputMode::Modern, true);
        assert_eq!(text, "feature-a (no change request) (needs rebase)\n\n");
    }

    #[test]
    fn node_text_no_cr_without_rebase_warning() {
        let node = make_node("feature-a");
        let cr_state = ChangeRequests::default();
        let live_crs = HashMap::new();

        let text =
            render_node_plain_with_rebase(&node, &cr_state, &live_crs, OutputMode::Modern, false);
        assert_eq!(text, "feature-a (no change request)\n\n");
    }

    #[test]
    fn node_text_with_live_cr_and_rebase_warning() {
        let node = make_node("feat");
        let mut cr_state = ChangeRequests::default();
        cr_state.set(
            "feat".into(),
            ForgeMeta {
                forge: Some(ForgeOneof::Github(GitHubMeta {
                    number: 3,
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
                    number: 3,
                    source_branch: "feat".into(),
                    target_branch: "main".into(),
                    source_repo: "o/r".into(),
                    target_repo: "o/r".into(),
                    graphql_id: String::new(),
                    comment_id: None,
                },
                host: "github.com".into(),
                title: "Cool".into(),
                body: None,
                status: ChangeStatus::Open,
                url: "https://github.com/o/r/pull/3".into(),
            })),
        );

        let text =
            render_node_plain_with_rebase(&node, &cr_state, &live_crs, OutputMode::Modern, true);
        let first_line = text.lines().next().unwrap();
        assert!(first_line.contains("(needs rebase)"));
        assert!(first_line.contains("Open"));
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

        for mode in [OutputMode::Modern, OutputMode::Classic] {
            let text = render_node_plain(&node, &cr_state, &live_crs, mode);
            let lines: Vec<&str> = text.lines().collect();
            assert!(lines[0].starts_with("feature-b"));
            assert!(lines[0].contains("#10"));
            assert!(lines[0].contains("?"));
            assert_eq!(text.matches('\n').count(), 2);
        }
    }

    // -- WcPosition / node symbol tests --

    #[test]
    fn node_symbol_current() {
        assert_eq!(CURRENT_SYMBOL, "@");
    }

    #[test]
    fn node_symbol_nearby() {
        assert_eq!(NEARBY_SYMBOL, "\u{25c9}");
    }

    #[test]
    fn render_node_symbol_plain_text() {
        // In plain-text mode the symbol is the raw glyph without color.
        let factory = jj_cli::formatter::FormatterFactory::plain_text();
        assert_eq!(render_node_symbol(&factory, None), NODE_SYMBOL);
        assert_eq!(
            render_node_symbol(&factory, Some(WcPosition::Current)),
            CURRENT_SYMBOL
        );
        assert_eq!(
            render_node_symbol(&factory, Some(WcPosition::Nearby)),
            NEARBY_SYMBOL
        );
    }

    #[test]
    fn render_node_symbol_normal_has_no_ansi() {
        // Normal nodes should never contain escape sequences, even when
        // a color factory is theoretically available.
        let factory = jj_cli::formatter::FormatterFactory::plain_text();
        let symbol = render_node_symbol(&factory, None);
        assert!(!symbol.contains('\x1b'), "plain symbol should have no ANSI");
    }
}
