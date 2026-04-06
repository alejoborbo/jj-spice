use std::fmt::Write as _;

use clap::builder::StyledStr;
use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Style as AnsiStyle};
use clap::{Args, Command, Parser, Subcommand};
use jj_cli::cli_util::GlobalArgs;

/// Colour theme matching jj-cli's help output.
///
/// Copied from jj-cli (where the constant is crate-private) so that
/// `jj-spice --help` looks visually consistent with `jj --help`.
const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Yellow.on_default().bold())
    .usage(AnsiColor::Yellow.on_default().bold())
    .literal(AnsiColor::Green.on_default().bold())
    .placeholder(AnsiColor::Green.on_default());

/// Style for section headers in after-help text (matches `STYLES` header).
const HEADER_STYLE: AnsiStyle = AnsiColor::Yellow.on_default().bold();
/// Style for config key literals in after-help text (matches `STYLES` literal).
const LITERAL_STYLE: AnsiStyle = AnsiColor::Green.on_default().bold();

/// Build a styled "Configuration:" after-help section for `--help` output.
///
/// Matches the indentation and layout of clap's built-in "Options:" section:
/// 6-space indent for key names, 10-space indent for description lines,
/// blank line between entries. ANSI codes are stripped automatically by
/// clap's output pipeline when colour is disabled.
fn config_after_help(entries: &[(&str, &str)]) -> StyledStr {
    let mut out = StyledStr::new();
    writeln!(out, "{HEADER_STYLE}Configuration:{HEADER_STYLE:#}").unwrap();

    for (i, &(key, desc)) in entries.iter().enumerate() {
        writeln!(out, "      {LITERAL_STYLE}{key}{LITERAL_STYLE:#}").unwrap();
        for line in desc.lines() {
            writeln!(out, "          {line}").unwrap();
        }
        // Blank line between entries, but not after the last one.
        if i + 1 < entries.len() {
            writeln!(out).unwrap();
        }
    }
    out
}

/// jj-spice: forge integration for jj.
#[derive(Parser, Clone, Debug)]
#[command(name = "jj-spice", styles = STYLES, version, about, long_about = None)]
pub struct Cli {
    #[command(flatten)]
    pub global_args: GlobalArgs,

    #[command(subcommand)]
    pub command: SpiceCommand,
}

/// Top-level subcommands exposed by jj-spice.
#[derive(Subcommand, Clone, Debug)]
pub enum SpiceCommand {
    /// Manage the bookmark stack.
    Stack(StackArgs),
    /// Miscellaneous utility commands.
    Util(UtilArgs),
}

/// Arguments for the `stack` subcommand group.
#[derive(Args, Clone, Debug)]
#[command(after_long_help = config_after_help(&[
    ("spice.output",                 "Terminal output fidelity: \"modern\" (default) or \"classic\""),
    ("spice.upstream-remote",        "Override the upstream remote name for fork workflows (string)"),
    ("spice.forges.<hostname>.type", "Forge type for a custom hostname: \"github\" or \"gitlab\""),
]))]
pub struct StackArgs {
    /// The stack operation to perform.
    #[command(subcommand)]
    pub command: StackCommand,
}

/// Operations available under `jj-spice stack`.
#[derive(Subcommand, Clone, Debug)]
pub enum StackCommand {
    /// Remove stale and inactive change request entries from local tracking.
    ///
    /// Stale entries are bookmarks that no longer exist in the repository.
    /// Inactive entries are change requests that are closed or merged on the
    /// forge. Both kinds are removed by default.
    Clean(CleanArgs),
    /// Show the bookmark DAG with change request status.
    Log(LogArgs),
    /// Submit the current stack of bookmarks for review.
    Submit(SubmitArgs),
    /// Discover and track existing change requests for bookmarks in the stack.
    Sync(SyncArgs),
    /// Stop tracking change requests for specific bookmarks.
    ///
    /// Removes the bookmark-to-CR mapping from local storage so that a new
    /// change request can be created on the next `stack submit`.
    Untrack(UntrackArgs),
}

