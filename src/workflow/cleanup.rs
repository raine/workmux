use anyhow::{Context, Result};
use std::path::Path;
use std::{thread, time::Duration};

use crate::{cmd, git, tmux};
use tracing::{debug, info, warn};

use super::context::WorkflowContext;
use super::types::CleanupResult;

const WINDOW_CLOSE_DELAY_MS: u64 = 300;

/// Centralized function to clean up tmux and git resources.
/// `branch_name` is used for git operations (branch deletion).
/// `handle` is used for tmux operations (window lookup/kill).
pub fn cleanup(
    context: &WorkflowContext,
    branch_name: &str,
    handle: &str,
    worktree_path: &Path,
    force: bool,
    keep_branch: bool,
) -> Result<CleanupResult> {
    info!(
        branch = branch_name,
        handle = handle,
        path = %worktree_path.display(),
        force,
        keep_branch,
        "cleanup:start"
    );
    // Change the CWD to main worktree before any destructive operations.
    // This prevents "Unable to read current working directory" errors when the command
    // is run from within the worktree being deleted.
    context.chdir_to_main_worktree()?;

    let tmux_running = tmux::is_running().unwrap_or(false);
    let running_inside_target_window = if tmux_running {
        match tmux::current_window_name() {
            Ok(Some(current_name)) => current_name == tmux::prefixed(&context.prefix, handle),
            _ => false,
        }
    } else {
        false
    };

    let mut result = CleanupResult {
        tmux_window_killed: false,
        worktree_removed: false,
        local_branch_deleted: false,
        ran_inside_target_window: running_inside_target_window,
    };

    // Helper closure to perform the actual filesystem and git cleanup.
    // This avoids code duplication while enforcing the correct operational order.
    let perform_fs_git_cleanup = |result: &mut CleanupResult| -> Result<()> {
        // Run pre-delete hooks before removing the worktree directory
        if let Some(pre_delete_hooks) = &context.config.pre_delete {
            info!(
                branch = branch_name,
                count = pre_delete_hooks.len(),
                "cleanup:running pre-delete hooks"
            );
            let hook_env = [("WORKMUX_HANDLE", handle)];
            for command in pre_delete_hooks {
                // Run the hook with the worktree path as the working directory.
                // This allows for relative paths like `node_modules` in the command.
                cmd::shell_command_with_env(command, worktree_path, &hook_env)
                    .with_context(|| format!("Failed to run pre-delete command: '{}'", command))?;
            }
        }

        // 1. Forcefully remove the worktree directory from the filesystem.
        if worktree_path.exists() {
            std::fs::remove_dir_all(worktree_path).with_context(|| {
                format!(
                    "Failed to remove worktree directory at {}. \
                Please close any terminals or editors using this directory and try again.",
                    worktree_path.display()
                )
            })?;
            result.worktree_removed = true;
            info!(branch = branch_name, path = %worktree_path.display(), "cleanup:worktree directory removed");
        }

        // Clean up the prompt file if it exists
        let prompt_filename = format!("workmux-prompt-{}.md", branch_name);
        let prompt_file = std::env::temp_dir().join(prompt_filename);
        if prompt_file.exists() {
            if let Err(e) = std::fs::remove_file(&prompt_file) {
                warn!(path = %prompt_file.display(), error = %e, "cleanup:failed to remove prompt file");
            } else {
                debug!(path = %prompt_file.display(), "cleanup:prompt file removed");
            }
        }

        // 2. Prune worktrees to clean up git's metadata.
        git::prune_worktrees().context("Failed to prune worktrees")?;
        debug!("cleanup:git worktrees pruned");

        // 3. Delete the local branch (unless keeping it).
        if !keep_branch {
            git::delete_branch(branch_name, force).context("Failed to delete local branch")?;
            result.local_branch_deleted = true;
            info!(branch = branch_name, "cleanup:local branch deleted");
        }

        Ok(())
    };

    if running_inside_target_window {
        info!(
            branch = branch_name,
            "cleanup:deferring tmux window kill because command is running inside the window"
        );
        // Perform all filesystem and git cleanup *before* returning. The caller
        // will then schedule the asynchronous window close.
        perform_fs_git_cleanup(&mut result)?;
    } else {
        // Not running inside the target window, so we kill the window first
        // to release any shell locks on the directory.
        if tmux_running && tmux::window_exists(&context.prefix, handle).unwrap_or(false) {
            tmux::kill_window(&context.prefix, handle).context("Failed to kill tmux window")?;
            result.tmux_window_killed = true;
            info!(handle = handle, "cleanup:tmux window killed");

            // Poll to confirm the window is gone before proceeding. This prevents a race
            // condition where we try to delete the directory before the shell inside
            // the tmux window has terminated.
            const MAX_RETRIES: u32 = 20;
            const RETRY_DELAY: Duration = Duration::from_millis(50);
            let mut window_is_gone = false;
            for _ in 0..MAX_RETRIES {
                if !tmux::window_exists(&context.prefix, handle)? {
                    window_is_gone = true;
                    break;
                }
                thread::sleep(RETRY_DELAY);
            }

            if !window_is_gone {
                warn!(
                    handle = handle,
                    "cleanup:tmux window did not close within retry budget"
                );
                eprintln!(
                    "Warning: tmux window for '{}' did not close in the allotted time. \
                    Filesystem cleanup may fail.",
                    handle
                );
            }
        }
        // Now that the window is gone, it's safe to clean up the filesystem and git state.
        perform_fs_git_cleanup(&mut result)?;
    }

    Ok(result)
}

