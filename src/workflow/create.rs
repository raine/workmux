use anyhow::{Context, Result, anyhow};
use std::path::Path;

use crate::config::MuxMode;
use crate::multiplexer::MuxHandle;
use crate::{git, spinner};
use tracing::{debug, info, warn};

/// Check if a path is registered as a git worktree.
/// Uses canonicalize() to handle symlinks, case sensitivity, and relative paths.
fn is_registered_worktree(path: &Path) -> Result<bool> {
    // Canonicalize the input path for reliable comparison
    let abs_path = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return Ok(false), // Can't canonicalize = not a valid worktree
    };

    let worktrees = git::list_worktrees()?;
    for (wt_path, _) in worktrees {
        // Canonicalize git's reported path as well
        if let Ok(abs_wt) = std::fs::canonicalize(&wt_path) {
            if abs_wt == abs_path {
                return Ok(true);
            }
        } else if wt_path == path {
            // Fallback to string comparison if canonicalization fails
            return Ok(true);
        }
    }
    Ok(false)
}

use super::cleanup;
use super::context::WorkflowContext;
use super::setup;
use super::types::{CreateArgs, CreateResult, SetupOptions};

/// Create a new worktree with tmux window and panes
pub fn create(context: &WorkflowContext, args: CreateArgs) -> Result<CreateResult> {
    let CreateArgs {
        branch_name,
        handle,
        base_branch,
        remote_branch,
        prompt,
        options,
        agent,
        is_explicit_name,
    } = args;

    info!(
        branch = branch_name,
        handle = handle,
        base = ?base_branch,
        remote = ?remote_branch,
        "create:start"
    );

    // Validate layout config before any other operations
    if context.config.panes.is_some() && context.config.windows.is_some() {
        anyhow::bail!("Cannot specify both 'panes' and 'windows' in configuration.");
    }
    if let Some(windows) = &context.config.windows {
        if options.mode != MuxMode::Session {
            anyhow::bail!(
                "'windows' configuration requires 'mode: session'. \
                 Either add 'mode: session' to your config or use --session flag."
            );
        }
        crate::config::validate_windows_config(windows)?;
    }
    if let Some(panes) = &context.config.panes {
        crate::config::validate_panes_config(panes)?;
    }

    // Pre-flight checks
    context.ensure_mux_running()?;

    // Validate backend supports session mode before creating any git state
    if options.mode == MuxMode::Session && context.mux.name() != "tmux" {
        return Err(anyhow!(
            "Session mode (--session) is only supported with tmux.\n\
             Current backend: {}. Use window mode instead.",
            context.mux.name()
        ));
    }

    // Check if worktree or target (window/session) already exists
    let target = MuxHandle::new(context.mux.as_ref(), options.mode, &context.prefix, handle);
    let full_target_name = target.full_name();
    let mut target_exists = target.exists()?;
    let worktree_exists = git::worktree_exists(branch_name)?;

    // Detect cross-repo collision: mux target exists but local worktree does not.
    // This means the target belongs to a different repository. Auto-suffix with the
    // project directory name to avoid the collision. We use a non-numeric suffix so
    // cleanup's `find_matching_windows` regex (base(-\d+)?) won't confuse it with
    // `open --new` duplicates.
    let mut current_handle = handle.to_string();
    if target_exists && !worktree_exists && !is_explicit_name {
        let project_name = context
            .main_worktree_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        let mut project_slug = slug::slugify(project_name);
        // Guard against empty slugs (e.g., project dir "___") and purely numeric
        // slugs (e.g., "123") which would match cleanup's `base(-\d+)?` regex and
        // cause `wm rm` in one repo to kill the other repo's window.
        if project_slug.is_empty() || project_slug.chars().all(|c| c.is_ascii_digit()) {
            project_slug = format!(
                "repo-{}",
                if project_slug.is_empty() {
                    "unnamed"
                } else {
                    &project_slug
                }
            );
        }
        current_handle = format!("{}-{}", handle, project_slug);

        let suffixed_target = MuxHandle::new(
            context.mux.as_ref(),
            options.mode,
            &context.prefix,
            &current_handle,
        );

        eprintln!(
            "workmux: {} '{}' exists in another repository, using '{}'",
            target.kind(),
            full_target_name,
            suffixed_target.full_name()
        );

        target_exists = suffixed_target.exists()?;
    }

    // If open_if_exists is set and either exists, delegate to open workflow
    if options.open_if_exists && (target_exists || worktree_exists) {
        debug!(
            branch = branch_name,
            handle = handle,
            target_exists,
            worktree_exists,
            "create:delegating to open (open_if_exists=true)"
        );

        // Create open options - don't run hooks or file ops since this is an existing worktree.
        // Pane commands are handled by the open workflow: if the window exists it just switches,
        // if not it creates the window and runs pane commands.
        let open_options = SetupOptions {
            run_hooks: false,
            run_file_ops: false,
            run_pane_commands: options.run_pane_commands,
            prompt_file_path: options.prompt_file_path.clone(),
            focus_window: options.focus_window,
            working_dir: options.working_dir.clone(),
            config_root: options.config_root.clone(),
            open_if_exists: false,
            mode: options.mode,
        };

        return super::open::open(branch_name, context, open_options, false, false);
    }

    // Check target using handle (the display name)
    if target_exists {
        return Err(anyhow!(
            "A {} {} named '{}' already exists.\n\
             Hint: use --name to specify a unique name.",
            context.mux.name(),
            target.kind(),
            MuxHandle::new(
                context.mux.as_ref(),
                options.mode,
                &context.prefix,
                &current_handle,
            )
            .full_name()
        ));
    }

    // Check if branch already has a worktree
    if worktree_exists {
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
        spinner::with_spinner(&format!("Fetching from '{}'", spec.remote), || {
            git::fetch_remote(&spec.remote)
        })
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
    // Use current_handle for the worktree directory name (may be suffixed for cross-repo collision)
    let worktree_path = base_dir.join(&current_handle);

    // Check if path already exists (handle collision detection)
    if worktree_path.exists() {
        // Check if this is an orphan directory (exists on disk but not registered with git).
        // This can happen when cleanup renames a worktree but a background process (build tool,
        // file watcher, shell prompt) recreates the directory structure using stale $PWD.
        if is_registered_worktree(&worktree_path)? {
            return Err(anyhow!(
                "Worktree directory '{}' already exists and is registered with git.\n\
                 This may be from another branch with the same handle.\n\
                 Hint: Use --name to specify a different name.",
                worktree_path.display()
            ));
        }

        // Safety check: if the directory contains a .git file/folder, it might be a
        // corrupted worktree or a manual clone. Don't auto-delete to prevent data loss.
        if worktree_path.join(".git").exists() {
            return Err(anyhow!(
                "Directory '{}' exists and contains a .git resource, but is not registered.\n\
                 This looks like a repository or worktree with corrupted metadata.\n\
                 Please remove it manually to prevent data loss.",
                worktree_path.display()
            ));
        }

        // It's an orphan directory (not registered with git) - safe to remove.
        // This typically happens when cleanup renames a worktree but a background process
        // (build tool, file watcher) recreates files using stale $PWD paths.
        // Since it's not a registered worktree, any files are just build artifacts.
        info!(
            path = %worktree_path.display(),
            "create:removing orphan directory from previous cleanup"
        );
        std::fs::remove_dir_all(&worktree_path).with_context(|| {
            format!(
                "Failed to remove orphan directory '{}'. Please remove it manually.",
                worktree_path.display()
            )
        })?;
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

    // Store the tmux mode in git config for cleanup operations
    // This allows remove/close/merge to know whether to kill a window or session
    if options.mode == MuxMode::Session {
        git::set_worktree_meta(&current_handle, "mode", "session").with_context(|| {
            format!(
                "Failed to store tmux mode for worktree '{}'",
                current_handle
            )
        })?;
        debug!(
            handle = %current_handle,
            mode = "session",
            "create:stored tmux mode in git config"
        );
    }

    // Setup the rest of the environment (tmux, files, hooks)
    let prompt_file_path = if let Some(p) = prompt {
        Some(setup::write_prompt_file(
            Some(&worktree_path),
            branch_name,
            p,
        )?)
    } else {
        None
    };

    // Compute working directory from config location
    let working_dir = if !context.config_rel_dir.as_os_str().is_empty() {
        let subdir_in_worktree = worktree_path.join(&context.config_rel_dir);
        // Only use subdir if it exists (may not exist if base branch lacks it)
        if subdir_in_worktree.exists() {
            Some(subdir_in_worktree)
        } else {
            debug!(
                subdir = %context.config_rel_dir.display(),
                "create:config subdir does not exist in worktree, falling back to root"
            );
            None
        }
    } else {
        None
    };

    // Use config_source_dir for file operations (the directory where config was found)
    let config_root = if !context.config_rel_dir.as_os_str().is_empty() {
        Some(context.config_source_dir.clone())
    } else {
        None
    };

    // Merge options
    let options_with_prompt = SetupOptions {
        prompt_file_path,
        working_dir,
        config_root,
        ..options
    };
    let mut result = setup::setup_environment(
        context.mux.as_ref(),
        branch_name,
        &current_handle,
        &worktree_path,
        &context.config,
        &options_with_prompt,
        agent,
        None,
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

    // Capture mode before moving options (needed for rollback cleanup)
    let mode = options.mode;

    // 2. Create new worktree
    let create_result = match create(
        context,
        CreateArgs {
            branch_name,
            handle,
            base_branch: None,
            remote_branch: None,
            prompt: None,
            options,
            agent: None,
            is_explicit_name: false,
        },
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
                &create_result.resolved_handle,
                &create_result.worktree_path,
                true,  // force
                false, // keep_branch
                false, // no_hooks: run hooks normally for rollback
            )
            .context(
                "Rollback failed: could not clean up the new worktree. Please do so manually.",
            )?;

            // Handle window navigation/closing based on whether we're inside the source window
            cleanup::navigate_to_target_and_close(
                context.mux.as_ref(),
                &context.prefix,
                &context.main_branch,
                &create_result.resolved_handle,
                &cleanup_result,
                mode,
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
