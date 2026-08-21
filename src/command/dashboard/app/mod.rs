//! Application state and business logic for the dashboard TUI.

mod agents;
mod appearance;
mod background;
mod events;
mod preview;
mod types;
mod worktrees;

pub use types::*;

use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::widgets::TableState;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::git::{self, GitStatus};
use crate::github::{CheckSummary, PrSummary};
use crate::multiplexer::{AgentPane, Multiplexer};
use crate::state::StateStore;
use crate::workflow::types::WorktreeInfo;

use super::keymap::Context;
use super::ui::theme::ThemePalette;

const PR_FETCH_INTERVAL: Duration = Duration::from_secs(30);

use super::scope::ScopeMode;
use super::settings::{load_hide_stale, load_last_pane_id, load_preview_size};
use super::sort::{SortMode, WorktreeSortMode};

/// App state for the TUI
pub struct App {
    /// Terminal area used to route mouse coordinates against the rendered frame.
    pub terminal_area: Rect,
    /// The multiplexer backend
    pub mux: Arc<dyn Multiplexer>,
    pub agents: Vec<AgentPane>,
    /// Full agent list before name/stale filtering (populated by refresh())
    all_agents: Vec<AgentPane>,
    pub table_state: TableState,
    /// Track the selected item by pane_id to preserve selection across reorders
    selected_pane_id: Option<String>,
    /// The directory from which the dashboard was launched (used to indicate the active worktree).
    pub current_worktree: Option<PathBuf>,
    pub stale_threshold_secs: u64,
    pub config: Config,
    pub should_quit: bool,
    pub should_jump: bool,
    pub sort_mode: SortMode,
    /// Current view mode (Dashboard or Diff modal)
    pub view_mode: ViewMode,
    /// Cached preview of the currently selected agent's terminal output
    pub preview: Option<String>,
    /// Track which pane_id the preview was captured from (to detect selection changes)
    preview_pane_id: Option<String>,
    /// Input mode: keystrokes are sent directly to the selected agent's pane
    pub input_mode: bool,
    /// Manual scroll offset for the preview (None = auto-scroll to bottom)
    pub preview_scroll: Option<u16>,
    /// Number of lines in the current preview content
    pub preview_line_count: u16,
    /// Height of the preview area (updated during rendering)
    pub preview_height: u16,
    /// Git status for each worktree path
    pub git_statuses: HashMap<PathBuf, GitStatus>,
    /// Last time git status was fetched (to throttle background fetches)
    last_git_fetch: std::time::Instant,
    /// Flag to track if a git fetch is in progress (prevents thread pile-up)
    pub is_git_fetching: Arc<AtomicBool>,
    /// PR info indexed by repo root, then branch name
    pr_statuses: HashMap<PathBuf, HashMap<String, PrSummary>>,
    /// GitHub check info indexed by repo root, then branch name
    check_statuses: HashMap<PathBuf, HashMap<String, CheckSummary>>,
    /// Last PR fetch time
    last_pr_fetch: std::time::Instant,
    /// Flag to prevent concurrent PR fetches
    is_pr_fetching: Arc<AtomicBool>,
    /// Unified event sender (cloned by all background threads)
    pub event_tx: mpsc::Sender<AppEvent>,
    /// Cache of repo roots for agent paths
    repo_roots: HashMap<PathBuf, PathBuf>,
    /// Frame counter for spinner animation (increments each tick)
    pub spinner_frame: u8,
    /// Whether to hide stale agents from the list
    pub hide_stale: bool,
    /// Whether to show the help overlay
    pub show_help: bool,
    /// Preview pane size as percentage (1-90). Higher = larger preview.
    pub preview_size: u8,
    /// Last jumped-to pane_id for quick toggle (cached from settings)
    last_pane_id: Option<String>,
    /// Color palette based on the configured theme
    pub palette: ThemePalette,
    /// Current color scheme
    pub scheme: crate::config::ThemeScheme,
    /// Current theme mode (dark/light)
    pub theme_mode: crate::config::ThemeMode,
    /// Path to the project config file (for persisting theme changes)
    config_path: Option<PathBuf>,
    /// Dashboard scope filter mode (All or Session)
    pub scope_mode: ScopeMode,
    /// Session name at launch time (for session scope filtering)
    launch_session: Option<String>,
    /// Whether the filter input is active (accepting keystrokes)
    pub filter_active: bool,
    /// Text filter for filtering agents by name. Empty string means no filter.
    pub filter_text: String,
    /// Pane ID awaiting kill confirmation (set when pressing x on a working agent)
    pub pending_kill_pane_id: Option<String>,
    /// Which tab is active (Agents or Worktrees)
    pub active_tab: DashboardTab,
    /// Full worktree list from background fetch (baseline for filtering/sorting)
    all_worktrees: Vec<WorktreeInfo>,
    /// Filtered and sorted worktree list for display
    pub worktrees: Vec<WorktreeInfo>,
    /// Table state for the worktree view
    pub worktree_table_state: TableState,
    /// Track selected worktree by path for stable selection
    selected_worktree_path: Option<PathBuf>,
    /// Filter text for worktree view (separate from agent filter)
    pub worktree_filter_text: String,
    /// Whether worktree filter input is active
    pub worktree_filter_active: bool,
    /// Current sort mode for the worktree list
    pub worktree_sort_mode: WorktreeSortMode,
    /// Pending worktree removal (shown in confirmation modal)
    pub pending_remove: Option<RemovePlan>,
    /// Pending bulk sweep state (shown in sweep modal)
    pub pending_sweep: Option<SweepState>,
    /// Pending project picker state (shown in project picker modal)
    pub pending_project_picker: Option<ProjectPicker>,
    /// Pending base branch picker state (shown in base picker modal)
    pub pending_base_picker: Option<BaseBranchPicker>,
    /// Pending add-worktree modal state
    pub pending_add_worktree: Option<AddWorktreeState>,
    /// Active sweep progress overlay (blocks input while removing worktrees)
    pub sweep_progress: Option<SweepProgress>,
    /// Override which repo's worktrees are shown (name, git root path)
    pub worktree_project_override: Option<(String, PathBuf)>,
    /// Flag to prevent concurrent worktree fetches
    is_worktree_fetching: Arc<AtomicBool>,
    /// Last time worktree list was fetched
    last_worktree_fetch: std::time::Instant,
    /// Cached git log preview for selected worktree
    pub worktree_preview: Option<String>,
    /// Path of the worktree whose preview is cached
    worktree_preview_path: Option<PathBuf>,
    /// Temporary status message shown in the footer (auto-clears after timeout)
    pub status_message: Option<(String, std::time::Instant)>,
    /// Whether to show the "New: workmux sidebar" tip in the tab header
    pub show_sidebar_tip: bool,
    /// Pane IDs of agents detected as interrupted by the sidebar daemon.
    pub interrupted_pane_ids: std::collections::HashSet<String>,
    /// Pending command palette state (shown in command palette modal)
    pub pending_command_palette: Option<CommandPaletteState>,
    /// Context actions opened for an agent or worktree row.
    pub pending_row_context: Option<RowContextMenu>,
    /// Last selected row click used to recognize double-click activation.
    pub last_mouse_row_click: Option<MouseRowClick>,
}

