use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use jj_cli::cli_util::{GlobalArgs, RevisionArg, find_workspace_dir};
use jj_cli::command_error::print_parse_diagnostics;
use jj_cli::config::{
    ConfigArgKind, ConfigEnv, config_from_environment, default_config_layers, parse_config_args,
};
use jj_cli::revset_util::{RevsetExpressionEvaluator, load_revset_aliases};
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::config::{ConfigLayer, ConfigSource};
use jj_lib::git::GitSettings;
use jj_lib::id_prefix::IdPrefixContext;
use jj_lib::op_walk;
use jj_lib::ref_name::RemoteNameBuf;
use jj_lib::repo::{ReadonlyRepo, Repo as _, StoreFactories};
use jj_lib::repo_path::RepoPathUiConverter;
use jj_lib::revset::{
    ResolvedRevsetExpression, RevsetAliasesMap, RevsetDiagnostics, RevsetExtensions,
    RevsetParseContext, RevsetWorkspaceContext,
};
use jj_lib::settings::UserSettings;
use jj_lib::transaction::Transaction;
use jj_lib::workspace::{
    DefaultWorkspaceLoaderFactory, Workspace, WorkspaceLoaderFactory,
    default_working_copy_factories,
};

use jj_spice_lib::forge::Forge;

/// Resolved forge for PR creation, with an optional source repository
/// identifier for cross-repo (fork) head refs.
pub(crate) type ResolvedForge = (Box<dyn Forge>, Option<String>);

use jj_spice_lib::store::SpiceStore;

/// Controls terminal output fidelity.
///
/// - **Modern** (default): Nerd Font Powerline glyphs for status pills,
///   OSC 8 terminal hyperlinks.
/// - **Classic**: ASCII brackets `[Status]` with foreground-only colors,
///   plain-text URL fallbacks. Safe for terminals without Nerd Fonts.
///
/// Configured via `spice.output` in jj config (`"modern"` or `"classic"`).
/// Defaults to `Modern` when unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum OutputMode {
    #[default]
    Modern,
    Classic,
}

impl OutputMode {
    /// Resolve the output mode from the jj config stack.
    ///
    /// Reads `spice.output` and maps `"classic"` → [`Classic`](Self::Classic),
    /// anything else (including absent) → [`Modern`](Self::Modern).
    pub(crate) fn from_config(config: &jj_lib::config::StackedConfig) -> Self {
        match config.get::<String>(["spice", "output"]) {
            Ok(value) if value.eq_ignore_ascii_case("classic") => Self::Classic,
            _ => Self::Modern,
        }
    }
}

/// Shared context built once from the jj config pipeline and workspace.
pub(crate) struct SpiceEnv {
    /// Terminal UI handle for user-facing output and diagnostics.
    pub(crate) ui: Ui,
    /// Immutable repository snapshot at HEAD.
    pub(crate) repo: Arc<ReadonlyRepo>,
    /// Resolved user settings from the full jj config stack.
    pub(crate) settings: UserSettings,
    /// Resolved Git settings from the full jj config stack.
    pub(crate) git_settings: GitSettings,
    /// Open workspace (working copy + repo loader).
    pub(crate) workspace: Workspace,
    /// Configuration environment for locating config files (e.g. repo config).
    pub(crate) config_env: ConfigEnv,
    /// Immutable store for change requests.
    pub(crate) store: SpiceStore,
    /// Terminal output fidelity (modern vs classic).
    pub(crate) output_mode: OutputMode,
    path_converter: RepoPathUiConverter,
    user_email: String,
    revset_aliases: RevsetAliasesMap,
    revset_extensions: Arc<RevsetExtensions>,
}

