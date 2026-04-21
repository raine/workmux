//! Worktree tab: navigation, removal, sweep, project picker, and preview.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::git;
use crate::workflow;

use super::super::agent;
use super::super::sort::WorktreeSortMode;
use super::App;
use super::types::*;

/// Delete the last word from a string (Emacs Ctrl+w behavior).
fn delete_word_backward(s: &mut String) {
    // Trim trailing whitespace first
    let trimmed_len = s.trim_end().len();
    s.truncate(trimmed_len);
    // Then delete back to the previous word boundary
    if let Some(pos) = s.rfind(|c: char| c == '/' || c == '-' || c.is_whitespace()) {
        s.truncate(pos);
    } else {
        s.clear();
    }
}

impl App {
    /// Reset the worktree fetch timer to trigger an immediate refetch
    pub fn trigger_worktree_refetch(&mut self) {
        self.last_worktree_fetch = std::time::Instant::now() - Duration::from_secs(60);
    }

    /// Switch between Agents and Worktrees tabs
    pub fn switch_tab(&mut self) {
        self.active_tab = match self.active_tab {
            DashboardTab::Agents => DashboardTab::Worktrees,
            DashboardTab::Worktrees => DashboardTab::Agents,
        };
        if self.active_tab == DashboardTab::Worktrees {
            // Trigger immediate fetch on switch
            self.last_worktree_fetch = std::time::Instant::now();
            self.spawn_worktree_fetch();
        }
    }

