use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::debug;

use crate::{cmd, git};
use which::{which, which_in};

/// Default script for cleaning up node_modules directories before worktree deletion.
/// This script moves node_modules to a temporary location and deletes them in the background,
/// making the workmux remove command return almost instantly.
const NODE_MODULES_CLEANUP_SCRIPT: &str = r#"#!/bin/bash
set -euo pipefail

# Create a temporary directory that will be cleaned up on script exit (success or error)
TRASH_DIR=$(mktemp -d)
trap 'rm -rf "$TRASH_DIR"' EXIT

# Find and move all node_modules directories
# -prune prevents descending into node_modules directories
find . -name "node_modules" -type d -prune -print0 | while IFS= read -r -d '' dir; do
  # Generate unique name from path: './frontend/node_modules' -> 'frontend_node_modules'
  unique_name=$(printf '%s\n' "${dir#./}" | tr '/' '_')

  if ! mv -- "$dir" "$TRASH_DIR/$unique_name"; then
    echo "Warning: Failed to move '$dir'. Check permissions." >&2
  fi
done

# Detach the final slow deletion from the script's execution
if [ -n "$(ls -A "$TRASH_DIR")" ]; then
  # Disown the trap and start a new background process for deletion
  trap - EXIT
  nohup rm -rf "$TRASH_DIR" >/dev/null 2>&1 &
fi
"#;

/// Configuration for file operations during worktree creation
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct FileConfig {
    /// Glob patterns for files to copy from the repo root to the new worktree
    #[serde(default)]
    pub copy: Option<Vec<String>>,

    /// Glob patterns for files to symlink from the repo root into the new worktree
    #[serde(default)]
    pub symlink: Option<Vec<String>>,
}

/// Configuration for agent status icons displayed in tmux window bar
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct StatusIcons {
    /// Icon shown when agent is working. Default: 🤖
    pub working: Option<String>,
    /// Icon shown when agent is waiting for input. Default: 💬
    pub waiting: Option<String>,
    /// Icon shown when agent is done. Default: ✅
    pub done: Option<String>,
}

impl StatusIcons {
    pub fn working(&self) -> &str {
        self.working.as_deref().unwrap_or("🤖")
    }

    pub fn waiting(&self) -> &str {
        self.waiting.as_deref().unwrap_or("💬")
    }

    pub fn done(&self) -> &str {
        self.done.as_deref().unwrap_or("✅")
    }
}

/// Configuration for the workmux tool, read from .workmux.yaml
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Config {
    /// The primary branch to merge into (optional, auto-detected if not set)
    #[serde(default)]
    pub main_branch: Option<String>,

    /// Directory where worktrees should be created (optional, defaults to <project>__worktrees pattern)
    /// Can be relative to repo root or absolute path
    #[serde(default)]
    pub worktree_dir: Option<String>,

    /// Prefix for tmux window names (optional, defaults to "wm-")
    #[serde(default)]
    pub window_prefix: Option<String>,

    /// Tmux pane configuration
    #[serde(default)]
    pub panes: Option<Vec<PaneConfig>>,

    /// Commands to run after creating the worktree
    #[serde(default)]
    pub post_create: Option<Vec<String>>,

    /// Commands to run before deleting the worktree (e.g., for fast cleanup)
    #[serde(default)]
    pub pre_delete: Option<Vec<String>>,

    /// The agent command to use (e.g., "claude", "gemini")
    #[serde(default)]
    pub agent: Option<String>,

    /// Default merge strategy for `workmux merge`
    #[serde(default)]
    pub merge_strategy: Option<MergeStrategy>,

    /// Strategy for deriving worktree/window names from branch names
    #[serde(default)]
    pub worktree_naming: WorktreeNaming,

    /// Prefix for worktree directory and window names
    #[serde(default)]
    pub worktree_prefix: Option<String>,

    /// File operations to perform after creating the worktree
    #[serde(default)]
    pub files: FileConfig,

    /// Whether to auto-apply workmux status to tmux window format.
    /// Default: true
    #[serde(default)]
    pub status_format: Option<bool>,

    /// Custom icons for agent status display.
    #[serde(default)]
    pub status_icons: StatusIcons,
}

