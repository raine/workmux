use anyhow::{Context, Result, anyhow};
use regex::Regex;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::config::MuxMode;
use crate::multiplexer::{Multiplexer, WindowTarget, util::prefixed};
use crate::{cmd, git};
use tracing::{debug, info, warn};

// Re-export for use by other modules in the workflow
pub use git::get_worktree_mode;

use super::context::WorkflowContext;
use super::types::{CleanupResult, DeferredCleanup, SourceTarget, WorktreeCleanupIdentity};

const WINDOW_CLOSE_DELAY_MS: u64 = 300;
const TARGET_POLL_INTERVAL: Duration = Duration::from_millis(50);
const TARGET_CLOSE_RETRIES: u32 = 20;
const DEFERRED_TARGET_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

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
            cmd::shell_command_with_env_output(
                context.config.hook_shell.as_deref(),
                command,
                worktree_path,
                &hook_env,
                show_hook_output,
            )
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

fn prepare_destructive_cleanup(
    context: &WorkflowContext,
    handle: &str,
    worktree_path: &Path,
    branch_name: &str,
    no_hooks: bool,
    show_hook_output: bool,
) -> Result<()> {
    // Hooks observe the live worktree before prompt files or repository state disappear.
    if worktree_path.exists() && !no_hooks {
        run_pre_remove_hooks(
            context,
            handle,
            worktree_path,
            branch_name,
            show_hook_output,
        )?;
    }
    cleanup_prompt_files(branch_name);
    Ok(())
}

fn matching_target_regex(prefix: &str, target_name: &str) -> Regex {
    let base_name = prefixed(prefix, target_name);
    Regex::new(&format!(r"^{}(-\d+)?$", regex::escape(&base_name)))
        .expect("invalid matching-target regex")
}

fn poll_bounded(mut condition: impl FnMut() -> Result<bool>) -> Result<()> {
    for _ in 0..TARGET_CLOSE_RETRIES {
        if condition()? {
            break;
        }
        thread::sleep(TARGET_POLL_INTERVAL);
    }
    Ok(())
}

fn find_matching_window_targets(
    mux: &dyn Multiplexer,
    prefix: &str,
    target_name: &str,
    parent_session: Option<&str>,
    window_token: Option<&str>,
    worktree_path: &Path,
) -> Result<Vec<WindowTarget>> {
    if let Some(token) = window_token {
        let expected_full_name = prefixed(prefix, target_name);
        return Ok(mux
            .resolve_owned_window_targets(
                token,
                &expected_full_name,
                parent_session,
                worktree_path,
            )?
            .into_iter()
            .map(|owned| owned.target)
            .collect());
    }

    let all_windows = match parent_session {
        Some(session) => mux.get_window_names_in_session(session)?,
        None => mux.get_all_window_names()?,
    };
    let re = matching_target_regex(prefix, target_name);

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
    worktree_path: &Path,
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
            .resolve_owned_window_targets(
                token,
                &prefixed(prefix, target_name),
                parent_session,
                worktree_path,
            )?
            .into_iter()
            .find(|owned| owned.target.window_id.as_deref() == Some(current_id))
    {
        return Ok(Some((owned.target.full_name, Some(current_id.to_string()))));
    }

    let current_name = match current_name {
        Some(name) => name,
        None => return Ok(None),
    };

    if matching_target_regex(prefix, target_name).is_match(&current_name) {
        Ok(Some((current_name, current_id)))
    } else {
        Ok(None)
    }
}

/// Controls cleanup behavior for worktree resources.
pub struct CleanupOptions {
    pub force: bool,
    pub keep_branch: bool,
    pub no_hooks: bool,
    pub show_hook_output: bool,
}

fn capture_worktree_identity(
    worktree_path: &Path,
    expected_common_dir: &Path,
) -> Result<Option<WorktreeCleanupIdentity>> {
    let metadata = match std::fs::symlink_metadata(worktree_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "Worktree path must be a real directory: {}",
            worktree_path.display()
        );
    }
    let repository = git::RepositoryIdentity::discover(worktree_path)
        .context("Failed to capture worktree repository identity")?;
    let expected_common_dir = expected_common_dir.canonicalize()?;
    if repository.common_dir != expected_common_dir {
        anyhow::bail!("Worktree repository identity does not match the expected repository");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Some(WorktreeCleanupIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            repository,
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        anyhow::bail!("Worktree cleanup identity requires Unix filesystem identity");
    }
}