    /// Spawn background thread to fetch worktree list
    pub(super) fn spawn_worktree_fetch(&self) {
        if self
            .is_worktree_fetching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let tx = self.event_tx.clone();
        let is_fetching = self.is_worktree_fetching.clone();
        let config = self.config.clone();
        let mux = self.mux.clone();
        let repo_override = self
            .worktree_project_override
            .as_ref()
            .map(|(_, p)| p.clone());

        std::thread::spawn(move || {
            struct ResetFlag(Arc<AtomicBool>);
            impl Drop for ResetFlag {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _reset = ResetFlag(is_fetching);

            // fetch_pr_status=false: the dashboard fetches PR status separately,
            // and workflow::list's spinner would corrupt the TUI output
            if let Ok(worktrees) =
                workflow::list_in(&config, mux.as_ref(), false, &[], repo_override.as_deref())
            {
                let _ = tx.send(AppEvent::WorktreeList(worktrees));
            }
        });
    }

    /// Cycle to the next worktree sort mode.
    pub fn cycle_worktree_sort_mode(&mut self) {
        self.worktree_sort_mode = self.worktree_sort_mode.next();
        self.worktree_sort_mode.save();
        self.apply_worktree_filters();
    }

    /// Sort worktrees according to the current sort mode.
    fn sort_worktrees(&mut self) {
        match self.worktree_sort_mode {
            WorktreeSortMode::Natural => {} // Keep original order from git
            WorktreeSortMode::Age => {
                self.worktrees
                    .sort_by(|a, b| b.created_at.cmp(&a.created_at));
            }
        }
    }

    /// Apply filter text to worktree list and restore selection
    pub(super) fn apply_worktree_filters(&mut self) {
        // Reset from baseline
        self.worktrees = self.all_worktrees.clone();

        // Merge PR data from dashboard's own PR fetching into worktrees
        // (workflow::list is called with fetch_pr_status=false to avoid spinner)
        if !self.pr_statuses.is_empty() {
            for wt in &mut self.worktrees {
                if wt.pr_info.is_some() || wt.is_main {
                    continue;
                }
                // Search all repo roots for a matching branch
                for prs in self.pr_statuses.values() {
                    if let Some(pr) = prs.get(&wt.branch) {
                        wt.pr_info = Some(pr.clone());
                        break;
                    }
                }
            }
        }

        // Apply name filter
        if !self.worktree_filter_text.is_empty() {
            let filter = self.worktree_filter_text.to_lowercase();
            self.worktrees.retain(|w| {
                let handle = w.handle.to_lowercase();
                handle.contains(&filter) || w.branch.to_lowercase().contains(&filter)
            });
        }

        // Sort after filtering
        self.sort_worktrees();

        // Restore selection by path
        if let Some(ref path) = self.selected_worktree_path {
            if let Some(idx) = self.worktrees.iter().position(|w| &w.path == path) {
                self.worktree_table_state.select(Some(idx));
            } else {
                self.selected_worktree_path = None;
                if self.worktrees.is_empty() {
                    self.worktree_table_state.select(None);
                } else {
                    self.worktree_table_state.select(Some(0));
                }
            }
        } else if !self.worktrees.is_empty() && self.worktree_table_state.selected().is_none() {
            self.worktree_table_state.select(Some(0));
            self.selected_worktree_path = self.worktrees.first().map(|w| w.path.clone());
        }

        self.update_worktree_preview();
    }

    pub fn worktree_next(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        let i = self.worktree_table_state.selected().unwrap_or(0);
        let next = if i >= self.worktrees.len() - 1 {
            0
        } else {
            i + 1
        };
        self.worktree_table_state.select(Some(next));
        self.selected_worktree_path = self.worktrees.get(next).map(|w| w.path.clone());
        self.update_worktree_preview();
    }

    pub fn worktree_previous(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        let i = self.worktree_table_state.selected().unwrap_or(0);
        let prev = if i == 0 {
            self.worktrees.len() - 1
        } else {
            i - 1
        };
        self.worktree_table_state.select(Some(prev));
        self.selected_worktree_path = self.worktrees.get(prev).map(|w| w.path.clone());
        self.update_worktree_preview();
    }

    pub fn worktree_jump_to_index(&mut self, index: usize) {
        if index < self.worktrees.len() {
            self.worktree_table_state.select(Some(index));
            self.selected_worktree_path = self.worktrees.get(index).map(|w| w.path.clone());
            self.jump_to_selected_worktree();
        }
    }

    /// Show the remove confirmation modal for the selected worktree.
    /// Always shows the modal (even for clean worktrees). Skips main worktree.
    /// Works from both the worktrees tab (uses selected worktree) and agents tab
    /// (finds the worktree matching the selected agent's path).
    pub fn remove_selected_worktree(&mut self) {
        let worktree = match self.active_tab {
            DashboardTab::Worktrees => {
                let Some(selected) = self.worktree_table_state.selected() else {
                    return;
                };
                self.worktrees.get(selected).cloned()
            }
            DashboardTab::Agents => {
                let Some(selected) = self.table_state.selected() else {
                    return;
                };
                let Some(agent) = self.agents.get(selected) else {
                    return;
                };
                let agent_path = &agent.path;
                self.worktrees
                    .iter()
                    .find(|w| w.path == *agent_path)
                    .cloned()
            }
        };

        let Some(worktree) = worktree else {
            return;
        };

        // Block removal of main worktree
        if worktree.is_main {
            return;
        }

        let is_dirty = git::has_uncommitted_changes(&worktree.path).unwrap_or(false);

        self.pending_remove = Some(RemovePlan {
            handle: worktree.handle.clone(),
            path: worktree.path.clone(),
            is_dirty,
            is_unmerged: worktree.has_unmerged,
            keep_branch: false,
            force_armed: false,
        });
    }

    /// Toggle keep-branch in the pending remove plan.
    pub fn toggle_remove_keep_branch(&mut self) {
        if let Some(ref mut plan) = self.pending_remove {
            plan.keep_branch = !plan.keep_branch;
        }
    }

    /// Arm force mode for dirty worktree removal.
    pub fn arm_remove_force(&mut self) {
        if let Some(ref mut plan) = self.pending_remove
            && plan.is_dirty
        {
            plan.force_armed = true;
        }
    }

    /// Execute the pending remove confirmation.
    pub fn confirm_remove(&mut self) {
        let Some(plan) = self.pending_remove.take() else {
            return;
        };

        // Dirty worktrees require force to be armed
        if plan.is_dirty && !plan.force_armed {
            self.pending_remove = Some(plan);
            return;
        }

        self.do_remove_worktree(&plan.path, plan.keep_branch);
    }

    fn do_remove_worktree(&mut self, path: &Path, keep_branch: bool) {
        let handle = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        let Ok(ctx) = workflow::WorkflowContext::new(self.config.clone(), self.mux.clone(), None)
        else {
            return;
        };

        // force=true because user confirmed via modal
        if workflow::remove(&handle, true, keep_branch, &ctx).is_ok() {
            self.worktrees.retain(|w| w.path != *path);

            if self.worktrees.is_empty() {
                self.worktree_table_state.select(None);
                self.selected_worktree_path = None;
            } else {
                let idx = self.worktree_table_state.selected().unwrap_or(0);
                let new_idx = idx.min(self.worktrees.len() - 1);
                self.worktree_table_state.select(Some(new_idx));
                self.selected_worktree_path = self.worktrees.get(new_idx).map(|w| w.path.clone());
            }
        }
    }

    /// Close the mux window/session for the selected worktree without removing it.
    pub fn close_selected_worktree_window(&mut self) {
        let Some(selected) = self.worktree_table_state.selected() else {
            return;
        };
        let Some(worktree) = self.worktrees.get(selected) else {
            return;
        };

        if worktree.is_main || !worktree.has_mux_window {
            return;
        }

        let prefix = self.config.window_prefix();
        let full_name = crate::multiplexer::util::prefixed(prefix, &worktree.handle);
        let _ = crate::multiplexer::handle::MuxHandle::kill_full(
            self.mux.as_ref(),
            worktree.mode,
            &full_name,
        );
        self.trigger_worktree_refetch();
    }

    /// Build the sweep candidate list and open the sweep modal.
    /// If worktree data hasn't been loaded yet, triggers a background fetch
    /// and opens an empty sweep modal (data will arrive on next refresh).
    pub fn start_sweep(&mut self) {
        // Ensure worktree data is loaded (may not be if called from agents view)
        if self.worktrees.is_empty() {
            self.spawn_worktree_fetch();
        }

        let gone = git::get_gone_branches().unwrap_or_default();

        let mut candidates: Vec<SweepCandidate> = Vec::new();

        for wt in &self.worktrees {
            if wt.is_main {
                continue;
            }

            let status = self.git_statuses.get(&wt.path);
            let is_dirty = status.is_some_and(|s| s.is_dirty);
            let has_upstream = status.is_some_and(|s| s.has_upstream);

            // Determine reason: PR merged > PR closed > upstream gone > merged locally
            let reason = if let Some(ref pr) = wt.pr_info {
                match pr.state.as_str() {
                    "MERGED" => Some(SweepReason::PrMerged),
                    "CLOSED" => Some(SweepReason::PrClosed),
                    _ => {
                        if gone.contains(&wt.branch) {
                            Some(SweepReason::UpstreamGone)
                        } else {
                            None
                        }
                    }
                }
            } else if gone.contains(&wt.branch) {
                Some(SweepReason::UpstreamGone)
            } else if !has_upstream && !wt.has_unmerged {
                Some(SweepReason::MergedLocally)
            } else {
                None
            };

            let Some(reason) = reason else { continue };

            candidates.push(SweepCandidate {
                handle: wt.handle.clone(),
                path: wt.path.clone(),
                reason,
                is_dirty,
                selected: !is_dirty, // Pre-select non-dirty candidates
            });
        }

        self.pending_sweep = Some(SweepState {
            candidates,
            cursor: 0,
        });
    }

    /// Toggle selection of the current sweep candidate.
    pub fn sweep_toggle(&mut self) {
        if let Some(ref mut sweep) = self.pending_sweep
            && let Some(candidate) = sweep.candidates.get_mut(sweep.cursor)
            && !candidate.is_dirty
        {
            candidate.selected = !candidate.selected;
        }
    }

    /// Move cursor up in sweep modal.
    pub fn sweep_up(&mut self) {
        if let Some(ref mut sweep) = self.pending_sweep {
            sweep.cursor = sweep.cursor.saturating_sub(1);
        }
    }

    /// Move cursor down in sweep modal.
    pub fn sweep_down(&mut self) {
        if let Some(ref mut sweep) = self.pending_sweep
            && sweep.cursor + 1 < sweep.candidates.len()
        {
            sweep.cursor += 1;
        }
    }

    /// Execute sweep: remove all selected candidates.
    pub fn confirm_sweep(&mut self) {
        let Some(sweep) = self.pending_sweep.take() else {
            return;
        };

        let paths_to_remove: Vec<PathBuf> = sweep
            .candidates
            .iter()
            .filter(|c| c.selected)
            .map(|c| c.path.clone())
            .collect();

        for path in &paths_to_remove {
            self.do_remove_worktree(path, false);
        }
    }

    // ── Project picker methods ─────────────────────────────────────

    /// Discover projects from cached repo roots and open the picker modal.
    pub fn show_project_picker(&mut self) {
        // Deduplicate by project name, keeping one representative path per project
        let mut by_name: std::collections::BTreeMap<String, PathBuf> =
            std::collections::BTreeMap::new();

        for root in self.repo_roots.values() {
            let name = agent::extract_project_name(root);
            by_name.entry(name).or_insert_with(|| root.clone());
        }

        let projects: Vec<ProjectEntry> = by_name
            .into_iter()
            .map(|(name, path)| ProjectEntry { name, path })
            .collect();

        let current_name = self
            .worktree_project_override
            .as_ref()
            .map(|(name, _)| name.clone())
            .or_else(|| {
                self.current_worktree
                    .as_deref()
                    .map(agent::extract_project_name)
            });

        let initial_cursor = current_name
            .as_ref()
            .and_then(|name| projects.iter().position(|p| &p.name == name))
            .unwrap_or(0);

        self.pending_project_picker = Some(ProjectPicker {
            projects,
            cursor: initial_cursor,
            filter: String::new(),
            current_name,
        });
    }

    /// Move cursor down in project picker.
    pub fn project_picker_down(&mut self) {
        if let Some(ref mut picker) = self.pending_project_picker {
            let filtered = picker.filtered();
            if !filtered.is_empty() && picker.cursor + 1 < filtered.len() {
                picker.cursor += 1;
            }
        }
    }

    /// Move cursor up in project picker.
    pub fn project_picker_up(&mut self) {
        if let Some(ref mut picker) = self.pending_project_picker {
            picker.cursor = picker.cursor.saturating_sub(1);
        }
    }

    /// Append a character to the project picker filter.
    pub fn project_picker_filter_append(&mut self, c: char) {
        if let Some(ref mut picker) = self.pending_project_picker {
            picker.filter.push(c);
            picker.cursor = 0;
        }
    }

    /// Delete the last character from the project picker filter.
    pub fn project_picker_filter_delete(&mut self) {
        if let Some(ref mut picker) = self.pending_project_picker {
            picker.filter.pop();
            picker.cursor = 0;
        }
    }

    /// Confirm project picker selection: set override and trigger refetch.
    pub fn confirm_project_picker(&mut self) {
        let Some(picker) = self.pending_project_picker.take() else {
            return;
        };
        let filtered = picker.filtered();
        let Some(&idx) = filtered.get(picker.cursor) else {
            return;
        };
        let selected = &picker.projects[idx];

        self.worktree_project_override = Some((selected.name.clone(), selected.path.clone()));
        self.worktrees.clear();
        self.all_worktrees.clear();
        self.last_worktree_fetch = std::time::Instant::now();
        self.spawn_worktree_fetch();

        // Switch to worktrees tab to show the result
        if self.active_tab != DashboardTab::Worktrees {
            self.active_tab = DashboardTab::Worktrees;
        }
    }

    // ── Base branch picker methods ──────────────────────────────────

    /// Open the base branch picker for the selected worktree (works from both tabs).
    pub fn show_base_branch_picker(&mut self) {
        // Resolve repo path, branch, and current base from whichever tab is active
        let (repo_path, worktree_branch, current_base) = match self.active_tab {
            DashboardTab::Worktrees => {
                let Some(selected) = self.worktree_table_state.selected() else {
                    return;
                };
                let Some(worktree) = self.worktrees.get(selected) else {
                    return;
                };
                if worktree.is_main || worktree.branch == "(detached)" {
                    return;
                }
                (
                    worktree.path.clone(),
                    worktree.branch.clone(),
                    worktree.base_branch.clone(),
                )
            }
            DashboardTab::Agents => {
                let Some(selected) = self.table_state.selected() else {
                    return;
                };
                let Some(agent) = self.agents.get(selected) else {
                    return;
                };
                let path = agent.path.clone();
                let branch = match git::get_current_branch_in(&path) {
                    Ok(b) if !b.is_empty() && b != "(detached)" => b,
                    _ => return,
                };
                let base = git::get_branch_base_in(&branch, Some(&path)).ok();
                (path, branch, base)
            }
        };

        // List local branches, excluding the worktree's own branch
        let branches = match git::list_local_branches_in(Some(&repo_path)) {
            Ok(b) => b,
            Err(_) => {
                self.status_message = Some((
                    "Failed to list branches".to_string(),
                    std::time::Instant::now(),
                ));
                return;
            }
        };
        let mut branches: Vec<_> = branches
            .into_iter()
            .filter(|b| *b != worktree_branch)
            .collect();

        // Pin current base to the top if it exists in the list
        if let Some(ref base) = current_base
            && let Some(pos) = branches.iter().position(|b| b == base)
        {
            let pinned = branches.remove(pos);
            branches.insert(0, pinned);
        }

        let initial_cursor = 0;

        self.pending_base_picker = Some(BaseBranchPicker {
            branches,
            cursor: initial_cursor,
            filter: String::new(),
            current_base,
            worktree_branch,
            repo_path,
        });
    }

    /// Move cursor down in base branch picker.
    pub fn base_picker_down(&mut self) {
        if let Some(ref mut picker) = self.pending_base_picker {
            let filtered = picker.filtered();
            if !filtered.is_empty() && picker.cursor + 1 < filtered.len() {
                picker.cursor += 1;
            }
        }
    }

    /// Move cursor up in base branch picker.
    pub fn base_picker_up(&mut self) {
        if let Some(ref mut picker) = self.pending_base_picker {
            picker.cursor = picker.cursor.saturating_sub(1);
        }
    }

    /// Append a character to the base branch picker filter.
    pub fn base_picker_filter_append(&mut self, c: char) {
        if let Some(ref mut picker) = self.pending_base_picker {
            picker.filter.push(c);
            picker.cursor = 0;
        }
    }

    /// Delete the last character from the base branch picker filter.
    pub fn base_picker_filter_delete(&mut self) {
        if let Some(ref mut picker) = self.pending_base_picker {
            picker.filter.pop();
            picker.cursor = 0;
        }
    }

    /// Confirm base branch picker selection: set the base and trigger refetch.
    pub fn confirm_base_picker(&mut self) {
        let Some(picker) = self.pending_base_picker.take() else {
            return;
        };
        let filtered = picker.filtered();
        let Some(&idx) = filtered.get(picker.cursor) else {
            return;
        };
        let new_base = &picker.branches[idx];

        if let Err(e) =
            git::set_branch_base_in(&picker.worktree_branch, new_base, Some(&picker.repo_path))
        {
            self.status_message = Some((
                format!("Failed to set base: {}", e),
                std::time::Instant::now(),
            ));
            return;
        }

        // Update in-memory state immediately so the UI reflects the change
        let new_base_owned = new_base.to_string();
        for wt in self
            .all_worktrees
            .iter_mut()
            .chain(self.worktrees.iter_mut())
        {
            if wt.branch == picker.worktree_branch {
                wt.base_branch = Some(new_base_owned.clone());
            }
        }
        // Also update the cached GitStatus so the Git column and detail panel refresh
        if let Some(status) = self.git_statuses.get_mut(&picker.repo_path) {
            status.base_branch = new_base_owned.clone();
        }

        self.status_message = Some((
            format!(
                "Base for '{}' set to '{}'",
                picker.worktree_branch, new_base
            ),
            std::time::Instant::now(),
        ));

        self.trigger_worktree_refetch();
    }

    /// Open a tmux window/session for the selected worktree via workflow::open,
    /// then close the dashboard.
    pub fn open_selected_worktree(&mut self) {
        let Some(selected) = self.worktree_table_state.selected() else {
            return;
        };
        let Some(worktree) = self.worktrees.get(selected) else {
            return;
        };

        let handle = worktree.handle.clone();

        let Ok(ctx) = workflow::WorkflowContext::new(self.config.clone(), self.mux.clone(), None)
        else {
            return;
        };

        let mut options = workflow::types::SetupOptions::new(false, false, true);
        options.mode = self.config.mode();
        if workflow::open(&handle, &ctx, options, false, false, None).is_ok() {
            self.should_jump = true;
        }
    }

    /// Jump to the selected worktree's agent or mux window.
    /// Tries the agent pane first, then falls back to workflow::open
    /// which switches to an existing window/session or creates one.
    pub fn jump_to_selected_worktree(&mut self) {
        let Some(selected) = self.worktree_table_state.selected() else {
            return;
        };
        let Some(worktree) = self.worktrees.get(selected) else {
            return;
        };

        // Try agent pane first for direct pane targeting
        if let Some(agent) = self.all_agents.iter().find(|a| a.path == worktree.path) {
            let target = agent.pane_id.clone();
            self.switch_to_pane_and_track(&target);
            return;
        }

        // Fall back to workflow::open (switches to existing or creates new)
        self.open_selected_worktree();
    }

    // ── Add worktree methods ───────────────────────────────────────

    /// Get the repo path for the current worktree view context.
    fn worktree_repo_path(&self) -> Option<PathBuf> {
        self.worktree_project_override
            .as_ref()
            .map(|(_, p)| p.clone())
            .or_else(|| self.worktrees.first().map(|w| w.path.clone()))
            .or_else(|| self.current_worktree.clone())
    }

    /// Open the add-worktree modal with unified picker (branches pre-fetched).
    pub fn show_add_worktree(&mut self) {
        let Some(repo_path) = self.worktree_repo_path() else {
            self.status_message = Some((
                "No project context available".to_string(),
                std::time::Instant::now(),
            ));
            return;
        };

        let branches = match git::list_local_branches_in(Some(&repo_path)) {
            Ok(b) => b,
            Err(_) => {
                self.status_message = Some((
                    "Failed to list branches".to_string(),
                    std::time::Instant::now(),
                ));
                return;
            }
        };

        let default_branch =
            git::get_default_branch_in(Some(&repo_path)).unwrap_or_else(|_| "main".to_string());

        // Collect branches that already have worktrees
        let occupied_branches: std::collections::HashSet<String> =
            self.worktrees.iter().map(|w| w.branch.clone()).collect();

        self.pending_add_worktree = Some(AddWorktreeState {
            branches,
            occupied_branches,
            cursor: 0,
            filter: String::new(),
            tab_prefix: None,
            base_branch: default_branch,
            editing_base: false,
            base_filter: String::new(),
            base_tab_prefix: None,
            repo_path,
            mode: AddWorktreeMode::Branch,
            pr_list: None,
            pr_request_counter: 0,
        });
    }

    /// Append a character to the add-worktree filter/name input.
    pub fn add_worktree_append(&mut self, c: char) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.filter.push(c);
            state.tab_prefix = None;
            state.cursor = 0;
        }
    }

    /// Delete the last character from the add-worktree filter/name input.
    pub fn add_worktree_delete(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.filter.pop();
            state.tab_prefix = None;
            state.cursor = 0;
        }
    }

    /// Delete the last word from the add-worktree filter (Ctrl+w).
    pub fn add_worktree_delete_word(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            delete_word_backward(&mut state.filter);
            state.tab_prefix = None;
            state.cursor = 0;
        }
    }

    /// Clear the add-worktree filter (Ctrl+u).
    pub fn add_worktree_clear(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.filter.clear();
            state.tab_prefix = None;
            state.cursor = 0;
        }
    }

    /// Move cursor down in the add-worktree picker.
    pub fn add_worktree_down(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            let max_idx = match state.mode {
                AddWorktreeMode::Branch => {
                    let has_create_row = !state.filter.trim().is_empty();
                    if has_create_row {
                        state.selectable_count()
                    } else {
                        state.selectable_count().saturating_sub(1)
                    }
                }
                AddWorktreeMode::Pr => state.filtered_prs().len().saturating_sub(1),
            };
            if state.cursor < max_idx {
                state.cursor += 1;
            }
        }
    }

    /// Move cursor up in the add-worktree picker.
    pub fn add_worktree_up(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.cursor = state.cursor.saturating_sub(1);
        }
    }

    /// Tab-complete: cycle through matching branch names.
    /// First press saves the typed prefix and fills the first match.
    /// Subsequent presses cycle to the next match, wrapping around.
    pub fn add_worktree_tab_complete(&mut self) {
        let Some(ref mut state) = self.pending_add_worktree else {
            return;
        };
        if state.mode == AddWorktreeMode::Pr {
            return;
        }

        // Save the original typed text on first Tab press
        if state.tab_prefix.is_none() {
            state.tab_prefix = Some(state.filter.clone());
        }

        let prefix = state.tab_prefix.as_deref().unwrap_or(&state.filter);
        let lower = prefix.to_lowercase();

        // Tab uses the same fuzzy matching as the filter list
        let candidates: Vec<usize> = state
            .branches
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                crate::command::dashboard::app::types::fuzzy_match(&lower, &b.to_lowercase())
                    && !state.occupied_branches.contains(*b)
            })
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return;
        }

        // Find current position among candidates
        let current_pos = candidates
            .iter()
            .position(|&idx| state.branches[idx] == state.filter);

        let next = match current_pos {
            Some(pos) => (pos + 1) % candidates.len(),
            None => 0,
        };

        state.filter = state.branches[candidates[next]].clone();
        state.cursor = next + 1; // +1 because cursor 0 is "Create" row
    }

    /// Toggle between Branch and PR modes (Ctrl+p).
    pub fn add_worktree_toggle_pr_mode(&mut self) {
        let Some(ref mut state) = self.pending_add_worktree else {
            return;
        };

        match state.mode {
            AddWorktreeMode::Branch => {
                state.mode = AddWorktreeMode::Pr;
                state.cursor = 0;
                state.editing_base = false;
                // Start async fetch if not already loaded
                if state.pr_list.is_none() {
                    state.pr_request_counter += 1;
                    let request_id = state.pr_request_counter;
                    state.pr_list = Some(PrListState::Loading);
                    let tx = self.event_tx.clone();
                    let repo_path = state.repo_path.clone();
                    std::thread::spawn(move || {
                        let result = crate::github::list_open_prs(&repo_path);
                        match result {
                            Ok(prs) => {
                                let _ = tx.send(AppEvent::AddWorktreePrList(request_id, Ok(prs)));
                            }
                            Err(e) => {
                                let _ = tx.send(AppEvent::AddWorktreePrList(
                                    request_id,
                                    Err(e.to_string()),
                                ));
                            }
                        }
                    });
                }
            }
            AddWorktreeMode::Pr => {
                state.mode = AddWorktreeMode::Branch;
                state.cursor = 0;
            }
        }
    }

    /// Toggle base branch editing mode (Ctrl+b).
    pub fn add_worktree_toggle_base(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            // No base editing in PR mode
            if state.mode == AddWorktreeMode::Pr {
                return;
            }
            if state.editing_base {
                // Accept current base_filter as the base branch if non-empty
                let text = state.base_filter.trim().to_string();
                if !text.is_empty() {
                    state.base_branch = text;
                }
                state.editing_base = false;
                state.base_filter.clear();
                state.base_tab_prefix = None;
            } else {
                state.editing_base = true;
                state.base_filter = state.base_branch.clone();
                state.base_tab_prefix = None;
            }
        }
    }

    /// Tab-complete for the base branch field.
    pub fn add_worktree_base_tab_complete(&mut self) {
        let Some(ref mut state) = self.pending_add_worktree else {
            return;
        };

        if state.base_tab_prefix.is_none() {
            state.base_tab_prefix = Some(state.base_filter.clone());
        }

        let prefix = state
            .base_tab_prefix
            .as_deref()
            .unwrap_or(&state.base_filter);
        let lower = prefix.to_lowercase();
        let candidates: Vec<usize> = state
            .branches
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                crate::command::dashboard::app::types::fuzzy_match(&lower, &b.to_lowercase())
            })
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return;
        }

        let current_pos = candidates
            .iter()
            .position(|&idx| state.branches[idx] == state.base_filter);

        let next = match current_pos {
            Some(pos) => (pos + 1) % candidates.len(),
            None => 0,
        };

        state.base_filter = state.branches[candidates[next]].clone();
    }

    /// Append a character to the base branch filter.
    pub fn add_worktree_base_append(&mut self, c: char) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.base_filter.push(c);
            state.base_tab_prefix = None;
        }
    }

    /// Delete last character from the base branch filter.
    pub fn add_worktree_base_delete(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.base_filter.pop();
            state.base_tab_prefix = None;
        }
    }

    /// Delete last word from the base branch filter (Ctrl+w).
    pub fn add_worktree_base_delete_word(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            delete_word_backward(&mut state.base_filter);
            state.base_tab_prefix = None;
        }
    }

    /// Clear the base branch filter (Ctrl+u).
    pub fn add_worktree_base_clear(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.base_filter.clear();
            state.base_tab_prefix = None;
        }
    }

    /// Handle Enter - create the worktree or checkout a PR.
    pub fn add_worktree_confirm_selection(&mut self) {
        let Some(ref mut state) = self.pending_add_worktree else {
            return;
        };

        // If editing base, confirm the base field first
        if state.editing_base {
            self.add_worktree_toggle_base();
            return;
        }

        match state.mode {
            AddWorktreeMode::Branch => {
                if state.cursor == 0 {
                    // Check for PR number detection (e.g. "#123" or "123")
                    if let Some(pr_number) = state.detected_pr_number() {
                        let repo_path = state.repo_path.clone();
                        self.pending_add_worktree = None;
                        self.do_checkout_pr(pr_number, repo_path);
                        return;
                    }

                    // "Create new branch" selected
                    let name = state.filter.trim().to_string();
                    if name.is_empty() {
                        return;
                    }
                    let base = state.base_branch.clone();
                    let repo_path = state.repo_path.clone();
                    self.pending_add_worktree = None;
                    self.do_create_worktree(name, Some(base), repo_path);
                } else {
                    // Existing branch selected
                    let filtered = state.filtered();
                    let Some(&idx) = filtered.get(state.cursor - 1) else {
                        return;
                    };
                    let branch = state.branches[idx].clone();
                    let repo_path = state.repo_path.clone();
                    self.pending_add_worktree = None;
                    self.do_create_worktree(branch, None, repo_path);
                }
            }
            AddWorktreeMode::Pr => {
                let filtered = state.filtered_prs();
                let Some(&idx) = filtered.get(state.cursor) else {
                    return;
                };
                let prs = match &state.pr_list {
                    Some(PrListState::Loaded { prs, .. }) => prs,
                    _ => return,
                };
                let pr_number = prs[idx].number;
                let repo_path = state.repo_path.clone();
                self.pending_add_worktree = None;
                self.do_checkout_pr(pr_number, repo_path);
            }
        }
    }

    /// Checkout a PR in a background thread (quiet, no stdout/spinner).
    fn do_checkout_pr(&mut self, pr_number: u32, repo_path: PathBuf) {
        let config = self.config.clone();
        let mux = self.mux.clone();
        let tx = self.event_tx.clone();

        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<String> {
                std::env::set_current_dir(&repo_path)?;

                let result = workflow::pr::resolve_pr_ref(pr_number, None, true)?;
                let local_branch = result.local_branch;

                let ctx = workflow::WorkflowContext::new(config.clone(), mux, None)?;
                let handle = crate::naming::derive_handle(&local_branch, None, &config)?;
                let mut options = workflow::types::SetupOptions::new(true, true, true);
                options.focus_window = false;
                options.mode = config.mode();

                let result = workflow::create(
                    &ctx,
                    workflow::CreateArgs {
                        branch_name: &local_branch,
                        handle: &handle,
                        base_branch: None,
                        remote_branch: None,
                        prompt: None,
                        options,
                        agent: None,
                        is_explicit_name: false,
                        prompt_file_only: false,
                        fork_source: None,
                    },
                )?;
                Ok(result.branch_name)
            })();

            match result {
                Ok(branch) => {
                    let _ = tx.send(AppEvent::AddWorktreeResult(Ok(branch)));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::AddWorktreeResult(Err(e.to_string())));
                }
            }
        });

        self.status_message = Some((
            format!("Checking out PR #{}...", pr_number),
            std::time::Instant::now(),
        ));
    }

    /// Execute worktree creation in a background thread.
    fn do_create_worktree(
        &mut self,
        name: String,
        base_branch: Option<String>,
        repo_path: PathBuf,
    ) {
        let config = self.config.clone();
        let mux = self.mux.clone();
        let tx = self.event_tx.clone();
        let status_name = name.clone();

        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<String> {
                let ctx = workflow::WorkflowContext::new(config.clone(), mux, None)?;
                let handle = crate::naming::derive_handle(&name, None, &config)?;
                let mut options = workflow::types::SetupOptions::new(true, true, true);
                options.focus_window = false;
                options.mode = config.mode();

                // Set working directory for git operations
                std::env::set_current_dir(&repo_path)?;

                let result = workflow::create(
                    &ctx,
                    workflow::CreateArgs {
                        branch_name: &name,
                        handle: &handle,
                        base_branch: base_branch.as_deref(),
                        remote_branch: None,
                        prompt: None,
                        options,
                        agent: None,
                        is_explicit_name: false,
                        prompt_file_only: false,
                        fork_source: None,
                    },
                )?;
                Ok(result.branch_name)
            })();

            match result {
                Ok(branch) => {
                    // Trigger worktree list refresh by sending a refetch
                    // The main loop will pick this up and refresh
                    let _ = tx.send(AppEvent::AddWorktreeResult(Ok(branch)));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::AddWorktreeResult(Err(e.to_string())));
                }
            }
        });

        self.status_message = Some((
            format!("Creating worktree '{}'...", status_name),
            std::time::Instant::now(),
        ));
    }

    /// Handle the result of a background add-worktree operation.
    pub fn handle_add_worktree_result(&mut self, result: Result<String, String>) {
        match result {
            Ok(branch) => {
                self.status_message = Some((
                    format!("Created worktree '{}'", branch),
                    std::time::Instant::now(),
                ));
                self.trigger_worktree_refetch();
            }
            Err(e) => {
                self.status_message = Some((
                    format!("Failed to create worktree: {}", e),
                    std::time::Instant::now(),
                ));
            }
        }
    }

    /// Update the preview for the selected worktree (git log)
    fn update_worktree_preview(&mut self) {
        let current_path = self
            .worktree_table_state
            .selected()
            .and_then(|idx| self.worktrees.get(idx))
            .map(|w| w.path.clone());

        if current_path != self.worktree_preview_path {
            self.worktree_preview_path = current_path.clone();
            self.worktree_preview = None;

            if let Some(path) = current_path {
                let tx = self.event_tx.clone();
                std::thread::spawn(move || {
                    let output = std::process::Command::new("git")
                        .args(["log", "--format=%h\t%ar\t%s", "-n", "20"])
                        .current_dir(&path)
                        .output();
                    if let Ok(out) = output {
                        let log = String::from_utf8_lossy(&out.stdout).to_string();
                        let _ = tx.send(AppEvent::WorktreeLog(path, log));
                    }
                });
            }
        }
    }
}
