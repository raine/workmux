//! Sidebar pane creation, destruction, and lifecycle management.

use anyhow::{Result, anyhow};
use tracing::debug;

use crate::cmd::Cmd;
use crate::config::SidebarPosition;

use super::SIDEBAR_ROLE_VALUE;
use super::app::HostIdentity;
use super::daemon_ctrl::kill_daemon;
use super::hooks::remove_hooks;
use super::layout_tree::{layout_after_sidebar_remove, reflow_after_sidebar_add};

const PENDING_SELECTION_OPTION: &str = "@workmux_sidebar_pending_selection";

fn parse_window_panes(output: &str) -> (Option<&str>, Option<&str>) {
    let mut sidebar_pane_id = None;
    let mut first_pane_id = None;
    for line in output.lines() {
        let (pane_id, role) = line.split_once('\t').unwrap_or((line, ""));
        let pane_id = pane_id.trim();
        if first_pane_id.is_none() && !pane_id.is_empty() {
            first_pane_id = Some(pane_id);
        }
        if role.trim() == SIDEBAR_ROLE_VALUE {
            sidebar_pane_id = Some(pane_id);
        }
    }
    (sidebar_pane_id, first_pane_id)
}

fn query_window_panes(window_id: &str) -> Result<String> {
    Cmd::new("tmux")
        .args(&[
            "list-panes",
            "-t",
            window_id,
            "-F",
            "#{pane_id}\t#{@workmux_role}",
        ])
        .run_and_capture_stdout()
}

/// Find the sidebar pane in a window, if one exists.
pub(super) fn find_sidebar_pane_in_window(window_id: &str) -> Result<Option<String>> {
    let output = query_window_panes(window_id)?;
    Ok(parse_window_panes(&output).0.map(str::to_string))
}

/// Check if a window already has a sidebar pane.
pub(super) fn find_sidebar_in_window(window_id: &str) -> Result<bool> {
    Ok(find_sidebar_pane_in_window(window_id)?.is_some())
}

/// Leave an agent selection for a sidebar process before switching to it.
pub(super) fn set_pending_selection(sidebar_pane_id: &str, agent_pane_id: &str) -> Result<()> {
    Cmd::new("tmux")
        .args(&[
            "set-option",
            "-p",
            "-t",
            sidebar_pane_id,
            PENDING_SELECTION_OPTION,
            agent_pane_id,
        ])
        .run()
        .map(|_| ())
}

pub(super) fn clear_pending_selection(sidebar_pane_id: &str) {
    let _ = Cmd::new("tmux")
        .args(&[
            "set-option",
            "-pu",
            "-t",
            sidebar_pane_id,
            PENDING_SELECTION_OPTION,
        ])
        .run();
}

/// Take the selection handed off by another sidebar process.
pub(super) fn take_pending_selection(sidebar_pane_id: &str) -> Option<String> {
    let selection = Cmd::new("tmux")
        .args(&[
            "show-option",
            "-pqv",
            "-t",
            sidebar_pane_id,
            PENDING_SELECTION_OPTION,
        ])
        .run_and_capture_stdout()
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    clear_pending_selection(sidebar_pane_id);
    selection
}