impl App {
    pub fn new(
        mux: Arc<dyn Multiplexer>,
        cli_session_filter: bool,
        event_tx: mpsc::Sender<AppEvent>,
    ) -> Result<Self> {
        let config = Config::load(None)?;

        // Get the active pane's directory to indicate the active worktree.
        // Try multiplexer first (handles popup case), fall back to current_dir.
        let current_worktree = mux
            .get_client_active_pane_path()
            .or_else(|_| std::env::current_dir())
            .ok();

        // Preview size: CLI override > tmux saved > config default
        // Clamp to 10-90 to handle manually corrupted tmux variables
        let preview_size = load_preview_size()
            .unwrap_or_else(|| config.dashboard.preview_size())
            .clamp(10, 90);

        // Determine theme mode: config override or auto-detect from terminal
        let theme_mode = config
            .theme
            .mode
            .unwrap_or_else(|| match terminal_light::luma() {
                Ok(luma) if luma > 0.6 => crate::config::ThemeMode::Light,
                _ => crate::config::ThemeMode::Dark,
            });
        let scheme = config.theme.scheme;
        let palette = ThemePalette::from_config(&config.theme, theme_mode);
        let config_path = crate::config::global_config_path();
        // Sort mode: persisted preference > config.yaml default > priority
        let sort_mode = SortMode::load_with_default(
            config
                .dashboard
                .sort_mode
                .as_deref()
                .map(SortMode::parse)
                .unwrap_or_default(),
        );
        let scope_mode = if cli_session_filter {
            ScopeMode::Session
        } else {
            ScopeMode::load()
        };
        let launch_session = mux.client_session();
        let git_statuses = git::load_status_cache();
        let pr_statuses = crate::github::load_pr_cache();
        let check_statuses = crate::github::load_check_cache();
        let hide_stale = load_hide_stale();
        let last_pane_id = load_last_pane_id();

        let mut app = Self {
            terminal_area: Rect::default(),
            mux,
            agents: Vec::new(),
            all_agents: Vec::new(),
            table_state: TableState::default(),
            selected_pane_id: None,
            current_worktree,
            stale_threshold_secs: 60 * 60, // 60 minutes
            config,
            should_quit: false,
            should_jump: false,
            sort_mode,
            view_mode: ViewMode::default(),
            preview: None,
            preview_pane_id: None,
            input_mode: false,
            preview_scroll: None,
            preview_line_count: 0,
            preview_height: 0,
            git_statuses,
            // Set to past to trigger immediate fetch on first refresh
            last_git_fetch: std::time::Instant::now() - Duration::from_secs(60),
            is_git_fetching: Arc::new(AtomicBool::new(false)),
            pr_statuses,
            check_statuses,
            // Set to past to trigger immediate fetch on first refresh
            last_pr_fetch: std::time::Instant::now() - PR_FETCH_INTERVAL,
            is_pr_fetching: Arc::new(AtomicBool::new(false)),
            event_tx,
            repo_roots: HashMap::new(),
            spinner_frame: 0,
            hide_stale,
            show_help: false,
            preview_size,
            last_pane_id,
            palette,
            scheme,
            theme_mode,
            config_path,
            scope_mode,
            launch_session,
            filter_active: false,
            filter_text: String::new(),
            pending_kill_pane_id: None,
            active_tab: DashboardTab::Agents,
            all_worktrees: Vec::new(),
            worktrees: Vec::new(),
            worktree_table_state: TableState::default(),
            selected_worktree_path: None,
            worktree_filter_text: String::new(),
            worktree_filter_active: false,
            worktree_sort_mode: WorktreeSortMode::load(),
            pending_remove: None,
            pending_sweep: None,
            pending_project_picker: None,
            pending_base_picker: None,
            pending_add_worktree: None,
            worktree_project_override: None,
            is_worktree_fetching: Arc::new(AtomicBool::new(false)),
            // Set to past so first switch triggers immediate fetch
            last_worktree_fetch: std::time::Instant::now() - Duration::from_secs(60),
            worktree_preview: None,
            worktree_preview_path: None,
            status_message: None,
            show_sidebar_tip: crate::tips::should_show_sidebar_tip(),
            interrupted_pane_ids: std::collections::HashSet::new(),
            pending_command_palette: None,
            pending_row_context: None,
            last_mouse_row_click: None,
            sweep_progress: None,
        };

        app.refresh();

        // Select first item if available
        if !app.agents.is_empty() {
            app.table_state.select(Some(0));
            app.selected_pane_id = app.agents.first().map(|a| a.pane_id.clone());
        }

        // Initial preview fetch
        app.update_preview();

        // Fetch worktree list early so PR fetching can include worktree repo roots
        app.spawn_worktree_fetch();

        Ok(app)
    }

