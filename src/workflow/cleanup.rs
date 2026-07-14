use anyhow::{Context, Result, anyhow};
use regex::Regex;
use std::path::Path;
use std::time::SystemTime;
use std::{thread, time::Duration};

use crate::config::MuxMode;
use crate::multiplexer::{Multiplexer, WindowTarget, util::prefixed};
use crate::shell::shell_quote;
use crate::{cmd, git};
use tracing::{debug, info, warn};

// Re-export for use by other modules in the workflow
pub use git::get_worktree_mode;

use super::context::WorkflowContext;
use super::types::{CleanupResult, DeferredCleanup};

const WINDOW_CLOSE_DELAY_MS: u64 = 300;

/// Read the worktree's `.git` file to find the admin directory path.
///
/// Linked worktrees have a `.git` file (not directory) containing `gitdir: <path>`.
/// The path may be absolute or relative to the worktree directory.
/// Falls back to `$GIT_COMMON_DIR/worktrees/<dir_name>` if the `.git` file is missing
/// (e.g., the worktree directory was already deleted).
fn resolve_worktree_admin_dir(
    worktree_path: &Path,
    git_common_dir: &Path,
) -> Option<std::path::PathBuf> {
    let git_file = worktree_path.join(".git");
    if git_file.is_file() {
        match std::fs::read_to_string(&git_file) {
            Ok(content) => {
                if let Some(raw) = content.trim().strip_prefix("gitdir: ") {
                    let p = Path::new(raw.trim());
                    let abs = if p.is_absolute() {
                        p.to_path_buf()
                    } else {
                        worktree_path.join(p)
                    };
                    return Some(abs);
                }
                warn!(
                    path = %git_file.display(),
                    "cleanup:worktree .git file missing 'gitdir:' prefix"
                );
            }
            Err(e) => {
                warn!(
                    path = %git_file.display(),
                    error = %e,
                    "cleanup:failed to read worktree .git file"
                );
            }
        }
    }

    // Fallback: construct expected admin dir from the worktree directory name.
    // Git uses the basename of the worktree path as the admin directory name.
    worktree_path
        .file_name()
        .map(|name| git_common_dir.join("worktrees").join(name))
}

/// Best-effort recursive deletion of directory contents.
/// Used to ensure files are removed even if the directory itself is locked (e.g., CWD).
fn remove_dir_contents(path: &Path) {
    if !path.exists() {
        return;
    }

    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let entry_path = entry.path();
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir {
            let _ = std::fs::remove_dir_all(&entry_path);
        } else {
            let _ = std::fs::remove_file(&entry_path);
        }
    }
}

/// Find all windows matching the base handle pattern (including duplicates).
/// Matches: {prefix}{handle} and {prefix}{handle}-{N}
/// Run pre-remove hooks with environment variables set.
fn run_pre_remove_hooks(
    context: &WorkflowContext,
    handle: &str,
    worktree_path: &Path,
    branch_name: &str,
    show_hook_output: bool,
) -> Result<()> {
    if let Some(pre_remove_hooks) = &context.config.pre_remove {
        info!(
            branch = branch_name,
            count = pre_remove_hooks.len(),
            "cleanup:running pre-remove hooks"
        );
        let abs_worktree_path = worktree_path
            .canonicalize()
            .unwrap_or_else(|_| worktree_path.to_path_buf());
        let abs_project_root = context
            .main_worktree_root
            .canonicalize()
            .unwrap_or_else(|_| context.main_worktree_root.clone());
        let worktree_path_str = abs_worktree_path.to_string_lossy();
        let project_root_str = abs_project_root.to_string_lossy();
        let hook_env = [
            ("WORKMUX_HANDLE", handle),
            ("WM_HANDLE", handle),
            ("WM_WORKTREE_PATH", worktree_path_str.as_ref()),
            ("WM_PROJECT_ROOT", project_root_str.as_ref()),
        ];
        for command in pre_remove_hooks {
            // Run the hook with the worktree path as the working directory.
            // This allows for relative paths like `node_modules` in the command.
            cmd::shell_command_with_env_output(command, worktree_path, &hook_env, show_hook_output)
                .with_context(|| format!("Failed to run pre-remove command: '{}'", command))?;
        }
    }
    Ok(())
}

/// Remove prompt files from temp dir matching the branch name.
/// Handles both legacy fixed names and timestamped names:
/// workmux-prompt-{name}.md and workmux-prompt-{name}-{timestamp}.md
fn cleanup_prompt_files(branch_name: &str) {
    let temp_dir = std::env::temp_dir();
    let prefix = format!("workmux-prompt-{}", branch_name);
    if let Ok(entries) = std::fs::read_dir(&temp_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(filename) = path.file_name().and_then(|n| n.to_str())
                && filename.starts_with(&prefix)
                && filename.ends_with(".md")
            {
                if let Err(e) = std::fs::remove_file(&path) {
                    warn!(path = %path.display(), error = %e, "cleanup:failed to remove prompt file");
                } else {
                    debug!(path = %path.display(), "cleanup:prompt file removed");
                }
            }
        }
    }
}

fn find_matching_window_targets(
    mux: &dyn Multiplexer,
    prefix: &str,
    target_name: &str,
    parent_session: Option<&str>,
    window_token: Option<&str>,
) -> Result<Vec<WindowTarget>> {
    if let Some(token) = window_token {
        return Ok(mux
            .owned_window_targets(token)?
            .into_iter()
            .map(|owned| owned.target)
            .collect());
    }

    let all_windows = match parent_session {
        Some(session) => mux.get_window_names_in_session(session)?,
        None => mux.get_all_window_names()?,
    };
    let base_name = prefixed(prefix, target_name);
    let escaped_base = regex::escape(&base_name);
    let pattern = format!(r"^{}(-\d+)?$", escaped_base);
    let re = Regex::new(&pattern).expect("Invalid regex pattern");

    Ok(all_windows
        .into_iter()
        .filter(|window| re.is_match(window))
        .map(|window| WindowTarget::new(window, parent_session.map(str::to_string)))
        .collect())
}