#[derive(Clone, Copy)]
pub(super) struct DirectoryIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy)]
struct QuarantineIdentity<'a> {
    directory: DirectoryIdentity,
    repository: Option<&'a git::RepositoryIdentity>,
    admin_dir: Option<&'a Path>,
}

impl<'a> QuarantineIdentity<'a> {
    fn repository(expected: &'a WorktreeCleanupIdentity) -> Self {
        Self {
            directory: DirectoryIdentity {
                device: expected.device,
                inode: expected.inode,
            },
            repository: Some(&expected.repository),
            admin_dir: Some(&expected.repository.admin_dir),
        }
    }

    fn directory(directory: DirectoryIdentity) -> Self {
        Self {
            directory,
            repository: None,
            admin_dir: None,
        }
    }
}

fn directory_identity(metadata: &std::fs::Metadata) -> Result<DirectoryIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(DirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        anyhow::bail!("Worktree cleanup identity requires Unix filesystem identity");
    }
}

pub(super) fn metadata_matches(metadata: &std::fs::Metadata, expected: DirectoryIdentity) -> bool {
    directory_identity(metadata)
        .map(|actual| actual.device == expected.device && actual.inode == expected.inode)
        .unwrap_or(false)
}

fn quarantine_worktree(
    worktree_path: &Path,
    expected: DirectoryIdentity,
    repository: Option<&git::RepositoryIdentity>,
) -> Result<super::cleanup_retry::PendingCleanup> {
    let parent = worktree_path.parent().unwrap_or_else(|| Path::new("."));
    let dir_name = worktree_path
        .file_name()
        .context("Invalid worktree path: no directory name")?;
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let trash_path = parent.join(format!(
        ".workmux_trash_{}_{}_{}",
        dir_name.to_string_lossy(),
        std::process::id(),
        nonce
    ));

    let pending =
        super::cleanup_retry::PendingCleanup::prepare(worktree_path, trash_path.clone(), expected)?;

    // Identity is revalidated immediately before rename so path reuse cannot
    // redirect destructive cleanup to a different worktree.
    let metadata = std::fs::symlink_metadata(worktree_path)
        .context("Failed to inspect worktree before quarantine")?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || !metadata_matches(&metadata, expected)
    {
        anyhow::bail!("Worktree identity changed before quarantine");
    }
    if let Some(expected_repository) = repository {
        let repository = git::RepositoryIdentity::discover(worktree_path)
            .context("Failed to verify worktree before quarantine")?;
        if repository != *expected_repository {
            anyhow::bail!("Worktree repository identity changed before quarantine");
        }
    }

    // Renaming frees the original path for reuse while processes holding the
    // worktree as their CWD continue to refer to the quarantined directory.
    std::fs::rename(worktree_path, &trash_path).with_context(|| {
        format!(
            "Failed to rename worktree to quarantine path {}",
            trash_path.display()
        )
    })?;
    Ok(pending)
}

fn perform_destructive_cleanup(
    worktree_path: &Path,
    expected: Option<QuarantineIdentity<'_>>,
    branch_name: &str,
    handle: &str,
    keep_branch: bool,
    force: bool,
    git_common_dir: &Path,
) -> Result<()> {
    let linked_admin_dir = match expected {
        Some(expected) => expected.admin_dir.map(Path::to_path_buf),
        None => git::linked_worktree_registration_in(worktree_path, git_common_dir)?,
    };
    if let Some(admin_dir) = linked_admin_dir {
        // A linked-worktree lock prevents pruning. Removing it before quarantine
        // leaves the original worktree path available if the operation fails.
        let locked_file = admin_dir.join("locked");
        match std::fs::remove_file(&locked_file) {
            Ok(()) => debug!(path = %locked_file.display(), "cleanup:removed worktree lock"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to remove linked-worktree lock {}",
                        locked_file.display()
                    )
                });
            }
        }
    }

    let quarantine = expected
        .map(|expected| quarantine_worktree(worktree_path, expected.directory, expected.repository))
        .transpose()?;

    // The original worktree path is absent after quarantine, allowing Git to
    // discard its linked-worktree registration. These Git operations use the
    // protected execution path that pins repository identity and clears
    // ambient Git overrides.
    git::prune_worktrees_in(git_common_dir).context("Failed to prune worktrees")?;
    if expected.is_none() && git::worktree_registration_exists_in(worktree_path, git_common_dir)? {
        anyhow::bail!(
            "Linked-worktree registration remains after pruning: {}",
            worktree_path.display()
        );
    }
    if !keep_branch {
        git::delete_branch_in(branch_name, force, git_common_dir)
            .context("Failed to delete local branch")?;
    }
    git::remove_worktree_meta_at(handle, git_common_dir)
        .context("Failed to remove Workmux metadata after Git cleanup")?;
    if let Some(pending) = quarantine {
        pending.remove()?;
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
    options: CleanupOptions,
) -> Result<CleanupResult> {
    cleanup_impl(context, branch_name, handle, worktree_path, options, false)
}

