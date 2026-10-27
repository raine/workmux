//! Global configuration management commands.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use std::fs;
use std::process::Command;

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Open the global configuration file in your editor ($VISUAL, $EDITOR, or vi)
    Edit,
    /// Print the path to the global configuration file
    Path,
    /// Print the default configuration reference with all options documented
    Reference,
}

pub fn run(args: ConfigArgs) -> Result<()> {
    match args.command {
        ConfigCommand::Edit => run_edit(),
        ConfigCommand::Path => run_path(),
        ConfigCommand::Reference => run_reference(),
    }
}

fn run_edit() -> Result<()> {
    let config_path =
        crate::config::global_config_path().context("Could not determine home directory")?;

    // Ensure directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }

    // Create default config if it doesn't exist
    if !config_path.exists() {
        fs::write(&config_path, DEFAULT_GLOBAL_CONFIG)
            .with_context(|| format!("Failed to create {}", config_path.display()))?;
        println!("Created {}", config_path.display());
    }

    // Determine editor: $VISUAL -> $EDITOR -> vi
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());

    // Split editor string to handle values like "code --wait"
    let parts: Vec<&str> = editor.split_whitespace().collect();
    let (cmd, args) = parts.split_first().context("Editor variable is empty")?;

    let status = Command::new(cmd)
        .args(args)
        .arg(&config_path)
        .status()
        .with_context(|| format!("Failed to open editor '{}'", editor))?;

    if !status.success() {
        bail!("Editor '{}' exited with non-zero status", editor);
    }

    Ok(())
}

fn run_path() -> Result<()> {
    let config_path =
        crate::config::global_config_path().context("Could not determine home directory")?;
    println!("{}", config_path.display());
    Ok(())
}

fn run_reference() -> Result<()> {
    print!("{}", crate::config::EXAMPLE_PROJECT_CONFIG);
    Ok(())
}

const DEFAULT_GLOBAL_CONFIG: &str = r#"# workmux global configuration
# Settings here apply to all projects. Project-specific .workmux.yaml overrides these.
# See: https://workmux.dev/guide/configuration

# nerdfont: true
# agent: claude
# merge_strategy: rebase
# merge_keep: true
# confirm_unmerged_removal: false
#
# panes:
#   - command: <agent>
#     focus: true
#   - split: horizontal
#
# sandbox:
#   host_commands: ["just", "cargo"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_global_config_is_valid_yaml() {
        let result: Result<crate::config::Config, _> = serde_yaml::from_str(DEFAULT_GLOBAL_CONFIG);
        assert!(
            result.is_ok(),
            "Default global config is not valid YAML: {:?}",
            result.err()
        );
    }
}
