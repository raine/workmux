//! Application state for the sidebar TUI.

use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cmd::Cmd;
use crate::config::{
    AgentIcons, Config, SidebarGroupBy, SidebarPosition, SidebarWidth, StatusIcons,
    TemplatesConfig, ThemeConfig, ThemeMode,
};
use crate::git::GitStatus;
use crate::github::{CheckSummary, PrSummary};
use ratatui::style::Color;
use std::collections::BTreeMap;
use std::str::FromStr;
use tracing::warn;

use crate::multiplexer::{AgentPane, Multiplexer};

use crate::ui::theme::ThemePalette;

use super::snapshot::{SidebarSnapshot, group_label};
use super::template::parser::{ParseError, Token, TokenId, parse_line};

/// Sidebar layout mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarLayoutMode {
    Compact,
    #[default]
    Tiles,
}

impl SidebarLayoutMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Tiles => "tiles",
        }
    }
}

/// Sidebar filter mode: show all agents or only those in the host tmux session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarFilterMode {
    #[default]
    None,
    Session,
}

impl SidebarFilterMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::None => Self::Session,
            Self::Session => Self::None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Session => "session",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "none" | "all" => Self::None,
            "session" | "project" => Self::Session,
            _ => Self::None,
        }
    }
}

fn host_agent_index(
    agents: &[AgentPane],
    host_window_id: Option<&str>,
    active_pane_ids: &std::collections::HashSet<String>,
) -> Option<usize> {
    host_window_id.and_then(|wid| {
        let mut first_match = None;
        for (i, agent) in agents.iter().enumerate() {
            if agent.window_id != wid {
                continue;
            }
            if active_pane_ids.contains(&agent.pane_id) {
                return Some(i);
            }
            first_match.get_or_insert(i);
        }
        first_match
    })
}

/// Whether the sidebar auto-follows its host window or the user is navigating manually.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionMode {
    FollowHost,
    Manual,
}

/// Runtime form of `sidebar.agent_icons`: icon strings and parsed colors.
///
/// Built once when config loads or reloads. Color strings are parsed eagerly
/// so the render path does no string parsing per row per frame, and invalid
/// colors warn once at load time instead of being silently ignored every
/// render.
///
/// The `colors` map distinguishes:
///   - `Some(Some(c))`: user override color.
///   - `Some(None)`: explicit opt-out (`color: ''`); skip the
///     `AgentKind::default_color` fallback.
///   - kind missing from map: no override, fall through to default.
#[derive(Debug, Default, Clone)]
pub struct ResolvedAgentIcons {
    pub icons: BTreeMap<String, String>,
    pub colors: BTreeMap<String, Option<Color>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitBox {
    pub idx: usize,
    pub x_start: u16,
    pub x_end: u16,
}

impl ResolvedAgentIcons {
    pub fn from_config(map: Option<&AgentIcons>) -> Self {
        let mut icons = BTreeMap::new();
        let mut colors = BTreeMap::new();
        let Some(map) = map else {
            return Self { icons, colors };
        };
        for (kind, spec) in map {
            if let Some(i) = spec.icon() {
                icons.insert(kind.clone(), i.to_string());
            }
            if let Some(raw) = spec.color() {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    colors.insert(kind.clone(), None);
                } else {
                    match Color::from_str(trimmed) {
                        Ok(c) => {
                            colors.insert(kind.clone(), Some(c));
                        }
                        Err(_) => warn!(
                            "sidebar.agent_icons.{kind}.color = {raw:?}: invalid color, ignoring"
                        ),
                    }
                }
            }
        }
        Self { icons, colors }
    }
}

const DEFAULT_COMPACT_TEMPLATE: &str = "{status_icon} {primary} {pane_suffix} {fill} {elapsed}";
const DEFAULT_TILE_TEMPLATES: &[&str] = &[
    "{primary} {pane_suffix} {fill} {elapsed}",
    "{secondary} {fill} {git_stats}",
    "{pane_title} {fill} {pr_number} {pr_checks}",
];
const DEFAULT_HORIZONTAL_TEMPLATES: &[&str] = &[
    "{status_icon} {primary} {pane_suffix} {fill} {elapsed}",
    "{secondary} {fill} {git_stats}",
    "{pane_title} {fill} {pr_number} {pr_checks}",
];

/// Tile rows for the grouped presentation. The section header already names
/// the project or session, so the row that would repeat it is dropped. What it
/// carried moves up: git stats join the pane title, and the pull request the
/// dropped row would have shown sits beside the identity line.
const DEFAULT_GROUPED_TILE_TEMPLATES: &[&str] = &[
    "{primary} {pane_suffix} {fill} {pr_number} {elapsed}",
    "{pane_title} {fill} {pr_checks} {git_stats}",
];

const DEFAULT_GROUP_HEADER_TEMPLATE: &str = "{group} {fill} {group_count}";

/// Parsed templates for one sidebar instance.
#[derive(Debug, Clone)]
pub struct ParsedTemplates {
    pub compact: Vec<Token>,
    pub tiles: Vec<Vec<Token>>,
    pub horizontal: Vec<Vec<Token>>,
    pub group_header: Vec<Token>,
}

/// Template strings currently parsed into `ParsedTemplates`. Tracked so an
/// unchanged config does not re-parse, and a broken value is not retried on
/// every snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplateStrings {
    pub compact: String,
    pub tiles: Vec<String>,
    pub horizontal: Vec<String>,
    pub group_header: String,
}

/// Latest sidebar template parsing failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
    pub location: String,
    pub message: String,
}

impl TemplateError {
    fn new(location: impl Into<String>, error: &ParseError) -> Self {
        Self {
            location: location.into(),
            message: error.to_string(),
        }
    }

    pub fn display_message(&self) -> String {
        format!("template error: {} in {}", self.message, self.location)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HostIdentity {
    pub session_name: String,
    pub session_id: String,
    pub window_id: String,
    pub pane_id: String,
}

/// Lightweight sidebar app state. No preview, git, PR, diff, or input mode.
/// One rendered entry of a vertical sidebar list.
///
/// Headers are presentation only: they are never selected, never counted by
/// `{idx}`/`{jump_key}`, and never resolve to an agent under the mouse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarRow {
    Header {
        label: String,
        count: usize,
    },
    Agent(usize),
    /// A labeled rule. Marks where the groups holding no live work begin.
    Rule {
        label: String,
    },
    /// Stands for the stale agents of a group that also holds live ones, and
    /// toggles them.
    StaleTail {
        group: String,
        count: usize,
        expanded: bool,
    },
    /// Header of a group with no live agents. It doubles as the toggle for the
    /// whole group, so a dormant project costs one row while it is collapsed.
    StaleGroup {
        label: String,
        count: usize,
        expanded: bool,
    },
}

/// Label of the rule above the groups that hold nothing but stale agents.
pub const STALE_RULE_LABEL: &str = "STALE";

pub struct SidebarApp {
    pub mux: Arc<dyn Multiplexer>,
    pub agents: Vec<AgentPane>,
    /// Presentation rows over `agents`. Without grouping this is one `Agent`
    /// row per agent in order, so row indices equal agent indices.
    pub rows: Vec<SidebarRow>,
    /// Grouping the daemon ordered the current agent list with.
    pub group_by: Option<SidebarGroupBy>,
    /// Grouping the config asks for, which the runtime toggle restores.
    pub configured_group_by: Option<SidebarGroupBy>,
    /// Groups whose stale agents are currently shown. Shared through tmux, so
    /// every sidebar pane expands and collapses together.
    pub expanded_groups: std::collections::HashSet<String>,
    /// Pane IDs the daemon judged stale, so clients and the published pane
    /// list fold the same agents.
    pub stale_pane_ids: std::collections::HashSet<String>,
    /// Number each agent answers to for `{idx}`, `{jump_key}` and
    /// `workmux sidebar jump`, or `None` for an agent this pane shows but the
    /// published list does not. Indexed by agent.
    pub jump_numbers: Vec<Option<usize>>,
    pub has_loaded_snapshot: bool,
    pub list_state: ListState,
    pub should_quit: bool,
    pub pending_exit: bool,
    /// Whether the key overlay is open.
    pub show_help: bool,
    /// Whether this sidebar still offers the grouping hint.
    pub hint_pending: bool,
    /// When true, quit without triggering global sidebar shutdown (last-pane auto-exit).
    pub quit_silent: bool,
    pub quit_reason: Option<String>,
    pub palette: ThemePalette,
    /// Terminal background mode detected before the TUI input reader starts.
    detected_theme_mode: ThemeMode,
    pub status_icons: StatusIcons,
    pub spinner_frame: u8,
    pub stale_threshold_secs: u64,
    /// Whether stale agents use the dimmed visual treatment.
    pub dim_stale: bool,
    /// Whether stale agents fold, as published by the daemon. Read from the
    /// snapshot rather than this client's config so the rows drawn here and
    /// the pane list behind `jump` fold the same agents.
    pub collapse_stale: bool,
    pub position: SidebarPosition,
    pub layout_mode: SidebarLayoutMode,
    /// Area where the list was last rendered (for mouse hit testing)
    pub list_area: Rect,
    /// Window prefix from config
    window_prefix: String,
    /// Stable identity of the sidebar's host pane, detected once at startup.
    host_identity: Option<HostIdentity>,
    /// Index of the agent in the sidebar's host window (updated each snapshot)
    pub host_agent_idx: Option<usize>,
    /// Whether this sidebar's host window is the active window in the session
    host_window_active: bool,
    selection_mode: SelectionMode,
    /// Git status per worktree path (received from daemon snapshots).
    pub git_statuses: HashMap<PathBuf, GitStatus>,
    /// PR summary per worktree path (received from daemon snapshots).
    pub pr_statuses: HashMap<PathBuf, PrSummary>,
    /// GitHub check summary per worktree path (received from daemon snapshots).
    pub check_statuses: HashMap<PathBuf, CheckSummary>,
    /// Pane IDs of agents detected as interrupted by the daemon.
    pub interrupted_pane_ids: std::collections::HashSet<String>,
    /// Pane IDs of agents manually marked as sleeping by the user.
    pub sleeping_pane_ids: std::collections::HashSet<String>,
    /// Parsed sidebar templates.
    pub templates: ParsedTemplates,
    /// Most recent template parse failure, shown in the sidebar until fixed.
    pub template_error: Option<TemplateError>,
    /// Per-agent icon and color overrides, parsed once at config load.
    pub agent_icons: ResolvedAgentIcons,
    /// Cached tile row heights, including separators, for hit testing.
    /// Indexed by presentation row and updated on every tile render.
    pub tile_heights: Vec<usize>,
    /// Cached horizontal chip hitboxes for top bar mouse hit testing.
    pub horizontal_hitboxes: Vec<HitBox>,
    /// First agent index rendered in the horizontal top bar.
    pub first_visible_agent_idx: usize,
    /// Maximum width of each horizontal item in columns.
    pub horizontal_item_width: usize,
    /// Last `config_version` from the daemon snapshot. Increments trigger a
    /// client-side config reload.
    pub last_config_version: u64,
    /// Template strings currently parsed into `templates`.
    pub current_templates: TemplateStrings,
    /// Templates as configured, kept so switching presentation can re-resolve
    /// them without re-reading config from disk.
    template_config: Option<TemplatesConfig>,
    /// Live sidebar width as last loaded from config. Stored for parity with
    /// other live keys; tmux pane resize is not driven from here.
    pub current_width: Option<SidebarWidth>,
    /// Last known window width (for detecting manual pane resizes).
    last_window_width: Option<u16>,
    /// Last known window height (for detecting manual top bar resizes).
    last_window_height: Option<u16>,
    /// Pending resize columns to process after debounce.
    pending_resize_cols: Option<u16>,
    /// Pending resize rows to process after debounce.
    pending_resize_rows: Option<u16>,
    /// Deadline after which pending resize should be processed.
    pub(super) resize_deadline: Option<Instant>,
    suppress_resize_once: bool,
    /// Filter mode: show all agents or only those in the host tmux session.
    pub filter_mode: SidebarFilterMode,
}

fn detect_terminal_theme_mode() -> ThemeMode {
    match terminal_light::luma() {
        Ok(luma) if luma > 0.6 => ThemeMode::Light,
        _ => ThemeMode::Dark,
    }
}

impl SidebarApp {
    #[cfg(test)]
    pub(crate) fn test_with_template_error(template_error: TemplateError) -> Self {
        Self {
            mux: Arc::new(crate::multiplexer::TmuxBackend::new()),
            agents: Vec::new(),
            has_loaded_snapshot: true,
            rows: Vec::new(),
            group_by: None,
            configured_group_by: None,
            expanded_groups: std::collections::HashSet::new(),
            stale_pane_ids: std::collections::HashSet::new(),
            jump_numbers: Vec::new(),
            list_state: ListState::default(),
            should_quit: false,
            pending_exit: false,
            show_help: false,
            hint_pending: false,
            quit_silent: false,
            quit_reason: None,
            palette: ThemePalette::from_config(&Config::default().theme, ThemeMode::Dark),
            detected_theme_mode: ThemeMode::Dark,
            status_icons: StatusIcons::default(),
            spinner_frame: 0,
            stale_threshold_secs: super::snapshot::STALE_THRESHOLD_SECS,
            dim_stale: true,
            collapse_stale: false,
            position: SidebarPosition::Left,
            layout_mode: SidebarLayoutMode::Compact,
            list_area: Rect::default(),
            window_prefix: "wm-".to_string(),
            host_identity: None,
            host_agent_idx: None,
            host_window_active: true,
            selection_mode: SelectionMode::FollowHost,
            git_statuses: HashMap::new(),
            pr_statuses: HashMap::new(),
            check_statuses: HashMap::new(),
            interrupted_pane_ids: std::collections::HashSet::new(),
            sleeping_pane_ids: std::collections::HashSet::new(),
            templates: ParsedTemplates {
                compact: parse_line("{primary}").unwrap(),
                tiles: vec![parse_line("{primary}").unwrap()],
                horizontal: vec![parse_line("{primary}").unwrap()],
                group_header: parse_line(DEFAULT_GROUP_HEADER_TEMPLATE).unwrap(),
            },
            template_error: Some(template_error),
            agent_icons: ResolvedAgentIcons::default(),
            tile_heights: Vec::new(),
            horizontal_hitboxes: Vec::new(),
            first_visible_agent_idx: 0,
            horizontal_item_width: 24,
            last_config_version: 0,
            current_templates: TemplateStrings {
                compact: "{primary}".to_string(),
                tiles: vec!["{primary}".to_string()],
                horizontal: vec!["{primary}".to_string()],
                group_header: DEFAULT_GROUP_HEADER_TEMPLATE.to_string(),
            },
            template_config: None,
            current_width: None,
            last_window_width: None,
            last_window_height: None,
            pending_resize_cols: None,
            pending_resize_rows: None,
            resize_deadline: None,
            suppress_resize_once: false,
            filter_mode: SidebarFilterMode::default(),
        }
    }

