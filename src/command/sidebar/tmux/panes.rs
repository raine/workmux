//! Sidebar pane creation, destruction, and lifecycle management.

use anyhow::{Result, anyhow};
use tracing::debug;

use crate::config::SidebarPosition;

use super::super::app::HostIdentity;
use super::super::daemon_ctrl::kill_daemon;
use super::super::host::TmuxHost;
use super::super::read_sidebar_position;
use super::hooks::remove_hooks;
use super::layout_tree::{layout_after_sidebar_remove, reflow_after_sidebar_add};
use super::{SIDEBAR_ROLE_VALUE, SidebarScope};

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

fn query_window_panes(tmux: &TmuxHost, window_id: &str) -> Result<String> {
    tmux.cmd()
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
pub(crate) fn find_sidebar_pane_in_window(
    tmux: &TmuxHost,
    window_id: &str,
) -> Result<Option<String>> {
    let output = query_window_panes(tmux, window_id)?;
    Ok(parse_window_panes(&output).0.map(str::to_string))
}

/// Check if a window already has a sidebar pane.
pub(crate) fn find_sidebar_in_window(tmux: &TmuxHost, window_id: &str) -> Result<bool> {
    Ok(find_sidebar_pane_in_window(tmux, window_id)?.is_some())
}

/// Leave an agent selection for a sidebar process before switching to it.
pub(crate) fn set_pending_selection(
    tmux: &TmuxHost,
    sidebar_pane_id: &str,
    agent_pane_id: &str,
) -> Result<()> {
    tmux.cmd()
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

pub(crate) fn clear_pending_selection(tmux: &TmuxHost, sidebar_pane_id: &str) {
    let _ = tmux
        .cmd()
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
pub(crate) fn take_pending_selection(tmux: &TmuxHost, sidebar_pane_id: &str) -> Option<String> {
    let selection = tmux
        .cmd()
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
    clear_pending_selection(tmux, sidebar_pane_id);
    selection
}

/// Create a sidebar pane in a specific window (idempotent).
pub(crate) fn create_sidebar_in_window(
    tmux: &TmuxHost,
    window_id: &str,
    position: SidebarPosition,
    size: u16,
) -> Result<()> {
    let panes =
        query_window_panes(tmux, window_id).or_else(|_| query_window_panes(tmux, window_id))?;
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

    let new_pane_id = tmux
        .cmd()
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

    if let Err(error) = tmux
        .cmd()
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
        let _ = tmux.cmd().args(&["kill-pane", "-t", &new_pane_id]).run();
        return Err(anyhow!(
            "failed to mark sidebar pane {new_pane_id} in window {window_id}: {error}"
        ));
    }

    reflow_after_sidebar_add(tmux, window_id, &new_pane_id, position, size);

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
    tmux: &TmuxHost,
    scope: SidebarWindowScope<'_>,
    position: SidebarPosition,
) -> Result<String> {
    let format = sidebar_window_extent_format(position);
    match scope {
        SidebarWindowScope::All => tmux
            .cmd()
            .args(&["list-windows", "-a", "-F", format])
            .run_and_capture_stdout(),
        SidebarWindowScope::Session(session_id) => tmux
            .cmd()
            .args(&["list-windows", "-t", session_id, "-F", format])
            .run_and_capture_stdout(),
    }
}

/// Create sidebars in all existing tmux windows.
///
/// Computes width per-window from `#{window_width}` so each window gets a
/// sidebar proportional to its own dimensions. Unattached sessions may have
/// stale geometry, but `reflow()` corrects them on reattach.
pub(crate) fn create_sidebars_in_all_windows(
    tmux: &TmuxHost,
    config: &crate::config::Config,
) -> Result<()> {
    let position = read_sidebar_position(&tmux.host(), config);
    let output = list_windows_for_sidebars(tmux, SidebarWindowScope::All, position)?;

    debug!(position = ?position, "create_sidebars_in_all_windows: creating sidebars");
    create_sidebars_from_window_output(tmux, &output, config, position)
}

/// Create sidebars in all windows of a specific session (by session_id).
pub(crate) fn create_sidebars_in_session(
    tmux: &TmuxHost,
    session_id: &str,
    config: &crate::config::Config,
) -> Result<()> {
    let position = read_sidebar_position(&tmux.host(), config);
    let output =
        list_windows_for_sidebars(tmux, SidebarWindowScope::Session(session_id), position)?;

    debug!(session_id, position = ?position, "create_sidebars_in_session: creating sidebars");
    create_sidebars_from_window_output(tmux, &output, config, position)
}

fn create_sidebars_from_window_output(
    tmux: &TmuxHost,
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
        let size = super::effective_size_for(tmux, config, position, window_extent);
        if let Err(error) = create_sidebar_in_window(tmux, window_id, position, size) {
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
    tmux: &TmuxHost,
    sidebars: &[(String, String)],
    position: SidebarPosition,
) -> Vec<Option<String>> {
    sidebars
        .iter()
        .map(|(window_id, pane_id)| layout_after_sidebar_remove(tmux, window_id, pane_id, position))
        .collect()
}

fn kill_panes_and_apply_layouts(
    tmux: &TmuxHost,
    sidebars: &[(String, String)],
    layouts: &[Option<String>],
) {
    for (_, pane_id) in sidebars {
        let _ = tmux.cmd().args(&["kill-pane", "-t", pane_id]).run();
    }
    for (i, (window_id, _)) in sidebars.iter().enumerate() {
        if let Some(layout) = &layouts[i] {
            let _ = tmux
                .cmd()
                .args(&["select-layout", "-t", window_id, layout])
                .run();
        }
    }
}

/// Kill sidebar panes only in a specific session (by session_id).
/// Handles layout restoration for killed panes.
pub(crate) fn kill_sidebars_in_session(tmux: &TmuxHost, session_id: &str) {
    let config = crate::config::Config::load(None).unwrap_or_default();
    let position = read_sidebar_position(&tmux.host(), &config);
    let output = tmux
        .cmd()
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

    let layouts = compute_sidebar_layouts(tmux, &session_sidebars, position);
    kill_panes_and_apply_layouts(tmux, &session_sidebars, &layouts);
}

/// Find all sidebar panes across all windows. Returns (window_id, pane_id) pairs.
pub(crate) fn list_sidebar_panes(tmux: &TmuxHost) -> Vec<(String, String)> {
    let output = tmux
        .cmd()
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
pub(crate) fn kill_all_sidebars_and_restore_layouts(tmux: &TmuxHost) {
    let config = crate::config::Config::load(None).unwrap_or_default();
    let position = read_sidebar_position(&tmux.host(), &config);
    let sidebars = list_sidebar_panes(tmux);

    let layouts = compute_sidebar_layouts(tmux, &sidebars, position);
    kill_panes_and_apply_layouts(tmux, &sidebars, &layouts);
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
pub(crate) fn shutdown_all_sidebars(tmux: &TmuxHost, identity: &HostIdentity) {
    let config = crate::config::Config::load(None).unwrap_or_default();
    let position = read_sidebar_position(&tmux.host(), &config);
    let our_pane = identity.pane_id.as_str();
    let our_window = identity.window_id.as_str();
    let our_session_id = identity.session_id.as_str();

    let scope = super::current_scope(tmux);

    let sidebars = match &scope {
        SidebarScope::Sessions(ids) => {
            let our_sid = our_session_id;
            let output = tmux
                .cmd()
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
                super::set_scope(tmux, &SidebarScope::Off);
            } else {
                super::set_scope(tmux, &SidebarScope::Sessions(remaining));
            }

            sidebars
        }
        _ => list_sidebar_panes(tmux),
    };

    let layouts = compute_sidebar_layouts(tmux, &sidebars, position);

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

    kill_panes_and_apply_layouts(tmux, &other_sidebars, &other_layouts);

    // Full cleanup only if no scoped sessions remain (or global mode)
    let remaining_scope = super::current_scope(tmux);
    let full_cleanup = !matches!(remaining_scope, SidebarScope::Sessions(_));

    if full_cleanup {
        kill_daemon(&tmux.host());
        remove_hooks(tmux);
        super::clear_sidebar_globals(tmux);
    }

    // Defer our own window's layout reflow until after our pane closes
    if let Some(layout) = our_layout {
        let cmd = format!("sleep 0.1; tmux select-layout -t {our_window} '{layout}' 2>/dev/null");
        let _ = tmux.cmd().args(&["run-shell", "-b", &cmd]).run();
    }
}

/// The sidebar's stable identity, from its `TMUX_PANE`.
pub(crate) fn host_identity(tmux: &TmuxHost) -> Option<HostIdentity> {
    let pane_id = std::env::var("TMUX_PANE")
        .ok()
        .filter(|pane_id| !pane_id.is_empty())?;
    let output = tmux
        .cmd()
        .args(&[
            "display-message",
            "-p",
            "-t",
            &pane_id,
            "#{session_name}\t#{session_id}\t#{window_id}\t#{pane_id}",
        ])
        .run_and_capture_stdout()
        .ok()?;

    let identity = parse_host_identity(&output)?;
    (identity.pane_id == pane_id).then_some(identity)
}

fn parse_host_identity(output: &str) -> Option<HostIdentity> {
    let mut parts = output.trim().split('\t');
    let identity = HostIdentity {
        session_name: parts.next()?.to_string(),
        session_id: parts.next()?.to_string(),
        window_id: parts.next()?.to_string(),
        pane_id: parts.next()?.to_string(),
    };
    if parts.next().is_some()
        || identity.session_name.is_empty()
        || identity.session_id.is_empty()
        || identity.window_id.is_empty()
        || identity.pane_id.is_empty()
    {
        return None;
    }

    Some(identity)
}

fn pane_kill_command(pane_id: &str) -> String {
    format!(
        "sleep 0.05; tmux kill-pane -t {} 2>/dev/null || true",
        crate::shell::shell_quote(pane_id)
    )
}

/// Close a sidebar pane shortly after its client exits.
pub(crate) fn close_pane_after_exit(tmux: &TmuxHost, pane_id: &str) {
    let cmd = pane_kill_command(pane_id);
    let _ = tmux.cmd().args(&["run-shell", "-b", &cmd]).run();
}

fn sole_pane_is_sidebar(output: &str, pane_id: &str) -> bool {
    let mut panes = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    panes.next() == Some(pane_id) && panes.next().is_none()
}

/// Whether the sidebar pane is the only pane left in its window.
pub(crate) fn sidebar_is_only_pane(tmux: &TmuxHost, window_id: &str, pane_id: &str) -> bool {
    tmux.cmd()
        .args(&["list-panes", "-t", window_id, "-F", "#{pane_id}"])
        .run_and_capture_stdout()
        .is_ok_and(|output| sole_pane_is_sidebar(&output, pane_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_host_identity() {
        let identity = parse_host_identity("main\t$1\t@42\t%94").unwrap();

        assert_eq!(identity.session_name, "main");
        assert_eq!(identity.session_id, "$1");
        assert_eq!(identity.window_id, "@42");
        assert_eq!(identity.pane_id, "%94");
    }

    #[test]
    fn rejects_incomplete_host_identity() {
        assert!(parse_host_identity("main\t$1\t@42").is_none());
        assert!(parse_host_identity("main\t\t@42\t%94").is_none());
    }

    #[test]
    fn pane_kill_command_targets_captured_sidebar_pane() {
        assert_eq!(
            pane_kill_command("%12"),
            "sleep 0.05; tmux kill-pane -t '%12' 2>/dev/null || true"
        );
    }

    #[test]
    fn sole_pane_confirmation_requires_sidebar_identity() {
        assert!(sole_pane_is_sidebar("%12\n", "%12"));
        assert!(!sole_pane_is_sidebar("%12\n%13\n", "%12"));
        assert!(!sole_pane_is_sidebar("%13\n", "%12"));
        assert!(!sole_pane_is_sidebar("", "%12"));
    }

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