/// Check if the current window/session matches the base handle pattern (including duplicates).
fn is_inside_matching_target(
    mux: &dyn Multiplexer,
    prefix: &str,
    target_name: &str,
    mode: MuxMode,
    parent_session: Option<&str>,
    window_token: Option<&str>,
) -> Result<Option<(String, Option<String>)>> {
    let (current_name, current_id) = if mode == MuxMode::Session {
        (
            mux.current_session(),
            mux.current_session_id().unwrap_or(None),
        )
    } else {
        if let Some(parent) = parent_session {
            match mux.current_session() {
                Some(current_session) if current_session == parent => {}
                _ => return Ok(None),
            }
        }
        (
            mux.current_window_name()?,
            mux.current_window_id().unwrap_or(None),
        )
    };

    if mode == MuxMode::Window
        && let (Some(token), Some(current_id)) = (window_token, current_id.as_deref())
        && let Some(owned) = mux
            .owned_window_targets(token)?
            .into_iter()
            .find(|owned| owned.target.window_id.as_deref() == Some(current_id))
    {
        return Ok(Some((owned.target.full_name, Some(current_id.to_string()))));
    }

    let current_name = match current_name {
        Some(name) => name,
        None => return Ok(None),
    };

    let base_name = prefixed(prefix, target_name);
    let escaped_base = regex::escape(&base_name);
    let pattern = format!(r"^{}(-\d+)?$", escaped_base);
    let re = Regex::new(&pattern).expect("Invalid regex pattern");

    if re.is_match(&current_name) {
        Ok(Some((current_name, current_id)))
    } else {
        Ok(None)
    }
}

/// Kill the mux window(s)/session for `target_name`. Skipped entirely when a
/// faster atomic removal path (e.g. herdr) already tore down the target.
#[allow(clippy::too_many_arguments)]
fn kill_target(
    context: &WorkflowContext,
    mux_running: bool,
    is_session_mode: bool,
    target_name: &str,
    parent_session: Option<&str>,
    window_token: Option<&str>,
    handle: &str,
    result: &mut CleanupResult,
) -> Result<()> {
    if mux_running {
        if is_session_mode {
            // For session mode, kill the session directly
            let session_name = prefixed(&context.prefix, target_name);
            if context.mux.session_exists(&session_name)? {
                if let Err(e) = context.mux.kill_session(&session_name) {
                    warn!(session = session_name, error = %e, "cleanup:failed to kill session");
                } else {
                    result.tmux_window_killed = true;
                    info!(session = session_name, "cleanup:killed session");

                    // Poll to confirm session is gone before proceeding
                    const MAX_RETRIES: u32 = 20;
                    const RETRY_DELAY: Duration = Duration::from_millis(50);
                    for _ in 0..MAX_RETRIES {
                        if !context.mux.session_exists(&session_name)? {
                            break;
                        }
                        thread::sleep(RETRY_DELAY);
                    }
                }
            }
        } else {
            // For window mode, find and kill all matching windows (including duplicates)
            let matching_windows = find_matching_window_targets(
                context.mux.as_ref(),
                &context.prefix,
                target_name,
                parent_session,
                window_token,
            )?;
            let mut killed_count = 0;
            for target in &matching_windows {
                if let Err(e) = context.mux.kill_window_target(target) {
                    warn!(window = target.full_name, error = %e, "cleanup:failed to kill window");
                } else {
                    killed_count += 1;
                    debug!(window = target.full_name, "cleanup:killed window");
                }
            }
            if killed_count > 0 {
                result.tmux_window_killed = true;
                info!(
                    count = killed_count,
                    handle = handle,
                    "cleanup:killed all matching windows"
                );

                // Poll to confirm windows are gone before proceeding
                const MAX_RETRIES: u32 = 20;
                const RETRY_DELAY: Duration = Duration::from_millis(50);
                for _ in 0..MAX_RETRIES {
                    let remaining = find_matching_window_targets(
                        context.mux.as_ref(),
                        &context.prefix,
                        target_name,
                        parent_session,
                        window_token,
                    )?;
                    if remaining.is_empty() {
                        break;
                    }
                    thread::sleep(RETRY_DELAY);
                }
            }
        }
    }
    Ok(())
}