    /// Create a new sidebar client. Does config + host detection only, no tmux polling.
    pub fn new_client(mux: Arc<dyn Multiplexer>) -> Result<Self> {
        let config = Config::load(None)?;

        // Detection must happen before the TUI input reader starts because the
        // terminal query reads its response from stdin.
        let detected_theme_mode = detect_terminal_theme_mode();
        let theme_mode = config.theme.mode.unwrap_or(detected_theme_mode);
        let palette = ThemePalette::from_config(&config.theme, theme_mode);
        let window_prefix = config.window_prefix().to_string();
        let status_icons = config.status_icons.clone();

        let host_identity = detect_host_identity();

        let template_config = config.sidebar.templates.clone();
        let current_templates = resolved_template_strings(
            template_config.as_ref(),
            config.sidebar.group_by().is_some(),
        );
        let (templates, template_error) = parse_templates(&current_templates);
        let agent_icons = ResolvedAgentIcons::from_config(config.sidebar.agent_icons.as_ref());
        let current_width = config.sidebar.width.clone();
        let horizontal_item_width = config.sidebar.horizontal.item_width();
        let position = super::read_sidebar_position(&config);

        // Seed last_window_width so the first resize event after startup grace
        // can be compared against a baseline (fixes first-resize-dropped bug).
        let initial_window_width = query_window_width_for_pane();
        let initial_window_height = query_window_height_for_pane();

        Ok(Self {
            mux,
            agents: Vec::new(),
            has_loaded_snapshot: false,
            rows: Vec::new(),
            group_by: None,
            configured_group_by: config.sidebar.group_by(),
            expanded_groups: std::collections::HashSet::new(),
            stale_pane_ids: std::collections::HashSet::new(),
            jump_numbers: Vec::new(),
            list_state: ListState::default(),
            should_quit: false,
            pending_exit: false,
            show_help: false,
            hint_pending: claim_hint_for_this_version(),
            quit_silent: false,
            quit_reason: None,
            palette,
            detected_theme_mode,
            status_icons,
            spinner_frame: 0,
            stale_threshold_secs: super::snapshot::STALE_THRESHOLD_SECS,
            dim_stale: config.sidebar.dim_stale(),
            // Folding arrives with the first snapshot, from the daemon.
            collapse_stale: false,
            position,
            layout_mode: SidebarLayoutMode::default(),
            list_area: Rect::default(),
            window_prefix,
            host_identity,
            host_agent_idx: None,
            host_window_active: true,
            selection_mode: SelectionMode::FollowHost,
            git_statuses: HashMap::new(),
            pr_statuses: HashMap::new(),
            check_statuses: HashMap::new(),
            interrupted_pane_ids: std::collections::HashSet::new(),
            sleeping_pane_ids: std::collections::HashSet::new(),
            templates,
            template_error,
            agent_icons,
            tile_heights: Vec::new(),
            horizontal_hitboxes: Vec::new(),
            first_visible_agent_idx: 0,
            horizontal_item_width,
            last_config_version: 0,
            current_templates,
            template_config,
            current_width,
            last_window_width: initial_window_width,
            last_window_height: initial_window_height,
            pending_resize_cols: None,
            pending_resize_rows: None,
            resize_deadline: None,
            suppress_resize_once: false,
            filter_mode: SidebarFilterMode::default(),
        })
    }

    /// Apply a snapshot received from the daemon.
    pub fn apply_snapshot(&mut self, snapshot: SidebarSnapshot) {
        self.has_loaded_snapshot = true;

        // Compute host agent index from the new snapshot first so that a
        // config_version bump anchors the reload to the *current* host path,
        // not whatever was selected from the previous snapshot.
        self.host_agent_idx = host_agent_index(
            &snapshot.agents,
            self.host_window_id(),
            &snapshot.active_pane_ids,
        );

        if snapshot.config_version != self.last_config_version {
            self.last_config_version = snapshot.config_version;
            self.reload_config_from_disk(&snapshot);
        }

        self.position = snapshot.position;
        self.layout_mode = snapshot.layout_mode;
        self.filter_mode = snapshot.filter_mode;
        self.git_statuses = snapshot.git_statuses;
        self.pr_statuses = snapshot.pr_statuses;
        self.check_statuses = snapshot.check_statuses;
        self.interrupted_pane_ids = snapshot.interrupted_pane_ids;
        self.sleeping_pane_ids = snapshot.sleeping_pane_ids;

        // Track whether the host window and its sidebar pane are active.
        self.host_window_active = if let Some(identity) = &self.host_identity {
            snapshot
                .active_windows
                .contains(&(identity.session_name.clone(), identity.window_id.clone()))
        } else {
            true
        };
        let host_sidebar_active = self
            .host_identity
            .as_ref()
            .map(|identity| snapshot.active_pane_ids.contains(&identity.pane_id));

        // Manual selection belongs to direct sidebar interaction. When an agent
        // pane has focus, the selection follows the agent in the host window.
        if self.host_window_active && host_sidebar_active == Some(false) {
            self.selection_mode = SelectionMode::FollowHost;
        }

        // Preserve selection by pane_id, or by group when it rests on a toggle
        let selected_pane = self
            .selected_agent_idx()
            .and_then(|i| self.agents.get(i))
            .map(|a| a.pane_id.clone());
        let selected_toggle = self.selected_toggle();
        let previous_agent_idx = self.selected_agent_idx();

        self.group_by = snapshot.group_by;
        self.refresh_templates();
        self.collapse_stale = snapshot.collapse_stale;
        self.expanded_groups = snapshot.expanded_groups.into_iter().collect();
        self.stale_pane_ids = snapshot.stale_pane_ids;
        self.agents = snapshot.agents;

        // Apply session filter: retain only agents in the sidebar's host session.
        if self.filter_mode == SidebarFilterMode::Session
            && let Some(host_session) = self.host_session().map(str::to_owned)
        {
            self.agents.retain(|a| a.session == host_session);
            // Recompute host_agent_idx after filtering
            self.host_agent_idx = host_agent_index(
                &self.agents,
                self.host_window_id(),
                &snapshot.active_pane_ids,
            );
        }

        // Rows are rebuilt after filtering so header counts reflect what this
        // client actually shows.
        self.rebuild_rows();

        // A selection resting on a toggle follows its group, not an agent.
        if let Some(row) = selected_toggle.and_then(|group| self.toggle_row_of(&group)) {
            self.list_state.select(Some(row));
            self.sync_selection();
            return;
        }

        // Restore selection in agent space, then map back to a row.
        let restored_agent = if let Some(ref pane_id) = selected_pane {
            if let Some(idx) = self.agents.iter().position(|a| &a.pane_id == pane_id) {
                Some(idx)
            } else if !self.agents.is_empty() {
                Some(previous_agent_idx.unwrap_or(0).min(self.agents.len() - 1))
            } else {
                None
            }
        } else if !self.agents.is_empty() {
            // No pane was selected before, so start at the first agent.
            Some(0)
        } else {
            None
        };
        self.select_agent(restored_agent);

        self.sync_selection();
    }

    /// Select the agent belonging to this sidebar's host window (only in FollowHost mode).
    pub fn sync_selection(&mut self) {
        if self.selection_mode != SelectionMode::FollowHost {
            return;
        }
        if let Some(idx) = self.host_agent_idx {
            self.select_agent(Some(idx));
        }
    }

    /// Re-read the merged config from disk and apply live presentation fields.
    /// Templates are anchored at the host agent's worktree path so per-project
    /// `.workmux.yaml` overrides are honored. On any parse error, keep the
    /// previously valid templates.
    fn reload_config_from_disk(&mut self, snapshot: &SidebarSnapshot) {
        let host_path = self
            .host_agent_idx
            .and_then(|i| snapshot.agents.get(i))
            .map(|a| a.path.clone());

        let cfg_result = match host_path.as_ref() {
            Some(p) => Config::load_with_location_from(p, None).map(|(c, _)| c),
            None => Config::load(None),
        };
        let cfg = match cfg_result {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("client config reload failed: {}", e);
                return;
            }
        };

        self.template_config = cfg.sidebar.templates.clone();
        self.refresh_templates();

        self.apply_theme_config(&cfg.theme);
        self.agent_icons = ResolvedAgentIcons::from_config(cfg.sidebar.agent_icons.as_ref());
        self.horizontal_item_width = cfg.sidebar.horizontal.item_width();
        self.current_width = cfg.sidebar.width.clone();
        self.dim_stale = cfg.sidebar.dim_stale();
        self.configured_group_by = cfg.sidebar.group_by();
    }

    /// Whether to offer the grouping hint on this frame.
    ///
    /// The hint stays through the switch it invites rather than vanishing the
    /// moment it is acted on, so pressing `t` changes a line of text instead of
    /// reflowing the sidebar, and someone who has just landed in an unfamiliar
    /// mode can read the way back out of it.
    ///
    /// It is drawn only where it applies: more than one project, since a single
    /// project has nothing to group; not in the top bar, where a line is a
    /// third of the sidebar; and not over a template error, which owns the same
    /// row and matters more.
    pub fn show_hint(&self) -> bool {
        self.hint_pending
            && self.position != SidebarPosition::Top
            && self.template_error.is_none()
            && self.distinct_projects() > 1
    }

    fn distinct_projects(&self) -> usize {
        let mut projects: Vec<&str> = self
            .agents
            .iter()
            .filter_map(|agent| agent.path.parent()?.file_name()?.to_str())
            .collect();
        projects.sort_unstable();
        projects.dedup();
        projects.len()
    }

    /// The hint has served its purpose once the user answers it.
    pub fn dismiss_hint(&mut self) {
        if !self.hint_pending {
            return;
        }
        self.hint_pending = false;
        if let Ok(store) = crate::state::StateStore::new()
            && let Ok(mut settings) = store.load_settings()
        {
            settings.sidebar_hint_dismissed = true;
            let _ = store.save_settings(&settings);
        }
    }