/// Configuration for a single tmux pane
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PaneConfig {
    /// A command to run when the pane is created. The pane will remain open
    /// with an interactive shell after the command completes. If not provided,
    /// the pane will start with the default shell.
    #[serde(default)]
    pub command: Option<String>,

    /// Whether this pane should receive focus after creation
    #[serde(default)]
    pub focus: bool,

    /// Split direction from the previous pane (horizontal or vertical)
    #[serde(default)]
    pub split: Option<SplitDirection>,

    /// The size of the new pane in lines (for vertical splits) or cells (for horizontal splits).
    /// Mutually exclusive with `percentage`.
    #[serde(default)]
    pub size: Option<u16>,

    /// The size of the new pane as a percentage of the available space.
    /// Mutually exclusive with `size`.
    #[serde(default)]
    pub percentage: Option<u8>,

    /// The 0-based index of the pane to split.
    /// If not specified, splits the most recently created pane.
    /// Only used when `split` is specified.
    #[serde(default)]
    pub target: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MergeStrategy {
    #[default]
    Merge,
    Rebase,
    Squash,
}

/// Strategy for deriving worktree/window names from branch names
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeNaming {
    /// Use the full branch name (slashes become dashes after slugification)
    #[default]
    Full,
    /// Use only the part after the last `/` (e.g., `prj-123/feature` → `feature`)
    Basename,
}

impl WorktreeNaming {
    /// Derive a name from a branch name using this strategy
    pub fn derive_name(&self, branch: &str) -> String {
        match self {
            Self::Full => branch.to_string(),
            Self::Basename => branch
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(branch)
                .to_string(),
        }
    }
}

/// Validate pane configuration
pub fn validate_panes_config(panes: &[PaneConfig]) -> anyhow::Result<()> {
    for (i, pane) in panes.iter().enumerate() {
        if i == 0 {
            // First pane cannot have a split or size
            if pane.split.is_some() {
                anyhow::bail!("First pane (index 0) cannot have a 'split' direction.");
            }
            if pane.size.is_some() || pane.percentage.is_some() {
                anyhow::bail!("First pane (index 0) cannot have 'size' or 'percentage'.");
            }
        } else {
            // Subsequent panes must have a split
            if pane.split.is_none() {
                anyhow::bail!("Pane {} must have a 'split' direction specified.", i);
            }
        }

        // size and percentage are mutually exclusive
        if pane.size.is_some() && pane.percentage.is_some() {
            anyhow::bail!(
                "Pane {} cannot have both 'size' and 'percentage' specified.",
                i
            );
        }

        // Validate percentage range
        if let Some(p) = pane.percentage
            && !(1..=100).contains(&p)
        {
            anyhow::bail!(
                "Pane {} has invalid percentage {}. Must be between 1 and 100.",
                i,
                p
            );
        }

        // If target is specified, validate it's a valid index
        if let Some(target) = pane.target
            && target >= i
        {
            anyhow::bail!(
                "Pane {} has invalid target {}. Target must reference a previously created pane (0-{}).",
                i,
                target,
                i.saturating_sub(1)
            );
        }
    }
    Ok(())
}

impl Config {
    /// Load and merge global and project configurations.
    pub fn load(cli_agent: Option<&str>) -> anyhow::Result<Self> {
        debug!("config:loading");
        let global_config = Self::load_global()?.unwrap_or_default();
        let project_config = Self::load_project()?.unwrap_or_default();

        let final_agent = cli_agent
            .map(|s| s.to_string())
            .or_else(|| project_config.agent.clone())
            .or_else(|| global_config.agent.clone())
            .unwrap_or_else(|| "claude".to_string());

        let mut config = global_config.merge(project_config);
        config.agent = Some(final_agent);

        // After merging, apply sensible defaults for any values that are not configured.
        let needs_defaults = config.panes.is_none() || config.pre_delete.is_none();

        if needs_defaults {
            if let Ok(repo_root) = git::get_repo_root() {
                // Apply defaults that require inspecting the repository.

                // Default panes based on project type
                if config.panes.is_none() {
                    if repo_root.join("CLAUDE.md").exists() {
                        config.panes = Some(Self::claude_default_panes());
                    } else {
                        config.panes = Some(Self::default_panes());
                    }
                }

                // Default pre_delete hooks based on package manager
                if config.pre_delete.is_none() {
                    let has_node_modules = repo_root.join("pnpm-lock.yaml").exists()
                        || repo_root.join("package-lock.json").exists()
                        || repo_root.join("yarn.lock").exists();

                    if has_node_modules {
                        config.pre_delete = Some(vec![NODE_MODULES_CLEANUP_SCRIPT.to_string()]);
                    }
                }
            } else {
                // Apply fallback defaults for when not in a git repo (e.g., `workmux init`).
                if config.panes.is_none() {
                    config.panes = Some(Self::default_panes());
                }
            }
        }

        debug!(
            agent = ?config.agent,
            panes = config.panes.as_ref().map_or(0, |p| p.len()),
            "config:loaded"
        );
        Ok(config)
    }

    /// Load configuration from a specific path.
    fn load_from_path(path: &Path) -> anyhow::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        debug!(path = %path.display(), "config:reading file");
        let contents = fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&contents)
            .map_err(|e| anyhow::anyhow!("Failed to parse config at {}: {}", path.display(), e))?;
        Ok(Some(config))
    }

    /// Load the global configuration file from the XDG config directory.
    fn load_global() -> anyhow::Result<Option<Self>> {
        // Check ~/.config/workmux (XDG convention, works cross-platform)
        if let Some(home_dir) = home::home_dir() {
            let xdg_config_path = home_dir.join(".config/workmux/config.yaml");
            if xdg_config_path.exists() {
                return Self::load_from_path(&xdg_config_path);
            }
            let xdg_config_path_yml = home_dir.join(".config/workmux/config.yml");
            if xdg_config_path_yml.exists() {
                return Self::load_from_path(&xdg_config_path_yml);
            }
        }
        Ok(None)
    }

    /// Load the project-specific configuration file from the current directory.
    fn load_project() -> anyhow::Result<Option<Self>> {
        let config_path_yaml = Path::new(".workmux.yaml");
        if config_path_yaml.exists() {
            return Self::load_from_path(config_path_yaml);
        }
        let config_path_yml = Path::new(".workmux.yml");
        if config_path_yml.exists() {
            return Self::load_from_path(config_path_yml);
        }
        Ok(None)
    }

    /// Merge a project config into a global config.
    /// Project config takes precedence. For lists, "<global>" placeholder expands to global items.
    fn merge(self, project: Self) -> Self {
        // Helper to merge vectors with "<global>" placeholder expansion
        fn merge_vec_with_placeholder(
            global: Option<Vec<String>>,
            project: Option<Vec<String>>,
        ) -> Option<Vec<String>> {
            match (global, project) {
                (Some(global_items), Some(project_items)) => {
                    // Check if project items contain the "<global>" placeholder
                    let has_placeholder = project_items.iter().any(|s| s == "<global>");
                    if has_placeholder {
                        // Replace "<global>" with global items
                        let mut result = Vec::new();
                        for item in project_items {
                            if item == "<global>" {
                                result.extend(global_items.clone());
                            } else {
                                result.push(item);
                            }
                        }
                        Some(result)
                    } else {
                        // No placeholder, project completely replaces global
                        Some(project_items)
                    }
                }
                (global, project) => project.or(global),
            }
        }

        Self {
            // Scalar values: project wins
            main_branch: project.main_branch.or(self.main_branch),
            worktree_dir: project.worktree_dir.or(self.worktree_dir),
            window_prefix: project.window_prefix.or(self.window_prefix),
            agent: project.agent.or(self.agent),
            merge_strategy: project.merge_strategy.or(self.merge_strategy),

            // Worktree naming: project wins if not default
            worktree_naming: if project.worktree_naming != WorktreeNaming::default() {
                project.worktree_naming
            } else {
                self.worktree_naming
            },
            worktree_prefix: project.worktree_prefix.or(self.worktree_prefix),

            // Panes: project replaces global (no placeholder support)
            panes: project.panes.or(self.panes),

            // List values with placeholder support
            post_create: merge_vec_with_placeholder(self.post_create, project.post_create),
            pre_delete: merge_vec_with_placeholder(self.pre_delete, project.pre_delete),

            // File config with placeholder support
            files: FileConfig {
                copy: merge_vec_with_placeholder(self.files.copy, project.files.copy),
                symlink: merge_vec_with_placeholder(self.files.symlink, project.files.symlink),
            },

            // Status format: project overrides global, default true
            status_format: project.status_format.or(self.status_format),

            // Status icons: partial override (project icons override global per-field)
            status_icons: StatusIcons {
                working: project.status_icons.working.or(self.status_icons.working),
                waiting: project.status_icons.waiting.or(self.status_icons.waiting),
                done: project.status_icons.done.or(self.status_icons.done),
            },
        }
    }

    /// Get default panes.
    fn default_panes() -> Vec<PaneConfig> {
        vec![
            PaneConfig {
                command: None, // Default shell
                focus: true,
                split: None,
                size: None,
                percentage: None,
                target: None,
            },
            PaneConfig {
                command: Some("clear".to_string()),
                focus: false,
                split: Some(SplitDirection::Horizontal),
                size: None,
                percentage: None,
                target: None, // Splits most recent (pane 0)
            },
        ]
    }

    /// Get default panes for a Claude project.
    fn claude_default_panes() -> Vec<PaneConfig> {
        vec![
            PaneConfig {
                command: Some("<agent>".to_string()),
                focus: true,
                split: None,
                size: None,
                percentage: None,
                target: None,
            },
            PaneConfig {
                command: Some("clear".to_string()),
                focus: false,
                split: Some(SplitDirection::Horizontal),
                size: None,
                percentage: None,
                target: None, // Splits most recent (pane 0)
            },
        ]
    }

    /// Get the window prefix to use, defaulting to "wm-" if not configured
    pub fn window_prefix(&self) -> &str {
        self.window_prefix.as_deref().unwrap_or("wm-")
    }

    /// Create an example .workmux.yaml configuration file
    pub fn init() -> anyhow::Result<()> {
        use std::path::PathBuf;

        let config_path = PathBuf::from(".workmux.yaml");

        if config_path.exists() {
            return Err(anyhow::anyhow!(
                ".workmux.yaml already exists. Remove it first if you want to regenerate it."
            ));
        }

        let example_config = r#"# workmux project configuration
# For global settings, edit ~/.config/workmux/config.yaml

# The primary branch to merge into.
# Default: Auto-detected from remote's HEAD, or falls back to main or master.
# main_branch: main

# Custom directory where worktrees should be created.
# Can be relative to the repository root or an absolute path.
# Default: A sibling directory named '<project_name>__worktrees'.
# worktree_dir: .worktrees

# Custom prefix for tmux window names.
# window_prefix: wm-

# The agent command to use when <agent> is specified in pane commands.
# agent: claude

# Default merge strategy for `workmux merge`.
# Options: merge (default), rebase, squash
# CLI flags (--rebase, --squash) always override this setting.
# merge_strategy: rebase

# Commands to run in the new worktree before the tmux window is opened.
# These hooks block window creation, so reserve them for short tasks.
# For long-running setup (e.g., pnpm install), prefer pane `command`s instead.
# To disable, set to an empty list: `post_create: []`
# post_create:
  # Use "<global>" to inherit hooks from your global config.
  # - "<global>"
  # - mise use

# Cleanup commands run before worktree deletion
# Default: Auto-detects Node.js projects and fast-deletes node_modules in background
# You can override or disable this behavior:
# pre_delete:
#   - echo "Custom cleanup"
# Or disable:
# pre_delete: []

# Custom tmux pane layout for this project.
# Default: A two-pane layout with a shell and clear command
# panes:
#   # Run a long-running command like pnpm install; a shell remains afterward
#   - command: pnpm install
#     focus: true
#
#   # Just a default shell (command is omitted)
#   - split: horizontal
#
#   # Run a command that exits immediately
#   - command: clear
#     split: vertical

# File operations to perform when creating a worktree.
files:
  # Glob patterns for files to copy from the repo root.
  # Useful for files that need to be unique per worktree.
  # copy:
    # - .env

  # Glob patterns for files/directories to symlink from the repo root.
  # Ideal for shared resources like dependency caches to save disk space and time.
  # Use "<global>" to inherit patterns from your global config.
  symlink:
    # - "<global>"
    - node_modules
    # - .pnpm-store
"#;

        fs::write(&config_path, example_config)?;

        println!("✓ Created .workmux.yaml");
        println!("\nThis file provides project-specific overrides.");
        println!("For global settings, edit ~/.config/workmux/config.yaml");

        Ok(())
    }
}