/// Arguments for `jj-spice stack log`.
#[derive(Args, Clone, Debug)]
#[command(after_long_help = config_after_help(&[
    ("spice.output", "Terminal output fidelity: \"modern\" (default) or \"classic\".\n\
                      Controls status badge rendering and link format."),
]))]
pub struct LogArgs {
    /// Which revisions to show bookmarks for.
    ///
    /// Accepts any jj revset expression. Only bookmarks whose commit falls
    /// within the evaluated set are shown.
    ///
    /// Defaults to all local bookmarks (equivalent to `trunk()..bookmarks()`).
    /// Use `-r 'trunk()..@'` to see only the current stack.
    #[arg(short = 'r', long)]
    pub revisions: Option<String>,
}

/// Arguments for `jj-spice stack submit`.
#[derive(Args, Clone, Debug)]
#[command(after_long_help = config_after_help(&[
    ("spice.auto-accept-changes", "Skip the push-confirmation prompt (bool, default: false).\n\
                                   Equivalent to --auto-accept."),
    ("spice.auto-clean",          "Remove closed CRs from tracking automatically (bool, default: true)."),
]))]
pub struct SubmitArgs {
    /// Allow intactive (merged and closed) change requests to be tracked.
    ///
    /// Allow tracking closed and merged change requests when fetching them
    /// from remote.
    ///
    /// By default, jj-spice will only track change requests that are open, or in draft.
    #[arg(long, default_value_t = false)]
    pub allow_inactive: bool,
    /// Auto accepts pushing untracked changes.
    ///
    /// By default, the user will be prompted if some changes are untracked.
    ///
    /// Use `--auto-accept` or `--auto-accept=true` to enable,
    /// `--auto-accept=false` to disable (overrides config).
    ///
    /// [config: `spice.auto-accept-changes`]
    #[arg(
        long,
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
    )]
    pub auto_accept: Option<bool>,
    /// Auto track bookmarks
    ///
    /// When a bookmark has no remote, allow them to be tracked directly when submitting changes.
    #[arg(long)]
    pub auto_track_bookmarks: bool,
    /// Set the change request in draft state.
    ///
    /// By default, the user is prompted to choose the CR state.
    ///
    /// Use `--draft` or `--draft=true` for draft,
    /// `--draft=false` for non-draft.
    #[arg(
        long,
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
    )]
    pub draft: Option<bool>,
}

/// Arguments for `jj-spice stack sync`.
#[derive(Args, Clone, Debug)]
#[command(after_long_help = config_after_help(&[
    ("spice.auto-clean", "Remove stale/inactive CRs from tracking automatically (bool, default: true)."),
]))]
pub struct SyncArgs {
    /// Re-discover change requests even for bookmarks that are already tracked.
    #[arg(long)]
    pub force: bool,
    /// Allow inactive (merged and closed) change requests to be tracked.
    ///
    /// Allow tracking closed and merged change requests when fetching them
    /// from remote.
    ///
    /// By default, jj-spice will only track change requests that are open, or in draft.
    #[arg(long, default_value_t = false)]
    pub allow_inactive: bool,
    /// Restack the current stack onto trunk.
    ///
    /// By default, latest version of the trunk is fetched, but the stack is not rebased.
    #[arg(long)]
    pub restack: bool,
}

/// Arguments for `jj-spice stack untrack`.
#[derive(Args, Clone, Debug)]
pub struct UntrackArgs {
    /// Bookmark names to stop tracking.
    #[arg(required_unless_present = "all_inactive")]
    pub bookmarks: Vec<String>,
    /// Remove all entries whose change request is closed or merged on the forge.
    #[arg(long)]
    pub all_inactive: bool,
}

