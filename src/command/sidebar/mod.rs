//! Sidebar TUI for monitoring active workmux agents.
//!
//! Uses a daemon process that polls tmux and pushes state snapshots to
//! render-only sidebar clients via Unix socket. Each sidebar pane connects
//! to the daemon and receives updates, enabling instant window-switch response
//! without per-pane polling.
//!
//! # Module structure
//!
//! - `app` - application state and selection logic
//! - `client` - Unix socket client for receiving daemon snapshots
//! - `daemon` - background process that polls tmux and broadcasts snapshots
//! - `daemon_ctrl` - daemon lifecycle (spawn, kill, signal, health checks)
//! - `hooks` - tmux hook installation and removal
//! - `layout_tree` - tmux layout tree parser, reflow, and sidebar removal
//! - `panes` - sidebar pane creation, destruction, and shutdown
//! - `runtime` - TUI event loop
//! - `snapshot` - snapshot data types and builder
//! - `ui` - ratatui rendering (compact and tile layouts)

mod app;
mod client;
mod daemon;
mod daemon_ctrl;
mod hooks;
mod layout_tree;
mod panes;
mod runtime;
mod snapshot;
mod template;
mod ui;

use crate::cmd::Cmd;
use crate::config::SidebarPosition;
use anyhow::{Result, anyhow, bail};

use self::daemon_ctrl::{ensure_daemon_running, kill_daemon, signal_daemon};
use self::hooks::{install_hooks, remove_hooks};
use self::panes::{
    create_sidebar_in_window, create_sidebars_in_all_windows, create_sidebars_in_session,
    find_sidebar_in_window, kill_all_sidebars_and_restore_layouts, kill_sidebars_in_session,
};

const SIDEBAR_ROLE_VALUE: &str = "sidebar";
const MIN_WIDTH: u16 = 25;
const MAX_WIDTH: u16 = 50;
const MAX_SANE_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 1;
const MAX_HEIGHT: u16 = 5;

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

/// Active sidebar scope on this tmux server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SidebarScope {
    /// No sidebar active.
    Off,
    /// Sidebar active in all sessions.
    Global,
    /// Sidebar active in specific sessions (by stable session_id like "$0").
    Sessions(std::collections::HashSet<String>),
}

fn parse_scope(raw: &str, enabled: bool) -> SidebarScope {
    match raw.trim() {
        "" if enabled => SidebarScope::Global,
        "" => SidebarScope::Off,
        "global" => SidebarScope::Global,
        ids => {
            let set: std::collections::HashSet<String> =
                ids.split_whitespace().map(String::from).collect();
            SidebarScope::Sessions(set)
        }
    }
}

/// Read the current sidebar scope from tmux.
pub(super) fn current_scope() -> SidebarScope {
    let raw = Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sidebar_scope"])
        .run_and_capture_stdout()
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let enabled = Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sidebar_enabled"])
        .run_and_capture_stdout()
        .ok()
        .is_some_and(|s| s.trim() == "1");
    parse_scope(&raw, enabled)
}

/// Set the sidebar scope in tmux.
fn set_scope(scope: &SidebarScope) {
    match scope {
        SidebarScope::Off => {
            let _ = Cmd::new("tmux")
                .args(&["set-option", "-gu", "@workmux_sidebar_scope"])
                .run();
        }
        SidebarScope::Global => {
            let _ = Cmd::new("tmux")
                .args(&["set-option", "-g", "@workmux_sidebar_scope", "global"])
                .run();
        }
        SidebarScope::Sessions(ids) => {
            let val: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
            let val = val.join(" ");
            let _ = Cmd::new("tmux")
                .args(&["set-option", "-g", "@workmux_sidebar_scope", &val])
                .run();
        }
    }
}

fn parse_session_id_set(raw: &str) -> std::collections::HashSet<String> {
    raw.split_whitespace().map(String::from).collect()
}

fn serialize_session_id_set(ids: &std::collections::HashSet<String>) -> String {
    let mut val: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    val.sort_unstable();
    val.join(" ")
}

fn current_optout_sessions() -> std::collections::HashSet<String> {
    Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sidebar_optout_sessions"])
        .run_and_capture_stdout()
        .ok()
        .map(|s| parse_session_id_set(&s))
        .unwrap_or_default()
}

fn set_optout_sessions(ids: &std::collections::HashSet<String>) {
    if ids.is_empty() {
        let _ = Cmd::new("tmux")
            .args(&["set-option", "-gu", "@workmux_sidebar_optout_sessions"])
            .run();
    } else {
        let val = serialize_session_id_set(ids);
        let _ = Cmd::new("tmux")
            .args(&["set-option", "-g", "@workmux_sidebar_optout_sessions", &val])
            .run();
    }
}

