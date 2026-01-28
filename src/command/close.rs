use crate::config::TmuxTarget;
use crate::{config, git, tmux};
use anyhow::{Context, Result, anyhow};

pub fn run(name: Option<&str>) -> Result<()> {
    let config = config::Config::load(None)?;
    let prefix = config.window_prefix();

    // Resolve the handle first to determine target mode
    let resolved_handle = match name {
        Some(h) => h.to_string(),
        None => super::resolve_name(None)?,
    };

    // Determine if this worktree was created as a session or window
    let is_session_mode = git::get_worktree_target(&resolved_handle) == TmuxTarget::Session;

    // When no name is provided, prefer the current tmux window/session name
    // This handles duplicate windows/sessions (e.g., wm:feature-2) correctly
    let (full_target_name, is_current_target) = match name {
        Some(handle) => {
            // Explicit name provided - validate the worktree exists
            git::find_worktree(handle).with_context(|| {
                format!(
                    "No worktree found with name '{}'. Use 'workmux list' to see available worktrees.",
                    handle
                )
            })?;
            let prefixed = tmux::prefixed(prefix, handle);
            let is_current = if is_session_mode {
                let current_session = tmux::current_session_name()?;
                current_session.as_deref() == Some(&prefixed)
            } else {
                let current_window = tmux::current_window_name()?;
                current_window.as_deref() == Some(&prefixed)
            };
            (prefixed, is_current)
        }
        None => {
            // No name provided - check if we're in a workmux window/session
            if is_session_mode {
                if let Some(current) = tmux::current_session_name()? {
                    if current.starts_with(prefix) {
                        // We're in a workmux session, use it directly
                        (current.clone(), true)
                    } else {
                        // Not in a workmux session, use resolved handle
                        (tmux::prefixed(prefix, &resolved_handle), false)
                    }
                } else {
                    // Not in tmux, use resolved handle
                    (tmux::prefixed(prefix, &resolved_handle), false)
                }
            } else if let Some(current) = tmux::current_window_name()? {
                if current.starts_with(prefix) {
                    // We're in a workmux window, use it directly
                    (current.clone(), true)
                } else {
                    // Not in a workmux window, fall back to directory name
                    (tmux::prefixed(prefix, &resolved_handle), false)
                }
            } else {
                // Not in tmux, use directory name
                (tmux::prefixed(prefix, &resolved_handle), false)
            }
        }
    };

    let target_type = if is_session_mode { "session" } else { "window" };

    // Check if the tmux window/session exists
    let target_exists = if is_session_mode {
        tmux::session_exists_by_full_name(&full_target_name)?
    } else {
        tmux::window_exists_by_full_name(&full_target_name)?
    };

    if !target_exists {
        return Err(anyhow!(
            "No active tmux {} found for '{}'. The worktree exists but has no open {}.",
            target_type,
            full_target_name,
            target_type
        ));
    }

    if is_current_target {
        // Schedule the close with a small delay so the command can complete
        if is_session_mode {
            tmux::schedule_session_close_by_full_name(
                &full_target_name,
                std::time::Duration::from_millis(100),
            )?;
        } else {
            tmux::schedule_window_close_by_full_name(
                &full_target_name,
                std::time::Duration::from_millis(100),
            )?;
        }
    } else {
        // Kill the target directly
        if is_session_mode {
            tmux::kill_session_by_full_name(&full_target_name)
                .context("Failed to close tmux session")?;
        } else {
            tmux::kill_window_by_full_name(&full_target_name)
                .context("Failed to close tmux window")?;
        }
        println!(
            "✓ Closed {} '{}' (worktree kept)",
            target_type, full_target_name
        );
    }

    Ok(())
}
