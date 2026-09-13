use anyhow::{Context, Result, anyhow};
use std::path::Path;

use crate::config::MuxMode;
use crate::multiplexer::MuxHandle;
use crate::{git, spinner};
use tracing::{debug, info, warn};

/// Check if a path is registered as a git worktree.
/// Uses canonicalize() to handle symlinks, case sensitivity, and relative paths.
fn is_registered_worktree(path: &Path, context: &WorkflowContext) -> Result<bool> {
    // Canonicalize the input path for reliable comparison
    let abs_path = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return Ok(false), // Can't canonicalize = not a valid worktree
    };

    let worktrees = git::list_worktrees_in(Some(&context.execution_dir))?;
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
use super::types::{CreateArgs, CreateResult, ProvisionResult, SetupOptions};

enum CreateOutcome {
    Attached(CreateResult),
    Provisioned(ProvisionResult),
}

/// Create a new worktree with a multiplexer target.
pub fn create(context: &WorkflowContext, args: CreateArgs) -> Result<CreateResult> {
    match create_impl(context, args, false, false)? {
        CreateOutcome::Attached(result) => Ok(result),
        CreateOutcome::Provisioned(_) => {
            unreachable!("attached creation returned provision result")
        }
    }
}

/// Create a new worktree without reading or creating multiplexer state.
pub fn create_headless(
    context: &WorkflowContext,
    args: CreateArgs,
    json_output: bool,
) -> Result<ProvisionResult> {
    match create_impl(context, args, true, json_output)? {
        CreateOutcome::Provisioned(result) => Ok(result),
        CreateOutcome::Attached(_) => unreachable!("headless creation returned attached result"),
    }
}

