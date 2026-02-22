use crate::multiplexer::handle::mode_label;
use crate::multiplexer::{MuxHandle, create_backend, detect_backend};
use crate::{config, sandbox, vcs};
use anyhow::{Context, Result, anyhow};

pub fn run(name: Option<&str>) -> Result<()> {
    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let prefix = config.window_prefix();

    // Resolve the handle first to determine target mode
    let resolved_handle = match name {
        Some(h) => h.to_string(),
        None => super::resolve_name(None)?,
    };

    // Determine if this worktree was created as a session or window
    let vcs = vcs::detect_vcs()?;
    let mode = vcs.get_workspace_mode(&resolved_handle);

    // When no name is provided, prefer the current window/session name
    // This handles duplicate windows/sessions (e.g., wm:feature-2) correctly
    let (full_target_name, is_current_target) = match name {
        Some(handle) => {
            // Explicit name provided - validate the worktree exists
            vcs.find_workspace(handle).with_context(|| {
                format!(
                    "No worktree found with name '{}'. Use 'workmux list' to see available worktrees.",
                    handle
                )
            })?;
            let target = MuxHandle::new(mux.as_ref(), mode, prefix, handle);
            let full = target.full_name();
            let current = target.current_name()?;
            let is_current = current.as_deref() == Some(full.as_str());
            (full, is_current)
        }
        None => {
            // No name provided - check if we're in a workmux window/session
            let target = MuxHandle::new(mux.as_ref(), mode, prefix, &resolved_handle);
            let current_name = target.current_name()?;
            if let Some(current) = current_name {
                if current.starts_with(prefix) {
                    // We're in a workmux target, use it directly
                    (current.clone(), true)
                } else {
                    // Not in a workmux target, fall back to resolved handle
                    (target.full_name(), false)
                }
            } else {
                // Not in multiplexer, use resolved handle
                (target.full_name(), false)
            }
        }
    };

    let kind = mode_label(mode);
    let target_exists = MuxHandle::exists_full(mux.as_ref(), mode, &full_target_name)?;

    if !target_exists {
        return Err(anyhow!(
            "No active {} found for '{}'. The worktree exists but has no open {}.",
            kind,
            full_target_name,
            kind
        ));
    }

    // Stop any running containers for this worktree before killing the target.
    if let Some(handle) = full_target_name.strip_prefix(prefix) {
        sandbox::stop_containers_for_handle(handle);
    }

    if is_current_target {
        let delay = std::time::Duration::from_millis(100);
        MuxHandle::schedule_close_full(mux.as_ref(), mode, &full_target_name, delay)?;
    } else {
        MuxHandle::kill_full(mux.as_ref(), mode, &full_target_name)
            .context("Failed to close target")?;
        println!("✓ Closed {} '{}' (worktree kept)", kind, full_target_name);
    }

    Ok(())
}
