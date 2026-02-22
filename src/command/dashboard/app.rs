//! Application state and business logic for the dashboard TUI.

use anyhow::Result;
use ratatui::style::{Color, Style};
use ratatui::widgets::TableState;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::vcs::{self, VcsStatus};
use crate::github::PrSummary;
use crate::multiplexer::{AgentPane, AgentStatus, Multiplexer};
use crate::state::StateStore;

use super::ui::theme::ThemePalette;

const PR_FETCH_INTERVAL: Duration = Duration::from_secs(30);

use super::agent;
use super::diff::DiffView;
use super::settings::{
    load_hide_stale, load_last_pane_id, load_preview_size, save_hide_stale, save_last_pane_id,
    save_preview_size,
};
use super::sort::SortMode;
use super::spinner::SPINNER_FRAMES;

/// Number of lines to capture from the agent's terminal for preview (scrollable history)
pub const PREVIEW_LINES: u16 = 200;

/// Current view mode of the dashboard
#[derive(Debug, Default, PartialEq)]
pub enum ViewMode {
    #[default]
    Dashboard,
    Diff(Box<DiffView>),
}

/// App state for the TUI
pub struct App {
    /// The multiplexer backend
    pub mux: Arc<dyn Multiplexer>,
    pub agents: Vec<AgentPane>,
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
    pub git_statuses: HashMap<PathBuf, VcsStatus>,
    /// Channel receiver for git status updates from background thread
    git_rx: mpsc::Receiver<(PathBuf, VcsStatus)>,
    /// Channel sender for git status updates (cloned for background threads)
    git_tx: mpsc::Sender<(PathBuf, VcsStatus)>,
    /// Last time git status was fetched (to throttle background fetches)
    last_git_fetch: std::time::Instant,
    /// Flag to track if a git fetch is in progress (prevents thread pile-up)
    pub is_git_fetching: Arc<AtomicBool>,
    /// PR info indexed by repo root, then branch name
    pr_statuses: HashMap<PathBuf, HashMap<String, PrSummary>>,
    /// Channel for PR status updates (repo_root, prs)
    pr_rx: mpsc::Receiver<(PathBuf, HashMap<String, PrSummary>)>,
    pr_tx: mpsc::Sender<(PathBuf, HashMap<String, PrSummary>)>,
    /// Last PR fetch time
    last_pr_fetch: std::time::Instant,
    /// Flag to prevent concurrent PR fetches
    is_pr_fetching: Arc<AtomicBool>,
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
}

impl App {
    pub fn new(mux: Arc<dyn Multiplexer>) -> Result<Self> {
        let config = Config::load(None)?;
        let (git_tx, git_rx) = mpsc::channel();
        let (pr_tx, pr_rx) = mpsc::channel();

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

        let palette = ThemePalette::from_theme(config.theme);
        let sort_mode = SortMode::load();
        let git_statuses = vcs::try_detect_vcs()
            .map(|v| v.load_status_cache())
            .unwrap_or_default();
        let pr_statuses = crate::github::load_pr_cache();
        let hide_stale = load_hide_stale();
        let last_pane_id = load_last_pane_id();

        let mut app = Self {
            mux,
            agents: Vec::new(),
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
            git_rx,
            git_tx,
            // Set to past to trigger immediate fetch on first refresh
            last_git_fetch: std::time::Instant::now() - Duration::from_secs(60),
            is_git_fetching: Arc::new(AtomicBool::new(false)),
            pr_statuses,
            pr_rx,
            pr_tx,
            // Set to past to trigger immediate fetch on first refresh
            last_pr_fetch: std::time::Instant::now() - PR_FETCH_INTERVAL,
            is_pr_fetching: Arc::new(AtomicBool::new(false)),
            repo_roots: HashMap::new(),
            spinner_frame: 0,
            hide_stale,
            show_help: false,
            preview_size,
            last_pane_id,
            palette,
        };

        app.refresh();

        // Select first item if available
        if !app.agents.is_empty() {
            app.table_state.select(Some(0));
            app.selected_pane_id = app.agents.first().map(|a| a.pane_id.clone());
        }

        // Initial preview fetch
        app.update_preview();

        Ok(app)
    }

