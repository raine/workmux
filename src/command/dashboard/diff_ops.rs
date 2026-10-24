//! Diff and patch mode operations for the dashboard.
//!
//! This module contains all operations related to:
//! - Loading diffs (WIP and branch diffs)
//! - Patch mode (hunk-by-hunk staging)
//! - Hunk manipulation (stage, skip, split, undo)
//! - Sending commands to agents (commit, merge)

use std::io::Write;

use super::ansi::parse_ansi_to_lines;
use super::app::{App, ViewMode};
use super::diff::{
    DiffView, RemoteDiff, extract_file_list, get_diff_content, get_file_list_numstat,
    map_file_offsets, parse_hunk_header, render_diff,
};
use crate::remote::agents::CachedDiff;
use crate::remote::protocol::DiffMode;

fn diff_mode(branch_diff: bool) -> DiffMode {
    if branch_diff {
        DiffMode::Branch
    } else {
        DiffMode::Wip
    }
}

/// Show a remote workspace's diff: the copy last fetched from its node at
/// once, then the node's current one when it arrives.
fn load_remote_diff(
    app: &mut App,
    remote: RemoteDiff,
    branch_diff: bool,
    path: std::path::PathBuf,
    pane_id: String,
) {
    let mode = diff_mode(branch_diff);
    let cached = crate::remote::agents::cached_diff(&remote.at.remote, &remote.at.ws_id, mode);
    let view = match &cached {
        Some(diff) => {
            let age = crate::util::format_compact_age(
                crate::remote::cache::now().saturating_sub(diff.fetched_at),
            );
            let note = format!("cached {age} ago, refreshing");
            remote_diff_view(&remote, branch_diff, path, pane_id, Ok(diff), Some(&note))
        }
        None => {
            let fetching = format!("Fetching the diff from '{}'...", remote.at.remote);
            remote_diff_view(&remote, branch_diff, path, pane_id, Err(&fetching), None)
        }
    };
    app.view_mode = ViewMode::Diff(Box::new(view));

    let tx = app.event_tx.clone();
    let config = app.config.clone();
    let at = remote.at;
    std::thread::spawn(move || {
        let result = crate::remote::agents::fetch_diff(&config, &at.remote, &at.ws_id, mode)
            .map_err(|err| format!("{err:#}"));
        let _ = tx.send(super::app::AppEvent::RemoteDiff(
            at.ws_id,
            branch_diff,
            result,
        ));
    });
}

/// Apply a diff fetched from a node to the open diff view it was fetched
/// for. A failed fetch leaves the cached copy, if any, with a note.
pub(super) fn apply_remote_diff(
    app: &mut App,
    ws_id: &str,
    branch_diff: bool,
    result: Result<CachedDiff, String>,
) {
    let ViewMode::Diff(ref view) = app.view_mode else {
        return;
    };
    let Some(remote) = view
        .remote
        .clone()
        .filter(|remote| remote.at.ws_id == ws_id && view.is_branch_diff == branch_diff)
    else {
        return;
    };
    let (path, pane_id, scroll) = (
        view.worktree_path.clone(),
        view.pane_id.clone(),
        view.scroll,
    );
    let mut next = match result {
        Ok(diff) => remote_diff_view(&remote, branch_diff, path, pane_id, Ok(&diff), None),
        Err(err) => {
            let mode = diff_mode(branch_diff);
            match crate::remote::agents::cached_diff(&remote.at.remote, ws_id, mode) {
                Some(diff) => {
                    let age = crate::util::format_compact_age(
                        crate::remote::cache::now().saturating_sub(diff.fetched_at),
                    );
                    let note = format!("cached {age} ago; {} is unreachable", remote.at.remote);
                    remote_diff_view(&remote, branch_diff, path, pane_id, Ok(&diff), Some(&note))
                }
                None => {
                    let message = format!("Could not fetch the diff: {err}");
                    remote_diff_view(&remote, branch_diff, path, pane_id, Err(&message), None)
                }
            }
        }
    };
    next.scroll = scroll.min(next.line_count.saturating_sub(1));
    app.view_mode = ViewMode::Diff(Box::new(next));
}

