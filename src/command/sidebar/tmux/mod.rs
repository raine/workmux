//! Sidebar code that runs only on tmux.
//!
//! Everything here takes a `TmuxHost`, which exists only when tmux hosts the
//! sidebar, so other backends cannot reach it.

pub(super) mod hooks;
pub(super) mod layout_tree;
pub(super) mod panes;
pub(super) mod resize;

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, bail};

use crate::config::{Config, SidebarHeight, SidebarPosition, SidebarWidth};

use self::hooks::{install_hooks, remove_hooks};
use self::panes::{
    create_sidebar_in_window, create_sidebars_in_all_windows, create_sidebars_in_session,
    find_sidebar_in_window, kill_all_sidebars_and_restore_layouts, kill_sidebars_in_session,
};
use super::app::SidebarFilterMode;
use super::daemon_ctrl::{ensure_daemon_running, kill_daemon, signal_daemon};
use super::host::TmuxHost;
use super::view::WindowView;
use super::{
    NavAction, apply_cli_dimensions, compute_nav_target, configured_position,
    navigation_anchor_pane, read_sidebar_filter_mode, read_sidebar_position, resolve_height_for,
    resolve_width_for,
};

pub(super) const SIDEBAR_ROLE_VALUE: &str = "sidebar";
/// Pane option holding the extent the last layout reflow gave a sidebar.
/// A sidebar resized to this extent was resized by workmux, not by the user.
pub(super) const SIDEBAR_APPLIED_SIZE_OPTION: &str = "@workmux_sidebar_applied_size";

/// Global tmux options set while the sidebar is active.
const SIDEBAR_GLOBAL_OPTIONS: &[&str] = &[
    "@workmux_sidebar_enabled",
    "@workmux_sidebar_agents",
    "@workmux_sleeping_panes",
    "@workmux_sidebar_scope",
    "@workmux_sidebar_width",
    "@workmux_sidebar_height",
    "@workmux_sidebar_position",
    "@workmux_sidebar_optout_sessions",
];

/// Global option holding the sidebar scope.
pub(super) const SCOPE_OPTION: &str = "@workmux_sidebar_scope";

/// Window option holding the view a window's sidebar shows.
pub(super) const VIEW_OPTION: &str = "@workmux_sidebar_scope";

/// Active sidebar scope on this tmux server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SidebarScope {
    /// No sidebar active.
    Off,
    /// Sidebar active in all sessions.
    Global,
    /// Sidebar active in specific sessions (by stable session_id like "$0").
    Sessions(HashSet<String>),
}

fn parse_scope(raw: &str, enabled: bool) -> SidebarScope {
    match raw.trim() {
        "" if enabled => SidebarScope::Global,
        "" => SidebarScope::Off,
        "global" => SidebarScope::Global,
        ids => SidebarScope::Sessions(parse_session_id_set(ids)),
    }
}

/// Read the current sidebar scope.
pub(super) fn current_scope(tmux: &TmuxHost) -> SidebarScope {
    let raw = tmux.option(SCOPE_OPTION).unwrap_or_default();
    let enabled = tmux
        .option("@workmux_sidebar_enabled")
        .is_some_and(|value| value == "1");
    parse_scope(&raw, enabled)
}

/// Set the sidebar scope.
pub(super) fn set_scope(tmux: &TmuxHost, scope: &SidebarScope) {
    let _ = match scope {
        SidebarScope::Off => tmux.set_option(SCOPE_OPTION, None),
        SidebarScope::Global => tmux.set_option(SCOPE_OPTION, Some("global")),
        SidebarScope::Sessions(ids) => {
            let val: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
            tmux.set_option(SCOPE_OPTION, Some(&val.join(" ")))
        }
    };
}

fn parse_session_id_set(raw: &str) -> HashSet<String> {
    raw.split_whitespace().map(String::from).collect()
}

fn serialize_session_id_set(ids: &HashSet<String>) -> String {
    let mut val: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    val.sort_unstable();
    val.join(" ")
}

fn current_optout_sessions(tmux: &TmuxHost) -> HashSet<String> {
    tmux.option("@workmux_sidebar_optout_sessions")
        .map(|s| parse_session_id_set(&s))
        .unwrap_or_default()
}