    pub fn refresh(&mut self) {
        // Load agents from StateStore with reconciliation against live pane state
        self.agents = StateStore::new()
            .and_then(|store| store.load_reconciled_agents(self.mux.as_ref()))
            .unwrap_or_default();

        self.sort_agents();

        // Cache repo roots for new agents (parallel execution)
        let paths_to_resolve: Vec<PathBuf> = self
            .agents
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
                        let root = vcs::try_detect_vcs().and_then(|v| v.get_repo_root_for(&path).ok());
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

        // Filter out stale agents if hide_stale is enabled
        if self.hide_stale {
            let threshold = self.stale_threshold_secs;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            self.agents.retain(|agent| {
                agent
                    .status_ts
                    .map(|ts| now.saturating_sub(ts) <= threshold)
                    .unwrap_or(true) // Keep agents without timestamp
            });
        }

        // Consume any pending git status updates from background thread
        while let Ok((path, status)) = self.git_rx.try_recv() {
            self.git_statuses.insert(path, status);
        }

        // Trigger background git status fetch every 5 seconds
        if self.last_git_fetch.elapsed() >= Duration::from_secs(5) {
            self.last_git_fetch = std::time::Instant::now();
            self.spawn_git_status_fetch();
        }

        // Consume any pending PR status updates
        while let Ok((repo_root, prs)) = self.pr_rx.try_recv() {
            self.pr_statuses.insert(repo_root, prs);
        }

        // Trigger PR fetch every 30 seconds
        if self.last_pr_fetch.elapsed() >= PR_FETCH_INTERVAL {
            self.last_pr_fetch = std::time::Instant::now();
            self.spawn_pr_status_fetch();
        }

        // Restore selection by pane_id to follow the item across reorders
        if let Some(ref pane_id) = self.selected_pane_id {
            // Find the new index of the previously selected item
            if let Some(new_idx) = self.agents.iter().position(|a| &a.pane_id == pane_id) {
                self.table_state.select(Some(new_idx));
            } else {
                // Item was removed (filtered out or closed), keep selection in bounds
                self.selected_pane_id = None;
                if self.agents.is_empty() {
                    self.table_state.select(None);
                } else if let Some(selected) = self.table_state.selected() {
                    if selected >= self.agents.len() {
                        self.table_state.select(Some(self.agents.len() - 1));
                    }
                    // Update selected_pane_id to the new selection
                    if let Some(idx) = self.table_state.selected() {
                        self.selected_pane_id = self.agents.get(idx).map(|a| a.pane_id.clone());
                    }
                }
            }
        } else if let Some(selected) = self.table_state.selected() {
            // No tracked pane_id but we have a selection - adjust if out of bounds
            if selected >= self.agents.len() {
                self.table_state.select(if self.agents.is_empty() {
                    None
                } else {
                    Some(self.agents.len() - 1)
                });
            }
            // Sync selected_pane_id to ensure we start tracking the current selection
            if let Some(idx) = self.table_state.selected() {
                self.selected_pane_id = self.agents.get(idx).map(|a| a.pane_id.clone());
            }
        }

        // Update preview for current selection
        self.update_preview();
    }