/// A read-only diff view of a remote workspace's diff, or of `message`
/// when there is none to show.
fn remote_diff_view(
    remote: &RemoteDiff,
    branch_diff: bool,
    path: std::path::PathBuf,
    pane_id: String,
    diff: Result<&CachedDiff, &str>,
    note: Option<&str>,
) -> DiffView {
    let empty = if branch_diff {
        "No commits on this branch yet"
    } else {
        "No uncommitted changes"
    };
    let (mut content, lines_added, lines_removed, hunks) = match diff {
        Ok(diff) if diff.text.trim().is_empty() => (empty.to_string(), 0, 0, Vec::new()),
        Ok(diff) => render_diff(diff.text.clone().into_bytes(), true)
            .unwrap_or_else(|err| (err, 0, 0, Vec::new())),
        Err(message) => (message.to_string(), 0, 0, Vec::new()),
    };
    if diff.is_ok_and(|diff| diff.truncated) {
        content.push_str("\n(the node cut this diff short)\n");
    }
    let parsed_lines = parse_ansi_to_lines(&content);
    let mut file_list = extract_file_list(&hunks);
    map_file_offsets(&mut file_list, &parsed_lines);
    let title = match note {
        Some(note) => format!("{} ({note})", remote.title),
        None => remote.title.clone(),
    };
    DiffView {
        line_count: parsed_lines.len().max(1),
        content,
        parsed_lines,
        scroll: 0,
        viewport_height: 0,
        title,
        worktree_path: path,
        pane_id,
        is_branch_diff: branch_diff,
        lines_added,
        lines_removed,
        patch_mode: false,
        hunks: Vec::new(),
        current_hunk: 0,
        hunks_total: 0,
        hunks_processed: 0,
        staged_hunks: Vec::new(),
        comment_input: None,
        file_list,
        remote: Some(remote.clone()),
    }
}

/// Extension trait for diff and patch mode operations on App.
pub trait DiffOps {
    fn stage_hunk(&mut self) -> Result<(), String>;
    fn next_hunk(&mut self) -> bool;
    fn prev_hunk(&mut self);
    fn enter_patch_mode(&mut self);
    fn exit_patch_mode(&mut self);
    fn stage_and_next(&mut self);
    fn skip_hunk(&mut self);
    fn undo_staged_hunk(&mut self);
    fn send_hunk_comment(&mut self);
    fn split_current_hunk(&mut self) -> bool;
    fn load_diff(&mut self, branch_diff: bool);
    fn close_diff(&mut self);
    fn send_commit_to_agent(&mut self);
    fn trigger_merge(&mut self);
    fn send_commit_to_selected(&mut self);
    fn trigger_merge_for_selected(&mut self);
}

