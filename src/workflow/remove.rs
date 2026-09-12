use anyhow::{Result, anyhow};
use std::io::{self, Write};
use std::path::PathBuf;

use crate::git;
use crate::sandbox;
use crate::vcs::VcsBackend;
use tracing::{debug, info};

use super::cleanup::{self, attachment_via_vcs, mode_via_vcs};
use super::context::WorkflowContext;
use super::types::RemoveResult;

/// Find a worktree/workspace by handle (directory name) or branch/bookmark
/// name, over any [`VcsBackend`] rather than `git::find_worktree`'s
/// git-worktree-only lookup.
///
/// Mirrors `command::remove::find_worktree`'s two-pass logic (handle match,
/// then branch/bookmark match), since both need the same behavior against
/// the same `VcsBackend::list_workspaces_in` data and there is currently no
/// shared location both `workflow` and `command` can pull a single copy
/// from without introducing a `workflow` -> `command` dependency. Unlike
/// that copy (which always passes `None`, relying on its caller building a
/// fresh `WorkflowContext` from the process's actual cwd), this takes an
/// explicit `workdir` so it works correctly when `context.execution_dir`
/// differs from the process's current directory - e.g. under test, where
/// `WorkflowContext::new_in` is pointed at a fixture directory without a
/// matching `std::env::set_current_dir` call. A jj workspace is never
/// registered as a git worktree (even when colocated), so
/// `git::find_worktree` (used here previously) always fails to find one;
/// this is what makes `workflow::remove` reach jj workspaces at all.
fn find_worktree(
    vcs: &dyn VcsBackend,
    name: &str,
    workdir: Option<&std::path::Path>,
) -> Result<(PathBuf, String)> {
    let workspaces = vcs.list_workspaces_in(workdir)?;

    for entry in &workspaces {
        if entry.name.as_deref() == Some(name) {
            let branch = entry
                .branch_or_bookmark
                .clone()
                .unwrap_or_else(|| "(detached)".to_string());
            return Ok((entry.path.clone(), branch));
        }
    }

    for entry in workspaces {
        if entry.branch_or_bookmark.as_deref() == Some(name) {
            let branch = entry.branch_or_bookmark.unwrap_or_default();
            return Ok((entry.path, branch));
        }
    }

    Err(anyhow!(
        "Worktree '{}' not found among the repository's worktrees/workspaces",
        name
    ))
}

pub fn fallback_worktree_path(handle: &str, context: &WorkflowContext) -> Result<Option<PathBuf>> {
    let base_dir = if let Some(ref worktree_dir) = context.config.worktree_dir {
        crate::util::expand_worktree_dir(worktree_dir, &context.main_worktree_root)?
    } else {
        let project_name = context
            .main_worktree_root
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("Could not determine project name"))?;
        context
            .main_worktree_root
            .parent()
            .ok_or_else(|| anyhow!("Could not determine parent directory"))?
            .join(format!("{}__worktrees", project_name))
    };

    let path = base_dir.join(handle);
    let Some(admin_dir) = git::linked_worktree_admin_dir(&path) else {
        return Ok(None);
    };
    let expected_parent = context.git_common_dir.join("worktrees");
    Ok((!admin_dir.is_dir() && admin_dir.starts_with(expected_parent)).then_some(path))
}

/// Remove a worktree without merging
pub fn remove(
    handle: &str,
    force: bool,
    keep_branch: bool,
    context: &WorkflowContext,
) -> Result<RemoveResult> {
    remove_with_hook_output(handle, force, keep_branch, context, true)
}

/// Remove a worktree while keeping hook output away from the terminal.
pub fn remove_quiet(
    handle: &str,
    force: bool,
    keep_branch: bool,
    context: &WorkflowContext,
) -> Result<RemoveResult> {
    remove_with_hook_output(handle, force, keep_branch, context, false)
}

fn remove_with_hook_output(
    handle: &str,
    force: bool,
    keep_branch: bool,
    context: &WorkflowContext,
    show_hook_output: bool,
) -> Result<RemoveResult> {
    info!(handle = handle, force, keep_branch, "remove:start");

    // Get worktree path and branch - this also validates that the worktree exists
    // Smart resolution: try handle first, then branch name
    let (worktree_path, branch_name) =
        match find_worktree(context.vcs.as_ref(), handle, Some(&context.execution_dir)) {
            Ok(worktree) => worktree,
            Err(e) => {
                if let Some(path) = fallback_worktree_path(handle, context)? {
                    (path, String::new())
                } else {
                    return Err(anyhow!(
                        "Worktree '{}' not found. Use 'workmux list' to see available worktrees.",
                        handle
                    )
                    .context(e));
                }
            }
        };

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

    // Capture metadata before cleanup removes it.
    let mode = mode_via_vcs(context, actual_handle);
    let attachment = attachment_via_vcs(context, actual_handle);

    // Safety Check: Prevent deleting the main worktree itself, regardless of branch.
    if context.is_main_worktree(&worktree_path) {
        return Err(anyhow!(
            "Cannot remove branch '{}' because it is checked out in the main worktree at '{}'. \
            Switch the main worktree to a different branch first, or create a linked worktree for '{}'.",
            branch_name,
            context.main_worktree_root.display(),
            branch_name
        ));
    }

    if branch_name.is_empty() && !keep_branch {
        return Err(anyhow!(
            "Worktree '{}' has broken Git metadata, so its branch cannot be determined. \
            Use --keep-branch to remove only the worktree directory.",
            actual_handle
        ));
    }

    // Safety Check: Prevent deleting the main branch by name (secondary check)
    if branch_name == context.main_branch {
        return Err(anyhow!(
            "Cannot delete the main branch ('{}')",
            context.main_branch
        ));
    }

    if worktree_path.exists()
        && !git::has_missing_admin_dir(&worktree_path)
        && context.vcs.get_status(&worktree_path, None)?.is_dirty
        && !force
    {
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
        cleanup::CleanupOptions {
            force,
            keep_branch,
            no_hooks: false,
            show_hook_output,
        },
    )?;

    if attachment.manages_mux() {
        cleanup::navigate_to_target_and_close(
            context.mux.as_ref(),
            &context.prefix,
            &context.main_branch,
            actual_handle,
            &cleanup_result,
            mode,
        )?;
    }

    let cleanup_scheduled = cleanup_result.deferred_cleanup.is_some();
    if cleanup_scheduled && show_hook_output {
        if keep_branch {
            println!(
                "✓ Scheduled removal of worktree '{}' (branch '{}' will be kept)",
                actual_handle, branch_name
            );
        } else {
            println!(
                "✓ Scheduled removal of worktree '{}' and branch '{}'",
                actual_handle, branch_name
            );
        }
        io::stdout().flush()?;
    }

    Ok(RemoveResult {
        branch_removed: branch_name.to_string(),
        cleanup_scheduled,
    })
}
