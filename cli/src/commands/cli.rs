use clap::builder::Styles;
use clap::builder::styling::AnsiColor;
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
pub struct StackArgs {
    /// The stack operation to perform.
    #[command(subcommand)]
    pub command: StackCommand,
}

/// Operations available under `jj-spice stack`.
#[derive(Subcommand, Clone, Debug)]
pub enum StackCommand {
    /// Show the bookmark DAG with change request status.
    Log(LogArgs),
    /// Submit the current stack of bookmarks for review.
    Submit(SubmitArgs),
    /// Discover and track existing change requests for bookmarks in the stack.
    Sync(SyncArgs),
}

/// Arguments for `jj-spice stack log`.
#[derive(Args, Clone, Debug)]
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
    /// If the --auto-accept flag is passed, untracked changes will be automatically pushed.
    #[arg(long)]
    pub auto_accept: bool,
}

/// Arguments for `jj-spice stack sync`.
#[derive(Args, Clone, Debug)]
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
