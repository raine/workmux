use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::debug;

use crate::{cmd, git, nerdfont};
use which::{which, which_in};

/// Default script for cleaning up node_modules directories before worktree deletion.
/// This script moves node_modules to a temporary location and deletes them in the background,
/// making the workmux remove command return almost instantly.
const NODE_MODULES_CLEANUP_SCRIPT: &str = include_str!("scripts/cleanup_node_modules.sh");

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

/// Configuration for LLM-based branch name generation
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct AutoNameConfig {
    /// Model to use with llm CLI (e.g., "gpt-4o-mini", "claude-3-5-sonnet").
    /// If not set, uses llm's default model.
    pub model: Option<String>,

    /// Custom system prompt for branch name generation.
    /// If not set, uses the default prompt that asks for a kebab-case branch name.
    pub system_prompt: Option<String>,
}

/// Configuration for dashboard actions (commit, merge keybindings)
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct DashboardConfig {
    /// Text to send to agent for commit action (c key).
    /// Default: "Commit staged changes with a descriptive message"
    pub commit: Option<String>,

    /// Text to send to agent for merge action (m key).
    /// Default: "!workmux merge"
    pub merge: Option<String>,

    /// Size of the preview pane as a percentage of terminal height (1-90).
    /// Default: 60 (60% for preview, 40% for table)
    pub preview_size: Option<u8>,
}

impl DashboardConfig {
    pub fn commit(&self) -> &str {
        self.commit
            .as_deref()
            .unwrap_or("Commit staged changes with a descriptive message")
    }

    pub fn merge(&self) -> &str {
        self.merge.as_deref().unwrap_or("!workmux merge")
    }