/// Centralized function to clean up tmux and git resources.
/// `branch_name` is used for git operations (branch deletion).
/// `handle` is used for tmux operations (window/session lookup/kill).
pub fn cleanup(
    context: &WorkflowContext,
    branch_name: &str,
    handle: &str,
    worktree_path: &Path,
    force: bool,
    keep_branch: bool,
    no_hooks: bool,
    show_hook_output: bool,
) -> Result<CleanupResult> {
    if context.is_main_worktree(worktree_path) {
        return Err(anyhow!(
            "Refusing to clean up the main worktree at '{}'",
            context.main_worktree_root.display()
        ));
    }

    // Determine if this worktree was created as a session or window
    let mode = get_worktree_mode(handle);
    let target_name = if mode == MuxMode::Session {
        git::get_worktree_target_session(handle).unwrap_or_else(|| handle.to_string())
    } else {
        git::get_worktree_target_window(handle).unwrap_or_else(|| handle.to_string())
    };
    let is_session_mode = mode == MuxMode::Session;
    let parent_session = if is_session_mode {
        None
    } else {
        git::get_worktree_window_session(handle)
    };
    let window_token = if is_session_mode || !context.mux.supports_window_ownership() {
        None
    } else {
        git::get_worktree_window_token(handle)
    };
    let kind = crate::multiplexer::handle::mode_label(mode);

    info!(
        branch = branch_name,
        handle = handle,
        path = %worktree_path.display(),
        force,
        keep_branch,
        mode = kind,
        "cleanup:start"
    );
    // Change the CWD to main worktree before any destructive operations.
    // This prevents "Unable to read current working directory" errors when the command
    // is run from within the worktree being deleted.
    context.chdir_to_main_worktree()?;

    let mux_running = context.mux.is_running().unwrap_or(false);

    // Check if we're running inside ANY matching target (original or duplicate)
    let current_matching_target = if mux_running {
        is_inside_matching_target(
            context.mux.as_ref(),
            &context.prefix,
            &target_name,
            mode,
            parent_session.as_deref(),
            window_token.as_deref(),
        )?
    } else {
        None
    };
    let running_inside_target = current_matching_target.is_some();
    let current_pane_id = if mux_running {
        context.mux.current_pane_id()
    } else {
        None
    };
    let active_pane_id = if mux_running {
        context.mux.active_pane_id()
    } else {
        None
    };
    let active_pane_info = active_pane_id
        .as_deref()
        .and_then(|pane_id| context.mux.get_live_pane_info(pane_id).ok().flatten());
    let source_target_is_active = match (&current_matching_target, &active_pane_info) {
        (Some((_, Some(target_id))), Some(info)) if mode == MuxMode::Window => {
            info.window_id.as_ref() == Some(target_id)
        }
        (Some((_, Some(target_id))), Some(info)) if mode == MuxMode::Session => {
            info.session_id.as_ref() == Some(target_id)
        }
        (Some((target_name, None)), Some(info)) if mode == MuxMode::Window => {
            info.window.as_ref() == Some(target_name)
        }
        (Some((target_name, None)), Some(info)) if mode == MuxMode::Session => {
            info.session.as_ref() == Some(target_name)
        }
        _ => false,
    };
    info!(
        handle = handle,
        target_name = target_name,
        mode = kind,
        parent_session = ?parent_session,
        current_pane_id = ?current_pane_id,
        current_matching_target = ?current_matching_target,
        running_inside_target,
        source_target_is_active,
        active_pane_id = ?active_pane_id,
        active_session = ?active_pane_info.as_ref().and_then(|info| info.session.as_deref()),
        active_session_id = ?active_pane_info.as_ref().and_then(|info| info.session_id.as_deref()),
        active_window = ?active_pane_info.as_ref().and_then(|info| info.window.as_deref()),
        active_window_id = ?active_pane_info.as_ref().and_then(|info| info.window_id.as_deref()),
        "cleanup:mux focus context"
    );

    let mut result = CleanupResult {
        tmux_window_killed: false,
        worktree_removed: false,
        local_branch_deleted: false,
        source_target_is_active,
        window_to_close_later: None,
        window_target_to_close_later: None,
        target_id_to_close_later: None,
        trash_path_to_delete: None,
        deferred_cleanup: None,
    };

    // Helper closure to perform the actual filesystem and git cleanup.
    // This avoids code duplication while enforcing the correct operational order.
    //
    // `hooks_already_ran` is set when the atomic-removal path fired the hooks
    // before its attempt failed; re-running them here would execute the user's
    // whole pre_remove suite a second time.
    let perform_fs_git_cleanup = |result: &mut CleanupResult,
                                  hooks_already_ran: bool|
     -> Result<()> {
        // Resolve the admin dir before the rename so we can unlock it later.
        let worktree_admin_dir = resolve_worktree_admin_dir(worktree_path, &context.git_common_dir);

        // Run pre-remove hooks before removing the worktree directory.
        // Skip if the worktree directory doesn't exist (e.g., user manually deleted it).
        // Skip if --no-hooks is set (e.g., RPC-triggered merge).
        if hooks_already_ran {
            debug!("cleanup:pre-remove hooks already ran on the atomic removal attempt");
        } else if worktree_path.exists() && !no_hooks {
            run_pre_remove_hooks(
                context,
                handle,
                worktree_path,
                branch_name,
                show_hook_output,
            )?;
        } else {
            debug!(
                path = %worktree_path.display(),
                "cleanup:skipping pre-remove hooks, worktree directory does not exist"
            );
        }

        // Track the trash path for best-effort deletion at the end
        let mut trash_path: Option<std::path::PathBuf> = None;

        // 1. Rename the worktree directory to a trash location.
        // This immediately frees the original path for reuse, even if a shell process
        // still has it as CWD (the shell's CWD moves with the rename).
        // This fixes a race condition where running `workmux remove` from inside the
        // target tmux window could leave the directory behind.
        if worktree_path.exists() {
            let parent = worktree_path.parent().unwrap_or_else(|| Path::new("."));
            let dir_name = worktree_path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("Invalid worktree path: no directory name"))?;

            // Generate unique trash name: .workmux_trash_<name>_<timestamp>
            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let trash_name = format!(
                ".workmux_trash_{}_{}",
                dir_name.to_string_lossy(),
                timestamp
            );
            let target_trash_path = parent.join(&trash_name);

            debug!(
                from = %worktree_path.display(),
                to = %target_trash_path.display(),
                "cleanup:renaming worktree to trash"
            );

            std::fs::rename(worktree_path, &target_trash_path).with_context(|| {
                format!(
                    "Failed to rename worktree directory to trash location '{}'. \
                    Please close any terminals or editors using this directory and try again.",
                    target_trash_path.display()
                )
            })?;

            trash_path = Some(target_trash_path);
            result.worktree_removed = true;
            info!(branch = branch_name, path = %worktree_path.display(), "cleanup:worktree directory removed");
        }

        // Clean up prompt files immediately (harmless, doesn't affect CWD)
        cleanup_prompt_files(branch_name);

        // 2. Remove any worktree lock before pruning.
        // Git creates a "locked" file during `git worktree add` (with content "initializing")
        // and removes it on completion. If creation was interrupted, this file persists and
        // prevents `git worktree prune` from cleaning up the metadata.
        if let Some(ref admin_dir) = worktree_admin_dir {
            let locked_file = admin_dir.join("locked");
            match std::fs::remove_file(&locked_file) {
                Ok(()) => debug!(path = %locked_file.display(), "cleanup:removed worktree lock"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(path = %locked_file.display(), error = %e, "cleanup:failed to remove worktree lock")
                }
            }
        }

        // 3. Prune worktrees to clean up git's metadata.
        // Git will see the original path as missing since we renamed it.
        git::prune_worktrees_in(&context.git_common_dir).context("Failed to prune worktrees")?;
        debug!("cleanup:git worktrees pruned");

        // 4. Delete the local branch (unless keeping it).
        if !keep_branch {
            git::delete_branch_in(branch_name, force, &context.git_common_dir)
                .context("Failed to delete local branch")?;
            result.local_branch_deleted = true;
            info!(branch = branch_name, "cleanup:local branch deleted");
        }

        // 5. Best-effort deletion of the trash directory.
        // If the shell is inside this directory, remove_dir_all on the root might fail
        // immediately. Clearing children first ensures we reclaim the space.
        if let Some(tp) = trash_path {
            // If we're deferring window close, also defer trash deletion.
            // This prevents a race condition where processes in the window (e.g., Claude Code)
            // fail to run their stop hooks because their CWD was deleted.
            if result.window_to_close_later.is_some() {
                debug!(path = %tp.display(), "cleanup:deferring trash deletion until window close");
                result.trash_path_to_delete = Some(tp);
            } else {
                // First, aggressively clear contents to reclaim disk space
                remove_dir_contents(&tp);

                // Then try to remove the (now empty) directory
                if let Err(e) = std::fs::remove_dir(&tp) {
                    warn!(
                        path = %tp.display(),
                        error = %e,
                        "cleanup:failed to remove trash directory (likely held by active shell). \
                        The directory is empty and harmless."
                    );
                } else {
                    debug!(path = %tp.display(), "cleanup:trash directory removed");
                }
            }
        }

        Ok(())
    };

    // Gated on the capability, not just on the id being present: the id is
    // written at create time and outlives the herdr session, so a worktree
    // created under herdr and removed under tmux would emit a herdr command
    // into a tmux `run-shell` script and fail silently behind `>/dev/null 2>&1`.
    let herdr_workspace_id = if context.mux.supports_atomic_worktree_workspace() {
        git::get_worktree_meta(handle, "workspace-id")
    } else {
        None
    };

    if running_inside_target {
        let (current_target, current_target_id) = current_matching_target.unwrap();
        info!(
            branch = branch_name,
            current_target = current_target,
            kind,
            "cleanup:running inside matching target, deferring destructive cleanup",
        );

        // Find and kill all OTHER matching windows (not the current one)
        // Note: Sessions don't have duplicates like windows, so skip for session mode
        if mux_running && !is_session_mode {
            let matching_windows = find_matching_window_targets(
                context.mux.as_ref(),
                &context.prefix,
                &target_name,
                parent_session.as_deref(),
                window_token.as_deref(),
            )?;
            let mut killed_count = 0;
            for target in &matching_windows {
                let is_current = match (&target.window_id, &current_target_id) {
                    (Some(candidate), Some(current)) => candidate == current,
                    _ => target.full_name == current_target,
                };
                if !is_current {
                    if let Err(e) = context.mux.kill_window_target(target) {
                        warn!(window = target.full_name, error = %e, "cleanup:failed to kill duplicate window");
                    } else {
                        killed_count += 1;
                        debug!(window = target.full_name, "cleanup:killed duplicate window");
                    }
                }
            }
            if killed_count > 0 {
                info!(
                    count = killed_count,
                    kind, "cleanup:killed duplicate {}s", kind
                );
            }
        }

        // Store the current window/session name for deferred close
        result.window_to_close_later = Some(current_target.clone());
        result.target_id_to_close_later = current_target_id.clone();
        if !is_session_mode {
            result.window_target_to_close_later = Some(match current_target_id {
                Some(window_id) => {
                    WindowTarget::with_id(current_target, parent_session.clone(), window_id)
                }
                None => WindowTarget::new(current_target, parent_session.clone()),
            });
        }

        // Run pre-remove hooks synchronously (they need the worktree intact)
        // Skip if --no-hooks is set (e.g., RPC-triggered merge).
        if worktree_path.exists() && !no_hooks {
            run_pre_remove_hooks(
                context,
                handle,
                worktree_path,
                branch_name,
                show_hook_output,
            )?;
        }

        // Clean up prompt files immediately (harmless, doesn't affect CWD)
        cleanup_prompt_files(branch_name);

        // Defer destructive operations (rename, prune, branch delete) until after window/session close.
        // This keeps the worktree path valid so agents can run their hooks.
        if worktree_path.exists() {
            let parent = worktree_path.parent().unwrap_or_else(|| Path::new("."));
            let dir_name = worktree_path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("Invalid worktree path: no directory name"))?;
            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let trash_name = format!(
                ".workmux_trash_{}_{}",
                dir_name.to_string_lossy(),
                timestamp
            );
            let trash_path = parent.join(&trash_name);

            // Resolve the admin dir before the worktree is renamed.
            let worktree_admin_dir =
                resolve_worktree_admin_dir(worktree_path, &context.git_common_dir);

            result.deferred_cleanup = Some(DeferredCleanup {
                worktree_path: worktree_path.to_path_buf(),
                trash_path,
                branch_name: branch_name.to_string(),
                handle: handle.to_string(),
                keep_branch,
                force,
                git_common_dir: context.git_common_dir.clone(),
                worktree_admin_dir,
                worktree_removal_cmd: match herdr_workspace_id {
                    Some(ref wid) => context.mux.shell_remove_worktree_and_workspace_cmd(wid)?,
                    None => None,
                },
            });
            debug!(
                worktree = %worktree_path.display(),
                kind,
                "cleanup:deferred destructive cleanup until target close",
            );
        }
    } else {
        // Not running inside any matching target.
        //
        // For herdr: a stored workspace_id means we can close the workspace and
        // remove the git worktree in a single atomic `herdr worktree remove` call.
        let mut hooks_ran = false;
        let herdr_removed = if let Some(ref wid) = herdr_workspace_id {
            if worktree_path.exists() && !no_hooks {
                run_pre_remove_hooks(
                    context,
                    handle,
                    worktree_path,
                    branch_name,
                    show_hook_output,
                )?;
                hooks_ran = true;
            }
            cleanup_prompt_files(branch_name);
            match context.mux.remove_worktree_and_workspace(wid) {
                Ok(true) => {
                    result.tmux_window_killed = true;
                    result.worktree_removed = true;
                    info!(
                        handle = handle,
                        workspace_id = %wid,
                        "cleanup:herdr worktree+workspace removed atomically"
                    );
                    if !keep_branch {
                        git::delete_branch_in(branch_name, force, &context.git_common_dir)
                            .context("Failed to delete local branch")?;
                        result.local_branch_deleted = true;
                        info!(branch = branch_name, "cleanup:local branch deleted");
                    }
                    true
                }
                Ok(false) => false, // backend declined; fall through
                Err(e) => {
                    warn!(handle = handle, error = %e, "cleanup:herdr atomic remove failed, falling back");
                    false
                }
            }
        } else {
            false
        };

        if !herdr_removed {
            kill_target(
                context,
                mux_running,
                is_session_mode,
                &target_name,
                parent_session.as_deref(),
                window_token.as_deref(),
                handle,
                &mut result,
            )?;
            perform_fs_git_cleanup(&mut result, hooks_ran)?;
        }
    }

    // Clean up worktree metadata from git config.
    // Only remove immediately when not deferring -- deferred cleanup includes this
    // in the shell script so metadata survives if the deferred script fails.
    if result.deferred_cleanup.is_none()
        && let Err(e) = git::remove_worktree_meta(handle)
    {
        warn!(handle = handle, error = %e, "cleanup:failed to remove worktree metadata");
    }

    Ok(result)
}

