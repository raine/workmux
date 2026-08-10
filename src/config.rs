use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::debug;

use crate::util::{PROJECT_PLACEHOLDER, expand_project_placeholder};
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
    /// Custom command to use instead of `llm` for branch name generation.
    /// The command string is split into program and arguments (e.g., "claude -p").
    /// The composed prompt is piped via stdin at execution time.
    /// When set, `model` is ignored.
    pub command: Option<String>,

    /// Model to use with llm CLI (e.g., "gpt-4o-mini", "claude-3-5-sonnet").
    /// If not set, uses llm's default model. Ignored when `command` is set.
    pub model: Option<String>,

    /// Custom system prompt for branch name generation.
    /// If not set, uses the default prompt that asks for a kebab-case branch name.
    pub system_prompt: Option<String>,

    /// Whether to always run in background mode when using --auto-name.
    /// If true, the window will be created but not focused.
    pub background: Option<bool>,
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

    /// Show check pass/total counts alongside check icon (default: false)
    #[serde(default)]
    pub show_check_counts: Option<bool>,

    /// Columns of the agents table, in display order. Columns left out of the
    /// list are not rendered.
    /// Default: number, project, worktree, git, pr, status, time, title.
    pub agent_columns: Option<Vec<AgentColumn>>,

    /// Default sort mode for the agent list: "priority", "project", "recency", "natural".
    /// Used when the dashboard has no persisted sort preference.
    /// Default: "priority"
    #[serde(default)]
    pub sort_mode: Option<String>,

    /// Close the dashboard after jumping to an agent or worktree.
    /// Default: true.
    pub close_on_jump: Option<bool>,
}

/// A configurable column of the dashboard agents table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentColumn {
    /// Jump key of the row, shown under the `#` header.
    Number,
    /// Project name.
    Project,
    /// Worktree name, with a pane suffix for multi-pane windows.
    Worktree,
    /// Git status of the worktree.
    Git,
    /// Pull request and check status. Rendered only while at least one agent
    /// has GitHub status to show.
    Pr,
    /// Agent status (icons).
    Status,
    /// Time elapsed in the current status.
    Time,
    /// Agent pane title.
    Title,
}

/// Columns used when the config does not set `dashboard.agent_columns`.
pub const DEFAULT_AGENT_COLUMNS: [AgentColumn; 8] = [
    AgentColumn::Number,
    AgentColumn::Project,
    AgentColumn::Worktree,
    AgentColumn::Git,
    AgentColumn::Pr,
    AgentColumn::Status,
    AgentColumn::Time,
    AgentColumn::Title,
];

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

    /// Columns of the agents table, in display order. Duplicates are dropped so
    /// a column is never rendered twice, and an empty or absent list falls back
    /// to the default order.
    pub fn agent_columns(&self) -> Vec<AgentColumn> {
        let Some(configured) = self.agent_columns.as_ref() else {
            return DEFAULT_AGENT_COLUMNS.to_vec();
        };

        let mut seen = Vec::new();
        for column in configured {
            if !seen.contains(column) {
                seen.push(*column);
            }
        }

        if seen.is_empty() {
            DEFAULT_AGENT_COLUMNS.to_vec()
        } else {
            seen
        }
    }

    /// Whether to show check pass/total counts alongside check icons.
    /// Default: false
    pub fn show_check_counts(&self) -> bool {
        self.show_check_counts.unwrap_or(false)
    }

    /// Whether the dashboard closes after jumping to an agent or worktree.
    /// Default: true.
    pub fn close_on_jump(&self) -> bool {
        self.close_on_jump.unwrap_or(true)
    }
}

/// Per-mode template strings for sidebar rendering.
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct TemplatesConfig {
    /// Single-line template for compact mode.
    pub compact: Option<String>,
    /// Multi-line templates for tile mode (one string per line).
    pub tiles: Option<Vec<String>>,
    /// Multi-line templates for horizontal bar chips (one string per line).
    #[serde(alias = "top")]
    pub horizontal: Option<Vec<String>>,
}

/// Detailed per-agent icon override: `{ icon, color }`.
///
/// `deny_unknown_fields` catches typos like `colour:` instead of silently
/// dropping them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIconDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

/// Per-agent icon and color override.
///
/// Backwards compatible: a bare string (`claude: "C"`) parses as `Plain`.
/// Detailed form (`claude: { icon: "C", color: "#d97757" }`) parses as
/// `Detailed`. Bare key with no value (`claude:`) parses as `Null` and
/// behaves like no override.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum AgentIconConfig {
    Plain(String),
    Detailed(AgentIconDetails),
    Null,
}

impl AgentIconConfig {
    /// Icon override, if any.
    pub fn icon(&self) -> Option<&str> {
        match self {
            Self::Plain(s) => Some(s.as_str()),
            Self::Detailed(d) => d.icon.as_deref(),
            Self::Null => None,
        }
    }

    /// Color override, if any. The string is unparsed; callers parse and
    /// validate at config-load time.
    pub fn color(&self) -> Option<&str> {
        match self {
            Self::Plain(_) | Self::Null => None,
            Self::Detailed(d) => d.color.as_deref(),
        }
    }
}

/// Per-agent icon overrides. Maps agent kind (e.g. "claude", "codex") to
/// either a bare icon string or `{ icon, color }`.
pub type AgentIcons = BTreeMap<String, AgentIconConfig>;

/// Configuration for horizontal sidebar rendering.
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct HorizontalSidebarConfig {
    /// Maximum width of each horizontal item in columns. Default: 24.
    pub item_width: Option<u16>,
}

impl HorizontalSidebarConfig {
    pub fn item_width(&self) -> usize {
        self.item_width.unwrap_or(24).clamp(12, 80) as usize
    }
}

/// Configuration for the sidebar.
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct SidebarConfig {
    /// Position of the sidebar pane. Default: left.
    pub position: Option<SidebarPosition>,

    /// Width of the left sidebar. Can be an absolute column count (e.g. 40)
    /// or a percentage of terminal width (e.g. "15%").
    /// Default: "10%" (clamped to 25-50 columns)
    pub width: Option<SidebarWidth>,

    /// Height of the top sidebar. Can be an absolute row count (e.g. 3)
    /// or a percentage of terminal height (e.g. "10%").
    /// Default: "10%" (clamped to 1-5 rows)
    pub height: Option<SidebarHeight>,

    /// Layout mode: "compact" or "tiles". Default: "tiles"
    pub layout: Option<String>,

    /// Horizontal bar configuration.
    #[serde(default)]
    pub horizontal: HorizontalSidebarConfig,

    /// Custom templates for sidebar rendering.
    pub templates: Option<TemplatesConfig>,

    /// Per-agent icon overrides.
    pub agent_icons: Option<AgentIcons>,

    /// Row ordering: "recency" (default) or "window".
    pub sort: Option<SidebarSort>,

    /// Dim agents whose status has not changed for the stale threshold.
    /// Default: true.
    pub dim_stale: Option<bool>,
}

impl SidebarConfig {
    pub fn dim_stale(&self) -> bool {
        self.dim_stale.unwrap_or(true)
    }
}

/// Sidebar row ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SidebarSort {
    /// Most recent status change first, sleeping agents last.
    #[default]
    Recency,
    /// Multiplexer window order (session name, then window index). Keeps
    /// rows stable while agents work, matching the tmux window list.
    Window,
}

/// Sidebar pane position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum, Default)]
#[serde(rename_all = "lowercase")]
pub enum SidebarPosition {
    #[default]
    Left,
    Top,
}

/// Sidebar width: either absolute columns or a percentage of terminal width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarWidth {
    Absolute(u16),
    Percent(u16),
}

impl SidebarWidth {
    /// Resolve to an absolute column count given the terminal width.
    pub fn resolve(&self, terminal_width: u16) -> u16 {
        match self {
            SidebarWidth::Absolute(w) => *w,
            SidebarWidth::Percent(p) => {
                if terminal_width == 0 {
                    25
                } else {
                    terminal_width * p / 100
                }
            }
        }
    }
}

impl Serialize for SidebarWidth {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            SidebarWidth::Absolute(w) => serializer.serialize_u16(*w),
            SidebarWidth::Percent(p) => serializer.serialize_str(&format!("{}%", p)),
        }
    }
}

macro_rules! impl_dimension_deserialize {
    ($name:ident, $dimension:literal, $expect:literal) => {
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                use serde::de;

                struct Visitor;

                impl<'de> de::Visitor<'de> for Visitor {
                    type Value = $name;

                    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                        formatter.write_str($expect)
                    }

                    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                        Ok($name::Absolute(v as u16))
                    }

                    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                        if v < 0 {
                            return Err(de::Error::custom(concat!(
                                $dimension,
                                " cannot be negative"
                            )));
                        }
                        Ok($name::Absolute(v as u16))
                    }

                    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                        if let Some(pct) = v.strip_suffix('%') {
                            let p: u16 = pct
                                .trim()
                                .parse()
                                .map_err(|_| de::Error::custom("invalid percentage"))?;
                            if p == 0 || p > 100 {
                                return Err(de::Error::custom("percentage must be 1-100"));
                            }
                            Ok($name::Percent(p))
                        } else {
                            let val: u16 = v
                                .trim()
                                .parse()
                                .map_err(|_| de::Error::custom(concat!("invalid ", $dimension)))?;
                            Ok($name::Absolute(val))
                        }
                    }
                }

                deserializer.deserialize_any(Visitor)
            }
        }
    };
}

impl_dimension_deserialize!(
    SidebarWidth,
    "width",
    "a number (columns) or a string like \"15%\""
);

impl std::str::FromStr for SidebarWidth {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_sidebar_dimension(value, "width").map(|value| match value {
            SidebarDimension::Absolute(value) => Self::Absolute(value),
            SidebarDimension::Percent(value) => Self::Percent(value),
        })
    }
}

/// Sidebar height: either absolute rows or a percentage of terminal height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarHeight {
    Absolute(u16),
    Percent(u16),
}

impl SidebarHeight {
    /// Resolve to an absolute row count given the terminal height.
    pub fn resolve(&self, terminal_height: u16) -> u16 {
        match self {
            SidebarHeight::Absolute(h) => *h,
            SidebarHeight::Percent(p) => terminal_height * p / 100,
        }
    }
}

impl Serialize for SidebarHeight {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            SidebarHeight::Absolute(h) => serializer.serialize_u16(*h),
            SidebarHeight::Percent(p) => serializer.serialize_str(&format!("{}%", p)),
        }
    }
}

impl_dimension_deserialize!(
    SidebarHeight,
    "height",
    "a number (rows) or a string like \"10%\""
);

impl std::str::FromStr for SidebarHeight {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_sidebar_dimension(value, "height").map(|value| match value {
            SidebarDimension::Absolute(value) => Self::Absolute(value),
            SidebarDimension::Percent(value) => Self::Percent(value),
        })
    }
}

/// Shared parser for command-line sidebar dimensions.
fn parse_sidebar_dimension(value: &str, name: &str) -> Result<SidebarDimension, String> {
    if let Some(percent) = value.strip_suffix('%') {
        let percent = percent
            .trim()
            .parse::<u16>()
            .map_err(|_| format!("invalid {name} percentage"))?;
        if percent == 0 || percent > 100 {
            return Err(format!("{name} percentage must be 1-100"));
        }
        Ok(SidebarDimension::Percent(percent))
    } else {
        let value = value
            .trim()
            .parse::<u16>()
            .map_err(|_| format!("invalid {name}"))?;
        Ok(SidebarDimension::Absolute(value))
    }
}

enum SidebarDimension {
    Absolute(u16),
    Percent(u16),
}

/// Configuration for a single window within a session (session mode only)
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WindowConfig {
    /// Optional window name. If omitted, tmux auto-names based on running command.
    #[serde(default)]
    pub name: Option<String>,

    /// Panes within this window. Same schema as top-level `panes`.
    #[serde(default)]
    pub panes: Option<Vec<PaneConfig>>,
}

/// Configuration for the workmux tool, read from .workmux.yaml
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Config {
    /// The primary branch to merge into (optional, auto-detected if not set)
    #[serde(default)]
    pub main_branch: Option<String>,

    /// Default base branch/commit to branch from when creating new worktrees.
    /// The value "auto" uses the effective main branch.
    /// Used as fallback when --base is not passed to `workmux add`.
    #[serde(default)]
    pub base_branch: Option<String>,

    /// Directory where worktrees should be created (optional, defaults to <project>__worktrees pattern)
    /// Can be relative to repo root or absolute path
    #[serde(default)]
    pub worktree_dir: Option<String>,

    /// Prefix for tmux window names (optional, defaults to "wm-")
    /// Supports a `{project}` placeholder for the main worktree's directory name.
    #[serde(default)]
    pub window_prefix: Option<String>,

    /// Tmux pane configuration (single window layout, mutually exclusive with `windows`)
    #[serde(default)]
    pub panes: Option<Vec<PaneConfig>>,

    /// Named pane layouts, selectable with `-l/--layout`.
    #[serde(default)]
    pub layouts: Option<HashMap<String, LayoutConfig>>,

    /// Multiple window configuration (session mode only, mutually exclusive with `panes`)
    #[serde(default)]
    pub windows: Option<Vec<WindowConfig>>,

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

    /// Keep worktree, window, and branch by default after `workmux merge`
    #[serde(default)]
    pub merge_keep: Option<bool>,

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

    /// Sidebar configuration
    #[serde(default)]
    pub sidebar: SidebarConfig,

    /// Whether to use nerdfont icons (None = prompt user on first run)
    #[serde(default)]
    pub nerdfont: Option<bool>,

    /// Color theme for the dashboard
    #[serde(default)]
    pub theme: ThemeConfig,

    /// Mode for tmux operations: window (default) or session
    #[serde(default)]
    /// None means "use default" (Window), Some means explicitly set
    pub mode: Option<MuxMode>,

    /// Placement for new tmux windows in window mode.
    #[serde(default)]
    pub window_placement: Option<WindowPlacement>,

    /// Automatically check for updates in the background. Default: true
    #[serde(default)]
    pub auto_update_check: Option<bool>,

    /// Write prompt files without injecting into agent commands.
    /// Useful when your editor has an embedded agent that reads prompt files directly.
    #[serde(default)]
    pub prompt_file_only: Option<bool>,

    /// Named agent commands. Maps short names to command strings or
    /// `{ command, type }` objects. Global-only for security.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentEntry>,

    /// Resolved agent type override from the agents map.
    /// Set internally during config loading, not deserialized.
    #[serde(skip)]
    pub agent_type: Option<String>,

    /// Selected agent name before resolving through the agents map.
    /// Set internally during config loading, not deserialized.
    #[serde(skip)]
    pub selected_agent: Option<String>,

    /// Container sandbox configuration
    #[serde(default)]
    pub sandbox: SandboxConfig,
}

/// A named agent environment value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentEnvValue {
    Literal(String),
    FromEnv {
        #[serde(rename = "from_env")]
        from_env: String,
    },
}

impl AgentEnvValue {
    pub fn shell_value(&self) -> String {
        match self {
            Self::Literal(value) => crate::multiplexer::agent::shell_quote(value),
            Self::FromEnv { from_env } => format!("\"${}\"", from_env),
        }
    }
}

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A named agent entry: either a plain command string or a structured launch profile.
///
/// Deserializes from:
/// - `"claude --flags"` (string shorthand)
/// - `{ command: "/path/to/wrapper", type: "claude" }` (explicit type override)
/// - `{ type: "claude", args: ["-p"], env: { KEY: value } }`
#[derive(Debug, Clone, Serialize)]
pub struct AgentEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Explicit agent type override for profile detection.
    /// When set, profile resolution uses this instead of the executable stem.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Arguments appended after the executable, before any injected prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables set for this agent process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, AgentEnvValue>,
}

impl AgentEntry {
    pub fn command_or_default(&self, name: &str) -> String {
        self.command
            .clone()
            .or_else(|| self.agent_type.clone())
            .unwrap_or_else(|| name.to_string())
    }
}

impl<'de> Deserialize<'de> for AgentEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawEntry {
            String(String),
            Map {
                #[serde(default)]
                command: Option<String>,
                #[serde(rename = "type")]
                agent_type: Option<String>,
                #[serde(default)]
                args: Vec<String>,
                #[serde(default)]
                env: BTreeMap<String, AgentEnvValue>,
            },
        }

        match RawEntry::deserialize(deserializer)? {
            RawEntry::String(s) => Ok(AgentEntry {
                command: Some(s),
                agent_type: None,
                args: Vec::new(),
                env: BTreeMap::new(),
            }),
            RawEntry::Map {
                command,
                agent_type,
                args,
                env,
            } => {
                for (key, value) in &env {
                    if !is_valid_env_name(key) {
                        return Err(serde::de::Error::custom(format!(
                            "agent env key must be a valid environment variable name: {key}"
                        )));
                    }
                    if let AgentEnvValue::FromEnv { from_env } = value
                        && !is_valid_env_name(from_env)
                    {
                        return Err(serde::de::Error::custom(format!(
                            "agent env from_env value must be a valid environment variable name: {from_env}"
                        )));
                    }
                }
                Ok(AgentEntry {
                    command,
                    agent_type,
                    args,
                    env,
                })
            }
        }
    }
}

/// Configuration for a single multiplexer pane
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct PaneConfig {
    /// Optional display name for the pane.
    ///
    /// Supported by the Zellij backend. Other backends may ignore this value.
    #[serde(default)]
    pub name: Option<String>,

    /// A command to run when the pane is created. The pane will remain open
    /// with an interactive shell after the command completes. If not provided,
    /// the pane will start with the default shell.
    #[serde(default)]
    pub command: Option<String>,

    /// Whether this pane should receive focus after creation
    #[serde(default)]
    pub focus: bool,

    /// Split direction from the previous pane (horizontal, vertical, or zellij-only stacked)
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

    /// Whether this pane should be zoomed (fullscreen) after creation.
    /// Implies `focus: true`.
    #[serde(default)]
    pub zoom: bool,
}

/// A named pane layout, selectable with `-l/--layout` at add-time.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LayoutConfig {
    /// Pane configuration for this layout.
    pub panes: Vec<PaneConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
    Stacked,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MergeStrategy {
    #[default]
    Merge,
    Rebase,
    Squash,
}

/// Dark or light mode for the dashboard
#[derive(Debug, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

impl<'de> serde::Deserialize<'de> for ThemeMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.to_lowercase().as_str() {
            "light" => Ok(ThemeMode::Light),
            _ => Ok(ThemeMode::Dark),
        }
    }
}

/// Named color scheme for the dashboard
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThemeScheme {
    #[default]
    Default,
    Emberforge,
    GlacierSignal,
    ObsidianPop,
    SlateGarden,
    PhosphorArcade,
    Lasergrid,
    Mossfire,
    NightSorbet,
    GraphiteCode,
    FestivalCircuit,
    TealDrift,
}

impl ThemeScheme {
    pub const ALL: [ThemeScheme; 12] = [
        ThemeScheme::Default,
        ThemeScheme::Emberforge,
        ThemeScheme::GlacierSignal,
        ThemeScheme::ObsidianPop,
        ThemeScheme::SlateGarden,
        ThemeScheme::PhosphorArcade,
        ThemeScheme::Lasergrid,
        ThemeScheme::Mossfire,
        ThemeScheme::NightSorbet,
        ThemeScheme::GraphiteCode,
        ThemeScheme::FestivalCircuit,
        ThemeScheme::TealDrift,
    ];

    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|&s| s == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    #[allow(dead_code)]
    pub fn name(&self) -> &'static str {
        match self {
            ThemeScheme::Default => "Default",
            ThemeScheme::Emberforge => "Emberforge",
            ThemeScheme::GlacierSignal => "Glacier Signal",
            ThemeScheme::ObsidianPop => "Obsidian Pop",
            ThemeScheme::SlateGarden => "Slate Garden",
            ThemeScheme::PhosphorArcade => "Phosphor Arcade",
            ThemeScheme::Lasergrid => "Lasergrid",
            ThemeScheme::Mossfire => "Mossfire",
            ThemeScheme::NightSorbet => "Night Sorbet",
            ThemeScheme::GraphiteCode => "Graphite Code",
            ThemeScheme::FestivalCircuit => "Festival Circuit",
            ThemeScheme::TealDrift => "Teal Drift",
        }
    }

    pub fn slug(&self) -> &'static str {
        match self {
            ThemeScheme::Default => "default",
            ThemeScheme::Emberforge => "emberforge",
            ThemeScheme::GlacierSignal => "glacier-signal",
            ThemeScheme::ObsidianPop => "obsidian-pop",
            ThemeScheme::SlateGarden => "slate-garden",
            ThemeScheme::PhosphorArcade => "phosphor-arcade",
            ThemeScheme::Lasergrid => "lasergrid",
            ThemeScheme::Mossfire => "mossfire",
            ThemeScheme::NightSorbet => "night-sorbet",
            ThemeScheme::GraphiteCode => "graphite-code",
            ThemeScheme::FestivalCircuit => "festival-circuit",
            ThemeScheme::TealDrift => "teal-drift",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().find(|v| v.slug() == s).copied()
    }
}