impl SpiceEnv {
    /// Bootstrap the environment from the current working directory and
    /// jj-compatible global options.
    pub(crate) fn init(global_args: &GlobalArgs) -> Result<Self, Box<dyn std::error::Error>> {
        let cwd = std::env::current_dir()?;

        // 1. Load the full jj config stack (defaults + user + repo + workspace),
        //    applying --repository, --config, --config-file, --color, --quiet,
        //    and --no-pager overrides.
        let (config, ui, workspace_root, config_env) = load_config(&cwd, global_args)?;

        // 2. Load workspace + repo via jj-lib.
        let settings = UserSettings::from_config(config.clone())?;
        let workspace = Workspace::load(
            &settings,
            &workspace_root,
            &StoreFactories::default(),
            &default_working_copy_factories(),
        )?;
        let git_settings = GitSettings::from_settings(&settings)?;

        // 3. Load the repo, optionally at a specific operation (--at-operation).
        let repo = if let Some(op_str) = &global_args.at_operation {
            let loader = workspace.repo_loader();
            let op = op_walk::resolve_op_for_load(loader, op_str)?;
            loader.load_at(&op)?
        } else {
            workspace.repo_loader().load_at_head()?
        };

        // 4. Revset setup: load aliases once, like jj-cli does.
        let user_email: String = config.get(["user", "email"]).unwrap_or_default();
        let revset_aliases = load_revset_aliases(&ui, &config).map_err(cmd_err)?;
        let revset_extensions = Arc::new(RevsetExtensions::new());

        // 5. Build path converter once so revset_parse_context() can borrow it.
        let path_converter = RepoPathUiConverter::Fs {
            cwd,
            base: workspace.workspace_root().to_owned(),
        };

        // 6. Build the store.
        let store = SpiceStore::init_at(workspace.repo_path()).expect("failed to init store");

        // 7. Resolve output mode from config.
        let output_mode = OutputMode::from_config(&config);

        Ok(Self {
            ui,
            repo,
            settings,
            git_settings,
            workspace,
            config_env,
            store,
            output_mode,
            path_converter,
            user_email,
            revset_aliases,
            revset_extensions,
        })
    }