/// Create a sidebar pane in a specific window (idempotent).
pub(super) fn create_sidebar_in_window(
    window_id: &str,
    position: SidebarPosition,
    size: u16,
) -> Result<()> {
    let panes = query_window_panes(window_id).or_else(|_| query_window_panes(window_id))?;
    let target_pane = match parse_window_panes(&panes) {
        (Some(_), _) => {
            debug!(
                window_id,
                "create_sidebar_in_window: already exists, skipping"
            );
            return Ok(());
        }
        (None, Some(target_pane)) => target_pane.to_string(),
        (None, None) => return Ok(()),
    };
    if target_pane.is_empty() {
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let exe_str = exe.to_str().ok_or_else(|| anyhow!("exe path not UTF-8"))?;
    let size_str = size.to_string();

    debug!(window_id, position = ?position, size, "create_sidebar_in_window: creating");

    let split_flag = match position {
        SidebarPosition::Left => "-hbf",
        SidebarPosition::Top => "-vbf",
    };

    let new_pane_id = Cmd::new("tmux")
        .args(&[
            "split-window",
            split_flag,
            "-l",
            &size_str,
            "-t",
            &target_pane,
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            exe_str,
            "_sidebar-run",
        ])
        .run_and_capture_stdout()?
        .trim()
        .to_string();

    if let Err(error) = Cmd::new("tmux")
        .args(&[
            "set-option",
            "-p",
            "-t",
            &new_pane_id,
            "@workmux_role",
            SIDEBAR_ROLE_VALUE,
        ])
        .run()
    {
        let _ = Cmd::new("tmux")
            .args(&["kill-pane", "-t", &new_pane_id])
            .run();
        return Err(anyhow!(
            "failed to mark sidebar pane {new_pane_id} in window {window_id}: {error}"
        ));
    }

    reflow_after_sidebar_add(window_id, &new_pane_id, position, size);

    debug!(
        window_id,
        pane_id = new_pane_id.as_str(),
        requested_size = size,
        "create_sidebar_in_window: done"
    );

    Ok(())
}

enum SidebarWindowScope<'a> {
    All,
    Session(&'a str),
}

fn sidebar_window_extent_format(position: SidebarPosition) -> &'static str {
    match position {
        SidebarPosition::Left => "#{window_id} #{window_width}",
        SidebarPosition::Top => "#{window_id} #{window_height}",
    }
}

fn list_windows_for_sidebars(
    scope: SidebarWindowScope<'_>,
    position: SidebarPosition,
) -> Result<String> {
    let format = sidebar_window_extent_format(position);
    match scope {
        SidebarWindowScope::All => Cmd::new("tmux")
            .args(&["list-windows", "-a", "-F", format])
            .run_and_capture_stdout(),
        SidebarWindowScope::Session(session_id) => Cmd::new("tmux")
            .args(&["list-windows", "-t", session_id, "-F", format])
            .run_and_capture_stdout(),
    }
}

/// Create sidebars in all existing tmux windows.
///
/// Computes width per-window from `#{window_width}` so each window gets a
/// sidebar proportional to its own dimensions. Unattached sessions may have
/// stale geometry, but `reflow()` corrects them on reattach.
pub(super) fn create_sidebars_in_all_windows(config: &crate::config::Config) -> Result<()> {
    let position = super::read_sidebar_position(config);
    let output = list_windows_for_sidebars(SidebarWindowScope::All, position)?;

    debug!(position = ?position, "create_sidebars_in_all_windows: creating sidebars");
    create_sidebars_from_window_output(&output, config, position)
}

/// Create sidebars in all windows of a specific session (by session_id).
pub(super) fn create_sidebars_in_session(
    session_id: &str,
    config: &crate::config::Config,
) -> Result<()> {
    let position = super::read_sidebar_position(config);
    let output = list_windows_for_sidebars(SidebarWindowScope::Session(session_id), position)?;

    debug!(session_id, position = ?position, "create_sidebars_in_session: creating sidebars");
    create_sidebars_from_window_output(&output, config, position)
}

fn create_sidebars_from_window_output(
    output: &str,
    config: &crate::config::Config,
    position: SidebarPosition,
) -> Result<()> {
    let mut errors = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (window_id, extent_str) = line.split_once(' ').unwrap_or((line, "0"));
        let window_extent: u16 = extent_str.parse().unwrap_or(0);
        let size = super::effective_size_for(config, position, window_extent);
        if let Err(error) = create_sidebar_in_window(window_id, position, size) {
            errors.push(format!("{window_id}: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to create sidebars in {} window(s): {}",
            errors.len(),
            errors.join("; ")
        ))
    }
}

fn compute_sidebar_layouts(
    sidebars: &[(String, String)],
    position: SidebarPosition,
) -> Vec<Option<String>> {
    sidebars
        .iter()
        .map(|(window_id, pane_id)| layout_after_sidebar_remove(window_id, pane_id, position))
        .collect()
}

