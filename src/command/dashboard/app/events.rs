//! Event dispatching for background events.

use super::App;
use super::types::{AppEvent, PrListState, SweepOutcome};

fn sweep_complete_message(outcome: SweepOutcome) -> String {
    match (outcome.completed, outcome.scheduled) {
        (_, 0) => "Sweep complete".to_string(),
        (0, scheduled) => format!("Sweep cleanup scheduled for {scheduled} worktree(s)"),
        (completed, scheduled) => {
            format!("Sweep complete: {completed} removed, {scheduled} scheduled")
        }
    }
}

impl App {
    /// Apply a background event to app state.
    /// Called from the main loop when an AppEvent arrives on the unified channel.
    pub fn apply_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Terminal(_) => {} // handled separately in main loop
            AppEvent::RemotePreview(pane_id, capture) => {
                if let Some((_, running)) = &mut self.remote_preview_fetch {
                    *running = false;
                }
                let text =
                    capture.unwrap_or_else(|err| format!("(remote preview unavailable: {err})"));
                if self.preview_pane_id.as_ref() == Some(&pane_id) {
                    self.preview = Some(text.clone());
                }
                self.remote_previews.insert(pane_id, text);
            }
            AppEvent::GitStatus(path, status) => {
                self.git_statuses.insert(path, status);
            }
            AppEvent::GithubStatus(repo_root, summaries) => {
                let mut prs = std::collections::HashMap::new();
                let mut checks = std::collections::HashMap::new();
                for (branch, summary) in summaries {
                    if let Some(pr) = summary.pr {
                        prs.insert(branch.clone(), pr);
                    }
                    if let Some(check_summary) = summary.checks {
                        checks.insert(branch, check_summary);
                    }
                }
                if prs.is_empty() {
                    self.pr_statuses.remove(&repo_root);
                } else {
                    self.pr_statuses.insert(repo_root.clone(), prs);
                }
                if checks.is_empty() {
                    self.check_statuses.remove(&repo_root);
                } else {
                    self.check_statuses.insert(repo_root, checks);
                }
                if !self.all_worktrees.is_empty() {
                    self.apply_worktree_filters();
                }
            }
            AppEvent::WorktreeList(mut worktrees) => {
                self.removals.filter_snapshot(&mut worktrees);
                let needs_pr_fetch = self.all_worktrees.is_empty() && !worktrees.is_empty();
                self.all_worktrees = worktrees;
                self.apply_worktree_filters();

                // Force a PR re-fetch on initial load or after project switch
                // (confirm_project_picker clears all_worktrees, so this fires)
                if needs_pr_fetch {
                    self.last_pr_fetch = std::time::Instant::now() - super::PR_FETCH_INTERVAL;
                }
            }
            AppEvent::WorktreeLog(path, log) => {
                if self.worktree_preview_path.as_ref() == Some(&path) {
                    self.worktree_preview = Some(log);
                }
            }
            AppEvent::RemoveWorktreeResult(path, result) => {
                self.handle_remove_worktree_result(path, result);
            }
            AppEvent::AddWorktreeResult(result) => {
                self.handle_add_worktree_result(result);
            }
            AppEvent::AddWorktreePrList(request_id, result) => {
                if let Some(ref mut state) = self.pending_add_worktree
                    && request_id == state.pr_request_counter
                {
                    state.pr_list = Some(match result {
                        Ok(prs) => PrListState::Loaded { prs },
                        Err(msg) => PrListState::Error { message: msg },
                    });
                }
            }
            AppEvent::SweepProgressUpdate(current, total, handle) => {
                self.sweep_progress = Some(super::types::SweepProgress {
                    total,
                    current,
                    handle,
                });
            }
            AppEvent::SweepComplete(result) => {
                self.sweep_progress = None;
                match result {
                    Ok(outcome) => {
                        self.status_message =
                            Some((sweep_complete_message(outcome), std::time::Instant::now()));
                    }
                    Err(e) => {
                        self.status_message =
                            Some((format!("Sweep failed: {e}"), std::time::Instant::now()));
                    }
                }
                self.trigger_worktree_refetch();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{AppEvent, SweepOutcome};
    use super::sweep_complete_message;

    #[test]
    fn sweep_event_preserves_scheduled_cleanup_count() {
        let event = AppEvent::SweepComplete(Ok(SweepOutcome {
            completed: 1,
            scheduled: 2,
        }));
        let AppEvent::SweepComplete(Ok(outcome)) = event else {
            panic!("expected successful sweep event");
        };
        assert_eq!(outcome.completed, 1);
        assert_eq!(outcome.scheduled, 2);
        assert_eq!(
            sweep_complete_message(outcome),
            "Sweep complete: 1 removed, 2 scheduled"
        );
    }

    #[test]
    fn scheduled_only_sweep_is_not_reported_as_complete() {
        assert_eq!(
            sweep_complete_message(SweepOutcome {
                completed: 0,
                scheduled: 2,
            }),
            "Sweep cleanup scheduled for 2 worktree(s)"
        );
    }
}
