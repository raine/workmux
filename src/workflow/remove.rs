use anyhow::{Context, Result, anyhow};

use crate::git;
use tracing::{debug, info};

use super::cleanup;
use super::context::WorkflowContext;
use super::types::RemoveResult;

/// Remove a worktree without merging
pub fn remove(
    branch_name: &str,
    force: bool,
    keep_branch: bool,
    context: &WorkflowContext,
) -> Result<RemoveResult> {
    info!(branch = branch_name, force, keep_branch, "remove:start");

    // Get worktree path - this also validates that the worktree exists
    let worktree_path = git::get_worktree_path(branch_name)
        .with_context(|| format!("No worktree found for branch '{}'", branch_name))?;
    debug!(branch = branch_name, path = %worktree_path.display(), "remove:worktree resolved");

    // The handle is the basename of the worktree directory. This is the source of truth
    // for tmux window naming, as it was derived during `workmux add` using the config's
    // naming strategy and prefix at that time.
    let handle = worktree_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| {
            anyhow!(
                "Could not derive handle from worktree path: {}",
                worktree_path.display()
            )
        })?;
    debug!(
        branch = branch_name,
        handle = handle,
        "remove:derived handle from path"
    );

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
    info!(branch = branch_name, keep_branch, "remove:cleanup start");
    let cleanup_result = cleanup::cleanup(
        context,
        branch_name,
        handle,
        &worktree_path,
        force,
        keep_branch,
    )?;

    // Navigate to the main branch window and close the source window
    cleanup::navigate_to_target_and_close(
        &context.prefix,
        &context.main_branch,
        handle,
        &cleanup_result,
    )?;

    Ok(RemoveResult {
        branch_removed: branch_name.to_string(),
    })
}