/// Reload diff showing only unstaged changes (for patch mode).
/// Private helper - not part of the public trait.
fn reload_unstaged_diff(app: &mut App) {
    let (path, pane_id, worktree_name) = if let ViewMode::Diff(ref diff) = app.view_mode {
        (
            diff.worktree_path.clone(),
            diff.pane_id.clone(),
            diff.title
                .strip_prefix("WIP: ")
                .unwrap_or(&diff.title)
                .to_string(),
        )
    } else {
        return;
    };

    // Use empty diff_arg for unstaged changes only (git diff without args)
    // Include untracked files, parse hunks for patch mode
    match get_diff_content(&path, "", true, true) {
        Ok((content, lines_added, lines_removed, hunks)) => {
            let (content, line_count) = if content.trim().is_empty() {
                ("No uncommitted changes".to_string(), 1)
            } else {
                let count = content.lines().count();
                (content, count)
            };
            let parsed_lines = parse_ansi_to_lines(&content);
            let mut file_list = extract_file_list(&hunks);
            map_file_offsets(&mut file_list, &parsed_lines);

            app.view_mode = ViewMode::Diff(Box::new(DiffView {
                content,
                parsed_lines,
                scroll: 0,
                line_count,
                viewport_height: 0,
                title: format!("WIP: {}", worktree_name),
                worktree_path: path,
                pane_id,
                is_branch_diff: false,
                lines_added,
                lines_removed,
                patch_mode: false,
                hunks,
                current_hunk: 0,
                hunks_total: 0,
                hunks_processed: 0,
                staged_hunks: Vec::new(),
                comment_input: None,
                file_list,
                remote: None,
            }));
        }
        Err(e) => {
            let parsed_lines = parse_ansi_to_lines(&e);
            app.view_mode = ViewMode::Diff(Box::new(DiffView {
                content: e,
                parsed_lines,
                scroll: 0,
                line_count: 1,
                viewport_height: 0,
                title: "Error".to_string(),
                worktree_path: path,
                pane_id,
                is_branch_diff: false,
                lines_added: 0,
                lines_removed: 0,
                patch_mode: false,
                hunks: Vec::new(),
                current_hunk: 0,
                hunks_total: 0,
                hunks_processed: 0,
                staged_hunks: Vec::new(),
                comment_input: None,
                file_list: Vec::new(),
                remote: None,
            }));
        }
    }
}

/// Focus the diff agent pane and send a command, then close the diff modal
fn focus_and_send(app: &mut App, command: String) {
    if let ViewMode::Diff(diff) = &app.view_mode {
        if app.mux.requires_focus_for_input() {
            let window_hint = app
                .agents
                .iter()
                .find(|a| a.pane_id == diff.pane_id)
                .map(|a| a.window_name.clone());
            let _ = app
                .mux
                .switch_to_pane(&diff.pane_id, window_hint.as_deref());
        }

        let _ = app
            .mux
            .send_keys_to_agent(&diff.pane_id, &command, app.config.agent.as_deref());
    }
    app.close_diff();
}

impl DiffOps for App {
    /// Stage a single hunk using git apply --cached
    fn stage_hunk(&mut self) -> Result<(), String> {
        let ViewMode::Diff(ref diff) = self.view_mode else {
            return Err("Not in diff view".to_string());
        };

        if !diff.patch_mode || diff.hunks.is_empty() {
            return Err("Not in patch mode or no hunks".to_string());
        }

        let hunk = &diff.hunks[diff.current_hunk];
        // Hunks are clean (no ANSI codes) since we use --no-color for diff
        let patch_content = format!("{}\n{}\n", hunk.file_header, hunk.hunk_body);

        let mut child = crate::git::pinned_git(&diff.worktree_path)
            .map_err(|e| format!("Cannot validate Git metadata: {e}"))?
            .args(["apply", "--cached", "--recount", "--3way", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn git: {}", e))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(patch_content.as_bytes())
                .map_err(|e| format!("Failed to write to stdin: {}", e))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| format!("Failed to wait on git: {}", e))?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git apply failed: {}", err));
        }

        Ok(())
    }

    /// Move to next hunk in patch mode, returns true if there are more hunks
    fn next_hunk(&mut self) -> bool {
        if let ViewMode::Diff(ref mut diff) = self.view_mode
            && diff.patch_mode
            && diff.current_hunk + 1 < diff.hunks.len()
        {
            diff.current_hunk += 1;
            diff.scroll = 0;
            return true;
        }
        false
    }

    /// Move to previous hunk in patch mode
    fn prev_hunk(&mut self) {
        if let ViewMode::Diff(ref mut diff) = self.view_mode
            && diff.patch_mode
            && diff.current_hunk > 0
        {
            diff.current_hunk -= 1;
            diff.scroll = 0;
        }
    }