    /// Re-resolve templates for the presentation now on screen, reparsing only
    /// when the resolved strings actually change.
    fn refresh_templates(&mut self) {
        let new_templates =
            resolved_template_strings(self.template_config.as_ref(), self.group_by.is_some());
        if new_templates != self.current_templates {
            self.template_error = try_reparse_templates(
                &mut self.templates,
                &mut self.current_templates,
                new_templates,
            );
        }
    }

    fn apply_theme_config(&mut self, theme: &ThemeConfig) {
        let mode = theme.mode.unwrap_or(self.detected_theme_mode);
        self.palette = ThemePalette::from_config(theme, mode);
    }

    pub(super) fn host_identity(&self) -> Option<&HostIdentity> {
        self.host_identity.as_ref()
    }

    pub fn host_window_id(&self) -> Option<&str> {
        self.host_identity
            .as_ref()
            .map(|identity| identity.window_id.as_str())
    }

    pub fn host_session(&self) -> Option<&str> {
        self.host_identity
            .as_ref()
            .map(|identity| identity.session_name.as_str())
    }

    pub fn host_window_active(&self) -> bool {
        self.host_window_active
    }

    pub fn tick(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1) % 10;
    }

    /// Conservative refresh interval for state that can produce time-dependent content.
    pub(super) fn refresh_interval(&self) -> Option<Duration> {
        const ANIMATION_INTERVAL: Duration = Duration::from_millis(250);
        const ELAPSED_INTERVAL: Duration = Duration::from_secs(1);

        if !self.has_loaded_snapshot
            || (self.status_icons.working.is_none()
                && self
                    .agents
                    .iter()
                    .any(|agent| agent.status == Some(crate::multiplexer::AgentStatus::Working)))
            || self
                .check_statuses
                .values()
                .any(|summary| matches!(summary.state, crate::github::CheckState::Pending { .. }))
        {
            return Some(ANIMATION_INTERVAL);
        }

        self.agents
            .iter()
            .any(|agent| agent.status_ts.is_some())
            .then_some(ELAPSED_INTERVAL)
    }

    /// Rebuild presentation rows from the current (already filtered) agents.
    ///
    /// Headers and folds appear only in vertical sidebars with grouping
    /// active; the horizontal bar and the flat list keep one row per agent, so
    /// row indices equal agent indices.
    pub(crate) fn rebuild_rows(&mut self) {
        let group_by = match self.group_by {
            Some(mode) if self.position != SidebarPosition::Top => mode,
            _ => {
                self.rows = (0..self.agents.len()).map(SidebarRow::Agent).collect();
                self.rebuild_jump_numbers(&[]);
                return;
            }
        };

        let labels: Vec<String> = self
            .agents
            .iter()
            .map(|agent| group_label(agent, group_by))
            .collect();
        let stale = self.stale_agents();
        let mut rows = Vec::with_capacity(self.agents.len() + labels.len());
        let mut pinned_agents: Vec<usize> = Vec::new();
        // Row a group opens on, and whether it holds only stale agents. The
        // daemon already sorted those groups to the end.
        let mut groups: Vec<(usize, bool)> = Vec::new();
        let mut start = 0;
        while start < labels.len() {
            let count = labels[start..]
                .iter()
                .take_while(|label| **label == labels[start])
                .count();
            let label = labels[start].clone();
            let group = start..start + count;
            let hidden = group.clone().filter(|idx| stale[*idx]).count();
            let expanded = self.expanded_groups.contains(&label);
            // A collapsed group still shows the agent the sidebar's own window
            // sits in, so the sidebar never hides where the user is. Only that
            // one row differs between panes, rather than a whole group.
            let pinned = self
                .host_agent_idx
                .filter(|idx| !expanded && group.contains(idx) && stale[*idx]);
            pinned_agents.extend(pinned);
            let folded = hidden - usize::from(pinned.is_some());
            groups.push((rows.len(), hidden == count));

            if hidden == count {
                rows.push(SidebarRow::StaleGroup {
                    label,
                    count,
                    expanded,
                });
                if expanded {
                    rows.extend(group.map(SidebarRow::Agent));
                } else {
                    rows.extend(pinned.map(SidebarRow::Agent));
                }
            } else {
                rows.push(SidebarRow::Header {
                    label: label.clone(),
                    count,
                });
                rows.extend(
                    group
                        .clone()
                        .filter(|idx| !stale[*idx])
                        .map(SidebarRow::Agent),
                );
                if folded > 0 {
                    rows.push(SidebarRow::StaleTail {
                        group: label,
                        count: folded,
                        expanded,
                    });
                }
                if expanded {
                    rows.extend(group.filter(|idx| stale[*idx]).map(SidebarRow::Agent));
                } else {
                    rows.extend(pinned.map(SidebarRow::Agent));
                }
            }
            start += count;
        }

        // Mark where live work ends. Only the trailing run of dormant groups
        // counts, so a session filter that strips a group's live agents cannot
        // strand the rule in the middle of the list.
        if self.collapse_stale
            && let Some((row, _)) = groups
                .iter()
                .rev()
                .take_while(|(_, all_stale)| *all_stale)
                .last()
            && *row > 0
        {
            rows.insert(
                *row,
                SidebarRow::Rule {
                    label: STALE_RULE_LABEL.to_string(),
                },
            );
        }
        self.rows = rows;
        self.rebuild_jump_numbers(&pinned_agents);
    }

    /// Show or hide the stale agents of one group, for every sidebar pane.
    pub fn toggle_group(&mut self, group: &str) {
        if !self.expanded_groups.remove(group) {
            self.expanded_groups.insert(group.to_string());
        }
        self.publish_expanded_groups();
    }

    /// Group of the selected agent, so a key can toggle what the mouse can.
    pub fn selected_group(&self) -> Option<String> {
        let agent = self.agents.get(self.selected_agent_idx()?)?;
        Some(group_label(agent, self.group_by?))
    }

    /// Expand every group holding stale agents, or collapse them all when any
    /// is already expanded. This is the only way to reach a collapsed group
    /// from the keyboard, since its rows are not selectable.
    pub fn toggle_all_groups(&mut self) {
        let Some(group_by) = self.group_by else {
            return;
        };
        if self.expanded_groups.is_empty() {
            let stale = self.stale_agents();
            self.expanded_groups = self
                .agents
                .iter()
                .zip(stale)
                .filter(|(_, stale)| *stale)
                .map(|(agent, _)| group_label(agent, group_by))
                .collect();
        } else {
            self.expanded_groups.clear();
        }
        self.publish_expanded_groups();
    }

    /// Persist the expanded set to tmux and rebuild immediately, so the pane
    /// that was clicked responds before the next snapshot arrives.
    fn publish_expanded_groups(&mut self) {
        let mut labels: Vec<&str> = self.expanded_groups.iter().map(String::as_str).collect();
        labels.sort_unstable();
        let value = labels.join("\t");
        let result = if value.is_empty() {
            Cmd::new("tmux")
                .args(&["set-option", "-gu", "@workmux_sidebar_expanded"])
                .run()
        } else {
            Cmd::new("tmux")
                .args(&["set-option", "-g", "@workmux_sidebar_expanded", &value])
                .run()
        };
        if let Err(error) = result {
            warn!(%error, "failed to persist expanded sidebar groups to tmux");
        }
        let selected_agent = self.selected_agent_idx();
        let selected_toggle = self.selected_toggle();
        self.rebuild_rows();
        // Stay on the toggle that was just used, so it can be toggled back.
        match selected_toggle.and_then(|group| self.toggle_row_of(&group)) {
            Some(row) => self.list_state.select(Some(row)),
            None => self.select_agent(selected_agent),
        }
        super::daemon_ctrl::signal_daemon_for(self.mux.as_ref());
    }

    /// Fill in the staleness the daemon would publish for the current agents.
    #[cfg(test)]
    pub(crate) fn refresh_stale_pane_ids(&mut self) {
        let now = super::ui::now_secs();
        self.stale_pane_ids = self
            .agents
            .iter()
            .filter(|agent| {
                super::template::context::agent_is_stale(
                    agent,
                    now,
                    self.stale_threshold_secs,
                    self.sleeping_pane_ids.contains(&agent.pane_id),
                    self.interrupted_pane_ids.contains(&agent.pane_id),
                )
            })
            .map(|agent| agent.pane_id.clone())
            .collect();
    }

    /// Staleness by agent index, or all false when stale agents are shown
    /// exactly like live ones.
    fn stale_agents(&self) -> Vec<bool> {
        self.agents
            .iter()
            .map(|agent| self.renders_collapsed(agent))
            .collect()
    }

    /// Whether an agent folds, and gives up its extra tile lines while shown.
    /// The daemon judges staleness so its published pane list and the rows
    /// here agree on which agents a fold stands for.
    pub fn renders_collapsed(&self, agent: &AgentPane) -> bool {
        self.collapse_stale && self.stale_pane_ids.contains(&agent.pane_id)
    }

    /// Number the agents the published pane list carries, in row order. An
    /// agent shown only because it is this pane's host, inside a group that is
    /// otherwise folded, is not in that list and answers to no number.
    fn rebuild_jump_numbers(&mut self, pinned: &[usize]) {
        let mut numbers = vec![None; self.agents.len()];
        let mut next = 0;
        for row in &self.rows {
            if let SidebarRow::Agent(idx) = row
                && !pinned.contains(idx)
            {
                numbers[*idx] = Some(next);
                next += 1;
            }
        }
        self.jump_numbers = numbers;
    }

    /// Agent index of a presentation row, or `None` for a header.
    pub fn agent_of_row(&self, row: usize) -> Option<usize> {
        match self.rows.get(row) {
            Some(SidebarRow::Agent(idx)) => Some(*idx),
            _ => None,
        }
    }

    /// Presentation row of an agent.
    pub fn row_of_agent(&self, agent_idx: usize) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, SidebarRow::Agent(idx) if *idx == agent_idx))
    }

    /// Currently selected agent, ignoring headers.
    pub fn selected_agent_idx(&self) -> Option<usize> {
        self.list_state
            .selected()
            .and_then(|row| self.agent_of_row(row))
    }

    /// Select an agent by index, mapping it to its presentation row. An agent
    /// inside a collapsed group falls back to the nearest agent still shown,
    /// so a selection is never lost to a row that is not rendered.
    fn select_agent(&mut self, agent_idx: Option<usize>) {
        let row = agent_idx.and_then(|idx| {
            self.row_of_agent(idx)
                .or_else(|| self.nearest_agent_row(idx))
        });
        self.list_state.select(row);
    }

    fn nearest_agent_row(&self, agent_idx: usize) -> Option<usize> {
        self.visible_agents()
            .into_iter()
            .min_by_key(|(_, idx)| idx.abs_diff(agent_idx))
            .map(|(row, _)| row)
    }

    /// Presentation row and agent index of every agent with a row, in row
    /// order. A collapsed group leaves gaps in the agent indices.
    fn visible_agents(&self) -> Vec<(usize, usize)> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(row, entry)| match entry {
                SidebarRow::Agent(idx) => Some((row, *idx)),
                _ => None,
            })
            .collect()
    }

    /// Rows the selection can rest on: agents and the toggles that stand for
    /// folded ones. Headers and rules are labels and stay unreachable.
    fn selectable_rows(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(row, entry)| match entry {
                SidebarRow::Agent(_)
                | SidebarRow::StaleTail { .. }
                | SidebarRow::StaleGroup { .. } => Some(row),
                _ => None,
            })
            .collect()
    }

    /// Row holding a group's toggle, for restoring a selection that rested on
    /// one after the rows are rebuilt.
    fn toggle_row_of(&self, group: &str) -> Option<usize> {
        self.rows.iter().position(|row| match row {
            SidebarRow::StaleTail { group: label, .. } | SidebarRow::StaleGroup { label, .. } => {
                label == group
            }
            _ => false,
        })
    }

    /// Group of the toggle the selection rests on, if it rests on one.
    pub fn selected_toggle(&self) -> Option<String> {
        match self
            .list_state
            .selected()
            .and_then(|row| self.rows.get(row))
        {
            Some(SidebarRow::StaleTail { group, .. }) => Some(group.clone()),
            Some(SidebarRow::StaleGroup { label, .. }) => Some(label.clone()),
            _ => None,
        }
    }

    /// Step `delta` selectable rows from the selection, wrapping when `wrap`
    /// is set.
    fn step_selection(&mut self, delta: isize, wrap: bool) {
        let rows = self.selectable_rows();
        if rows.is_empty() {
            return;
        }
        let current = self
            .list_state
            .selected()
            .and_then(|row| rows.iter().position(|candidate| *candidate == row));
        let next = match current {
            Some(pos) => {
                let last = rows.len() - 1;
                let target = pos as isize + delta;
                if target < 0 {
                    if wrap { last } else { 0 }
                } else if target as usize > last {
                    if wrap { 0 } else { last }
                } else {
                    target as usize
                }
            }
            None => 0,
        };
        self.list_state.select(Some(rows[next]));
    }

    pub fn next(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        self.step_selection(1, true);
    }

    pub fn previous(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        self.step_selection(-1, true);
    }

    pub fn select_first(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        if let Some((row, _)) = self.visible_agents().first() {
            self.list_state.select(Some(*row));
        }
    }

    pub fn select_last(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        if let Some(row) = self.selectable_rows().last() {
            self.list_state.select(Some(*row));
        }
    }

    /// Fold or unfold the group the selection is in, from the keyboard. On a
    /// toggle row that is the group it stands for; on an agent, its own group.
    pub fn toggle_selected_group(&mut self) {
        if let Some(group) = self.selected_toggle().or_else(|| self.selected_group()) {
            self.toggle_group(&group);
        }
    }

    /// Fold (`expand` false) or unfold the group the selection is in, for keys
    /// that mean one direction rather than a toggle.
    pub fn set_selected_group_expanded(&mut self, expand: bool) {
        let Some(group) = self.selected_toggle().or_else(|| self.selected_group()) else {
            return;
        };
        if self.expanded_groups.contains(&group) != expand {
            self.toggle_group(&group);
        }
    }

    pub fn select_index(&mut self, idx: usize) {
        self.selection_mode = SelectionMode::Manual;
        if !self.agents.is_empty() {
            self.select_agent(Some(idx.min(self.agents.len() - 1)));
        }
    }

    pub fn scroll_up(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        self.step_selection(-1, false);
    }

    pub fn scroll_down(&mut self) {
        self.selection_mode = SelectionMode::Manual;
        self.step_selection(1, false);
    }

    pub fn hit_test(&self, column: u16, row: u16) -> Option<usize> {
        self.hit_test_row(column, row)
            .and_then(|row| self.agent_of_row(row))
    }

    /// Group whose toggle sits under the cursor, if any.
    pub fn hit_test_toggle(&self, column: u16, row: u16) -> Option<String> {
        match self
            .hit_test_row(column, row)
            .and_then(|row| self.rows.get(row))
        {
            Some(SidebarRow::StaleTail { group, .. }) => Some(group.clone()),
            Some(SidebarRow::StaleGroup { label, .. }) => Some(label.clone()),
            _ => None,
        }
    }

    /// Presentation row under the cursor, if the cursor is over the list.
    fn hit_test_row(&self, column: u16, row: u16) -> Option<usize> {
        if self.agents.is_empty() {
            return None;
        }
        let area = self.list_area;
        if row < area.y || row >= area.y + area.height {
            return None;
        }

        if self.position == SidebarPosition::Top {
            return self
                .horizontal_hitboxes
                .iter()
                .find(|hit| column >= hit.x_start && column < hit.x_end)
                .map(|hit| hit.idx);
        }

        let relative_row = (row - area.y) as usize;
        let offset = self.list_state.offset();

        match self.layout_mode {
            SidebarLayoutMode::Compact => {
                let row = offset + relative_row;
                (row < self.rows.len()).then_some(row)
            }
            SidebarLayoutMode::Tiles => {
                let mut y = 0;
                for row in offset..self.rows.len() {
                    let h = self.tile_item_height(row);
                    if relative_row < y + h {
                        return Some(row);
                    }
                    y += h;
                }
                None
            }
        }
    }

    pub fn ensure_selected_visible(&mut self, visible_count: usize) {
        let Some(selected) = self.list_state.selected() else {
            return;
        };
        if selected < self.first_visible_agent_idx {
            self.first_visible_agent_idx = selected;
        } else if visible_count > 0 && selected >= self.first_visible_agent_idx + visible_count {
            self.first_visible_agent_idx = selected + 1 - visible_count;
        }
    }

    /// Height in rows of a tile-mode presentation row, separators included.
    /// Uses cached heights from the last render pass.
    fn tile_item_height(&self, row: usize) -> usize {
        self.tile_heights.get(row).copied().unwrap_or(3)
    }

    pub fn jump_to_selected(&mut self) {
        if let Some(idx) = self.selected_agent_idx()
            && let Some(agent) = self.agents.get(idx)
        {
            let pane_id = agent.pane_id.clone();
            let _ = self.mux.switch_to_pane(&pane_id, None);
            // Signal daemon directly to bypass tmux hook round-trip latency
            super::daemon_ctrl::signal_daemon_for(self.mux.as_ref());
        }
    }

    pub fn toggle_layout_mode(&mut self) {
        if self.position == SidebarPosition::Top {
            return;
        }
        self.layout_mode = match self.layout_mode {
            SidebarLayoutMode::Compact => SidebarLayoutMode::Tiles,
            SidebarLayoutMode::Tiles => SidebarLayoutMode::Compact,
        };
        // Persist to tmux so all sidebar instances pick it up immediately
        let _ = Cmd::new("tmux")
            .args(&[
                "set-option",
                "-g",
                "@workmux_sidebar_layout",
                self.layout_mode.as_str(),
            ])
            .run();
        // Persist to settings.json so it survives tmux restarts
        if let Ok(store) = crate::state::StateStore::new()
            && let Ok(mut settings) = store.load_settings()
        {
            settings.sidebar_layout = Some(self.layout_mode.as_str().to_string());
            let _ = store.save_settings(&settings);
        }
        super::daemon_ctrl::signal_daemon_for(self.mux.as_ref());
    }

    /// Toggle the sleeping state of the selected agent.
    /// Does a read-modify-write on the tmux global option so concurrent
    /// toggles from different sidebar clients don't clobber each other.
    pub fn toggle_sleeping(&mut self) {
        let Some(pane_id) = self
            .selected_agent_idx()
            .and_then(|i| self.agents.get(i))
            .map(|a| a.pane_id.clone())
        else {
            return;
        };

        // Read current set from tmux (source of truth) to avoid losing
        // toggles made by other sidebar clients since our last snapshot.
        let mut current: std::collections::HashSet<String> = Cmd::new("tmux")
            .args(&["show-option", "-gqv", "@workmux_sleeping_panes"])
            .run_and_capture_stdout()
            .ok()
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();

        if !current.insert(pane_id.clone()) {
            current.remove(&pane_id);
        }

        // Update local state for immediate rendering
        self.sleeping_pane_ids = current.clone();

        // Write back to tmux
        let panes: String = current.into_iter().collect::<Vec<_>>().join(" ");
        if panes.is_empty() {
            let _ = Cmd::new("tmux")
                .args(&["set-option", "-gu", "@workmux_sleeping_panes"])
                .run();
        } else {
            let _ = Cmd::new("tmux")
                .args(&["set-option", "-g", "@workmux_sleeping_panes", &panes])
                .run();
        }

        // Signal daemon for immediate refresh (re-sort + broadcast)
        super::daemon_ctrl::signal_daemon_for(self.mux.as_ref());
    }

    pub fn toggle_filter_mode(&mut self) {
        self.filter_mode = self.filter_mode.toggle();
        // Persist to tmux so all sidebar instances pick it up immediately
        if let Err(error) = Cmd::new("tmux")
            .args(&[
                "set-option",
                "-g",
                "@workmux_sidebar_filter",
                self.filter_mode.as_str(),
            ])
            .run()
        {
            warn!(%error, "failed to persist sidebar filter mode to tmux");
        }
        // Persist to settings.json so it survives tmux restarts
        match crate::state::StateStore::new().and_then(|store| {
            let mut settings = store.load_settings()?;
            settings.sidebar_filter = Some(self.filter_mode.as_str().to_string());
            store.save_settings(&settings)
        }) {
            Ok(()) => {}
            Err(error) => warn!(%error, "failed to persist sidebar filter mode to settings"),
        }
        // Signal daemon for immediate refresh
        super::daemon_ctrl::signal_daemon_for(self.mux.as_ref());
    }

    /// Switch between the configured grouping and one ungrouped list. The
    /// choice is a tmux global, so every sidebar and the agent navigation
    /// commands move together.
    pub fn toggle_grouping(&mut self) {
        let next = match self.group_by {
            Some(_) => None,
            None => self.configured_group_by.or(Some(SidebarGroupBy::Project)),
        };
        if let Err(error) = Cmd::new("tmux")
            .args(&[
                "set-option",
                "-g",
                "@workmux_sidebar_group_by",
                super::group_by_option_value(next),
            ])
            .run()
        {
            warn!(%error, "failed to persist sidebar grouping to tmux");
        }
        match crate::state::StateStore::new().and_then(|store| {
            let mut settings = store.load_settings()?;
            settings.sidebar_group_by = Some(super::group_by_option_value(next).to_string());
            store.save_settings(&settings)
        }) {
            Ok(()) => {}
            Err(error) => warn!(%error, "failed to persist sidebar grouping to settings"),
        }
        super::daemon_ctrl::signal_daemon_for(self.mux.as_ref());
    }

    pub fn window_prefix(&self) -> &str {
        &self.window_prefix
    }

    /// Record a resize event for debounced manual pane resize processing.
    pub fn on_resize_event(&mut self, cols: u16, rows: u16) {
        if self.suppress_resize_once {
            self.suppress_resize_once = false;
            self.pending_resize_cols = None;
            self.pending_resize_rows = None;
            self.resize_deadline = None;
            return;
        }

        match self.position {
            SidebarPosition::Left => {
                let window_w = self.query_host_window_width();
                if self.last_window_width.is_some_and(|prev| prev != window_w) {
                    self.last_window_width = Some(window_w);
                    self.pending_resize_cols = None;
                    self.pending_resize_rows = None;
                    self.resize_deadline = None;
                    let _ = super::reflow_all_to_window_extent(Some(window_w), None);
                    return;
                }
                self.pending_resize_cols = Some(cols);
            }
            SidebarPosition::Top => {
                let window_h = self.query_host_window_height();
                if self.last_window_height.is_some_and(|prev| prev != window_h) {
                    self.last_window_height = Some(window_h);
                    self.pending_resize_cols = None;
                    self.pending_resize_rows = None;
                    self.resize_deadline = None;
                    let _ = super::reflow_all_to_window_extent(Some(window_h), None);
                    return;
                }
                self.pending_resize_rows = Some(rows);
            }
        }

        self.resize_deadline = Some(Instant::now() + Duration::from_millis(500));
    }

    /// Process any pending resize after the debounce period has elapsed.
    /// Skips detection during startup grace period.
    pub fn process_pending_resize(&mut self, startup: &Instant, startup_grace: Duration) {
        if startup.elapsed() < startup_grace {
            // Suppress detection during startup to avoid false positives from
            // initial pane creation layout divergence.
            self.pending_resize_cols = None;
            self.pending_resize_rows = None;
            self.resize_deadline = None;
            return;
        }

        let Some(deadline) = self.resize_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }

        let config = Config::load(None).unwrap_or_default();
        match self.position {
            SidebarPosition::Left => {
                let Some(pane_width) = self.pending_resize_cols else {
                    self.resize_deadline = None;
                    return;
                };
                let window_w = self.query_host_window_width();
                let prev_window_w = self.last_window_width;
                self.last_window_width = Some(window_w);
                self.pending_resize_cols = None;
                self.pending_resize_rows = None;
                self.resize_deadline = None;
                let Some(prev_ww) = prev_window_w else { return };
                if prev_ww != window_w {
                    return;
                }
                let actual_width = query_pane_width_for_pane().unwrap_or(pane_width);
                let expected = super::effective_width_for(&config, window_w);
                let delta = (actual_width as i16 - expected as i16).abs();
                if delta > 0 {
                    if super::width_exceeds_defensive_max(actual_width) {
                        if let Some(wid) = self.host_window_id().map(str::to_string) {
                            super::set_sidebar_width(expected);
                            self.suppress_resize_once = true;
                            let _ = super::reflow(Some(&wid));
                        }
                    } else if config.sidebar.width.is_none() {
                        super::set_sidebar_width(actual_width);
                        if let Some(wid) = self.host_window_id() {
                            super::reflow_all_sidebars_except(wid);
                        }
                    } else if let Some(wid) = self.host_window_id().map(str::to_string) {
                        self.suppress_resize_once = true;
                        let _ = super::reflow(Some(&wid));
                    }
                }
            }
            SidebarPosition::Top => {
                let Some(pane_height) = self.pending_resize_rows else {
                    self.resize_deadline = None;
                    return;
                };
                let window_h = self.query_host_window_height();
                let prev_window_h = self.last_window_height;
                self.last_window_height = Some(window_h);
                self.pending_resize_cols = None;
                self.pending_resize_rows = None;
                self.resize_deadline = None;
                let Some(prev_wh) = prev_window_h else { return };
                if prev_wh != window_h {
                    return;
                }
                let actual_height = query_pane_height_for_pane().unwrap_or(pane_height);
                let expected = super::effective_height_for(&config, window_h);
                let delta = (actual_height as i16 - expected as i16).abs();
                if delta > 0 {
                    if config.sidebar.height.is_none() {
                        super::set_sidebar_height(actual_height);
                        if let Some(wid) = self.host_window_id() {
                            super::reflow_all_sidebars_except(wid);
                        }
                    } else if let Some(wid) = self.host_window_id().map(str::to_string) {
                        self.suppress_resize_once = true;
                        let _ = super::reflow(Some(&wid));
                    }
                }
            }
        }
    }

    fn query_host_window_width(&self) -> u16 {
        query_window_width_for_pane().unwrap_or(0)
    }

    fn query_host_window_height(&self) -> u16 {
        query_window_height_for_pane().unwrap_or(0)
    }
}