pub fn cleanup_headless(
    context: &WorkflowContext,
    branch_name: &str,
    handle: &str,
    worktree_path: &Path,
    options: CleanupOptions,
) -> Result<CleanupResult> {
    cleanup_impl(context, branch_name, handle, worktree_path, options, true)
}

fn cleanup_impl(
    context: &WorkflowContext,
    branch_name: &str,
    handle: &str,
    worktree_path: &Path,
    options: CleanupOptions,
    force_headless: bool,
) -> Result<CleanupResult> {
    let CleanupOptions {
        force,
        keep_branch,
        no_hooks,
        show_hook_output,
    } = options;

    if context.is_main_worktree(worktree_path) {
        return Err(anyhow!(
            "Refusing to clean up the main worktree at '{}'",
            context.main_worktree_root.display()
        ));
    }
    let (expected_identity, missing_admin_identity) =
        match capture_worktree_identity(worktree_path, &context.git_common_dir) {
            Ok(identity) => (identity, None),
            Err(error) if keep_branch => {
                let metadata = std::fs::symlink_metadata(worktree_path)
                    .context("Failed to inspect worktree with missing Git admin state")?;
                let dot_git = std::fs::symlink_metadata(worktree_path.join(".git"));
                if metadata.file_type().is_symlink()
                    || !metadata.is_dir()
                    || !dot_git.is_ok_and(|metadata| metadata.is_file())
                {
                    return Err(error);
                }
                (None, Some(directory_identity(&metadata)?))
            }
            Err(error) => return Err(error),
        };

    if force_headless
        || !git::get_worktree_attachment_in(handle, Some(&context.execution_dir)).manages_mux()
    {
        info!(branch = branch_name, handle, path = %worktree_path.display(), "cleanup:headless");
        context.chdir_to_main_worktree()?;
        if worktree_path.exists() && !no_hooks {
            run_pre_remove_hooks(
                context,
                handle,
                worktree_path,
                branch_name,
                show_hook_output,
            )?;
        }
        cleanup_prompt_files(branch_name);
        let quarantine_identity = expected_identity
            .as_ref()
            .map(QuarantineIdentity::repository)
            .or_else(|| missing_admin_identity.map(QuarantineIdentity::directory));
        perform_destructive_cleanup(
            worktree_path,
            quarantine_identity,
            branch_name,
            handle,
            keep_branch,
            force,
            &context.git_common_dir,
        )?;
        return Ok(CleanupResult {
            tmux_window_killed: false,
            source_target_is_active: false,
            source_target_to_close: None,
            deferred_cleanup: None,
        });
    }

    // Determine if this worktree was created as a session or window
    let workdir = Some(context.execution_dir.as_path());
    let mode = git::get_worktree_mode_opt_in(handle, workdir).unwrap_or(MuxMode::Window);
    let target_name = if mode == MuxMode::Session {
        git::get_worktree_target_session_in(handle, workdir).unwrap_or_else(|| handle.to_string())
    } else {
        git::get_worktree_target_window_in(handle, workdir).unwrap_or_else(|| handle.to_string())
    };
    let is_session_mode = mode == MuxMode::Session;
    let parent_session = if is_session_mode {
        None
    } else {
        git::get_worktree_window_session_in(handle, workdir)
    };
    let window_token = if is_session_mode || !context.mux.supports_window_ownership() {
        None
    } else {
        git::get_worktree_window_token_in(handle, workdir)
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
            worktree_path,
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
        source_target_is_active,
        source_target_to_close: None,
        deferred_cleanup: None,
    };

    let perform_fs_git_cleanup = || -> Result<()> {
        let quarantine_identity = expected_identity
            .as_ref()
            .map(QuarantineIdentity::repository)
            .or_else(|| missing_admin_identity.map(QuarantineIdentity::directory));
        perform_destructive_cleanup(
            worktree_path,
            quarantine_identity,
            branch_name,
            handle,
            keep_branch,
            force,
            &context.git_common_dir,
        )?;
        Ok(())
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
                worktree_path,
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

        result.source_target_to_close = Some(if is_session_mode {
            SourceTarget::Session {
                name: current_target,
                id: current_target_id,
            }
        } else {
            SourceTarget::Window(match current_target_id {
                Some(window_id) => {
                    WindowTarget::with_id(current_target, parent_session.clone(), window_id)
                }
                None => WindowTarget::new(current_target, parent_session.clone()),
            })
        });

        prepare_destructive_cleanup(
            context,
            handle,
            worktree_path,
            branch_name,
            no_hooks,
            show_hook_output,
        )?;

        if missing_admin_identity.is_some() {
            anyhow::bail!(
                "Removal from inside the target requires intact linked-worktree admin state"
            );
        }
        if let Some(expected_identity) = expected_identity.clone() {
            result.deferred_cleanup = Some(DeferredCleanup {
                worktree_path: worktree_path.to_path_buf(),
                branch_name: branch_name.to_string(),
                handle: handle.to_string(),
                keep_branch,
                force,
                expected_identity,
            });
            debug!(
                worktree = %worktree_path.display(),
                kind,
                "cleanup:deferred destructive cleanup until target close",
            );
        } else {
            perform_fs_git_cleanup()?;
        }
    } else {
        // Not running inside any matching target, so kill it first
        if mux_running {
            if is_session_mode {
                // For session mode, kill the session directly
                let session_name = prefixed(&context.prefix, &target_name);
                if context.mux.session_exists(&session_name)? {
                    if let Err(e) = context.mux.kill_session(&session_name) {
                        warn!(session = session_name, error = %e, "cleanup:failed to kill session");
                    } else {
                        result.tmux_window_killed = true;
                        info!(session = session_name, "cleanup:killed session");

                        poll_bounded(|| Ok(!context.mux.session_exists(&session_name)?))?;
                    }
                }
            } else {
                // For window mode, find and kill all matching windows (including duplicates)
                let matching_windows = find_matching_window_targets(
                    context.mux.as_ref(),
                    &context.prefix,
                    &target_name,
                    parent_session.as_deref(),
                    window_token.as_deref(),
                    worktree_path,
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

                    poll_bounded(|| {
                        Ok(find_matching_window_targets(
                            context.mux.as_ref(),
                            &context.prefix,
                            &target_name,
                            parent_session.as_deref(),
                            window_token.as_deref(),
                            worktree_path,
                        )?
                        .is_empty())
                    })?;
                }
            }
        }
        // Targets close before hooks so hook subprocesses cannot keep them alive.
        prepare_destructive_cleanup(
            context,
            handle,
            worktree_path,
            branch_name,
            no_hooks,
            show_hook_output,
        )?;
        perform_fs_git_cleanup()?;
    }

    Ok(result)
}

