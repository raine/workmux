use anyhow::{Context, Result, anyhow};
use std::path::Path;

use crate::{git, prompt::Prompt, tmux};
use tracing::{debug, info, warn};

use super::cleanup;
use super::context::WorkflowContext;
use super::setup;
use super::types::{CreateResult, SetupOptions};

/// Create a new worktree with tmux window and panes
///
/// # Arguments
/// * `branch_name` - The git branch name (used for git operations)
/// * `handle` - The display name for worktree directory and tmux window
/// * `base_branch` - Optional base branch/commit/tag to branch from
/// * `remote_branch` - Optional remote branch reference
/// * `prompt` - Optional prompt for AI agents
/// * `context` - Workflow context with config and repo info
/// * `options` - Setup options (hooks, file ops, etc.)
/// * `agent` - Optional agent override
#[allow(clippy::too_many_arguments)]
pub fn create(
    branch_name: &str,
    handle: &str,
    base_branch: Option<&str>,
    remote_branch: Option<&str>,
    prompt: Option<&Prompt>,
    context: &WorkflowContext,
    options: SetupOptions,
    agent: Option<&str>,
) -> Result<CreateResult> {
    info!(
        branch = branch_name,
        handle = handle,
        base = ?base_branch,
        remote = ?remote_branch,
        "create:start"
    );

    // Validate pane config before any other operations
    if let Some(panes) = &context.config.panes {
        crate::config::validate_panes_config(panes)?;
    }

    // Pre-flight checks
    context.ensure_tmux_running()?;

    // Check tmux window using handle (the display name)
    if tmux::window_exists(&context.prefix, handle)? {
        return Err(anyhow!(
            "A tmux window named '{}{}' already exists",
            context.prefix,
            handle
        ));
    }

    // Check if branch already has a worktree
    if git::worktree_exists(branch_name)? {
        return Err(anyhow!(
            "A worktree for branch '{}' already exists. Use 'workmux open {}' to open it.",
            branch_name,
            branch_name
        ));
    }

    // Auto-detect: create branch if it doesn't exist
    let branch_exists = git::branch_exists(branch_name)?;
    if branch_exists && remote_branch.is_some() {
        return Err(anyhow!(
            "Branch '{}' already exists. Remove '--remote' or pick a different branch name.",
            branch_name
        ));
    }
    let create_new = !branch_exists;
    let mut track_upstream = false;
    debug!(
        branch = branch_name,
        branch_exists, create_new, "create:branch detection"
    );

    // Determine the base for the new branch
    let base_branch_for_creation = if let Some(remote_spec) = remote_branch {
        let spec = git::parse_remote_branch_spec(remote_spec)?;
        if !git::remote_exists(&spec.remote)? {
            return Err(anyhow!(
                "Remote '{}' does not exist. Available remotes: {:?}",
                spec.remote,
                git::list_remotes()?
            ));
        }
        git::fetch_remote(&spec.remote)
            .with_context(|| format!("Failed to fetch from remote '{}'", spec.remote))?;
        let remote_ref = format!("{}/{}", spec.remote, spec.branch);
        if !git::branch_exists(&remote_ref)? {
            return Err(anyhow!(
                "Remote branch '{}' was not found. Double-check the name or fetch it manually.",
                remote_ref
            ));
        }
        track_upstream = true;
        Some(remote_ref)
    } else if create_new {
        if let Some(base) = base_branch {
            // Use the explicitly provided base branch/commit/tag
            Some(base.to_string())
        } else {
            // Default to the current branch when no explicit base was provided
            let current_branch = git::get_current_branch()
                .context("Failed to determine the current branch to use as the base")?;
            let current_branch = current_branch.trim().to_string();

            if current_branch.is_empty() {
                return Err(anyhow!(
                    "Cannot determine current branch (detached HEAD). \
                     Use --base to explicitly specify the starting point."
                ));
            }

            Some(current_branch)
        }
    } else {
        None
    };

    // Determine worktree path: use config.worktree_dir or default to <project>__worktrees pattern
    // Always use main_worktree_root (not repo_root) to ensure consistent paths even when
    // running from inside an existing worktree.
    let base_dir = if let Some(ref worktree_dir) = context.config.worktree_dir {
        let path = Path::new(worktree_dir);
        if path.is_absolute() {
            // Use absolute path as-is
            path.to_path_buf()
        } else {
            // Relative path: resolve from main worktree root
            context.main_worktree_root.join(path)
        }
    } else {
        // Default behavior: <main_worktree_root>/../<project_name>__worktrees
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
    // Use handle for the worktree directory name (not branch_name)
    let worktree_path = base_dir.join(handle);

    // Check if path already exists (handle collision detection)
    if worktree_path.exists() {
        return Err(anyhow!(
            "Worktree directory '{}' already exists.\n\
             This may be from another branch with the same handle.\n\
             Hint: Use --name to specify a different name.",
            worktree_path.display()
        ));
    }

    // Create worktree
    info!(
        branch = branch_name,
        path = %worktree_path.display(),
        create_new,
        base = ?base_branch_for_creation,
        "create:creating worktree"
    );

    git::create_worktree(
        &worktree_path,
        branch_name,
        create_new,
        base_branch_for_creation.as_deref(),
        track_upstream,
    )
    .context("Failed to create git worktree")?;

    // Store the base branch in git config for future reference (used during removal checks)
    if let Some(ref base) = base_branch_for_creation {
        git::set_branch_base(branch_name, base).with_context(|| {
            format!(
                "Failed to store base branch '{}' for branch '{}'",
                base, branch_name
            )
        })?;
        debug!(
            branch = branch_name,
            base = base,
            "create:stored base branch in git config"
        );
    }

    // Setup the rest of the environment (tmux, files, hooks)
    let prompt_file_path = if let Some(p) = prompt {
        Some(setup::write_prompt_file(branch_name, p)?)
    } else {
        None
    };

    // Merge prompt file path into options
    let options_with_prompt = SetupOptions {
        prompt_file_path,
        ..options
    };
    let mut result = setup::setup_environment(
        branch_name,
        handle,
        &worktree_path,
        &context.config,
        &options_with_prompt,
        agent,
    )?;
    result.base_branch = base_branch_for_creation.clone();
    info!(
        branch = branch_name,
        path = %result.worktree_path.display(),
        hooks_run = result.post_create_hooks_run,
        "create:completed"
    );
    Ok(result)
}

/// Create a new worktree and move uncommitted changes from the current worktree into it.
pub fn create_with_changes(
    branch_name: &str,
    handle: &str,
    include_untracked: bool,
    patch: bool,
    context: &WorkflowContext,
    options: SetupOptions,
) -> Result<CreateResult> {
    info!(
        branch = branch_name,
        handle = handle,
        include_untracked,
        patch,
        "create_with_changes:start"
    );

    // Capture the current working directory, which is the worktree with the changes.
    let original_worktree_path = std::env::current_dir()
        .context("Failed to get current working directory to rescue changes from")?;

    // Check for changes based on the include_untracked flag
    let has_tracked_changes = git::has_tracked_changes(&original_worktree_path)?;
    let has_movable_untracked =
        include_untracked && git::has_untracked_files(&original_worktree_path)?;

    if !has_tracked_changes && !has_movable_untracked {
        return Err(anyhow!(
            "No uncommitted changes to move. Use 'workmux add {}' to create a clean worktree.",
            branch_name
        ));
    }

    if git::branch_exists(branch_name)? {
        return Err(anyhow!("Branch '{}' already exists.", branch_name));
    }

    // 1. Stash changes
    let stash_message = format!("workmux: moving changes to {}", branch_name);
    git::stash_push(&stash_message, include_untracked, patch)
        .context("Failed to stash current changes")?;
    info!(branch = branch_name, "create_with_changes: changes stashed");

    // 2. Create new worktree
    let create_result = match create(
        branch_name,
        handle,
        None,
        None,
        None,
        context,
        options,
        None,
    ) {
        Ok(result) => result,
        Err(e) => {
            warn!(error = %e, "create_with_changes: worktree creation failed, popping stash");
            // Best effort to restore the stash - if this fails, user still has stash@{0}
            let _ = git::stash_pop(&original_worktree_path);
            return Err(e).context(
                "Failed to create new worktree. Stashed changes have been restored if possible.",
            );
        }
    };

    let new_worktree_path = &create_result.worktree_path;
    info!(
        path = %new_worktree_path.display(),
        "create_with_changes: worktree created"
    );

    // 3. Apply stash in new worktree
    match git::stash_pop(new_worktree_path) {
        Ok(_) => {
            // 4. Success: Clean up original worktree
            info!("create_with_changes: stash applied successfully, cleaning original worktree");
            git::reset_hard(&original_worktree_path)?;

            info!(
                branch = branch_name,
                "create_with_changes: completed successfully"
            );
            Ok(create_result)
        }
        Err(e) => {
            // 5. Failure: Rollback
            warn!(error = %e, "create_with_changes: failed to apply stash, rolling back");

            let cleanup_result = cleanup::cleanup(
                context,
                branch_name,
                handle,
                &create_result.worktree_path,
                true,  // force
                false, // delete_remote
                false, // keep_branch
            )
            .context(
                "Rollback failed: could not clean up the new worktree. Please do so manually.",
            )?;

            // Handle tmux window navigation/closing based on whether we're inside the target window
            cleanup::navigate_to_main_and_close(
                &context.prefix,
                &context.main_branch,
                handle,
                &cleanup_result,
            )?;

            Err(anyhow!(
                "Could not apply changes to '{}', likely due to conflicts.\n\n\
                The new worktree has been removed.\n\
                Your changes are safe in the latest stash. Run 'git stash pop' manually to resolve.",
                branch_name
            ))
        }
    }
}