    /// Build a lightweight [`RevsetParseContext`] borrowing cached state.
    fn revset_parse_context(&self) -> RevsetParseContext<'_> {
        let workspace_ctx = RevsetWorkspaceContext {
            path_converter: &self.path_converter,
            workspace_name: self.workspace.workspace_name(),
        };
        RevsetParseContext {
            aliases_map: &self.revset_aliases,
            local_variables: Default::default(),
            user_email: &self.user_email,
            date_pattern_context: chrono::Local::now().into(),
            default_ignored_remote: Some(jj_lib::git::REMOTE_NAME_FOR_LOCAL_GIT_REPO),
            use_glob_by_default: false,
            extensions: &self.revset_extensions,
            workspace: Some(workspace_ctx),
        }
    }

    /// Access the resolved configuration.
    pub(crate) fn config(&self) -> &jj_lib::config::StackedConfig {
        self.settings.config()
    }

    /// Resolve the default push remote from config (`git.push`),
    /// falling back to `"origin"`.
    pub(crate) fn get_default_remote(&self) -> RemoteNameBuf {
        let name = self
            .config()
            .get::<String>(["git", "push"])
            .unwrap_or_else(|_| "origin".to_string());
        RemoteNameBuf::from(name)
    }

    /// Resolve the upstream remote for PR creation in fork mode.
    ///
    /// Priority:
    /// 1. `spice.upstream-remote` config (explicit override).
    /// 2. `"upstream"` if a remote with that name exists in the git repo.
    /// 3. `None` — single-remote mode, push remote is also the PR target.
    pub(crate) fn get_upstream_remote(&self) -> Option<RemoteNameBuf> {
        // 1. Explicit config override.
        if let Ok(name) = self.config().get::<String>(["spice", "upstream-remote"])
            && !name.is_empty()
        {
            return Some(RemoteNameBuf::from(name));
        }
        // 2. Fall back to "upstream" if that remote exists.
        let git_repo = jj_lib::git::get_git_repo(self.repo.store()).ok()?;
        // Check if any remote is named "upstream" by iterating all remote names.
        let has_upstream = git_repo
            .remote_names()
            .iter()
            .any(|n| n.as_ref() == b"upstream");
        if has_upstream {
            return Some(RemoteNameBuf::from("upstream"));
        }
        None
    }

    /// Whether a git remote with the given name is configured in this repo.
    fn remote_exists(&self, remote: &RemoteNameBuf) -> bool {
        let Ok(git_repo) = jj_lib::git::get_git_repo(self.repo.store()) else {
            return false;
        };
        git_repo
            .remote_names()
            .iter()
            .any(|n| n.as_ref() == remote.as_str().as_bytes())
    }

    /// Whether fork mode is active.
    ///
    /// Fork mode requires **two distinct remotes** that both exist: the push
    /// remote (the fork, defaulting to `"origin"`) and the upstream remote
    /// (where PRs are created). Returns `false` when only one remote is
    /// configured — even if it happens to be named `"upstream"` — because
    /// there is no fork relationship to express.
    ///
    /// When `true`, branches are pushed to the push remote while change
    /// requests are created against the upstream remote.
    pub(crate) fn is_fork_mode(&self) -> bool {
        let Some(upstream) = self.get_upstream_remote() else {
            return false;
        };
        let push = self.get_default_remote();
        // The two remotes must be distinct, and the push remote must actually
        // exist (it may only be a config default like "origin").
        upstream != push && self.remote_exists(&push)
    }

    /// Resolve the forge and optional source repository for PR creation.
    ///
    /// In **fork mode** (two distinct remotes), PRs target the upstream forge
    /// while branches are pushed to the fork (push remote). The fork's
    /// `repo_id` is returned as `source_repo` so the forge can format
    /// cross-repo head refs (e.g. `"fork-owner:branch"`).
    ///
    /// In **single-remote mode**, the only detected forge is used directly
    /// and `source_repo` is `None`.
    pub(crate) fn resolve_forge(
        &self,
        mut forges: HashMap<String, Box<dyn Forge>>,
    ) -> Result<ResolvedForge, Box<dyn std::error::Error>> {
        if self.is_fork_mode() {
            let upstream_remote = self.get_upstream_remote().unwrap();
            let push_remote = self.get_default_remote();

            let fork_repo_id = forges.get(push_remote.as_str()).map(|f| f.repo_id());
            let upstream_forge = forges.remove(upstream_remote.as_str()).ok_or_else(|| {
                format!(
                    "upstream remote `{}` has no \
                         detected forge — check your git remotes",
                    upstream_remote.as_str()
                )
            })?;

            Ok((upstream_forge, fork_repo_id))
        } else {
            let forge = forges
                .into_values()
                .next()
                .ok_or("no forge detected — is a git remote configured?")?;
            Ok((forge, None))
        }
    }

    /// Resolve a revset expression to exactly one commit ID.
    ///
    /// Uses [`RevsetExpressionEvaluator`] from jj-cli for symbol resolution
    /// and evaluation, matching jj's own resolution pipeline.
    pub(crate) fn resolve_single_rev(
        &self,
        revision: &RevisionArg,
    ) -> Result<CommitId, Box<dyn std::error::Error>> {
        let context = self.revset_parse_context();
        let mut diagnostics = RevsetDiagnostics::new();
        let expression = jj_lib::revset::parse(&mut diagnostics, revision.as_ref(), &context)?;
        print_parse_diagnostics(&self.ui, "In revset expression", &diagnostics)?;

        let id_prefix_context = IdPrefixContext::default();
        let evaluator = RevsetExpressionEvaluator::new(
            self.repo.as_ref(),
            self.revset_extensions.clone(),
            &id_prefix_context,
            expression,
        );

        let mut iter = evaluator.evaluate_to_commits()?.fuse();
        match (iter.next(), iter.next()) {
            (Some(Ok(commit)), None) => Ok(commit.id().clone()),
            (Some(Err(e)), _) => Err(e.into()),
            (None, _) => Err(format!("revset `{revision}` didn't resolve to any revisions").into()),
            (Some(_), Some(_)) => {
                Err(format!("revset `{revision}` resolved to more than one revision").into())
            }
        }
    }

    /// Parse and resolve a revset expression string into a resolved expression.
    ///
    /// Unlike [`Self::resolve_single_rev`], this does not require the revset
    /// to resolve to exactly one commit — it can match any number of commits.
    /// The returned expression can be passed to
    /// [`BookmarkGraph::from_revset`][jj_spice_lib::bookmark::graph::BookmarkGraph::from_revset].
    pub(crate) fn resolve_revset(
        &self,
        revset_str: &str,
    ) -> Result<Arc<ResolvedRevsetExpression>, Box<dyn std::error::Error>> {
        let context = self.revset_parse_context();
        let mut diagnostics = RevsetDiagnostics::new();
        let expression = jj_lib::revset::parse(&mut diagnostics, revset_str, &context)?;
        print_parse_diagnostics(&self.ui, "In revset expression", &diagnostics)?;

        let id_prefix_context = IdPrefixContext::default();
        let evaluator = RevsetExpressionEvaluator::new(
            self.repo.as_ref(),
            self.revset_extensions.clone(),
            &id_prefix_context,
            expression,
        );

        Ok(evaluator.resolve()?)
    }

    /// Commit a transaction and update the on-disk working copy to match.
    ///
    /// After transactions that rewrite commits (rebase, sign, etc.) the
    /// operation log records the new state but the working tree on disk still
    /// reflects the pre-rewrite commit.  This helper bridges the gap:
    ///
    /// 1. Resolves the old and new working-copy commits from the transaction.
    /// 2. For colocated repos, resets the git HEAD and exports refs.
    /// 3. Commits the transaction to the operation log.
    /// 4. Checks out the (potentially rewritten) working-copy commit on disk.
    ///
    /// Returns the committed `ReadonlyRepo` snapshot so callers can continue
    /// operating against the up-to-date state.
    pub(crate) fn commit_and_update_working_copy(
        &mut self,
        mut tx: Transaction,
        description: impl Into<String>,
    ) -> Result<Arc<ReadonlyRepo>, Box<dyn std::error::Error>> {
        let ws_name = self.workspace.workspace_name().to_owned();

        // Resolve old working-copy commit from the base repo (before the tx).
        let old_wc_commit = tx
            .base_repo()
            .view()
            .get_wc_commit_id(&ws_name)
            .map(|id| tx.base_repo().store().get_commit(id))
            .transpose()?;

        // Resolve new working-copy commit from the mutated repo (after rewrites).
        let new_wc_commit = tx
            .repo()
            .view()
            .get_wc_commit_id(&ws_name)
            .map(|id| tx.repo().store().get_commit(id))
            .transpose()?;

        // Colocated git repo: reset HEAD and export refs before committing.
        if jj_cli::git_util::is_colocated_git_workspace(&self.workspace, tx.base_repo()) {
            if let Some(wc_commit) = &new_wc_commit {
                // Errors updating HEAD are non-fatal — the actual state will
                // be imported on the next snapshot.
                if let Err(e) = jj_lib::git::reset_head(tx.repo_mut(), wc_commit) {
                    writeln!(self.ui.warning_default(), "{e}")?;
                }
            }
            let stats = jj_lib::git::export_refs(tx.repo_mut())?;
            jj_cli::git_util::print_git_export_stats(&self.ui, &stats)?;
        }

        // Commit the transaction to the operation log.
        let repo = tx.commit(description)?;

        // Check out the (possibly rewritten) working-copy tree on disk.
        if let Some(new_commit) = &new_wc_commit {
            let old_tree = old_wc_commit.as_ref().map(|c| c.tree());
            let stats =
                self.workspace
                    .check_out(repo.op_id().clone(), old_tree.as_ref(), new_commit)?;
            if stats.added_files > 0 || stats.updated_files > 0 || stats.removed_files > 0 {
                writeln!(
                    self.ui.status(),
                    "Added {} files, modified {} files, removed {} files",
                    stats.added_files,
                    stats.updated_files,
                    stats.removed_files,
                )?;
            }
        }

        Ok(repo)
    }
}