/// Arguments for `jj-spice stack clean`.
#[derive(Args, Clone, Debug)]
pub struct CleanArgs {
    /// Show what would be removed without making changes.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for the `util` subcommand group.
#[derive(Args, Clone, Debug)]
pub struct UtilArgs {
    /// The utility operation to perform.
    #[command(subcommand)]
    pub command: UtilCommand,
}

/// Operations available under `jj-spice util`.
#[derive(Subcommand, Clone, Debug)]
pub enum UtilCommand {
    /// Print a command-line-completion script.
    ///
    /// Apply it by running one of these:
    ///
    /// - Bash: `source <(jj-spice util completion bash)`
    /// - Fish: `jj-spice util completion fish | source`
    /// - Nushell:
    ///      ```nu
    ///      jj-spice util completion nushell | save -f "completions-jj-spice.nu"
    ///      use "completions-jj-spice.nu" *
    ///      ```
    /// - Zsh:
    ///      ```shell
    ///      autoload -U compinit
    ///      compinit
    ///      source <(jj-spice util completion zsh)
    ///      ```
    Completion(CompletionArgs),

    /// Register jj aliases so jj-spice commands can be invoked as jj
    /// subcommands.
    ///
    /// By default this writes the aliases directly to the user-level jj
    /// config file (`~/.config/jj/config.toml` or equivalent). Use `--print`
    /// to preview the TOML snippet without modifying any file.
    ///
    /// The following aliases are installed:
    ///
    /// - `jj stack <cmd>` → `jj-spice stack <cmd>`
    /// - `jj spice <cmd>` → `jj-spice <cmd>`
    InstallAliases(InstallAliasesArgs),
}

/// Arguments for `jj-spice util install-aliases`.
#[derive(Args, Clone, Debug)]
pub struct InstallAliasesArgs {
    /// Print the TOML snippet to stdout instead of writing to the jj config
    /// file.
    #[arg(long)]
    pub print: bool,
}

/// Arguments for `jj-spice util completion`.
#[derive(Args, Clone, Debug)]
pub struct CompletionArgs {
    /// The shell to generate completions for.
    pub shell: ShellCompletion,
}

/// Supported shells for completion script generation.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ShellCompletion {
    Bash,
    Elvish,
    Fish,
    Nushell,
    PowerShell,
    Zsh,
}