fn set_optout_sessions(tmux: &TmuxHost, ids: &HashSet<String>) {
    let val = serialize_session_id_set(ids);
    let _ = tmux.set_option(
        "@workmux_sidebar_optout_sessions",
        (!ids.is_empty()).then_some(val.as_str()),
    );
}

fn session_opted_out(tmux: &TmuxHost, session_id: &str) -> bool {
    current_optout_sessions(tmux).contains(session_id)
}

/// Check whether a window passes the sidebar scope filter.
///
/// Returns `true` if the window should be processed, `false` if it should be
/// skipped.
fn apply_scope_filter(tmux: &TmuxHost, scope: &SidebarScope, window_id: &str) -> bool {
    let window_session_id = get_window_session_id(tmux, window_id);
    match scope {
        SidebarScope::Global => match window_session_id {
            Some(ref window_sid) if session_opted_out(tmux, window_sid) => false,
            Some(_) => true,
            None => false,
        },
        SidebarScope::Sessions(ids) => {
            matches!(window_session_id, Some(ref window_sid) if ids.contains(window_sid))
        }
        SidebarScope::Off => false,
    }
}

/// Get the current tmux session's stable ID (e.g., "$0").
fn get_current_session_id(tmux: &TmuxHost) -> Result<String> {
    let s = tmux
        .cmd()
        .args(&["display-message", "-p", "#{session_id}"])
        .run_and_capture_stdout()?
        .trim()
        .to_string();
    if s.is_empty() {
        return Err(anyhow!("could not detect tmux session"));
    }
    Ok(s)
}