/// Build a [`Ui`] from default + user config, with optional CLI overrides.
///
/// Loads the jj config stack up to user-level (defaults → environment →
/// user config file), then applies `--no-pager` and `--color` overrides.
/// Workspace and repo config are **not** loaded — they may not exist when
/// the user asks for `--help` outside a jj repository.
///
/// This is the shared config/Ui bootstrap used by both the early `--help`
/// path (in `main.rs`) and the full [`SpiceEnv::init`] pipeline (via
/// [`load_config`]).
pub(crate) fn load_ui(
    no_pager: bool,
    color: Option<&str>,
) -> Result<(Ui, ConfigEnv, jj_cli::config::RawConfig), Box<dyn std::error::Error>> {
    let config_env = ConfigEnv::from_environment();
    let mut raw_config = config_from_environment(default_config_layers());
    config_env.reload_user_config(&mut raw_config)?;

    let mut cli_layer = ConfigLayer::empty(ConfigSource::CommandArg);
    if no_pager {
        cli_layer.set_value("ui.paginate", "never").unwrap();
    }
    if let Some(value) = color {
        cli_layer.set_value("ui.color", value).unwrap();
    }
    if !cli_layer.is_empty() {
        raw_config.as_mut().add_layer(cli_layer);
    }

    let config = config_env.resolve_config(&raw_config)?;
    let ui = Ui::with_config(&config).map_err(cmd_err)?;
    Ok((ui, config_env, raw_config))
}