    /// Enter patch mode for the current diff view
    fn enter_patch_mode(&mut self) {
        if let ViewMode::Diff(ref diff) = self.view_mode
            && diff.remote.is_some()
        {
            self.status_message = Some((
                "Patch mode needs a local worktree".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        // Check if we are in WIP diff view (patch mode not supported for branch diffs)
        let is_wip_diff = if let ViewMode::Diff(ref diff) = self.view_mode {
            !diff.is_branch_diff
        } else {
            false
        };

        if !is_wip_diff {
            return;
        }

        // Reload the diff to show only unstaged changes.
        // This ensures we only patch hunks that aren't already staged.
        // The WIP view uses `git diff HEAD` which shows all uncommitted changes,
        // but patch mode should only show unstaged changes (like `git add -p`).
        reload_unstaged_diff(self);

        // Enable patch mode if there are unstaged hunks
        if let ViewMode::Diff(ref mut diff) = self.view_mode {
            if diff.hunks.is_empty() {
                return;
            }
            diff.patch_mode = true;
            diff.current_hunk = 0;
            diff.scroll = 0;
            // Initialize progress tracking
            diff.hunks_total = diff.hunks.len();
            diff.hunks_processed = 0;
            diff.staged_hunks.clear();
        }
    }

    /// Exit patch mode back to normal diff view
    fn exit_patch_mode(&mut self) {
        if let ViewMode::Diff(ref mut diff) = self.view_mode {
            diff.patch_mode = false;
            diff.scroll = 0;
        }
    }

    /// Stage current hunk and advance to next, refreshing if needed
    fn stage_and_next(&mut self) {
        if let Err(e) = self.stage_hunk() {
            // TODO: Show error to user
            eprintln!("Failed to stage hunk: {}", e);
            return;
        }

        // Remove the staged hunk from the in-memory list and advance
        // Don't reload from git immediately - this preserves split hunks
        let should_reload = if let ViewMode::Diff(ref mut diff) = self.view_mode {
            if !diff.hunks.is_empty() {
                // Save the staged hunk for undo functionality
                let staged_hunk = diff.hunks.remove(diff.current_hunk);
                diff.staged_hunks.push(staged_hunk);
                diff.hunks_processed += 1;
                // Adjust index if we were at the end
                if diff.current_hunk >= diff.hunks.len() && !diff.hunks.is_empty() {
                    diff.current_hunk = diff.hunks.len() - 1;
                }
                diff.scroll = 0;
            }
            diff.hunks.is_empty()
        } else {
            false
        };

        if should_reload {
            // No more hunks in memory - reload to check for any remaining unstaged changes
            reload_unstaged_diff(self);

            // Re-enter patch mode if git found more hunks
            if let ViewMode::Diff(ref mut diff) = self.view_mode {
                if !diff.hunks.is_empty() {
                    diff.patch_mode = true;
                    diff.current_hunk = 0;
                } else {
                    diff.patch_mode = false;
                }
            }
        }
    }

    /// Skip current hunk and move to next
    fn skip_hunk(&mut self) {
        // Increment processed count
        if let ViewMode::Diff(ref mut diff) = self.view_mode {
            diff.hunks_processed += 1;
        }
        if !self.next_hunk() {
            // No more hunks, exit patch mode
            self.exit_patch_mode();
        }
    }

    /// Undo the last staged hunk (unstage it and restore to the list)
    fn undo_staged_hunk(&mut self) {
        let ViewMode::Diff(ref mut diff) = self.view_mode else {
            return;
        };

        if !diff.patch_mode || diff.staged_hunks.is_empty() {
            return;
        }

        // Pop the last staged hunk
        let hunk = diff.staged_hunks.pop().unwrap();

        // Unstage it using git apply --cached --reverse
        let patch_content = format!("{}\n{}\n", hunk.file_header, hunk.hunk_body);

        let result = crate::git::pinned_git(&diff.worktree_path)
            .and_then(|mut command| {
                command
                    .args(["apply", "--cached", "--reverse", "-"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(anyhow::Error::from)
            })
            .and_then(|mut child| {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(patch_content.as_bytes());
                }
                child.wait_with_output().map_err(anyhow::Error::from)
            });

        if let Ok(output) = result
            && output.status.success()
        {
            // Insert the hunk back at the current position
            diff.hunks.insert(diff.current_hunk, hunk);
            diff.hunks_processed = diff.hunks_processed.saturating_sub(1);
            diff.scroll = 0;
        }
    }

    /// Send a comment about the current hunk to the agent
    fn send_hunk_comment(&mut self) {
        let ViewMode::Diff(ref mut diff) = self.view_mode else {
            return;
        };

        if !diff.patch_mode || diff.hunks.is_empty() {
            return;
        }

        let comment = match diff.comment_input.take() {
            Some(c) if !c.trim().is_empty() => c,
            _ => return,
        };

        let hunk = &diff.hunks[diff.current_hunk];

        // Extract line number from hunk header (e.g., "@@ -10,5 +12,7 @@" -> 12)
        let line_num = parse_hunk_header(&hunk.hunk_body)
            .map(|(_, new_start)| new_start)
            .unwrap_or(1);

        // Determine safe code fence (use more backticks if content contains ```)
        let mut fence = "```".to_string();
        while hunk.hunk_body.contains(&fence) {
            fence.push('`');
        }

        // Format the message with file path, line number, hunk content, and comment
        let message = format!(
            "{}:{}\n\n{}diff\n{}\n{}\n\n{}",
            hunk.filename, line_num, fence, hunk.hunk_body, fence, comment
        );

        // Use paste_multiline to properly handle newlines in the message
        let _ = self.mux.paste_multiline(&diff.pane_id, &message);
        // Send an additional Enter to submit the comment to the agent
        let _ = self.mux.send_key(&diff.pane_id, "Enter");
    }

    /// Split the current hunk into smaller hunks if possible
    /// Returns true if the split was successful
    fn split_current_hunk(&mut self) -> bool {
        if let ViewMode::Diff(ref mut diff) = self.view_mode {
            if !diff.patch_mode || diff.hunks.is_empty() {
                return false;
            }

            let current_idx = diff.current_hunk;
            let current = &diff.hunks[current_idx];

            if let Some(sub_hunks) = current.split() {
                let num_new_hunks = sub_hunks.len();
                // Remove the original hunk and insert the split hunks
                diff.hunks.remove(current_idx);
                for (i, h) in sub_hunks.into_iter().enumerate() {
                    diff.hunks.insert(current_idx + i, h);
                }
                // Adjust total to account for the split (one hunk became num_new_hunks)
                diff.hunks_total += num_new_hunks - 1;
                // Stay at the first split hunk, reset scroll
                diff.scroll = 0;
                return num_new_hunks > 1;
            }
        }
        false
    }

    /// Load diff for the selected worktree
    /// - `branch_diff`: if true, diff against main branch; if false, diff HEAD (uncommitted)
    fn load_diff(&mut self, branch_diff: bool) {
        let Some(selected) = self.table_state.selected() else {
            return;
        };
        let Some(agent) = self.agents.get(selected) else {
            return;
        };

        let path = &agent.path;
        let pane_id = agent.pane_id.clone();
        let worktree_name = self.extract_worktree_name(agent).0;

        if let Some(remote) = agent.remote.clone() {
            let title = if branch_diff {
                let base = remote
                    .git
                    .as_ref()
                    .map(|status| status.base_branch.clone())
                    .filter(|base| !base.is_empty())
                    .unwrap_or_else(|| "base".to_string());
                format!("Review: {worktree_name} \u{2192} {base}")
            } else {
                format!("WIP: {worktree_name}")
            };
            let remote = RemoteDiff {
                at: remote.at,
                title,
            };
            let path = path.clone();
            load_remote_diff(self, remote, branch_diff, path, pane_id);
            return;
        }

        let (diff_arg, title) = if branch_diff {
            // Get the base branch from git status if available, then config, then "main"
            let base = self
                .git_statuses
                .get(path)
                .map(|s| s.base_branch.as_str())
                .filter(|b| !b.is_empty())
                .or_else(|| self.config.main_branch.as_deref().filter(|s| !s.is_empty()))
                .unwrap_or("main");
            (
                format!("{}...HEAD", base),
                format!("Review: {} \u{2192} {}", worktree_name, base),
            )
        } else {
            ("HEAD".to_string(), format!("WIP: {}", worktree_name))
        };

        // Include untracked files only for uncommitted changes view
        // Don't parse hunks eagerly - they're only needed for patch mode,
        // which reloads and parses them on demand via reload_unstaged_diff()
        let include_untracked = !branch_diff;
        let parse_hunks = false;
        match get_diff_content(path, &diff_arg, include_untracked, parse_hunks) {
            Ok((content, lines_added, lines_removed, hunks)) => {
                let (content, line_count) = if content.trim().is_empty() {
                    let msg = if branch_diff {
                        "No commits on this branch yet"
                    } else {
                        "No uncommitted changes"
                    };
                    (msg.to_string(), 1)
                } else {
                    let count = content.lines().count();
                    (content, count)
                };
                let parsed_lines = parse_ansi_to_lines(&content);

                // Get file list: from hunks for WIP, or via numstat for review mode
                let mut file_list = if !hunks.is_empty() {
                    extract_file_list(&hunks)
                } else {
                    get_file_list_numstat(path, &diff_arg, include_untracked)
                };
                map_file_offsets(&mut file_list, &parsed_lines);

                self.view_mode = ViewMode::Diff(Box::new(DiffView {
                    content,
                    parsed_lines,
                    scroll: 0,
                    line_count,
                    viewport_height: 0, // Will be set by UI
                    title,
                    worktree_path: path.clone(),
                    pane_id,
                    is_branch_diff: branch_diff,
                    lines_added,
                    lines_removed,
                    patch_mode: false,
                    hunks,
                    current_hunk: 0,
                    hunks_total: 0,
                    hunks_processed: 0,
                    staged_hunks: Vec::new(),
                    comment_input: None,
                    file_list,
                    remote: None,
                }));
            }
            Err(e) => {
                // Show error in diff view
                let parsed_lines = parse_ansi_to_lines(&e);
                self.view_mode = ViewMode::Diff(Box::new(DiffView {
                    content: e,
                    parsed_lines,
                    scroll: 0,
                    line_count: 1,
                    viewport_height: 0,
                    title: "Error".to_string(),
                    worktree_path: path.clone(),
                    pane_id,
                    is_branch_diff: branch_diff,
                    lines_added: 0,
                    lines_removed: 0,
                    patch_mode: false,
                    hunks: Vec::new(),
                    current_hunk: 0,
                    hunks_total: 0,
                    hunks_processed: 0,
                    staged_hunks: Vec::new(),
                    comment_input: None,
                    file_list: Vec::new(),
                    remote: None,
                }));
            }
        }
    }

    /// Close the diff modal and return to dashboard view
    fn close_diff(&mut self) {
        self.view_mode = ViewMode::Dashboard;
    }

    /// Send commit action to the agent pane and close diff modal
    fn send_commit_to_agent(&mut self) {
        focus_and_send(self, self.config.dashboard.commit().to_string());
    }

    /// Send merge action to the agent pane and close diff modal
    fn trigger_merge(&mut self) {
        focus_and_send(self, self.config.dashboard.merge().to_string());
    }

    /// Send commit action to the currently selected agent's pane (from dashboard view)
    fn send_commit_to_selected(&mut self) {
        let keys = self.config.dashboard.commit().to_string();
        self.send_keys_to_selected_agent(&keys);
    }

    /// Send merge action to the currently selected agent's pane (from dashboard view)
    fn trigger_merge_for_selected(&mut self) {
        let keys = self.config.dashboard.merge().to_string();
        self.send_keys_to_selected_agent(&keys);
    }
}