    /// Spawn a background thread to fetch git status for all agent worktrees
    fn spawn_git_status_fetch(&self) {
        // Skip if a fetch is already in progress (prevents thread pile-up)
        if self
            .is_git_fetching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let tx = self.git_tx.clone();
        let is_fetching = self.is_git_fetching.clone();
        let agent_paths: Vec<PathBuf> = self.agents.iter().map(|a| a.path.clone()).collect();

        std::thread::spawn(move || {
            // Reset flag when thread completes (even on panic)
            struct ResetFlag(Arc<AtomicBool>);
            impl Drop for ResetFlag {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _reset = ResetFlag(is_fetching);

            for path in agent_paths {
                let status = vcs::try_detect_vcs()
                    .map(|v| v.get_status(&path))
                    .unwrap_or_default();
                // Ignore send errors (receiver dropped means app is shutting down)
                let _ = tx.send((path, status));
            }
        });
    }

    /// Spawn a background thread to fetch PR status for all repos
    fn spawn_pr_status_fetch(&self) {
        // Skip if already fetching
        if self
            .is_pr_fetching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        // Collect repo roots that have at least one non-main feature branch
        let repo_roots: std::collections::HashSet<PathBuf> = self
            .agents
            .iter()
            .filter(|agent| {
                let Some(status) = self.git_statuses.get(&agent.path) else {
                    return false;
                };
                let Some(ref branch) = status.branch else {
                    return false;
                };
                // Skip main/master - they don't need PR status
                branch != "main" && branch != "master"
            })
            .filter_map(|agent| self.repo_roots.get(&agent.path).cloned())
            .collect();

        let tx = self.pr_tx.clone();
        let is_fetching = self.is_pr_fetching.clone();

        std::thread::spawn(move || {
            struct ResetFlag(Arc<AtomicBool>);
            impl Drop for ResetFlag {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _reset = ResetFlag(is_fetching);

            for repo_root in repo_roots {
                match crate::github::list_prs_in_repo(&repo_root) {
                    Ok(prs) => {
                        let _ = tx.send((repo_root, prs));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch PRs for {:?}: {}", repo_root, e);
                    }
                }
            }
        });
    }

    /// Update the preview for the currently selected agent.
    /// Only fetches if the selection has changed or preview is stale.
    pub fn update_preview(&mut self) {
        if !self.mux.supports_preview() {
            return;
        }
        let current_pane_id = self
            .table_state
            .selected()
            .and_then(|idx| self.agents.get(idx))
            .map(|agent| agent.pane_id.clone());

        // Only fetch if selection changed
        if current_pane_id != self.preview_pane_id {
            self.preview_pane_id = current_pane_id.clone();
            self.preview = current_pane_id
                .as_ref()
                .and_then(|pane_id| self.mux.capture_pane(pane_id, PREVIEW_LINES));
            // Reset scroll position when selection changes
            self.preview_scroll = None;
        }
    }

    /// Force refresh the preview (used on periodic refresh)
    pub fn refresh_preview(&mut self) {
        if !self.mux.supports_preview() {
            return;
        }
        self.preview = self
            .preview_pane_id
            .as_ref()
            .and_then(|pane_id| self.mux.capture_pane(pane_id, PREVIEW_LINES));
    }

    /// Parse pane_id to a number for proper ordering.
    /// Handles tmux format (%0, %10) and numeric formats (WezTerm, kitty).
    /// Uses u64 since kitty pane IDs can exceed u32 range.
    fn parse_pane_id(pane_id: &str) -> u64 {
        pane_id
            .strip_prefix('%')
            .unwrap_or(pane_id)
            .parse()
            .unwrap_or(u64::MAX)
    }

    /// Sort agents based on the current sort mode
    fn sort_agents(&mut self) {
        let stale_threshold = self.stale_threshold_secs;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Helper closure to get status priority (lower = higher priority)
        let get_priority = |agent: &AgentPane| -> u8 {
            let is_stale = agent
                .status_ts
                .map(|ts| now.saturating_sub(ts) > stale_threshold)
                .unwrap_or(false);

            if is_stale {
                return 3; // Stale: lowest priority
            }

            match agent.status {
                Some(AgentStatus::Waiting) => 0, // Waiting: needs input
                Some(AgentStatus::Done) => 1,    // Done: needs review
                Some(AgentStatus::Working) => 2, // Working: no action needed
                None => 3,                       // Unknown/other: lowest priority
            }
        };

        // Helper closure to get elapsed time (lower = more recent)
        let get_elapsed = |agent: &AgentPane| -> u64 {
            agent
                .status_ts
                .map(|ts| now.saturating_sub(ts))
                .unwrap_or(u64::MAX)
        };

        // Helper closure to get numeric pane_id for stable ordering
        let pane_num = |agent: &AgentPane| Self::parse_pane_id(&agent.pane_id);

        // Use sort_by_cached_key for better performance (calls key fn O(N) times vs O(N log N))
        // Include pane_id as final tiebreaker for stable ordering within groups
        match self.sort_mode {
            SortMode::Priority => {
                // Sort by priority, then by elapsed time (most recent first), then by pane_id
                self.agents
                    .sort_by_cached_key(|a| (get_priority(a), get_elapsed(a), pane_num(a)));
            }
            SortMode::Project => {
                // Sort by project name first, then by status priority within each project
                self.agents.sort_by_cached_key(|a| {
                    (Self::extract_project_name(a), get_priority(a), pane_num(a))
                });
            }
            SortMode::Recency => {
                self.agents
                    .sort_by_cached_key(|a| (get_elapsed(a), pane_num(a)));
            }
            SortMode::Natural => {
                self.agents.sort_by_cached_key(pane_num);
            }
        }
    }

    /// Cycle to the next sort mode, re-sort, and persist to tmux
    pub fn cycle_sort_mode(&mut self) {
        self.sort_mode = self.sort_mode.next();
        self.sort_mode.save();
        self.sort_agents();
    }

    /// Toggle hiding stale agents
    pub fn toggle_stale_filter(&mut self) {
        self.hide_stale = !self.hide_stale;
        save_hide_stale(self.hide_stale);
        self.refresh();
    }

    /// Increase preview size by 10% (max 90%)
    pub fn increase_preview_size(&mut self) {
        self.preview_size = (self.preview_size + 10).min(90);
        save_preview_size(self.preview_size);
    }

    /// Decrease preview size by 10% (min 10%)
    pub fn decrease_preview_size(&mut self) {
        self.preview_size = self.preview_size.saturating_sub(10).max(10);
        save_preview_size(self.preview_size);
    }

    pub fn next(&mut self) {
        if self.agents.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i >= self.agents.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
        self.selected_pane_id = self.agents.get(i).map(|a| a.pane_id.clone());
        self.update_preview();
    }

    pub fn previous(&mut self) {
        if self.agents.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.agents.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
        self.selected_pane_id = self.agents.get(i).map(|a| a.pane_id.clone());
        self.update_preview();
    }

    /// Switch to a pane and track the previous pane for toggle feature.
    /// This is the single source of truth for all pane switching.
    fn switch_to_pane_and_track(&mut self, target_pane_id: &str) {
        // Get the REAL current pane from the multiplexer (not UI state)
        let current_pane = self.mux.active_pane_id();

        // Attempt the switch first - only update state on success
        // Look up window_name for the target pane (needed by Zellij)
        let window_hint = self
            .agents
            .iter()
            .find(|a| a.pane_id == target_pane_id)
            .map(|a| a.window_name.as_str());
        if self
            .mux
            .switch_to_pane(target_pane_id, window_hint)
            .is_err()
        {
            return;
        }

        // Exit dashboard after jump (or keep open, depending on multiplexer)
        if self.mux.should_exit_on_jump() {
            self.should_jump = true;
        }

        // Only update last_pane_id if:
        // 1. We actually moved to a different pane
        // 2. The previous pane was an agent pane (not just any tmux pane)
        if let Some(ref current) = current_pane
            && current != target_pane_id
            && self.agents.iter().any(|a| a.pane_id == *current)
        {
            self.last_pane_id = Some(current.clone());
            save_last_pane_id(current);
        }
    }

    pub fn jump_to_selected(&mut self) {
        if let Some(selected) = self.table_state.selected()
            && let Some(agent) = self.agents.get(selected)
        {
            let target = agent.pane_id.clone();
            self.switch_to_pane_and_track(&target);
        }
    }

    pub fn jump_to_index(&mut self, index: usize) {
        if index < self.agents.len() {
            self.table_state.select(Some(index));
            self.selected_pane_id = self.agents.get(index).map(|a| a.pane_id.clone());
            self.jump_to_selected();
        }
    }

    /// Jump to the last visited agent (toggle behavior).
    /// Reloads from settings to pick up changes from CLI command.
    pub fn jump_to_last(&mut self) {
        // Reload from settings to handle CLI/dashboard interop
        self.last_pane_id = load_last_pane_id();

        let Some(ref last_id) = self.last_pane_id else {
            return;
        };
        let last_id = last_id.clone();

        // Update table selection if the pane exists in current list
        // (handles filtered/hidden agents gracefully - still switches even if not visible)
        if let Some(idx) = self.agents.iter().position(|a| a.pane_id == last_id) {
            self.table_state.select(Some(idx));
        }

        // Switch to the pane (works even if agent is filtered out of dashboard)
        self.switch_to_pane_and_track(&last_id);
    }

    pub fn peek_selected(&mut self) {
        // Switch to pane but keep popup open
        if let Some(selected) = self.table_state.selected()
            && let Some(agent) = self.agents.get(selected)
        {
            let _ = self
                .mux
                .switch_to_pane(&agent.pane_id, Some(&agent.window_name));
            // Don't set should_jump - popup stays open
        }
    }

    /// Send a key to the selected agent's pane
    pub fn send_key_to_selected(&self, key: &str) {
        if let Some(selected) = self.table_state.selected()
            && let Some(agent) = self.agents.get(selected)
        {
            let _ = self.mux.send_key(&agent.pane_id, key);
        }
    }

    /// Scroll preview up (toward older content). Returns the amount to scroll by.
    pub fn scroll_preview_up(&mut self, visible_height: u16, total_lines: u16) {
        let max_scroll = total_lines.saturating_sub(visible_height);
        let current = self.preview_scroll.unwrap_or(max_scroll);
        let half_page = visible_height / 2;
        self.preview_scroll = Some(current.saturating_sub(half_page));
    }

    /// Scroll preview down (toward newer content).
    pub fn scroll_preview_down(&mut self, visible_height: u16, total_lines: u16) {
        let max_scroll = total_lines.saturating_sub(visible_height);
        let current = self.preview_scroll.unwrap_or(max_scroll);
        let half_page = visible_height / 2;
        let new_scroll = (current + half_page).min(max_scroll);
        // If at or past max, return to auto-scroll mode
        if new_scroll >= max_scroll {
            self.preview_scroll = None;
        } else {
            self.preview_scroll = Some(new_scroll);
        }
    }

    pub fn format_duration(&self, secs: u64) -> String {
        agent::format_duration(secs)
    }

    pub fn is_stale(&self, agent: &AgentPane) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        agent::is_stale(agent.status_ts, self.stale_threshold_secs, now)
    }

    pub fn get_elapsed(&self, agent: &AgentPane) -> Option<u64> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        agent::elapsed_secs(agent.status_ts, now)
    }