/// Load the full jj config stack and locate the workspace root.
///
/// Builds on [`load_ui`] by additionally loading repo and workspace config,
/// applying `--config`, `--config-file`, `--quiet` overrides from
/// [`GlobalArgs`], and resolving the workspace root directory.
///
/// Returns the resolved config, UI, workspace root, and the [`ConfigEnv`] so
/// callers can later locate config files for writing.
fn load_config(
    cwd: &std::path::Path,
    global_args: &GlobalArgs,
) -> Result<
    (
        jj_lib::config::StackedConfig,
        Ui,
        std::path::PathBuf,
        ConfigEnv,
    ),
    Box<dyn std::error::Error>,
> {
    let early = &global_args.early_args;
    let (_, mut config_env, mut raw_config) = load_ui(
        early.no_pager.unwrap_or_default(),
        early.color.map(|c| c.to_string()).as_deref(),
    )?;

    // Resolve the workspace root: --repository overrides cwd-based discovery.
    let workspace_root = if let Some(repo_path) = &global_args.repository {
        let abs_path = cwd.join(repo_path);
        let abs_path = dunce::canonicalize(&abs_path).unwrap_or(abs_path);
        if !abs_path.join(".jj").is_dir() {
            return Err(format!("not a jj workspace: {path}", path = abs_path.display()).into());
        }
        abs_path
    } else {
        // find_workspace_dir returns cwd when no .jj is found — check explicitly.
        let root = find_workspace_dir(cwd);
        if !root.join(".jj").is_dir() {
            return Err("not a jj workspace (or any parent up to mount point)".into());
        }
        root.to_owned()
    };

    // Use jj-lib's workspace loader to resolve the actual repo directory.
    // In non-default workspaces `.jj/repo` is a file containing a relative
    // path to the shared repo, not a directory.  The loader follows the
    // indirection so `repo_path()` always returns the real directory.
    let loader = DefaultWorkspaceLoaderFactory
        .create(&workspace_root)
        .map_err(|e| format!("{e}"))?;
    config_env.reset_repo_path(loader.repo_path());
    config_env.reset_workspace_path(loader.workspace_root());

    // Temporary Ui for reload helpers (they may emit warnings).
    let tmp_config = config_env.resolve_config(&raw_config)?;
    let tmp_ui = Ui::with_config(&tmp_config).map_err(cmd_err)?;
    config_env
        .reload_repo_config(&tmp_ui, &mut raw_config)
        .map_err(cmd_err)?;
    config_env
        .reload_workspace_config(&tmp_ui, &mut raw_config)
        .map_err(cmd_err)?;

    // Apply --config and --config-file as CommandArg-priority layers.
    let config_args: Vec<(ConfigArgKind, &str)> = early
        .config
        .iter()
        .map(|s| (ConfigArgKind::Item, s.as_str()))
        .chain(
            early
                .config_file
                .iter()
                .map(|s| (ConfigArgKind::File, s.as_str())),
        )
        .collect();
    if !config_args.is_empty() {
        let layers = parse_config_args(&config_args).map_err(cmd_err)?;
        let stacked: &mut jj_lib::config::StackedConfig = raw_config.as_mut();
        for layer in layers {
            stacked.add_layer(layer);
        }
    }

    // Apply --quiet as highest-priority config override.
    // (--no-pager and --color are already applied by load_ui above.)
    if early.quiet.unwrap_or_default() {
        let mut layer = ConfigLayer::empty(ConfigSource::CommandArg);
        layer.set_value("ui.quiet", true).unwrap();
        raw_config.as_mut().add_layer(layer);
    }

    let config = config_env.resolve_config(&raw_config)?;
    let ui = Ui::with_config(&config).map_err(cmd_err)?;
    Ok((config, ui, workspace_root, config_env))
}

/// Convert a [`jj_cli::command_error::CommandError`] to a boxed std error.
///
/// `CommandError` does not implement `Display` or `std::error::Error`, so we
/// reach into its public `.error` field.
pub(crate) fn cmd_err(e: jj_cli::command_error::CommandError) -> Box<dyn std::error::Error> {
    Box::from(format!("{}", e.error))
}