impl ShellCompletion {
    /// Generate a completion script for this shell from the given [`Command`].
    pub fn generate(self, cmd: &mut Command) -> Vec<u8> {
        use clap_complete::{Shell, generate};
        use clap_complete_nushell::Nushell;

        let bin_name = "jj-spice";
        let mut buf = Vec::new();
        match self {
            Self::Bash => generate(Shell::Bash, cmd, bin_name, &mut buf),
            Self::Elvish => generate(Shell::Elvish, cmd, bin_name, &mut buf),
            Self::Fish => generate(Shell::Fish, cmd, bin_name, &mut buf),
            Self::Nushell => generate(Nushell, cmd, bin_name, &mut buf),
            Self::PowerShell => generate(Shell::PowerShell, cmd, bin_name, &mut buf),
            Self::Zsh => generate(Shell::Zsh, cmd, bin_name, &mut buf),
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parse_stack_log() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "log"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Log(args),
            }) => assert!(args.revisions.is_none()),
            _ => panic!("expected Log"),
        }
    }

    #[test]
    fn parse_stack_log_with_revisions() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "log", "-r", "trunk()..@"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Log(args),
            }) => assert_eq!(args.revisions.as_deref(), Some("trunk()..@")),
            _ => panic!("expected Log"),
        }
    }

    #[test]
    fn parse_stack_log_with_long_revisions() {
        let cli = Cli::try_parse_from([
            "jj-spice",
            "stack",
            "log",
            "--revisions",
            "bookmarks() & mine()",
        ])
        .unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Log(args),
            }) => assert_eq!(args.revisions.as_deref(), Some("bookmarks() & mine()")),
            _ => panic!("expected Log"),
        }
    }

    #[test]
    fn parse_stack_submit() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "submit"]).unwrap();
        assert!(matches!(
            cli.command,
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Submit(_)
            })
        ));
    }

    // -- auto_accept tests --

    #[test]
    fn parse_submit_absent_auto_accept_is_none() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "submit"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Submit(args),
            }) => assert_eq!(args.auto_accept, None),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn parse_submit_bare_auto_accept_is_true() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "submit", "--auto-accept"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Submit(args),
            }) => assert_eq!(args.auto_accept, Some(true)),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn parse_submit_auto_accept_eq_true() {
        let cli =
            Cli::try_parse_from(["jj-spice", "stack", "submit", "--auto-accept=true"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Submit(args),
            }) => assert_eq!(args.auto_accept, Some(true)),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn parse_submit_auto_accept_eq_false() {
        let cli =
            Cli::try_parse_from(["jj-spice", "stack", "submit", "--auto-accept=false"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Submit(args),
            }) => assert_eq!(args.auto_accept, Some(false)),
            _ => panic!("expected Submit"),
        }
    }

    // -- draft tests --

    #[test]
    fn parse_submit_absent_draft_is_none() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "submit"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Submit(args),
            }) => assert_eq!(args.draft, None),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn parse_submit_bare_draft_is_true() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "submit", "--draft"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Submit(args),
            }) => assert_eq!(args.draft, Some(true)),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn parse_submit_draft_eq_false() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "submit", "--draft=false"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Submit(args),
            }) => assert_eq!(args.draft, Some(false)),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn parse_stack_sync_without_force() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "sync"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Sync(args),
            }) => assert!(!args.force),
            _ => panic!("expected Sync"),
        }
    }

    #[test]
    fn parse_stack_sync_with_force() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "sync", "--force"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Sync(args),
            }) => assert!(args.force),
            _ => panic!("expected Sync"),
        }
    }

    #[test]
    fn parse_no_args_fails() {
        assert!(Cli::try_parse_from(["jj-spice"]).is_err());
    }

    #[test]
    fn parse_unknown_subcommand_fails() {
        assert!(Cli::try_parse_from(["jj-spice", "stack", "unknown"]).is_err());
    }

    // ---- Global args integration tests ----

    #[test]
    fn parse_repository_short_flag() {
        let cli = Cli::try_parse_from(["jj-spice", "-R", "/tmp/repo", "stack", "submit"]).unwrap();
        assert_eq!(cli.global_args.repository.as_deref(), Some("/tmp/repo"));
    }

    #[test]
    fn parse_repository_long_flag() {
        let cli = Cli::try_parse_from(["jj-spice", "--repository", "/tmp/repo", "stack", "submit"])
            .unwrap();
        assert_eq!(cli.global_args.repository.as_deref(), Some("/tmp/repo"));
    }

    #[test]
    fn parse_at_operation() {
        let cli = Cli::try_parse_from(["jj-spice", "--at-operation", "abc123", "stack", "submit"])
            .unwrap();
        assert_eq!(cli.global_args.at_operation.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_at_op_alias() {
        let cli = Cli::try_parse_from(["jj-spice", "--at-op", "@-", "stack", "submit"]).unwrap();
        assert_eq!(cli.global_args.at_operation.as_deref(), Some("@-"));
    }

    #[test]
    fn parse_color_flag() {
        let cli = Cli::try_parse_from(["jj-spice", "--color", "never", "stack", "submit"]).unwrap();
        assert!(cli.global_args.early_args.color.is_some());
    }

    #[test]
    fn parse_no_pager_flag() {
        let cli = Cli::try_parse_from(["jj-spice", "--no-pager", "stack", "submit"]).unwrap();
        assert!(cli.global_args.early_args.no_pager.unwrap_or_default());
    }

    #[test]
    fn parse_quiet_flag() {
        let cli = Cli::try_parse_from(["jj-spice", "--quiet", "stack", "submit"]).unwrap();
        assert!(cli.global_args.early_args.quiet.unwrap_or_default());
    }

    #[test]
    fn parse_config_flag() {
        let cli =
            Cli::try_parse_from(["jj-spice", "--config", "ui.color=never", "stack", "submit"])
                .unwrap();
        assert_eq!(cli.global_args.early_args.config, vec!["ui.color=never"]);
    }

    #[test]
    fn parse_multiple_config_flags() {
        let cli = Cli::try_parse_from([
            "jj-spice",
            "--config",
            "ui.color=never",
            "--config",
            "user.email=test@example.com",
            "stack",
            "submit",
        ])
        .unwrap();
        assert_eq!(
            cli.global_args.early_args.config,
            vec!["ui.color=never", "user.email=test@example.com"]
        );
    }

    #[test]
    fn parse_debug_flag() {
        let cli = Cli::try_parse_from(["jj-spice", "--debug", "stack", "submit"]).unwrap();
        assert!(cli.global_args.debug);
    }

    #[test]
    fn parse_ignore_working_copy_flag() {
        let cli =
            Cli::try_parse_from(["jj-spice", "--ignore-working-copy", "stack", "submit"]).unwrap();
        assert!(cli.global_args.ignore_working_copy);
    }

    #[test]
    fn parse_global_args_after_subcommand() {
        // Global args should work after the subcommand too.
        let cli = Cli::try_parse_from([
            "jj-spice",
            "stack",
            "submit",
            "-R",
            "/tmp/repo",
            "--no-pager",
        ])
        .unwrap();
        assert_eq!(cli.global_args.repository.as_deref(), Some("/tmp/repo"));
        assert!(cli.global_args.early_args.no_pager.unwrap_or_default());
    }

    #[test]
    fn parse_defaults_are_none_or_false() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "submit"]).unwrap();
        assert!(cli.global_args.repository.is_none());
        assert!(cli.global_args.at_operation.is_none());
        assert!(!cli.global_args.debug);
        assert!(!cli.global_args.ignore_working_copy);
        assert!(!cli.global_args.ignore_immutable);
        assert!(cli.global_args.early_args.color.is_none());
        assert!(cli.global_args.early_args.config.is_empty());
        assert!(cli.global_args.early_args.config_file.is_empty());
    }

    // ---- Stack untrack tests ----

    #[test]
    fn parse_stack_untrack_single_bookmark() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "untrack", "feat-a"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Untrack(args),
            }) => {
                assert_eq!(args.bookmarks, vec!["feat-a"]);
                assert!(!args.all_inactive);
            }
            _ => panic!("expected Untrack"),
        }
    }

    #[test]
    fn parse_stack_untrack_multiple_bookmarks() {
        let cli =
            Cli::try_parse_from(["jj-spice", "stack", "untrack", "feat-a", "feat-b"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Untrack(args),
            }) => {
                assert_eq!(args.bookmarks, vec!["feat-a", "feat-b"]);
                assert!(!args.all_inactive);
            }
            _ => panic!("expected Untrack"),
        }
    }

    #[test]
    fn parse_stack_untrack_all_inactive() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "untrack", "--all-inactive"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Untrack(args),
            }) => {
                assert!(args.all_inactive);
                assert!(args.bookmarks.is_empty());
            }
            _ => panic!("expected Untrack"),
        }
    }

    #[test]
    fn parse_stack_untrack_no_args_fails() {
        assert!(Cli::try_parse_from(["jj-spice", "stack", "untrack"]).is_err());
    }

    // ---- Stack clean tests ----

    #[test]
    fn parse_stack_clean() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "clean"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Clean(args),
            }) => assert!(!args.dry_run),
            _ => panic!("expected Clean"),
        }
    }

    #[test]
    fn parse_stack_clean_dry_run() {
        let cli = Cli::try_parse_from(["jj-spice", "stack", "clean", "--dry-run"]).unwrap();
        match cli.command {
            SpiceCommand::Stack(StackArgs {
                command: StackCommand::Clean(args),
            }) => assert!(args.dry_run),
            _ => panic!("expected Clean"),
        }
    }

    // ---- Util completion tests ----

    #[test]
    fn parse_util_completion_bash() {
        let cli = Cli::try_parse_from(["jj-spice", "util", "completion", "bash"]).unwrap();
        match cli.command {
            SpiceCommand::Util(UtilArgs {
                command: UtilCommand::Completion(args),
            }) => assert_eq!(args.shell, ShellCompletion::Bash),
            _ => panic!("expected Util Completion"),
        }
    }

    #[test]
    fn parse_util_completion_zsh() {
        let cli = Cli::try_parse_from(["jj-spice", "util", "completion", "zsh"]).unwrap();
        match cli.command {
            SpiceCommand::Util(UtilArgs {
                command: UtilCommand::Completion(args),
            }) => assert_eq!(args.shell, ShellCompletion::Zsh),
            _ => panic!("expected Util Completion"),
        }
    }

    #[test]
    fn parse_util_completion_fish() {
        let cli = Cli::try_parse_from(["jj-spice", "util", "completion", "fish"]).unwrap();
        match cli.command {
            SpiceCommand::Util(UtilArgs {
                command: UtilCommand::Completion(args),
            }) => assert_eq!(args.shell, ShellCompletion::Fish),
            _ => panic!("expected Util Completion"),
        }
    }

    #[test]
    fn parse_util_completion_nushell() {
        let cli = Cli::try_parse_from(["jj-spice", "util", "completion", "nushell"]).unwrap();
        match cli.command {
            SpiceCommand::Util(UtilArgs {
                command: UtilCommand::Completion(args),
            }) => assert_eq!(args.shell, ShellCompletion::Nushell),
            _ => panic!("expected Util Completion"),
        }
    }

    #[test]
    fn parse_util_completion_elvish() {
        let cli = Cli::try_parse_from(["jj-spice", "util", "completion", "elvish"]).unwrap();
        match cli.command {
            SpiceCommand::Util(UtilArgs {
                command: UtilCommand::Completion(args),
            }) => assert_eq!(args.shell, ShellCompletion::Elvish),
            _ => panic!("expected Util Completion"),
        }
    }

    #[test]
    fn parse_util_completion_powershell() {
        let cli = Cli::try_parse_from(["jj-spice", "util", "completion", "power-shell"]).unwrap();
        match cli.command {
            SpiceCommand::Util(UtilArgs {
                command: UtilCommand::Completion(args),
            }) => assert_eq!(args.shell, ShellCompletion::PowerShell),
            _ => panic!("expected Util Completion"),
        }
    }

    #[test]
    fn parse_util_completion_missing_shell_fails() {
        assert!(Cli::try_parse_from(["jj-spice", "util", "completion"]).is_err());
    }

    #[test]
    fn parse_util_completion_invalid_shell_fails() {
        assert!(Cli::try_parse_from(["jj-spice", "util", "completion", "tcsh"]).is_err());
    }

    // ---- Util install-aliases tests ----

    #[test]
    fn parse_util_install_aliases() {
        let cli = Cli::try_parse_from(["jj-spice", "util", "install-aliases"]).unwrap();
        match cli.command {
            SpiceCommand::Util(UtilArgs {
                command: UtilCommand::InstallAliases(args),
            }) => assert!(!args.print),
            _ => panic!("expected Util InstallAliases"),
        }
    }

    #[test]
    fn parse_util_install_aliases_print() {
        let cli = Cli::try_parse_from(["jj-spice", "util", "install-aliases", "--print"]).unwrap();
        match cli.command {
            SpiceCommand::Util(UtilArgs {
                command: UtilCommand::InstallAliases(args),
            }) => assert!(args.print),
            _ => panic!("expected Util InstallAliases"),
        }
    }

    // ---- Completion script generation tests ----

    #[test]
    fn generate_completion_scripts_are_non_empty() {
        use clap::CommandFactory;
        for shell in [
            ShellCompletion::Bash,
            ShellCompletion::Elvish,
            ShellCompletion::Fish,
            ShellCompletion::Nushell,
            ShellCompletion::PowerShell,
            ShellCompletion::Zsh,
        ] {
            let mut cmd = Cli::command();
            let buf = shell.generate(&mut cmd);
            assert!(!buf.is_empty(), "{shell:?} completion script is empty");
            let script = String::from_utf8_lossy(&buf);
            assert!(
                script.contains("jj-spice"),
                "{shell:?} completion script should reference jj-spice"
            );
        }
    }
}
