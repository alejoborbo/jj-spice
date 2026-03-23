use std::io::Write as _;

use jj_cli::config::{ConfigEnv, config_from_environment, default_config_layers};
use jj_lib::config::ConfigNamePathBuf;
use jj_lib::config::ConfigValue;

/// Aliases installed by this command.
///
/// Each entry is a `(config_name_path, toml_value_literal)` pair.
/// `config_name_path` is the dotted key under `[aliases]` in the jj config.
/// `toml_value_literal` is a valid TOML expression that parses into a
/// [`ConfigValue`].
const ALIASES: &[(&str, &str)] = &[
    (
        "aliases.stack",
        r#"["util", "exec", "--", "jj-spice", "stack"]"#,
    ),
    ("aliases.spice", r#"["util", "exec", "--", "jj-spice"]"#),
];

/// Run the `util install-aliases` command.
///
/// When `print` is true the TOML snippet is written to stdout and nothing is
/// persisted. Otherwise the aliases are written to the user-level jj config
/// file.
pub(crate) fn run(print: bool) -> Result<(), Box<dyn std::error::Error>> {
    if print {
        print_aliases()
    } else {
        write_aliases()
    }
}

/// Print the TOML alias snippet to stdout.
fn print_aliases() -> Result<(), Box<dyn std::error::Error>> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "[aliases]")?;
    for &(name, value) in ALIASES {
        // Strip the "aliases." prefix for display.
        let key = name
            .strip_prefix("aliases.")
            .expect("alias name must start with 'aliases.'");
        writeln!(out, "{key} = {value}")?;
    }
    Ok(())
}

/// Write the aliases to the user-level jj config file via jj-lib's
/// [`ConfigFile`] API.
fn write_aliases() -> Result<(), Box<dyn std::error::Error>> {
    let config_env = ConfigEnv::from_environment();
    let raw_config = config_from_environment(default_config_layers());

    let mut files = config_env
        .user_config_files(&raw_config)
        .map_err(|e| format!("{}", e.error))?;
    let file = files
        .last_mut()
        .ok_or("could not determine user config path for jj")?;

    for &(name, value_str) in ALIASES {
        let name_path: ConfigNamePathBuf = name
            .parse()
            .map_err(|e| format!("invalid config name '{name}': {e}"))?;
        let value: ConfigValue = value_str
            .parse()
            .map_err(|e| format!("invalid TOML value '{value_str}': {e}"))?;
        file.set_value(name_path, value)
            .map_err(|e| format!("failed to set {name}: {e}"))?;
    }

    file.save()
        .map_err(|e| format!("failed to save config: {e}"))?;

    eprintln!("Aliases written to {}", file.path().display());
    eprintln!();
    eprintln!("You can now use:");
    eprintln!("  jj stack log       (instead of jj-spice stack log)");
    eprintln!("  jj stack submit    (instead of jj-spice stack submit)");
    eprintln!("  jj stack sync      (instead of jj-spice stack sync)");
    eprintln!("  jj spice <cmd>     (instead of jj-spice <cmd>)");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_name_paths_are_valid() {
        for &(name, _) in ALIASES {
            let result: Result<ConfigNamePathBuf, _> = name.parse();
            assert!(result.is_ok(), "invalid config name path: {name}");
        }
    }

    #[test]
    fn alias_values_are_valid_toml() {
        for &(name, value_str) in ALIASES {
            let result: Result<ConfigValue, _> = value_str.parse();
            assert!(result.is_ok(), "invalid TOML value for {name}: {value_str}");
        }
    }

    #[test]
    fn alias_values_are_arrays() {
        for &(name, value_str) in ALIASES {
            let value: ConfigValue = value_str.parse().unwrap();
            assert!(
                value.as_array().is_some(),
                "expected array value for {name}, got: {value}"
            );
        }
    }

    #[test]
    fn alias_names_start_with_aliases_prefix() {
        for &(name, _) in ALIASES {
            assert!(
                name.starts_with("aliases."),
                "alias name must start with 'aliases.': {name}"
            );
        }
    }

    #[test]
    fn stack_alias_points_to_jj_spice_stack() {
        let (_, value_str) = ALIASES
            .iter()
            .find(|(name, _)| *name == "aliases.stack")
            .expect("stack alias must exist");
        let value: ConfigValue = value_str.parse().unwrap();
        let arr: Vec<&str> = value
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(arr, &["util", "exec", "--", "jj-spice", "stack"]);
    }

    #[test]
    fn spice_alias_points_to_jj_spice() {
        let (_, value_str) = ALIASES
            .iter()
            .find(|(name, _)| *name == "aliases.spice")
            .expect("spice alias must exist");
        let value: ConfigValue = value_str.parse().unwrap();
        let arr: Vec<&str> = value
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(arr, &["util", "exec", "--", "jj-spice"]);
    }
}
