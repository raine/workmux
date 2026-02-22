use anyhow::{Context, Result, anyhow};

use crate::sandbox;
use tracing::{debug, info};

use super::cleanup;
use super::context::WorkflowContext;
use super::types::RemoveResult;

/// Remove a worktree without merging
pub fn remove(
    handle: &str,
    force: bool,
    keep_branch: bool,
    context: &WorkflowContext,
) -> Result<RemoveResult> {
    info!(handle = handle, force, keep_branch, "remove:start");

    // Get worktree path and branch - this also validates that the worktree exists
    // Smart resolution: try handle first, then branch name
    let (worktree_path, branch_name) = context.vcs.find_workspace(handle)
        .with_context(|| format!("No workspace found with name '{}'", handle))?;

    // Extract actual handle from worktree path (directory name)
    // User may have provided branch name (with slashes) but window names use handle (with dashes)
    let actual_handle = worktree_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            anyhow!(
                "Could not derive handle from worktree path: {}",
                worktree_path.display()
            )
        })?;

    debug!(handle = actual_handle, branch = branch_name, path = %worktree_path.display(), "remove:worktree resolved");

    // Capture mode BEFORE cleanup (cleanup removes the metadata)
    let mode = context.vcs.get_workspace_mode(actual_handle);

    // Safety Check: Prevent deleting the main worktree itself, regardless of branch.
    let is_main_worktree = match (
        worktree_path.canonicalize(),
        context.main_worktree_root.canonicalize(),
    ) {
        (Ok(canon_wt_path), Ok(canon_main_path)) => {
            // Best case: both paths exist and can be resolved. This is the most reliable check.
            canon_wt_path == canon_main_path
        }
        _ => {
            // Fallback: If canonicalization fails on either path (e.g., directory was
            // manually removed, broken symlink), compare the raw paths provided by git.
            // This is a critical safety net.
            worktree_path == context.main_worktree_root
        }
    };

    if is_main_worktree {
        return Err(anyhow!(
            "Cannot remove branch '{}' because it is checked out in the main worktree at '{}'. \
            Switch the main worktree to a different branch first, or create a linked worktree for '{}'.",
            branch_name,
            context.main_worktree_root.display(),
            branch_name
        ));
    }

    // Safety Check: Prevent deleting the main branch by name (secondary check)
    if branch_name == context.main_branch {
        return Err(anyhow!(
            "Cannot delete the main branch ('{}')",
            context.main_branch
        ));
    }

    if worktree_path.exists() && context.vcs.has_uncommitted_changes(&worktree_path)? && !force {
        return Err(anyhow!(
            "Worktree has uncommitted changes. Use --force to delete anyway."
        ));
    }

    // Note: Unmerged branch check removed - git branch -d/D handles this natively
    // The CLI provides a user-friendly confirmation prompt before calling this function

    // Stop any running containers for this worktree before killing the window.
    // This is necessary because tmux kill-window sends SIGHUP which doesn't allow
    // the supervisor's Drop handler to run. We try unconditionally since sandbox
    // may have been enabled via --sandbox flag even if disabled in config.
    sandbox::stop_containers_for_handle(actual_handle);

    info!(branch = %branch_name, keep_branch, "remove:cleanup start");
    let cleanup_result = cleanup::cleanup(
        context,
        &branch_name,
        actual_handle,
        &worktree_path,
        force,
        keep_branch,
        false, // no_hooks: run hooks normally for user-initiated remove
    )?;

    // Navigate to the main branch window/session and close the source
    cleanup::navigate_to_target_and_close(
        context.mux.as_ref(),
        &context.prefix,
        &context.main_branch,
        actual_handle,
        &cleanup_result,
        mode,
    )?;

    Ok(RemoveResult {
        branch_removed: branch_name.to_string(),
    })
}