    /// Get the preview size percentage (clamped to 10-90).
    /// Default: 60
    pub fn preview_size(&self) -> u8 {
        self.preview_size.unwrap_or(60).clamp(10, 90)
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

    /// Commands to run before merging (e.g., linting, tests)
    #[serde(default)]
    pub pre_merge: Option<Vec<String>>,

    /// Commands to run before removing the worktree (e.g., for backups)
    #[serde(default)]
    pub pre_remove: Option<Vec<String>>,

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

    /// Configuration for LLM-based branch name generation
    #[serde(default)]
    pub auto_name: Option<AutoNameConfig>,

    /// Dashboard actions configuration
    #[serde(default)]
    pub dashboard: DashboardConfig,

    /// Whether to use nerdfont icons (None = prompt user on first run)
    #[serde(default)]
    pub nerdfont: Option<bool>,

    /// Target for tmux operations: window (default) or session
    #[serde(default)]
    pub target: TmuxTarget,
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

/// Target for tmux operations: create windows within the current session or create new sessions
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TmuxTarget {
    /// Create windows within the current tmux session (default)
    #[default]
    Window,
    /// Create new tmux sessions for each worktree
    Session,
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

/// Result of config discovery, including the relative path from repo root
#[derive(Debug, Clone)]
pub struct ConfigLocation {
    /// Absolute path to the config file
    pub config_path: PathBuf,
    /// Absolute path to the directory containing the config
    pub config_dir: PathBuf,
    /// Relative path from repo root to config dir (e.g., "backend" for backend/.workmux.yaml)
    /// Empty if config is at repo root
    pub rel_dir: PathBuf,
}

/// Find the nearest .workmux.yaml by walking up from start_dir to repo root.
/// Returns ConfigLocation with the relative path computed at discovery time.
pub fn find_project_config(start_dir: &Path) -> anyhow::Result<Option<ConfigLocation>> {
    let config_names = [".workmux.yaml", ".workmux.yml"];

    let repo_root = match git::get_repo_root_for(start_dir) {
        Ok(root) => root,
        Err(_) => return Ok(None),
    };

    // Canonicalize both paths to handle symlinks and ensure consistent comparison
    let repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    let mut dir = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());

    // Safety: ensure we're inside the repo
    if !dir.starts_with(&repo_root) {
        return Ok(None);
    }

    // Walk upward from start_dir to repo_root (inclusive)
    loop {
        for name in &config_names {
            let candidate = dir.join(name);
            if candidate.exists() {
                let rel_dir = dir
                    .strip_prefix(&repo_root)
                    .map(|p| p.to_path_buf())
                    .unwrap_or_default();
                debug!(
                    path = %candidate.display(),
                    rel_dir = %rel_dir.display(),
                    "config:found project config"
                );
                return Ok(Some(ConfigLocation {
                    config_path: candidate,
                    config_dir: dir,
                    rel_dir,
                }));
            }
        }
        if dir == repo_root {
            break;
        }
        if !dir.pop() {
            break;
        }
    }

    // Fallback: check main worktree root (preserves existing behavior for linked worktrees)
    if let Ok(main_root) = git::get_main_worktree_root() {
        let main_root = main_root.canonicalize().unwrap_or(main_root);
        if main_root != repo_root {
            for name in &config_names {
                let candidate = main_root.join(name);
                if candidate.exists() {
                    debug!(path = %candidate.display(), "config:found main-worktree config");
                    return Ok(Some(ConfigLocation {
                        config_path: candidate,
                        config_dir: main_root.clone(),
                        rel_dir: PathBuf::new(), // Main worktree root = empty rel_dir
                    }));
                }
            }
        }
    }

    Ok(None)
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
        if let Ok(repo_root) = git::get_repo_root() {
            // Apply defaults that require inspecting the repository.
            let has_node_modules = repo_root.join("pnpm-lock.yaml").exists()
                || repo_root.join("package-lock.json").exists()
                || repo_root.join("yarn.lock").exists();

            // Default panes based on project type
            if config.panes.is_none() {
                if repo_root.join("CLAUDE.md").exists() {
                    config.panes = Some(Self::claude_default_panes());
                } else {
                    config.panes = Some(Self::default_panes());
                }
            }

            // Default pre_remove hook for Node.js projects
            if config.pre_remove.is_none() && has_node_modules {
                config.pre_remove = Some(vec![NODE_MODULES_CLEANUP_SCRIPT.to_string()]);
            }
        } else {
            // Apply fallback defaults for when not in a git repo (e.g., `workmux init`).
            if config.panes.is_none() {
                config.panes = Some(Self::default_panes());
            }
        }

        debug!(
            agent = ?config.agent,
            panes = config.panes.as_ref().map_or(0, |p| p.len()),
            "config:loaded"
        );
        Ok(config)
    }

    /// Load and merge configs, returning the final config and project config location.
    /// The location indicates where the project config was found (for working dir calculation).
    pub fn load_with_location(
        cli_agent: Option<&str>,
    ) -> anyhow::Result<(Self, Option<ConfigLocation>)> {
        debug!("config:loading with location");
        let global_config = Self::load_global()?.unwrap_or_default();
        let (project_config, location) = Self::load_project_with_location()?;
        let project_config = project_config.unwrap_or_default();

        let final_agent = cli_agent
            .map(|s| s.to_string())
            .or_else(|| project_config.agent.clone())
            .or_else(|| global_config.agent.clone())
            .unwrap_or_else(|| "claude".to_string());

        let mut config = global_config.merge(project_config);
        config.agent = Some(final_agent);

        // Apply defaults - scope to config directory if nested config found
        let defaults_root = location
            .as_ref()
            .map(|loc| loc.config_dir.clone())
            .or_else(|| git::get_repo_root().ok())
            .unwrap_or_default();

        if !defaults_root.as_os_str().is_empty() {
            let has_node_modules = defaults_root.join("pnpm-lock.yaml").exists()
                || defaults_root.join("package-lock.json").exists()
                || defaults_root.join("yarn.lock").exists();

            if config.panes.is_none() {
                if defaults_root.join("CLAUDE.md").exists() {
                    config.panes = Some(Self::claude_default_panes());
                } else {
                    config.panes = Some(Self::default_panes());
                }
            }

            if config.pre_remove.is_none() && has_node_modules {
                config.pre_remove = Some(vec![NODE_MODULES_CLEANUP_SCRIPT.to_string()]);
            }
        } else if config.panes.is_none() {
            config.panes = Some(Self::default_panes());
        }

        debug!(
            agent = ?config.agent,
            panes = config.panes.as_ref().map_or(0, |p| p.len()),
            has_location = location.is_some(),
            "config:loaded with location"
        );
        Ok((config, location))
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

    /// Load project config and return its location.
    /// Returns (Config, Option<ConfigLocation>) - location is None if no config found.
    fn load_project_with_location() -> anyhow::Result<(Option<Self>, Option<ConfigLocation>)> {
        let start_dir = std::env::current_dir().unwrap_or_default();

        if let Some(location) = find_project_config(&start_dir)? {
            let config = Self::load_from_path(&location.config_path)?;
            return Ok((config, Some(location)));
        }

        Ok((None, None))
    }

    /// Load the project-specific configuration file.
    ///
    /// Searches for `.workmux.yaml` or `.workmux.yml` by walking upward from CWD:
    /// 1. Current directory up to repo root (finds nearest config)
    /// 2. Main worktree root (fallback for linked worktrees)
    fn load_project() -> anyhow::Result<Option<Self>> {
        let (config, _location) = Self::load_project_with_location()?;
        Ok(config)
    }

    /// Merge a project config into a global config.
    /// Project config takes precedence. For lists, "<global>" placeholder expands to global items.
    fn merge(self, project: Self) -> Self {
        /// Merge vectors with "<global>" placeholder expansion.
        /// When project contains "<global>", it expands to global items at that position.
        fn merge_vec_with_placeholder(
            global: Option<Vec<String>>,
            project: Option<Vec<String>>,
        ) -> Option<Vec<String>> {
            match (global, project) {
                (Some(global_items), Some(project_items)) => {
                    let has_placeholder = project_items.iter().any(|s| s == "<global>");
                    if has_placeholder {
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
                        Some(project_items)
                    }
                }
                (global, project) => project.or(global),
            }
        }

        /// Macro to merge Option fields where project overrides global.
        /// Reduces boilerplate for simple `project.field.or(self.field)` patterns.
        macro_rules! merge_options {
            ($global:expr, $project:expr, $($field:ident),+ $(,)?) => {
                Self {
                    $($field: $project.$field.or($global.$field),)+
                    ..Default::default()
                }
            };
        }

        // Merge simple Option<T> fields using the macro
        let mut merged = merge_options!(
            self,
            project,
            main_branch,
            worktree_dir,
            window_prefix,
            agent,
            merge_strategy,
            worktree_prefix,
            panes,
            status_format,
            auto_name,
            nerdfont,
        );

        // Special case: worktree_naming (project wins if not default)
        merged.worktree_naming = if project.worktree_naming != WorktreeNaming::default() {
            project.worktree_naming
        } else {
            self.worktree_naming
        };

        // Special case: target (project wins if not default)
        merged.target = if project.target != TmuxTarget::default() {
            project.target
        } else {
            self.target
        };

        // List values with "<global>" placeholder support
        merged.post_create = merge_vec_with_placeholder(self.post_create, project.post_create);
        merged.pre_merge = merge_vec_with_placeholder(self.pre_merge, project.pre_merge);
        merged.pre_remove = merge_vec_with_placeholder(self.pre_remove, project.pre_remove);

        // File config with placeholder support
        merged.files = FileConfig {
            copy: merge_vec_with_placeholder(self.files.copy, project.files.copy),
            symlink: merge_vec_with_placeholder(self.files.symlink, project.files.symlink),
        };

        // Status icons: per-field override
        merged.status_icons = StatusIcons {
            working: project.status_icons.working.or(self.status_icons.working),
            waiting: project.status_icons.waiting.or(self.status_icons.waiting),
            done: project.status_icons.done.or(self.status_icons.done),
        };

        // Dashboard actions: per-field override
        merged.dashboard = DashboardConfig {
            commit: project.dashboard.commit.or(self.dashboard.commit),
            merge: project.dashboard.merge.or(self.dashboard.merge),
            preview_size: project
                .dashboard
                .preview_size
                .or(self.dashboard.preview_size),
        };

        merged
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

    /// Get the window prefix to use.
    /// Priority: explicit window_prefix config > nerdfont icon > "wm-"
    pub fn window_prefix(&self) -> &str {
        if let Some(ref prefix) = self.window_prefix {
            prefix
        } else if nerdfont::is_enabled() {
            "\u{f418} " // nf-oct-git_branch
        } else {
            "wm-"
        }
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
# All options below are commented out - uncomment to override defaults.

#-------------------------------------------------------------------------------
# Git
#-------------------------------------------------------------------------------

# The primary branch to merge into.
# Default: Auto-detected from remote HEAD, falls back to main/master.
# main_branch: main

# Default merge strategy for `workmux merge`.
# Options: merge (default), rebase, squash
# CLI flags (--rebase, --squash) always override this.
# merge_strategy: rebase

#-------------------------------------------------------------------------------
# Naming & Paths
#-------------------------------------------------------------------------------

# Directory where worktrees are created.
# Can be relative to repo root or absolute.
# Default: Sibling directory '<project>__worktrees'.
# worktree_dir: .worktrees

# Strategy for deriving names from branch names.
# Options: full (default), basename (part after last '/').
# worktree_naming: basename

# Prefix added to worktree directories and tmux window names.
# worktree_prefix: ""

# Prefix for tmux window names.
# Default: "wm-"
# window_prefix: "wm-"

#-------------------------------------------------------------------------------
# Tmux
#-------------------------------------------------------------------------------

# Target for tmux operations: window (default) or session.
# - window: Create windows within the current tmux session
# - session: Create new tmux sessions for each worktree (useful for session-per-project workflows)
# target: session

# Custom tmux pane layout.
# Default: Two-pane layout with shell and clear command.
# panes:
#   - command: pnpm install
#     focus: true
#   - split: horizontal
#   - command: clear
#     split: vertical
#     size: 5

# Auto-apply agent status icons to tmux window format.
# Default: true
# status_format: true

# Custom icons for agent status display.
# status_icons:
#   working: "🤖"
#   waiting: "💬"
#   done: "✅"

#-------------------------------------------------------------------------------
# Agent & AI
#-------------------------------------------------------------------------------

# Agent command for '<agent>' placeholder in pane commands.
# Default: "claude"
# agent: claude

# LLM-based branch name generation (`workmux add -a`).
# auto_name:
#   model: "gpt-4o-mini"
#   system_prompt: "Generate a kebab-case git branch name."

#-------------------------------------------------------------------------------
# Hooks
#-------------------------------------------------------------------------------

# Commands to run in new worktree before tmux window opens.
# These block window creation - use for short tasks only.
# Use "<global>" to inherit from global config.
# Set to empty list to disable: `post_create: []`
# post_create:
#   - "<global>"
#   - mise use

# Commands to run before merging (e.g., linting, tests).
# Aborts the merge if any command fails.
# Use "<global>" to inherit from global config.
# Environment variables available:
#   - WM_BRANCH_NAME: The name of the branch being merged
#   - WM_TARGET_BRANCH: The name of the target branch (e.g., main)
#   - WM_WORKTREE_PATH: Absolute path to the worktree
#   - WM_PROJECT_ROOT: Absolute path of the main project directory
#   - WM_HANDLE: The worktree handle/window name
# pre_merge:
#   - "<global>"
#   - cargo test
#   - cargo clippy -- -D warnings

# Commands to run before worktree removal (during merge or remove).
# Useful for backing up gitignored files before cleanup.
# Default: Auto-detects Node.js projects and fast-deletes node_modules.
# Set to empty list to disable: `pre_remove: []`
# Environment variables available:
#   - WM_HANDLE: The worktree handle (directory name)
#   - WM_WORKTREE_PATH: Absolute path of the worktree being deleted
#   - WM_PROJECT_ROOT: Absolute path of the main project directory
# pre_remove:
#   - mkdir -p "$WM_PROJECT_ROOT/artifacts/$WM_HANDLE"
#   - cp -r test-results/ "$WM_PROJECT_ROOT/artifacts/$WM_HANDLE/"

#-------------------------------------------------------------------------------
# Files
#-------------------------------------------------------------------------------

# File operations when creating a worktree.
# files:
#   # Files to copy (useful for .env files that need to be unique).
#   copy:
#     - .env.local
#
#   # Files/directories to symlink (saves disk space, shares caches).
#   # Default: None.
#   # Use "<global>" to inherit from global config.
#   symlink:
#     - "<global>"
#     - node_modules

#-------------------------------------------------------------------------------
# Dashboard
#-------------------------------------------------------------------------------

# Actions for dashboard keybindings (c = commit, m = merge).
# Values are sent to the agent's pane. Use ! prefix for shell commands.
# Preview size (10-90): larger = more preview, less table. Use +/- keys to adjust.
# dashboard:
#   commit: "Commit staged changes with a descriptive message"
#   merge: "!workmux merge"
#   preview_size: 60
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

/// Checks if a command string corresponds to the given agent command.
///
/// Returns true if:
/// 1. The command is the literal placeholder "<agent>"
/// 2. The command's executable stem matches the agent's executable stem
///    (e.g., "claude" matches "/usr/bin/claude")
pub fn is_agent_command(command_line: &str, agent_command: &str) -> bool {
    let trimmed = command_line.trim();

    let Some((cmd_token, _)) = split_first_token(trimmed) else {
        return false;
    };

    // Allow <agent> token regardless of what follows (e.g., "<agent> --verbose")
    if cmd_token == "<agent>" {
        return true;
    }

    let Some((agent_token, _)) = split_first_token(agent_command) else {
        return false;
    };

    let resolved_cmd = resolve_executable_path(cmd_token).unwrap_or_else(|| cmd_token.to_string());
    let resolved_agent =
        resolve_executable_path(agent_token).unwrap_or_else(|| agent_token.to_string());

    let cmd_stem = Path::new(&resolved_cmd).file_stem();
    let agent_stem = Path::new(&resolved_agent).file_stem();

    cmd_stem.is_some() && cmd_stem == agent_stem
}

#[cfg(test)]
mod tests {
    use super::{is_agent_command, split_first_token};

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

    #[test]
    fn is_agent_command_placeholder() {
        assert!(is_agent_command("<agent>", "claude"));
        assert!(is_agent_command("  <agent>  ", "gemini"));
        // <agent> with arguments should also match
        assert!(is_agent_command("<agent> --verbose", "claude"));
        assert!(is_agent_command("<agent> -p foo", "gemini"));
    }

    #[test]
    fn is_agent_command_exact_match() {
        assert!(is_agent_command("claude", "claude"));
        assert!(is_agent_command("gemini", "gemini"));
    }

    #[test]
    fn is_agent_command_with_args() {
        assert!(is_agent_command("claude --verbose", "claude"));
        assert!(is_agent_command("gemini -i", "gemini --model foo"));
    }

    #[test]
    fn is_agent_command_mismatch() {
        assert!(!is_agent_command("claude", "gemini"));
        assert!(!is_agent_command("vim", "claude"));
        assert!(!is_agent_command("clear", "claude"));
    }

    #[test]
    fn is_agent_command_empty() {
        assert!(!is_agent_command("", "claude"));
        assert!(!is_agent_command("   ", "claude"));
    }

    use super::find_project_config;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn find_project_config_from_subdir() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Initialize git repo
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();

        // Create nested structure: root/backend/.workmux.yaml
        let backend = root.join("backend");
        fs::create_dir_all(&backend).unwrap();
        fs::write(backend.join(".workmux.yaml"), "agent: claude").unwrap();

        // Create deeper directory: root/backend/src
        let src = backend.join("src");
        fs::create_dir_all(&src).unwrap();

        // Find from src should find backend/.workmux.yaml
        let result = find_project_config(&src).unwrap();
        assert!(result.is_some());
        let loc = result.unwrap();
        assert!(loc.config_path.ends_with("backend/.workmux.yaml"));
        assert_eq!(loc.rel_dir, std::path::PathBuf::from("backend"));
    }

    #[test]
    fn find_project_config_nearest_wins() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Initialize git repo
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();

        // Create root config
        fs::write(root.join(".workmux.yaml"), "agent: root").unwrap();

        // Create nested config
        let backend = root.join("backend");
        fs::create_dir_all(&backend).unwrap();
        fs::write(backend.join(".workmux.yaml"), "agent: backend").unwrap();

        // Find from backend should find backend config, not root
        let result = find_project_config(&backend).unwrap();
        assert!(result.is_some());
        let loc = result.unwrap();
        assert!(loc.config_path.ends_with("backend/.workmux.yaml"));
    }
}
