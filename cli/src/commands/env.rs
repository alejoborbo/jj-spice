use std::sync::Arc;

use jj_cli::cli_util::{find_workspace_dir, GlobalArgs, RevisionArg};
use jj_cli::command_error::print_parse_diagnostics;
use jj_cli::config::{
    config_from_environment, default_config_layers, parse_config_args, ConfigArgKind, ConfigEnv,
};
use jj_cli::revset_util::{load_revset_aliases, RevsetExpressionEvaluator};
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::config::{ConfigLayer, ConfigSource};
use jj_lib::id_prefix::IdPrefixContext;
use jj_lib::op_walk;
use jj_lib::repo::{ReadonlyRepo, StoreFactories};
use jj_lib::repo_path::RepoPathUiConverter;
use jj_lib::revset::{
    RevsetAliasesMap, RevsetDiagnostics, RevsetExtensions, RevsetParseContext,
    RevsetWorkspaceContext,
};
use jj_lib::settings::UserSettings;
use jj_lib::workspace::{default_working_copy_factories, Workspace};

use jj_spice_lib::store::SpiceStore;

/// Shared context built once from the jj config pipeline and workspace.
pub(crate) struct SpiceEnv {
    /// Terminal UI handle for user-facing output and diagnostics.
    pub(crate) ui: Ui,
    /// Immutable repository snapshot at HEAD.
    pub(crate) repo: Arc<ReadonlyRepo>,
    /// Resolved user settings from the full jj config stack.
    pub(crate) settings: UserSettings,
    /// Open workspace (working copy + repo loader).
    pub(crate) workspace: Workspace,
    /// Configuration environment for locating config files (e.g. repo config).
    pub(crate) config_env: ConfigEnv,
    /// Immutable store for change requests.
    pub(crate) store: SpiceStore,
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

        Ok(Self {
            ui,
            repo,
            settings,
            workspace,
            config_env,
            path_converter,
            user_email,
            revset_aliases,
            revset_extensions,
            store,
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
}

/// Load the full jj config stack and locate the workspace root.
///
/// Uses jj-cli's public config pipeline: defaults → user → repo → workspace.
/// Applies command-line overrides from [`GlobalArgs`]:
/// - `--repository` (`-R`) overrides the workspace search path
/// - `--config NAME=VALUE` and `--config-file PATH` add config layers
/// - `--color`, `--quiet`, `--no-pager` override UI settings
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
    let mut config_env = ConfigEnv::from_environment();
    let mut raw_config = config_from_environment(default_config_layers());
    config_env.reload_user_config(&mut raw_config)?;

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

    // Inject repo/workspace paths so per-repo config and revset aliases load.
    config_env.reset_repo_path(&workspace_root.join(".jj").join("repo"));
    config_env.reset_workspace_path(&workspace_root);

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
    let early = &global_args.early_args;
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

    // Apply --color, --quiet, --no-pager as highest-priority config overrides.
    let mut cli_layer = ConfigLayer::empty(ConfigSource::CommandArg);
    if let Some(choice) = early.color {
        cli_layer.set_value("ui.color", choice.to_string()).unwrap();
    }
    if early.quiet.unwrap_or_default() {
        cli_layer.set_value("ui.quiet", true).unwrap();
    }
    if early.no_pager.unwrap_or_default() {
        cli_layer.set_value("ui.paginate", "never").unwrap();
    }
    if !cli_layer.data.is_empty() {
        raw_config.as_mut().add_layer(cli_layer);
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