/// Build the deferred cleanup script for rename, prune, branch delete, and trash removal.
///
/// Generates a semicolon-separated sequence of shell commands:
/// 1. Renames the worktree directory to a trash path (frees the original path)
/// 2. Removes any worktree lock (so prune can clean up the metadata)
/// 3. Prunes git worktree metadata
/// 4. Deletes the local branch (unless `keep_branch` is set)
/// 5. Removes workmux worktree metadata from git config
/// 6. Deletes the trash directory
///
/// A backend that owns its worktrees supplies `worktree_removal_cmd`, a single
/// command covering steps 1, 2, 3 and 6; only the branch and config cleanup
/// remain generic.
///
/// The returned string starts with "; " so it can be appended to other commands.
fn build_deferred_cleanup_script(dc: &DeferredCleanup) -> String {
    let git_dir = shell_quote(&dc.git_common_dir.to_string_lossy());

    let mut cmds = Vec::new();

    // A backend that removes the worktree itself never creates a trash dir.
    let trash_to_delete = match dc.worktree_removal_cmd {
        Some(ref cmd) => {
            cmds.push(format!("{} >/dev/null 2>&1", cmd));
            None
        }
        None => {
            let wt = shell_quote(&dc.worktree_path.to_string_lossy());
            let trash = shell_quote(&dc.trash_path.to_string_lossy());
            // 1. Rename worktree to trash
            cmds.push(format!("mv {} {} >/dev/null 2>&1", wt, trash));
            // 2. Remove worktree lock if present (git worktree prune skips locked entries)
            if let Some(ref admin_dir) = dc.worktree_admin_dir
                && admin_dir.is_absolute()
            {
                let locked = shell_quote(&admin_dir.join("locked").to_string_lossy());
                cmds.push(format!("rm -f {} >/dev/null 2>&1", locked));
            }
            // 3. Prune git worktrees
            cmds.push(format!("git -C {} worktree prune >/dev/null 2>&1", git_dir));
            Some(trash)
        }
    };

    // Branch delete and config cleanup always run after worktree removal.
    if !dc.keep_branch {
        let branch = shell_quote(&dc.branch_name);
        let force_flag = if dc.force { "-D" } else { "-d" };
        cmds.push(format!(
            "git -C {} branch {} {} >/dev/null 2>&1",
            git_dir, force_flag, branch
        ));
    }
    let handle = shell_quote(&dc.handle);
    cmds.push(format!(
        "git -C {} config --local --remove-section workmux.worktree.{} >/dev/null 2>&1",
        git_dir, handle
    ));
    if let Some(trash) = trash_to_delete {
        cmds.push(format!("rm -rf {} >/dev/null 2>&1", trash));
    }

    format!("; {}", cmds.join("; "))
}