/// How long the grouping hint stays on offer before retiring itself.
const HINT_LIFETIME_SECS: u64 = 14 * 24 * 60 * 60;

/// Whether this client should still offer the grouping hint, recording the
/// installed version the first time it asks.
///
/// The hint belongs to a version, not to a sidebar: a new release offers it
/// once, and answering it, or simply leaving it alone for a fortnight, retires
/// it. State is shared by every pane, so the answer holds across windows and
/// tmux restarts.
fn claim_hint_for_this_version() -> bool {
    let Ok(store) = crate::state::StateStore::new() else {
        return false;
    };
    let Ok(mut settings) = store.load_settings() else {
        return false;
    };
    let version = env!("CARGO_PKG_VERSION");
    let now = super::ui::now_secs();

    if settings.sidebar_hint_version.as_deref() != Some(version) {
        settings.sidebar_hint_version = Some(version.to_string());
        settings.sidebar_hint_since = Some(now);
        settings.sidebar_hint_dismissed = false;
        let _ = store.save_settings(&settings);
        return true;
    }
    if settings.sidebar_hint_dismissed {
        return false;
    }
    settings
        .sidebar_hint_since
        .is_some_and(|since| now.saturating_sub(since) < HINT_LIFETIME_SECS)
}

/// Resolve template strings for the presentation currently on screen.
///
/// While grouped, a `templates.grouped` field wins, then the ungrouped
/// template of the same name, so a sidebar customized before grouping existed
/// keeps its rows. Only a user who writes neither gets the grouped defaults.
fn resolved_template_strings(
    templates: Option<&TemplatesConfig>,
    grouped: bool,
) -> TemplateStrings {
    let defaults =
        |lines: &[&str]| -> Vec<String> { lines.iter().map(|s| s.to_string()).collect() };
    let group_overrides = templates.and_then(|t| t.grouped.as_ref());
    let flat_compact = templates.and_then(|t| t.compact.clone());
    let flat_tiles = templates.and_then(|t| t.tiles.clone());

    let (compact, tiles) = if grouped {
        (
            group_overrides
                .and_then(|g| g.compact.clone())
                .or(flat_compact)
                .unwrap_or_else(|| DEFAULT_COMPACT_TEMPLATE.to_string()),
            group_overrides
                .and_then(|g| g.tiles.clone())
                .or(flat_tiles)
                .unwrap_or_else(|| defaults(DEFAULT_GROUPED_TILE_TEMPLATES)),
        )
    } else {
        (
            flat_compact.unwrap_or_else(|| DEFAULT_COMPACT_TEMPLATE.to_string()),
            flat_tiles.unwrap_or_else(|| defaults(DEFAULT_TILE_TEMPLATES)),
        )
    };

    TemplateStrings {
        compact,
        tiles,
        horizontal: templates
            .and_then(|t| t.horizontal.clone())
            .unwrap_or_else(|| defaults(DEFAULT_HORIZONTAL_TEMPLATES)),
        group_header: group_overrides
            .and_then(|g| g.header.clone())
            .unwrap_or_else(|| DEFAULT_GROUP_HEADER_TEMPLATE.to_string()),
    }
}