/// Navigate to the target branch window and close the source window.
/// Handles both cases: running inside the source window (async) and outside (sync).
/// `target_window_name` is the tmux window name of the merge target.
/// `source_handle` is the tmux window name of the branch being merged/removed.
pub fn navigate_to_target_and_close(
    prefix: &str,
    target_window_name: &str,
    source_handle: &str,
    cleanup_result: &CleanupResult,
) -> Result<()> {
    /// Helper function to shell-escape strings for safe inclusion in shell commands
    fn shell_escape(s: &str) -> String {
        format!("'{}'", s.replace('\'', r#"'\''"#))
    }

    // Check if target window exists
    if !tmux::is_running()? || !tmux::window_exists(prefix, target_window_name)? {
        // If target window doesn't exist, still need to close source window if running inside it
        if cleanup_result.ran_inside_target_window {
            let delay = Duration::from_millis(WINDOW_CLOSE_DELAY_MS);
            match tmux::schedule_window_close(prefix, source_handle, delay) {
                Ok(_) => info!(
                    handle = source_handle,
                    "cleanup:tmux window close scheduled"
                ),
                Err(e) => warn!(
                    handle = source_handle,
                    error = %e,
                    "cleanup:failed to schedule tmux window close",
                ),
            }
        }
        return Ok(());
    }

    if cleanup_result.ran_inside_target_window {
        // Running inside source window: schedule both navigation and kill together
        let delay = Duration::from_millis(WINDOW_CLOSE_DELAY_MS);
        let delay_secs = format!("{:.3}", delay.as_secs_f64());
        let target_prefixed = shell_escape(&tmux::prefixed(prefix, target_window_name));
        let source_prefixed = shell_escape(&tmux::prefixed(prefix, source_handle));
        let script = format!(
            "sleep {delay}; tmux select-window -t ={target} >/dev/null 2>&1; tmux kill-window -t ={source} >/dev/null 2>&1",
            delay = delay_secs,
            target = target_prefixed,
            source = source_prefixed,
        );

        match tmux::run_shell(&script) {
            Ok(_) => info!(
                handle = source_handle,
                target = target_window_name,
                "cleanup:scheduled navigation to target and window close"
            ),
            Err(e) => warn!(
                handle = source_handle,
                error = %e,
                "cleanup:failed to schedule navigation and window close",
            ),
        }
    } else {
        // Running outside source window: synchronously navigate to target and close source
        tmux::select_window(prefix, target_window_name)?;
        info!(
            handle = source_handle,
            target = target_window_name,
            "cleanup:navigated to target branch window"
        );

        // Close the source window now that we've navigated away
        match tmux::kill_window(prefix, source_handle) {
            Ok(_) => info!(
                handle = source_handle,
                "cleanup:closed source branch window"
            ),
            Err(e) => warn!(
                handle = source_handle,
                error = %e,
                "cleanup:failed to close source branch window",
            ),
        }
    }

    Ok(())
}