fn spawn_deferred_cleanup_worker(
    cleanup: &DeferredCleanup,
    mode: MuxMode,
    source: &SourceTarget,
) -> Result<Child> {
    let executable = std::env::current_exe().context("Failed to locate Workmux executable")?;
    let repository = &cleanup.expected_identity.repository;
    let (source_name, parent_session, source_id) = match source {
        SourceTarget::Window(target) => (
            target.full_name.as_str(),
            target.parent_session.as_deref(),
            target.window_id.as_deref(),
        ),
        SourceTarget::Session { name, id } => (name.as_str(), None, id.as_deref()),
    };
    let mut command = Command::new(&executable);
    command
        .arg("_deferred-cleanup")
        .arg("--worktree-path")
        .arg(&cleanup.worktree_path)
        .arg("--branch-name")
        .arg(&cleanup.branch_name)
        .arg("--handle")
        .arg(&cleanup.handle)
        .arg("--device")
        .arg(cleanup.expected_identity.device.to_string())
        .arg("--inode")
        .arg(cleanup.expected_identity.inode.to_string())
        .arg("--expected-worktree")
        .arg(&repository.worktree)
        .arg("--admin-dir")
        .arg(&repository.admin_dir)
        .arg("--common-dir")
        .arg(&repository.common_dir)
        .arg("--dot-git")
        .arg(&repository.dot_git)
        .arg("--source-mode")
        .arg(match mode {
            MuxMode::Window => "window",
            MuxMode::Session => "session",
        })
        .arg("--source-name")
        .arg(source_name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if cleanup.keep_branch {
        command.arg("--keep-branch");
    }
    if cleanup.force {
        command.arg("--force");
    }
    if let Some(parent) = parent_session {
        command.arg("--parent-session").arg(parent);
    }
    if let Some(id) = source_id {
        command.arg("--source-id").arg(id);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // The worker must survive closure of the terminal target that started it.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn().with_context(|| {
        format!(
            "Failed to start deferred cleanup worker from {}",
            executable.display()
        )
    })
}

