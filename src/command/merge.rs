use crate::config::MergeStrategy;
use crate::workflow::WorkflowContext;
use crate::{config, workflow};
use anyhow::{Context, Result};

pub fn run(
    branch_name: Option<&str>,
    into_branch: Option<&str>,
    ignore_uncommitted: bool,
    mut rebase: bool,
    mut squash: bool,
    keep: bool,
) -> Result<()> {
    let config = config::Config::load(None)?;

    // Apply default strategy from config if no CLI flags are provided
    if !rebase
        && !squash
        && let Some(strategy) = config.merge_strategy
    {
        match strategy {
            MergeStrategy::Rebase => rebase = true,
            MergeStrategy::Squash => squash = true,
            MergeStrategy::Merge => {}
        }
    }

    // Resolve branch name from argument or current branch
    // Note: Must be done BEFORE creating WorkflowContext (which may change CWD)
    let branch_to_merge = super::resolve_branch(branch_name, "merge")?;

    let context = WorkflowContext::new(config)?;

    // Only announce pre-delete hooks if we're actually going to run cleanup
    if !keep {
        super::announce_hooks(&context.config, None, super::HookPhase::PreDelete);
    }

    let result = workflow::merge(
        &branch_to_merge,
        into_branch,
        ignore_uncommitted,
        rebase,
        squash,
        keep,
        &context,
    )
    .context("Failed to merge worktree")?;

    if result.had_staged_changes {
        println!("✓ Committed staged changes");
    }

    println!(
        "Merging '{}' into '{}'...",
        result.branch_merged, result.main_branch
    );
    println!("✓ Merged '{}'", result.branch_merged);

    if keep {
        println!("Worktree, window, and branch kept");
    } else {
        println!(
            "✓ Successfully merged and cleaned up '{}'",
            result.branch_merged
        );
    }

    Ok(())
}
