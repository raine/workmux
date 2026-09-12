use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;

use crate::config::MuxMode;
use crate::multiplexer::MuxHandle;
use crate::vcs::CreateWorkspaceOptions;
use crate::{git, spinner};
use tracing::{debug, info, warn};

/// Check if a path is registered as a worktree/workspace, over any
/// [`crate::vcs::VcsBackend`] rather than raw `git worktree list`.
/// Uses canonicalize() to handle symlinks, case sensitivity, and relative paths.
fn is_registered_worktree(path: &Path, context: &WorkflowContext) -> Result<bool> {
    // Canonicalize the input path for reliable comparison
    let abs_path = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return Ok(false), // Can't canonicalize = not a valid worktree
    };

    let worktrees = context
        .vcs
        .list_workspaces_in(Some(&context.execution_dir))?;
    for entry in worktrees {
        // Canonicalize the reported path as well
        if let Ok(abs_wt) = std::fs::canonicalize(&entry.path) {
            if abs_wt == abs_path {
                return Ok(true);
            }
        } else if entry.path == path {
            // Fallback to string comparison if canonicalization fails
            return Ok(true);
        }
    }
    Ok(false)
}

/// Check if any existing worktree/workspace already has `branch_name`
/// checked out, over any [`crate::vcs::VcsBackend`]. This is the
/// backend-generic replacement for `git::worktree_exists_in`, which shells
/// out to `git worktree list --porcelain` and therefore errors outright in a
/// jj-only repository (no `.git` at all, even colocated jj never registers a
/// secondary workspace as a git worktree).
fn workspace_exists_for_branch(context: &WorkflowContext, branch_name: &str) -> Result<bool> {
    Ok(context
        .vcs
        .list_workspaces_in(Some(&context.execution_dir))?
        .iter()
        .any(|entry| entry.branch_or_bookmark.as_deref() == Some(branch_name)))
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

    let worktree_exists = workspace_exists_for_branch(context, branch_name)?;
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
    let branch_exists = context
        .vcs
        .branch_exists_in(branch_name, Some(&context.execution_dir))?;
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
        track_upstream = true;
        Some(remote_ref)
    } else if create_new {
        if let Some(base) = base_branch.filter(|base| !base.trim().is_empty()) {
            // Use the explicitly provided base branch/commit/tag
            Some(base.to_string())
        } else {
            // Default to the current branch when no explicit base was
            // provided. This goes through the VCS backend rather than
            // `git::get_current_branch_in`: `git branch --show-current` fails
            // outright inside a jj-only repository, and in a colocated one jj
            // keeps git's HEAD detached, so it exits 0 with empty output.
            //
            // `VcsBackend::get_current_branch_in` returns `Option<String>`,
            // and the meaning of `None` differs per backend:
            //
            // - git: HEAD really is detached. That has always been a hard
            //   error here, and still is — silently substituting the default
            //   branch would change long-standing git behavior.
            // - jj: `@` simply carries no bookmark, which is the *ordinary*
            //   state of a jj working-copy commit rather than an anomaly.
            //   There is no "current branch" concept to report, so fall back
            //   to the repository's default bookmark (jj's `trunk()`, then
            //   `main`/`master`) — the same answer `workmux` would use for a
            //   repo whose base is unconfigured.
            let current_branch = context
                .vcs
                .get_current_branch_in(&context.execution_dir)
                .context("Failed to determine the current branch to use as the base")?
                .map(|branch| branch.trim().to_string())
                .filter(|branch| !branch.is_empty());

            match current_branch {
                Some(branch) => Some(branch),
                None if context.vcs.name() == "git" => {
                    return Err(anyhow!(
                        "Cannot determine current branch (detached HEAD). \
                         Use --base to explicitly specify the starting point."
                    ));
                }
                None => Some(
                    context
                        .vcs
                        .get_default_branch_in(Some(&context.execution_dir))
                        .context("Failed to determine the current branch to use as the base")?,
                ),
            }
        }
    } else {
        None
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
        "create:creating worktree"
    );

    // Acquire an exclusive lock to serialize the whole creation sequence
    // (workspace creation plus the metadata writes below) across parallel
    // workmux processes. For git, without this, concurrent `workmux add`
    // commands race on git's config.lock file and fail with "could not lock
    // config file". Backends that don't share such a file (jj, whose metadata
    // store locks internally per write) return a no-op guard.
    let _config_lock = context
        .vcs
        .lock_creation_sequence(&context.git_common_dir)
        .context("Failed to acquire git config lock")?;

    // Store the base branch before checkout so observers that see the worktree
    // appear on disk also see complete branch metadata.
    if let Some(ref base) = base_branch_for_creation {
        context
            .vcs
            .meta()
            .set_branch_base(branch_name, base, Some(&context.execution_dir))
            .with_context(|| {
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

    context
        .vcs
        .create_workspace_in(
            &CreateWorkspaceOptions {
                path: worktree_path.clone(),
                name_or_branch: branch_name.to_string(),
                create_branch: create_new,
                base: base_branch_for_creation.clone(),
                track_upstream,
            },
            Some(&context.execution_dir),
        )
        .context("Failed to create git worktree")?;

    if headless {
        if let Err(error) = git::WorktreeAttachment::Headless
            .as_meta_value()
            .and_then(|value| {
                context.vcs.meta().set(
                    &current_handle,
                    "attachment",
                    value,
                    Some(&context.execution_dir),
                )
            })
        {
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
        context
            .vcs
            .meta()
            .set(
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
        context.vcs.meta().set(
            &current_handle,
            "attachment",
            git::WorktreeAttachment::Multiplexer.as_meta_value()?,
            Some(&context.execution_dir),
        )?;
        if let Some(target_window_name) = &options.target_window_name {
            context.vcs.meta().set(
                &current_handle,
                "target-window",
                target_window_name,
                Some(&context.execution_dir),
            )?;
        }
        if let Some(target_session_name) = &options.target_session_name {
            context.vcs.meta().set(
                &current_handle,
                "target-session",
                target_session_name,
                Some(&context.execution_dir),
            )?;
        }
        if let Some(window_session_name) = &options.window_session_name {
            context.vcs.meta().set(
                &current_handle,
                "window-session",
                window_session_name,
                Some(&context.execution_dir),
            )?;
        }
        if options.mode == MuxMode::Window && context.mux.supports_window_ownership() {
            if context.vcs.name() != "git" {
                bail!(
                    "workmux does not yet support window-token tracking for session-mode \
                     attachment on '{}' repositories; this metadata write is git-specific \
                     and would silently land in the wrong store on a colocated jj repo. \
                     Use a git repository, or a mux mode that does not require window \
                     ownership tracking, for '{}'.",
                    context.vcs.name(),
                    current_handle
                );
            }
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
            base_branch: base_branch_for_creation,
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
    result.base_branch = base_branch_for_creation;
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

    /// Characterization test for the attached `create()` path after routing
    /// creation through `context.vcs` instead of calling `git::*` directly.
    ///
    /// Every assertion below describes the git-level state the pre-migration
    /// code produced: a registered worktree at the handle path, a new branch,
    /// a `workmux.branch.<branch>.base` entry, and the full per-worktree
    /// metadata block (mode / attachment / target-window / target-session /
    /// window-session) in `.git/config`.
    #[test]
    fn attached_create_writes_identical_git_state_through_the_vcs_backend() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&repo);

        let ctx =
            WorkflowContext::new_in(&repo, Config::default(), Arc::new(TestMux), None).unwrap();
        assert_eq!(ctx.vcs.name(), "git");

        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;
        options.mode = MuxMode::Window;
        options.target_window_name = Some("my-window".to_string());
        options.target_session_name = Some("my-session".to_string());
        options.window_session_name = Some("parent-session".to_string());

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

        // Worktree exists on disk and is registered with git.
        assert!(result.worktree_path.exists());
        assert!(is_registered_worktree(&result.worktree_path, &ctx).unwrap());
        assert_eq!(
            git::get_worktree_path_in("feature", Some(&repo))
                .unwrap()
                .canonicalize()
                .unwrap(),
            result.worktree_path.canonicalize().unwrap()
        );

        // Branch was created and its base recorded.
        assert!(git::branch_exists_in("feature", Some(&repo)).unwrap());
        assert_eq!(
            git::get_branch_base_in("feature", Some(&repo)).unwrap(),
            "main"
        );

        // Full metadata block, still readable via the unchanged git readers.
        assert_eq!(
            git::get_worktree_meta_in("feature", "mode", Some(&repo)).as_deref(),
            Some("window")
        );
        assert_eq!(
            git::get_worktree_attachment_in("feature", Some(&repo)),
            git::WorktreeAttachment::Multiplexer
        );
        assert_eq!(
            git::get_worktree_meta_in("feature", "target-window", Some(&repo)).as_deref(),
            Some("my-window")
        );
        assert_eq!(
            git::get_worktree_meta_in("feature", "target-session", Some(&repo)).as_deref(),
            Some("my-session")
        );
        assert_eq!(
            git::get_worktree_meta_in("feature", "window-session", Some(&repo)).as_deref(),
            Some("parent-session")
        );
        // TestMux does not claim window ownership, so no token is minted -
        // unchanged from before the migration.
        assert_eq!(
            git::get_worktree_window_token_in("feature", Some(&repo)),
            None
        );
    }

    /// Same characterization, for the headless (`workmux add --headless`)
    /// path, which shares `create_impl` with the attached path.
    #[test]
    fn headless_create_writes_identical_git_state_through_the_vcs_backend() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&repo);

        let ctx =
            WorkflowContext::new_in(&repo, Config::default(), Arc::new(TestMux), None).unwrap();

        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;

        let result = create_headless(
            &ctx,
            CreateArgs {
                branch_name: "headless-feature",
                handle: "headless-feature",
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
            false,
        )
        .unwrap();

        assert!(result.worktree_path.exists());
        assert!(is_registered_worktree(&result.worktree_path, &ctx).unwrap());
        assert!(git::branch_exists_in("headless-feature", Some(&repo)).unwrap());
        assert_eq!(
            git::get_branch_base_in("headless-feature", Some(&repo)).unwrap(),
            "main"
        );
        assert_eq!(
            git::get_worktree_attachment_in("headless-feature", Some(&repo)),
            git::WorktreeAttachment::Headless
        );
        // The headless path writes no multiplexer metadata.
        assert_eq!(
            git::get_worktree_meta_in("headless-feature", "mode", Some(&repo)),
            None
        );
        assert_eq!(
            git::get_worktree_meta_in("headless-feature", "target-window", Some(&repo)),
            None
        );
    }

    /// Seed a freshly-initialized jj fixture with an initial commit and a
    /// `main` bookmark, then move `@` off it, mirroring `jj_backend.rs`'s
    /// private `seed` test helper (duplicated here rather than shared, since
    /// that helper is private to `vcs::jj_backend`'s own test module).
    fn seed_jj_fixture(repo: &Path) {
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        test_support::run_jj(repo, &["describe", "-m", "initial"]);
        test_support::run_jj(repo, &["bookmark", "create", "main", "-r", "@"]);
        test_support::run_jj(repo, &["new"]);
    }

    /// End-to-end coverage for the full path through the jj wiring from
    /// Tasks 5-8: a real `WorkflowContext::new_in` against a real jj
    /// fixture, `create_headless` (`workmux add --headless`), asserting the
    /// resulting on-disk and jj-repo state entirely through the same
    /// `VcsBackend`/`WorkmuxMetaStore` methods production code uses (never a
    /// raw `jj` shell-out from the assertions themselves).
    ///
    /// Run for both a jj-only fixture (`jj git init --no-colocate`, no
    /// `.git` at the workspace root) and a colocated one (`jj git init
    /// --colocate`), since `create_impl`'s worktree-collision detection
    /// (`is_registered_worktree`/`workspace_exists_for_branch`) and
    /// `JjBackend::create_workspace_in`'s parent-directory handling both
    /// depend on repo layout that differs between the two.
    fn jj_headless_create_writes_workspace_bookmark_and_metadata(init_fixture: fn(&Path)) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_fixture(&repo);
        seed_jj_fixture(&repo);

        let ctx =
            WorkflowContext::new_in(&repo, Config::default(), Arc::new(TestMux), None).unwrap();
        assert_eq!(ctx.vcs.name(), "jj");

        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;

        let result = create_headless(
            &ctx,
            CreateArgs {
                branch_name: "jj-feature",
                handle: "jj-feature",
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
            false,
        )
        .unwrap();

        // Workspace directory exists on disk with the base's content checked out.
        assert!(result.worktree_path.exists());
        assert!(result.worktree_path.join("README.md").exists());

        // Workspace is registered with jj at the expected path, with a
        // bookmark checked out matching the plan's
        // "bookmark-per-workspace-by-default" ruling.
        let workspaces = ctx.vcs.list_workspaces_in(Some(&repo)).unwrap();
        let expected_path = result.worktree_path.canonicalize().unwrap();
        let entry = workspaces
            .iter()
            .find(|entry| entry.path.canonicalize().unwrap() == expected_path)
            .expect("newly created jj workspace should be listed");
        assert_eq!(entry.branch_or_bookmark.as_deref(), Some("jj-feature"));
        assert!(ctx.vcs.branch_exists_in("jj-feature", Some(&repo)).unwrap());

        // Metadata landed in JjMetaStore's TOML file, read back through the
        // same WorkmuxMetaStore methods the rest of workmux uses (not a
        // direct TOML-file read from the test).
        assert_eq!(
            ctx.vcs.meta().get_branch_base("jj-feature", Some(&repo)),
            Some("main".to_string())
        );
        assert_eq!(
            ctx.vcs.meta().get("jj-feature", "attachment", Some(&repo)),
            Some("headless".to_string())
        );
        // The headless path writes no multiplexer metadata, same as the git case.
        assert_eq!(ctx.vcs.meta().get("jj-feature", "mode", Some(&repo)), None);
    }

    /// The *default* base path: no `--base`, no `base_branch:` in
    /// `.workmux.yaml`. Every other jj test in this module supplies
    /// `base_branch: Some("main")`, so this is the path that `git branch
    /// --show-current` used to break — it fails outright in a jj-only repo and
    /// reports an empty (detached) branch in a colocated one, because jj keeps
    /// git's HEAD detached. With no bookmark on `@`, the jj backend has no
    /// "current branch" to report at all, so the base must fall back to the
    /// repository's default bookmark.
    fn jj_create_with_no_configured_base_uses_default_bookmark(init_fixture: fn(&Path)) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_fixture(&repo);
        // `seed_jj_fixture` leaves `@` as a fresh bookmark-less child of
        // `main` - jj's ordinary state, and the one that has no current
        // branch.
        seed_jj_fixture(&repo);

        let ctx =
            WorkflowContext::new_in(&repo, Config::default(), Arc::new(TestMux), None).unwrap();
        assert_eq!(ctx.vcs.name(), "jj");
        assert_eq!(ctx.vcs.get_current_branch_in(&repo).unwrap(), None);

        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;

        let result = create_headless(
            &ctx,
            CreateArgs {
                branch_name: "jj-default-base",
                handle: "jj-default-base",
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
            false,
        )
        .unwrap();

        assert!(result.worktree_path.exists());
        // The base fell back to the default bookmark, and was recorded as such.
        assert_eq!(
            ctx.vcs
                .meta()
                .get_branch_base("jj-default-base", Some(&repo)),
            Some("main".to_string())
        );
        // ...and the new workspace really is parented on `main`, with the
        // base's content checked out.
        assert!(result.worktree_path.join("README.md").exists());
        let parent_bookmarks = test_support::run_jj(
            &result.worktree_path,
            &[
                "log",
                "--no-graph",
                "-r",
                "@-",
                "-T",
                r#"local_bookmarks.map(|b| b.name()).join(",")"#,
            ],
        );
        assert_eq!(parent_bookmarks.trim(), "main");
        assert!(
            ctx.vcs
                .branch_exists_in("jj-default-base", Some(&repo))
                .unwrap()
        );
    }

    #[test]
    fn jj_create_with_no_configured_base_uses_default_bookmark_jj_only() {
        jj_create_with_no_configured_base_uses_default_bookmark(test_support::init_jj_repo);
    }

    #[test]
    fn jj_create_with_no_configured_base_uses_default_bookmark_colocated() {
        jj_create_with_no_configured_base_uses_default_bookmark(test_support::init_colocated_repo);
    }

    /// Characterization: for a git repo the default base path is unchanged -
    /// the current branch is used verbatim, with no default-branch fallback.
    #[test]
    fn git_create_with_no_configured_base_uses_the_current_branch() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&repo);
        // A non-default current branch, so "current branch" and "default
        // branch" are distinguishable.
        test_support::run_git(&repo, &["checkout", "-b", "side"]);

        let ctx =
            WorkflowContext::new_in(&repo, Config::default(), Arc::new(TestMux), None).unwrap();
        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;

        create_headless(
            &ctx,
            CreateArgs {
                branch_name: "git-default-base",
                handle: "git-default-base",
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
            false,
        )
        .unwrap();

        assert_eq!(
            git::get_branch_base_in("git-default-base", Some(&repo)).unwrap(),
            "side"
        );
    }

    /// Characterization: a git repo with a genuinely detached HEAD still
    /// errors rather than silently falling back to the default branch. Only
    /// jj's bookmark-less `@` - a backend with no current-branch concept -
    /// gets the fallback treatment.
    #[test]
    fn git_create_with_detached_head_and_no_base_still_errors() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&repo);
        test_support::run_git(&repo, &["checkout", "--detach", "HEAD"]);

        let ctx =
            WorkflowContext::new_in(&repo, Config::default(), Arc::new(TestMux), None).unwrap();
        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;

        let error = create_headless(
            &ctx,
            CreateArgs {
                branch_name: "git-detached",
                handle: "git-detached",
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
            false,
        )
        .map(|_| ())
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("detached HEAD"), "{message}");
    }

    #[test]
    fn jj_headless_create_writes_workspace_bookmark_and_metadata_jj_only() {
        jj_headless_create_writes_workspace_bookmark_and_metadata(test_support::init_jj_repo);
    }

    #[test]
    fn jj_headless_create_writes_workspace_bookmark_and_metadata_colocated() {
        jj_headless_create_writes_workspace_bookmark_and_metadata(
            test_support::init_colocated_repo,
        );
    }

    /// The remove half of the full create -> remove round trip, exercised
    /// once against the colocated fixture. The jj-only fixture is not
    /// re-run through `remove` as well: `workflow::remove`'s jj path (the
    /// `find_worktree`/`attachment_via_vcs`/`perform_jj_destructive_cleanup`
    /// wiring added alongside this test) never branches on colocated vs
    /// jj-only - every jj call it makes resolves through `jj`'s own CLI
    /// against `.jj/repo`, which is identical in shape for both fixture
    /// kinds. The create-side test above already covers both kinds for the
    /// parts of the path that *do* differ (parent-directory creation,
    /// worktree-collision detection). Running the identical remove
    /// assertions twice would add test runtime without covering any new
    /// branch.
    ///
    /// `workflow::remove` -> `cleanup::cleanup_impl` calls
    /// `context.chdir_to_main_worktree()`, which mutates the whole test
    /// binary's process-wide current directory - unlike every other test in
    /// this module, which only ever reads paths explicitly. Under `cargo
    /// test`'s default parallel execution that would race with any other
    /// test relying on an implicit cwd, so (mirroring
    /// `workflow_create_uses_explicit_repo_not_process_cwd` above) this test
    /// re-execs itself as an isolated child process via
    /// `test_support::run_isolated_test`, confining the chdir to a process
    /// that runs this one test and exits immediately after.
    #[test]
    fn jj_headless_create_then_remove_round_trip_forgets_workspace_and_metadata() {
        const TEST_NAME: &str = "workflow::create::tests::jj_headless_create_then_remove_round_trip_forgets_workspace_and_metadata";
        if !test_support::is_isolated_child(TEST_NAME) {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            test_support::init_colocated_repo(&repo);
            seed_jj_fixture(&repo);

            test_support::run_isolated_test(TEST_NAME, &repo, &[("WM_TEST_TEMP", temp.path())]);
            return;
        }

        println!("{}", test_support::ISOLATED_TEST_CANARY);
        let temp = std::env::var_os("WM_TEST_TEMP").map(PathBuf::from).unwrap();
        let repo = temp.join("repo");

        let ctx =
            WorkflowContext::new_in(&repo, Config::default(), Arc::new(TestMux), None).unwrap();

        let mut options = SetupOptions::new(false, false, false);
        options.focus_window = false;

        let result = create_headless(
            &ctx,
            CreateArgs {
                branch_name: "jj-round-trip",
                handle: "jj-round-trip",
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
            false,
        )
        .unwrap();
        let worktree_path = result.worktree_path.clone();
        assert!(worktree_path.exists());

        // Sanity: the workspace and bookmark exist before removal.
        assert!(
            ctx.vcs
                .list_workspaces_in(Some(&repo))
                .unwrap()
                .iter()
                .any(|entry| entry.name.as_deref() == Some("jj-round-trip"))
        );
        assert!(
            ctx.vcs
                .branch_exists_in("jj-round-trip", Some(&repo))
                .unwrap()
        );

        let remove_result = super::super::remove("jj-round-trip", true, false, &ctx).unwrap();
        assert_eq!(remove_result.branch_removed, "jj-round-trip");
        assert!(!remove_result.cleanup_scheduled);

        // The workspace directory is gone from disk.
        assert!(!worktree_path.exists());

        // The workspace registration was forgotten and the bookmark deleted.
        assert!(
            !ctx.vcs
                .list_workspaces_in(Some(&repo))
                .unwrap()
                .iter()
                .any(|entry| entry.name.as_deref() == Some("jj-round-trip"))
        );
        assert!(
            !ctx.vcs
                .branch_exists_in("jj-round-trip", Some(&repo))
                .unwrap()
        );

        // Workmux's own per-worktree metadata for the handle was removed.
        // `branch_base` is keyed by branch name rather than handle and is
        // deliberately left behind by `WorkmuxMetaStore::remove_all_at` for
        // both backends (confirmed against `GitBackend`:
        // `git::remove_worktree_meta_at`'s key pattern only ever matches
        // `workmux.worktree.<handle>.*`, never `workmux.branch.<branch>.*`)
        // - so it is asserted preserved here, not removed.
        assert_eq!(
            ctx.vcs.meta().get_branch_base("jj-round-trip", Some(&repo)),
            Some("main".to_string())
        );
        assert_eq!(
            ctx.vcs
                .meta()
                .get("jj-round-trip", "attachment", Some(&repo)),
            None
        );
    }
}