fn default_template_lines(default_lines: &[&str]) -> Vec<Vec<Token>> {
    default_lines
        .iter()
        .map(|s| parse_line(s).expect("default template is valid"))
        .collect()
}

fn parse_template_lines(lines: &[String], kind: &str) -> Result<Vec<Vec<Token>>, TemplateError> {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            parse_line(line).map_err(|e| {
                let location = format!("{kind}[{i}]");
                tracing::warn!("failed to parse {location} template '{}': {}", line, e);
                TemplateError::new(location, &e)
            })
        })
        .collect()
}

fn parse_templates(strings: &TemplateStrings) -> (ParsedTemplates, Option<TemplateError>) {
    let mut first_error = None;

    let compact = match parse_line(&strings.compact) {
        Ok(tokens) => tokens,
        Err(e) => {
            tracing::warn!("failed to parse compact template: {}, using default", e);
            first_error.get_or_insert_with(|| TemplateError::new("compact", &e));
            parse_line(DEFAULT_COMPACT_TEMPLATE).expect("default template is valid")
        }
    };
    let tiles = match parse_template_lines(&strings.tiles, "tiles") {
        Ok(tokens) => tokens,
        Err(e) => {
            first_error.get_or_insert(e);
            default_template_lines(DEFAULT_TILE_TEMPLATES)
        }
    };
    let horizontal = match parse_template_lines(&strings.horizontal, "horizontal") {
        Ok(tokens) => tokens,
        Err(e) => {
            first_error.get_or_insert(e);
            default_template_lines(DEFAULT_HORIZONTAL_TEMPLATES)
        }
    };

    let group_header = match parse_group_header(&strings.group_header) {
        Ok(tokens) => tokens,
        Err(e) => {
            first_error.get_or_insert(e);
            parse_line(DEFAULT_GROUP_HEADER_TEMPLATE).expect("default template is valid")
        }
    };

    (
        ParsedTemplates {
            compact,
            tiles,
            horizontal,
            group_header,
        },
        first_error,
    )
}

fn query_tmux_format_for_current_pane(format: &str) -> Option<String> {
    let pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
    let mut args = vec!["display-message", "-p"];
    if !pane_id.is_empty() {
        args.extend_from_slice(&["-t", &pane_id]);
    }
    args.push(format);
    Cmd::new("tmux")
        .args(&args)
        .run_and_capture_stdout()
        .ok()
        .map(|s| s.trim().to_string())
}

fn query_tmux_u16_for_current_pane(format: &str) -> Option<u16> {
    query_tmux_format_for_current_pane(format).and_then(|s| s.parse().ok())
}

fn query_tmux_positive_u16_for_current_pane(format: &str) -> Option<u16> {
    query_tmux_u16_for_current_pane(format).filter(|&extent| extent > 0)
}

/// Query the window width for the current tmux pane (standalone for use before
/// `Self` exists).
fn query_window_width_for_pane() -> Option<u16> {
    query_tmux_u16_for_current_pane("#{window_width}")
}

fn query_window_height_for_pane() -> Option<u16> {
    query_tmux_u16_for_current_pane("#{window_height}")
}

/// Query the actual pane width from tmux. Used to verify the sidebar pane
/// size after a manual resize, since crossterm's SIGWINCH-derived cols may
/// differ from what tmux reports via #{pane_width}.
fn query_pane_width_for_pane() -> Option<u16> {
    query_pane_extent_for_pane("#{pane_width}")
}

fn query_pane_height_for_pane() -> Option<u16> {
    query_pane_extent_for_pane("#{pane_height}")
}

fn query_pane_extent_for_pane(format: &str) -> Option<u16> {
    query_tmux_positive_u16_for_current_pane(format)
}

/// Reject agent tokens in a group header template. A header does not
/// represent one agent, so only group tokens, `{fill}`, literals and style
/// directives are meaningful there.
fn validate_group_header_tokens(tokens: &[Token]) -> Result<(), TemplateError> {
    for token in tokens {
        if let Token::Field(id) = token
            && !matches!(
                id,
                TokenId::Group | TokenId::GroupCount | TokenId::GroupStatus
            )
        {
            return Err(TemplateError {
                location: "group_header".to_string(),
                message: format!("unsupported token '{}'", id),
            });
        }
    }
    Ok(())
}

fn parse_group_header(template: &str) -> Result<Vec<Token>, TemplateError> {
    let tokens = parse_line(template).map_err(|e| TemplateError::new("group_header", &e))?;
    validate_group_header_tokens(&tokens)?;
    Ok(tokens)
}

/// Parse new template strings, mutating `templates` and the cached strings.
/// On any parse error, keep `templates` as-is and log a warning. The cached
/// strings are still updated so we don't retry the same broken value on every
/// snapshot.
fn try_reparse_templates(
    templates: &mut ParsedTemplates,
    current: &mut TemplateStrings,
    new: TemplateStrings,
) -> Option<TemplateError> {
    let mut first_error = None;

    match parse_line(&new.compact) {
        Ok(tokens) => templates.compact = tokens,
        Err(e) => {
            tracing::warn!("compact template parse error, keeping previous: {}", e);
            first_error.get_or_insert_with(|| TemplateError::new("compact", &e));
        }
    }

    match parse_template_lines(&new.tiles, "tiles") {
        Ok(tokens) => templates.tiles = tokens,
        Err(e) => {
            tracing::warn!(
                "{} template parse error, keeping previous: {}",
                e.location,
                e.message
            );
            first_error.get_or_insert(e);
        }
    }

    match parse_template_lines(&new.horizontal, "horizontal") {
        Ok(tokens) => templates.horizontal = tokens,
        Err(e) => {
            tracing::warn!(
                "{} template parse error, keeping previous: {}",
                e.location,
                e.message
            );
            first_error.get_or_insert(e);
        }
    }

    match parse_group_header(&new.group_header) {
        Ok(tokens) => templates.group_header = tokens,
        Err(e) => {
            tracing::warn!(
                "{} template parse error, keeping previous: {}",
                e.location,
                e.message
            );
            first_error.get_or_insert(e);
        }
    }

    *current = new;
    first_error
}

/// Detect the sidebar's stable host identity from its tmux pane.
fn detect_host_identity() -> Option<HostIdentity> {
    let pane_id = std::env::var("TMUX_PANE")
        .ok()
        .filter(|pane_id| !pane_id.is_empty())?;
    let output = Cmd::new("tmux")
        .args(&[
            "display-message",
            "-p",
            "-t",
            &pane_id,
            "#{session_name}\t#{session_id}\t#{window_id}\t#{pane_id}",
        ])
        .run_and_capture_stdout()
        .ok()?;

    let identity = parse_host_identity(&output)?;
    (identity.pane_id == pane_id).then_some(identity)
}

