//! Agent list management, navigation, sorting, filtering, and display helpers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::style::Style;

use crate::github::PrSummary;
use crate::multiplexer::{AgentPane, AgentStatus};

use super::DashboardTab;

use super::super::agent;
use super::super::ansi;
use super::super::settings::{load_last_pane_id, save_hide_stale, save_last_pane_id};
use super::super::sort::SortMode;
use super::super::spinner::SPINNER_FRAMES;
use super::App;
use crate::command::pane_history::pane_to_remember;

/// Selected agent pane target for multiplexer operations.
pub(crate) struct PaneTarget {
    pub pane_id: String,
    pub window_name: String,
}

impl App {
    /// Apply name and stale filters to the cached agent list, sort, and restore selection.
    /// This is fast (in-memory only) and safe to call on every filter keystroke.
    pub fn apply_filters(&mut self) {
        self.agents = self.all_agents.clone();

        // Apply name filter if active
        if !self.filter_text.is_empty() {
            let filter_lower = self.filter_text.to_lowercase();
            let window_prefix = self.config.window_prefix();
            self.agents.retain(|a| {
                let project = Self::extract_project_name(a).to_lowercase();
                let (worktree, _) = agent::extract_worktree_name(
                    &a.session,
                    &a.window_name,
                    window_prefix,
                    &a.path,
                );
                let worktree_lower = worktree.to_lowercase();
                project.contains(&filter_lower) || worktree_lower.contains(&filter_lower)
            });
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
                    .unwrap_or(true)
            });
        }

        self.sort_agents();

        // Restore selection by pane_id to follow the item across reorders
        if let Some(ref pane_id) = self.selected_pane_id {
            if let Some(new_idx) = self.agents.iter().position(|a| &a.pane_id == pane_id) {
                self.table_state.select(Some(new_idx));
            } else {
                self.selected_pane_id = None;
                if self.agents.is_empty() {
                    self.table_state.select(None);
                } else if let Some(selected) = self.table_state.selected() {
                    if selected >= self.agents.len() {
                        self.table_state.select(Some(self.agents.len() - 1));
                    }
                    if let Some(idx) = self.table_state.selected() {
                        self.selected_pane_id = self.agents.get(idx).map(|a| a.pane_id.clone());
                    }
                }
            }
        } else if let Some(selected) = self.table_state.selected() {
            if selected >= self.agents.len() {
                self.table_state.select(if self.agents.is_empty() {
                    None
                } else {
                    Some(self.agents.len() - 1)
                });
            }
            if let Some(idx) = self.table_state.selected() {
                self.selected_pane_id = self.agents.get(idx).map(|a| a.pane_id.clone());
            }
        }

        // Fallback: if nothing is selected but agents exist, select the first one.
        // This handles the case where filtering produced zero matches (clearing selection)
        // and then results reappear.
        if self.selected_pane_id.is_none()
            && self.table_state.selected().is_none()
            && !self.agents.is_empty()
        {
            self.table_state.select(Some(0));
            self.selected_pane_id = self.agents.first().map(|a| a.pane_id.clone());
        }

        self.update_preview();
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
                return 4; // Stale: lowest priority
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

    pub fn cycle_sort_mode(&mut self) {
        self.sort_mode = self.sort_mode.next();
        self.sort_mode.save();
        self.sort_agents();
    }

    /// Toggle between showing all agents or only the current session's agents
    pub fn toggle_scope_mode(&mut self) {
        self.scope_mode = self.scope_mode.toggle();
        self.scope_mode.save();
        self.refresh();
    }

    /// Toggle hiding stale agents
    pub fn toggle_stale_filter(&mut self) {
        self.hide_stale = !self.hide_stale;
        save_hide_stale(self.hide_stale);
        self.refresh();
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
    pub(crate) fn switch_to_pane_and_track(&mut self, target_pane_id: &str) {
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

        if let Some(current) = pane_to_remember(current_pane.as_deref(), target_pane_id) {
            self.last_pane_id = Some(current.to_string());
            save_last_pane_id(current);
        }
    }

    pub fn jump_to_selected(&mut self) {
        if let Some(target) = self.selected_agent_target() {
            self.switch_to_pane_and_track(&target.pane_id);
        }
    }

    pub(crate) fn selected_agent_target(&self) -> Option<PaneTarget> {
        let selected = self.table_state.selected()?;
        let agent = self.agents.get(selected)?;
        Some(PaneTarget {
            pane_id: agent.pane_id.clone(),
            window_name: agent.window_name.clone(),
        })
    }

    pub(crate) fn send_keys_to_selected_agent(&mut self, keys: &str) {
        let Some(target) = self.selected_agent_target() else {
            return;
        };

        if self.mux.requires_focus_for_input() {
            let _ = self
                .mux
                .switch_to_pane(&target.pane_id, Some(&target.window_name));
        }

        let _ = self
            .mux
            .send_keys_to_agent(&target.pane_id, keys, self.config.agent.as_deref());
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

    /// Kill the selected agent's pane and remove it from the list.
    /// Shows a confirmation popup for working agents.
    pub fn kill_selected(&mut self) {
        if let Some(selected) = self.table_state.selected()
            && let Some(agent) = self.agents.get(selected)
        {
            if agent.status == Some(AgentStatus::Working) {
                // Show confirmation popup
                self.pending_kill_pane_id = Some(agent.pane_id.clone());
            } else {
                self.do_kill(&agent.pane_id.clone());
            }
        }
    }

    /// Execute the pending kill confirmation.
    pub fn confirm_kill(&mut self) {
        if let Some(pane_id) = self.pending_kill_pane_id.take() {
            self.do_kill(&pane_id);
        }
    }

    /// Kill a pane and remove it from the agent list.
    fn do_kill(&mut self, pane_id: &str) {
        let _ = self.mux.kill_pane(pane_id);

        let selected = self.table_state.selected().unwrap_or(0);

        // Remove from local lists immediately for responsive UI
        self.agents.retain(|a| a.pane_id != pane_id);
        self.all_agents.retain(|a| a.pane_id != pane_id);

        // Adjust selection
        if self.agents.is_empty() {
            self.table_state.select(None);
            self.selected_pane_id = None;
        } else {
            let new_idx = selected.min(self.agents.len() - 1);
            self.table_state.select(Some(new_idx));
            self.selected_pane_id = self.agents.get(new_idx).map(|a| a.pane_id.clone());
        }

        // Force preview refresh for new selection
        self.preview_pane_id = None;
        self.update_preview();
    }

    /// Send a key to the selected agent's pane
    pub fn send_key_to_selected(&self, key: &str) {
        if let Some(selected) = self.table_state.selected()
            && let Some(agent) = self.agents.get(selected)
        {
            let _ = self.mux.send_key(&agent.pane_id, key);
        }
    }

    /// Paste text to the selected agent's pane.
    pub fn paste_text_to_selected(&self, text: &str) {
        if let Some(selected) = self.table_state.selected()
            && let Some(agent) = self.agents.get(selected)
        {
            let _ = self.mux.paste_text(&agent.pane_id, text);
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
            Some(AgentStatus::Working) => {
                (self.config.status_icons.working(), self.palette.info, true)
            }
            Some(AgentStatus::Waiting) => (
                self.config.status_icons.waiting(),
                self.palette.accent,
                false,
            ),
            Some(AgentStatus::Done) => {
                (self.config.status_icons.done(), self.palette.success, false)
            }
            None => ("", self.palette.text, false),
        };

        let base_style = Style::default().fg(base_color);
        let mut spans = ansi::parse_tmux_styles(icon, base_style);

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
            &agent_pane.path,
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

    /// Open the PR associated with the selected agent or worktree in the browser.
    pub fn open_pr_for_selected(&mut self) {
        self.open_pr_url(|url| url.to_string());
    }

    /// Open the PR checks page for the selected agent or worktree in the browser.
    pub fn open_pr_checks_for_selected(&mut self) {
        self.open_pr_url(|url| format!("{url}/checks"));
    }

    /// Open a URL derived from the selected item's PR URL.
    fn open_pr_url(&mut self, make_url: impl FnOnce(&str) -> String) {
        let pr = match self.active_tab {
            DashboardTab::Agents => self
                .table_state
                .selected()
                .and_then(|i| self.agents.get(i))
                .and_then(|agent| self.get_pr_for_agent(agent))
                .cloned(),
            DashboardTab::Worktrees => self
                .worktree_table_state
                .selected()
                .and_then(|i| self.worktrees.get(i))
                .and_then(|wt| wt.pr_info.clone()),
        };

        match pr {
            Some(ref pr) if pr.url.is_some() => {
                let url = make_url(pr.url.as_ref().unwrap());

                #[cfg(target_os = "macos")]
                let cmd = "open";
                #[cfg(not(target_os = "macos"))]
                let cmd = "xdg-open";

                if let Err(e) = std::process::Command::new(cmd).arg(&url).spawn() {
                    self.status_message = Some((
                        format!("Failed to open browser: {e}"),
                        std::time::Instant::now(),
                    ));
                }
            }
            Some(_) => {
                self.status_message = Some((
                    "PR URL not available yet".to_string(),
                    std::time::Instant::now(),
                ));
            }
            None => {
                self.status_message = Some((
                    "No PR found for selected item".to_string(),
                    std::time::Instant::now(),
                ));
            }
        }
    }
}
