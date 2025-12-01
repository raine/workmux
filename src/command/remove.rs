use crate::workflow::WorkflowContext;
use crate::{config, git, workflow};
use anyhow::{Context, Result, anyhow};
use std::io::{self, Write};

/// User's choice when prompted about unmerged commits.
enum UserChoice {
    Confirmed, // User confirmed deletion
    Aborted,   // User aborted deletion
    NotNeeded, // No prompt needed (no unmerged commits)
}

pub fn run(branch_name: Option<&str>, force: bool, keep_branch: bool) -> Result<()> {
    // Resolve branch name from argument or current branch
    let branch_to_remove = super::resolve_branch(branch_name, "remove")?;

    // Validate removal safety and get effective force flag
    let effective_force = match validate_removal_safety(&branch_to_remove, force, keep_branch)? {
        Some(force_flag) => force_flag,
        None => return Ok(()), // User aborted
    };

    let config = config::Config::load(None)?;
    let context = WorkflowContext::new(config)?;

    super::announce_hooks(&context.config, None, super::HookPhase::PreDelete);

    let result = workflow::remove(&branch_to_remove, effective_force, keep_branch, &context)
        .context("Failed to remove worktree")?;

    if keep_branch {
        println!(
            "✓ Successfully removed worktree for branch '{}'. The local branch was kept.",
            result.branch_removed
        );
    } else {
        println!(
            "✓ Successfully removed worktree and branch '{}'",
            result.branch_removed
        );
    }

    Ok(())
}

/// Validates whether it's safe to remove the branch/worktree.
/// Returns Some(force_flag) to proceed, or None if user aborted.
fn validate_removal_safety(
    branch_name: &str,
    force: bool,
    keep_branch: bool,
) -> Result<Option<bool>> {
    if force {
        return Ok(Some(true));
    }

    // First check for uncommitted changes (must be checked before unmerged prompt)
    // to avoid prompting user about unmerged commits only to error on uncommitted changes
    check_uncommitted_changes(branch_name)?;

    // Check if we need to prompt for unmerged commits (only relevant when deleting the branch)
    if !keep_branch {
        match check_unmerged_commits(branch_name)? {
            UserChoice::Confirmed => return Ok(Some(true)), // User confirmed - use force
            UserChoice::Aborted => return Ok(None),         // User aborted
            UserChoice::NotNeeded => {}                     // No unmerged commits
        }
    }

    Ok(Some(false))
}

/// Check for uncommitted changes in the worktree.
fn check_uncommitted_changes(branch_name: &str) -> Result<()> {
    let worktree_path = git::get_worktree_path(branch_name)
        .with_context(|| format!("Failed to get worktree path for branch '{}'", branch_name))?;

    if worktree_path.exists() {
        let has_changes = git::has_uncommitted_changes(&worktree_path).with_context(|| {
            format!(
                "Failed to check for uncommitted changes in worktree at '{}'",
                worktree_path.display()
            )
        })?;

        if has_changes {
            return Err(anyhow!(
                "Worktree has uncommitted changes. Use --force to delete anyway."
            ));
        }
    }

    Ok(())
}

/// Check for unmerged commits and prompt user for confirmation.
fn check_unmerged_commits(branch_name: &str) -> Result<UserChoice> {
    // Try to get the stored base branch, fall back to default branch
    let base = git::get_branch_base(branch_name)
        .ok()
        .unwrap_or_else(|| git::get_default_branch().unwrap_or_else(|_| "main".to_string()));

    // Get the merge base with fallback if the stored base is invalid
    let base_branch = match git::get_merge_base(&base) {
        Ok(b) => b,
        Err(_) => {
            let default_main = git::get_default_branch().context("Failed to get default branch")?;
            eprintln!(
                "Warning: Could not resolve base '{}'; falling back to '{}'",
                base, default_main
            );
            git::get_merge_base(&default_main)
                .with_context(|| format!("Failed to get merge base for '{}'", default_main))?
        }
    };

    let unmerged_branches = git::get_unmerged_branches(&base_branch)
        .with_context(|| format!("Failed to get unmerged branches for base '{}'", base_branch))?;

    let has_unmerged = unmerged_branches.contains(branch_name);

    if has_unmerged {
        prompt_unmerged_confirmation(branch_name, &base_branch, &base)
    } else {
        Ok(UserChoice::NotNeeded)
    }
}

/// Prompt user to confirm deletion of branch with unmerged commits.
fn prompt_unmerged_confirmation(
    branch_name: &str,
    base_branch: &str,
    base: &str,
) -> Result<UserChoice> {
    println!(
        "This will delete the worktree, tmux window, and local branch for '{}'.",
        branch_name
    );
    println!(
        "Warning: Branch '{}' has commits that are not merged into '{}' (base: '{}').",
        branch_name, base_branch, base
    );
    println!("This action cannot be undone.");
    print!("Are you sure you want to continue? [y/N] ");

    // Flush stdout to ensure the prompt is displayed before reading input
    io::stdout().flush().context("Failed to flush stdout")?;

    let mut confirmation = String::new();
    io::stdin()
        .read_line(&mut confirmation)
        .context("Failed to read user confirmation")?;

    if confirmation.trim().to_lowercase() == "y" {
        Ok(UserChoice::Confirmed)
    } else {
        println!("Aborted.");
        Ok(UserChoice::Aborted)
    }
}