fn session_opted_out(session_id: &str) -> bool {
    current_optout_sessions().contains(session_id)
}

/// Get the current tmux session's stable ID (e.g., "$0").
/// Check whether a window passes the sidebar scope filter.
///
/// Returns `true` if the window should be processed, `false` if it should be
/// skipped.
fn apply_scope_filter(scope: &SidebarScope, window_id: &str) -> bool {
    let window_session_id = get_window_session_id(window_id);
    match scope {
        SidebarScope::Global => match window_session_id {
            Some(ref window_sid) if session_opted_out(window_sid) => false,
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
fn get_current_session_id() -> Result<String> {
    let s = Cmd::new("tmux")
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
fn get_window_session_id(window_id: &str) -> Option<String> {
    Cmd::new("tmux")
        .args(&["display-message", "-t", window_id, "-p", "#{session_id}"])
        .run_and_capture_stdout()
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Unset all sidebar global tmux options.
fn clear_sidebar_globals() {
    for opt in SIDEBAR_GLOBAL_OPTIONS {
        let _ = Cmd::new("tmux").args(&["set-option", "-gu", opt]).run();
    }
}

fn configured_position(
    config: &crate::config::Config,
    position: Option<SidebarPosition>,
) -> SidebarPosition {
    position.or(config.sidebar.position).unwrap_or_default()
}

pub(super) fn read_sidebar_position(config: &crate::config::Config) -> SidebarPosition {
    if let Ok(output) = Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sidebar_position"])
        .run_and_capture_stdout()
    {
        match output.trim() {
            "top" => return SidebarPosition::Top,
            "left" => return SidebarPosition::Left,
            _ => {}
        }
    }

    configured_position(config, None)
}

fn set_sidebar_position(position: SidebarPosition) {
    let value = match position {
        SidebarPosition::Left => "left",
        SidebarPosition::Top => "top",
    };
    let _ = Cmd::new("tmux")
        .args(&["set-option", "-g", "@workmux_sidebar_position", value])
        .run();
}

fn default_width_for(tw: u16) -> u16 {
    if tw == 0 {
        return MIN_WIDTH;
    }
    (tw * 10 / 100).clamp(MIN_WIDTH, MAX_WIDTH)
}

pub(super) fn width_exceeds_defensive_max(width: u16) -> bool {
    width > MAX_SANE_WIDTH
}

/// Resolve sidebar width for a given terminal/window width.
///
/// Priority: explicit config > synced (persisted resize) > default (10%).
fn resolve_width_for(config: &crate::config::Config, tw: u16, synced_width: Option<u16>) -> u16 {
    if let Some(ref w) = config.sidebar.width {
        let width = w.resolve(tw).max(10);
        return if width_exceeds_defensive_max(width) {
            default_width_for(tw)
        } else {
            width
        };
    }

    if let Some(w) = synced_width
        && !width_exceeds_defensive_max(w)
    {
        let max_w = tw.saturating_sub(10).max(10);
        return w.clamp(10, max_w);
    }

    default_width_for(tw)
}

fn resolve_height_for(config: &crate::config::Config, th: u16, synced_height: Option<u16>) -> u16 {
    let max_h = th.saturating_sub(3).max(1);
    if let Some(ref h) = config.sidebar.height {
        return h.resolve(th).clamp(1, max_h);
    }

    if let Some(h) = synced_height {
        return h.clamp(1, max_h);
    }

    let default = if th == 0 {
        3
    } else {
        (th * 10 / 100).clamp(MIN_HEIGHT, MAX_HEIGHT)
    };
    default.clamp(1, max_h)
}

/// Read the synced sidebar width from tmux global option, falling back to settings.
fn read_sidebar_width() -> Option<u16> {
    if let Ok(output) = Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sidebar_width"])
        .run_and_capture_stdout()
        && let Ok(w) = output.trim().parse::<u16>()
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

fn read_sidebar_height() -> Option<u16> {
    if let Ok(output) = Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sidebar_height"])
        .run_and_capture_stdout()
        && let Ok(h) = output.trim().parse::<u16>()
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
fn set_sidebar_width(width: u16) {
    let _ = Cmd::new("tmux")
        .args(&[
            "set-option",
            "-g",
            "@workmux_sidebar_width",
            &width.to_string(),
        ])
        .run();

    if let Ok(store) = crate::state::StateStore::new()
        && let Ok(mut settings) = store.load_settings()
    {
        settings.sidebar_width = Some(width);
        let _ = store.save_settings(&settings);
    }
}

fn set_sidebar_height(height: u16) {
    let _ = Cmd::new("tmux")
        .args(&[
            "set-option",
            "-g",
            "@workmux_sidebar_height",
            &height.to_string(),
        ])
        .run();

    if let Ok(store) = crate::state::StateStore::new()
        && let Ok(mut settings) = store.load_settings()
    {
        settings.sidebar_height = Some(height);
        let _ = store.save_settings(&settings);
    }
}

/// Resolve effective sidebar width, checking synced width first.
fn effective_width_for(config: &crate::config::Config, window_w: u16) -> u16 {
    let synced = read_sidebar_width();
    resolve_width_for(config, window_w, synced)
}

fn effective_height_for(config: &crate::config::Config, window_h: u16) -> u16 {
    let synced = read_sidebar_height();
    resolve_height_for(config, window_h, synced)
}

fn effective_size_for(
    config: &crate::config::Config,
    position: SidebarPosition,
    window_extent: u16,
) -> u16 {
    match position {
        SidebarPosition::Left => effective_width_for(config, window_extent),
        SidebarPosition::Top => effective_height_for(config, window_extent),
    }
}

/// Reflow all sidebar windows except the given one.
pub(super) fn reflow_all_sidebars_except(exclude_window_id: &str) {
    let config = crate::config::Config::load(None).unwrap_or_default();
    let synced = read_sidebar_width();
    let sidebars = panes::list_sidebar_panes();

    for (window_id, pane_id) in sidebars {
        if window_id == exclude_window_id {
            continue;
        }
        let position = read_sidebar_position(&config);
        let format = match position {
            SidebarPosition::Left => "#{window_width}",
            SidebarPosition::Top => "#{window_height}",
        };
        let window_extent: u16 = Cmd::new("tmux")
            .args(&["display-message", "-t", &window_id, "-p", format])
            .run_and_capture_stdout()
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let size = match position {
            SidebarPosition::Left => resolve_width_for(&config, window_extent, synced),
            SidebarPosition::Top => {
                resolve_height_for(&config, window_extent, read_sidebar_height())
            }
        };
        layout_tree::reflow_after_sidebar_add(&window_id, &pane_id, position, size);
    }
}

/// Reflow sidebar layouts in all windows. Called by the window-resized hook
/// so inactive windows get their sidebar widths corrected without waiting for
/// the user to visit them.
pub fn reflow_all(exclude_window: Option<&str>) -> Result<()> {
    reflow_all_to_window_extent(None, exclude_window)
}

pub(super) fn reflow_all_to_window_extent(
    window_extent: Option<u16>,
    exclude_window: Option<&str>,
) -> Result<()> {
    let scope = current_scope();
    if matches!(scope, SidebarScope::Off) {
        return Ok(());
    }

    let config = crate::config::Config::load(None).unwrap_or_default();
    let position = read_sidebar_position(&config);
    let synced_width = read_sidebar_width();
    let synced_height = read_sidebar_height();

    for (window_id, pane_id) in panes::list_sidebar_panes() {
        if exclude_window.is_some_and(|ex| ex == window_id) {
            continue;
        }

        // Scope filter
        if !apply_scope_filter(&scope, &window_id) {
            continue;
        }

        let format = match position {
            SidebarPosition::Left => "#{window_width}",
            SidebarPosition::Top => "#{window_height}",
        };
        let current_extent = match window_extent {
            Some(extent) => extent,
            None => Cmd::new("tmux")
                .args(&["display-message", "-t", &window_id, "-p", format])
                .run_and_capture_stdout()
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0),
        };
        if current_extent == 0 {
            continue;
        }

        let size = match position {
            SidebarPosition::Left => resolve_width_for(&config, current_extent, synced_width),
            SidebarPosition::Top => resolve_height_for(&config, current_extent, synced_height),
        };
        layout_tree::reflow_after_sidebar_add_to_window_extent(
            &window_id,
            &pane_id,
            position,
            size,
            window_extent,
        );
    }

    Ok(())
}

/// Toggle the sidebar globally across all tmux windows.
pub fn toggle(position: Option<SidebarPosition>) -> Result<()> {
    let config = crate::config::Config::load(None)?;

    if std::env::var("TMUX").is_err() {
        return Err(anyhow!("Sidebar requires tmux"));
    }

    // If session-scoped sidebars are active, clean them up first
    if let SidebarScope::Sessions(_) = current_scope() {
        kill_all_sidebars_and_restore_layouts();
        kill_daemon();
        remove_hooks();
        clear_sidebar_globals();
    }

    // Determine intent based on the current window's state
    let current_window = Cmd::new("tmux")
        .args(&["display-message", "-p", "#{window_id}"])
        .run_and_capture_stdout()?
        .trim()
        .to_string();

    let current_has_sidebar = find_sidebar_in_window(&current_window).unwrap_or(false);

    if current_has_sidebar {
        // Current window has sidebar → toggle OFF globally
        kill_all_sidebars_and_restore_layouts();
        kill_daemon();
        remove_hooks();
        clear_sidebar_globals();
        return Ok(());
    }

    // Mark sidebar as used so the dashboard tip is dismissed
    let _ = std::thread::spawn(crate::tips::mark_sidebar_used);

    // Current window missing sidebar → enable/repair globally
    Cmd::new("tmux")
        .args(&["set-option", "-g", "@workmux_sidebar_enabled", "1"])
        .run()?;
    let position = configured_position(&config, position);
    set_sidebar_position(position);
    set_scope(&SidebarScope::Global);

    // Ensure daemon is running (spawns if needed)
    ensure_daemon_running()?;

    create_sidebars_in_all_windows(&config)?;
    install_hooks()?;

    Ok(())
}

/// Toggle the sidebar for the current tmux session only.
pub fn toggle_session(position: Option<SidebarPosition>) -> Result<()> {
    let config = crate::config::Config::load(None)?;

    if std::env::var("TMUX").is_err() {
        return Err(anyhow!("Sidebar requires tmux"));
    }

    let scope = current_scope();
    let session_id = get_current_session_id()?;

    if matches!(&scope, SidebarScope::Global) {
        let mut optout_sessions = current_optout_sessions();
        if optout_sessions.remove(&session_id) {
            set_optout_sessions(&optout_sessions);
            create_sidebars_in_session(&session_id, &config)?;
        } else {
            optout_sessions.insert(session_id.clone());
            set_optout_sessions(&optout_sessions);
            kill_sidebars_in_session(&session_id);
        }
        return Ok(());
    }

    let current_window = Cmd::new("tmux")
        .args(&["display-message", "-p", "#{window_id}"])
        .run_and_capture_stdout()?
        .trim()
        .to_string();

    let current_has_sidebar = find_sidebar_in_window(&current_window).unwrap_or(false);

    if current_has_sidebar {
        // Toggle OFF for this session
        kill_sidebars_in_session(&session_id);

        // Remove this session from the scope set
        if let SidebarScope::Sessions(mut ids) = scope {
            ids.remove(&session_id);
            if ids.is_empty() {
                // Last session removed: full cleanup
                kill_daemon();
                remove_hooks();
                clear_sidebar_globals();
            } else {
                set_scope(&SidebarScope::Sessions(ids));
            }
        }
        return Ok(());
    }

    // Toggle ON for this session
    let _ = std::thread::spawn(crate::tips::mark_sidebar_used);

    Cmd::new("tmux")
        .args(&["set-option", "-g", "@workmux_sidebar_enabled", "1"])
        .run()?;
    let position = configured_position(&config, position);
    set_sidebar_position(position);

    // Add this session to the scope set
    let new_scope = match scope {
        SidebarScope::Sessions(mut ids) => {
            ids.insert(session_id.clone());
            SidebarScope::Sessions(ids)
        }
        _ => {
            let mut ids = std::collections::HashSet::new();
            ids.insert(session_id.clone());
            SidebarScope::Sessions(ids)
        }
    };
    set_scope(&new_scope);

    ensure_daemon_running()?;
    create_sidebars_in_session(&session_id, &config)?;
    install_hooks()?;

    Ok(())
}

fn get_sidebar_position_and_size(target: &str) -> Result<(SidebarPosition, u16)> {
    let config = crate::config::Config::load(None).unwrap_or_default();
    let position = read_sidebar_position(&config);
    let format = match position {
        SidebarPosition::Left => "#{window_width}",
        SidebarPosition::Top => "#{window_height}",
    };
    let window_extent: u16 = Cmd::new("tmux")
        .args(&["display-message", "-t", target, "-p", format])
        .run_and_capture_stdout()
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let size = effective_size_for(&config, position, window_extent);
    Ok((position, size))
}

/// Resolve window ID from an optional argument, falling back to current window.
fn resolve_target_window(window_id: Option<&str>) -> Result<String> {
    match window_id {
        Some(id) => Ok(id.to_string()),
        None => Ok(Cmd::new("tmux")
            .args(&["display-message", "-p", "#{window_id}"])
            .run_and_capture_stdout()?
            .trim()
            .to_string()),
    }
}

/// Sync sidebar into a window (called by tmux hooks for new windows/sessions).
pub fn sync(window_id: Option<&str>) -> Result<()> {
    let scope = current_scope();
    if matches!(scope, SidebarScope::Off) {
        return Ok(());
    }

    // Ensure daemon is running (may have auto-exited or crashed)
    let _ = ensure_daemon_running();

    let target = resolve_target_window(window_id)?;
    if target.is_empty() {
        return Ok(());
    }

    // Session filter: skip windows not in a scoped session (or on lookup failure)
    if !apply_scope_filter(&scope, &target) {
        return Ok(());
    }

    // Check if this window already has a sidebar
    if find_sidebar_in_window(&target)? {
        return Ok(());
    }

    let (position, size) = get_sidebar_position_and_size(&target)?;
    create_sidebar_in_window(&target, position, size)?;

    Ok(())
}

/// Reflow sidebar layout after a window resize (called by tmux hook).
///
/// Finds the sidebar pane in the target window and runs the layout tree
/// reflow to keep the sidebar at the correct width and content panes balanced.
pub fn reflow(window_id: Option<&str>) -> Result<()> {
    let scope = current_scope();
    if matches!(scope, SidebarScope::Off) {
        return Ok(());
    }

    let target = resolve_target_window(window_id)?;
    if target.is_empty() {
        return Ok(());
    }

    // Session filter: skip windows not in a scoped session (or on lookup failure)
    if !apply_scope_filter(&scope, &target) {
        return Ok(());
    }

    // Find the sidebar pane ID in this window
    let output = Cmd::new("tmux")
        .args(&[
            "list-panes",
            "-t",
            &target,
            "-F",
            "#{pane_id} #{@workmux_role}",
        ])
        .run_and_capture_stdout()?;

    let sidebar_pane_id = output.lines().find_map(|line| {
        let (id, role) = line.split_once(' ')?;
        (role.trim() == SIDEBAR_ROLE_VALUE).then(|| id.to_string())
    });

    let Some(sidebar_pane_id) = sidebar_pane_id else {
        return Ok(());
    };

    let (position, size) = get_sidebar_position_and_size(&target)?;

    layout_tree::reflow_after_sidebar_add(&target, &sidebar_pane_id, position, size);
    Ok(())
}

/// Request a sidebar refresh if the daemon is running.
pub(crate) fn request_refresh() {
    signal_daemon();
}

/// Run the sidebar daemon (called by the hidden `_sidebar-daemon` command).
pub fn run_daemon() -> Result<()> {
    daemon::run()
}

/// Run the sidebar TUI (called by the hidden `_sidebar-run` command).
pub fn run_sidebar() -> Result<()> {
    runtime::run_sidebar()
}

/// Navigation action for sidebar hotkeys.
pub enum NavAction {
    Next,
    Prev,
    Jump(usize),
}

/// Compute the target index for a navigation action given the current index and list length.
fn compute_nav_target(action: &NavAction, current_idx: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match action {
        NavAction::Next => {
            let i = current_idx.unwrap_or(len - 1);
            if i >= len - 1 { 0 } else { i + 1 }
        }
        NavAction::Prev => {
            let i = current_idx.unwrap_or(0);
            if i == 0 { len - 1 } else { i - 1 }
        }
        NavAction::Jump(n) => {
            let idx = n - 1;
            if idx >= len {
                return None;
            }
            idx
        }
    })
}

fn pane_window_ids() -> std::collections::HashMap<String, String> {
    Cmd::new("tmux")
        .args(&["list-panes", "-a", "-F", "#{pane_id}\t#{window_id}"])
        .run_and_capture_stdout()
        .ok()
        .map(|output| {
            output
                .lines()
                .filter_map(|line| {
                    let (pane_id, window_id) = line.split_once('\t')?;
                    Some((pane_id.to_string(), window_id.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn pane_session_ids() -> std::collections::HashMap<String, String> {
    Cmd::new("tmux")
        .args(&["list-panes", "-a", "-F", "#{pane_id}\t#{session_name}"])
        .run_and_capture_stdout()
        .ok()
        .map(|output| {
            output
                .lines()
                .filter_map(|line| {
                    let (pane_id, session_name) = line.split_once('\t')?;
                    Some((pane_id.to_string(), session_name.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_sidebar_filter_mode(raw: &str) -> Result<app::SidebarFilterMode> {
    match raw.trim().to_lowercase().as_str() {
        "none" | "all" => Ok(app::SidebarFilterMode::None),
        "session" | "project" => Ok(app::SidebarFilterMode::Session),
        other => bail!("invalid sidebar filter mode {other:?}; expected none or session"),
    }
}

fn read_sidebar_filter_mode() -> app::SidebarFilterMode {
    if let Ok(output) = Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sidebar_filter"])
        .run_and_capture_stdout()
    {
        let trimmed = output.trim();
        if !trimmed.is_empty() {
            return parse_sidebar_filter_mode(trimmed).unwrap_or_default();
        }
    }

    if let Ok(store) = crate::state::StateStore::new()
        && let Ok(settings) = store.load_settings()
        && let Some(ref mode) = settings.sidebar_filter
    {
        return parse_sidebar_filter_mode(mode).unwrap_or_default();
    }

    app::SidebarFilterMode::default()
}

fn current_listed_window_pane<'a>(
    panes: &'a [&str],
    current_pane_id: &'a str,
    current_window_id: &str,
    pane_window_ids: &std::collections::HashMap<String, String>,
) -> Option<&'a str> {
    if panes.contains(&current_pane_id) {
        return Some(current_pane_id);
    }

    panes.iter().copied().find(|pane_id| {
        pane_window_ids
            .get(*pane_id)
            .is_some_and(|window_id| window_id == current_window_id)
    })
}

fn navigation_anchor_pane<'a>(
    panes: &'a [&str],
    current_pane_id: &'a str,
    current_window_id: &str,
    pane_window_ids: &std::collections::HashMap<String, String>,
) -> Option<&'a str> {
    current_listed_window_pane(panes, current_pane_id, current_window_id, pane_window_ids)
        .or(Some(current_pane_id).filter(|pane_id| panes.contains(pane_id)))
}

/// Navigate to an agent by reading the daemon's ordered agent list from tmux.
/// Respects the sidebar filter mode: when set to "session", only navigates
/// among agents in the current tmux session.
pub fn navigate(action: NavAction) -> Result<()> {
    if std::env::var("TMUX").is_err() {
        return Err(anyhow!("Sidebar requires tmux"));
    }

    let agents_str = Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sidebar_agents"])
        .run_and_capture_stdout()
        .unwrap_or_default();
    let agents_str = agents_str.trim();

    if agents_str.is_empty() {
        anyhow::bail!("no sidebar agents found (is the sidebar running?)");
    }

    // Parse space-separated pane IDs
    let mut panes: Vec<&str> = agents_str.split_whitespace().collect();

    if panes.is_empty() {
        anyhow::bail!("no sidebar agents found");
    }

    // Find current pane/window context
    let current_pane_id = Cmd::new("tmux")
        .args(&["display-message", "-p", "#{pane_id}"])
        .run_and_capture_stdout()
        .unwrap_or_default();
    let current_pane_id = current_pane_id.trim();
    let current_window_id = Cmd::new("tmux")
        .args(&["display-message", "-p", "#{window_id}"])
        .run_and_capture_stdout()
        .unwrap_or_default();
    let current_window_id = current_window_id.trim();
    let pane_window_ids = pane_window_ids();
    let pane_session_ids = pane_session_ids();
    let current_session = Cmd::new("tmux")
        .args(&["display-message", "-p", "#{session_name}"])
        .run_and_capture_stdout()
        .unwrap_or_default();
    let current_session = current_session.trim();

    // Apply session filter if active.
    if read_sidebar_filter_mode() == app::SidebarFilterMode::Session {
        panes.retain(|pane_id| {
            pane_session_ids
                .get(*pane_id)
                .is_some_and(|session| session == current_session)
        });
    }

    if panes.is_empty() {
        anyhow::bail!("no matching agents found");
    }

    let current_anchor =
        navigation_anchor_pane(&panes, current_pane_id, current_window_id, &pane_window_ids);
    let current_idx =
        current_anchor.and_then(|pane_id| panes.iter().position(|&pid| pid == pane_id));

    let len = panes.len();
    let target_idx = match &action {
        NavAction::Jump(n) => compute_nav_target(&action, current_idx, len)
            .ok_or_else(|| anyhow::anyhow!("agent {} out of range (1-{})", n, len))?,
        _ => compute_nav_target(&action, current_idx, len)
            .expect("len > 0 guarantees a result for Next/Prev"),
    };

    let target_pane = panes[target_idx];
    Cmd::new("tmux")
        .args(&["switch-client", "-t", target_pane])
        .run()?;

    signal_daemon();
    Ok(())
}

/// Set sidebar filter mode from CLI. Toggles if no mode is given.
pub fn set_filter_mode(mode: Option<&str>) -> Result<()> {
    let new_mode = match mode {
        Some(m) => parse_sidebar_filter_mode(m)?,
        None => read_sidebar_filter_mode().toggle(),
    };

    // Write to tmux global
    Cmd::new("tmux")
        .args(&[
            "set-option",
            "-g",
            "@workmux_sidebar_filter",
            new_mode.as_str(),
        ])
        .run()?;

    // Persist to settings.json
    let store = crate::state::StateStore::new()?;
    let mut settings = store.load_settings()?;
    settings.sidebar_filter = Some(new_mode.as_str().to_string());
    store.save_settings(&settings)?;

    signal_daemon();
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

    #[test]
    fn parse_sidebar_filter_mode_rejects_invalid_values() {
        assert!(parse_sidebar_filter_mode("sessoin").is_err());
    }

    #[test]
    fn next_wraps_from_last_to_first() {
        assert_eq!(compute_nav_target(&NavAction::Next, Some(2), 3), Some(0));
    }

    #[test]
    fn next_advances_normally() {
        assert_eq!(compute_nav_target(&NavAction::Next, Some(0), 3), Some(1));
        assert_eq!(compute_nav_target(&NavAction::Next, Some(1), 3), Some(2));
    }

    #[test]
    fn next_without_current_wraps_from_last() {
        // No current window match: starts from last, wraps to first
        assert_eq!(compute_nav_target(&NavAction::Next, None, 3), Some(0));
    }

    #[test]
    fn prev_wraps_from_first_to_last() {
        assert_eq!(compute_nav_target(&NavAction::Prev, Some(0), 3), Some(2));
    }

    #[test]
    fn prev_goes_back_normally() {
        assert_eq!(compute_nav_target(&NavAction::Prev, Some(2), 3), Some(1));
        assert_eq!(compute_nav_target(&NavAction::Prev, Some(1), 3), Some(0));
    }

    #[test]
    fn prev_without_current_wraps_to_last() {
        // No current window match: starts from 0, wraps to last
        assert_eq!(compute_nav_target(&NavAction::Prev, None, 3), Some(2));
    }

    #[test]
    fn jump_converts_1_indexed_to_0_indexed() {
        assert_eq!(compute_nav_target(&NavAction::Jump(1), None, 3), Some(0));
        assert_eq!(compute_nav_target(&NavAction::Jump(2), None, 3), Some(1));
        assert_eq!(compute_nav_target(&NavAction::Jump(3), None, 3), Some(2));
    }

    #[test]
    fn jump_out_of_range_returns_none() {
        assert_eq!(compute_nav_target(&NavAction::Jump(4), None, 3), None);
        assert_eq!(compute_nav_target(&NavAction::Jump(10), None, 3), None);
    }

    #[test]
    fn empty_list_returns_none() {
        assert_eq!(compute_nav_target(&NavAction::Next, None, 0), None);
        assert_eq!(compute_nav_target(&NavAction::Prev, None, 0), None);
        assert_eq!(compute_nav_target(&NavAction::Jump(1), None, 0), None);
    }

    #[test]
    fn single_agent_next_stays() {
        assert_eq!(compute_nav_target(&NavAction::Next, Some(0), 1), Some(0));
    }

    #[test]
    fn single_agent_prev_stays() {
        assert_eq!(compute_nav_target(&NavAction::Prev, Some(0), 1), Some(0));
    }

    #[test]
    fn resolve_width_uses_synced_width() {
        let config = crate::config::Config::default();
        assert_eq!(resolve_width_for(&config, 200, Some(40)), 40);
    }

    #[test]
    fn resolve_width_clamps_synced_to_window() {
        let config = crate::config::Config::default();
        assert_eq!(resolve_width_for(&config, 60, Some(80)), 50);
        assert_eq!(resolve_width_for(&config, 200, Some(5)), 10);
    }

    #[test]
    fn resolve_width_clamps_narrow_window() {
        let config = crate::config::Config::default();
        assert_eq!(resolve_width_for(&config, 25, Some(50)), 15);
    }

    #[test]
    fn resolve_width_falls_back_to_default_without_sync() {
        let config = crate::config::Config::default();
        assert_eq!(resolve_width_for(&config, 200, None), 25);
        assert_eq!(resolve_width_for(&config, 500, None), 50);
        assert_eq!(resolve_width_for(&config, 400, None), 40);
    }

    #[test]
    fn resolve_width_uses_default_for_excessive_synced_width() {
        let config = crate::config::Config::default();
        assert_eq!(resolve_width_for(&config, 200, Some(150)), 25);
    }

    #[test]
    fn resolve_width_uses_default_for_excessive_explicit_width() {
        let mut config = crate::config::Config::default();
        config.sidebar.width = Some(crate::config::SidebarWidth::Absolute(150));
        assert_eq!(resolve_width_for(&config, 200, Some(40)), 25);
    }

    #[test]
    fn resolve_width_keeps_explicit_width_at_defensive_max() {
        let mut config = crate::config::Config::default();
        config.sidebar.width = Some(crate::config::SidebarWidth::Absolute(80));
        assert_eq!(resolve_width_for(&config, 200, None), 80);
    }

    #[test]
    fn resolve_height_uses_explicit_config_before_synced_height() {
        let mut config = crate::config::Config::default();
        config.sidebar.height = Some(crate::config::SidebarHeight::Absolute(2));
        assert_eq!(resolve_height_for(&config, 40, Some(1)), 2);
    }

    #[test]
    fn resolve_width_uses_explicit_config_before_synced_width() {
        let mut config = crate::config::Config::default();
        config.sidebar.width = Some(crate::config::SidebarWidth::Percent(10));
        assert_eq!(resolve_width_for(&config, 200, Some(150)), 20);
    }

    #[test]
    fn resolve_width_keeps_explicit_width_upper_bound() {
        let mut config = crate::config::Config::default();
        config.sidebar.width = Some(crate::config::SidebarWidth::Absolute(80));
        assert_eq!(resolve_width_for(&config, 100, None), 80);
    }

    #[test]
    fn current_window_falls_back_to_listed_agent_pane() {
        let panes = vec!["%1", "%2"];
        let pane_window_ids = std::collections::HashMap::from([
            ("%1".to_string(), "@10".to_string()),
            ("%2".to_string(), "@20".to_string()),
        ]);

        assert_eq!(
            current_listed_window_pane(&panes, "%99", "@20", &pane_window_ids),
            Some("%2")
        );
    }

    #[test]
    fn current_listed_pane_wins_over_window_fallback() {
        let panes = vec!["%1", "%2"];
        let pane_window_ids = std::collections::HashMap::from([
            ("%1".to_string(), "@10".to_string()),
            ("%2".to_string(), "@10".to_string()),
        ]);

        assert_eq!(
            current_listed_window_pane(&panes, "%2", "@10", &pane_window_ids),
            Some("%2")
        );
    }

    #[test]
    fn navigation_anchor_uses_agent_in_current_window() {
        let panes = vec!["%1", "%2"];
        let pane_window_ids = std::collections::HashMap::from([
            ("%sidebar".to_string(), "@10".to_string()),
            ("%1".to_string(), "@10".to_string()),
            ("%2".to_string(), "@20".to_string()),
        ]);

        assert_eq!(
            navigation_anchor_pane(&panes, "%sidebar", "@10", &pane_window_ids),
            Some("%1")
        );
    }

    #[test]
    fn navigation_anchor_prefers_current_agent_pane() {
        let panes = vec!["%1", "%2"];
        let pane_window_ids = std::collections::HashMap::from([
            ("%1".to_string(), "@10".to_string()),
            ("%2".to_string(), "@10".to_string()),
        ]);

        assert_eq!(
            navigation_anchor_pane(&panes, "%2", "@10", &pane_window_ids),
            Some("%2")
        );
    }

    #[test]
    fn next_from_sidebar_in_filtered_two_window_session_advances_to_other_window() {
        let panes = vec!["%0", "%317"];
        let pane_window_ids = std::collections::HashMap::from([
            ("%sidebar".to_string(), "@0".to_string()),
            ("%0".to_string(), "@0".to_string()),
            ("%317".to_string(), "@33".to_string()),
        ]);

        let current_idx = navigation_anchor_pane(&panes, "%sidebar", "@0", &pane_window_ids)
            .and_then(|pane_id| panes.iter().position(|&pid| pid == pane_id));

        assert_eq!(current_idx, Some(0));
        assert_eq!(
            compute_nav_target(&NavAction::Next, current_idx, panes.len()),
            Some(1)
        );
    }

    #[test]
    fn next_from_agent_in_filtered_two_window_session_advances_to_other_window() {
        let panes = vec!["%0", "%317"];
        let pane_window_ids = std::collections::HashMap::from([
            ("%0".to_string(), "@0".to_string()),
            ("%317".to_string(), "@33".to_string()),
        ]);

        let current_idx = navigation_anchor_pane(&panes, "%0", "@0", &pane_window_ids)
            .and_then(|pane_id| panes.iter().position(|&pid| pid == pane_id));

        assert_eq!(current_idx, Some(0));
        assert_eq!(
            compute_nav_target(&NavAction::Next, current_idx, panes.len()),
            Some(1)
        );
    }
}
