mod commands;

use std::io::Write as _;

use clap::{CommandFactory, Parser};
use jj_cli::ui::Ui;

use commands::cli::Cli;
use commands::env::load_ui;

fn main() {
    let exit_code = run();
    std::process::exit(exit_code);
}

fn run() -> i32 {
    // When the COMPLETE env var is set, act as a dynamic shell completion
    // engine and exit. Otherwise this is a no-op and execution continues.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    // Build a Ui *before* full workspace init so that --help and --version
    // output can flow through the user's configured pager, matching
    // `jj --help` behaviour.  Uses the same config pipeline (defaults +
    // user config + CLI overrides) as SpiceEnv::init().
    let args: Vec<String> = std::env::args().collect();
    let no_pager = has_flag(&args, "--no-pager");
    let color = flag_value(&args, "--color");

    let mut ui = match load_ui(no_pager, color) {
        Ok((ui, ..)) => ui,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    match Cli::try_parse() {
        Ok(cli) => {
            if let Err(e) = commands::run(cli) {
                eprintln!("error: {e}");
                return 1;
            }
            0
        }
        Err(err) => {
            let code = handle_clap_error(&mut ui, &err);
            ui.finalize_pager();
            code
        }
    }
}

/// Check whether `flag` (e.g. `"--no-pager"`) appears anywhere in `args`.
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// Return the value following `flag` (e.g. `"--color" "never"`), or the
/// value after `=` (e.g. `"--color=never"`).
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().map(|s| s.as_str());
        }
        if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
            return Some(value);
        }
    }
    None
}

/// Handle a clap parse error (--help, --version, or real errors).
///
/// For `--help` output the pager is engaged so long help text can be
/// scrolled, matching `jj --help` behaviour.
fn handle_clap_error(ui: &mut Ui, err: &clap::Error) -> i32 {
    let clap_str = if ui.color() {
        err.render().ansi().to_string()
    } else {
        err.render().to_string()
    };

    match err.kind() {
        clap::error::ErrorKind::DisplayHelp
        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            ui.request_pager();
        }
        _ => {}
    }

    match err.kind() {
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
            let _ = write!(ui.stdout(), "{clap_str}");
            0
        }
        _ => {
            let _ = write!(ui.stderr(), "{clap_str}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_flag_present() {
        let args = vec![
            "jj-spice".into(),
            "--no-pager".into(),
            "stack".into(),
            "log".into(),
        ];
        assert!(has_flag(&args, "--no-pager"));
    }

    #[test]
    fn has_flag_absent() {
        let args = vec!["jj-spice".into(), "stack".into(), "log".into()];
        assert!(!has_flag(&args, "--no-pager"));
    }

    #[test]
    fn flag_value_space_separated() {
        let args = vec![
            "jj-spice".into(),
            "--color".into(),
            "never".into(),
            "stack".into(),
            "log".into(),
        ];
        assert_eq!(flag_value(&args, "--color"), Some("never"));
    }

    #[test]
    fn flag_value_equals_syntax() {
        let args = vec![
            "jj-spice".into(),
            "--color=always".into(),
            "stack".into(),
            "log".into(),
        ];
        assert_eq!(flag_value(&args, "--color"), Some("always"));
    }

    #[test]
    fn flag_value_absent() {
        let args = vec!["jj-spice".into(), "stack".into(), "log".into()];
        assert_eq!(flag_value(&args, "--color"), None);
    }

    #[test]
    fn flag_value_no_value_after_flag() {
        let args = vec!["jj-spice".into(), "--color".into()];
        assert_eq!(flag_value(&args, "--color"), None);
    }
}