impl serde::Serialize for ThemeScheme {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.slug())
    }
}

impl<'de> serde::Deserialize<'de> for ThemeScheme {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self::from_slug(&s.to_lowercase()).unwrap_or_default())
    }
}

/// Custom color overrides for the theme palette.
/// Each field corresponds to a `ThemePalette` field and accepts a CSS hex color (e.g. "#51afef").
/// Shorthand aliases: `bg` for `current_row_bg`, `fg` for `text`, `error` for `danger`.
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct CustomThemeColors {
    #[serde(default, alias = "bg")]
    pub current_row_bg: Option<String>,
    #[serde(default)]
    pub highlight_row_bg: Option<String>,
    #[serde(default)]
    pub current_worktree_fg: Option<String>,
    #[serde(default)]
    pub dimmed: Option<String>,
    #[serde(default, alias = "fg")]
    pub text: Option<String>,
    #[serde(default)]
    pub border: Option<String>,
    #[serde(default)]
    pub help_border: Option<String>,
    #[serde(default)]
    pub help_muted: Option<String>,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub keycap: Option<String>,
    #[serde(default)]
    pub info: Option<String>,
    #[serde(default)]
    pub success: Option<String>,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default, alias = "error")]
    pub danger: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
}

/// Theme configuration: scheme + optional mode override + custom color overrides.
/// Supports deserializing from:
///   - `theme: emberforge` (scheme name, auto-detect mode)
///   - `theme: dark` or `theme: light` (legacy mode override)
///   - `theme: { scheme: emberforge, mode: dark }` (structured)
///   - `theme: { scheme: emberforge, custom: { accent: "#51afef" } }` (with overrides)
#[derive(Debug, Serialize, Clone, Default, PartialEq, Eq)]
pub struct ThemeConfig {
    pub scheme: ThemeScheme,
    /// None = auto-detect from terminal background
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ThemeMode>,
    /// Custom color overrides applied on top of the base scheme
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomThemeColors>,
}

impl<'de> serde::Deserialize<'de> for ThemeConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de;

        struct ThemeVisitor;

        impl<'de> de::Visitor<'de> for ThemeVisitor {
            type Value = ThemeConfig;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a theme scheme name, \"dark\", \"light\", or a {scheme, mode} map")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<ThemeConfig, E> {
                let lower = v.to_lowercase();
                match lower.as_str() {
                    "dark" => Ok(ThemeConfig {
                        scheme: ThemeScheme::Default,
                        mode: Some(ThemeMode::Dark),
                        custom: None,
                    }),
                    "light" => Ok(ThemeConfig {
                        scheme: ThemeScheme::Default,
                        mode: Some(ThemeMode::Light),
                        custom: None,
                    }),
                    _ => Ok(ThemeConfig {
                        scheme: ThemeScheme::from_slug(&lower).unwrap_or_default(),
                        mode: None,
                        custom: None,
                    }),
                }
            }

            fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<ThemeConfig, M::Error> {
                let mut scheme = None;
                let mut mode = None;
                let mut custom = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "scheme" => {
                            let s: String = map.next_value()?;
                            scheme = ThemeScheme::from_slug(&s.to_lowercase());
                        }
                        "mode" => {
                            let s: String = map.next_value()?;
                            mode = Some(match s.to_lowercase().as_str() {
                                "light" => ThemeMode::Light,
                                _ => ThemeMode::Dark,
                            });
                        }
                        "custom" => {
                            custom = Some(map.next_value::<CustomThemeColors>()?);
                        }
                        _ => {
                            let _ = map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(ThemeConfig {
                    scheme: scheme.unwrap_or_default(),
                    mode,
                    custom,
                })
            }
        }

        d.deserialize_any(ThemeVisitor)
    }
}

/// Placement for new tmux windows in window mode.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WindowPlacement {
    /// Place the new window immediately after the calling window.
    #[default]
    AfterCurrent,
    /// Place the new window after the rightmost window in the current session.
    Rightmost,
}

/// Mode for multiplexer operations: create windows within the current session or create new sessions
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MuxMode {
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

/// Sandbox backend type
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxBackend {
    /// Docker/Podman containers (default)
    #[default]
    Container,
    /// Lima VM backend
    Lima,
}

/// Container runtime for sandbox
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxRuntime {
    /// Docker (default fallback when neither runtime is found in PATH)
    #[default]
    Docker,
    /// Podman
    Podman,
    /// Apple Container (macOS only, uses `container` binary)
    #[serde(rename = "apple-container")]
    AppleContainer,
}

impl SandboxRuntime {
    /// Auto-detect container runtime by checking PATH.
    ///
    /// On macOS, prefers Apple Container (`container`) over Docker/Podman.
    /// The `container` probe is gated behind macOS since the generic binary name
    /// could false-positive on Linux. Falls back to Docker if nothing is found
    /// (will fail later with a clear "command not found" error).
    pub fn detect() -> Self {
        #[cfg(target_os = "macos")]
        if which("container").is_ok() {
            return SandboxRuntime::AppleContainer;
        }

        if which("docker").is_ok() {
            SandboxRuntime::Docker
        } else if which("podman").is_ok() {
            SandboxRuntime::Podman
        } else {
            debug!("no container runtime found in PATH, defaulting to docker");
            SandboxRuntime::Docker
        }
    }

    /// Returns the binary name for this runtime.
    pub fn binary_name(&self) -> &'static str {
        match self {
            SandboxRuntime::Docker => "docker",
            SandboxRuntime::Podman => "podman",
            SandboxRuntime::AppleContainer => "container",
        }
    }

    /// Human-readable name for user-facing messages.
    pub fn display_name(&self) -> &'static str {
        match self {
            SandboxRuntime::Docker => "docker",
            SandboxRuntime::Podman => "podman",
            SandboxRuntime::AppleContainer => "apple-container",
        }
    }

    /// Whether this runtime needs `--add-host host.docker.internal:host-gateway`.
    /// Only Docker requires this.
    pub fn needs_add_host(&self) -> bool {
        matches!(self, SandboxRuntime::Docker)
    }

    /// Whether this runtime needs `--userns=keep-id`.
    /// Only Podman requires this.
    pub fn needs_userns_keep_id(&self) -> bool {
        matches!(self, SandboxRuntime::Podman)
    }

    /// Whether this runtime needs `--cap-add=NET_ADMIN` and `--security-opt
    /// no-new-privileges` in network deny mode. Apple Container runs each
    /// container as a full VM where root already has all capabilities.
    pub fn needs_deny_mode_caps(&self) -> bool {
        matches!(self, SandboxRuntime::Docker | SandboxRuntime::Podman)
    }

    /// Whether this runtime supports binding individual files (not just directories).
    /// Apple Container only supports directory mounts via virtiofs.
    pub fn supports_file_mounts(&self) -> bool {
        !matches!(self, SandboxRuntime::AppleContainer)
    }

    /// Returns the arguments for pulling an image.
    /// Apple Container uses `image pull`, others use `pull`.
    pub fn pull_args(&self, image: &str) -> Vec<String> {
        match self {
            SandboxRuntime::AppleContainer => {
                vec!["image".into(), "pull".into(), image.into()]
            }
            _ => vec!["pull".into(), image.into()],
        }
    }

    /// Returns the default hostname that a container guest should use to reach the host.
    ///
    /// - Docker: `host.docker.internal` (Docker Desktop built-in)
    /// - Podman: `host.containers.internal` (Podman built-in)
    /// - Apple Container: `192.168.64.1` (default gateway for Apple VMs)
    pub fn rpc_host_address(&self) -> &'static str {
        match self {
            SandboxRuntime::Docker => "host.docker.internal",
            SandboxRuntime::Podman => "host.containers.internal",
            SandboxRuntime::AppleContainer => "192.168.64.1",
        }
    }

    /// Returns the default memory limit for this runtime, if one should be applied
    /// when the user hasn't configured an explicit value.
    ///
    /// Apple Container defaults to 1 GB RAM per VM which is insufficient for most
    /// workloads. Since memory is a ceiling (not an upfront allocation), a generous
    /// default is safe. Docker/Podman use host resources directly and don't need this.
    pub fn default_memory(&self) -> Option<&'static str> {
        match self {
            SandboxRuntime::AppleContainer => Some("16G"),
            _ => None,
        }
    }

    /// Returns the serde name for this runtime (used for state store serialization).
    pub fn serde_name(&self) -> &'static str {
        match self {
            SandboxRuntime::Docker => "docker",
            SandboxRuntime::Podman => "podman",
            SandboxRuntime::AppleContainer => "apple-container",
        }
    }

    /// Parse a runtime from its serde name. Returns None for unrecognized values.
    pub fn from_serde_name(s: &str) -> Option<Self> {
        match s {
            "docker" => Some(SandboxRuntime::Docker),
            "podman" => Some(SandboxRuntime::Podman),
            "apple-container" => Some(SandboxRuntime::AppleContainer),
            _ => None,
        }
    }
}

/// Isolation level for Lima backend
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum IsolationLevel {
    /// Single shared VM for all projects (fastest)
    Shared,
    /// One VM per git repository (default, balanced)
    #[default]
    Project,
}

/// Which panes to sandbox
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxTarget {
    /// Only sandbox agent panes (default, recommended)
    #[default]
    Agent,
    /// Sandbox all panes
    All,
}

/// Toolchain integration mode for Lima sandboxes.
/// Controls whether devbox.json/flake.nix are detected and used
/// to wrap agent commands with the appropriate environment.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolchainMode {
    /// Auto-detect devbox.json or flake.nix and wrap commands (default)
    #[default]
    Auto,
    /// Disable toolchain integration
    Off,
    /// Force Devbox mode (use devbox.json)
    Devbox,
    /// Force Nix flake mode (use flake.nix)
    Flake,
}

/// An extra mount point for the sandbox.
///
/// Supports two forms:
/// - Simple string: `"~/my-notes"` (read-only, mirrored path)
/// - Detailed spec: `{ host_path: "~/data", guest_path: "/mnt/data", writable: true }`
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(untagged)]
pub enum ExtraMount {
    /// Simple host path (read-only, guest path mirrors host path)
    Path(String),
    /// Detailed mount specification
    Spec {
        host_path: String,
        #[serde(default)]
        guest_path: Option<String>,
        #[serde(default)]
        writable: Option<bool>,
    },
}

impl ExtraMount {
    /// Resolve the mount to (host_path, guest_path, read_only).
    /// Expands `~` in host_path to the user's home directory.
    /// Returns an error if host_path or guest_path is not absolute after expansion.
    pub fn resolve(&self) -> anyhow::Result<(PathBuf, PathBuf, bool)> {
        let (host_str, guest_str, writable) = match self {
            Self::Path(p) => (p.as_str(), None, false),
            Self::Spec {
                host_path,
                guest_path,
                writable,
            } => (
                host_path.as_str(),
                guest_path.as_deref(),
                writable.unwrap_or(false),
            ),
        };

        let host_path = crate::util::expand_tilde(host_str);
        if !host_path.is_absolute() {
            anyhow::bail!(
                "extra_mounts: host path must be absolute (got '{}'). Use an absolute path or ~/.",
                host_str
            );
        }

        let guest_path = guest_str
            .map(PathBuf::from)
            .unwrap_or_else(|| host_path.clone());
        if !guest_path.is_absolute() {
            anyhow::bail!(
                "extra_mounts: guest_path must be absolute (got '{}')",
                guest_str.unwrap_or("")
            );
        }

        let read_only = !writable;
        Ok((host_path, guest_path, read_only))
    }
}

/// Lima-specific sandbox configuration.
/// Nested under `sandbox.lima` in YAML.
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct LimaConfig {
    /// Isolation level. Default: project
    #[serde(default)]
    pub isolation: Option<IsolationLevel>,

    /// Projects directory for shared isolation (required when isolation: shared)
    #[serde(default)]
    pub projects_dir: Option<PathBuf>,

    /// Number of CPUs for Lima VMs. Default: 4 (Lima default)
    #[serde(default)]
    pub cpus: Option<u32>,

    /// Memory for Lima VMs (e.g. "4GiB", "8GiB"). Default: "4GiB" (Lima default)
    #[serde(default)]
    pub memory: Option<String>,

    /// Disk size for Lima VMs (e.g. "100GiB"). Default: "100GiB" (Lima default)
    #[serde(default)]
    pub disk: Option<String>,

    /// Custom user provision script run once during Lima VM creation,
    /// after built-in system and user provisioning steps.
    /// Runs as user (not root). Use `sudo` for system-level commands.
    #[serde(default)]
    pub provision: Option<String>,

    /// Skip built-in provisioning scripts (system dependencies and tool installation).
    /// Useful when using a custom image that already has everything pre-installed.
    /// Custom `provision` script still runs if specified.
    #[serde(default)]
    pub skip_default_provision: Option<bool>,

    /// Accept Lima's reduced Git metadata isolation without an interactive prompt.
    #[serde(default)]
    pub accept_reduced_git_metadata_isolation: Option<bool>,
}

impl LimaConfig {
    pub fn isolation(&self) -> IsolationLevel {
        self.isolation.clone().unwrap_or_default()
    }

    pub fn cpus(&self) -> u32 {
        self.cpus.unwrap_or(4)
    }

    pub fn memory(&self) -> &str {
        self.memory.as_deref().unwrap_or("4GiB")
    }

    pub fn disk(&self) -> &str {
        self.disk.as_deref().unwrap_or("100GiB")
    }

    pub fn provision_script(&self) -> Option<&str> {
        self.provision.as_deref().filter(|s| !s.trim().is_empty())
    }

    pub fn skip_default_provision(&self) -> bool {
        self.skip_default_provision.unwrap_or(false)
    }

    pub fn accepts_reduced_git_metadata_isolation(&self) -> bool {
        self.accept_reduced_git_metadata_isolation.unwrap_or(false)
    }

    /// Merge: project overrides global, per-field.
    fn merge(global: Self, project: Self) -> Self {
        Self {
            isolation: project.isolation.or(global.isolation),
            projects_dir: project.projects_dir.or(global.projects_dir),
            cpus: project.cpus.or(global.cpus),
            memory: project.memory.or(global.memory),
            disk: project.disk.or(global.disk),
            provision: project.provision.or(global.provision),
            skip_default_provision: project
                .skip_default_provision
                .or(global.skip_default_provision),
            accept_reduced_git_metadata_isolation: project
                .accept_reduced_git_metadata_isolation
                .or(global.accept_reduced_git_metadata_isolation),
        }
    }
}

/// Host device mapping for container sandboxes.
///
/// Supports two YAML forms:
/// - string: `"/dev/kvm"`, `"/dev/dri:/dev/dri"`, `"/dev/dri:/dev/dri:rwm"`
/// - struct: `{ host_path, guest_path?, permissions? }`
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(untagged)]
pub enum ContainerDevice {
    String(String),
    Struct {
        host_path: String,
        #[serde(default)]
        guest_path: Option<String>,
        #[serde(default)]
        permissions: Option<String>,
    },
}

impl ContainerDevice {
    /// Render as the value for a `--device` flag.
    pub fn to_arg(&self) -> String {
        match self {
            Self::String(s) => s.clone(),
            Self::Struct {
                host_path,
                guest_path,
                permissions,
            } => {
                let gp = guest_path.as_deref().unwrap_or(host_path.as_str());
                match permissions.as_deref() {
                    Some(p) => format!("{host_path}:{gp}:{p}"),
                    None if guest_path.is_some() => format!("{host_path}:{gp}"),
                    None => host_path.clone(),
                }
            }
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::String(s) => {
                if s.is_empty() {
                    anyhow::bail!("empty device entry");
                }
                if s.chars().any(char::is_whitespace) {
                    anyhow::bail!("device entry must not contain whitespace: {s}");
                }
                let mut parts = s.split(':');
                let host = parts.next().unwrap_or("");
                if !host.starts_with('/') {
                    anyhow::bail!("device host path must be absolute: {s}");
                }
                if let Some(second) = parts.next() {
                    let third = parts.next();
                    if parts.next().is_some() {
                        anyhow::bail!("device entry has too many ':' separators: {s}");
                    }
                    // second is either a guest path (absolute) or a permissions token.
                    // third, if present, must be a permissions token.
                    let is_perms = |tok: &str| {
                        !tok.is_empty() && tok.chars().all(|c| matches!(c, 'r' | 'w' | 'm'))
                    };
                    match third {
                        Some(perms) => {
                            if !second.starts_with('/') {
                                anyhow::bail!("device guest path must be absolute: {s}");
                            }
                            if !is_perms(perms) {
                                anyhow::bail!(
                                    "invalid device permissions (expected subset of r/w/m): {s}"
                                );
                            }
                        }
                        None => {
                            if !second.starts_with('/') && !is_perms(second) {
                                anyhow::bail!(
                                    "device entry second token must be an absolute path or r/w/m permissions: {s}"
                                );
                            }
                        }
                    }
                }
                Ok(())
            }
            Self::Struct {
                host_path,
                guest_path,
                permissions,
            } => {
                if host_path.is_empty()
                    || !host_path.starts_with('/')
                    || host_path.chars().any(char::is_whitespace)
                {
                    anyhow::bail!(
                        "device host_path must be an absolute path with no whitespace: {host_path}"
                    );
                }
                if let Some(gp) = guest_path
                    && (gp.is_empty()
                        || !gp.starts_with('/')
                        || gp.chars().any(char::is_whitespace))
                {
                    anyhow::bail!(
                        "device guest_path must be an absolute path with no whitespace: {gp}"
                    );
                }
                if let Some(p) = permissions
                    && (p.is_empty() || !p.chars().all(|c| matches!(c, 'r' | 'w' | 'm')))
                {
                    anyhow::bail!("invalid device permissions (expected subset of r/w/m): {p}");
                }
                Ok(())
            }
        }
    }
}

pub(crate) fn validate_group_add_entry(group: &str) -> anyhow::Result<()> {
    let trimmed = group.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty group_add entry");
    }
    if trimmed
        .chars()
        .any(|c| c.is_whitespace() || c == ',' || c == ':')
    {
        anyhow::bail!("group_add entry must not contain whitespace or separators: {group}");
    }
    Ok(())
}

/// Container-specific sandbox configuration.
/// Nested under `sandbox.container` in YAML.
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct ContainerConfig {
    /// Container runtime. Auto-detected from PATH if not set.
    #[serde(default)]
    pub runtime: Option<SandboxRuntime>,

    /// Number of CPUs for the container. Only passed when explicitly set.
    /// Apple Container defaults to 4 CPUs which is sufficient for most workloads.
    #[serde(default)]
    pub cpus: Option<u32>,

    /// Memory limit for the container (e.g. "8G", "16G").
    /// For Apple Container, defaults to "16G" when not set (the VM's 1 GB default
    /// is too low). For Docker/Podman, only passed when explicitly set.
    #[serde(default)]
    pub memory: Option<String>,

    /// Host device nodes exposed to the container sandbox (docker `--device`).
    /// Global-only: ignored in project config.
    #[serde(default)]
    pub devices: Option<Vec<ContainerDevice>>,

    /// Supplementary groups added to the sandboxed process
    /// (docker `--group-add`). Values can be group names or numeric GIDs.
    /// Global-only: ignored in project config.
    #[serde(default)]
    pub group_add: Option<Vec<String>>,

    /// Files (relative to the worktree root) to mask out of the container's
    /// worktree bind mounts. Each path is shadowed by bind-mounting `/dev/null`
    /// over it so agents running inside the container cannot read the host
    /// file. Useful for keeping `.env` and other secret-bearing files out of
    /// the sandbox without restructuring the project.
    ///
    /// Masking applies to both the current worktree and, where applicable, the
    /// main-worktree mount (which workmux adds for symlink resolution), so a
    /// symlinked secret cannot be read via the alias path.
    ///
    /// Only existing regular files are masked; missing paths are skipped with
    /// a warning. Directories are not supported.
    ///
    /// Security: this field is global-only. It is ignored when set in a
    /// project's `.workmux.yaml`, and workmux fails fast rather than running
    /// with excluded_files on a runtime that lacks file-level bind mounts
    /// (Apple Container).
    #[serde(default)]
    pub excluded_files: Option<Vec<String>>,

    /// OCI runtime to run the container under (docker/local Podman `--runtime`).
    /// When unset, workmux omits `--runtime` and the engine uses its configured
    /// default. Set to e.g. "kata" to run the container under a VM-based OCI
    /// runtime for stronger isolation. The named runtime must be installed and
    /// registered with the container runtime. Global-only: ignored in project
    /// config, since it changes the isolation boundary.
    #[serde(default)]
    pub oci_runtime: Option<String>,

    /// Extra Linux capabilities to grant the container (docker/podman
    /// `--cap-add`). Each entry is passed verbatim, e.g. "ALL" or "SYS_ADMIN".
    /// Global-only: ignored in project config, since capabilities relax the
    /// isolation boundary. Empty by default.
    ///
    /// Needed for docker-in-docker under a VM runtime (`oci_runtime: kata`),
    /// where `--privileged` is not usable (it injects host device nodes that
    /// collide with the guest's, failing to boot). Granting capabilities +
    /// security_opt instead boots cleanly, and the daemon stays confined to
    /// the guest VM.
    #[serde(default)]
    pub cap_add: Option<Vec<String>>,

    /// Extra security options for the container (docker/podman
    /// `--security-opt`). Each entry is passed verbatim, e.g.
    /// "seccomp=unconfined" or "systempaths=unconfined". Global-only: ignored
    /// in project config, since these options control the isolation boundary.
    /// Empty by default. See `cap_add` for the docker-in-docker rationale.
    #[serde(default)]
    pub security_opt: Option<Vec<String>>,
}