/// Wait for the source target to close, then perform deferred destructive cleanup.
pub fn run_deferred_cleanup_worker(
    cleanup: DeferredCleanup,
    mode: MuxMode,
    source_name: String,
    parent_session: Option<String>,
    source_id: Option<String>,
) -> Result<()> {
    let handle = cleanup.handle.clone();
    let worktree_path = cleanup.worktree_path.clone();
    let result =
        run_deferred_cleanup_worker_inner(cleanup, mode, source_name, parent_session, source_id);
    if let Err(error) = &result
        && let Err(log_error) =
            crate::logger::record_deferred_cleanup_failure(&handle, &worktree_path, error)
    {
        return Err(anyhow!(error.to_string()).context(format!(
            "Failed to record deferred cleanup failure: {log_error:#}"
        )));
    }
    result
}

fn run_deferred_cleanup_worker_inner(
    cleanup: DeferredCleanup,
    mode: MuxMode,
    source_name: String,
    parent_session: Option<String>,
    source_id: Option<String>,
) -> Result<()> {
    let mux = crate::multiplexer::create_backend(crate::multiplexer::detect_backend());
    let source_target = WindowTarget {
        full_name: source_name.clone(),
        parent_session,
        window_id: source_id.clone(),
    };
    let deadline = std::time::Instant::now() + DEFERRED_TARGET_CLOSE_TIMEOUT;
    loop {
        let target_query = match mode {
            MuxMode::Window => mux.window_target_exists(&source_target),
            MuxMode::Session => {
                let source_id = source_id
                    .as_deref()
                    .context("Deferred session cleanup requires a stable source session ID")?;
                mux.session_exists(source_id)
            }
        };
        let exists = match target_query {
            Ok(exists) => exists,
            Err(error) => match mux.is_running() {
                Ok(true) => return Err(error),
                Ok(false) | Err(_) => false,
            },
        };
        if !exists {
            break;
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "Timed out waiting for source target '{}' to close; close it and retry removal",
                source_name
            );
        }
        thread::sleep(TARGET_POLL_INTERVAL);
    }
    perform_destructive_cleanup(
        &cleanup.worktree_path,
        Some(QuarantineIdentity::repository(&cleanup.expected_identity)),
        &cleanup.branch_name,
        &cleanup.handle,
        cleanup.keep_branch,
        cleanup.force,
        &cleanup.expected_identity.repository.common_dir,
    )
}