fn cleanup_body_for(cleanup_result: &CleanupResult) -> String {
    let cleanup_script = if let Some(ref dc) = cleanup_result.deferred_cleanup {
        build_deferred_cleanup_script(dc)
    } else {
        cleanup_result
            .trash_path_to_delete
            .as_ref()
            .map(|tp| format!("; rm -rf {}", shell_quote(&tp.to_string_lossy())))
            .unwrap_or_default()
    };
    cleanup_script
        .strip_prefix("; ")
        .unwrap_or(&cleanup_script)
        .to_string()
}

fn close_and_cleanup_command(kill_cmd: Option<&str>, cleanup_body: &str) -> String {
    kill_cmd
        .map(|cmd| {
            if cleanup_body.is_empty() {
                cmd.to_string()
            } else {
                format!("{} && {}", cmd, cleanup_body)
            }
        })
        .unwrap_or_else(|| cleanup_body.to_string())
}

/// Navigate to the target branch window and close the source window.
/// Handles both cases: running inside the source window (async) and outside (sync).
/// `target_window_name` is the window name of the merge target.
/// `source_handle` is the window name of the branch being merged/removed.
pub fn navigate_to_target_and_close(
    mux: &dyn Multiplexer,
    prefix: &str,
    target_window_name: &str,
    source_handle: &str,
    cleanup_result: &CleanupResult,
    mode: MuxMode,
) -> Result<()> {
    use crate::multiplexer::MuxHandle;

    // Check if target window/session exists (probe both modes since target
    // may be a different mode than source, e.g. session worktree -> window main)
    let mux_running = mux.is_running()?;
    let target_full = prefixed(prefix, target_window_name);
    let (target_exists, target_mode) = if mux_running {
        let is_session = mux.session_exists(&target_full).unwrap_or(false);
        let is_window = mux
            .window_exists_by_full_name(&target_full)
            .unwrap_or(false);
        if is_session {
            (true, MuxMode::Session)
        } else if is_window {
            (true, MuxMode::Window)
        } else {
            (false, mode) // doesn't matter, target doesn't exist
        }
    } else {
        (false, mode)
    };
    let kind = crate::multiplexer::handle::mode_label(mode);

    // Prepare window names for shell commands
    // Use the actual window name from window_to_close_later when available (includes -N suffix),
    // otherwise fall back to the base prefixed name
    let source_full = cleanup_result
        .window_to_close_later
        .clone()
        .unwrap_or_else(|| prefixed(prefix, source_handle));

    // Generate backend-specific shell commands for deferred scripts.
    // Kill uses source mode, select uses target's detected mode.
    let kill_source_cmd = cleanup_result
        .target_id_to_close_later
        .as_deref()
        .and_then(|id| {
            if mode == MuxMode::Window {
                mux.shell_close_window_by_id_guard_cmd(id).ok()
            } else {
                mux.shell_close_session_by_id_guard_cmd(id).ok()
            }
        })
        .or_else(|| {
            if mode == MuxMode::Window {
                cleanup_result
                    .window_target_to_close_later
                    .as_ref()
                    .and_then(|target| MuxHandle::shell_kill_window_target_cmd(mux, target).ok())
                    .or_else(|| MuxHandle::shell_kill_cmd_full(mux, mode, &source_full).ok())
            } else {
                MuxHandle::shell_kill_cmd_full(mux, mode, &source_full).ok()
            }
        });
    let select_target_cmd = MuxHandle::shell_select_cmd_full(mux, target_mode, &target_full).ok();

    info!(
        prefix = prefix,
        target_window_name = target_window_name,
        mux_running = mux_running,
        target_exists = target_exists,
        target_mode = crate::multiplexer::handle::mode_label(target_mode),
        target_full = target_full,
        source_handle = source_handle,
        source_full = source_full,
        source_mode = kind,
        tmux_window_killed = cleanup_result.tmux_window_killed,
        source_target_is_active = cleanup_result.source_target_is_active,
        window_to_close = ?cleanup_result.window_to_close_later,
        window_target_to_close = ?cleanup_result.window_target_to_close_later,
        target_id_to_close = ?cleanup_result.target_id_to_close_later,
        deferred_cleanup = cleanup_result.deferred_cleanup.is_some(),
        current_pane_id = ?mux.current_pane_id(),
        active_pane_id = ?mux.active_pane_id(),
        "navigate_to_target_and_close:entry"
    );

    if !mux_running || !target_exists {
        // If target window doesn't exist, still need to close source window if running inside it
        if let Some(ref window_to_close) = cleanup_result.window_to_close_later {
            let delay = Duration::from_millis(WINDOW_CLOSE_DELAY_MS);
            let delay_secs = format!("{:.3}", delay.as_secs_f64());

            // For session mode, switch to the last session before killing so
            // the client returns to where the user was previously instead of
            // tmux picking an arbitrary session.
            let switch_last_part = if mode == MuxMode::Session {
                mux.shell_switch_to_last_session_cmd()
                    .ok()
                    .map(|cmd| format!("{}; ", cmd))
                    .unwrap_or_default()
            } else {
                String::new()
            };

            let cleanup_body = cleanup_body_for(cleanup_result);
            let close_and_cleanup =
                close_and_cleanup_command(kill_source_cmd.as_deref(), &cleanup_body);

            let script = format!(
                "sleep {delay}; {switch}{close_and_cleanup}",
                delay = delay_secs,
                switch = switch_last_part,
                close_and_cleanup = close_and_cleanup,
            );
            debug!(
                script = script,
                kind, "navigate_to_target_and_close:kill_only_script"
            );
            match mux.run_deferred_script(&script) {
                Ok(_) => info!(
                    target = window_to_close,
                    script = script,
                    kind,
                    "cleanup:scheduled target close",
                ),
                Err(e) => warn!(
                    target = window_to_close,
                    error = ?e,
                    kind,
                    "cleanup:failed to schedule target close",
                ),
            }
        }
        return Ok(());
    }

    if cleanup_result.window_to_close_later.is_some() {
        // Running inside a matching window: schedule navigation and kill together
        let delay = Duration::from_millis(WINDOW_CLOSE_DELAY_MS);
        let delay_secs = format!("{:.3}", delay.as_secs_f64());

        // Select the target before closing the source target the active client is using.
        let select_part = if cleanup_result.source_target_is_active {
            select_target_cmd
                .as_ref()
                .map(|cmd| format!("{}; ", cmd))
                .unwrap_or_default()
        } else {
            String::new()
        };

        let cleanup_body = cleanup_body_for(cleanup_result);
        let close_and_cleanup =
            close_and_cleanup_command(kill_source_cmd.as_deref(), &cleanup_body);

        let script = format!(
            "sleep {delay}; {select}{close_and_cleanup}",
            delay = delay_secs,
            select = select_part,
            close_and_cleanup = close_and_cleanup,
        );
        info!(
            script = script,
            kind,
            source_target_is_active = cleanup_result.source_target_is_active,
            "navigate_to_target_and_close:nav_and_kill_script"
        );

        match mux.run_deferred_script(&script) {
            Ok(_) => info!(
                source = source_handle,
                target = target_window_name,
                kind,
                "cleanup:scheduled navigation to target and source close",
            ),
            Err(e) => warn!(
                source = source_handle,
                error = ?e,
                kind,
                "cleanup:failed to schedule navigation and source close",
            ),
        }
    } else if !cleanup_result.tmux_window_killed {
        info!(
            handle = source_handle,
            target = target_window_name,
            kind,
            current_pane_id = ?mux.current_pane_id(),
            active_pane_id = ?mux.active_pane_id(),
            "cleanup:skipped target selection because source target was not deferred"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_deferred_cleanup(
        worktree: &str,
        trash: &str,
        branch: &str,
        handle: &str,
        git_dir: &str,
        keep_branch: bool,
        force: bool,
    ) -> DeferredCleanup {
        DeferredCleanup {
            worktree_path: PathBuf::from(worktree),
            trash_path: PathBuf::from(trash),
            branch_name: branch.to_string(),
            handle: handle.to_string(),
            keep_branch,
            force,
            git_common_dir: PathBuf::from(git_dir),
            worktree_admin_dir: None,
            worktree_removal_cmd: None,
        }
    }

    #[test]
    fn deferred_cleanup_script_includes_all_steps() {
        let dc = make_deferred_cleanup(
            "/repo/worktrees/feature",
            "/repo/worktrees/.workmux_trash_feature_123",
            "feature",
            "feature",
            "/repo/.git",
            false,
            false,
        );

        let script = build_deferred_cleanup_script(&dc);

        assert!(script.contains(
            "mv /repo/worktrees/feature /repo/worktrees/.workmux_trash_feature_123 >/dev/null 2>&1"
        ));
        assert!(script.contains("git -C /repo/.git worktree prune >/dev/null 2>&1"));
        assert!(script.contains("git -C /repo/.git branch -d feature >/dev/null 2>&1"));
        assert!(script.contains("git -C /repo/.git config --local --remove-section workmux.worktree.feature >/dev/null 2>&1"));
        assert!(
            script.contains("rm -rf /repo/worktrees/.workmux_trash_feature_123 >/dev/null 2>&1")
        );
    }

    #[test]
    fn deferred_cleanup_script_keep_branch_skips_branch_delete() {
        let dc = make_deferred_cleanup(
            "/repo/worktrees/feature",
            "/repo/worktrees/.trash",
            "feature",
            "feature",
            "/repo/.git",
            true, // keep_branch
            false,
        );

        let script = build_deferred_cleanup_script(&dc);

        assert!(
            !script.contains("branch -d"),
            "Should not delete branch when keep_branch is set"
        );
        assert!(
            !script.contains("branch -D"),
            "Should not delete branch when keep_branch is set"
        );
        // Other steps should still be present
        assert!(script.contains("mv "));
        assert!(script.contains("worktree prune"));
        assert!(script.contains("config --local --remove-section"));
        assert!(script.contains("rm -rf"));
    }

    #[test]
    fn deferred_cleanup_script_force_uses_capital_d() {
        let dc = make_deferred_cleanup(
            "/repo/worktrees/feature",
            "/repo/worktrees/.trash",
            "feature",
            "feature",
            "/repo/.git",
            false,
            true, // force
        );

        let script = build_deferred_cleanup_script(&dc);

        assert!(
            script.contains("branch -D feature"),
            "Force delete should use -D flag"
        );
        assert!(
            !script.contains("branch -d feature"),
            "Force delete should not use -d flag"
        );
    }

    #[test]
    fn deferred_cleanup_script_quotes_paths_with_spaces() {
        let dc = make_deferred_cleanup(
            "/my repo/worktrees/my feature",
            "/my repo/worktrees/.trash_123",
            "my-feature",
            "my-feature",
            "/my repo/.git",
            false,
            false,
        );

        let script = build_deferred_cleanup_script(&dc);

        assert!(
            script.contains("'/my repo/worktrees/my feature'"),
            "Worktree path with spaces should be quoted: {script}"
        );
        assert!(
            script.contains("'/my repo/worktrees/.trash_123'"),
            "Trash path with spaces should be quoted: {script}"
        );
        assert!(
            script.contains("'/my repo/.git'"),
            "Git dir with spaces should be quoted: {script}"
        );
    }

    #[test]
    fn deferred_cleanup_script_preserves_command_order() {
        let dc = make_deferred_cleanup(
            "/repo/wt/feat",
            "/repo/wt/.trash",
            "feat",
            "feat",
            "/repo/.git",
            false,
            false,
        );

        let script = build_deferred_cleanup_script(&dc);

        // Commands must execute in this order for correctness:
        // mv (free original path) -> prune (update git metadata) ->
        // branch delete -> config remove -> rm (delete trash)
        let mv_pos = script.find("mv ").expect("should contain mv");
        let prune_pos = script.find("worktree prune").expect("should contain prune");
        let branch_pos = script.find("branch -d").expect("should contain branch -d");
        let config_pos = script
            .find("config --local --remove-section")
            .expect("should contain config remove");
        let rm_pos = script.find("rm -rf").expect("should contain rm -rf");

        assert!(mv_pos < prune_pos, "mv should precede prune");
        assert!(prune_pos < branch_pos, "prune should precede branch delete");
        assert!(
            branch_pos < config_pos,
            "branch delete should precede config remove"
        );
        assert!(config_pos < rm_pos, "config remove should precede rm");
    }

    #[test]
    fn deferred_cleanup_script_starts_with_separator() {
        let dc = make_deferred_cleanup(
            "/repo/wt/feat",
            "/repo/wt/.trash",
            "feat",
            "feat",
            "/repo/.git",
            false,
            false,
        );

        let script = build_deferred_cleanup_script(&dc);

        assert!(
            script.starts_with("; "),
            "Script should start with '; ' so it can be appended to other commands: {script}"
        );
    }

    #[test]
    fn deferred_cleanup_script_simple_paths_not_quoted() {
        let dc = make_deferred_cleanup(
            "/repo/worktrees/feature-branch",
            "/repo/worktrees/.trash_feature",
            "feature-branch",
            "feature-branch",
            "/repo/.git",
            false,
            false,
        );

        let script = build_deferred_cleanup_script(&dc);

        // Simple paths (alphanumeric, dash, underscore, dot, slash) should not be quoted
        assert!(
            script.contains("mv /repo/worktrees/feature-branch /repo/worktrees/.trash_feature"),
            "Simple paths should not be quoted: {script}"
        );
    }

    #[test]
    fn deferred_cleanup_script_removes_lock_when_admin_dir_set() {
        let mut dc = make_deferred_cleanup(
            "/repo/worktrees/feature",
            "/repo/worktrees/.trash",
            "feature",
            "feature",
            "/repo/.git",
            false,
            false,
        );
        dc.worktree_admin_dir = Some(PathBuf::from("/repo/.git/worktrees/feature"));

        let script = build_deferred_cleanup_script(&dc);

        assert!(
            script.contains("rm -f /repo/.git/worktrees/feature/locked"),
            "Should remove lock file when admin dir is set: {script}"
        );

        // Lock removal should happen after mv but before prune
        let mv_pos = script.find("mv ").unwrap();
        let lock_pos = script
            .find("rm -f /repo/.git/worktrees/feature/locked")
            .unwrap();
        let prune_pos = script.find("worktree prune").unwrap();
        assert!(mv_pos < lock_pos, "lock removal should follow mv");
        assert!(lock_pos < prune_pos, "lock removal should precede prune");
    }

    #[test]
    fn deferred_cleanup_script_no_lock_step_without_admin_dir() {
        let dc = make_deferred_cleanup(
            "/repo/worktrees/feature",
            "/repo/worktrees/.trash",
            "feature",
            "feature",
            "/repo/.git",
            false,
            false,
        );

        let script = build_deferred_cleanup_script(&dc);

        assert!(
            !script.contains("/locked"),
            "Should not have lock removal without admin dir: {script}"
        );
    }

    #[test]
    fn deferred_cleanup_script_backend_removal_replaces_mv_and_prune() {
        let mut dc = make_deferred_cleanup(
            "/repo/worktrees/feature",
            "/repo/worktrees/.trash",
            "feature",
            "feature",
            "/repo/.git",
            false,
            false,
        );
        dc.worktree_removal_cmd = Some("herdr worktree remove --workspace ws-abc123".to_string());

        let script = build_deferred_cleanup_script(&dc);

        assert!(
            script.contains("herdr worktree remove --workspace ws-abc123"),
            "backend removal command should be emitted: {script}"
        );
        assert!(
            !script.contains("mv "),
            "herdr path should not rename to trash: {script}"
        );
        assert!(
            !script.contains("worktree prune"),
            "herdr path should not call git worktree prune: {script}"
        );
        assert!(
            !script.contains("rm -rf"),
            "herdr path should not delete trash directory: {script}"
        );
        // Branch delete and config cleanup still run
        assert!(
            script.contains("branch -d feature"),
            "herdr path should still delete the branch: {script}"
        );
        assert!(
            script.contains("config --local --remove-section"),
            "herdr path should still remove worktree metadata: {script}"
        );
    }

    #[test]
    fn deferred_cleanup_script_backend_removal_keep_branch_skips_branch_delete() {
        let mut dc = make_deferred_cleanup(
            "/repo/worktrees/feature",
            "/repo/worktrees/.trash",
            "feature",
            "feature",
            "/repo/.git",
            true, // keep_branch
            false,
        );
        dc.worktree_removal_cmd = Some("herdr worktree remove --workspace ws-xyz".to_string());

        let script = build_deferred_cleanup_script(&dc);

        assert!(
            !script.contains("branch -d"),
            "keep_branch should skip branch delete: {script}"
        );
        assert!(
            !script.contains("branch -D"),
            "keep_branch should skip branch delete: {script}"
        );
        assert!(
            script.contains("herdr worktree remove"),
            "should still emit the backend removal command: {script}"
        );
    }
}