impl ContainerConfig {
    pub fn runtime(&self) -> SandboxRuntime {
        self.runtime.unwrap_or_else(SandboxRuntime::detect)
    }

    /// OCI runtime name to pass to `docker/podman run --runtime`, if configured.
    pub fn oci_runtime(&self) -> Option<&str> {
        self.oci_runtime.as_deref()
    }

    pub fn devices(&self) -> &[ContainerDevice] {
        self.devices.as_deref().unwrap_or(&[])
    }

    pub fn group_add(&self) -> &[String] {
        self.group_add.as_deref().unwrap_or(&[])
    }

    /// Extra capabilities to pass to `docker/podman run --cap-add`.
    /// Returns empty slice when unset.
    pub fn cap_add(&self) -> &[String] {
        self.cap_add.as_deref().unwrap_or(&[])
    }

    /// Extra security options to pass to `docker/podman run --security-opt`.
    /// Returns empty slice when unset.
    pub fn security_opt(&self) -> &[String] {
        self.security_opt.as_deref().unwrap_or(&[])
    }

    /// Files to mask out of the worktree bind mount (relative to worktree root).
    /// Returns empty slice when unset.
    pub fn excluded_files(&self) -> &[String] {
        self.excluded_files.as_deref().unwrap_or(&[])
    }

    /// Structural validation for hardware access fields. Called at config load.
    pub fn validate(&self) -> anyhow::Result<()> {
        for d in self.devices() {
            d.validate()?;
        }
        for g in self.group_add() {
            validate_group_add_entry(g)?;
        }
        Ok(())
    }

    /// Merge: project overrides global, per-field, EXCEPT for `devices`,
    /// `group_add`, `oci_runtime`, `cap_add` and `security_opt` which are
    /// security-sensitive and global-only. Warnings for project-level attempts
    /// are emitted in `Config::merge` where both values are visible.
    fn merge(global: Self, project: Self) -> Self {
        // Security: excluded_files is global-only. Project config cannot set it --
        // otherwise a repo's .workmux.yaml could delete user-level secret
        // protections by providing an empty/overriding list.
        if project.excluded_files.is_some() {
            tracing::warn!(
                "sandbox.container.excluded_files in project config (.workmux.yaml) is ignored -- \
                move it to your global config (~/.config/workmux/config.yaml)"
            );
        }
        Self {
            runtime: project.runtime.or(global.runtime),
            cpus: project.cpus.or(global.cpus),
            memory: project.memory.or(global.memory),
            devices: global.devices,
            group_add: global.group_add,
            excluded_files: global.excluded_files,
            oci_runtime: global.oci_runtime,
            cap_add: global.cap_add,
            security_opt: global.security_opt,
        }
    }
}

/// Network restriction policy for sandboxed containers.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicy {
    /// No network restrictions (default).
    Allow,
    /// Block all outbound except whitelisted domains via CONNECT proxy.
    Deny,
}

/// Detailed allowed domain rule: `{ host, allow_private_ips }`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedDomainDetails {
    pub host: String,
    #[serde(default)]
    pub allow_private_ips: bool,
}

/// Allowed outbound HTTPS domain entry.
///
/// Backwards compatible: a bare string parses as a public-only rule.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum AllowedDomainEntry {
    Plain(String),
    Detailed(AllowedDomainDetails),
}

impl AllowedDomainEntry {
    pub fn host(&self) -> &str {
        match self {
            Self::Plain(host) => host,
            Self::Detailed(details) => &details.host,
        }
    }

    pub fn allow_private_ips(&self) -> bool {
        match self {
            Self::Plain(_) => false,
            Self::Detailed(details) => details.allow_private_ips,
        }
    }
}

/// Runtime form of an allowed domain rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedDomainRule {
    pub host: String,
    pub allow_private_ips: bool,
}

pub const DEFAULT_NETWORK_MAX_CONNECTIONS: usize = 128;
pub const MAX_NETWORK_MAX_CONNECTIONS: usize = 1024;

/// Network restriction configuration for the container sandbox.
///
/// When `policy` is `deny`, all outbound connections are blocked except those
/// to whitelisted domains via an HTTP CONNECT proxy. An iptables firewall
/// inside the container enforces that only the proxy and RPC ports are
/// reachable, preventing bypass via direct connections.
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct NetworkConfig {
    /// Network restriction policy. Default: allow (no restrictions).
    /// Set to "deny" to block all outbound except whitelisted domains.
    #[serde(default)]
    pub policy: Option<NetworkPolicy>,

    /// Allowed outbound HTTPS domains when policy is "deny".
    /// Supports exact matches and wildcard prefixes (e.g., "*.googleapis.com").
    /// The host RPC endpoint is always allowed regardless of this list.
    #[serde(default)]
    pub allowed_domains: Option<Vec<AllowedDomainEntry>>,

    /// Maximum number of concurrent CONNECT proxy connections.
    #[serde(default)]
    pub max_connections: Option<usize>,
}

impl NetworkConfig {
    /// Get the effective network policy. Default: Allow.
    pub fn policy(&self) -> NetworkPolicy {
        self.policy.clone().unwrap_or(NetworkPolicy::Allow)
    }

    /// Get the allowed domains list (empty if not set).
    pub fn allowed_domains(&self) -> &[AllowedDomainEntry] {
        self.allowed_domains.as_deref().unwrap_or(&[])
    }

    /// Get the maximum number of concurrent proxy connections.
    pub fn max_connections(&self) -> usize {
        self.max_connections
            .unwrap_or(DEFAULT_NETWORK_MAX_CONNECTIONS)
    }

    /// Get normalized allowed domain rules.
    pub fn allowed_domain_rules(&self) -> Vec<AllowedDomainRule> {
        self.allowed_domains()
            .iter()
            .map(|entry| AllowedDomainRule {
                host: entry.host().to_string(),
                allow_private_ips: entry.allow_private_ips(),
            })
            .collect()
    }

    /// Validate all domain entries. Called at config load time.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(max_connections) = self.max_connections
            && !(1..=MAX_NETWORK_MAX_CONNECTIONS).contains(&max_connections)
        {
            anyhow::bail!(
                "network.max_connections must be between 1 and {}",
                MAX_NETWORK_MAX_CONNECTIONS
            );
        }

        for entry in self.allowed_domains() {
            let host = entry.host();
            validate_domain(host)?;
            if entry.allow_private_ips() && host.starts_with("*.") {
                anyhow::bail!(
                    "allow_private_ips is only allowed for exact domains: {}",
                    host
                );
            }
        }
        Ok(())
    }
}

/// Validate a single domain entry for the allowed_domains list.
fn validate_domain(domain: &str) -> anyhow::Result<()> {
    use std::net::IpAddr;
    // Reject IP literals
    if domain.parse::<IpAddr>().is_ok() {
        anyhow::bail!("IP literals not allowed in allowed_domains: {}", domain);
    }
    // Reject trailing dots
    if domain.ends_with('.') {
        anyhow::bail!("trailing dot not allowed in domain: {}", domain);
    }
    // Wildcard must be *.suffix form only
    if domain.contains('*') && !domain.starts_with("*.") {
        anyhow::bail!("invalid wildcard pattern (must be *.suffix): {}", domain);
    }
    // Empty domains
    if domain.is_empty() {
        anyhow::bail!("empty domain not allowed in allowed_domains");
    }
    Ok(())
}

/// Configuration for sandboxing (Container or Lima)
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct SandboxConfig {
    /// Enable sandboxing. Default: false
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Sandbox backend. Default: container
    #[serde(default)]
    pub backend: Option<SandboxBackend>,

    /// Which panes to sandbox. Default: agent
    #[serde(default)]
    pub target: Option<SandboxTarget>,

    /// Container/VM image. For containers: Docker image name.
    /// For Lima: qcow2 image URL or file:// path.
    #[serde(default)]
    pub image: Option<String>,

    /// Environment variables to pass to sandbox.
    /// Default: []
    #[serde(default)]
    pub env_passthrough: Option<Vec<String>>,

    /// Environment variables to set in the sandbox with explicit values.
    /// Unlike env_passthrough (which reads from host), these are set directly.
    /// Global-only: project config cannot set this to prevent a sandboxed agent
    /// from injecting env vars into its next session via .workmux.yaml.
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,

    /// Override the hostname used by containers to reach the host RPC server.
    /// Defaults to `host.docker.internal` (Docker) or `host.containers.internal` (Podman).
    /// Useful for non-standard Podman or custom networking setups.
    #[serde(default)]
    pub rpc_host: Option<String>,

    /// Toolchain integration mode for sandboxes.
    /// Controls automatic detection and use of devbox.json/flake.nix.
    /// Default: auto (detect and wrap automatically)
    #[serde(default)]
    pub toolchain: Option<ToolchainMode>,

    /// Commands to proxy from guest to host via host-exec RPC.
    /// When set, shims are created in the guest VM that forward these
    /// commands to the host's toolchain environment.
    #[serde(default)]
    pub host_commands: Option<Vec<String>>,

    /// Extra mount points for the sandbox.
    /// Paths are mounted read-only by default. Supports simple string paths
    /// or detailed specs with guest_path and writable options.
    #[serde(default)]
    pub extra_mounts: Option<Vec<ExtraMount>>,

    /// Custom host directory for agent config (mounted instead of the default).
    /// Supports `{agent}` placeholder, e.g. `~/sandbox-config/{agent}`.
    /// When not set, defaults to the agent's standard config directory
    /// (e.g. `~/.claude/`, `~/.gemini/`).
    #[serde(default)]
    pub agent_config_dir: Option<String>,

    /// Lima-specific configuration
    #[serde(default)]
    pub lima: LimaConfig,

    /// Container-specific configuration
    #[serde(default)]
    pub container: ContainerConfig,

    /// Network restriction configuration (container backend only).
    #[serde(default)]
    pub network: NetworkConfig,

    /// Allow host-exec to run without bwrap sandboxing on Linux.
    /// Default: false (fail closed -- refuse to run if bwrap is missing).
    /// When true, falls back to unsandboxed execution with a warning.
    #[serde(default)]
    pub dangerously_allow_unsandboxed_host_exec: Option<bool>,
}

impl SandboxConfig {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    pub fn backend(&self) -> SandboxBackend {
        self.backend.clone().unwrap_or_default()
    }

    pub fn runtime(&self) -> SandboxRuntime {
        self.container.runtime()
    }

    pub fn target(&self) -> SandboxTarget {
        self.target.clone().unwrap_or_default()
    }

    /// Get the image name, falling back to the default ghcr.io image for the agent.
    ///
    /// `agent` must be a canonical agent name (e.g. "claude", "codex"), not a raw
    /// command string. Use `resolve_profile().name()` to obtain it.
    pub fn resolved_image(&self, agent: &str) -> String {
        match &self.image {
            Some(image) => image.clone(),
            None => format!("{}:{}", crate::sandbox::DEFAULT_IMAGE_REGISTRY, agent),
        }
    }

    pub fn env_passthrough(&self) -> Vec<&str> {
        self.env_passthrough
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }

    /// Get explicit environment variables to set in the sandbox.
    pub fn env_vars(&self) -> Vec<(&str, &str)> {
        self.env
            .as_ref()
            .map(|m| m.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect())
            .unwrap_or_default()
    }

    /// Get the RPC host address, using config override or runtime default.
    pub fn resolved_rpc_host(&self) -> String {
        self.rpc_host
            .clone()
            .unwrap_or_else(|| self.runtime().rpc_host_address().to_string())
    }

    pub fn toolchain(&self) -> ToolchainMode {
        self.toolchain.clone().unwrap_or_default()
    }

    pub fn host_commands(&self) -> &[String] {
        self.host_commands.as_deref().unwrap_or(&[])
    }

    pub fn extra_mounts(&self) -> &[ExtraMount] {
        self.extra_mounts.as_deref().unwrap_or(&[])
    }

    pub fn allow_unsandboxed_host_exec(&self) -> bool {
        self.dangerously_allow_unsandboxed_host_exec
            .unwrap_or(false)
    }

    /// Returns true if network policy is deny (restrictions active).
    pub fn network_policy_is_deny(&self) -> bool {
        self.network.policy() == NetworkPolicy::Deny
    }

    /// Returns the resolved agent config directory path for the given agent.
    /// Performs `{agent}` substitution and tilde expansion on the configured path.
    /// Falls back to the agent's default config directory when not configured.
    pub fn resolved_agent_config_dir(&self, agent: &str) -> Option<PathBuf> {
        self.resolved_agent_config_dir_with_env(agent, |key| std::env::var_os(key))
    }