fn create_impl(
    context: &WorkflowContext,
    args: CreateArgs,
    headless: bool,
    json_output: bool,
) -> Result<CreateOutcome> {
    let CreateArgs {
        branch_name,
        handle,
        base_branch,
        remote_branch,
        checkout_ref,
        prompt,
        mut options,
        mode_override,
        agent,
        is_explicit_name,
        prompt_file_only,
        fork_source,
    } = args;

    info!(
        branch = branch_name,
        handle = handle,
        base = ?base_branch,
        remote = ?remote_branch,
        "create:start"
    );

    let worktree_exists = git::worktree_exists_in(branch_name, Some(&context.execution_dir))?;
    let mut current_handle = handle.to_string();
    let mut placement_window_id = None;

    if !headless {
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

        context.ensure_mux_running()?;
        if options.mode == MuxMode::Window && options.window_session_name.is_none() {
            placement_window_id =
                setup::resolve_window_placement_target(context.mux.as_ref(), &context.config)?;
        }
        if options.mode == MuxMode::Session && context.mux.name() != "tmux" {
            return Err(anyhow!(
                "Session mode (--mode session / --session) is only supported with tmux.\n\
                 Current backend: {}. Use window mode instead.",
                context.mux.name()
            ));
        }

        let requested_target_name = options.primary_mux_target_name(handle);
        let explicit_target_name = options.has_explicit_primary_mux_target();
        let target = MuxHandle::new(
            context.mux.as_ref(),
            options.mode,
            &context.prefix,
            requested_target_name,
        );
        let full_target_name = target.full_name();
        let mut target_exists = target.exists()?;
        let mut current_target_name = requested_target_name.to_string();
        if target_exists && !worktree_exists && !is_explicit_name && !explicit_target_name {
            let project_name = context
                .main_worktree_root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("repo");
            let mut project_slug = slug::slugify(project_name);
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
            current_target_name = current_handle.clone();
            let suffixed_target = MuxHandle::new(
                context.mux.as_ref(),
                options.mode,
                &context.prefix,
                &current_target_name,
            );
            eprintln!(
                "workmux: {} '{}' exists in another repository, using '{}'",
                target.kind(),
                full_target_name,
                suffixed_target.full_name()
            );
            target_exists = suffixed_target.exists()?;
        }

        if options.open_if_exists && (target_exists || worktree_exists) {
            debug!(
                branch = branch_name,
                handle = handle,
                target_exists,
                worktree_exists,
                "create:delegating to open (open_if_exists=true)"
            );
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
                target_window_name: options.target_window_name.clone(),
                target_session_name: options.target_session_name.clone(),
                window_session_name: options.window_session_name.clone(),
                window_token: options.window_token.clone(),
                primary_window: options.primary_window,
                resume_mode: options.resume_mode.clone(),
            };
            let file_only_prompt = if prompt_file_only { prompt } else { None };
            return super::open::open(
                branch_name,
                context,
                open_options,
                false,
                mode_override,
                file_only_prompt,
                agent,
            )
            .map(CreateOutcome::Attached);
        }

        if target_exists {
            return Err(anyhow!(
                "A {} {} named '{}' already exists.\n\
                 Hint: use --name or --target-name to specify a unique name.",
                context.mux.name(),
                target.kind(),
                MuxHandle::new(
                    context.mux.as_ref(),
                    options.mode,
                    &context.prefix,
                    &current_target_name,
                )
                .full_name()
            ));
        }
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
    let branch_exists = git::branch_exists_in(branch_name, Some(&context.execution_dir))?;
    if branch_exists && remote_branch.is_some() && checkout_ref.is_none() {
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
        if !git::remote_exists_in(&spec.remote, Some(&context.execution_dir))? {
            return Err(anyhow!(
                "Remote '{}' does not exist. Available remotes: {:?}",
                spec.remote,
                git::list_remotes_in(Some(&context.execution_dir))?
            ));
        }

        // Review refs on the target repository also cover deleted source branches.
        if let Some(checkout_ref) = checkout_ref {
            let pr_refspec = format!(
                "+{}:refs/remotes/{}/{}",
                checkout_ref.head_ref(),
                spec.remote,
                spec.branch
            );
            let pr_fetch = spinner::with_spinner(
                &format!("Fetching review #{} from origin", checkout_ref.number),
                || git::fetch_refspec_in("origin", &pr_refspec, Some(&context.execution_dir)),
            );
            if pr_fetch.is_err() {
                let source_refspec = format!(
                    "+refs/heads/{}:refs/remotes/{}/{}",
                    spec.branch, spec.remote, spec.branch
                );
                spinner::with_spinner(
                    &format!("Fetching branch '{}' from '{}'", spec.branch, spec.remote),
                    || {
                        git::fetch_refspec_in(
                            &spec.remote,
                            &source_refspec,
                            Some(&context.execution_dir),
                        )
                    },
                )
                .with_context(|| {
                    format!(
                        "Failed to fetch branch '{}' from remote '{}'",
                        spec.branch, spec.remote
                    )
                })?;
            }
        } else {
            spinner::with_spinner(&format!("Fetching from '{}'", spec.remote), || {
                git::fetch_remote_in(&spec.remote, Some(&context.execution_dir))
            })
            .with_context(|| format!("Failed to fetch from remote '{}'", spec.remote))?;
        }

        let remote_ref = format!("{}/{}", spec.remote, spec.branch);
        if !git::branch_exists_in(&remote_ref, Some(&context.execution_dir))? {
            return Err(anyhow!(
                "Remote branch '{}' was not found. Double-check the name or fetch it manually.",
                remote_ref
            ));
        }
        // A review checkout compares against the PR target branch, which lives
        // in the target repository (origin) even when the head is on a fork
        // remote. Refresh the target ref so the review reflects its current
        // tip instead of a stale remote-tracking ref.
        if checkout_ref.is_some()
            && let Some(base) = base_branch.filter(|base| !base.trim().is_empty())
        {
            let base_spec = git::parse_remote_branch_spec(base)
                .with_context(|| format!("Invalid review base '{}'", base))?;
            let base_refspec = format!(
                "+refs/heads/{}:refs/remotes/{}/{}",
                base_spec.branch, base_spec.remote, base_spec.branch
            );
            let base_fetch = spinner::with_spinner(
                &format!(
                    "Fetching base branch '{}' from '{}'",
                    base_spec.branch, base_spec.remote
                ),
                || {
                    git::fetch_refspec_in(
                        &base_spec.remote,
                        &base_refspec,
                        Some(&context.execution_dir),
                    )
                },
            );
            if let Err(error) = base_fetch {
                let local_ref = format!("refs/remotes/{}/{}", base_spec.remote, base_spec.branch);
                if !git::branch_exists_in(&local_ref, Some(&context.execution_dir))? {
                    return Err(error).context(format!(
                        "Failed to fetch base branch '{}' from remote '{}'",
                        base_spec.branch, base_spec.remote
                    ));
                }
                eprintln!(
                    "warning: could not refresh base branch '{}' from '{}'; using the existing local ref",
                    base_spec.branch, base_spec.remote
                );
            }
        }
        track_upstream = true;
        Some(remote_ref)
    } else if create_new {
        if let Some(base) = base_branch.filter(|base| !base.trim().is_empty()) {
            // Use the explicitly provided base branch/commit/tag
            Some(base.to_string())
        } else {
            // Default to the current branch when no explicit base was provided
            let current_branch = git::get_current_branch_in(&context.execution_dir)
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

    // The ref recorded for review comparisons. For ordinary branch creation
    // this is the same ref the worktree was populated from. PR checkouts store
    // the target branch instead, so review diffs compare the head against the
    // branch the PR would merge into.
    let comparison_base = if checkout_ref.is_some() {
        base_branch
            .filter(|base| !base.trim().is_empty())
            .map(str::to_string)
            .or_else(|| base_branch_for_creation.clone())
    } else {
        base_branch_for_creation.clone()
    };

    // Determine worktree path: use config.worktree_dir or default to <project>__worktrees pattern
    // Always use main_worktree_root (not repo_root) to ensure consistent paths even when
    // running from inside an existing worktree.
    let base_dir = if let Some(ref worktree_dir) = context.config.worktree_dir {
        crate::util::expand_worktree_dir(worktree_dir, &context.main_worktree_root)?
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
        if is_registered_worktree(&worktree_path, context)? {
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
        review_base = ?comparison_base,
        "create:creating worktree"
    );

    // Acquire an exclusive lock to serialize .git/config writes across parallel
    // workmux processes. Without this, concurrent `workmux add` commands race on
    // git's config.lock file and fail with "could not lock config file".
    let _config_lock = git::GitConfigLock::acquire(&context.git_common_dir)
        .context("Failed to acquire git config lock")?;

    // Store the review base before checkout so observers that see the worktree
    // appear on disk also see complete branch metadata.
    if let Some(ref base) = comparison_base {
        git::set_branch_base_in(branch_name, base, Some(&context.execution_dir)).with_context(
            || {
                format!(
                    "Failed to store base branch '{}' for branch '{}'",
                    base, branch_name
                )
            },
        )?;
        debug!(
            branch = branch_name,
            base = base,
            "create:stored review base in git config"
        );
    }

    git::create_worktree_in(
        &worktree_path,
        branch_name,
        create_new,
        base_branch_for_creation.as_deref(),
        track_upstream,
        Some(&context.execution_dir),
    )
    .context("Failed to create git worktree")?;

    if headless {
        if let Err(error) = git::set_worktree_attachment_in(
            &current_handle,
            git::WorktreeAttachment::Headless,
            Some(&context.execution_dir),
        ) {
            drop(_config_lock);
            let rollback = cleanup::cleanup_headless(
                context,
                branch_name,
                &current_handle,
                &worktree_path,
                cleanup::CleanupOptions {
                    force: true,
                    keep_branch: !create_new,
                    no_hooks: true,
                    show_hook_output: false,
                },
            );
            return match rollback {
                Ok(_) => Err(error),
                Err(rollback_error) => Err(error.context(format!(
                    "Metadata rollback also failed for '{}': {rollback_error:#}",
                    worktree_path.display()
                ))),
            };
        }
    } else {
        let mode_str = match options.mode {
            MuxMode::Session => "session",
            MuxMode::Window => "window",
        };
        git::set_worktree_meta_in(
            &current_handle,
            "mode",
            mode_str,
            Some(&context.execution_dir),
        )
        .with_context(|| {
            format!(
                "Failed to store tmux mode for worktree '{}'",
                current_handle
            )
        })?;
        git::set_worktree_attachment_in(
            &current_handle,
            git::WorktreeAttachment::Multiplexer,
            Some(&context.execution_dir),
        )?;
        if let Some(target_window_name) = &options.target_window_name {
            git::set_worktree_meta_in(
                &current_handle,
                "target-window",
                target_window_name,
                Some(&context.execution_dir),
            )?;
        }
        if let Some(target_session_name) = &options.target_session_name {
            git::set_worktree_meta_in(
                &current_handle,
                "target-session",
                target_session_name,
                Some(&context.execution_dir),
            )?;
        }
        if let Some(window_session_name) = &options.window_session_name {
            git::set_worktree_meta_in(
                &current_handle,
                "window-session",
                window_session_name,
                Some(&context.execution_dir),
            )?;
        }
        if options.mode == MuxMode::Window && context.mux.supports_window_ownership() {
            options.window_token = Some(git::ensure_worktree_window_token_in(
                &current_handle,
                Some(&context.execution_dir),
            )?);
            options.primary_window = true;
        }
        debug!(handle = %current_handle, mode = mode_str, "create:stored mux metadata");
    }

    // Release the config lock before proceeding to non-git operations
    // (prompt files, tmux setup, hooks, etc.)
    drop(_config_lock);

    // Fork conversation into the new worktree if requested.
    // Must happen after git::create_worktree() (path is finalized) and before
    // setup_environment() (which launches the agent with resume args).
    if let Some(fork) = fork_source {
        let session_id = fork
            .forker
            .prepare_fork(&fork.session, &worktree_path)
            .context("Failed to fork conversation into new worktree")?;
        options.resume_mode =
            crate::multiplexer::types::ResumeMode::ForkSession(session_id.clone());
        info!(
            session_id = %session_id,
            target = %worktree_path.display(),
            "create:forked conversation into new worktree"
        );
    }

    // Write prompt file to worktree if provided
    let prompt_file_path = if let Some(p) = prompt {
        Some(setup::write_prompt_file(
            Some(&worktree_path),
            branch_name,
            p,
        )?)
    } else {
        None
    };

    // In file-only mode, the prompt file is written but not passed to setup.
    // This skips agent validation and prompt injection into pane commands.
    let setup_prompt_file_path = if prompt_file_only {
        None
    } else {
        prompt_file_path
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
    let config_root = Some(context.config_source_dir.clone());

    // Merge options
    let options_with_prompt = SetupOptions {
        prompt_file_path: setup_prompt_file_path,
        working_dir,
        config_root,
        ..options
    };
    if headless {
        let hook_output = if json_output {
            crate::cmd::ShellOutput::RedirectToStderr
        } else {
            crate::cmd::ShellOutput::Inherit
        };
        let provisioned = match setup::provision_environment(
            branch_name,
            &current_handle,
            &worktree_path,
            &context.config,
            &options_with_prompt,
            hook_output,
        ) {
            Ok(result) => result,
            Err(error) => {
                let rollback = cleanup::cleanup_headless(
                    context,
                    branch_name,
                    &current_handle,
                    &worktree_path,
                    cleanup::CleanupOptions {
                        force: true,
                        keep_branch: !create_new,
                        no_hooks: true,
                        show_hook_output: false,
                    },
                );
                return match rollback {
                    Ok(_) => Err(error),
                    Err(rollback_error) => Err(error.context(format!(
                        "Provisioning rollback also failed for '{}': {rollback_error:#}",
                        worktree_path.display()
                    ))),
                };
            }
        };
        let result = ProvisionResult {
            worktree_path,
            working_directory: provisioned.working_directory,
            branch_name: branch_name.to_string(),
            post_create_hooks_run: provisioned.post_create_hooks_run,
            base_branch: comparison_base,
            resolved_handle: current_handle,
        };
        return Ok(CreateOutcome::Provisioned(result));
    }

    let mut result = setup::setup_environment(
        context.mux.as_ref(),
        branch_name,
        &current_handle,
        &worktree_path,
        &context.config,
        &options_with_prompt,
        agent,
        placement_window_id,
    )?;
    result.base_branch = comparison_base;
    info!(
        branch = branch_name,
        path = %result.worktree_path.display(),
        hooks_run = result.post_create_hooks_run,
        "create:completed"
    );
    Ok(CreateOutcome::Attached(result))
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
            checkout_ref: None,
            prompt: None,
            options,
            mode_override: None,
            agent: None,
            is_explicit_name: false,
            prompt_file_only: false,
            fork_source: None,
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
                cleanup::CleanupOptions {
                    force: true,
                    keep_branch: false,
                    no_hooks: false,
                    show_hook_output: true,
                },
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
                context.config.default_session(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::multiplexer::types::{
        CreateSessionParams, CreateWindowInSessionParams, CreateWindowParams, LivePaneInfo,
        PaneSetupOptions, PaneSetupResult,
    };
    use crate::multiplexer::{Multiplexer, PaneHandshake};
    use crate::test_support;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    struct TestMux;

    impl Multiplexer for TestMux {
        fn name(&self) -> &'static str {
            "tmux"
        }

        fn is_running(&self) -> Result<bool> {
            Ok(true)
        }

        fn current_pane_id(&self) -> Option<String> {
            None
        }

        fn active_pane_id(&self) -> Option<String> {
            None
        }

        fn get_client_active_pane_path(&self) -> Result<PathBuf> {
            Ok(PathBuf::new())
        }

        fn create_window(&self, _params: CreateWindowParams) -> Result<String> {
            Ok("pane-1".to_string())
        }

        fn create_session(&self, _params: CreateSessionParams) -> Result<String> {
            Ok("pane-1".to_string())
        }

        fn create_window_in_session(&self, _params: CreateWindowInSessionParams) -> Result<String> {
            Ok("pane-1".to_string())
        }

        fn switch_to_session(&self, _prefix: &str, _name: &str) -> Result<()> {
            Ok(())
        }

        fn session_exists(&self, _full_name: &str) -> Result<bool> {
            Ok(false)
        }

        fn kill_session(&self, _full_name: &str) -> Result<()> {
            Ok(())
        }

        fn kill_window(&self, _full_name: &str) -> Result<()> {
            Ok(())
        }

        fn schedule_window_close(&self, _full_name: &str, _delay: Duration) -> Result<()> {
            Ok(())
        }

        fn schedule_session_close(&self, _full_name: &str, _delay: Duration) -> Result<()> {
            Ok(())
        }

        fn run_deferred_script(&self, _script: &str) -> Result<()> {
            Ok(())
        }

        fn shell_select_window_cmd(&self, _full_name: &str) -> Result<String> {
            Ok(String::new())
        }

        fn shell_kill_window_cmd(&self, _full_name: &str) -> Result<String> {
            Ok(String::new())
        }

        fn shell_switch_session_cmd(&self, _full_name: &str) -> Result<String> {
            Ok(String::new())
        }

        fn shell_kill_session_cmd(&self, _full_name: &str) -> Result<String> {
            Ok(String::new())
        }

        fn select_window(&self, _prefix: &str, _name: &str) -> Result<()> {
            Ok(())
        }

        fn window_exists(&self, _prefix: &str, _name: &str) -> Result<bool> {
            Ok(false)
        }

        fn window_exists_by_full_name(&self, _full_name: &str) -> Result<bool> {
            Ok(false)
        }

        fn current_window_name(&self) -> Result<Option<String>> {
            Ok(None)
        }

        fn get_all_window_names(&self) -> Result<HashSet<String>> {
            Ok(HashSet::new())
        }

        fn get_all_session_names(&self) -> Result<HashSet<String>> {
            Ok(HashSet::new())
        }

        fn filter_active_windows(&self, _windows: &[String]) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        fn wait_until_windows_closed(&self, _full_window_names: &[String]) -> Result<()> {
            Ok(())
        }

        fn wait_until_session_closed(&self, _full_session_name: &str) -> Result<()> {
            Ok(())
        }

        fn select_pane(&self, _pane_id: &str) -> Result<()> {
            Ok(())
        }

        fn switch_to_pane(&self, _pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
            Ok(())
        }

        fn kill_pane(&self, _pane_id: &str) -> Result<()> {
            Ok(())
        }

        fn respawn_pane(&self, pane_id: &str, _cwd: &Path, _cmd: Option<&str>) -> Result<String> {
            Ok(pane_id.to_string())
        }

        fn capture_pane(&self, _pane_id: &str, _lines: u16) -> Option<String> {
            None
        }

        fn send_text_fragment(&self, _pane_id: &str, _text: &str) -> Result<()> {
            Ok(())
        }

        fn send_enter(&self, _pane_id: &str) -> Result<()> {
            Ok(())
        }

        fn send_key(&self, _pane_id: &str, _key: &str) -> Result<()> {
            Ok(())
        }

        fn paste_text(&self, _pane_id: &str, _content: &str) -> Result<()> {
            Ok(())
        }

        fn get_default_shell(&self) -> Result<String> {
            Ok("/bin/sh".to_string())
        }

        fn create_handshake(&self) -> Result<Box<dyn PaneHandshake>> {
            Err(anyhow::anyhow!("not used"))
        }

        fn set_status(
            &self,
            _pane_id: &str,
            _icon: &str,
            _auto_clear_on_focus: bool,
        ) -> Result<()> {
            Ok(())
        }

        fn clear_status(&self, _pane_id: &str) -> Result<()> {
            Ok(())
        }

        fn ensure_status_format(&self, _pane_id: &str) -> Result<()> {
            Ok(())
        }

        fn split_pane(
            &self,
            _target_pane_id: &str,
            _direction: &crate::config::SplitDirection,
            _cwd: &Path,
            _size: Option<u16>,
            _percentage: Option<u8>,
            _command: Option<&str>,
        ) -> Result<String> {
            Ok("pane-2".to_string())
        }

        fn setup_panes(
            &self,
            initial_pane_id: &str,
            _panes: &[crate::config::PaneConfig],
            _working_dir: &Path,
            _options: PaneSetupOptions<'_>,
            _config: &Config,
            _task_agent: Option<&str>,
        ) -> Result<PaneSetupResult> {
            Ok(PaneSetupResult {
                focus_pane_id: initial_pane_id.to_string(),
                zoom_pane_id: None,
            })
        }

        fn instance_id(&self) -> String {
            "test".to_string()
        }

        fn get_live_pane_info(&self, _pane_id: &str) -> Result<Option<LivePaneInfo>> {
            Ok(None)
        }

        fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
            Ok(HashMap::new())
        }
    }

    #[test]
    fn review_checkout_fallback_fetches_the_source_branch_explicitly() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&source);
        test_support::run_git(&source, &["checkout", "-b", "review-source"]);
        std::fs::write(source.join("review.txt"), "review\n").unwrap();
        test_support::run_git(&source, &["add", "review.txt"]);
        test_support::run_git(&source, &["commit", "-m", "review source"]);

        test_support::init_repo(&repo);
        test_support::run_git(
            &repo,
            &["remote", "add", "origin", source.to_str().unwrap()],
        );
        test_support::run_git(
            &repo,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/main:refs/remotes/origin/main",
            ],
        );

        let ctx =
            WorkflowContext::new_in(&repo, Config::default(), Arc::new(TestMux), None).unwrap();
        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;
        let result = create(
            &ctx,
            CreateArgs {
                branch_name: "review-source",
                handle: "review-source",
                base_branch: None,
                remote_branch: Some("origin/review-source"),
                checkout_ref: Some(super::super::pr::CheckoutRef {
                    number: 999,
                    forge: super::super::pr::Forge::Github,
                }),
                prompt: None,
                options,
                mode_override: None,
                agent: None,
                is_explicit_name: false,
                prompt_file_only: false,
                fork_source: None,
            },
        )
        .unwrap();

        assert!(result.worktree_path.join("review.txt").exists());
        assert!(git::branch_exists_in("origin/review-source", Some(&repo)).unwrap());
    }

    #[test]
    fn review_checkout_fallback_rejects_a_stale_source_ref() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&source);
        test_support::run_git(&source, &["checkout", "-b", "review-source"]);
        std::fs::write(source.join("review.txt"), "review\n").unwrap();
        test_support::run_git(&source, &["add", "review.txt"]);
        test_support::run_git(&source, &["commit", "-m", "review source"]);

        test_support::init_repo(&repo);
        test_support::run_git(
            &repo,
            &["remote", "add", "origin", source.to_str().unwrap()],
        );
        test_support::run_git(
            &repo,
            &[
                "fetch",
                "origin",
                "+refs/heads/review-source:refs/remotes/origin/review-source",
            ],
        );
        test_support::run_git(&source, &["checkout", "main"]);
        test_support::run_git(&source, &["branch", "-D", "review-source"]);

        let ctx =
            WorkflowContext::new_in(&repo, Config::default(), Arc::new(TestMux), None).unwrap();
        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;
        let error = match create(
            &ctx,
            CreateArgs {
                branch_name: "review-source",
                handle: "review-source",
                base_branch: None,
                remote_branch: Some("origin/review-source"),
                checkout_ref: Some(super::super::pr::CheckoutRef {
                    number: 999,
                    forge: super::super::pr::Forge::Github,
                }),
                prompt: None,
                options,
                mode_override: None,
                agent: None,
                is_explicit_name: false,
                prompt_file_only: false,
                fork_source: None,
            },
        ) {
            Ok(_) => panic!("stale source ref should not be accepted"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("Failed to fetch branch 'review-source' from remote 'origin'")
        );
        assert!(!git::branch_exists_in("review-source", Some(&repo)).unwrap());
    }

    #[test]
    fn review_checkout_records_target_base_and_checks_out_head() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&source);
        test_support::run_git(&source, &["checkout", "-b", "feature"]);
        std::fs::write(source.join("feature.txt"), "feature\n").unwrap();
        test_support::run_git(&source, &["add", "feature.txt"]);
        test_support::run_git(&source, &["commit", "-m", "feature change"]);
        let feature_commit = test_support::run_git_output(&source, &["rev-parse", "HEAD"]);
        // Advance the target branch after the feature branched off so a stale
        // remote-tracking ref would point at the wrong commit.
        test_support::run_git(&source, &["checkout", "main"]);
        std::fs::write(source.join("main.txt"), "main moved\n").unwrap();
        test_support::run_git(&source, &["add", "main.txt"]);
        test_support::run_git(&source, &["commit", "-m", "advance main"]);
        let main_commit = test_support::run_git_output(&source, &["rev-parse", "HEAD"]);
        test_support::run_git(&source, &["checkout", "feature"]);

        test_support::init_repo(&repo);
        test_support::run_git(
            &repo,
            &["remote", "add", "origin", source.to_str().unwrap()],
        );
        assert!(!git::branch_exists_in("origin/main", Some(&repo)).unwrap());

        let ctx =
            WorkflowContext::new_in(&repo, Config::default(), Arc::new(TestMux), None).unwrap();
        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;
        let result = create(
            &ctx,
            CreateArgs {
                branch_name: "feature",
                handle: "feature",
                base_branch: Some("origin/main"),
                remote_branch: Some("origin/feature"),
                checkout_ref: Some(super::super::pr::CheckoutRef {
                    number: 999,
                    forge: super::super::pr::Forge::Github,
                }),
                prompt: None,
                options,
                mode_override: None,
                agent: None,
                is_explicit_name: false,
                prompt_file_only: false,
                fork_source: None,
            },
        )
        .unwrap();

        // The worktree is populated from the PR head...
        assert_eq!(
            std::fs::read_to_string(result.worktree_path.join("feature.txt")).unwrap(),
            "feature\n"
        );
        assert_eq!(
            test_support::run_git_output(&result.worktree_path, &["rev-parse", "HEAD"]),
            feature_commit
        );
        // ...while the review base records the target branch.
        assert_eq!(result.base_branch.as_deref(), Some("origin/main"));
        assert_eq!(
            git::get_branch_base_in("feature", Some(&repo)).unwrap(),
            "origin/main"
        );
        // The missing target ref was fetched and reflects the current target tip.
        assert_eq!(
            test_support::run_git_output(&result.worktree_path, &["rev-parse", "origin/main"]),
            main_commit
        );
        let diff = test_support::run_git_output(
            &result.worktree_path,
            &["diff", "--name-only", "origin/main...HEAD"],
        );
        assert!(diff.contains("feature.txt"), "unexpected diff: {diff}");
    }

    #[test]
    fn review_checkout_rejects_missing_target_branch() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&source);
        test_support::run_git(&source, &["checkout", "-b", "feature"]);
        std::fs::write(source.join("feature.txt"), "feature\n").unwrap();
        test_support::run_git(&source, &["add", "feature.txt"]);
        test_support::run_git(&source, &["commit", "-m", "feature change"]);
        test_support::run_git(&source, &["checkout", "main"]);

        test_support::init_repo(&repo);
        test_support::run_git(
            &repo,
            &["remote", "add", "origin", source.to_str().unwrap()],
        );

        let ctx =
            WorkflowContext::new_in(&repo, Config::default(), Arc::new(TestMux), None).unwrap();
        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;
        let error = match create(
            &ctx,
            CreateArgs {
                branch_name: "feature",
                handle: "feature",
                base_branch: Some("origin/missing-base"),
                remote_branch: Some("origin/feature"),
                checkout_ref: Some(super::super::pr::CheckoutRef {
                    number: 999,
                    forge: super::super::pr::Forge::Github,
                }),
                prompt: None,
                options,
                mode_override: None,
                agent: None,
                is_explicit_name: false,
                prompt_file_only: false,
                fork_source: None,
            },
        ) {
            Ok(_) => panic!("missing target branch should not be accepted"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("Failed to fetch base branch 'missing-base' from remote 'origin'")
        );
    }

    #[test]
    fn workflow_create_uses_explicit_repo_not_process_cwd() {
        const TEST_NAME: &str =
            "workflow::create::tests::workflow_create_uses_explicit_repo_not_process_cwd";
        if !test_support::is_isolated_child(TEST_NAME) {
            let temp = tempfile::tempdir().unwrap();
            let repo_a = temp.path().join("repo-a");
            let repo_b = temp.path().join("repo-b");
            let non_repo = temp.path().join("not-a-repo");
            std::fs::create_dir_all(&repo_a).unwrap();
            std::fs::create_dir_all(&repo_b).unwrap();
            std::fs::create_dir_all(&non_repo).unwrap();
            test_support::init_repo(&repo_a);
            test_support::init_repo(&repo_b);

            test_support::run_isolated_test(TEST_NAME, &non_repo, &[("WM_TEST_TEMP", temp.path())]);
            return;
        }

        println!("{}", test_support::ISOLATED_TEST_CANARY);
        let temp = std::env::var_os("WM_TEST_TEMP").map(PathBuf::from).unwrap();
        let repo_a = temp.join("repo-a");
        let repo_b = temp.join("repo-b");
        let non_repo = temp.join("not-a-repo");
        assert_eq!(
            std::env::current_dir().unwrap(),
            non_repo.canonicalize().unwrap()
        );

        let config = Config::default();
        let ctx =
            WorkflowContext::new_in(&repo_b, config.clone(), Arc::new(TestMux), None).unwrap();
        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;
        let result = create(
            &ctx,
            CreateArgs {
                branch_name: "feature",
                handle: "feature",
                base_branch: Some("main"),
                remote_branch: None,
                checkout_ref: None,
                prompt: None,
                options,
                mode_override: None,
                agent: None,
                is_explicit_name: false,
                prompt_file_only: false,
                fork_source: None,
            },
        )
        .unwrap();

        assert!(result.worktree_path.exists());
        assert_eq!(
            git::get_worktree_meta_in("feature", "mode", Some(&repo_b)).as_deref(),
            Some("window")
        );
        assert!(!git::branch_exists_in("feature", Some(&repo_a)).unwrap());
    }
}