/// Get the session_id a window belongs to.
fn get_window_session_id(tmux: &TmuxHost, window_id: &str) -> Option<String> {
    tmux.cmd()
        .args(&["display-message", "-t", window_id, "-p", "#{session_id}"])
        .run_and_capture_stdout()
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Unset all sidebar global tmux options.
pub(super) fn clear_sidebar_globals(tmux: &TmuxHost) {
    for opt in SIDEBAR_GLOBAL_OPTIONS {
        let _ = tmux.set_option(opt, None);
    }
}

fn set_sidebar_position(tmux: &TmuxHost, position: SidebarPosition) {
    let _ = tmux.host().store().set_position(position);
}

/// Read the synced sidebar width from tmux global option, falling back to settings.
fn read_sidebar_width(tmux: &TmuxHost) -> Option<u16> {
    if let Some(w) = tmux
        .option("@workmux_sidebar_width")
        .and_then(|output| output.parse::<u16>().ok())
        && w > 0
    {
        return Some(w);
    }

    if let Ok(store) = crate::state::StateStore::new()
        && let Ok(settings) = store.load_settings()
    {
        return settings.sidebar_width;
    }

    None
}

fn read_sidebar_height(tmux: &TmuxHost) -> Option<u16> {
    if let Some(h) = tmux
        .option("@workmux_sidebar_height")
        .and_then(|output| output.parse::<u16>().ok())
        && h > 0
    {
        return Some(h);
    }

    if let Ok(store) = crate::state::StateStore::new()
        && let Ok(settings) = store.load_settings()
    {
        return settings.sidebar_height;
    }

    None
}

/// Set the synced sidebar width in tmux global option and persist to settings.
pub(super) fn set_sidebar_width(tmux: &TmuxHost, width: u16) {
    let _ = tmux.set_option("@workmux_sidebar_width", Some(&width.to_string()));

    if let Ok(store) = crate::state::StateStore::new()
        && let Ok(mut settings) = store.load_settings()
    {
        settings.sidebar_width = Some(width);
        let _ = store.save_settings(&settings);
    }
}

pub(super) fn set_sidebar_height(tmux: &TmuxHost, height: u16) {
    let _ = tmux.set_option("@workmux_sidebar_height", Some(&height.to_string()));

    if let Ok(store) = crate::state::StateStore::new()
        && let Ok(mut settings) = store.load_settings()
    {
        settings.sidebar_height = Some(height);
        let _ = store.save_settings(&settings);
    }
}

/// Resolve effective sidebar width, checking synced width first.
pub(super) fn effective_width_for(tmux: &TmuxHost, config: &Config, window_w: u16) -> u16 {
    resolve_width_for(config, window_w, read_sidebar_width(tmux))
}

pub(super) fn effective_height_for(tmux: &TmuxHost, config: &Config, window_h: u16) -> u16 {
    resolve_height_for(config, window_h, read_sidebar_height(tmux))
}

fn effective_size_for(
    tmux: &TmuxHost,
    config: &Config,
    position: SidebarPosition,
    window_extent: u16,
) -> u16 {
    match position {
        SidebarPosition::Left => effective_width_for(tmux, config, window_extent),
        SidebarPosition::Top => effective_height_for(tmux, config, window_extent),
    }
}

/// A window's extent along the sidebar's axis, or 0 when tmux can't say.
fn window_extent(tmux: &TmuxHost, window_id: &str, position: SidebarPosition) -> u16 {
    let format = match position {
        SidebarPosition::Left => "#{window_width}",
        SidebarPosition::Top => "#{window_height}",
    };
    tmux.cmd()
        .args(&["display-message", "-t", window_id, "-p", format])
        .run_and_capture_stdout()
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Reflow all sidebar windows except the given one.
pub(super) fn reflow_all_sidebars_except(tmux: &TmuxHost, exclude_window_id: &str) {
    let config = Config::load(None).unwrap_or_default();
    let position = read_sidebar_position(&tmux.host(), &config);
    let synced = match position {
        SidebarPosition::Left => read_sidebar_width(tmux),
        SidebarPosition::Top => read_sidebar_height(tmux),
    };

    for (window_id, pane_id) in panes::list_sidebar_panes(tmux) {
        if window_id == exclude_window_id {
            continue;
        }
        let extent = window_extent(tmux, &window_id, position);
        let size = match position {
            SidebarPosition::Left => resolve_width_for(&config, extent, synced),
            SidebarPosition::Top => resolve_height_for(&config, extent, synced),
        };
        layout_tree::reflow_after_sidebar_add(tmux, &window_id, &pane_id, position, size);
    }
}

/// Reflow sidebar layouts in all windows.
pub(super) fn reflow_all(tmux: &TmuxHost, exclude_window: Option<&str>) -> Result<()> {
    let scope = current_scope(tmux);
    if matches!(scope, SidebarScope::Off) {
        return Ok(());
    }

    let config = Config::load(None).unwrap_or_default();
    let position = read_sidebar_position(&tmux.host(), &config);
    let synced_width = read_sidebar_width(tmux);
    let synced_height = read_sidebar_height(tmux);

    for (window_id, pane_id) in panes::list_sidebar_panes(tmux) {
        if exclude_window.is_some_and(|ex| ex == window_id) {
            continue;
        }

        // Scope filter
        if !apply_scope_filter(tmux, &scope, &window_id) {
            continue;
        }

        let current_extent = window_extent(tmux, &window_id, position);
        if current_extent == 0 {
            continue;
        }

        let size = match position {
            SidebarPosition::Left => resolve_width_for(&config, current_extent, synced_width),
            SidebarPosition::Top => resolve_height_for(&config, current_extent, synced_height),
        };
        layout_tree::reflow_after_sidebar_add(tmux, &window_id, &pane_id, position, size);
    }

    Ok(())
}

pub(super) fn stop_all(tmux: &TmuxHost) {
    kill_all_sidebars_and_restore_layouts(tmux);
    kill_daemon(&tmux.host());
    remove_hooks(tmux);
    clear_sidebar_globals(tmux);
}

fn enable(tmux: &TmuxHost) -> Result<()> {
    tmux.cmd()
        .args(&["set-option", "-g", "@workmux_sidebar_enabled", "1"])
        .run()?;
    Ok(())
}

/// Ensure the sidebar is running globally across all tmux windows.
pub(super) fn on(
    tmux: &TmuxHost,
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    let host = tmux.host();
    let mut config = Config::load(None)?;
    let scope = current_scope(tmux);
    let appearance_changed = position.is_some() || width.is_some() || height.is_some();
    apply_cli_dimensions(&mut config, width, height);
    let position = if position.is_none() && matches!(&scope, SidebarScope::Global) {
        read_sidebar_position(&host, &config)
    } else {
        configured_position(&config, position)
    };

    if matches!(&scope, SidebarScope::Sessions(_)) {
        stop_all(tmux);
    } else if appearance_changed && matches!(&scope, SidebarScope::Global) {
        kill_all_sidebars_and_restore_layouts(tmux);
    }

    let _ = std::thread::spawn(crate::tips::mark_sidebar_used);
    enable(tmux)?;
    set_sidebar_position(tmux, position);
    set_scope(tmux, &SidebarScope::Global);
    ensure_daemon_running(&host)?;
    create_sidebars_in_all_windows(tmux, &config)?;
    install_hooks(tmux)?;
    Ok(())
}

/// The current window: the caller's, even if a client is focused elsewhere.
pub(super) fn current_window(tmux: &TmuxHost) -> Result<String> {
    let pane = std::env::var("TMUX_PANE").ok();
    let mut args = vec!["display-message", "-p"];
    if let Some(pane) = pane.as_deref() {
        args.extend(["-t", pane]);
    }
    args.push("#{window_id}");
    Ok(tmux
        .cmd()
        .args(&args)
        .run_and_capture_stdout()?
        .trim()
        .to_string())
}

/// The view a window's sidebar shows, as the daemon published it.
pub(super) fn window_view(tmux: &TmuxHost, window: &str) -> Option<WindowView> {
    let value = tmux
        .cmd()
        .args(&["show-option", "-wqv", "-t", window, VIEW_OPTION])
        .run_and_capture_stdout()
        .ok()?;
    serde_json::from_str(&value).ok()
}

pub(super) fn set_window_view(
    tmux: &TmuxHost,
    window: &str,
    view: Option<&WindowView>,
) -> Result<()> {
    match view {
        Some(view) => {
            tmux.backend()
                .set_window_option(window, VIEW_OPTION, &serde_json::to_string(view)?)
        }
        None => tmux.backend().unset_window_option(window, VIEW_OPTION),
    }
}

fn focused_window(tmux: &TmuxHost) -> Result<String> {
    Ok(tmux
        .cmd()
        .args(&["display-message", "-p", "#{window_id}"])
        .run_and_capture_stdout()?
        .trim()
        .to_string())
}

/// Toggle the sidebar globally across all tmux windows.
pub(super) fn toggle(
    tmux: &TmuxHost,
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    // Determine intent based on the current window's state
    let current_window = focused_window(tmux)?;
    if find_sidebar_in_window(tmux, &current_window).unwrap_or(false) {
        stop_all(tmux);
        Ok(())
    } else {
        on(tmux, position, width, height)
    }
}

/// Ensure the sidebar is running for the current tmux session.
pub(super) fn on_session(
    tmux: &TmuxHost,
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    let host = tmux.host();
    let mut config = Config::load(None)?;
    let appearance_changed = position.is_some() || width.is_some() || height.is_some();
    apply_cli_dimensions(&mut config, width, height);
    let scope = current_scope(tmux);
    let session_id = get_current_session_id(tmux)?;
    let position = if position.is_none() && !matches!(&scope, SidebarScope::Off) {
        read_sidebar_position(&host, &config)
    } else {
        configured_position(&config, position)
    };

    if matches!(&scope, SidebarScope::Global) {
        let mut optouts = current_optout_sessions(tmux);
        optouts.remove(&session_id);
        set_optout_sessions(tmux, &optouts);
        if appearance_changed {
            kill_sidebars_in_session(tmux, &session_id);
            set_sidebar_position(tmux, position);
        }
        create_sidebars_in_session(tmux, &session_id, &config)?;
        ensure_daemon_running(&host)?;
        install_hooks(tmux)?;
        return Ok(());
    }

    if appearance_changed {
        kill_sidebars_in_session(tmux, &session_id);
    }
    enable(tmux)?;
    set_sidebar_position(tmux, position);
    let mut ids = match scope {
        SidebarScope::Sessions(ids) => ids,
        _ => HashSet::new(),
    };
    ids.insert(session_id.clone());
    set_scope(tmux, &SidebarScope::Sessions(ids));
    let _ = std::thread::spawn(crate::tips::mark_sidebar_used);
    ensure_daemon_running(&host)?;
    create_sidebars_in_session(tmux, &session_id, &config)?;
    install_hooks(tmux)?;
    Ok(())
}

/// Ensure the sidebar is stopped for the current tmux session.
pub(super) fn off_session(tmux: &TmuxHost) -> Result<()> {
    let scope = current_scope(tmux);
    let session_id = get_current_session_id(tmux)?;
    match scope {
        SidebarScope::Global => {
            let mut optouts = current_optout_sessions(tmux);
            optouts.insert(session_id.clone());
            set_optout_sessions(tmux, &optouts);
            kill_sidebars_in_session(tmux, &session_id);
        }
        SidebarScope::Sessions(mut ids) => {
            kill_sidebars_in_session(tmux, &session_id);
            ids.remove(&session_id);
            if ids.is_empty() {
                stop_all(tmux);
            } else {
                set_scope(tmux, &SidebarScope::Sessions(ids));
            }
        }
        SidebarScope::Off => {}
    }
    Ok(())
}

/// Toggle the sidebar for the current tmux session only.
pub(super) fn toggle_session(
    tmux: &TmuxHost,
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    let scope = current_scope(tmux);
    let session_id = get_current_session_id(tmux)?;

    if matches!(&scope, SidebarScope::Global) {
        return if session_opted_out(tmux, &session_id) {
            on_session(tmux, position, width, height)
        } else {
            off_session(tmux)
        };
    }

    let current_window = focused_window(tmux)?;
    if find_sidebar_in_window(tmux, &current_window).unwrap_or(false) {
        off_session(tmux)
    } else {
        on_session(tmux, position, width, height)
    }
}

fn get_sidebar_position_and_size(tmux: &TmuxHost, target: &str) -> (SidebarPosition, u16) {
    let config = Config::load(None).unwrap_or_default();
    let position = read_sidebar_position(&tmux.host(), &config);
    let extent = window_extent(tmux, target, position);
    let size = effective_size_for(tmux, &config, position, extent);
    (position, size)
}

/// Resolve window ID from an optional argument, falling back to current window.
fn resolve_target_window(tmux: &TmuxHost, window_id: Option<&str>) -> Result<String> {
    match window_id {
        Some(id) => Ok(id.to_string()),
        None => focused_window(tmux),
    }
}

/// Sync sidebar into a window (called by tmux hooks for new windows/sessions).
pub(super) fn sync(tmux: &TmuxHost, window_id: Option<&str>) -> Result<()> {
    let scope = current_scope(tmux);
    if matches!(scope, SidebarScope::Off) {
        return Ok(());
    }

    // Ensure daemon is running (may have auto-exited or crashed)
    let _ = ensure_daemon_running(&tmux.host());

    let target = resolve_target_window(tmux, window_id)?;
    if target.is_empty() {
        return Ok(());
    }

    // Session filter: skip windows not in a scoped session (or on lookup failure)
    if !apply_scope_filter(tmux, &scope, &target) {
        return Ok(());
    }

    // Check if this window already has a sidebar
    if find_sidebar_in_window(tmux, &target)? {
        return Ok(());
    }

    let (position, size) = get_sidebar_position_and_size(tmux, &target);
    create_sidebar_in_window(tmux, &target, position, size)?;

    Ok(())
}

/// Reflow sidebar layout after a window resize (called by tmux hook).
///
/// Finds the sidebar pane in the target window and runs the layout tree
/// reflow to keep the sidebar at the correct width and content panes balanced.
pub(super) fn reflow(tmux: &TmuxHost, window_id: Option<&str>) -> Result<()> {
    let scope = current_scope(tmux);
    if matches!(scope, SidebarScope::Off) {
        return Ok(());
    }

    let target = resolve_target_window(tmux, window_id)?;
    if target.is_empty() {
        return Ok(());
    }

    // Session filter: skip windows not in a scoped session (or on lookup failure)
    if !apply_scope_filter(tmux, &scope, &target) {
        return Ok(());
    }

    let Some(sidebar_pane_id) = panes::find_sidebar_pane_in_window(tmux, &target)? else {
        return Ok(());
    };

    let (position, size) = get_sidebar_position_and_size(tmux, &target);
    layout_tree::reflow_after_sidebar_add(tmux, &target, &sidebar_pane_id, position, size);
    Ok(())
}

/// Navigate to an agent in the order the daemon publishes to tmux.
/// Respects the sidebar filter mode: when set to "session", only navigates
/// among agents in the current tmux session.
pub(super) fn navigate(tmux: &TmuxHost, action: NavAction) -> Result<()> {
    // Resolve the caller's window even if another client is focused elsewhere.
    let current_window_id = current_window(tmux)?;
    let current_window_id = current_window_id.as_str();
    let caller_pane = std::env::var("TMUX_PANE").ok();
    let current_pane_id = tmux
        .cmd()
        .args(&[
            "display-message",
            "-p",
            "-t",
            caller_pane.as_deref().unwrap_or(current_window_id),
            "#{pane_id}",
        ])
        .run_and_capture_stdout()
        .unwrap_or_default();
    let current_pane_id = current_pane_id.trim();
    let mut pane_window_ids = HashMap::new();
    let mut pane_session_ids = HashMap::new();
    let listing = tmux
        .cmd()
        .args(&[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{window_id}\t#{session_name}",
        ])
        .run_and_capture_stdout()
        .unwrap_or_default();
    for line in listing.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(pane), Some(window), Some(session)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        pane_window_ids.insert(pane.to_string(), window.to_string());
        pane_session_ids.insert(pane.to_string(), session.to_string());
    }
    let current_session = tmux
        .cmd()
        .args(&[
            "display-message",
            "-p",
            "-t",
            current_window_id,
            "#{session_name}",
        ])
        .run_and_capture_stdout()
        .unwrap_or_default();
    let current_session = current_session.trim();

    let store = tmux.host().store();
    let agents = store.ordered_agents();
    let mut panes: Vec<&str> = agents.iter().map(String::as_str).collect();
    if panes.is_empty() {
        bail!("no sidebar agents found (is the sidebar running?)");
    }

    if let Some(view) = store.window_view(current_window_id) {
        panes.retain(|pane| view.pane_ids.contains(*pane));
    }

    if read_sidebar_filter_mode(&tmux.host()) == SidebarFilterMode::Session {
        panes.retain(|pane_id| {
            pane_session_ids
                .get(*pane_id)
                .is_some_and(|session| session == current_session)
        });
    }

    if panes.is_empty() {
        bail!("no matching agents found");
    }

    let current_anchor =
        navigation_anchor_pane(&panes, current_pane_id, current_window_id, &pane_window_ids);
    let current_idx =
        current_anchor.and_then(|pane_id| panes.iter().position(|&pid| pid == pane_id));

    let len = panes.len();
    let target_idx = match &action {
        NavAction::Jump(n) => compute_nav_target(&action, current_idx, len)
            .ok_or_else(|| anyhow!("agent {} out of range (1-{})", n, len))?,
        _ => compute_nav_target(&action, current_idx, len)
            .expect("len > 0 guarantees a result for Next/Prev"),
    };

    tmux.cmd()
        .args(&["switch-client", "-t", panes[target_idx]])
        .run()?;

    signal_daemon(&tmux.host());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_id_set() {
        let ids = parse_session_id_set("$2  $0\n$1");
        assert_eq!(ids.len(), 3);
        assert!(ids.contains("$0"));
        assert!(ids.contains("$1"));
        assert!(ids.contains("$2"));
    }

    #[test]
    fn empty_scope_with_enabled_flag_is_global() {
        assert_eq!(parse_scope("", true), SidebarScope::Global);
        assert_eq!(parse_scope("", false), SidebarScope::Off);
    }

    #[test]
    fn serializes_session_id_set_deterministically() {
        let ids = parse_session_id_set("$2 $0 $1");
        assert_eq!(serialize_session_id_set(&ids), "$0 $1 $2");
    }
}