/// Navigate to the target branch window and close the source window.
/// Handles both cases: running inside the source window (async) and outside (sync).
/// `target_window_name` is the window name of the merge target.
/// `source_handle` is the window name of the branch being merged/removed.
/// `default_session` is the configured session to prefer in session mode when
/// no workmux-managed session exists for the destination branch.
pub fn navigate_to_target_and_close(
    mux: &dyn Multiplexer,
    prefix: &str,
    target_window_name: &str,
    source_handle: &str,
    cleanup_result: &CleanupResult,
    mode: MuxMode,
    default_session: Option<&str>,
) -> Result<()> {
    use crate::multiplexer::MuxHandle;

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
            (false, mode)
        }
    } else {
        (false, mode)
    };
    let kind = crate::multiplexer::handle::mode_label(mode);
    let source = cleanup_result.source_target_to_close.as_ref();
    let source_full = source
        .map(|source| match source {
            SourceTarget::Window(target) => target.full_name.clone(),
            SourceTarget::Session { name, .. } => name.clone(),
        })
        .unwrap_or_else(|| prefixed(prefix, source_handle));
    let kill_source_cmd = source
        .and_then(|source| match source {
            SourceTarget::Window(target) => target
                .window_id
                .as_deref()
                .and_then(|id| mux.shell_close_window_by_id_guard_cmd(id).ok())
                .or_else(|| MuxHandle::shell_kill_window_target_cmd(mux, target).ok()),
            SourceTarget::Session { id, .. } => id.as_deref().and_then(|id| {
                // A managed session for the destination branch wins; otherwise
                // fall back to the configured session before the client's own
                // previous session.
                let preferred = (target_exists && target_mode == MuxMode::Session)
                    .then_some(target_full.as_str())
                    .or(default_session);
                mux.shell_close_session_by_id_guard_cmd(id, preferred).ok()
            }),
        })
        .or_else(|| MuxHandle::shell_kill_cmd_full(mux, mode, &source_full).ok());
    let select_target_cmd = MuxHandle::shell_select_cmd_full(mux, target_mode, &target_full).ok();

    info!(
        prefix,
        target_window_name,
        mux_running,
        target_exists,
        target_mode = crate::multiplexer::handle::mode_label(target_mode),
        target_full,
        source_handle,
        source_full,
        source_mode = kind,
        tmux_window_killed = cleanup_result.tmux_window_killed,
        source_target_is_active = cleanup_result.source_target_is_active,
        source_target_to_close = ?cleanup_result.source_target_to_close,
        deferred_cleanup = cleanup_result.deferred_cleanup.is_some(),
        "navigate_to_target_and_close:entry"
    );

    let Some(source) = source else {
        if !cleanup_result.tmux_window_killed {
            info!(
                handle = source_handle,
                target = target_window_name,
                kind,
                "cleanup:skipped target selection because source target was not deferred"
            );
        }
        return Ok(());
    };

    let delay = Duration::from_millis(WINDOW_CLOSE_DELAY_MS);
    let delay_secs = format!("{:.3}", delay.as_secs_f64());
    let switch_or_select = if mode == MuxMode::Session && mux.session_close_handles_navigation() {
        // The backend guards preferred navigation and handles fallback relocation.
        String::new()
    } else if !target_exists && mode == MuxMode::Session {
        // Return the client to its previous session instead of letting the
        // multiplexer choose an arbitrary destination when the source closes.
        mux.shell_switch_to_last_session_cmd()
            .ok()
            .map(|cmd| format!("{}; ", cmd))
            .unwrap_or_default()
    } else if target_exists && cleanup_result.source_target_is_active {
        // Move the active client before closing the source target it displays.
        select_target_cmd
            .as_ref()
            .map(|cmd| format!("{}; ", cmd))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let close_source = kill_source_cmd
        .as_deref()
        .context("Multiplexer did not provide a source target close command")?;
    let script = format!("sleep {delay_secs}; {switch_or_select}{close_source}");
    debug!(
        script,
        kind, "navigate_to_target_and_close:nav_and_kill_script"
    );

    // Start the worker before scheduling source closure so it retains the
    // trusted executable image and observes the complete target lifecycle.
    let mut worker = cleanup_result
        .deferred_cleanup
        .as_ref()
        .map(|cleanup| spawn_deferred_cleanup_worker(cleanup, mode, source))
        .transpose()?;

    if let Err(error) = mux.run_deferred_script(&script) {
        if let Some(worker) = worker.as_mut() {
            let _ = worker.kill();
            let _ = worker.wait();
        }
        return Err(error).context("Failed to schedule source target close");
    }
    info!(
        source = source_handle,
        target = target_window_name,
        kind,
        "cleanup:scheduled navigation and source close"
    );
    Ok(())
}