    fn resolved_agent_config_dir_with_env(
        &self,
        agent: &str,
        get_env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Option<PathBuf> {
        if let Some(ref dir) = self.agent_config_dir {
            let expanded = dir.replace("{agent}", agent);
            Some(crate::util::expand_tilde(&expanded))
        } else {
            let home = home::home_dir()?;
            match agent {
                "claude" => Some(home.join(".claude")),
                "copilot" => Some(home.join(".copilot")),
                "gemini" => Some(home.join(".gemini")),
                "agy" => Some(home.join(".gemini/antigravity-cli")),
                "codex" => Some(home.join(".codex")),
                "opencode" => Some(home.join(".local/share/opencode")),
                "pi" => Some(home.join(".pi/agent")),
                "omp" => Some(crate::agent_setup::omp::omp_agent_dir_with_env(
                    &home, get_env,
                )),
                _ => None,
            }
        }
    }
}

/// Result of config discovery, including the relative path from repo root
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

/// Validate windows configuration
pub fn validate_windows_config(windows: &[WindowConfig]) -> anyhow::Result<()> {
    if windows.is_empty() {
        anyhow::bail!("'windows' list must not be empty.");
    }
    for (i, window) in windows.iter().enumerate() {
        if let Some(panes) = &window.panes {
            validate_panes_config(panes).map_err(|e| {
                anyhow::anyhow!(
                    "Window {} ({}): {}",
                    i,
                    window.name.as_deref().unwrap_or("unnamed"),
                    e
                )
            })?;
        }
    }
    Ok(())
}

/// Validate pane configuration
pub fn validate_panes_config(panes: &[PaneConfig]) -> anyhow::Result<()> {
    for (i, pane) in panes.iter().enumerate() {
        if let Some(name) = pane.name.as_deref()
            && name.trim().is_empty()
        {
            anyhow::bail!("Pane {} has an empty 'name'.", i);
        }

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

        if matches!(pane.split, Some(SplitDirection::Stacked))
            && (pane.size.is_some() || pane.percentage.is_some())
        {
            anyhow::bail!(
                "Pane {} with 'split: stacked' cannot have 'size' or 'percentage' specified.",
                i
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

    // Only one pane can have zoom
    let zoom_count = panes.iter().filter(|p| p.zoom).count();
    if zoom_count > 1 {
        anyhow::bail!(
            "Only one pane can have 'zoom: true' (found {}).",
            zoom_count
        );
    }

    Ok(())
}

/// Validate layouts configuration by validating each layout's panes.
#[cfg(test)]
pub fn validate_layouts_config(layouts: &HashMap<String, LayoutConfig>) -> anyhow::Result<()> {
    for (name, layout) in layouts {
        validate_panes_config(&layout.panes)
            .map_err(|e| anyhow::anyhow!("Invalid panes in layout '{}': {}", name, e))?;
    }
    Ok(())
}

/// Get the path to the global config file.
///
/// Resolves via `$XDG_CONFIG_HOME/workmux/` (default `~/.config/workmux/`).
/// If a custom `XDG_CONFIG_HOME` is set and no config exists there yet,
/// falls back to the legacy `~/.config/workmux/` location for reading.
/// Prefers existing .yml file to avoid shadowing, otherwise defaults to .yaml.
pub fn global_config_path() -> Option<PathBuf> {
    let xdg_dir = crate::xdg::config_dir().ok()?;
    let yaml = xdg_dir.join("config.yaml");
    let yml = xdg_dir.join("config.yml");

    // Check XDG location first
    if yml.exists() && !yaml.exists() {
        return Some(yml);
    }
    if yaml.exists() {
        return Some(yaml);
    }

    // Legacy fallback: if XDG_CONFIG_HOME points elsewhere, check ~/.config/workmux/
    if let Some(home) = home::home_dir() {
        let legacy_dir = home.join(".config/workmux");
        if legacy_dir != xdg_dir {
            let legacy_yml = legacy_dir.join("config.yml");
            let legacy_yaml = legacy_dir.join("config.yaml");
            if legacy_yml.exists() && !legacy_yaml.exists() {
                return Some(legacy_yml);
            }
            if legacy_yaml.exists() {
                return Some(legacy_yaml);
            }
        }
    }

    // Default to XDG path for new config files
    Some(yaml)
}

impl Config {
    /// Load and merge global and project configurations.
    pub fn load(cli_agent: Option<&str>) -> anyhow::Result<Self> {
        Self::load_with_override(cli_agent, None)
    }

    pub fn load_with_override(
        cli_agent: Option<&str>,
        config_override: Option<&Path>,
    ) -> anyhow::Result<Self> {
        Self::load_with_location(cli_agent, config_override).map(|(cfg, _)| cfg)
    }

    /// Load and merge configs, returning the final config and project config location.
    /// The location indicates where the project config was found (for working dir calculation).
    pub fn load_with_location(
        cli_agent: Option<&str>,
        config_override: Option<&Path>,
    ) -> anyhow::Result<(Self, Option<ConfigLocation>)> {
        let start_dir = std::env::current_dir().unwrap_or_default();
        Self::load_with_location_from_override(&start_dir, cli_agent, config_override)
    }

    /// Like `load_with_location`, but searches for the project config starting
    /// from `start_dir` instead of CWD.
    pub fn load_with_location_from(
        start_dir: &std::path::Path,
        cli_agent: Option<&str>,
    ) -> anyhow::Result<(Self, Option<ConfigLocation>)> {
        Self::load_with_location_from_override(start_dir, cli_agent, None)
    }

    pub fn load_with_location_from_override(
        start_dir: &std::path::Path,
        cli_agent: Option<&str>,
        config_override: Option<&Path>,
    ) -> anyhow::Result<(Self, Option<ConfigLocation>)> {
        let frozen_path = std::env::var_os(crate::frozen_config::FROZEN_CONFIG_ENV);
        Self::load_with_location_from_sources(
            start_dir,
            cli_agent,
            config_override,
            frozen_path.as_deref().map(Path::new),
            crate::sandbox::guest::is_sandbox_guest(),
        )
    }

    fn load_with_location_from_sources(
        start_dir: &std::path::Path,
        cli_agent: Option<&str>,
        config_override: Option<&Path>,
        frozen_path: Option<&Path>,
        is_sandbox_guest: bool,
    ) -> anyhow::Result<(Self, Option<ConfigLocation>)> {
        if let Some(path) = frozen_path {
            if is_sandbox_guest {
                anyhow::bail!("Frozen configuration is only valid for host commands");
            }
            if config_override.is_some() {
                anyhow::bail!("Frozen configuration cannot be combined with --config");
            }
            return crate::frozen_config::load(path);
        }

        debug!(start_dir = %start_dir.display(), "config:loading with location from");
        let global_config = Self::load_global()?.unwrap_or_default();

        let (project_config, location) = if let Some(path) = config_override {
            let meta = std::fs::metadata(path)
                .with_context(|| format!("Config file not found: {}", path.display()))?;
            if meta.is_dir() {
                anyhow::bail!(
                    "--config path must be a file, not a directory: {}",
                    path.display()
                );
            }
            if !meta.is_file() {
                anyhow::bail!("--config path must be a regular file: {}", path.display());
            }
            let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            let config = Self::load_from_path(&abs_path)?
                .ok_or_else(|| anyhow::anyhow!("Config file not found: {}", path.display()))?;
            let config_dir = abs_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| start_dir.to_path_buf());
            // rel_dir is intentionally empty: --config overrides don't imply a subproject layout,
            // so panes always open at the worktree root regardless of where the config file lives.
            let location = ConfigLocation {
                config_path: abs_path,
                config_dir: config_dir.clone(),
                rel_dir: PathBuf::new(),
            };
            (config, Some(location))
        } else {
            let location = find_project_config(start_dir)?;
            let project_config = if let Some(ref loc) = location {
                Self::load_from_path(&loc.config_path)?.unwrap_or_default()
            } else {
                Self::default()
            };
            (project_config, location)
        };

        let defaults_root = location
            .as_ref()
            .and_then(|loc| {
                let repo_root = git::get_repo_root_for(start_dir).ok()?;
                if loc.config_dir.starts_with(&repo_root) {
                    Some(loc.config_dir.clone())
                } else {
                    Some(repo_root)
                }
            })
            .or_else(|| git::get_repo_root_for(start_dir).ok())
            .unwrap_or_else(|| start_dir.to_path_buf());

        let mut config = Self::merge_and_apply_defaults(
            global_config,
            project_config,
            cli_agent,
            &defaults_root,
        )?;

        // Expand `{project}` against the main worktree so every worktree of a
        // repository resolves the same prefix, whatever directory it runs from.
        // Outside a repository there is no project, so the template stands.
        if let Some(prefix) = config.window_prefix.as_deref()
            && prefix.contains(PROJECT_PLACEHOLDER)
            && let Ok(main_root) = git::get_main_worktree_root_in(Some(start_dir))
        {
            config.window_prefix = Some(expand_project_placeholder(prefix, &main_root)?);
        }

        debug!(
            agent = ?config.agent,
            has_location = location.is_some(),
            "config:loaded with location from"
        );
        Ok((config, location))
    }

    /// Merge global and project configs, resolve agent, and apply defaults.
    fn merge_and_apply_defaults(
        global_config: Self,
        project_config: Self,
        cli_agent: Option<&str>,
        defaults_root: &std::path::Path,
    ) -> anyhow::Result<Self> {
        let has_explicit_agent =
            cli_agent.is_some() || project_config.agent.is_some() || global_config.agent.is_some();

        let final_agent = cli_agent
            .map(|s| s.to_string())
            .or_else(|| project_config.agent.clone())
            .or_else(|| global_config.agent.clone())
            .unwrap_or_else(|| "claude".to_string());

        let mut config = global_config.merge(project_config);

        config.selected_agent = Some(final_agent.clone());

        // Resolve agent name through agents map
        if let Some(entry) = config.agents.get(&final_agent) {
            config.agent_type = entry.agent_type.clone();
            config.agent = Some(entry.command_or_default(&final_agent));
        } else {
            config.agent = Some(final_agent);
        }

        if !defaults_root.as_os_str().is_empty() {
            let has_node_modules = defaults_root.join("pnpm-lock.yaml").exists()
                || defaults_root.join("package-lock.json").exists()
                || defaults_root.join("yarn.lock").exists();

            if config.panes.is_none() && config.windows.is_none() {
                if defaults_root.join("CLAUDE.md").exists() || has_explicit_agent {
                    config.panes = Some(Self::agent_default_panes());
                } else {
                    config.panes = Some(Self::default_panes());
                }
            }

            if config.pre_remove.is_none() && has_node_modules {
                config.pre_remove = Some(vec![NODE_MODULES_CLEANUP_SCRIPT.to_string()]);
            }
        } else if config.panes.is_none() && config.windows.is_none() {
            if has_explicit_agent {
                config.panes = Some(Self::agent_default_panes());
            } else {
                config.panes = Some(Self::default_panes());
            }
        }

        config
            .sandbox
            .network
            .validate()
            .context("Invalid sandbox network config")?;
        config
            .sandbox
            .container
            .validate()
            .context("Invalid sandbox container config")?;

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

    /// Load the global configuration file.
    ///
    /// Uses `global_config_path()` which resolves via XDG_CONFIG_HOME with
    /// legacy fallback.
    fn load_global() -> anyhow::Result<Option<Self>> {
        if let Some(path) = global_config_path()
            && path.exists()
        {
            return Self::load_from_path(&path);
        }
        Ok(None)
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
            match project {
                Some(project_items) => {
                    let has_placeholder = project_items.iter().any(|s| s == "<global>");
                    if has_placeholder {
                        let global_items = global.unwrap_or_default();
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
                None => global,
            }
        }

        // Track which layout type the project config specified
        let project_has_windows = project.windows.is_some();

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
            base_branch,
            worktree_dir,
            window_prefix,
            agent,
            merge_strategy,
            merge_keep,
            worktree_prefix,
            panes,
            windows,
            status_format,
            nerdfont,
            auto_update_check,
            prompt_file_only,
        );

        // Layouts: merge maps by key so project layouts extend global ones
        merged.layouts = match (self.layouts, project.layouts) {
            (Some(mut global), Some(proj)) => {
                global.extend(proj);
                Some(global)
            }
            (global, proj) => proj.or(global),
        };

        // Deep merge auto_name. Security: command is global-only to prevent
        // a malicious .workmux.yaml from executing arbitrary commands on the host.
        merged.auto_name = match (self.auto_name, project.auto_name) {
            (Some(global), Some(project)) => {
                if project.command.is_some() {
                    tracing::warn!(
                        "auto_name.command in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                Some(AutoNameConfig {
                    command: global.command,
                    model: project.model.or(global.model),
                    system_prompt: project.system_prompt.or(global.system_prompt),
                    background: project.background.or(global.background),
                })
            }
            (Some(global), None) => Some(global),
            (None, Some(project)) => {
                if project.command.is_some() {
                    tracing::warn!(
                        "auto_name.command in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                Some(AutoNameConfig {
                    command: None,
                    model: project.model,
                    system_prompt: project.system_prompt,
                    background: project.background,
                })
            }
            (None, None) => None,
        };

        // windows and panes are mutually exclusive: project layout choice wins entirely
        if merged.windows.is_some() && merged.panes.is_some() {
            // If project set windows, clear panes (project intended multi-window)
            // If project set panes, clear windows (project intended single-window)
            if project_has_windows {
                merged.panes = None;
            } else {
                merged.windows = None;
            }
        }

        // Special case: worktree_naming (project wins if not default)
        merged.worktree_naming = if project.worktree_naming != WorktreeNaming::default() {
            project.worktree_naming
        } else {
            self.worktree_naming
        };

        // Special case: theme (merge field-by-field, project wins if explicitly set)
        merged.theme = ThemeConfig {
            scheme: if project.theme.scheme != ThemeScheme::Default {
                project.theme.scheme
            } else {
                self.theme.scheme
            },
            mode: project.theme.mode.or(self.theme.mode),
            custom: project.theme.custom.or(self.theme.custom),
        };

        // Special case: mode (project wins if explicitly set)
        merged.mode = project.mode.or(self.mode);

        // Special case: window_placement (project wins if explicitly set)
        merged.window_placement = project.window_placement.or(self.window_placement);

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
            show_check_counts: project
                .dashboard
                .show_check_counts
                .or(self.dashboard.show_check_counts),
            agent_columns: project
                .dashboard
                .agent_columns
                .clone()
                .or_else(|| self.dashboard.agent_columns.clone()),
            sort_mode: project.dashboard.sort_mode.or(self.dashboard.sort_mode),
            close_on_jump: project
                .dashboard
                .close_on_jump
                .or(self.dashboard.close_on_jump),
        };

        // Sidebar config: per-field override
        merged.sidebar = SidebarConfig {
            position: project.sidebar.position.or(self.sidebar.position),
            width: project.sidebar.width.or(self.sidebar.width),
            height: project.sidebar.height.or(self.sidebar.height),
            layout: project.sidebar.layout.or(self.sidebar.layout),
            horizontal: HorizontalSidebarConfig {
                item_width: project
                    .sidebar
                    .horizontal
                    .item_width
                    .or(self.sidebar.horizontal.item_width),
            },
            templates: project
                .sidebar
                .templates
                .clone()
                .or(self.sidebar.templates.clone()),
            agent_icons: match (
                self.sidebar.agent_icons.clone(),
                project.sidebar.agent_icons.clone(),
            ) {
                (Some(mut global), Some(proj)) => {
                    global.extend(proj);
                    Some(global)
                }
                (g, p) => p.or(g),
            },
            sort: project.sidebar.sort.or(self.sidebar.sort),
            dim_stale: project.sidebar.dim_stale.or(self.sidebar.dim_stale),
        };

        // Sandbox config: per-field override with nested struct merging
        merged.sandbox = SandboxConfig {
            enabled: project.sandbox.enabled.or(self.sandbox.enabled),
            backend: project
                .sandbox
                .backend
                .clone()
                .or(self.sandbox.backend.clone()),
            target: project
                .sandbox
                .target
                .clone()
                .or(self.sandbox.target.clone()),
            image: project.sandbox.image.clone().or(self.sandbox.image.clone()),
            // Security: env_passthrough is global-only. Project config cannot
            // set it -- this prevents a malicious repo from requesting
            // passthrough of host env secrets via .workmux.yaml.
            env_passthrough: {
                if project.sandbox.env_passthrough.is_some() {
                    tracing::warn!(
                        "env_passthrough in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                self.sandbox.env_passthrough.clone()
            },
            // Security: env is global-only. A sandboxed agent could modify
            // .workmux.yaml to inject env vars into its next session.
            env: {
                if project.sandbox.env.is_some() {
                    tracing::warn!(
                        "env in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                self.sandbox.env
            },
            // Security: rpc_host is global-only. Project config cannot
            // set it -- this prevents a malicious repo from redirecting
            // RPC traffic to attacker infrastructure via .workmux.yaml.
            rpc_host: {
                if project.sandbox.rpc_host.is_some() {
                    tracing::warn!(
                        "rpc_host in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                self.sandbox.rpc_host.clone()
            },
            toolchain: project
                .sandbox
                .toolchain
                .clone()
                .or(self.sandbox.toolchain.clone()),
            // Security: host_commands is global-only. Project config cannot
            // set it -- this prevents a malicious repo from granting itself
            // host-exec access via .workmux.yaml.
            host_commands: {
                if project.sandbox.host_commands.is_some() {
                    tracing::warn!(
                        "host_commands in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                self.sandbox.host_commands.clone()
            },
            // Security: extra_mounts is global-only. Project config cannot
            // set it -- this prevents a malicious repo from mounting over
            // host paths via .workmux.yaml.
            extra_mounts: {
                if project.sandbox.extra_mounts.is_some() {
                    tracing::warn!(
                        "extra_mounts in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                self.sandbox.extra_mounts.clone()
            },
            // Security: agent_config_dir is global-only. Project config cannot
            // set it -- this prevents a malicious repo from redirecting agent
            // config mounts via .workmux.yaml.
            agent_config_dir: {
                if project.sandbox.agent_config_dir.is_some() {
                    tracing::warn!(
                        "agent_config_dir in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                self.sandbox.agent_config_dir.clone()
            },
            lima: LimaConfig::merge(self.sandbox.lima, project.sandbox.lima),
            // Security: sandbox.container.devices, group_add, oci_runtime,
            // cap_add and security_opt are global-only. devices/group_add
            // expose host hardware and can expand filesystem access via
            // supplementary groups; oci_runtime changes the isolation boundary
            // (e.g. a VM runtime vs runc); cap_add/security_opt control the
            // container's capability/seccomp/apparmor confinement, and permissive
            // values relax it. A malicious repo must not be able to set them via
            // .workmux.yaml.
            container: {
                if project.sandbox.container.devices.is_some() {
                    tracing::warn!(
                        "sandbox.container.devices in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                if project.sandbox.container.group_add.is_some() {
                    tracing::warn!(
                        "sandbox.container.group_add in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                if project.sandbox.container.oci_runtime.is_some() {
                    tracing::warn!(
                        "sandbox.container.oci_runtime in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                if project.sandbox.container.cap_add.is_some() {
                    tracing::warn!(
                        "sandbox.container.cap_add in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                if project.sandbox.container.security_opt.is_some() {
                    tracing::warn!(
                        "sandbox.container.security_opt in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                ContainerConfig::merge(self.sandbox.container, project.sandbox.container)
            },
            // Security: network is global-only. Project config cannot
            // set it -- this prevents a malicious repo from weakening
            // network restrictions via .workmux.yaml.
            network: {
                if project.sandbox.network.policy.is_some()
                    || project.sandbox.network.allowed_domains.is_some()
                    || project.sandbox.network.max_connections.is_some()
                {
                    tracing::warn!(
                        "network in project config (.workmux.yaml) is ignored -- \
                        move it to your global config (~/.config/workmux/config.yaml)"
                    );
                }
                self.sandbox.network.clone()
            },
            // Security: global-only, same as host_commands.
            dangerously_allow_unsandboxed_host_exec: self
                .sandbox
                .dangerously_allow_unsandboxed_host_exec,
        };

        // Security: agents is global-only. Project config cannot define agents
        // -- this prevents a malicious repo from executing arbitrary commands
        // via .workmux.yaml.
        merged.agents = if !project.agents.is_empty() {
            tracing::warn!(
                "agents in project config (.workmux.yaml) is ignored -- \
                move it to your global config (~/.config/workmux/config.yaml)"
            );
            self.agents
        } else {
            self.agents
        };

        merged
    }

    /// Build the default two-pane layout with an optional primary command.
    fn default_panes_with_primary(command: Option<&str>) -> Vec<PaneConfig> {
        vec![
            PaneConfig {
                command: command.map(str::to_string),
                focus: true,
                ..Default::default()
            },
            PaneConfig {
                command: Some("clear".to_string()),
                split: Some(SplitDirection::Horizontal),
                ..Default::default()
            },
        ]
    }

    /// Get default panes.
    fn default_panes() -> Vec<PaneConfig> {
        Self::default_panes_with_primary(None)
    }

    /// Get default panes for a Claude project.
    fn agent_default_panes() -> Vec<PaneConfig> {
        Self::default_panes_with_primary(Some("<agent>"))
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

    /// Get the mode (window or session).
    /// Returns the configured value or defaults to Window.
    pub fn mode(&self) -> MuxMode {
        self.mode.unwrap_or(MuxMode::Window)
    }

    /// Get the window placement strategy.
    pub fn window_placement(&self) -> WindowPlacement {
        self.window_placement.unwrap_or_default()
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

        fs::write(&config_path, EXAMPLE_PROJECT_CONFIG)?;

        println!("✓ Created .workmux.yaml");
        println!("\nThis file provides project-specific overrides.");
        println!(
            "For global settings, edit {}",
            global_config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.config/workmux/config.yaml".to_string())
        );

        Ok(())
    }
}

/// Example project configuration with all options documented.
/// Used by `workmux init` and `workmux config show`.
pub const EXAMPLE_PROJECT_CONFIG: &str = r#"# workmux project configuration
# For global settings, edit ~/.config/workmux/config.yaml
# All options below are commented out - uncomment to override defaults.

#-------------------------------------------------------------------------------
# Appearance
#-------------------------------------------------------------------------------

# Color scheme for the dashboard. Press T (shift+t) in the dashboard to cycle.
# Options: default, emberforge, glacier-signal, obsidian-pop, slate-garden,
#          phosphor-arcade, lasergrid, mossfire, night-sorbet, graphite-code,
#          festival-circuit, teal-drift
# theme: default
#
# Or with explicit dark/light mode (otherwise auto-detected from terminal):
# theme:
#   scheme: emberforge
#   mode: dark

#-------------------------------------------------------------------------------
# Git
#-------------------------------------------------------------------------------

# The primary branch to merge into.
# Default: Auto-detected from remote HEAD, falls back to main/master.
# main_branch: main

# Default base branch/commit to branch from when creating new worktrees.
# Use "auto" to branch from main_branch or the locally detected main/master branch.
# The --base CLI flag always overrides this.
# Default: The currently checked out branch.
# base_branch: auto

# Default merge strategy for `workmux merge`.
# Options: merge (default), rebase, squash
# CLI flags (--rebase, --squash) always override this.
# merge_strategy: rebase

# Keep the worktree, window, and branch after `workmux merge` by default.
# Keep and cleanup CLI flags always override this.
# merge_keep: true

#-------------------------------------------------------------------------------
# Naming & Paths
#-------------------------------------------------------------------------------

# Directory where worktrees are created.
# Can be relative to repo root or absolute. Supports `~` for home directory
# and `{project}` for the main worktree's directory name, so a global config
# can namespace each repo, e.g. `~/.workmux/{project}`.
# Default: Sibling directory '<project>__worktrees'.
# worktree_dir: .worktrees

# Strategy for deriving names from branch names.
# Options: full (default), basename (part after last '/').
# worktree_naming: basename

# Prefix added to worktree directories and tmux window names.
# worktree_prefix: ""

# Prefix for tmux window and session names. Supports `{project}` for the main
# worktree's directory name, e.g. `{project} ` to scope session names by repo.
# Default: "wm-"
# window_prefix: "wm-"

#-------------------------------------------------------------------------------
# Tmux
#-------------------------------------------------------------------------------

# Mode for tmux operations: window (default) or session.
# - window: Create windows within the current tmux session
# - session: Create tmux sessions for each worktree
# mode: session

# Placement for new tmux windows in window mode.
# Options: after_current (default), rightmost
# window_placement: rightmost

# Custom tmux pane layout (mutually exclusive with 'windows').
# Default: Two-pane layout with shell and clear command.
# panes:
#   - command: pnpm install
#     name: deps
#     focus: true
#   - split: horizontal
#   - command: clear
#     split: vertical
#     size: 5
#   - command: just test --watch
#     split: stacked  # Zellij only

# Multiple windows per session (session mode only, mutually exclusive with 'panes').
# Each window can have its own pane layout. Unnamed windows get tmux's
# automatic naming based on the running command.
# windows:
#   - name: editor
#     panes:
#       - command: <agent>
#         focus: true
#       - split: horizontal
#         size: 20
#   - name: tests
#     panes:
#       - command: just test --watch
#   - panes:
#       - command: tail -f app.log

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

# LLM-based branch name generation (`workmux add -A`).
# auto_name:
#   model: "gpt-4o-mini"
#   system_prompt: "Generate a kebab-case git branch name."
#   background: true  # Always run in background when using --auto-name

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
# Use "<global>" to inherit from global config.
# Set to empty list to disable: `pre_remove: []`
# Environment variables available:
#   - WM_HANDLE: The worktree handle (directory name)
#   - WM_WORKTREE_PATH: Absolute path of the worktree being deleted
#   - WM_PROJECT_ROOT: Absolute path of the main project directory
# pre_remove:
#   - "<global>"
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
# Columns of the agents table, in display order. Omit a column to hide it:
# number, project, worktree, git, pr, status, time, title.
# Default sort mode for the agent list: priority (default), project, recency, natural.
# Used only when no sort preference has been persisted in the dashboard.
# dashboard:
#   commit: "Commit staged changes with a descriptive message"
#   merge: "!workmux merge"
#   preview_size: 60
#   agent_columns: [number, project, worktree, git, pr, status, time, title]
#   sort_mode: priority
#   close_on_jump: true

#-------------------------------------------------------------------------------
# Sidebar
#-------------------------------------------------------------------------------

# sidebar:
#   # Position: left (default) or top.
#   position: left
#
#   # Left sidebar width: absolute columns or percentage of terminal width.
#   # Default: "10%" (clamped to 25-50 columns).
#   # Explicit values are not clamped (minimum 10 columns).
#   width: 40       # absolute columns
#   # width: "15%"  # percentage of terminal width
#
#   # Top bar height in rows.
#   height: 3
#
#   # Layout mode for the left sidebar: "compact" or "tiles" (cards).
#   # Default: "tiles". Can be toggled at runtime with 'v' key.
#   layout: tiles
#
#   # Dim agents whose status has not changed for one hour. Default: true.
#   dim_stale: true
#
#   horizontal:
#     item_width: 24  # horizontal chip width in columns, clamped 12-80
#
#   templates:
#     horizontal:
#       - "{status_icon} {primary} {pane_suffix} {fill} {elapsed}"
#       - "{secondary} {fill} {git_stats}"
#       - "{pane_title}"

#-------------------------------------------------------------------------------
# Sandbox
#-------------------------------------------------------------------------------

# sandbox:
#   enabled: false
#   backend: lima
#   # host_commands: ["just", "cargo", "npm"]
#   # container:
#   #   runtime: docker          # docker | podman | apple-container
#   #   # memory: 16G            # VM memory limit (apple-container default: 16G)
#   #   # cpus: 4                # VM CPU count (only passed when set)
#   #   # OCI runtime to run under (docker/local Podman --runtime). Unset omits
#   #   # the flag and uses the engine's configured default.
#   #   # Set to e.g. "kata" for VM-based isolation (runtime must be installed
#   #   # and registered). GLOBAL-ONLY: ignored in a project .workmux.yaml.
#   #   # oci_runtime: kata
#   #   # Extra capabilities / security options (docker/podman --cap-add /
#   #   # --security-opt). Empty by default and GLOBAL-ONLY. Permissive values
#   #   # weaken host-kernel container isolation unless paired with a VM runtime.
#   #   # Main use: docker-in-docker under oci_runtime: kata.
#   #   # cap_add: [ALL]
#   #   # security_opt: [seccomp=unconfined, apparmor=unconfined, systempaths=unconfined]
#   #   # Mask files out of the worktree bind mounts (paths relative to the
#   #   # worktree root). Each listed file is shadowed by /dev/null so the
#   #   # sandboxed agent cannot read it. Missing files are skipped.
#   #   # GLOBAL-ONLY: ignored when set in a project .workmux.yaml.
#   #   # excluded_files:
#   #   #   - .env
#   #   #   - .env.local
#   # lima:
#   #   isolation: project
#   #   cpus: 4
#   #   memory: 4GiB
#   #   # Custom provision script (runs once on VM creation, as user).
#   #   # Use sudo for system commands.
#   #   # provision: |
#   #   #   sudo apt-get install -y ripgrep fd-find jq
#   # Extra mount points (read-only by default).
#   # Supports simple paths or detailed specs with guest_path and writable.
#   # extra_mounts:
#   #   - ~/my-notes
#   #   - host_path: ~/data
#   #     guest_path: /mnt/data
#   #     writable: true
"#;

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

/// Checks if a command string corresponds to the given agent command.
///
/// Returns true if:
/// 1. The command is the literal placeholder "<agent>"
/// 2. The command's executable stem matches the agent's executable stem
///    (e.g., "claude" matches "/usr/bin/claude")
///
/// Looks past `env` wrappers and `VAR=value` assignments to find the
/// real executable in both the command and agent strings.
pub fn is_agent_command(command_line: &str, agent_command: &str) -> bool {
    use crate::multiplexer::agent::find_executable_token;

    let trimmed = command_line.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Allow <agent> token regardless of what follows (e.g., "<agent> --verbose")
    let cmd_token = find_executable_token(trimmed);
    if cmd_token == "<agent>" {
        return true;
    }

    let agent_token = find_executable_token(agent_command);
    if agent_token.is_empty() {
        return false;
    }

    let resolved_cmd = resolve_executable_path(&cmd_token).unwrap_or_else(|| cmd_token.to_string());
    let resolved_agent =
        resolve_executable_path(&agent_token).unwrap_or_else(|| agent_token.to_string());

    let cmd_stem = Path::new(&resolved_cmd).file_stem();
    let agent_stem = Path::new(&resolved_agent).file_stem();

    cmd_stem.is_some() && cmd_stem == agent_stem
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        AgentColumn, AgentEnvValue, AgentIconConfig, AgentIconDetails, AllowedDomainDetails,
        AllowedDomainEntry, Config, ContainerConfig, ContainerDevice, DEFAULT_AGENT_COLUMNS,
        ExtraMount, FileConfig, LayoutConfig, LimaConfig, NetworkConfig, NetworkPolicy, PaneConfig,
        SandboxConfig, SandboxRuntime, SandboxTarget, SidebarHeight, SidebarPosition, SidebarWidth,
        SplitDirection, ToolchainMode, WindowPlacement, is_agent_command, validate_domain,
        validate_group_add_entry, validate_layouts_config,
    };
    use crate::test_support;
    use tempfile::TempDir;

    #[test]
    fn dashboard_closes_on_jump_by_default() {
        assert!(Config::default().dashboard.close_on_jump());
    }

    #[test]
    fn dashboard_close_on_jump_can_be_disabled() {
        let config: Config =
            serde_yaml::from_str("dashboard:\n  close_on_jump: false\n").expect("config parses");
        assert!(!config.dashboard.close_on_jump());
    }

    #[test]
    fn dashboard_close_on_jump_project_value_overrides_global() {
        let global: Config = serde_yaml::from_str("dashboard:\n  close_on_jump: false\n")
            .expect("global config parses");
        let project: Config = serde_yaml::from_str("dashboard:\n  close_on_jump: true\n")
            .expect("project config parses");
        assert!(global.merge(project).dashboard.close_on_jump());
    }

    #[test]
    fn dashboard_close_on_jump_inherits_global_value() {
        let global: Config = serde_yaml::from_str("dashboard:\n  close_on_jump: false\n")
            .expect("global config parses");
        assert!(!global.merge(Config::default()).dashboard.close_on_jump());
    }

    #[test]
    fn agent_columns_default_when_unset() {
        let config = Config::default();
        assert_eq!(
            config.dashboard.agent_columns(),
            vec![
                AgentColumn::Number,
                AgentColumn::Project,
                AgentColumn::Worktree,
                AgentColumn::Git,
                AgentColumn::Pr,
                AgentColumn::Status,
                AgentColumn::Time,
                AgentColumn::Title
            ]
        );
    }

    #[test]
    fn agent_columns_follow_configured_order() {
        let config: Config = serde_yaml::from_str(
            "dashboard:\n  agent_columns: [title, status, number, worktree, git, pr, project, time]\n",
        )
        .expect("config parses");
        assert_eq!(
            config.dashboard.agent_columns(),
            vec![
                AgentColumn::Title,
                AgentColumn::Status,
                AgentColumn::Number,
                AgentColumn::Worktree,
                AgentColumn::Git,
                AgentColumn::Pr,
                AgentColumn::Project,
                AgentColumn::Time
            ]
        );
    }

    #[test]
    fn agent_columns_drop_duplicates_and_allow_omitting() {
        let config: Config =
            serde_yaml::from_str("dashboard:\n  agent_columns: [title, title, status]\n")
                .expect("config parses");
        assert_eq!(
            config.dashboard.agent_columns(),
            vec![AgentColumn::Title, AgentColumn::Status]
        );
    }

    #[test]
    fn agent_columns_empty_list_falls_back_to_default() {
        let config: Config =
            serde_yaml::from_str("dashboard:\n  agent_columns: []\n").expect("config parses");
        assert_eq!(config.dashboard.agent_columns(), DEFAULT_AGENT_COLUMNS);
    }

    #[test]
    fn agent_columns_project_overrides_global() {
        let global: Config = serde_yaml::from_str("dashboard:\n  agent_columns: [status, title]\n")
            .expect("config parses");
        let project: Config =
            serde_yaml::from_str("dashboard:\n  agent_columns: [title, status]\n")
                .expect("config parses");
        assert_eq!(
            global.merge(project).dashboard.agent_columns(),
            vec![AgentColumn::Title, AgentColumn::Status]
        );
    }

    #[test]
    fn agent_columns_inherit_global_when_project_unset() {
        let global: Config = serde_yaml::from_str("dashboard:\n  agent_columns: [status, title]\n")
            .expect("config parses");
        assert_eq!(
            global.merge(Config::default()).dashboard.agent_columns(),
            vec![AgentColumn::Status, AgentColumn::Title]
        );
    }

    fn cfg(edit: impl FnOnce(&mut Config)) -> Config {
        let mut config = Config::default();
        edit(&mut config);
        config
    }

    fn sandbox_cfg(edit: impl FnOnce(&mut SandboxConfig)) -> Config {
        cfg(|config| edit(&mut config.sandbox))
    }

    fn lima_cfg(edit: impl FnOnce(&mut LimaConfig)) -> Config {
        sandbox_cfg(|sandbox| edit(&mut sandbox.lima))
    }

    fn container_cfg(edit: impl FnOnce(&mut ContainerConfig)) -> Config {
        sandbox_cfg(|sandbox| edit(&mut sandbox.container))
    }

    #[test]
    fn frozen_source_bypasses_live_config_discovery() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".workmux.yaml"),
            "status_icons:\n  working: hostile\n",
        )
        .unwrap();

        let trusted = Config {
            selected_agent: Some("trusted-agent".to_string()),
            agent_type: Some("claude".to_string()),
            status_icons: super::StatusIcons {
                working: Some("trusted".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let snapshot =
            crate::frozen_config::FrozenConfigGuard::capture_in(&trusted, None, root.path())
                .unwrap();

        let (loaded, location) = Config::load_with_location_from_sources(
            root.path(),
            Some("guest-agent"),
            None,
            Some(snapshot.path()),
            false,
        )
        .unwrap();

        assert_eq!(loaded.status_icons.working.as_deref(), Some("trusted"));
        assert_eq!(loaded.selected_agent.as_deref(), Some("trusted-agent"));
        assert_eq!(loaded.agent_type.as_deref(), Some("claude"));
        assert!(location.is_none());
    }

    #[test]
    fn frozen_source_rejects_config_override() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = crate::frozen_config::FrozenConfigGuard::capture_in(
            &Config::default(),
            None,
            root.path(),
        )
        .unwrap();
        let override_path = root.path().join("other.yaml");
        std::fs::write(&override_path, "{}").unwrap();

        let error = Config::load_with_location_from_sources(
            root.path(),
            None,
            Some(&override_path),
            Some(snapshot.path()),
            false,
        )
        .unwrap_err();

        assert!(error.to_string().contains("cannot be combined"));
    }

    #[test]
    fn frozen_source_rejects_guest_mode() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = crate::frozen_config::FrozenConfigGuard::capture_in(
            &Config::default(),
            None,
            root.path(),
        )
        .unwrap();

        let error = Config::load_with_location_from_sources(
            root.path(),
            None,
            None,
            Some(snapshot.path()),
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("only valid for host commands"));
    }

    #[test]
    fn frozen_source_env_is_authoritative_in_child_process() {
        const OUTPUT_ENV: &str = "WORKMUX_FROZEN_CONFIG_TEST_OUTPUT";

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".workmux.yaml"),
            "status_icons:\n  working: hostile\n",
        )
        .unwrap();
        let trusted = Config {
            status_icons: super::StatusIcons {
                working: Some("trusted".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let snapshot =
            crate::frozen_config::FrozenConfigGuard::capture_in(&trusted, None, root.path())
                .unwrap();
        let output_path = root.path().join("loaded-icon");

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "config::tests::frozen_source_env_subprocess_helper",
                "--nocapture",
            ])
            .env(crate::frozen_config::FROZEN_CONFIG_ENV, snapshot.path())
            .env(OUTPUT_ENV, &output_path)
            .env_remove("WM_SANDBOX_GUEST")
            .current_dir(root.path())
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(output_path).unwrap(), "trusted");
    }

    #[test]
    fn frozen_source_env_subprocess_helper() {
        const OUTPUT_ENV: &str = "WORKMUX_FROZEN_CONFIG_TEST_OUTPUT";
        let Some(output_path) = std::env::var_os(OUTPUT_ENV) else {
            return;
        };

        let config = Config::load(None).unwrap();
        std::fs::write(
            output_path,
            config
                .status_icons
                .working
                .expect("working icon in snapshot"),
        )
        .unwrap();
    }

    fn sandbox_extra_mounts(paths: &[&str]) -> Config {
        sandbox_cfg(|sandbox| {
            sandbox.extra_mounts = Some(
                paths
                    .iter()
                    .map(|path| ExtraMount::Path((*path).to_string()))
                    .collect(),
            );
        })
    }

    fn sandbox_agent_config_dir(dir: &str) -> Config {
        sandbox_cfg(|sandbox| sandbox.agent_config_dir = Some(dir.to_string()))
    }

    fn sandbox_network(policy: NetworkPolicy, domains: &[&str]) -> Config {
        sandbox_cfg(|sandbox| {
            sandbox.network = NetworkConfig {
                policy: Some(policy),
                allowed_domains: if domains.is_empty() {
                    None
                } else {
                    Some(
                        domains
                            .iter()
                            .map(|domain| AllowedDomainEntry::Plain((*domain).to_string()))
                            .collect(),
                    )
                },
                ..Default::default()
            };
        })
    }

    fn theme_cfg(edit: impl FnOnce(&mut super::ThemeConfig)) -> Config {
        cfg(|config| edit(&mut config.theme))
    }

    fn pane(command: &str) -> PaneConfig {
        PaneConfig {
            command: Some(command.into()),
            ..Default::default()
        }
    }

    fn split_pane(command: &str) -> PaneConfig {
        PaneConfig {
            command: Some(command.into()),
            split: Some(SplitDirection::Horizontal),
            ..Default::default()
        }
    }

    fn git_tempdir() -> TempDir {
        let temp = TempDir::new().unwrap();
        let mut command = std::process::Command::new("git");
        test_support::clear_local_git_env(&mut command);
        command
            .args(["init"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        temp
    }

    #[test]
    fn merge_keep_parses_boolean_values() {
        let enabled: Config = serde_yaml::from_str("merge_keep: true").unwrap();
        assert_eq!(enabled.merge_keep, Some(true));

        let disabled: Config = serde_yaml::from_str("merge_keep: false").unwrap();
        assert_eq!(disabled.merge_keep, Some(false));
    }

    #[test]
    fn window_placement_parses_values() {
        let after_current: Config =
            serde_yaml::from_str("window_placement: after_current").unwrap();
        assert_eq!(
            after_current.window_placement(),
            WindowPlacement::AfterCurrent
        );

        let rightmost: Config = serde_yaml::from_str("window_placement: rightmost").unwrap();
        assert_eq!(rightmost.window_placement(), WindowPlacement::Rightmost);
    }

    #[test]
    fn missing_global_lists_expand_placeholder_to_empty() {
        let project = Config {
            post_create: Some(vec![
                "post-before".to_string(),
                "<global>".to_string(),
                "post-after".to_string(),
            ]),
            pre_merge: Some(vec!["<global>".to_string()]),
            pre_remove: Some(vec![
                "remove-before".to_string(),
                "<global>".to_string(),
                "remove-after".to_string(),
            ]),
            files: FileConfig {
                copy: Some(vec!["<global>".to_string()]),
                symlink: Some(vec![
                    "symlink-before".to_string(),
                    "<global>".to_string(),
                    "symlink-after".to_string(),
                ]),
            },
            ..Default::default()
        };

        let merged = Config::default().merge(project);

        assert_eq!(
            merged.post_create,
            Some(vec!["post-before".to_string(), "post-after".to_string()])
        );
        assert_eq!(merged.pre_merge, Some(vec![]));
        assert_eq!(
            merged.pre_remove,
            Some(vec![
                "remove-before".to_string(),
                "remove-after".to_string()
            ])
        );
        assert_eq!(merged.files.copy, Some(vec![]));
        assert_eq!(
            merged.files.symlink,
            Some(vec![
                "symlink-before".to_string(),
                "symlink-after".to_string()
            ])
        );
    }

    #[test]
    fn window_placement_project_overrides_global() {
        let global = Config {
            window_placement: Some(WindowPlacement::AfterCurrent),
            ..Default::default()
        };
        let project = Config {
            window_placement: Some(WindowPlacement::Rightmost),
            ..Default::default()
        };

        let merged = global.merge(project);
        assert_eq!(merged.window_placement(), WindowPlacement::Rightmost);
    }

    #[test]
    fn merge_keep_project_false_overrides_global_true() {
        let global = Config {
            merge_keep: Some(true),
            ..Default::default()
        };
        let project = Config {
            merge_keep: Some(false),
            ..Default::default()
        };

        let merged = global.merge(project);
        assert_eq!(merged.merge_keep, Some(false));
    }

    #[test]
    fn agent_icon_config_parses_legacy_string() {
        let v: AgentIconConfig = serde_yaml::from_str("\"C\"").unwrap();
        assert_eq!(v, AgentIconConfig::Plain("C".to_string()));
    }

    #[test]
    fn agent_icon_config_parses_detailed_with_both() {
        let v: AgentIconConfig = serde_yaml::from_str("{ icon: \"X\", color: \"#fff\" }").unwrap();
        assert_eq!(
            v,
            AgentIconConfig::Detailed(AgentIconDetails {
                icon: Some("X".to_string()),
                color: Some("#fff".to_string()),
            })
        );
    }

    #[test]
    fn agent_icon_config_parses_detailed_with_color_only() {
        let v: AgentIconConfig = serde_yaml::from_str("{ color: red }").unwrap();
        assert_eq!(
            v,
            AgentIconConfig::Detailed(AgentIconDetails {
                icon: None,
                color: Some("red".to_string()),
            })
        );
    }

    #[test]
    fn agent_icon_config_parses_empty_object_as_detailed() {
        let v: AgentIconConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(
            v,
            AgentIconConfig::Detailed(AgentIconDetails {
                icon: None,
                color: None,
            })
        );
    }

    #[test]
    fn agent_icon_config_parses_null_as_null_variant() {
        let v: AgentIconConfig = serde_yaml::from_str("~").unwrap();
        assert_eq!(v, AgentIconConfig::Null);
    }

    #[test]
    fn agent_icon_config_rejects_unknown_field() {
        let err = serde_yaml::from_str::<AgentIconConfig>("{ colour: red }");
        assert!(err.is_err(), "expected unknown-field rejection");
    }

    #[test]
    fn agent_icons_merge_extends_per_key() {
        // Project key overrides global for that key; other global keys survive.
        let yaml_global = r##"
sidebar:
  agent_icons:
    claude: "C"
    codex:
      color: cyan
"##;
        let yaml_project = r##"
sidebar:
  agent_icons:
    claude:
      color: "#ff8c00"
"##;
        let global: Config = serde_yaml::from_str(yaml_global).unwrap();
        let project: Config = serde_yaml::from_str(yaml_project).unwrap();
        let merged = global.merge(project);
        let icons = merged.sidebar.agent_icons.unwrap();
        // codex (only in global) survives.
        assert!(icons.contains_key("codex"));
        // claude is replaced by the project entry (per-key replacement).
        assert_eq!(
            icons.get("claude"),
            Some(&AgentIconConfig::Detailed(AgentIconDetails {
                icon: None,
                color: Some("#ff8c00".to_string()),
            }))
        );
    }

    #[test]
    fn sidebar_position_and_height_parse() {
        let yaml = r#"
sidebar:
  position: top
  height: "10%"
  horizontal:
    item_width: 32
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.sidebar.position, Some(SidebarPosition::Top));
        assert_eq!(config.sidebar.height, Some(SidebarHeight::Percent(10)));
        assert_eq!(config.sidebar.horizontal.item_width, Some(32));
        assert_eq!(config.sidebar.horizontal.item_width(), 32);
    }

    #[test]
    fn sidebar_position_and_height_merge_per_field() {
        let global: Config = serde_yaml::from_str(
            r#"
sidebar:
  position: top
  width: 40
  height: 3
  horizontal:
    item_width: 36
"#,
        )
        .unwrap();
        let project: Config = serde_yaml::from_str(
            r#"
sidebar:
  height: 4
"#,
        )
        .unwrap();

        let merged = global.merge(project);

        assert_eq!(merged.sidebar.position, Some(SidebarPosition::Top));
        assert_eq!(merged.sidebar.width, Some(SidebarWidth::Absolute(40)));
        assert_eq!(merged.sidebar.height, Some(SidebarHeight::Absolute(4)));
        assert_eq!(merged.sidebar.horizontal.item_width, Some(36));
    }

    #[test]
    fn sidebar_dim_stale_defaults_true_and_merges_per_field() {
        let default_config = Config::default();
        assert!(default_config.sidebar.dim_stale());

        let global: Config = serde_yaml::from_str(
            r#"
sidebar:
  dim_stale: false
"#,
        )
        .unwrap();
        let project: Config = serde_yaml::from_str("sidebar: {}\n").unwrap();
        let merged = global.merge(project);

        assert_eq!(merged.sidebar.dim_stale, Some(false));
        assert!(!merged.sidebar.dim_stale());
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

    #[test]
    fn is_agent_command_env_wrapped() {
        assert!(is_agent_command("env -u FOO claude", "claude"));
        assert!(is_agent_command("claude", "env -u FOO claude"));
        assert!(is_agent_command("env -u FOO claude", "env -u BAR claude"));
        assert!(is_agent_command("FOO=bar claude", "claude"));
    }

    #[test]
    fn is_agent_command_env_wrapped_mismatch() {
        assert!(!is_agent_command("env -u FOO claude", "gemini"));
        assert!(!is_agent_command("env -u FOO vim", "claude"));
    }

    #[test]
    fn agents_deserialize_string_form() {
        let yaml = r#"
agents:
  cc-work: "claude --dangerously-skip-permissions"
  cc-bedrock: "env -u CLAUDE_CODE_USE_BEDROCK claude"
  cod: "codex --yolo"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.agents.len(), 3);
        assert_eq!(
            config.agents.get("cc-work").unwrap().command.as_deref(),
            Some("claude --dangerously-skip-permissions")
        );
        assert!(config.agents.get("cc-work").unwrap().agent_type.is_none());
        assert_eq!(
            config.agents.get("cc-bedrock").unwrap().command.as_deref(),
            Some("env -u CLAUDE_CODE_USE_BEDROCK claude")
        );
        assert_eq!(
            config.agents.get("cod").unwrap().command.as_deref(),
            Some("codex --yolo")
        );
    }

    #[test]
    fn agents_deserialize_map_form_with_type() {
        let yaml = r#"
agents:
  cc-smart:
    command: "/path/to/smart-picker"
    type: claude
  cod-plain: "codex"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.agents.len(), 2);
        let smart = config.agents.get("cc-smart").unwrap();
        assert_eq!(smart.command.as_deref(), Some("/path/to/smart-picker"));
        assert_eq!(smart.agent_type.as_deref(), Some("claude"));
        let cod = config.agents.get("cod-plain").unwrap();
        assert_eq!(cod.command.as_deref(), Some("codex"));
        assert!(cod.agent_type.is_none());
    }

    #[test]
    fn agents_deserialize_env_from_env() {
        let yaml = r#"
agents:
  cc-deepseek:
    type: claude
    command: claude
    env:
      ANTHROPIC_AUTH_TOKEN:
        from_env: DEEPSEEK_API_KEY
      ANTHROPIC_BASE_URL: https://api.deepseek.com/anthropic
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let entry = config.agents.get("cc-deepseek").unwrap();
        assert_eq!(
            entry.env.get("ANTHROPIC_AUTH_TOKEN"),
            Some(&AgentEnvValue::FromEnv {
                from_env: "DEEPSEEK_API_KEY".to_string(),
            })
        );
        assert_eq!(
            entry.env.get("ANTHROPIC_BASE_URL"),
            Some(&AgentEnvValue::Literal(
                "https://api.deepseek.com/anthropic".to_string()
            ))
        );
    }

    #[test]
    fn agents_reject_invalid_env_names() {
        let bad_key = r#"
agents:
  cc-bad:
    type: claude
    env:
      BAD-NAME: value
"#;
        assert!(serde_yaml::from_str::<Config>(bad_key).is_err());

        let bad_from_env = r#"
agents:
  cc-bad:
    type: claude
    env:
      TOKEN:
        from_env: BAD-NAME
"#;
        assert!(serde_yaml::from_str::<Config>(bad_from_env).is_err());
    }

    #[test]
    fn agents_empty_by_default() {
        let yaml = "agent: claude";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.agents.is_empty());
    }

    use super::find_project_config;
    use std::fs;

    #[test]
    fn find_project_config_from_subdir() {
        let temp = git_tempdir();
        let root = temp.path();

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
        let backend_config = backend.join(".workmux.yaml").canonicalize().unwrap();
        assert_eq!(loc.config_path, backend_config);
        assert_eq!(loc.config_dir, backend.canonicalize().unwrap());
        assert_eq!(loc.rel_dir, std::path::PathBuf::from("backend"));
    }

    #[test]
    fn find_project_config_nearest_wins() {
        let temp = git_tempdir();
        let root = temp.path();

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
        let backend_config = backend.join(".workmux.yaml").canonicalize().unwrap();
        assert_eq!(loc.config_path, backend_config);
        assert_eq!(loc.config_dir, backend.canonicalize().unwrap());
    }

    #[test]
    fn sandbox_config_defaults() {
        let config = SandboxConfig::default();
        assert!(!config.is_enabled());
        assert_eq!(config.target(), SandboxTarget::Agent);
        assert!(config.env_passthrough().is_empty());
    }

    #[test]
    fn sandbox_runtime_explicit_overrides_detect() {
        let config = ContainerConfig {
            runtime: Some(SandboxRuntime::Podman),
            ..Default::default()
        };
        assert_eq!(config.runtime(), SandboxRuntime::Podman);

        let config = ContainerConfig {
            runtime: Some(SandboxRuntime::Docker),
            ..Default::default()
        };
        assert_eq!(config.runtime(), SandboxRuntime::Docker);
    }

    #[test]
    fn sandbox_runtime_detect_when_unset() {
        let config = ContainerConfig {
            runtime: None,
            ..Default::default()
        };
        // Should auto-detect from PATH; result depends on environment
        // but should not panic
        let _runtime = config.runtime();
    }

    #[test]
    fn sandbox_config_merge() {
        let global = Config {
            sandbox: SandboxConfig {
                enabled: Some(true),
                container: ContainerConfig {
                    runtime: Some(SandboxRuntime::Docker),
                    ..Default::default()
                },
                image: Some("global-image".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let project = Config {
            sandbox: SandboxConfig {
                image: Some("project-image".to_string()),
                container: ContainerConfig {
                    runtime: Some(SandboxRuntime::Podman),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        let merged = global.merge(project);
        assert!(merged.sandbox.is_enabled()); // from global
        assert_eq!(merged.sandbox.resolved_image("claude"), "project-image"); // project overrides global
        assert_eq!(merged.sandbox.runtime(), SandboxRuntime::Podman); // from project
    }

    #[test]
    fn sandbox_provision_merge_override() {
        let global = lima_cfg(|lima| lima.provision = Some("echo global".into()));
        let project = lima_cfg(|lima| lima.provision = Some("echo project".into()));
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.lima.provision_script(), Some("echo project"));
    }

    #[test]
    fn sandbox_provision_merge_fallback() {
        let global = lima_cfg(|lima| lima.provision = Some("echo global".into()));
        let project = Config::default();
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.lima.provision_script(), Some("echo global"));
    }

    #[test]
    fn sandbox_provision_empty_disables_global() {
        let global = lima_cfg(|lima| lima.provision = Some("echo global".into()));
        let project = lima_cfg(|lima| lima.provision = Some("".into()));
        let merged = global.merge(project);
        // Empty string wins over global (project explicitly set it)
        assert_eq!(merged.sandbox.lima.provision, Some("".to_string()));
        // But provision_script() filters it out
        assert_eq!(merged.sandbox.lima.provision_script(), None);
    }

    #[test]
    fn sandbox_skip_default_provision_defaults_false() {
        let config = LimaConfig::default();
        assert!(!config.skip_default_provision());
    }

    #[test]
    fn sandbox_skip_default_provision_merge() {
        let global = lima_cfg(|lima| lima.skip_default_provision = Some(true));
        let project = Config::default();
        let merged = global.merge(project);
        assert!(merged.sandbox.lima.skip_default_provision());
    }

    #[test]
    fn sandbox_skip_default_provision_project_overrides() {
        let global = lima_cfg(|lima| lima.skip_default_provision = Some(true));
        let project = lima_cfg(|lima| lima.skip_default_provision = Some(false));
        let merged = global.merge(project);
        assert!(!merged.sandbox.lima.skip_default_provision());
    }

    #[test]
    fn test_rpc_host_address_defaults() {
        assert_eq!(
            SandboxRuntime::Docker.rpc_host_address(),
            "host.docker.internal"
        );
        assert_eq!(
            SandboxRuntime::Podman.rpc_host_address(),
            "host.containers.internal"
        );
    }

    #[test]
    fn test_resolved_rpc_host_uses_override() {
        let config = SandboxConfig {
            rpc_host: Some("custom.host.local".to_string()),
            ..Default::default()
        };
        assert_eq!(config.resolved_rpc_host(), "custom.host.local");
    }

    #[test]
    fn test_resolved_rpc_host_falls_back_to_runtime() {
        let config = SandboxConfig {
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Podman),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(config.resolved_rpc_host(), "host.containers.internal");
    }

    #[test]
    fn sandbox_toolchain_defaults_to_auto() {
        let config = SandboxConfig::default();
        assert_eq!(config.toolchain(), ToolchainMode::Auto);
    }

    #[test]
    fn sandbox_toolchain_merge_project_overrides() {
        let global = Config {
            sandbox: SandboxConfig {
                toolchain: Some(ToolchainMode::Auto),
                ..Default::default()
            },
            ..Default::default()
        };
        let project = Config {
            sandbox: SandboxConfig {
                toolchain: Some(ToolchainMode::Off),
                ..Default::default()
            },
            ..Default::default()
        };
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.toolchain(), ToolchainMode::Off);
    }

    #[test]
    fn sandbox_toolchain_merge_fallback_to_global() {
        let global = Config {
            sandbox: SandboxConfig {
                toolchain: Some(ToolchainMode::Devbox),
                ..Default::default()
            },
            ..Default::default()
        };
        let project = Config::default();
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.toolchain(), ToolchainMode::Devbox);
    }

    #[test]
    fn test_sandbox_host_commands_default_empty() {
        let config = SandboxConfig::default();
        assert!(config.host_commands().is_empty());
    }

    #[test]
    fn test_sandbox_host_commands_global_only() {
        // Project config is ignored -- only global matters
        let global = sandbox_cfg(|sandbox| {
            sandbox.host_commands = Some(vec!["just".into(), "cargo".into()]);
        });
        let project = sandbox_cfg(|sandbox| {
            sandbox.host_commands = Some(vec!["npm".into()]);
        });
        let merged = global.merge(project);
        assert_eq!(
            merged.sandbox.host_commands(),
            &["just".to_string(), "cargo".to_string()]
        );
    }

    #[test]
    fn test_sandbox_host_commands_project_ignored_when_no_global() {
        let global = Config::default(); // no host_commands
        let project = sandbox_cfg(|sandbox| sandbox.host_commands = Some(vec!["rm".into()]));
        let merged = global.merge(project);
        assert!(merged.sandbox.host_commands().is_empty());
    }

    #[test]
    fn test_sandbox_host_commands_uses_global() {
        let global = sandbox_cfg(|sandbox| sandbox.host_commands = Some(vec!["just".into()]));
        let project = Config::default();
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.host_commands(), &["just".to_string()]);
    }

    #[test]
    fn container_oci_runtime_is_global_only() {
        // oci_runtime changes the isolation boundary, so a project .workmux.yaml
        // must not be able to override the global value.
        let global = container_cfg(|c| c.oci_runtime = Some("kata".into()));
        let project = container_cfg(|c| c.oci_runtime = Some("runc".into()));
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.container.oci_runtime(), Some("kata"));
    }

    #[test]
    fn container_cap_add_and_security_opt_are_global_only() {
        // Capabilities / security options relax confinement, so a project
        // .workmux.yaml must not be able to override the global values.
        let global = container_cfg(|c| {
            c.cap_add = Some(vec!["ALL".into()]);
            c.security_opt = Some(vec!["seccomp=unconfined".into()]);
        });
        let project = container_cfg(|c| {
            c.cap_add = Some(vec!["NET_ADMIN".into()]);
            c.security_opt = Some(vec!["seccomp=default".into()]);
        });
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.container.cap_add(), &["ALL".to_string()]);
        assert_eq!(
            merged.sandbox.container.security_opt(),
            &["seccomp=unconfined".to_string()]
        );
    }

    #[test]
    fn container_cap_add_project_ignored_when_no_global() {
        let global = Config::default();
        let project = container_cfg(|c| c.cap_add = Some(vec!["ALL".into()]));
        let merged = global.merge(project);
        assert!(merged.sandbox.container.cap_add().is_empty());
        assert!(merged.sandbox.container.security_opt().is_empty());
    }

    #[test]
    fn container_oci_runtime_project_ignored_when_no_global() {
        let global = Config::default();
        let project = container_cfg(|c| c.oci_runtime = Some("kata".into()));
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.container.oci_runtime(), None);
    }

    #[test]
    fn container_oci_runtime_uses_global() {
        let global = container_cfg(|c| c.oci_runtime = Some("kata".into()));
        let project = Config::default();
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.container.oci_runtime(), Some("kata"));
    }

    #[test]
    fn test_allow_unsandboxed_host_exec_defaults_false() {
        let config = SandboxConfig::default();
        assert!(!config.allow_unsandboxed_host_exec());
    }

    #[test]
    fn test_allow_unsandboxed_host_exec_global_only() {
        let global = sandbox_cfg(|sandbox| {
            sandbox.dangerously_allow_unsandboxed_host_exec = Some(true);
        });
        // Project tries to set it -- should be ignored
        let project = sandbox_cfg(|sandbox| {
            sandbox.dangerously_allow_unsandboxed_host_exec = Some(false);
        });
        let merged = global.merge(project);
        assert!(merged.sandbox.allow_unsandboxed_host_exec());
    }

    #[test]
    fn test_allow_unsandboxed_host_exec_not_set_in_project() {
        let global = Config::default();
        let project = sandbox_cfg(|sandbox| {
            sandbox.dangerously_allow_unsandboxed_host_exec = Some(true);
        });
        let merged = global.merge(project);
        // Project value should be ignored
        assert!(!merged.sandbox.allow_unsandboxed_host_exec());
    }

    #[test]
    fn test_sandbox_rpc_host_global_only() {
        // Project config is ignored -- only global matters
        let global = sandbox_cfg(|sandbox| sandbox.rpc_host = Some("trusted.host".into()));
        let project = sandbox_cfg(|sandbox| sandbox.rpc_host = Some("evil.attacker.com".into()));
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.rpc_host, Some("trusted.host".to_string()));
    }

    #[test]
    fn test_sandbox_rpc_host_project_ignored_when_no_global() {
        let global = Config::default(); // no rpc_host
        let project = sandbox_cfg(|sandbox| sandbox.rpc_host = Some("evil.attacker.com".into()));
        let merged = global.merge(project);
        assert!(merged.sandbox.rpc_host.is_none());
    }

    #[test]
    fn test_sandbox_rpc_host_uses_global() {
        let global = sandbox_cfg(|sandbox| sandbox.rpc_host = Some("custom.host".into()));
        let project = Config::default();
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.rpc_host, Some("custom.host".to_string()));
    }

    #[test]
    fn test_sandbox_image_project_overrides_global() {
        let global = sandbox_cfg(|sandbox| sandbox.image = Some("global:latest".into()));
        let project = sandbox_cfg(|sandbox| sandbox.image = Some("custom:latest".into()));
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.image, Some("custom:latest".to_string()));
    }

    #[test]
    fn test_sandbox_image_project_used_when_no_global() {
        let global = Config::default();
        let project = sandbox_cfg(|sandbox| sandbox.image = Some("custom:latest".into()));
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.image, Some("custom:latest".to_string()));
    }

    #[test]
    fn test_sandbox_image_falls_back_to_global() {
        let global = sandbox_cfg(|sandbox| sandbox.image = Some("global:latest".into()));
        let project = Config::default();
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.image, Some("global:latest".to_string()));
    }

    #[test]
    fn test_excluded_files_default_empty() {
        let config = ContainerConfig::default();
        assert!(config.excluded_files().is_empty());
    }

    #[test]
    fn test_excluded_files_accessor() {
        let config = ContainerConfig {
            excluded_files: Some(vec![".env".into(), ".env.local".into()]),
            ..Default::default()
        };
        assert_eq!(config.excluded_files(), &[".env", ".env.local"]);
    }

    #[test]
    fn test_excluded_files_merge_project_is_ignored() {
        // Security: excluded_files is global-only. A project config (.workmux.yaml)
        // MUST NOT be able to weaken or replace the global list; otherwise a
        // malicious repo could delete user-level secret protections.
        let global = container_cfg(|container| {
            container.excluded_files = Some(vec![".env".into()]);
        });
        let project = container_cfg(|container| {
            container.excluded_files = Some(vec![".env.production".into()]);
        });
        let merged = global.merge(project);
        assert_eq!(
            merged.sandbox.container.excluded_files,
            Some(vec![".env".into()])
        );
    }

    #[test]
    fn test_excluded_files_merge_project_only_is_ignored() {
        // A project that sets excluded_files without any global list must NOT
        // take effect -- project config can never set this field.
        let global = Config::default();
        let project = container_cfg(|container| {
            container.excluded_files = Some(vec![".env".into()]);
        });
        let merged = global.merge(project);
        assert_eq!(merged.sandbox.container.excluded_files, None);
    }

    #[test]
    fn test_excluded_files_merge_inherits_global() {
        let global = container_cfg(|container| {
            container.excluded_files = Some(vec![".env".into()]);
        });
        let merged = global.merge(Config::default());
        assert_eq!(
            merged.sandbox.container.excluded_files,
            Some(vec![".env".into()])
        );
    }

    #[test]
    fn test_dashboard_sort_mode_project_overrides_global() {
        let global = cfg(|config| {
            config.dashboard.sort_mode = Some("priority".into());
        });
        let project = cfg(|config| {
            config.dashboard.sort_mode = Some("recency".into());
        });
        let merged = global.merge(project);
        assert_eq!(merged.dashboard.sort_mode.as_deref(), Some("recency"));
    }

    #[test]
    fn test_dashboard_sort_mode_project_only_inherited() {
        let global = Config::default();
        let project = cfg(|config| {
            config.dashboard.sort_mode = Some("recency".into());
        });
        let merged = global.merge(project);
        assert_eq!(merged.dashboard.sort_mode.as_deref(), Some("recency"));
    }

    #[test]
    fn test_dashboard_sort_mode_defaults_none() {
        assert_eq!(Config::default().dashboard.sort_mode, None);
    }

    #[test]
    fn test_sandbox_env_passthrough_global_only() {
        // Project config is ignored -- only global matters
        let global = sandbox_cfg(|sandbox| {
            sandbox.env_passthrough = Some(vec!["GITHUB_TOKEN".into()]);
        });
        let project = sandbox_cfg(|sandbox| {
            sandbox.env_passthrough = Some(vec!["AWS_SECRET_ACCESS_KEY".into()]);
        });
        let merged = global.merge(project);
        assert_eq!(
            merged.sandbox.env_passthrough,
            Some(vec!["GITHUB_TOKEN".to_string()])
        );
    }

    #[test]
    fn test_sandbox_env_passthrough_project_ignored_when_no_global() {
        let global = Config::default();
        let project = sandbox_cfg(|sandbox| {
            sandbox.env_passthrough = Some(vec!["AWS_SECRET_ACCESS_KEY".into()]);
        });
        let merged = global.merge(project);
        assert!(merged.sandbox.env_passthrough.is_none());
    }

    #[test]
    fn test_sandbox_env_passthrough_uses_global() {
        let global = sandbox_cfg(|sandbox| {
            sandbox.env_passthrough = Some(vec!["GITHUB_TOKEN".into()]);
        });
        let project = Config::default();
        let merged = global.merge(project);
        assert_eq!(
            merged.sandbox.env_passthrough,
            Some(vec!["GITHUB_TOKEN".to_string()])
        );
    }

    #[test]
    fn sandbox_env_global_only() {
        // Project config is ignored -- only global matters
        let global = sandbox_cfg(|sandbox| {
            sandbox.env = Some(HashMap::from([("GH_TOKEN".into(), "global_token".into())]));
        });
        let project = sandbox_cfg(|sandbox| {
            sandbox.env = Some(HashMap::from([("GH_TOKEN".into(), "project_token".into())]));
        });
        let merged = global.merge(project);
        let env = merged.sandbox.env.unwrap();
        assert_eq!(env.get("GH_TOKEN").unwrap(), "global_token");
    }

    #[test]
    fn sandbox_env_project_ignored_when_no_global() {
        let global = Config::default();
        let project = sandbox_cfg(|sandbox| {
            sandbox.env = Some(HashMap::from([("GH_TOKEN".into(), "project_token".into())]));
        });
        let merged = global.merge(project);
        assert!(merged.sandbox.env.is_none());
    }

    #[test]
    fn sandbox_env_uses_global() {
        let global = sandbox_cfg(|sandbox| {
            sandbox.env = Some(HashMap::from([("GH_TOKEN".into(), "global_token".into())]));
        });
        let project = Config::default();
        let merged = global.merge(project);
        let env = merged.sandbox.env.unwrap();
        assert_eq!(env.get("GH_TOKEN").unwrap(), "global_token");
    }

    #[test]
    fn sandbox_env_vars_accessor() {
        let config = SandboxConfig {
            env: Some(HashMap::from([("KEY".to_string(), "VALUE".to_string())])),
            ..Default::default()
        };
        let vars = config.env_vars();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0], ("KEY", "VALUE"));
    }

    #[test]
    fn sandbox_env_vars_accessor_empty() {
        let config = SandboxConfig::default();
        assert!(config.env_vars().is_empty());
    }

    #[test]
    fn test_extra_mount_parse_simple_string() {
        let yaml = r#"extra_mounts: ["/tmp/notes"]"#;
        let config: SandboxConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.extra_mounts().len(), 1);
        let (host, guest, read_only) = config.extra_mounts()[0].resolve().unwrap();
        assert_eq!(host, std::path::PathBuf::from("/tmp/notes"));
        assert_eq!(guest, std::path::PathBuf::from("/tmp/notes"));
        assert!(read_only);
    }

    #[test]
    fn test_extra_mount_parse_spec() {
        let yaml = r#"
extra_mounts:
  - host_path: /tmp/data
    guest_path: /mnt/data
    writable: true
"#;
        let config: SandboxConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.extra_mounts().len(), 1);
        let (host, guest, read_only) = config.extra_mounts()[0].resolve().unwrap();
        assert_eq!(host, std::path::PathBuf::from("/tmp/data"));
        assert_eq!(guest, std::path::PathBuf::from("/mnt/data"));
        assert!(!read_only);
    }

    #[test]
    fn test_extra_mount_spec_defaults() {
        let yaml = r#"
extra_mounts:
  - host_path: /tmp/data
"#;
        let config: SandboxConfig = serde_yaml::from_str(yaml).unwrap();
        let (host, guest, read_only) = config.extra_mounts()[0].resolve().unwrap();
        assert_eq!(host, std::path::PathBuf::from("/tmp/data"));
        // guest defaults to host path
        assert_eq!(guest, std::path::PathBuf::from("/tmp/data"));
        // writable defaults to false (read_only = true)
        assert!(read_only);
    }

    #[test]
    fn test_extra_mount_tilde_expansion() {
        let mount = ExtraMount::Path("~/notes".to_string());
        let (host, guest, _) = mount.resolve().unwrap();
        // Should expand ~ to home dir
        assert!(!host.to_string_lossy().starts_with('~'));
        assert!(host.to_string_lossy().ends_with("/notes"));
        // Guest should mirror expanded host
        assert_eq!(host, guest);
    }

    #[test]
    fn test_extra_mount_mixed_list() {
        let yaml = r#"
extra_mounts:
  - /tmp/notes
  - host_path: /tmp/data
    guest_path: /mnt/data
    writable: true
"#;
        let config: SandboxConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.extra_mounts().len(), 2);

        let (host0, _, ro0) = config.extra_mounts()[0].resolve().unwrap();
        assert_eq!(host0, std::path::PathBuf::from("/tmp/notes"));
        assert!(ro0);

        let (host1, guest1, ro1) = config.extra_mounts()[1].resolve().unwrap();
        assert_eq!(host1, std::path::PathBuf::from("/tmp/data"));
        assert_eq!(guest1, std::path::PathBuf::from("/mnt/data"));
        assert!(!ro1);
    }

    #[test]
    fn test_extra_mounts_default_empty() {
        let config = SandboxConfig::default();
        assert!(config.extra_mounts().is_empty());
    }

    #[test]
    fn test_extra_mounts_global_only() {
        // Project config is ignored -- only global matters
        let global = sandbox_extra_mounts(&["/global/path"]);
        let project = sandbox_extra_mounts(&["/project/path"]);

        let merged = global.merge(project);
        assert_eq!(merged.sandbox.extra_mounts().len(), 1);
        let (host, _, _) = merged.sandbox.extra_mounts()[0].resolve().unwrap();
        assert_eq!(host, std::path::PathBuf::from("/global/path"));
    }

    #[test]
    fn test_extra_mounts_project_ignored_when_no_global() {
        let global = Config::default(); // no extra_mounts
        let project = sandbox_extra_mounts(&["/project/path"]);

        let merged = global.merge(project);
        assert!(merged.sandbox.extra_mounts().is_empty());
    }

    #[test]
    fn test_extra_mounts_uses_global() {
        let global = sandbox_extra_mounts(&["/global/path"]);
        let project = Config::default();

        let merged = global.merge(project);
        assert_eq!(merged.sandbox.extra_mounts().len(), 1);
        let (host, _, _) = merged.sandbox.extra_mounts()[0].resolve().unwrap();
        assert_eq!(host, std::path::PathBuf::from("/global/path"));
    }

    #[test]
    fn test_resolved_agent_config_dir_with_placeholder() {
        let config = SandboxConfig {
            agent_config_dir: Some("~/sandbox/{agent}".to_string()),
            ..Default::default()
        };
        let dir = config.resolved_agent_config_dir("claude").unwrap();
        let home = home::home_dir().unwrap();
        assert_eq!(dir, home.join("sandbox/claude"));
    }

    #[test]
    fn test_resolved_agent_config_dir_without_placeholder() {
        let config = SandboxConfig {
            agent_config_dir: Some("~/my-config".to_string()),
            ..Default::default()
        };
        let dir = config.resolved_agent_config_dir("claude").unwrap();
        let home = home::home_dir().unwrap();
        assert_eq!(dir, home.join("my-config"));
    }

    #[test]
    fn test_resolved_agent_config_dir_default() {
        let config = SandboxConfig::default();
        let dir = config
            .resolved_agent_config_dir_with_env("claude", |_| None)
            .unwrap();
        let home = home::home_dir().unwrap();
        assert_eq!(dir, home.join(".claude"));
        let dir = config
            .resolved_agent_config_dir_with_env("agy", |_| None)
            .unwrap();
        assert_eq!(dir, home.join(".gemini/antigravity-cli"));
        let dir = config
            .resolved_agent_config_dir_with_env("omp", |_| None)
            .unwrap();
        assert_eq!(dir, home.join(".omp/agent"));
    }

    #[test]
    fn test_resolved_agent_config_dir_omp_with_pi_config_dir() {
        let config = SandboxConfig::default();
        let dir = config
            .resolved_agent_config_dir_with_env("omp", |key| {
                (key == "PI_CONFIG_DIR").then(|| std::ffi::OsString::from("custom-omp"))
            })
            .unwrap();
        let home = home::home_dir().unwrap();
        assert_eq!(dir, home.join("custom-omp/agent"));
    }

    #[test]
    fn test_resolved_agent_config_dir_unknown_agent_default() {
        let config = SandboxConfig::default();
        assert!(config.resolved_agent_config_dir("unknown").is_none());
    }

    #[test]
    fn test_resolved_agent_config_dir_unknown_agent_custom() {
        let config = SandboxConfig {
            agent_config_dir: Some("/custom/{agent}".to_string()),
            ..Default::default()
        };
        // Custom dir always returns Some, even for unknown agents
        let dir = config.resolved_agent_config_dir("unknown").unwrap();
        assert_eq!(dir, std::path::PathBuf::from("/custom/unknown"));
    }

    #[test]
    fn test_agent_config_dir_global_only() {
        let global = sandbox_agent_config_dir("~/global/{agent}");
        let project = sandbox_agent_config_dir("~/project/{agent}");
        let merged = global.merge(project);
        assert_eq!(
            merged.sandbox.agent_config_dir,
            Some("~/global/{agent}".to_string())
        );
    }

    #[test]
    fn test_agent_config_dir_project_ignored_when_no_global() {
        let global = Config::default();
        let project = sandbox_agent_config_dir("~/project/{agent}");
        let merged = global.merge(project);
        assert!(merged.sandbox.agent_config_dir.is_none());
    }

    #[test]
    fn test_extra_mount_rejects_relative_host_path() {
        let mount = ExtraMount::Path("relative/path".to_string());
        let result = mount.resolve();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("must be absolute"));
    }

    #[test]
    fn test_extra_mount_rejects_relative_guest_path() {
        let mount = ExtraMount::Spec {
            host_path: "/tmp/data".to_string(),
            guest_path: Some("relative/guest".to_string()),
            writable: None,
        };
        let result = mount.resolve();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("guest_path must be absolute"));
    }

    #[test]
    fn sandbox_nested_yaml_format() {
        let yaml = r#"
enabled: true
backend: lima
lima:
  isolation: shared
  cpus: 16
  memory: 16GiB
container:
  runtime: podman
"#;
        let config: SandboxConfig = serde_yaml::from_str(yaml).unwrap();

        assert!(config.is_enabled());
        assert_eq!(config.lima.isolation(), super::IsolationLevel::Shared);
        assert_eq!(config.lima.cpus(), 16);
        assert_eq!(config.lima.memory(), "16GiB");
        assert_eq!(config.container.runtime(), SandboxRuntime::Podman);
    }

    #[test]
    fn sandbox_lima_config_merge() {
        let global = LimaConfig {
            isolation: Some(super::IsolationLevel::Shared),
            accept_reduced_git_metadata_isolation: Some(true),
            cpus: Some(4),
            memory: Some("4GiB".to_string()),
            ..Default::default()
        };
        let project = LimaConfig {
            cpus: Some(8),
            provision: Some("echo project".to_string()),
            ..Default::default()
        };

        let merged = LimaConfig::merge(global, project);
        // Project overrides
        assert_eq!(merged.cpus(), 8);
        assert_eq!(merged.provision_script(), Some("echo project"));
        // Global fallback
        assert_eq!(merged.isolation(), super::IsolationLevel::Shared);
        assert_eq!(merged.memory(), "4GiB");
        assert!(merged.accepts_reduced_git_metadata_isolation());
    }

    #[test]
    fn lima_reduced_git_metadata_acceptance_defaults_to_false() {
        let default = LimaConfig::default();
        assert!(!default.accepts_reduced_git_metadata_isolation());

        let accepted: LimaConfig =
            serde_yaml::from_str("accept_reduced_git_metadata_isolation: true\n").unwrap();
        assert!(accepted.accepts_reduced_git_metadata_isolation());
    }

    #[test]
    fn sandbox_container_config_merge() {
        let global = ContainerConfig {
            runtime: Some(SandboxRuntime::Docker),
            ..Default::default()
        };
        let project = ContainerConfig {
            runtime: Some(SandboxRuntime::Podman),
            ..Default::default()
        };

        let merged = ContainerConfig::merge(global, project);
        assert_eq!(merged.runtime(), SandboxRuntime::Podman);
    }

    // --- Network config tests ---

    #[test]
    fn network_policy_defaults_to_allow() {
        let config = SandboxConfig::default();
        assert_eq!(config.network.policy(), NetworkPolicy::Allow);
        assert!(!config.network_policy_is_deny());
    }

    #[test]
    fn network_policy_deny() {
        let config = SandboxConfig {
            network: NetworkConfig {
                policy: Some(NetworkPolicy::Deny),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(config.network.policy(), NetworkPolicy::Deny);
        assert!(config.network_policy_is_deny());
    }

    #[test]
    fn network_allowed_domains_default_empty() {
        let config = NetworkConfig::default();
        assert!(config.allowed_domains().is_empty());
    }

    #[test]
    fn network_max_connections_has_bounded_default() {
        let config = NetworkConfig::default();
        assert_eq!(
            config.max_connections(),
            super::DEFAULT_NETWORK_MAX_CONNECTIONS
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn network_max_connections_validation() {
        for max_connections in [0, super::MAX_NETWORK_MAX_CONNECTIONS + 1] {
            let config = NetworkConfig {
                max_connections: Some(max_connections),
                ..Default::default()
            };
            assert!(config.validate().is_err());
        }

        let config = NetworkConfig {
            max_connections: Some(64),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        assert_eq!(config.max_connections(), 64);
    }

    #[test]
    fn network_config_global_only() {
        let mut global = sandbox_network(NetworkPolicy::Deny, &["api.anthropic.com"]);
        global.sandbox.network.max_connections = Some(64);
        let mut project = sandbox_network(NetworkPolicy::Allow, &["evil.com"]);
        project.sandbox.network.max_connections = Some(32);

        let merged = global.merge(project);
        // Global value should win
        assert_eq!(merged.sandbox.network.policy(), NetworkPolicy::Deny);
        assert_eq!(merged.sandbox.network.max_connections(), 64);
        assert_eq!(
            merged.sandbox.network.allowed_domains(),
            &[AllowedDomainEntry::Plain("api.anthropic.com".to_string())]
        );
    }

    #[test]
    fn network_config_project_ignored_when_no_global() {
        let global = Config::default();
        let project = sandbox_network(NetworkPolicy::Deny, &["evil.com"]);

        let merged = global.merge(project);
        assert_eq!(merged.sandbox.network.policy(), NetworkPolicy::Allow);
        assert!(merged.sandbox.network.allowed_domains().is_empty());
    }

    #[test]
    fn network_config_uses_global() {
        let global = sandbox_network(NetworkPolicy::Deny, &["github.com"]);
        let project = Config::default();

        let merged = global.merge(project);
        assert_eq!(merged.sandbox.network.policy(), NetworkPolicy::Deny);
        assert_eq!(
            merged.sandbox.network.allowed_domains(),
            &[AllowedDomainEntry::Plain("github.com".to_string())]
        );
    }

    #[test]
    fn validate_domain_rejects_ip_literal() {
        assert!(validate_domain("192.168.1.1").is_err());
        assert!(validate_domain("127.0.0.1").is_err());
        assert!(validate_domain("::1").is_err());
    }

    #[test]
    fn validate_domain_rejects_trailing_dot() {
        assert!(validate_domain("example.com.").is_err());
    }

    #[test]
    fn validate_domain_rejects_malformed_wildcard() {
        assert!(validate_domain("foo.*.com").is_err());
        assert!(validate_domain("*foo.com").is_err());
    }

    #[test]
    fn validate_domain_rejects_empty() {
        assert!(validate_domain("").is_err());
    }

    #[test]
    fn validate_domain_accepts_valid() {
        assert!(validate_domain("example.com").is_ok());
        assert!(validate_domain("api.anthropic.com").is_ok());
        assert!(validate_domain("*.googleapis.com").is_ok());
        assert!(validate_domain("*.github.com").is_ok());
    }

    #[test]
    fn network_config_validate_catches_bad_domains() {
        let config = NetworkConfig {
            policy: Some(NetworkPolicy::Deny),
            allowed_domains: Some(vec![
                AllowedDomainEntry::Plain("good.com".to_string()),
                AllowedDomainEntry::Plain("192.168.1.1".to_string()),
            ]),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn network_config_validate_passes_good_domains() {
        let config = NetworkConfig {
            policy: Some(NetworkPolicy::Deny),
            allowed_domains: Some(vec![
                AllowedDomainEntry::Plain("api.anthropic.com".to_string()),
                AllowedDomainEntry::Plain("*.github.com".to_string()),
                AllowedDomainEntry::Plain("registry.npmjs.org".to_string()),
            ]),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn network_config_yaml_roundtrip() {
        let yaml = r#"
network:
  policy: deny
  allowed_domains:
    - api.anthropic.com
    - "*.github.com"
  max_connections: 64
"#;
        let config: SandboxConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.network.policy(), NetworkPolicy::Deny);
        assert_eq!(config.network.allowed_domains().len(), 2);
        assert_eq!(config.network.max_connections(), 64);
        assert_eq!(
            config.network.allowed_domain_rules(),
            vec![
                super::AllowedDomainRule {
                    host: "api.anthropic.com".to_string(),
                    allow_private_ips: false,
                },
                super::AllowedDomainRule {
                    host: "*.github.com".to_string(),
                    allow_private_ips: false,
                },
            ]
        );
    }

    #[test]
    fn network_config_yaml_parses_private_domain_rule() {
        let yaml = r#"
network:
  policy: deny
  allowed_domains:
    - host: artifactory.example.com
      allow_private_ips: true
"#;
        let config: SandboxConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.network.allowed_domains(),
            &[AllowedDomainEntry::Detailed(AllowedDomainDetails {
                host: "artifactory.example.com".to_string(),
                allow_private_ips: true,
            })]
        );
    }

    #[test]
    fn network_config_yaml_rejects_unknown_allowed_domain_field() {
        let yaml = r#"
network:
  policy: deny
  allowed_domains:
    - host: artifactory.example.com
      alow_private: true
"#;
        assert!(serde_yaml::from_str::<SandboxConfig>(yaml).is_err());
    }

    #[test]
    fn network_config_validate_rejects_wildcard_private_rule() {
        let config = NetworkConfig {
            policy: Some(NetworkPolicy::Deny),
            allowed_domains: Some(vec![AllowedDomainEntry::Detailed(AllowedDomainDetails {
                host: "*.example.com".to_string(),
                allow_private_ips: true,
            })]),
            ..Default::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("allow_private_ips"));
    }

    #[test]
    fn network_config_load_rejects_wildcard_private_rule() {
        let config = Config {
            sandbox: SandboxConfig {
                network: NetworkConfig {
                    policy: Some(NetworkPolicy::Deny),
                    allowed_domains: Some(vec![AllowedDomainEntry::Detailed(
                        AllowedDomainDetails {
                            host: "*.example.com".to_string(),
                            allow_private_ips: true,
                        },
                    )]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let err = Config::merge_and_apply_defaults(
            config,
            Config::default(),
            None,
            std::path::Path::new(""),
        )
        .unwrap_err();
        assert!(err.to_string().contains("Invalid sandbox network config"));
    }

    // --- ContainerDevice / group_add tests ---

    #[test]
    fn container_device_string_form_parses() {
        let yaml = r#"
container:
  devices:
    - /dev/kvm
    - /dev/dri:/dev/dri
    - /dev/bus/usb/001/002:/dev/bus/usb/001/002:rwm
"#;
        let config: SandboxConfig = serde_yaml::from_str(yaml).unwrap();
        let devs = config.container.devices.unwrap();
        assert_eq!(devs.len(), 3);
        assert_eq!(devs[0].to_arg(), "/dev/kvm");
        assert_eq!(devs[1].to_arg(), "/dev/dri:/dev/dri");
        assert_eq!(
            devs[2].to_arg(),
            "/dev/bus/usb/001/002:/dev/bus/usb/001/002:rwm"
        );
    }

    #[test]
    fn container_device_struct_form_parses() {
        let yaml = r#"
container:
  devices:
    - host_path: /dev/bus/usb/001/002
      guest_path: /dev/bus/usb/001/002
      permissions: rw
    - host_path: /dev/kvm
"#;
        let config: SandboxConfig = serde_yaml::from_str(yaml).unwrap();
        let devs = config.container.devices.unwrap();
        assert_eq!(devs.len(), 2);
        assert_eq!(
            devs[0].to_arg(),
            "/dev/bus/usb/001/002:/dev/bus/usb/001/002:rw"
        );
        assert_eq!(devs[1].to_arg(), "/dev/kvm");
    }

    #[test]
    fn container_device_validation_rejects_relative_host_path() {
        let dev = ContainerDevice::String("ttyUSB0".to_string());
        assert!(dev.validate().is_err());
    }

    #[test]
    fn container_device_validation_rejects_whitespace() {
        let dev = ContainerDevice::String(" /dev/kvm".to_string());
        assert!(dev.validate().is_err());
        let dev = ContainerDevice::String("/dev/kvm ".to_string());
        assert!(dev.validate().is_err());
        let dev = ContainerDevice::String("/dev/kvm:/dev/kvm:r w".to_string());
        assert!(dev.validate().is_err());

        let dev = ContainerDevice::Struct {
            host_path: "/dev/kvm ".to_string(),
            guest_path: None,
            permissions: None,
        };
        assert!(dev.validate().is_err());
        let dev = ContainerDevice::Struct {
            host_path: "/dev/kvm".to_string(),
            guest_path: Some(" /dev/kvm".to_string()),
            permissions: None,
        };
        assert!(dev.validate().is_err());
    }

    #[test]
    fn container_device_validation_rejects_bad_permissions() {
        let dev = ContainerDevice::String("/dev/kvm:/dev/kvm:zzz".to_string());
        assert!(dev.validate().is_err());
    }

    #[test]
    fn container_device_validation_accepts_single_path() {
        let dev = ContainerDevice::String("/dev/kvm".to_string());
        assert!(dev.validate().is_ok());
    }

    #[test]
    fn container_device_validation_accepts_path_with_permissions_only() {
        let dev = ContainerDevice::String("/dev/kvm:rwm".to_string());
        assert!(dev.validate().is_ok());
    }

    #[test]
    fn container_device_validation_accepts_full_triple() {
        let dev = ContainerDevice::String("/dev/dri:/dev/dri:rw".to_string());
        assert!(dev.validate().is_ok());
    }

    #[test]
    fn container_device_struct_validation() {
        let bad = ContainerDevice::Struct {
            host_path: "dev/kvm".to_string(),
            guest_path: None,
            permissions: None,
        };
        assert!(bad.validate().is_err());

        let bad_perms = ContainerDevice::Struct {
            host_path: "/dev/kvm".to_string(),
            guest_path: None,
            permissions: Some("zzz".to_string()),
        };
        assert!(bad_perms.validate().is_err());

        let ok = ContainerDevice::Struct {
            host_path: "/dev/kvm".to_string(),
            guest_path: Some("/dev/kvm".to_string()),
            permissions: Some("rw".to_string()),
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn group_add_validation() {
        assert!(validate_group_add_entry("dialout").is_ok());
        assert!(validate_group_add_entry("20").is_ok());
        assert!(validate_group_add_entry("").is_err());
        assert!(validate_group_add_entry("dial out").is_err());
        assert!(validate_group_add_entry("a,b").is_err());
        assert!(validate_group_add_entry("a:b").is_err());
    }

    #[test]
    fn group_add_yaml_parses() {
        let yaml = r#"
container:
  group_add:
    - dialout
    - video
    - "46"
"#;
        let config: SandboxConfig = serde_yaml::from_str(yaml).unwrap();
        let groups = config.container.group_add.unwrap();
        assert_eq!(groups, vec!["dialout", "video", "46"]);
    }

    #[test]
    fn container_devices_are_global_only_in_merge() {
        let mut global = Config::default();
        global.sandbox.container.devices =
            Some(vec![ContainerDevice::String("/dev/kvm".to_string())]);
        let mut project = Config::default();
        project.sandbox.container.devices =
            Some(vec![ContainerDevice::String("/dev/ttyUSB0".to_string())]);

        let merged = global.merge(project);
        let devs = merged.sandbox.container.devices.unwrap();
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].to_arg(), "/dev/kvm");
    }

    #[test]
    fn container_devices_project_ignored_when_global_empty() {
        let global = Config::default();
        let mut project = Config::default();
        project.sandbox.container.devices =
            Some(vec![ContainerDevice::String("/dev/ttyUSB0".to_string())]);

        let merged = global.merge(project);
        assert!(merged.sandbox.container.devices.is_none());
    }

    #[test]
    fn container_group_add_global_only_in_merge() {
        let mut global = Config::default();
        global.sandbox.container.group_add = Some(vec!["dialout".to_string()]);
        let mut project = Config::default();
        project.sandbox.container.group_add = Some(vec!["video".to_string()]);

        let merged = global.merge(project);
        let groups = merged.sandbox.container.group_add.unwrap();
        assert_eq!(groups, vec!["dialout".to_string()]);
    }

    // --- WindowConfig tests ---

    use super::{WindowConfig, validate_windows_config};

    #[test]
    fn parse_windows_config_named() {
        let yaml = r#"
windows:
  - name: editor
    panes:
      - command: <agent>
        focus: true
      - split: horizontal
        size: 20
  - name: tests
    panes:
      - command: just test --watch
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let windows = config.windows.unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name.as_deref(), Some("editor"));
        assert_eq!(windows[0].panes.as_ref().unwrap().len(), 2);
        assert_eq!(windows[1].name.as_deref(), Some("tests"));
        assert_eq!(windows[1].panes.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn parse_windows_config_unnamed() {
        let yaml = r#"
windows:
  - panes:
      - command: tail -f app.log
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let windows = config.windows.unwrap();
        assert_eq!(windows.len(), 1);
        assert!(windows[0].name.is_none());
    }

    #[test]
    fn parse_windows_config_mixed() {
        let yaml = r#"
windows:
  - name: editor
    panes:
      - command: <agent>
        focus: true
  - panes:
      - command: tail -f app.log
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let windows = config.windows.unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name.as_deref(), Some("editor"));
        assert!(windows[1].name.is_none());
    }

    #[test]
    fn validate_windows_config_empty_errors() {
        let result = validate_windows_config(&[]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("must not be empty")
        );
    }

    #[test]
    fn validate_windows_config_valid() {
        let windows = vec![
            WindowConfig {
                name: Some("editor".to_string()),
                panes: Some(vec![super::PaneConfig {
                    command: Some("<agent>".to_string()),
                    focus: true,
                    ..Default::default()
                }]),
            },
            WindowConfig {
                name: None,
                panes: Some(vec![super::PaneConfig {
                    command: Some("tail -f app.log".to_string()),
                    ..Default::default()
                }]),
            },
        ];
        assert!(validate_windows_config(&windows).is_ok());
    }

    #[test]
    fn validate_windows_config_bad_pane_errors() {
        let windows = vec![WindowConfig {
            name: Some("bad".to_string()),
            panes: Some(vec![super::PaneConfig {
                split: Some(super::SplitDirection::Horizontal), // first pane cannot have split
                ..Default::default()
            }]),
        }];
        let result = validate_windows_config(&windows);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Window 0"));
    }

    #[test]
    fn merge_project_windows_overrides_global_panes() {
        let global = Config {
            panes: Some(vec![super::PaneConfig {
                command: Some("vim".to_string()),
                focus: true,
                ..Default::default()
            }]),
            ..Default::default()
        };
        let project = Config {
            windows: Some(vec![
                WindowConfig {
                    name: Some("editor".to_string()),
                    panes: None,
                },
                WindowConfig {
                    name: Some("tests".to_string()),
                    panes: None,
                },
            ]),
            ..Default::default()
        };

        let merged = global.merge(project);
        // Project windows should win, panes should be cleared
        assert!(merged.windows.is_some());
        assert!(merged.panes.is_none());
        assert_eq!(merged.windows.unwrap().len(), 2);
    }

    #[test]
    fn merge_project_panes_overrides_global_windows() {
        let global = Config {
            windows: Some(vec![WindowConfig {
                name: Some("global-window".to_string()),
                panes: None,
            }]),
            ..Default::default()
        };
        let project = Config {
            panes: Some(vec![super::PaneConfig {
                command: Some("vim".to_string()),
                focus: true,
                ..Default::default()
            }]),
            ..Default::default()
        };

        let merged = global.merge(project);
        // Project panes should win, windows should be cleared
        assert!(merged.panes.is_some());
        assert!(merged.windows.is_none());
    }

    #[test]
    fn merge_global_windows_inherited_when_no_project_layout() {
        let global = Config {
            windows: Some(vec![WindowConfig {
                name: Some("global-window".to_string()),
                panes: None,
            }]),
            ..Default::default()
        };
        let project = Config::default(); // no panes or windows

        let merged = global.merge(project);
        assert!(merged.windows.is_some());
        assert!(merged.panes.is_none());
    }

    #[test]
    fn parse_runtime_apple_container() {
        let yaml = r#"
sandbox:
  container:
    runtime: apple-container
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.sandbox.container.runtime,
            Some(SandboxRuntime::AppleContainer)
        );
    }

    #[test]
    fn runtime_binary_names() {
        assert_eq!(SandboxRuntime::Docker.binary_name(), "docker");
        assert_eq!(SandboxRuntime::Podman.binary_name(), "podman");
        assert_eq!(SandboxRuntime::AppleContainer.binary_name(), "container");
    }

    #[test]
    fn runtime_rpc_host_addresses() {
        assert_eq!(
            SandboxRuntime::Docker.rpc_host_address(),
            "host.docker.internal"
        );
        assert_eq!(
            SandboxRuntime::Podman.rpc_host_address(),
            "host.containers.internal"
        );
        assert_eq!(
            SandboxRuntime::AppleContainer.rpc_host_address(),
            "192.168.64.1"
        );
    }

    #[test]
    fn runtime_capability_flags() {
        // needs_add_host: only Docker
        assert!(SandboxRuntime::Docker.needs_add_host());
        assert!(!SandboxRuntime::Podman.needs_add_host());
        assert!(!SandboxRuntime::AppleContainer.needs_add_host());

        // needs_userns_keep_id: only Podman
        assert!(!SandboxRuntime::Docker.needs_userns_keep_id());
        assert!(SandboxRuntime::Podman.needs_userns_keep_id());
        assert!(!SandboxRuntime::AppleContainer.needs_userns_keep_id());

        // needs_deny_mode_caps: Docker and Podman, not Apple Container
        assert!(SandboxRuntime::Docker.needs_deny_mode_caps());
        assert!(SandboxRuntime::Podman.needs_deny_mode_caps());
        assert!(!SandboxRuntime::AppleContainer.needs_deny_mode_caps());
    }

    #[test]
    fn runtime_pull_args() {
        assert_eq!(
            SandboxRuntime::Docker.pull_args("img:latest"),
            vec!["pull", "img:latest"]
        );
        assert_eq!(
            SandboxRuntime::Podman.pull_args("img:latest"),
            vec!["pull", "img:latest"]
        );
        assert_eq!(
            SandboxRuntime::AppleContainer.pull_args("img:latest"),
            vec!["image", "pull", "img:latest"]
        );
    }

    #[test]
    fn runtime_serde_name_roundtrip() {
        for runtime in [
            SandboxRuntime::Docker,
            SandboxRuntime::Podman,
            SandboxRuntime::AppleContainer,
        ] {
            let name = runtime.serde_name();
            let parsed = SandboxRuntime::from_serde_name(name).unwrap();
            assert_eq!(parsed, runtime);
        }
    }

    #[test]
    fn runtime_from_serde_name_unknown() {
        assert_eq!(SandboxRuntime::from_serde_name("unknown"), None);
        assert_eq!(SandboxRuntime::from_serde_name(""), None);
    }

    #[test]
    fn runtime_default_memory() {
        assert_eq!(SandboxRuntime::AppleContainer.default_memory(), Some("16G"));
        assert_eq!(SandboxRuntime::Docker.default_memory(), None);
        assert_eq!(SandboxRuntime::Podman.default_memory(), None);
    }

    #[test]
    fn container_config_merge_resources() {
        let global = ContainerConfig {
            runtime: Some(SandboxRuntime::Docker),
            memory: Some("8G".to_string()),
            cpus: Some(4),
            ..Default::default()
        };
        let project = ContainerConfig {
            runtime: None,
            memory: Some("16G".to_string()),
            cpus: None,
            ..Default::default()
        };
        let merged = ContainerConfig::merge(global, project);
        assert_eq!(merged.memory.as_deref(), Some("16G")); // project overrides
        assert_eq!(merged.cpus, Some(4)); // falls back to global
        assert_eq!(merged.runtime, Some(SandboxRuntime::Docker));
    }

    // ── Theme config deserialization tests ───────────────────────

    use super::{ThemeConfig, ThemeMode, ThemeScheme};

    #[test]
    fn theme_scheme_slug_roundtrip() {
        for scheme in &ThemeScheme::ALL {
            let slug = scheme.slug();
            assert_eq!(
                ThemeScheme::from_slug(slug),
                Some(*scheme),
                "slug roundtrip failed for {:?}",
                scheme
            );
        }
    }

    #[test]
    fn theme_scheme_next_wraps() {
        let mut current = ThemeScheme::Default;
        for _ in 0..ThemeScheme::ALL.len() {
            current = current.next();
        }
        assert_eq!(current, ThemeScheme::Default);
    }

    #[test]
    fn theme_scheme_all_is_exhaustive() {
        // This match will fail to compile if a variant is added but not listed
        for scheme in &ThemeScheme::ALL {
            match scheme {
                ThemeScheme::Default
                | ThemeScheme::Emberforge
                | ThemeScheme::GlacierSignal
                | ThemeScheme::ObsidianPop
                | ThemeScheme::SlateGarden
                | ThemeScheme::PhosphorArcade
                | ThemeScheme::Lasergrid
                | ThemeScheme::Mossfire
                | ThemeScheme::NightSorbet
                | ThemeScheme::GraphiteCode
                | ThemeScheme::FestivalCircuit
                | ThemeScheme::TealDrift => {}
            }
        }
        assert_eq!(ThemeScheme::ALL.len(), 12);
    }

    #[test]
    fn theme_config_string_scheme() {
        let config: ThemeConfig = serde_yaml::from_str("emberforge").unwrap();
        assert_eq!(config.scheme, ThemeScheme::Emberforge);
        assert_eq!(config.mode, None);
    }

    #[test]
    fn theme_config_string_legacy_dark() {
        let config: ThemeConfig = serde_yaml::from_str("dark").unwrap();
        assert_eq!(config.scheme, ThemeScheme::Default);
        assert_eq!(config.mode, Some(ThemeMode::Dark));
    }

    #[test]
    fn theme_config_string_legacy_light() {
        let config: ThemeConfig = serde_yaml::from_str("light").unwrap();
        assert_eq!(config.scheme, ThemeScheme::Default);
        assert_eq!(config.mode, Some(ThemeMode::Light));
    }

    #[test]
    fn theme_config_structured() {
        let yaml = "scheme: glacier-signal\nmode: light";
        let config: ThemeConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.scheme, ThemeScheme::GlacierSignal);
        assert_eq!(config.mode, Some(ThemeMode::Light));
    }

    #[test]
    fn theme_config_structured_scheme_only() {
        let yaml = "scheme: night-sorbet";
        let config: ThemeConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.scheme, ThemeScheme::NightSorbet);
        assert_eq!(config.mode, None);
    }

    #[test]
    fn theme_config_unknown_scheme_defaults() {
        let config: ThemeConfig = serde_yaml::from_str("nonexistent").unwrap();
        assert_eq!(config.scheme, ThemeScheme::Default);
        assert_eq!(config.mode, None);
    }

    #[test]
    fn theme_config_full_config_file() {
        let yaml = "agent: claude\ntheme: teal-drift\nnerdfont: true";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.theme.scheme, ThemeScheme::TealDrift);
        assert_eq!(config.theme.mode, None);
    }

    #[test]
    fn theme_config_full_config_structured() {
        let yaml = "agent: claude\ntheme:\n  scheme: obsidian-pop\n  mode: dark\nnerdfont: true";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.theme.scheme, ThemeScheme::ObsidianPop);
        assert_eq!(config.theme.mode, Some(ThemeMode::Dark));
    }

    #[test]
    fn theme_config_full_config_legacy() {
        let yaml = "agent: claude\ntheme: light";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.theme.scheme, ThemeScheme::Default);
        assert_eq!(config.theme.mode, Some(ThemeMode::Light));
    }

    #[test]
    fn theme_config_missing_defaults() {
        let yaml = "agent: claude";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.theme.scheme, ThemeScheme::Default);
        assert_eq!(config.theme.mode, None);
    }

    // --- Layout tests ---

    #[test]
    fn deserialize_layouts() {
        let yaml = r#"
layouts:
  design:
    panes:
      - command: "<agent:claude>"
        focus: true
      - command: "<agent:codex>"
        split: vertical
  review:
    panes:
      - command: "<agent:claude>"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let layouts = config.layouts.unwrap();
        assert_eq!(layouts.len(), 2);
        assert!(layouts.contains_key("design"));
        assert_eq!(layouts["design"].panes.len(), 2);
        assert_eq!(layouts["review"].panes.len(), 1);
    }

    #[test]
    fn deserialize_layouts_absent() {
        let yaml = "agent: claude";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.layouts.is_none());
    }

    #[test]
    fn validate_layouts_valid() {
        let mut layouts = HashMap::new();
        layouts.insert(
            "test".to_string(),
            LayoutConfig {
                panes: vec![
                    PaneConfig {
                        focus: true,
                        ..pane("<agent:claude>")
                    },
                    split_pane("vim"),
                ],
            },
        );
        assert!(validate_layouts_config(&layouts).is_ok());
    }

    #[test]
    fn validate_panes_multiple_zoom_fails() {
        let panes = vec![
            PaneConfig {
                zoom: true,
                ..pane("vim")
            },
            PaneConfig {
                zoom: true,
                ..split_pane("echo hi")
            },
        ];
        let err = super::validate_panes_config(&panes).unwrap_err();
        assert!(err.to_string().contains("Only one pane"));
    }

    #[test]
    fn validate_panes_single_zoom_ok() {
        let panes = vec![
            PaneConfig {
                zoom: true,
                ..pane("vim")
            },
            split_pane("echo hi"),
        ];
        assert!(super::validate_panes_config(&panes).is_ok());
    }

    #[test]
    fn validate_panes_stacked_split_ok() {
        let panes = vec![
            pane("vim"),
            PaneConfig {
                command: Some("tail -f app.log".into()),
                split: Some(SplitDirection::Stacked),
                ..Default::default()
            },
        ];
        assert!(super::validate_panes_config(&panes).is_ok());
    }

    #[test]
    fn validate_panes_stacked_split_rejects_size() {
        let panes = vec![
            pane("vim"),
            PaneConfig {
                command: Some("tail -f app.log".into()),
                split: Some(SplitDirection::Stacked),
                size: Some(10),
                ..Default::default()
            },
        ];
        let err = super::validate_panes_config(&panes).unwrap_err();
        assert!(err.to_string().contains("split: stacked"));
    }

    #[test]
    fn validate_panes_empty_name_fails() {
        let panes = vec![PaneConfig {
            name: Some("  ".into()),
            ..pane("vim")
        }];

        let err = super::validate_panes_config(&panes).unwrap_err();
        assert!(err.to_string().contains("empty 'name'"));
    }

    #[test]
    fn stacked_split_deserializes_from_yaml() {
        let yaml = r#"
panes:
  - command: vim
  - command: tail -f app.log
    split: stacked
"#;
        let config: super::Config = serde_yaml::from_str(yaml).unwrap();
        let panes = config.panes.unwrap();
        assert_eq!(panes[1].split, Some(SplitDirection::Stacked));
    }

    #[test]
    fn pane_name_deserializes_from_yaml() {
        let yaml = r#"
panes:
  - command: vim
    name: editor
  - command: tail -f app.log
    name: logs
    split: horizontal
"#;
        let config: super::Config = serde_yaml::from_str(yaml).unwrap();
        let panes = config.panes.unwrap();
        assert_eq!(panes[0].name.as_deref(), Some("editor"));
        assert_eq!(panes[1].name.as_deref(), Some("logs"));
    }

    #[test]
    fn zoom_deserializes_from_yaml() {
        let yaml = r#"
panes:
  - command: vim
    zoom: true
  - command: echo hi
    split: horizontal
"#;
        let config: super::Config = serde_yaml::from_str(yaml).unwrap();
        let panes = config.panes.unwrap();
        assert!(panes[0].zoom);
        assert!(!panes[1].zoom);
    }

    #[test]
    fn validate_layouts_invalid_first_pane_has_split() {
        let mut layouts = HashMap::new();
        layouts.insert(
            "bad".to_string(),
            LayoutConfig {
                panes: vec![PaneConfig {
                    split: Some(SplitDirection::Horizontal),
                    ..Default::default()
                }],
            },
        );
        let err = validate_layouts_config(&layouts).unwrap_err();
        assert!(
            err.to_string().contains("layout 'bad'"),
            "error should mention layout name: {}",
            err
        );
    }

    #[test]
    fn merge_layouts_project_extends_global() {
        let global = Config {
            layouts: Some(HashMap::from([(
                "a".into(),
                LayoutConfig { panes: vec![] },
            )])),
            ..Default::default()
        };
        let project = Config {
            layouts: Some(HashMap::from([(
                "b".into(),
                LayoutConfig { panes: vec![] },
            )])),
            ..Default::default()
        };
        let merged = global.merge(project);
        let layouts = merged.layouts.unwrap();
        // Project layouts extend global (both available)
        assert!(layouts.contains_key("a"));
        assert!(layouts.contains_key("b"));
    }

    #[test]
    fn merge_layouts_project_overrides_collision() {
        let global = Config {
            layouts: Some(HashMap::from([(
                "shared".into(),
                LayoutConfig {
                    panes: vec![PaneConfig {
                        command: Some("global-cmd".into()),
                        ..Default::default()
                    }],
                },
            )])),
            ..Default::default()
        };
        let project = Config {
            layouts: Some(HashMap::from([(
                "shared".into(),
                LayoutConfig {
                    panes: vec![PaneConfig {
                        command: Some("project-cmd".into()),
                        ..Default::default()
                    }],
                },
            )])),
            ..Default::default()
        };
        let merged = global.merge(project);
        let layouts = merged.layouts.unwrap();
        // Project wins on collision
        assert_eq!(
            layouts["shared"].panes[0].command.as_deref(),
            Some("project-cmd")
        );
    }

    #[test]
    fn merge_layouts_global_used_when_project_has_none() {
        let global = Config {
            layouts: Some(HashMap::from([(
                "a".into(),
                LayoutConfig { panes: vec![] },
            )])),
            ..Default::default()
        };
        let project = Config::default();
        let merged = global.merge(project);
        let layouts = merged.layouts.unwrap();
        assert!(layouts.contains_key("a"));
    }

    #[test]
    fn theme_config_with_custom_colors() {
        let yaml = r##"
theme:
  scheme: emberforge
  custom:
    accent: "#51afef"
    success: "#98be65"
"##;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.theme.scheme, super::ThemeScheme::Emberforge);
        let custom = config.theme.custom.unwrap();
        assert_eq!(custom.accent, Some("#51afef".to_string()));
        assert_eq!(custom.success, Some("#98be65".to_string()));
        assert_eq!(custom.danger, None);
    }

    #[test]
    fn theme_config_custom_with_aliases() {
        let yaml = r##"
theme:
  custom:
    bg: "#282c34"
    fg: "#bbc2cf"
    error: "#ff6c6b"
"##;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let custom = config.theme.custom.unwrap();
        // bg alias maps to current_row_bg
        assert_eq!(custom.current_row_bg, Some("#282c34".to_string()));
        // fg alias maps to text
        assert_eq!(custom.text, Some("#bbc2cf".to_string()));
        // error alias maps to danger
        assert_eq!(custom.danger, Some("#ff6c6b".to_string()));
    }

    #[test]
    fn theme_config_custom_only() {
        let yaml = r##"
theme:
  custom:
    accent: "#51afef"
"##;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.theme.scheme, super::ThemeScheme::Default);
        assert!(config.theme.custom.is_some());
    }

    #[test]
    fn theme_config_simple_string_no_custom() {
        let yaml = "theme: emberforge\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.theme.scheme, super::ThemeScheme::Emberforge);
        assert!(config.theme.custom.is_none());
    }

    #[test]
    fn theme_config_merge_custom_project_wins() {
        let global = theme_cfg(|theme| {
            theme.custom = Some(super::CustomThemeColors {
                accent: Some("#111111".into()),
                ..Default::default()
            });
        });
        let project = theme_cfg(|theme| {
            theme.custom = Some(super::CustomThemeColors {
                accent: Some("#222222".into()),
                ..Default::default()
            });
        });
        let merged = global.merge(project);
        assert_eq!(
            merged.theme.custom.unwrap().accent,
            Some("#222222".to_string())
        );
    }

    #[test]
    fn theme_config_merge_custom_falls_back_to_global() {
        let global = theme_cfg(|theme| {
            theme.custom = Some(super::CustomThemeColors {
                accent: Some("#111111".into()),
                ..Default::default()
            });
        });
        let project = Config::default();
        let merged = global.merge(project);
        assert_eq!(
            merged.theme.custom.unwrap().accent,
            Some("#111111".to_string())
        );
    }
}
