use anyhow::{Context, Result, anyhow};

use crate::git;
use tracing::{debug, info};

use super::cleanup;
use super::context::WorkflowContext;
use super::types::MergeResult;

/// Merge a branch into the main branch and clean up
pub fn merge(
    branch_name: &str,
    ignore_uncommitted: bool,
    rebase: bool,
    squash: bool,
    keep: bool,
    context: &WorkflowContext,
) -> Result<MergeResult> {
    info!(
        branch = branch_name,
        ignore_uncommitted, rebase, squash, keep, "merge:start"
    );

    // Change CWD to main worktree to prevent errors if the command is run from within
    // the worktree that is about to be deleted.
    context.chdir_to_main_worktree()?;

    let branch_to_merge = branch_name;

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

    if branch_to_merge == context.main_branch {
        return Err(anyhow!("Cannot merge the main branch into itself."));
    }
    debug!(
        branch = branch_to_merge,
        main = &context.main_branch,
        "merge:main branch resolved"
    );

    // Safety check: Abort if the main worktree has uncommitted changes
    if git::has_uncommitted_changes(&context.main_worktree_root)? {
        return Err(anyhow!(
            "Main worktree has uncommitted changes. Please commit or stash them before merging."
        ));
    }

    // Explicitly switch to the main branch to ensure correct merge target
    git::switch_branch_in_worktree(&context.main_worktree_root, &context.main_branch)?;

    // Helper closure to generate the error message for merge conflicts
    let conflict_err = |branch: &str| -> anyhow::Error {
        anyhow!(
            "Merge failed due to conflicts. Main worktree kept clean.\n\n\
            To resolve, update your branch in worktree at {}:\n\
              git rebase {}  (recommended)\n\
            Or:\n\
              git merge {}\n\n\
            After resolving conflicts, retry: workmux merge {}",
            worktree_path.display(),
            &context.main_branch,
            &context.main_branch,
            branch
        )
    };

    if rebase {
        // Rebase the feature branch on top of main inside its own worktree.
        // This is where conflicts will be detected.
        println!(
            "Rebasing '{}' onto '{}'...",
            &branch_to_merge, &context.main_branch
        );
        info!(
            branch = branch_to_merge,
            base = &context.main_branch,
            "merge:rebase start"
        );
        git::rebase_branch_onto_base(&worktree_path, &context.main_branch).with_context(|| {
            format!(
                "Rebase failed, likely due to conflicts.\n\n\
                Please resolve them manually inside the worktree at '{}'.\n\
                Then, run 'git rebase --continue' to proceed or 'git rebase --abort' to cancel.",
                worktree_path.display()
            )
        })?;

        // After a successful rebase, merge into main. This will be a fast-forward.
        git::merge_in_worktree(&context.main_worktree_root, branch_to_merge)
            .context("Failed to merge rebased branch. This should have been a fast-forward.")?;
        info!(branch = branch_to_merge, "merge:fast-forward complete");
    } else if squash {
        // Perform the squash merge. This stages all changes from the feature branch but does not commit.
        if let Err(e) = git::merge_squash_in_worktree(&context.main_worktree_root, branch_to_merge)
        {
            info!(branch = branch_to_merge, error = %e, "merge:squash merge failed, resetting main worktree");
            // Best effort to reset; ignore failure as the user message is the priority.
            let _ = git::reset_hard(&context.main_worktree_root);
            return Err(conflict_err(branch_to_merge));
        }

        // Prompt the user to provide a commit message for the squashed changes.
        println!("Staged squashed changes. Please provide a commit message in your editor.");
        git::commit_with_editor(&context.main_worktree_root)
            .context("Failed to commit squashed changes. You may need to commit them manually.")?;
        info!(branch = branch_to_merge, "merge:squash merge committed");
    } else {
        // Default merge commit workflow
        if let Err(e) = git::merge_in_worktree(&context.main_worktree_root, branch_to_merge) {
            info!(branch = branch_to_merge, error = %e, "merge:standard merge failed, aborting merge in main worktree");
            // Best effort to abort; ignore failure as the user message is the priority.
            let _ = git::abort_merge_in_worktree(&context.main_worktree_root);
            return Err(conflict_err(branch_to_merge));
        }
        info!(branch = branch_to_merge, "merge:standard merge complete");
    }

    // Skip cleanup if --keep flag is used
    if keep {
        info!(branch = branch_to_merge, "merge:skipping cleanup (--keep)");
        return Ok(MergeResult {
            branch_merged: branch_to_merge.to_string(),
            main_branch: context.main_branch.clone(),
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

    // Navigate to the main branch window and close the target window
    cleanup::navigate_to_main_and_close(
        &context.prefix,
        &context.main_branch,
        handle,
        &cleanup_result,
    )?;

    Ok(MergeResult {
        branch_merged: branch_to_merge.to_string(),
        main_branch: context.main_branch.clone(),
        had_staged_changes,
    })
}
