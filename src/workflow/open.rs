use anyhow::{Context, Result, anyhow};
use regex::Regex;

use crate::git;
use crate::multiplexer::MuxHandle;
use crate::multiplexer::util::prefixed;
use tracing::info;

use super::cleanup::get_worktree_mode;
use super::context::WorkflowContext;
use super::setup;
use super::types::{CreateResult, SetupOptions};

/// Open a tmux window for an existing worktree
pub fn open(
    name: &str,
    context: &WorkflowContext,
    options: SetupOptions,
    new_window: bool,
    session_override: bool,
) -> Result<CreateResult> {
    info!(
        name = name,
        run_hooks = options.run_hooks,
        run_file_ops = options.run_file_ops,
        new_window = new_window,
        session_override = session_override,
        "open:start"
    );

    // Validate mutual exclusion of panes/windows config (mode-independent)
    if context.config.panes.is_some() && context.config.windows.is_some() {
        anyhow::bail!("Cannot specify both 'panes' and 'windows' in configuration.");
    }
    if let Some(panes) = &context.config.panes {
        crate::config::validate_panes_config(panes)?;
    }

    // Pre-flight checks
    context.ensure_mux_running()?;

    // This command requires the worktree to already exist
    // Smart resolution: try handle first, then branch name
    let (worktree_path, branch_name) = git::find_worktree(name).with_context(|| {
        format!(
            "No worktree found with name '{}'. Use 'workmux list' to see available worktrees.",
            name
        )
    })?;

    // Derive base handle from the worktree path (in case user provided branch name)
    let base_handle = worktree_path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid worktree path: no directory name"))?
        .to_string_lossy()
        .to_string();

    // Resolve mode using canonical base_handle (not the CLI-provided name which may be a branch)
    let stored_mode = get_worktree_mode(&base_handle);
    let mode = if session_override {
        crate::config::MuxMode::Session
    } else {
        stored_mode
    };

    // Validate windows config requires session mode (after canonical mode resolution)
    if let Some(windows) = &context.config.windows {
        if mode != crate::config::MuxMode::Session {
            anyhow::bail!(
                "'windows' configuration requires 'mode: session'. \
                 Add 'mode: session' to your config."
            );
        }
        crate::config::validate_windows_config(windows)?;
    }

    // If --session was explicitly passed and mode is changing, close existing targets and persist
    if session_override && stored_mode != mode {
        // Kill all matching window targets (base + any -N numeric duplicates only)
        let all_names = context.mux.get_all_window_names()?;
        let full_base = crate::multiplexer::util::prefixed(&context.prefix, &base_handle);
        let full_base_dash = format!("{}-", full_base);
        for name in &all_names {
            let is_exact = *name == full_base;
            let is_numeric_suffix = name
                .strip_prefix(&full_base_dash)
                .is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()));

            if is_exact || is_numeric_suffix {
                info!(
                    handle = base_handle,
                    window = name,
                    "open:closing window before mode conversion"
                );
                MuxHandle::kill_full(context.mux.as_ref(), stored_mode, name)?;
            }
        }
        git::set_worktree_meta(&base_handle, "mode", "session")
            .context("Failed to persist session mode")?;
    }

    // Update options with the resolved mode
    let options = SetupOptions { mode, ..options };

    let target = MuxHandle::new(context.mux.as_ref(), mode, &context.prefix, &base_handle);
    let target_exists = target.exists()?;

    // If target exists and we're not forcing new, switch to it
    if target_exists && !new_window {
        target.select()?;
        info!(
            handle = base_handle,
            branch = branch_name,
            path = %worktree_path.display(),
            kind = target.kind(),
            "open:switched to existing target"
        );
        return Ok(CreateResult {
            worktree_path,
            branch_name,
            post_create_hooks_run: 0,
            base_branch: None,
            did_switch: true,
            resolved_handle: base_handle,
            mode,
        });
    }

    // Session mode doesn't support --new (duplicate sessions would be orphaned on cleanup)
    if new_window && target.is_session() {
        return Err(anyhow!(
            "--new is not supported in session mode. Each worktree can only have one session."
        ));
    }

    // Determine handle: use suffix if forcing new target and one exists
    let (handle, after_window) = if new_window && target_exists {
        let unique_handle = resolve_unique_handle(context, &base_handle)?;
        // Insert after the last window in the base handle group (base or -N suffixes)
        let after = context
            .mux
            .find_last_window_with_base_handle(&context.prefix, &base_handle)
            .unwrap_or(None);
        (unique_handle, after)
    } else {
        (base_handle, None)
    };

    // Compute working directory from config location
    let working_dir = if !context.config_rel_dir.as_os_str().is_empty() {
        let subdir_in_worktree = worktree_path.join(&context.config_rel_dir);
        if subdir_in_worktree.exists() {
            Some(subdir_in_worktree)
        } else {
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

    let options_with_workdir = SetupOptions {
        working_dir,
        config_root,
        ..options
    };

    // Setup the environment
    let result = setup::setup_environment(
        context.mux.as_ref(),
        &branch_name,
        &handle,
        &worktree_path,
        &context.config,
        &options_with_workdir,
        None,
        after_window,
    )?;
    info!(
        handle = handle,
        branch = branch_name,
        path = %result.worktree_path.display(),
        hooks_run = result.post_create_hooks_run,
        "open:completed"
    );
    Ok(result)
}

/// Find a unique handle by appending a suffix if necessary.
///
/// If `base_handle` is "my-feature" and windows exist for:
/// - wm-my-feature
/// - wm-my-feature-2
///
/// This returns "my-feature-3".
///
/// Note: Only called in window mode (session mode rejects --new).
fn resolve_unique_handle(context: &WorkflowContext, base_handle: &str) -> Result<String> {
    let all_names = context.mux.get_all_window_names()?;
    let prefix = &context.prefix;
    let full_base = prefixed(prefix, base_handle);

    // If base name doesn't exist, use it directly
    if !all_names.contains(&full_base) {
        return Ok(base_handle.to_string());
    }

    // Find the highest existing suffix
    // Pattern matches: {prefix}{handle}-{number}
    let escaped_base = regex::escape(&full_base);
    let pattern = format!(r"^{}-(\d+)$", escaped_base);
    let re = Regex::new(&pattern).expect("Invalid regex pattern");

    let mut max_suffix: u32 = 1; // Start at 1 so first duplicate is -2

    for name in &all_names {
        if let Some(caps) = re.captures(name)
            && let Some(num_match) = caps.get(1)
            && let Ok(num) = num_match.as_str().parse::<u32>()
        {
            max_suffix = max_suffix.max(num);
        }
    }

    let new_handle = format!("{}-{}", base_handle, max_suffix + 1);

    info!(
        base_handle = base_handle,
        new_handle = new_handle,
        "open:generated unique handle for duplicate"
    );

    Ok(new_handle)
}