    pub fn get_status_display(&self, agent: &AgentPane) -> Vec<(String, Style)> {
        let is_stale = self.is_stale(agent);

        // Map status enum to icon and color
        let (icon, base_color, is_working) = match agent.status {
            Some(AgentStatus::Working) => (self.config.status_icons.working(), Color::Cyan, true),
            Some(AgentStatus::Waiting) => {
                (self.config.status_icons.waiting(), Color::Magenta, false)
            }
            Some(AgentStatus::Done) => (self.config.status_icons.done(), Color::Green, false),
            None => ("", self.palette.text, false),
        };

        let base_style = Style::default().fg(base_color);
        let mut spans = super::ansi::parse_tmux_styles(icon, base_style);

        if is_stale {
            // Override all styling for stale agents
            let dimmed = Style::default().fg(self.palette.dimmed);
            for span in &mut spans {
                span.1 = dimmed;
            }
            spans.push((" \u{f051b}".to_string(), dimmed));
        } else if is_working {
            // Add animated spinner when agent is working
            let spinner = SPINNER_FRAMES[self.spinner_frame as usize];
            spans.push((format!(" {}", spinner), base_style));
        }

        spans
    }

    /// Extract the worktree name from an agent.
    /// Returns (worktree_name, is_main) where is_main indicates if this is the main worktree.
    pub fn extract_worktree_name(&self, agent_pane: &AgentPane) -> (String, bool) {
        agent::extract_worktree_name(
            &agent_pane.session,
            &agent_pane.window_name,
            self.config.window_prefix(),
        )
    }

    pub fn extract_project_name(agent_pane: &AgentPane) -> String {
        agent::extract_project_name(&agent_pane.path)
    }

    /// Get PR info for an agent by looking up its branch in PR statuses
    pub fn get_pr_for_agent(&self, agent: &AgentPane) -> Option<&PrSummary> {
        let repo_root = self.repo_roots.get(&agent.path)?;
        let git_status = self.git_statuses.get(&agent.path)?;
        let branch = git_status.branch.as_ref()?;
        // Don't show PRs for main/master - you merge INTO main, not FROM it
        if branch == "main" || branch == "master" {
            return None;
        }
        self.pr_statuses.get(repo_root)?.get(branch)
    }

    /// Whether a PR fetch is currently in progress
    pub fn is_pr_fetching(&self) -> bool {
        self.is_pr_fetching.load(Ordering::Relaxed)
    }

    /// Whether any agent has a matching PR (for column visibility)
    pub fn has_any_pr(&self) -> bool {
        self.agents
            .iter()
            .any(|agent| self.get_pr_for_agent(agent).is_some())
    }

    /// Get PR statuses for caching
    pub fn pr_statuses(&self) -> &HashMap<PathBuf, HashMap<String, PrSummary>> {
        &self.pr_statuses
    }
}