    pub fn refresh(&mut self) {
        // Load agents from StateStore with reconciliation against live pane state
        self.all_agents = StateStore::new()
            .and_then(|store| store.load_reconciled_agents(self.mux.as_ref()))
            .unwrap_or_default();

        // Load interrupted pane IDs from daemon runtime state
        if let Ok(store) = StateStore::new() {
            let backend = self.mux.name();
            let instance = self.mux.instance_id();
            let runtime = store.read_runtime(backend, &instance);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Ignore stale runtime state (daemon not running for >15s)
            if now.saturating_sub(runtime.updated_ts) <= 15 {
                self.interrupted_pane_ids = runtime.interrupted_pane_ids;
            } else {
                self.interrupted_pane_ids.clear();
            }
        }

        // Cache repo roots for ALL agents before filtering (project picker needs all projects)
        let paths_to_resolve: Vec<PathBuf> = self
            .all_agents
            .iter()
            .filter(|a| !self.repo_roots.contains_key(&a.path))
            .map(|a| a.path.clone())
            .collect();

        if !paths_to_resolve.is_empty() {
            // Resolve repo roots in parallel using threads
            let results: Vec<_> = paths_to_resolve
                .into_iter()
                .map(|path| {
                    std::thread::spawn(move || {
                        let root = git::get_repo_root_for(&path).ok();
                        (path, root)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .collect();

            for (path, root) in results {
                if let Some(r) = root {
                    self.repo_roots.insert(path, r);
                }
            }
        }

        // Apply session scope filter after caching repo roots
        self.scope_mode
            .retain_agents(&mut self.all_agents, self.launch_session.as_deref());

        // Trigger background git status fetch every 5 seconds
        if self.last_git_fetch.elapsed() >= Duration::from_secs(5) {
            self.last_git_fetch = std::time::Instant::now();
            self.spawn_git_status_fetch();
        }

        // Trigger PR fetch every 30 seconds (only update timer if fetch actually started)
        if self.last_pr_fetch.elapsed() >= PR_FETCH_INTERVAL && self.spawn_pr_status_fetch() {
            self.last_pr_fetch = std::time::Instant::now();
        }

        // Trigger background worktree fetch every 5 seconds
        if self.active_tab == DashboardTab::Worktrees
            && self.last_worktree_fetch.elapsed() >= Duration::from_secs(5)
        {
            self.last_worktree_fetch = std::time::Instant::now();
            self.spawn_worktree_fetch();
        }

        // Clear expired status messages
        if let Some((_, created)) = &self.status_message
            && created.elapsed() >= Duration::from_millis(1500)
        {
            self.status_message = None;
        }

        // Apply name filter, stale filter, sort, and restore selection
        self.apply_filters();
    }

    pub(crate) fn keymap_context(&self) -> Context {
        match &self.view_mode {
            ViewMode::Dashboard => match self.active_tab {
                DashboardTab::Agents => {
                    if self.filter_active {
                        Context::DashboardFilter
                    } else if self.input_mode {
                        Context::DashboardInput
                    } else {
                        Context::DashboardNormal
                    }
                }
                DashboardTab::Worktrees => {
                    if self.worktree_filter_active {
                        Context::WorktreeFilter
                    } else {
                        Context::WorktreeNormal
                    }
                }
            },
            ViewMode::Diff(diff) => {
                if diff.patch_mode {
                    if diff.comment_input.is_some() {
                        Context::Comment
                    } else {
                        Context::Patch
                    }
                } else {
                    Context::DiffNormal
                }
            }
        }
    }
}
