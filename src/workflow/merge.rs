use anyhow::{Context, Result, anyhow};

use crate::git;
use tracing::{debug, info};

use super::cleanup;
use super::context::WorkflowContext;
use super::types::MergeResult;

/// Merge a branch into the target branch and clean up
#[allow(clippy::too_many_arguments)]
pub fn merge(
    branch_name: &str,
    into_branch: Option<&str>,
    ignore_uncommitted: bool,
    rebase: bool,
    squash: bool,
    keep: bool,
    context: &WorkflowContext,
) -> Result<MergeResult> {
    info!(
        branch = branch_name,
        into = into_branch,
        ignore_uncommitted,
        rebase,
        squash,
        keep,
        "merge:start"
    );

    // Change CWD to main worktree to prevent errors if the command is run from within
    // the worktree that is about to be deleted.
    context.chdir_to_main_worktree()?;

    let branch_to_merge = branch_name;
    let target_branch = into_branch.unwrap_or(&context.main_branch);

    // Resolve the worktree path and window handle for the TARGET branch.
    // If the target branch is the configured main branch, we use the main worktree root
    // and the main branch name as the window handle (standard workmux convention).
    // Otherwise, we check if the target branch has a dedicated worktree.
    // If it doesn't, we fallback to using the main worktree root but switch it to the target branch.
    let (target_worktree_path, target_window_name) = if target_branch == context.main_branch {
        (
            context.main_worktree_root.clone(),
            context.main_branch.clone(),
        )
    } else {
        match git::get_worktree_path(target_branch) {
            Ok(path) => {
                // Check if the target is checked out in the main worktree.
                // In that case, use the main branch name as the window handle
                // (main worktree window is named after main_branch, not directory).
                if path == context.main_worktree_root {
                    (path, context.main_branch.clone())
                } else {
                    // Target has its own dedicated worktree. Use its directory name as the handle.
                    let handle = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .ok_or_else(|| anyhow!("Invalid worktree path for target branch"))?
                        .to_string();
                    (path, handle)
                }
            }
            Err(_) => {
                // Target branch exists but is not checked out in any worktree.
                // We will use the main worktree to perform the merge.
                // The target window remains the main window (since that's where we are merging).
                debug!(
                    target = target_branch,
                    "merge:target branch has no worktree, using main worktree"
                );
                (
                    context.main_worktree_root.clone(),
                    context.main_branch.clone(),
                )
            }
        }
    };

    // Get worktree path for the branch to be merged
    let worktree_path = git::get_worktree_path(branch_to_merge)
        .with_context(|| format!("No worktree found for branch '{}'", branch_to_merge))?;
    debug!(
        branch = branch_to_merge,
        path = %worktree_path.display(),
        "merge:worktree resolved"
    );

    // The handle is the basename of the worktree directory (used for tmux operations)
    let handle = worktree_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| {
            anyhow!(
                "Could not derive handle from worktree path: {}",
                worktree_path.display()
            )
        })?;

    // Handle changes in the source worktree
    // Check for both unstaged changes and untracked files to prevent data loss during cleanup
    let has_unstaged = git::has_unstaged_changes(&worktree_path)?;
    let has_untracked = git::has_untracked_files(&worktree_path)?;

    if (has_unstaged || has_untracked) && !ignore_uncommitted {
        let mut issues = Vec::new();
        if has_unstaged {
            issues.push("unstaged changes");
        }
        if has_untracked {
            issues.push("untracked files (will be lost)");
        }
        return Err(anyhow!(
            "Worktree for '{}' has {}. Please stage or stash them, or use --ignore-uncommitted.",
            branch_to_merge,
            issues.join(" and ")
        ));
    }

    let had_staged_changes = git::has_staged_changes(&worktree_path)?;
    if had_staged_changes && !ignore_uncommitted {
        // Commit using git's editor (respects $EDITOR or git config)
        info!(path = %worktree_path.display(), "merge:committing staged changes");
        git::commit_with_editor(&worktree_path).context("Failed to commit staged changes")?;
    }

    if branch_to_merge == target_branch {
        return Err(anyhow!(
            "Cannot merge branch '{}' into itself.",
            branch_to_merge
        ));
    }
    debug!(
        branch = branch_to_merge,
        target = target_branch,
        "merge:target branch resolved"
    );

    // Safety check: Abort if the target worktree has uncommitted changes
    if git::has_uncommitted_changes(&target_worktree_path)? {
        return Err(anyhow!(
            "Target worktree ({}) has uncommitted changes. Please commit or stash them before merging.",
            target_worktree_path.display()
        ));
    }

    // Explicitly switch the target worktree to the target branch.
    // This ensures that if we are reusing the main worktree for a feature branch merge,
    // it is checked out to the correct branch.
    git::switch_branch_in_worktree(&target_worktree_path, target_branch)?;

    // Helper closure to generate the error message for merge conflicts
    let conflict_err = |branch: &str| -> anyhow::Error {
        let retry_cmd = if into_branch.is_some() {
            format!("workmux merge {} --into {}", branch, target_branch)
        } else {
            format!("workmux merge {}", branch)
        };
        anyhow!(
            "Merge failed due to conflicts. Target worktree kept clean.\n\n\
            To resolve, update your branch in worktree at {}:\n\
              git rebase {}  (recommended)\n\
            Or:\n\
              git merge {}\n\n\
            After resolving conflicts, retry: {}",
            worktree_path.display(),
            target_branch,
            target_branch,
            retry_cmd
        )
    };

    if rebase {
        // Rebase the feature branch on top of target inside its own worktree.
        // This is where conflicts will be detected.
        println!(
            "Rebasing '{}' onto '{}'...",
            &branch_to_merge, target_branch
        );
        info!(
            branch = branch_to_merge,
            base = target_branch,
            "merge:rebase start"
        );
        git::rebase_branch_onto_base(&worktree_path, target_branch).with_context(|| {
            format!(
                "Rebase failed, likely due to conflicts.\n\n\
                Please resolve them manually inside the worktree at '{}'.\n\
                Then, run 'git rebase --continue' to proceed or 'git rebase --abort' to cancel.",
                worktree_path.display()
            )
        })?;

        // After a successful rebase, merge into target. This will be a fast-forward.
        git::merge_in_worktree(&target_worktree_path, branch_to_merge)
            .context("Failed to merge rebased branch. This should have been a fast-forward.")?;
        info!(branch = branch_to_merge, "merge:fast-forward complete");
    } else if squash {
        // Perform the squash merge. This stages all changes from the feature branch but does not commit.
        if let Err(e) = git::merge_squash_in_worktree(&target_worktree_path, branch_to_merge) {
            info!(branch = branch_to_merge, error = %e, "merge:squash merge failed, resetting target worktree");
            // Best effort to reset; ignore failure as the user message is the priority.
            let _ = git::reset_hard(&target_worktree_path);
            return Err(conflict_err(branch_to_merge));
        }

        // Prompt the user to provide a commit message for the squashed changes.
        println!("Staged squashed changes. Please provide a commit message in your editor.");
        git::commit_with_editor(&target_worktree_path)
            .context("Failed to commit squashed changes. You may need to commit them manually.")?;
        info!(branch = branch_to_merge, "merge:squash merge committed");
    } else {
        // Default merge commit workflow
        if let Err(e) = git::merge_in_worktree(&target_worktree_path, branch_to_merge) {
            info!(branch = branch_to_merge, error = %e, "merge:standard merge failed, aborting merge in target worktree");
            // Best effort to abort; ignore failure as the user message is the priority.
            let _ = git::abort_merge_in_worktree(&target_worktree_path);
            return Err(conflict_err(branch_to_merge));
        }
        info!(branch = branch_to_merge, "merge:standard merge complete");
    }

    // Skip cleanup if --keep flag is used
    if keep {
        info!(branch = branch_to_merge, "merge:skipping cleanup (--keep)");
        return Ok(MergeResult {
            branch_merged: branch_to_merge.to_string(),
            main_branch: target_branch.to_string(),
            had_staged_changes,
        });
    }

    // Always force cleanup after a successful merge
    info!(branch = branch_to_merge, "merge:cleanup start");
    let cleanup_result = cleanup::cleanup(
        context,
        branch_to_merge,
        handle,
        &worktree_path,
        true,
        false, // keep_branch: always delete when merging
    )?;

    // Navigate to the target branch window and close the source window
    cleanup::navigate_to_target_and_close(
        &context.prefix,
        &target_window_name,
        handle,
        &cleanup_result,
    )?;

    Ok(MergeResult {
        branch_merged: branch_to_merge.to_string(),
        main_branch: target_branch.to_string(),
        had_staged_changes,
    })
}
