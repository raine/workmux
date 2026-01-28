use anyhow::{Context, Result, anyhow};

use crate::config::TmuxTarget;
use crate::git;
use tracing::{debug, info};

use super::cleanup::{self, get_worktree_target};
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
    let (worktree_path, branch_name) = git::find_worktree(handle)
        .with_context(|| format!("No worktree found with name '{}'", handle))?;
    debug!(handle = handle, branch = branch_name, path = %worktree_path.display(), "remove:worktree resolved");

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

    if worktree_path.exists() && git::has_uncommitted_changes(&worktree_path)? && !force {
        return Err(anyhow!(
            "Worktree has uncommitted changes. Use --force to delete anyway."
        ));
    }

    // Note: Unmerged branch check removed - git branch -d/D handles this natively
    // The CLI provides a user-friendly confirmation prompt before calling this function
    info!(branch = %branch_name, keep_branch, "remove:cleanup start");
    let cleanup_result = cleanup::cleanup(
        context,
        &branch_name,
        handle,
        &worktree_path,
        force,
        keep_branch,
    )?;

    // Navigate to the main branch window/session and close the source
    let is_session_mode = get_worktree_target(handle) == TmuxTarget::Session;
    cleanup::navigate_to_target_and_close(
        &context.prefix,
        &context.main_branch,
        handle,
        &cleanup_result,
        is_session_mode,
    )?;

    Ok(RemoveResult {
        branch_removed: branch_name.to_string(),
    })
}