/// Resolves an executable name or path to its full absolute path.
///
/// For absolute paths, returns as-is. For relative paths, resolves against current directory.
/// For plain executable names (e.g., "claude"), searches first in tmux's global PATH
/// (since panes will run in tmux's environment), then falls back to the current shell's PATH.
/// Returns None if the executable cannot be found.
pub fn resolve_executable_path(executable: &str) -> Option<String> {
    let exec_path = Path::new(executable);

    if exec_path.is_absolute() {
        return Some(exec_path.to_string_lossy().into_owned());
    }

    if executable.contains(std::path::MAIN_SEPARATOR)
        || executable.contains('/')
        || executable.contains('\\')
    {
        if let Ok(current_dir) = env::current_dir() {
            return Some(current_dir.join(exec_path).to_string_lossy().into_owned());
        }
    } else {
        if let Some(tmux_path) = tmux_global_path() {
            let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            if let Ok(found) = which_in(executable, Some(tmux_path.as_str()), &cwd) {
                return Some(found.to_string_lossy().into_owned());
            }
        }

        if let Ok(found) = which(executable) {
            return Some(found.to_string_lossy().into_owned());
        }
    }

    None
}

pub fn tmux_global_path() -> Option<String> {
    let output = cmd::Cmd::new("tmux")
        .args(&["show-environment", "-g", "PATH"])
        .run_and_capture_stdout()
        .ok()?;
    output.strip_prefix("PATH=").map(|s| s.to_string())
}

pub fn split_first_token(command: &str) -> Option<(&str, &str)> {
    let trimmed = command.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, "")),
    )
}

#[cfg(test)]
mod tests {
    use super::split_first_token;

    #[test]
    fn split_first_token_single_word() {
        assert_eq!(split_first_token("claude"), Some(("claude", "")));
    }

    #[test]
    fn split_first_token_with_args() {
        assert_eq!(
            split_first_token("claude --verbose"),
            Some(("claude", "--verbose"))
        );
    }

    #[test]
    fn split_first_token_multiple_spaces() {
        assert_eq!(
            split_first_token("claude   --verbose"),
            Some(("claude", "  --verbose"))
        );
    }

    #[test]
    fn split_first_token_leading_whitespace() {
        assert_eq!(
            split_first_token("  claude --verbose"),
            Some(("claude", "--verbose"))
        );
    }

    #[test]
    fn split_first_token_empty_string() {
        assert_eq!(split_first_token(""), None);
    }

    #[test]
    fn split_first_token_only_whitespace() {
        assert_eq!(split_first_token("   "), None);
    }
}