fn kill_panes_and_apply_layouts(sidebars: &[(String, String)], layouts: &[Option<String>]) {
    for (_, pane_id) in sidebars {
        let _ = Cmd::new("tmux").args(&["kill-pane", "-t", pane_id]).run();
    }
    for (i, (window_id, _)) in sidebars.iter().enumerate() {
        if let Some(layout) = &layouts[i] {
            let _ = Cmd::new("tmux")
                .args(&["select-layout", "-t", window_id, layout])
                .run();
        }
    }
}

/// Kill sidebar panes only in a specific session (by session_id).
/// Handles layout restoration for killed panes.
pub(super) fn kill_sidebars_in_session(session_id: &str) {
    let config = crate::config::Config::load(None).unwrap_or_default();
    let position = super::read_sidebar_position(&config);
    let output = Cmd::new("tmux")
        .args(&[
            "list-panes",
            "-a",
            "-F",
            "#{session_id} #{window_id} #{pane_id} #{@workmux_role}",
        ])
        .run_and_capture_stdout()
        .unwrap_or_default();

    let session_sidebars: Vec<(String, String)> = output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, ' ');
            let sid = parts.next()?;
            let window_id = parts.next()?;
            let pane_id = parts.next()?;
            let role = parts.next()?.trim();
            (role == SIDEBAR_ROLE_VALUE && sid == session_id)
                .then(|| (window_id.to_string(), pane_id.to_string()))
        })
        .collect();

    let layouts = compute_sidebar_layouts(&session_sidebars, position);
    kill_panes_and_apply_layouts(&session_sidebars, &layouts);
}

/// Find all sidebar panes across all windows. Returns (window_id, pane_id) pairs.
pub(super) fn list_sidebar_panes() -> Vec<(String, String)> {
    let output = Cmd::new("tmux")
        .args(&[
            "list-panes",
            "-a",
            "-F",
            "#{window_id} #{pane_id} #{@workmux_role}",
        ])
        .run_and_capture_stdout()
        .unwrap_or_default();

    output
        .lines()
        .filter_map(|line| {
            let (window_id, rest) = line.split_once(' ')?;
            let (pane_id, role) = rest.split_once(' ')?;
            (role.trim() == SIDEBAR_ROLE_VALUE)
                .then(|| (window_id.to_string(), pane_id.to_string()))
        })
        .collect()
}

/// Kill all sidebar panes and reflow content to fill the window.
///
/// Computes the target layout from the live tree BEFORE killing panes,
/// then applies it after. This preserves pane arrangements the user
/// created while the sidebar was open.
pub(super) fn kill_all_sidebars_and_restore_layouts() {
    let config = crate::config::Config::load(None).unwrap_or_default();
    let position = super::read_sidebar_position(&config);
    let sidebars = list_sidebar_panes();

    let layouts = compute_sidebar_layouts(&sidebars, position);
    kill_panes_and_apply_layouts(&sidebars, &layouts);
}

fn is_own_sidebar(pane_id: &str, own_pane_id: &str) -> bool {
    pane_id == own_pane_id
}