fn parse_host_identity(output: &str) -> Option<HostIdentity> {
    let mut parts = output.trim().split('\t');
    let identity = HostIdentity {
        session_name: parts.next()?.to_string(),
        session_id: parts.next()?.to_string(),
        window_id: parts.next()?.to_string(),
        pane_id: parts.next()?.to_string(),
    };
    if parts.next().is_some()
        || identity.session_name.is_empty()
        || identity.session_id.is_empty()
        || identity.window_id.is_empty()
        || identity.pane_id.is_empty()
    {
        return None;
    }

    Some(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentIconConfig, AgentIconDetails, CustomThemeColors, ThemeScheme};

    #[test]
    fn theme_reload_uses_detected_mode_after_explicit_mode_is_removed() {
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });
        app.detected_theme_mode = ThemeMode::Light;

        let mut theme = ThemeConfig {
            scheme: ThemeScheme::Default,
            mode: Some(ThemeMode::Dark),
            custom: None,
        };
        app.apply_theme_config(&theme);
        assert_eq!(app.palette.text, Color::Rgb(205, 214, 244));

        theme.mode = None;
        app.apply_theme_config(&theme);
        assert_eq!(app.palette.text, Color::Rgb(76, 79, 105));
    }

    #[test]
    fn theme_reload_removes_custom_color_overrides() {
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });
        let mut theme = ThemeConfig {
            scheme: ThemeScheme::Default,
            mode: Some(ThemeMode::Dark),
            custom: Some(CustomThemeColors {
                accent: Some("#010203".to_string()),
                ..Default::default()
            }),
        };

        app.apply_theme_config(&theme);
        assert_eq!(app.palette.accent, Color::Rgb(1, 2, 3));

        theme.custom = None;
        app.apply_theme_config(&theme);
        assert_eq!(app.palette.accent, Color::Rgb(203, 166, 247));
    }

    #[test]
    fn parses_complete_host_identity() {
        let identity = parse_host_identity("main\t$1\t@42\t%94").unwrap();

        assert_eq!(identity.session_name, "main");
        assert_eq!(identity.session_id, "$1");
        assert_eq!(identity.window_id, "@42");
        assert_eq!(identity.pane_id, "%94");
    }

    #[test]
    fn rejects_incomplete_host_identity() {
        assert!(parse_host_identity("main\t$1\t@42").is_none());
        assert!(parse_host_identity("main\t\t@42\t%94").is_none());
    }

    #[test]
    fn default_multiline_templates_show_the_pull_request() {
        assert_eq!(
            DEFAULT_TILE_TEMPLATES.last(),
            Some(&"{pane_title} {fill} {pr_number} {pr_checks}")
        );
        assert_eq!(
            DEFAULT_HORIZONTAL_TEMPLATES.last(),
            Some(&"{pane_title} {fill} {pr_number} {pr_checks}")
        );
    }

    #[test]
    fn resolved_icons_legacy_string() {
        let mut map = AgentIcons::new();
        map.insert(
            "claude".to_string(),
            AgentIconConfig::Plain("C".to_string()),
        );
        let r = ResolvedAgentIcons::from_config(Some(&map));
        assert_eq!(r.icons.get("claude").map(String::as_str), Some("C"));
        assert!(r.colors.is_empty());
    }

    #[test]
    fn resolved_icons_detailed_with_valid_color() {
        let mut map = AgentIcons::new();
        map.insert(
            "claude".to_string(),
            AgentIconConfig::Detailed(AgentIconDetails {
                icon: Some("X".to_string()),
                color: Some("#00ff00".to_string()),
            }),
        );
        let r = ResolvedAgentIcons::from_config(Some(&map));
        assert_eq!(r.icons.get("claude").map(String::as_str), Some("X"));
        assert_eq!(r.colors.get("claude"), Some(&Some(Color::Rgb(0, 255, 0))));
    }

    #[test]
    fn resolved_icons_blank_color_disables_default() {
        let mut map = AgentIcons::new();
        map.insert(
            "claude".to_string(),
            AgentIconConfig::Detailed(AgentIconDetails {
                icon: None,
                color: Some("   ".to_string()),
            }),
        );
        let r = ResolvedAgentIcons::from_config(Some(&map));
        assert_eq!(r.colors.get("claude"), Some(&None));
    }

    #[test]
    fn resolved_icons_invalid_color_is_dropped() {
        let mut map = AgentIcons::new();
        map.insert(
            "claude".to_string(),
            AgentIconConfig::Detailed(AgentIconDetails {
                icon: None,
                color: Some("not-a-color".to_string()),
            }),
        );
        let r = ResolvedAgentIcons::from_config(Some(&map));
        // No entry: lookup falls through to AgentKind::default_color at use site.
        assert!(!r.colors.contains_key("claude"));
    }

    #[test]
    fn resolved_icons_null_variant_is_no_op() {
        let mut map = AgentIcons::new();
        map.insert("claude".to_string(), AgentIconConfig::Null);
        let r = ResolvedAgentIcons::from_config(Some(&map));
        assert!(r.icons.is_empty());
        assert!(r.colors.is_empty());
    }

    fn parsed_for(s: &str) -> ParsedTemplates {
        ParsedTemplates {
            compact: parse_line(s).unwrap(),
            tiles: vec![parse_line(s).unwrap()],
            horizontal: vec![parse_line(s).unwrap()],
            group_header: parse_line(DEFAULT_GROUP_HEADER_TEMPLATE).unwrap(),
        }
    }

    fn strings_for(s: &str) -> TemplateStrings {
        TemplateStrings {
            compact: s.to_string(),
            tiles: vec![s.to_string()],
            horizontal: vec![s.to_string()],
            group_header: DEFAULT_GROUP_HEADER_TEMPLATE.to_string(),
        }
    }

    #[test]
    fn reparse_swaps_templates_on_change() {
        let mut templates = parsed_for("{primary}");
        let mut current = strings_for("{primary}");

        let new = TemplateStrings {
            compact: "{secondary} {fill}".to_string(),
            tiles: vec!["{primary} {fill} {elapsed}".to_string()],
            horizontal: vec!["{secondary} {fill} {git_stats}".to_string()],
            group_header: DEFAULT_GROUP_HEADER_TEMPLATE.to_string(),
        };
        let error = try_reparse_templates(&mut templates, &mut current, new.clone());

        assert_eq!(error, None);
        assert_eq!(current, new);
        // 3 tokens: secondary field, literal " ", fill
        assert_eq!(templates.compact.len(), 3);
    }

    #[test]
    fn reparse_keeps_previous_on_compact_parse_error() {
        let original_str = "{primary}".to_string();
        let mut templates = parsed_for(&original_str);
        let original_tokens = templates.compact.clone();
        let mut current = strings_for(&original_str);

        let bad_compact = "{unclosed";
        let mut new = strings_for(&original_str);
        new.compact = bad_compact.to_string();
        let error = try_reparse_templates(&mut templates, &mut current, new);

        assert_eq!(
            error,
            Some(TemplateError {
                location: "compact".to_string(),
                message: "unclosed brace at column 1: '{unclosed'".to_string(),
            })
        );
        // Templates unchanged
        assert_eq!(templates.compact, original_tokens);
        // But cached strings updated so we don't retry the broken value
        assert_eq!(current.compact, bad_compact);
    }

    #[test]
    fn reparse_keeps_previous_on_tile_parse_error() {
        let mut templates = parsed_for("{primary}");
        let original_tiles = templates.tiles.clone();
        let mut current = strings_for("{primary}");

        let mut new = strings_for("{primary}");
        new.tiles = vec!["{pr_status}".to_string()];
        let error = try_reparse_templates(&mut templates, &mut current, new);

        assert_eq!(templates.tiles, original_tiles);
        assert_eq!(current.tiles, vec!["{pr_status}".to_string()]);
        assert_eq!(
            error,
            Some(TemplateError {
                location: "tiles[0]".to_string(),
                message: "unknown token 'pr_status' at column 1".to_string(),
            })
        );
    }

    #[test]
    fn parse_templates_reports_invalid_horizontal_template() {
        let mut config = Config::default();
        config.sidebar.templates = Some(crate::config::TemplatesConfig {
            horizontal: Some(vec!["{primary}".to_string(), "{pr_status}".to_string()]),
            ..Default::default()
        });

        let (templates, error) = parse_templates(&resolved_template_strings(
            config.sidebar.templates.as_ref(),
            false,
        ));

        assert_eq!(
            templates.horizontal,
            default_template_lines(DEFAULT_HORIZONTAL_TEMPLATES)
        );
        assert_eq!(
            error,
            Some(TemplateError {
                location: "horizontal[1]".to_string(),
                message: "unknown token 'pr_status' at column 1".to_string(),
            })
        );
    }

    #[test]
    fn parse_templates_reports_first_error() {
        let mut config = Config::default();
        config.sidebar.templates = Some(crate::config::TemplatesConfig {
            compact: Some("{bad_compact}".to_string()),
            tiles: Some(vec!["{pr_status}".to_string()]),
            ..Default::default()
        });

        let (_, error) = parse_templates(&resolved_template_strings(
            config.sidebar.templates.as_ref(),
            false,
        ));

        assert_eq!(
            error,
            Some(TemplateError {
                location: "compact".to_string(),
                message: "unknown token 'bad_compact' at column 1".to_string(),
            })
        );
    }

    fn selection_agent(pane_id: &str, window_id: &str) -> AgentPane {
        AgentPane {
            session: "s".to_string(),
            window_name: window_id.to_string(),
            pane_id: pane_id.to_string(),
            window_id: window_id.to_string(),
            window_index: None,
            path: PathBuf::from(format!("/tmp/{pane_id}")),
            pane_title: None,
            status: None,
            status_ts: None,
            activity_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
        }
    }

    fn selection_snapshot(active_pane_ids: &[&str]) -> SidebarSnapshot {
        SidebarSnapshot {
            position: SidebarPosition::Left,
            layout_mode: SidebarLayoutMode::Tiles,
            filter_mode: SidebarFilterMode::None,
            group_by: None,
            expanded_groups: Vec::new(),
            stale_pane_ids: std::collections::HashSet::new(),
            collapse_stale: false,
            active_windows: std::collections::HashSet::from([(
                "s".to_string(),
                "@host".to_string(),
            )]),
            active_pane_ids: active_pane_ids
                .iter()
                .map(|pane_id| (*pane_id).to_string())
                .collect(),
            window_pane_counts: HashMap::new(),
            git_statuses: HashMap::new(),
            pr_statuses: HashMap::new(),
            check_statuses: HashMap::new(),
            interrupted_pane_ids: std::collections::HashSet::new(),
            sleeping_pane_ids: std::collections::HashSet::new(),
            agents: vec![
                selection_agent("%host-agent", "@host"),
                selection_agent("%other-agent", "@other"),
            ],
            config_version: 0,
        }
    }

    fn selection_app() -> SidebarApp {
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });
        app.host_identity = Some(HostIdentity {
            session_name: "s".to_string(),
            session_id: "$1".to_string(),
            window_id: "@host".to_string(),
            pane_id: "%sidebar".to_string(),
        });
        app.apply_snapshot(selection_snapshot(&["%host-agent"]));
        app
    }

    #[test]
    fn static_loaded_sidebar_has_no_periodic_refresh() {
        let app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });

        assert_eq!(app.refresh_interval(), None);
    }

    #[test]
    fn loading_and_animated_statuses_refresh_four_times_per_second() {
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });
        app.has_loaded_snapshot = false;
        assert_eq!(app.refresh_interval(), Some(Duration::from_millis(250)));

        app.has_loaded_snapshot = true;
        let mut agent = selection_agent("%working", "@host");
        agent.status = Some(crate::multiplexer::AgentStatus::Working);
        app.agents.push(agent);
        assert_eq!(app.refresh_interval(), Some(Duration::from_millis(250)));

        app.agents.clear();
        app.check_statuses.insert(
            PathBuf::from("/tmp/pending"),
            CheckSummary {
                state: crate::github::CheckState::Pending {
                    passed: 1,
                    total: 2,
                },
                meta: None,
            },
        );
        assert_eq!(app.refresh_interval(), Some(Duration::from_millis(250)));
    }

    #[test]
    fn elapsed_status_refreshes_once_per_second() {
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });
        let mut agent = selection_agent("%done", "@host");
        agent.status = Some(crate::multiplexer::AgentStatus::Done);
        agent.status_ts = Some(1);
        app.agents.push(agent);

        assert_eq!(app.refresh_interval(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn custom_working_icon_only_needs_elapsed_refresh() {
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });
        app.status_icons.working = Some("working".to_string());
        let mut agent = selection_agent("%working", "@host");
        agent.status = Some(crate::multiplexer::AgentStatus::Working);
        agent.status_ts = Some(1);
        app.agents.push(agent);

        assert_eq!(app.refresh_interval(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn active_agent_pane_restores_follow_host_selection() {
        let mut app = selection_app();
        app.select_index(1);

        app.apply_snapshot(selection_snapshot(&["%host-agent"]));

        assert_eq!(app.list_state.selected(), Some(0));
        assert_eq!(app.selection_mode, SelectionMode::FollowHost);
    }

    #[test]
    fn active_sidebar_pane_preserves_manual_selection() {
        let mut app = selection_app();
        app.select_index(1);

        app.apply_snapshot(selection_snapshot(&["%sidebar"]));

        assert_eq!(app.list_state.selected(), Some(1));
        assert_eq!(app.selection_mode, SelectionMode::Manual);
    }

    #[test]
    fn reparse_updates_valid_sections_when_tile_parse_fails() {
        let mut templates = parsed_for("{primary}");
        let mut current = strings_for("{primary}");

        let new = TemplateStrings {
            compact: "{secondary}".to_string(),
            tiles: vec!["{pr_status}".to_string()],
            horizontal: vec!["{elapsed}".to_string()],
            group_header: DEFAULT_GROUP_HEADER_TEMPLATE.to_string(),
        };
        let error = try_reparse_templates(&mut templates, &mut current, new);

        assert_eq!(templates.compact, parse_line("{secondary}").unwrap());
        assert_eq!(templates.tiles, vec![parse_line("{primary}").unwrap()]);
        assert_eq!(templates.horizontal, vec![parse_line("{elapsed}").unwrap()]);
        assert_eq!(
            error,
            Some(TemplateError {
                location: "tiles[0]".to_string(),
                message: "unknown token 'pr_status' at column 1".to_string(),
            })
        );
    }
}

#[cfg(test)]
mod grouping_tests {
    use super::*;

    fn agent(pane: &str, project: &str, session: &str) -> AgentPane {
        AgentPane {
            session: session.to_string(),
            window_name: "w".to_string(),
            pane_id: pane.to_string(),
            window_id: "@1".to_string(),
            window_index: None,
            path: PathBuf::from(format!("/tmp/{project}__worktrees/{pane}")),
            pane_title: None,
            status: None,
            status_ts: None,
            activity_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
        }
    }

    /// A snapshot from a daemon with folding on, which is what publishes the
    /// decision to clients.
    fn folding_snapshot(
        group_by: Option<SidebarGroupBy>,
        agents: Vec<AgentPane>,
    ) -> SidebarSnapshot {
        SidebarSnapshot {
            collapse_stale: true,
            ..snapshot(group_by, agents)
        }
    }

    fn snapshot(group_by: Option<SidebarGroupBy>, agents: Vec<AgentPane>) -> SidebarSnapshot {
        let now = super::super::ui::now_secs();
        let stale_pane_ids = agents
            .iter()
            .filter(|agent| {
                super::super::template::context::agent_is_stale(
                    agent,
                    now,
                    super::super::snapshot::STALE_THRESHOLD_SECS,
                    false,
                    false,
                )
            })
            .map(|agent| agent.pane_id.clone())
            .collect();
        SidebarSnapshot {
            stale_pane_ids,
            collapse_stale: false,
            position: SidebarPosition::Left,
            layout_mode: SidebarLayoutMode::Tiles,
            filter_mode: SidebarFilterMode::None,
            group_by,
            expanded_groups: Vec::new(),
            active_windows: std::collections::HashSet::new(),
            active_pane_ids: std::collections::HashSet::new(),
            window_pane_counts: HashMap::new(),
            git_statuses: HashMap::new(),
            pr_statuses: HashMap::new(),
            check_statuses: HashMap::new(),
            interrupted_pane_ids: std::collections::HashSet::new(),
            sleeping_pane_ids: std::collections::HashSet::new(),
            agents,
            config_version: 0,
        }
    }

    fn app() -> SidebarApp {
        SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        })
    }

    fn grouped_agents() -> Vec<AgentPane> {
        vec![
            agent("%1", "api", "alpha"),
            agent("%2", "api", "beta"),
            agent("%3", "mobile", "alpha"),
        ]
    }

    #[test]
    fn rows_are_the_identity_mapping_without_grouping() {
        let mut app = app();
        app.apply_snapshot(snapshot(None, grouped_agents()));
        assert_eq!(
            app.rows,
            vec![
                SidebarRow::Agent(0),
                SidebarRow::Agent(1),
                SidebarRow::Agent(2)
            ]
        );
        assert_eq!(app.selected_agent_idx(), Some(0));
    }

    #[test]
    fn grouped_rows_carry_headers_with_counts() {
        let mut app = app();
        app.apply_snapshot(snapshot(Some(SidebarGroupBy::Project), grouped_agents()));
        assert_eq!(
            app.rows,
            vec![
                SidebarRow::Header {
                    label: "api".to_string(),
                    count: 2
                },
                SidebarRow::Agent(0),
                SidebarRow::Agent(1),
                SidebarRow::Header {
                    label: "mobile".to_string(),
                    count: 1
                },
                SidebarRow::Agent(2),
            ]
        );
    }

    #[test]
    fn counts_are_computed_after_the_session_filter() {
        let mut app = app();
        app.host_identity = Some(HostIdentity {
            session_name: "alpha".to_string(),
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            pane_id: "%sidebar".to_string(),
        });
        let mut snap = snapshot(Some(SidebarGroupBy::Project), grouped_agents());
        snap.filter_mode = SidebarFilterMode::Session;
        app.apply_snapshot(snap);

        assert_eq!(
            app.rows,
            vec![
                SidebarRow::Header {
                    label: "api".to_string(),
                    count: 1
                },
                SidebarRow::Agent(0),
                SidebarRow::Header {
                    label: "mobile".to_string(),
                    count: 1
                },
                SidebarRow::Agent(1),
            ]
        );
    }

    #[test]
    fn a_single_remaining_group_still_renders_a_header() {
        let mut app = app();
        app.apply_snapshot(snapshot(
            Some(SidebarGroupBy::Session),
            vec![agent("%1", "api", "alpha")],
        ));
        assert!(matches!(app.rows[0], SidebarRow::Header { .. }));
    }

    #[test]
    fn navigation_and_selection_skip_headers() {
        let mut app = app();
        app.apply_snapshot(snapshot(Some(SidebarGroupBy::Project), grouped_agents()));

        app.select_first();
        assert_eq!(app.selected_agent_idx(), Some(0));
        assert_eq!(app.list_state.selected(), Some(1));

        app.next();
        assert_eq!(app.selected_agent_idx(), Some(1));
        app.next();
        assert_eq!(app.selected_agent_idx(), Some(2));
        assert_eq!(app.list_state.selected(), Some(4));
        app.next();
        assert_eq!(app.selected_agent_idx(), Some(0));

        app.previous();
        assert_eq!(app.selected_agent_idx(), Some(2));

        app.select_last();
        assert_eq!(app.selected_agent_idx(), Some(2));
        app.scroll_up();
        assert_eq!(app.selected_agent_idx(), Some(1));
        app.select_index(0);
        assert_eq!(app.selected_agent_idx(), Some(0));
    }

    #[test]
    fn selection_follows_the_pane_across_reorders_and_reapplies() {
        let mut app = app();
        app.apply_snapshot(snapshot(Some(SidebarGroupBy::Project), grouped_agents()));
        app.select_index(1);
        assert_eq!(app.agents[app.selected_agent_idx().unwrap()].pane_id, "%2");

        // The same agents in a different order keep the selected pane.
        let reordered = vec![
            agent("%2", "api", "beta"),
            agent("%1", "api", "alpha"),
            agent("%3", "mobile", "alpha"),
        ];
        app.apply_snapshot(snapshot(Some(SidebarGroupBy::Project), reordered));
        assert_eq!(app.agents[app.selected_agent_idx().unwrap()].pane_id, "%2");

        // Re-applying an unchanged snapshot does not drift the selection.
        app.apply_snapshot(snapshot(Some(SidebarGroupBy::Project), grouped_agents()));
        assert_eq!(app.agents[app.selected_agent_idx().unwrap()].pane_id, "%2");
    }

    #[test]
    fn rows_list_every_agent_once_in_order() {
        let mut app = app();
        app.apply_snapshot(snapshot(Some(SidebarGroupBy::Project), grouped_agents()));
        let agents: Vec<usize> = app
            .rows
            .iter()
            .filter_map(|row| match row {
                SidebarRow::Agent(idx) => Some(*idx),
                _ => None,
            })
            .collect();
        assert_eq!(agents, (0..app.agents.len()).collect::<Vec<_>>());
    }

    #[test]
    fn a_disappearing_selected_agent_lands_on_an_agent() {
        let mut app = app();
        app.apply_snapshot(snapshot(Some(SidebarGroupBy::Project), grouped_agents()));
        app.select_index(2);

        app.apply_snapshot(snapshot(
            Some(SidebarGroupBy::Project),
            vec![agent("%1", "api", "alpha"), agent("%2", "api", "beta")],
        ));
        assert!(app.selected_agent_idx().is_some());
        assert!(matches!(
            app.rows[app.list_state.selected().unwrap()],
            SidebarRow::Agent(_)
        ));
    }

    #[test]
    fn host_follow_selects_the_host_agents_row() {
        let mut app = app();
        let mut snap = snapshot(Some(SidebarGroupBy::Project), grouped_agents());
        snap.agents[2].window_id = "@host".to_string();
        app.host_identity = Some(HostIdentity {
            session_name: "alpha".to_string(),
            session_id: "$1".to_string(),
            window_id: "@host".to_string(),
            pane_id: "%sidebar".to_string(),
        });
        app.apply_snapshot(snap);

        assert_eq!(app.host_agent_idx, Some(2));
        assert_eq!(app.selected_agent_idx(), Some(2));
        assert_eq!(app.list_state.selected(), Some(4));
    }

    #[test]
    fn top_position_keeps_grouped_order_without_headers() {
        let mut app = app();
        let mut snap = snapshot(Some(SidebarGroupBy::Project), grouped_agents());
        snap.position = SidebarPosition::Top;
        app.apply_snapshot(snap);

        assert!(
            app.rows
                .iter()
                .all(|row| matches!(row, SidebarRow::Agent(_)))
        );
    }

    #[test]
    fn agent_tokens_are_rejected_in_a_group_header() {
        let tokens = parse_line("{group} {fill} {primary}").unwrap();
        let error = validate_group_header_tokens(&tokens).unwrap_err();
        assert_eq!(error.location, "group_header");
        assert!(error.message.contains("unsupported token 'primary'"));

        let valid = parse_line("#[bold]{group} {fill} {group_status} {group_count}").unwrap();
        assert!(validate_group_header_tokens(&valid).is_ok());
    }

    #[test]
    fn the_default_header_asks_only_for_the_name_and_the_count() {
        // `{group_status}` is available but not assumed: it costs columns a
        // narrow sidebar would rather give the label.
        assert_eq!(
            DEFAULT_GROUP_HEADER_TEMPLATE,
            "{group} {fill} {group_count}"
        );
    }

    #[test]
    fn a_broken_header_template_keeps_the_previous_one() {
        let mut templates = ParsedTemplates {
            compact: parse_line("{primary}").unwrap(),
            tiles: vec![parse_line("{primary}").unwrap()],
            horizontal: vec![parse_line("{primary}").unwrap()],
            group_header: parse_line(DEFAULT_GROUP_HEADER_TEMPLATE).unwrap(),
        };
        let previous = templates.group_header.clone();
        let mut current = TemplateStrings {
            compact: "{primary}".to_string(),
            tiles: vec!["{primary}".to_string()],
            horizontal: vec!["{primary}".to_string()],
            group_header: DEFAULT_GROUP_HEADER_TEMPLATE.to_string(),
        };
        let mut new = current.clone();
        new.group_header = "{group} {elapsed}".to_string();

        let error = try_reparse_templates(&mut templates, &mut current, new.clone());

        assert_eq!(templates.group_header, previous);
        assert_eq!(error.map(|e| e.location), Some("group_header".to_string()));
        // The broken value is cached so it is not retried every snapshot.
        assert_eq!(current, new);
    }

    /// An agent with recent activity, which is never stale. Fixture agents
    /// carry no activity timestamp at all, which counts as stale.
    fn active(mut agent: AgentPane) -> AgentPane {
        agent.activity_ts = Some(super::super::ui::now_secs());
        agent
    }

    fn stale_mix() -> Vec<AgentPane> {
        vec![
            agent("%1", "api", "alpha"),
            agent("%2", "mobile", "alpha"),
            active(agent("%3", "mobile", "beta")),
        ]
    }

    #[test]
    fn a_flat_list_shows_every_agent_it_carries() {
        let mut app = app();
        app.apply_snapshot(folding_snapshot(
            None,
            vec![
                active(agent("%1", "api", "alpha")),
                agent("%2", "mobile", "beta"),
            ],
        ));

        assert_eq!(app.rows, vec![SidebarRow::Agent(0), SidebarRow::Agent(1)]);
        assert_eq!(app.jump_numbers, vec![Some(0), Some(1)]);
        app.select_first();
        assert_eq!(app.selected_group(), None);
    }

    #[test]
    fn folding_follows_the_daemon_not_this_client() {
        let mut app = app();
        let agents = vec![
            active(agent("%1", "api", "alpha")),
            agent("%2", "api", "beta"),
        ];

        // A client that folded on its own would draw two rows and number them
        // 1 and 2, while the daemon's pane list carries only the live one.
        app.collapse_stale = true;
        app.apply_snapshot(snapshot(Some(SidebarGroupBy::Project), agents.clone()));

        assert!(!app.collapse_stale);
        assert_eq!(app.jump_numbers, vec![Some(0), Some(1)]);
        assert!(!app.rows.iter().any(|row| matches!(
            row,
            SidebarRow::StaleTail { .. } | SidebarRow::StaleGroup { .. }
        )));

        app.apply_snapshot(folding_snapshot(Some(SidebarGroupBy::Project), agents));

        assert!(app.collapse_stale);
        assert_eq!(app.jump_numbers, vec![Some(0), None]);
    }

    #[test]
    fn a_rule_marks_where_the_groups_without_live_work_begin() {
        let mut app = app();
        // The daemon sorts groups with no live work last; the client marks
        // where that run begins and folds each of them into one row.
        app.apply_snapshot(folding_snapshot(
            Some(SidebarGroupBy::Project),
            vec![
                active(agent("%1", "api", "alpha")),
                agent("%2", "mobile", "alpha"),
            ],
        ));

        assert_eq!(
            app.rows,
            vec![
                SidebarRow::Header {
                    label: "api".to_string(),
                    count: 1
                },
                SidebarRow::Agent(0),
                SidebarRow::Rule {
                    label: STALE_RULE_LABEL.to_string()
                },
                SidebarRow::StaleGroup {
                    label: "mobile".to_string(),
                    count: 1,
                    expanded: false
                },
            ]
        );
    }

    #[test]
    fn a_groups_stale_agents_fold_behind_one_toggle() {
        let mut app = app();
        app.apply_snapshot(folding_snapshot(
            Some(SidebarGroupBy::Project),
            vec![
                active(agent("%1", "api", "alpha")),
                agent("%2", "api", "beta"),
                agent("%3", "api", "beta"),
            ],
        ));

        assert_eq!(
            app.rows,
            vec![
                SidebarRow::Header {
                    label: "api".to_string(),
                    count: 3
                },
                SidebarRow::Agent(0),
                SidebarRow::StaleTail {
                    group: "api".to_string(),
                    count: 2,
                    expanded: false
                },
            ]
        );

        // Hidden agents keep their indices, so numbering never shifts.
        app.expanded_groups.insert("api".to_string());
        app.rebuild_rows();
        assert_eq!(
            app.rows.last(),
            Some(&SidebarRow::Agent(2)),
            "expanding shows the stale agents under the toggle"
        );
    }

    #[test]
    fn navigation_stops_on_a_toggle_and_skips_what_it_folds() {
        let mut app = app();
        app.apply_snapshot(folding_snapshot(
            Some(SidebarGroupBy::Project),
            vec![
                active(agent("%1", "api", "alpha")),
                agent("%2", "api", "beta"),
                active(agent("%3", "mobile", "alpha")),
            ],
        ));

        assert_eq!(app.selected_agent_idx(), Some(0));
        // The folded agent is skipped, but its toggle is reachable.
        app.next();
        assert_eq!(app.selected_agent_idx(), None);
        assert_eq!(app.selected_toggle().as_deref(), Some("api"));
        app.next();
        assert_eq!(app.selected_agent_idx(), Some(2));
        app.next();
        assert_eq!(app.selected_agent_idx(), Some(0));
        app.previous();
        assert_eq!(app.selected_agent_idx(), Some(2));
    }

    #[test]
    fn a_selected_toggle_expands_its_own_group_and_survives_a_snapshot() {
        let mut app = app();
        let agents = vec![
            active(agent("%1", "api", "alpha")),
            agent("%2", "api", "beta"),
        ];
        app.apply_snapshot(folding_snapshot(
            Some(SidebarGroupBy::Project),
            agents.clone(),
        ));

        app.next();
        assert_eq!(app.selected_toggle().as_deref(), Some("api"));
        app.toggle_selected_group();
        assert!(app.expanded_groups.contains("api"));
        assert!(app.rows.contains(&SidebarRow::Agent(1)));

        // A new snapshot, with the daemon echoing the expanded set back, keeps
        // the selection on the toggle rather than dropping it to an agent.
        let mut next = folding_snapshot(Some(SidebarGroupBy::Project), agents);
        next.expanded_groups = vec!["api".to_string()];
        app.apply_snapshot(next);
        assert_eq!(app.selected_toggle().as_deref(), Some("api"));

        app.set_selected_group_expanded(true);
        assert!(
            app.expanded_groups.contains("api"),
            "already open, unchanged"
        );
        app.set_selected_group_expanded(false);
        assert!(!app.expanded_groups.contains("api"));
    }

    #[test]
    fn only_the_agents_on_show_answer_to_a_jump_number() {
        let mut app = app();
        app.apply_snapshot(folding_snapshot(
            Some(SidebarGroupBy::Project),
            vec![
                active(agent("%1", "api", "alpha")),
                agent("%2", "api", "beta"),
                active(agent("%3", "mobile", "alpha")),
            ],
        ));

        // The folded agent answers to no number, and numbering closes up
        // behind it rather than leaving a gap.
        assert_eq!(app.jump_numbers, vec![Some(0), None, Some(1)]);

        app.toggle_group("api");
        assert_eq!(app.jump_numbers, vec![Some(0), Some(1), Some(2)]);
    }

    #[test]
    fn the_host_agent_of_a_folded_group_answers_to_no_number() {
        let mut app = app();
        app.apply_snapshot(folding_snapshot(
            Some(SidebarGroupBy::Project),
            vec![
                active(agent("%1", "api", "alpha")),
                agent("%2", "api", "beta"),
                active(agent("%3", "mobile", "alpha")),
            ],
        ));

        // This pane shows its own agent inside the folded group, but the
        // published pane list leaves it out, so no number may point at it.
        app.host_agent_idx = Some(1);
        app.rebuild_rows();

        assert!(app.rows.contains(&SidebarRow::Agent(1)));
        assert_eq!(app.jump_numbers, vec![Some(0), None, Some(1)]);
    }

    #[test]
    fn a_collapsed_group_still_shows_the_host_agent() {
        let mut app = app();
        app.apply_snapshot(folding_snapshot(
            Some(SidebarGroupBy::Project),
            vec![
                active(agent("%1", "api", "alpha")),
                agent("%2", "api", "beta"),
                agent("%3", "api", "gamma"),
            ],
        ));
        assert!(!app.rows.contains(&SidebarRow::Agent(1)));

        app.host_agent_idx = Some(1);
        app.rebuild_rows();

        // Only the host agent surfaces, and the toggle counts what is left.
        assert_eq!(
            app.rows,
            vec![
                SidebarRow::Header {
                    label: "api".to_string(),
                    count: 3
                },
                SidebarRow::Agent(0),
                SidebarRow::StaleTail {
                    group: "api".to_string(),
                    count: 1,
                    expanded: false
                },
                SidebarRow::Agent(1),
            ]
        );
    }

    #[test]
    fn a_list_with_no_live_work_at_all_needs_no_rule() {
        let mut app = app();
        app.apply_snapshot(folding_snapshot(
            Some(SidebarGroupBy::Project),
            grouped_agents(),
        ));

        assert!(
            !app.rows
                .iter()
                .any(|row| matches!(row, SidebarRow::Rule { .. }))
        );
    }

    #[test]
    fn groups_keep_their_alphabetical_order_without_collapsing() {
        let mut app = app();
        app.apply_snapshot(snapshot(Some(SidebarGroupBy::Project), stale_mix()));

        assert_eq!(
            app.rows,
            vec![
                SidebarRow::Header {
                    label: "api".to_string(),
                    count: 1
                },
                SidebarRow::Agent(0),
                SidebarRow::Header {
                    label: "mobile".to_string(),
                    count: 2
                },
                SidebarRow::Agent(1),
                SidebarRow::Agent(2),
            ]
        );
    }

    #[test]
    fn switching_presentation_reresolves_the_templates() {
        let mut app = app();
        let agents = vec![active(agent("%1", "api", "alpha"))];

        app.apply_snapshot(snapshot(None, agents.clone()));
        assert_eq!(app.current_templates.tiles, DEFAULT_TILE_TEMPLATES);

        // Grouping is switched at runtime, without a config reload.
        app.apply_snapshot(snapshot(Some(SidebarGroupBy::Project), agents.clone()));
        assert_eq!(app.current_templates.tiles, DEFAULT_GROUPED_TILE_TEMPLATES);

        app.apply_snapshot(snapshot(None, agents));
        assert_eq!(app.current_templates.tiles, DEFAULT_TILE_TEMPLATES);
    }

    #[test]
    fn an_override_of_one_template_keeps_the_defaults_for_the_rest() {
        let mut cfg = Config::default();
        cfg.sidebar.templates = Some(TemplatesConfig {
            grouped: Some(crate::config::GroupedTemplatesConfig {
                header: Some("{group}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        let strings = resolved_template_strings(cfg.sidebar.templates.as_ref(), true);
        assert_eq!(strings.group_header, "{group}");
        assert_eq!(strings.compact, DEFAULT_COMPACT_TEMPLATE);
    }

    #[test]
    fn grouped_rows_inherit_custom_templates_before_grouped_defaults() {
        // Nothing configured: each presentation gets its own default rows.
        assert_eq!(
            resolved_template_strings(None, false).tiles,
            DEFAULT_TILE_TEMPLATES
        );
        assert_eq!(
            resolved_template_strings(None, true).tiles,
            DEFAULT_GROUPED_TILE_TEMPLATES
        );

        // A sidebar customized before grouping existed keeps its own rows when
        // grouping is switched on.
        let custom = TemplatesConfig {
            tiles: Some(vec!["{primary}".to_string()]),
            ..Default::default()
        };
        assert_eq!(
            resolved_template_strings(Some(&custom), true).tiles,
            vec!["{primary}".to_string()]
        );

        // Until it says what the grouped rows should be.
        let both = TemplatesConfig {
            tiles: Some(vec!["{primary}".to_string()]),
            grouped: Some(crate::config::GroupedTemplatesConfig {
                tiles: Some(vec!["{pane_title}".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            resolved_template_strings(Some(&both), true).tiles,
            vec!["{pane_title}".to_string()]
        );
        assert_eq!(
            resolved_template_strings(Some(&both), false).tiles,
            vec!["{primary}".to_string()]
        );
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    #[test]
    fn host_agent_index_prefers_active_pane() {
        let agents = vec![
            AgentPane {
                session: "s".to_string(),
                window_name: "w".to_string(),
                pane_id: "%1".to_string(),
                window_id: "@1".to_string(),
                window_index: None,
                path: PathBuf::from("/tmp/a"),
                pane_title: None,
                status: None,
                status_ts: None,
                activity_ts: None,
                updated_ts: None,
                window_cmd: None,
                agent_command: None,
                agent_kind: None,
            },
            AgentPane {
                session: "s".to_string(),
                window_name: "w".to_string(),
                pane_id: "%2".to_string(),
                window_id: "@1".to_string(),
                window_index: None,
                path: PathBuf::from("/tmp/b"),
                pane_title: None,
                status: None,
                status_ts: None,
                activity_ts: None,
                updated_ts: None,
                window_cmd: None,
                agent_command: None,
                agent_kind: None,
            },
        ];
        let active_panes = std::collections::HashSet::from(["%2".to_string()]);

        assert_eq!(
            host_agent_index(&agents, Some("@1"), &active_panes),
            Some(1)
        );
    }

    #[test]
    fn filter_mode_toggle() {
        assert_eq!(SidebarFilterMode::None.toggle(), SidebarFilterMode::Session);
        assert_eq!(SidebarFilterMode::Session.toggle(), SidebarFilterMode::None);
    }

    #[test]
    fn filter_mode_roundtrip_strings() {
        for mode in [SidebarFilterMode::None, SidebarFilterMode::Session] {
            assert_eq!(SidebarFilterMode::from_str(mode.as_str()), mode);
        }
    }

    #[test]
    fn invalid_filter_mode_maps_to_all() {
        assert_eq!(SidebarFilterMode::from_str(""), SidebarFilterMode::None);
        assert_eq!(
            SidebarFilterMode::from_str("unknown"),
            SidebarFilterMode::None
        );
    }

    #[test]
    fn filter_mode_from_str_case_insensitive() {
        assert_eq!(
            SidebarFilterMode::from_str("Session"),
            SidebarFilterMode::Session
        );
        assert_eq!(
            SidebarFilterMode::from_str("SESSION"),
            SidebarFilterMode::Session
        );
        assert_eq!(
            SidebarFilterMode::from_str("project"),
            SidebarFilterMode::Session
        );
    }

    #[test]
    fn filter_mode_default_shows_all_sessions() {
        assert_eq!(SidebarFilterMode::default(), SidebarFilterMode::None);
    }
}