/// Shut down sidebars (called when any sidebar quits).
/// Kills other sidebar panes immediately, then defers our own window's
/// layout reflow so it fires after our process exits and the pane closes.
///
/// In session-scoped mode, only kills sidebars in our session and removes
/// our session from the scope set. Full cleanup (daemon, hooks) only happens
/// when no scoped sessions remain.
pub(super) fn shutdown_all_sidebars(identity: &HostIdentity) {
    let mux = crate::multiplexer::create_backend(crate::multiplexer::detect_backend());
    if mux.name() == "zellij" {
        use std::os::unix::process::CommandExt;
        // The helper must survive closing this pane so it can restore this tab too.
        let result = std::env::current_exe().and_then(|exe| {
            let mut command = std::process::Command::new(exe);
            command.args(["sidebar", "off"]);
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            command.output()
        });
        match result {
            Ok(output) if output.status.success() => {}
            Ok(output) => tracing::warn!(
                stderr = %String::from_utf8_lossy(&output.stderr),
                "failed to stop Zellij sidebars"
            ),
            Err(error) => tracing::warn!(%error, "failed to stop Zellij sidebars"),
        }
        return;
    }

    let config = crate::config::Config::load(None).unwrap_or_default();
    let position = super::read_sidebar_position(&config);
    let our_pane = identity.pane_id.as_str();
    let our_window = identity.window_id.as_str();
    let our_session_id = identity.session_id.as_str();

    let scope = super::current_scope();

    let sidebars = match &scope {
        super::SidebarScope::Sessions(ids) => {
            let our_sid = our_session_id;
            let output = Cmd::new("tmux")
                .args(&[
                    "list-panes",
                    "-a",
                    "-F",
                    "#{session_id} #{window_id} #{pane_id} #{@workmux_role}",
                ])
                .run_and_capture_stdout()
                .unwrap_or_default();
            let sidebars: Vec<_> = output
                .lines()
                .filter_map(|line| {
                    let mut parts = line.splitn(4, ' ');
                    let sid = parts.next()?;
                    let wid = parts.next()?;
                    let pid = parts.next()?;
                    let role = parts.next()?.trim();
                    (role == SIDEBAR_ROLE_VALUE && sid == our_sid)
                        .then(|| (wid.to_string(), pid.to_string()))
                })
                .collect();

            // Update scope: remove our session from the set
            let mut remaining = ids.clone();
            remaining.remove(our_sid);
            if remaining.is_empty() {
                super::set_scope(&super::SidebarScope::Off);
            } else {
                super::set_scope(&super::SidebarScope::Sessions(remaining));
            }

            sidebars
        }
        _ => list_sidebar_panes(),
    };

    let layouts = compute_sidebar_layouts(&sidebars, position);

    let other_sidebars: Vec<(String, String)> = sidebars
        .iter()
        .filter(|(_, pane_id)| !is_own_sidebar(pane_id, our_pane))
        .cloned()
        .collect();
    let other_layouts: Vec<Option<String>> = sidebars
        .iter()
        .enumerate()
        .filter(|(_, (_, pane_id))| !is_own_sidebar(pane_id, our_pane))
        .map(|(i, _)| layouts[i].clone())
        .collect();
    let our_layout = sidebars
        .iter()
        .enumerate()
        .find(|(_, (_, pane_id))| is_own_sidebar(pane_id, our_pane))
        .and_then(|(i, _)| layouts[i].clone());

    kill_panes_and_apply_layouts(&other_sidebars, &other_layouts);

    // Full cleanup only if no scoped sessions remain (or global mode)
    let remaining_scope = super::current_scope();
    let full_cleanup = !matches!(remaining_scope, super::SidebarScope::Sessions(_));

    if full_cleanup {
        kill_daemon();
        remove_hooks();
        super::clear_sidebar_globals();
    }

    // Defer our own window's layout reflow until after our pane closes
    if let Some(layout) = our_layout {
        let cmd = format!("sleep 0.1; tmux select-layout -t {our_window} '{layout}' 2>/dev/null");
        let _ = Cmd::new("tmux").args(&["run-shell", "-b", &cmd]).run();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_sidebar_matches_captured_pane_id() {
        assert!(is_own_sidebar("%12", "%12"));
        assert!(!is_own_sidebar("%13", "%12"));
    }

    #[test]
    fn window_pane_query_finds_sidebar_and_first_target() {
        let output = "%7\t\n%8\tsidebar\n";

        assert_eq!(parse_window_panes(output), (Some("%8"), Some("%7")));
    }

    #[test]
    fn window_pane_query_handles_missing_role_field() {
        assert_eq!(parse_window_panes("%7\n"), (None, Some("%7")));
        assert_eq!(parse_window_panes(""), (None, None));
    }
}
