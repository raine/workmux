//! Sidebar TUI for monitoring active workmux agents.
//!
//! Uses a daemon process that polls the active multiplexer and pushes snapshots to
//! render-only sidebar clients via Unix socket. Each sidebar pane connects
//! to the daemon and receives updates, enabling instant window-switch response
//! without per-pane polling.
//!
//! Backend-specific behaviour goes through `host::SidebarHost`: every entry
//! point resolves the host once and dispatches on it exhaustively.
//!
//! # Module structure
//!
//! - `app` - application state and selection logic
//! - `client` - Unix socket client for receiving daemon snapshots
//! - `daemon` - background process that polls multiplexer state and broadcasts snapshots
//! - `daemon_ctrl` - daemon lifecycle (spawn, kill, signal, health checks)
//! - `host` - the multiplexer hosting the sidebars, and its behaviour
//! - `placed` - entry points on Zellij and Rex, where workmux places sidebars
//! - `rex_panes` - sidebar placement, sizing, and scope on Rex
//! - `runtime` - TUI event loop
//! - `snapshot` - snapshot data types and builder
//! - `store` - where shared sidebar state lives on each host
//! - `tmux` - entry points, hooks, layout reflow, and sidebar panes on tmux
//! - `ui` - ratatui rendering (compact and tile layouts)
//! - `zellij_panes` - sidebar placement on Zellij

mod app;
mod client;
mod daemon;
mod daemon_ctrl;
#[cfg(target_os = "macos")]
mod fsevent;
mod host;
mod placed;
mod rex_panes;
mod runtime;
mod session_state;
mod snapshot;
mod store;
mod template;
mod tmux;
mod ui;
mod view;
mod zellij_panes;

use crate::config::{SidebarHeight, SidebarPosition, SidebarWidth};
use crate::multiplexer::{Multiplexer, create_backend, detect_backend_strict};
use anyhow::{Result, anyhow, bail};

pub(crate) use self::app::templates_use_prompt;

use self::daemon_ctrl::signal_daemon;
use self::host::SidebarHost;
use self::placed::Reach;

const MIN_WIDTH: u16 = 25;
const MAX_WIDTH: u16 = 50;
const MAX_SANE_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 1;
const MAX_HEIGHT: u16 = 5;

/// Navigate using the agent order the daemon keeps in the session state file.
fn navigate_from_state(host: &SidebarHost, mux: &dyn Multiplexer, action: NavAction) -> Result<()> {
    let state = session_state::read(&host.instance_id())?;
    let mut panes: Vec<String> = state.ordered_agents;
    if panes.is_empty() {
        bail!("no sidebar agents found (is the sidebar running?)");
    }

    let live = mux.get_all_live_pane_info()?;
    let current_pane_id = mux
        .active_pane_id()
        .or_else(|| mux.current_pane_id())
        .unwrap_or_default();
    let current_window_id = live
        .get(&current_pane_id)
        .and_then(|info| info.window_id.clone())
        .unwrap_or_default();
    let pane_window_ids: std::collections::HashMap<String, String> = live
        .iter()
        .filter_map(|(pane_id, info)| Some((pane_id.clone(), info.window_id.as_ref()?.clone())))
        .collect();

    if let Some(view) = state.window_views.get(&current_window_id) {
        panes.retain(|pane| view.pane_ids.contains(pane));
    }

    if read_sidebar_filter_mode(host) == app::SidebarFilterMode::Session {
        // Rex session labels need not be unique, so Rex compares IDs.
        let rex = matches!(host, SidebarHost::Rex { .. });
        let current_session = if rex {
            std::env::var("REX_SESSION").unwrap_or_default()
        } else {
            host.instance_id()
        };
        panes.retain(|pane_id| {
            live.get(pane_id)
                .and_then(|info| {
                    if rex {
                        info.session_id.as_deref()
                    } else {
                        info.session.as_deref()
                    }
                })
                .is_none_or(|session| session == current_session)
        });
    }
    if panes.is_empty() {
        bail!("no matching agents found");
    }

    let pane_refs: Vec<&str> = panes.iter().map(String::as_str).collect();
    let current_anchor = navigation_anchor_pane(
        &pane_refs,
        &current_pane_id,
        &current_window_id,
        &pane_window_ids,
    );
    let current_idx = current_anchor
        .and_then(|pane_id| pane_refs.iter().position(|candidate| *candidate == pane_id));
    let target_idx = match &action {
        NavAction::Jump(n) => compute_nav_target(&action, current_idx, pane_refs.len())
            .ok_or_else(|| anyhow!("agent {} out of range (1-{})", n, pane_refs.len()))?,
        _ => compute_nav_target(&action, current_idx, pane_refs.len())
            .expect("non-empty pane list guarantees a navigation target"),
    };

    mux.switch_to_pane(pane_refs[target_idx], None)?;
    signal_daemon(host);
    Ok(())
}

fn configured_position(
    config: &crate::config::Config,
    position: Option<SidebarPosition>,
) -> SidebarPosition {
    position.or(config.sidebar.position).unwrap_or_default()
}

pub(super) fn read_sidebar_position(
    host: &SidebarHost,
    config: &crate::config::Config,
) -> SidebarPosition {
    host.store()
        .position()
        .unwrap_or_else(|| configured_position(config, None))
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
        let width = w.resolve(tw).max(1);
        return if width_exceeds_defensive_max(width) {
            default_width_for(tw)
        } else {
            width
        };
    }

    if let Some(w) = synced_width
        && !width_exceeds_defensive_max(w)
    {
        let max_w = tw.saturating_sub(10).max(1);
        return w.clamp(1, max_w);
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

/// Reflow sidebar layouts in all windows (called by a hidden command).
pub fn reflow_all(exclude_window: Option<&str>) -> Result<()> {
    match SidebarHost::detect() {
        Some(SidebarHost::Tmux(tmux)) => tmux::reflow_all(&tmux, exclude_window),
        // Zellij and Rex have no layout reflow, and no host has no sidebars.
        Some(SidebarHost::Zellij { .. } | SidebarHost::Rex { .. }) | None => Ok(()),
    }
}

fn apply_cli_dimensions(
    config: &mut crate::config::Config,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) {
    if width.is_some() {
        config.sidebar.width = width;
    }
    if height.is_some() {
        config.sidebar.height = height;
    }
}

/// The multiplexer the sidebar commands act on, and its sidebar host.
fn sidebar_mux() -> Result<(std::sync::Arc<dyn Multiplexer>, SidebarHost)> {
    let mux = create_backend(detect_backend_strict()?);
    let host = SidebarHost::resolve(mux.as_ref())?;
    if !mux.is_running().unwrap_or(false) {
        return Err(anyhow!("{} is not running", mux.name()));
    }
    Ok((mux, host))
}

/// Ensure the sidebar is running globally across all windows.
pub fn on(
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    let (_, host) = sidebar_mux()?;
    match &host {
        SidebarHost::Tmux(tmux) => tmux::on(tmux, position, width, height),
        // Zellij sidebars always cover the whole session.
        SidebarHost::Zellij { instance_id } => {
            placed::on_zellij(&host, instance_id, position, width, height)
        }
        SidebarHost::Rex { instance_id } => {
            placed::on_rex(&host, instance_id, Reach::All, position, width, height)
        }
    }
}

/// Ensure the global sidebar is stopped.
pub fn off() -> Result<()> {
    let (_, host) = sidebar_mux()?;
    match &host {
        SidebarHost::Tmux(tmux) => {
            tmux::stop_all(tmux);
            Ok(())
        }
        SidebarHost::Zellij { instance_id } => placed::off_zellij(&host, instance_id),
        SidebarHost::Rex { instance_id } => placed::off_rex(&host, instance_id, Reach::All),
    }
}

/// Toggle the sidebar globally across all windows.
pub fn toggle(
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    let (_, host) = sidebar_mux()?;
    match &host {
        SidebarHost::Tmux(tmux) => tmux::toggle(tmux, position, width, height),
        SidebarHost::Zellij { instance_id } => {
            placed::toggle_zellij(&host, instance_id, position, width, height)
        }
        SidebarHost::Rex { instance_id } => {
            placed::toggle_rex(&host, instance_id, Reach::All, position, width, height)
        }
    }
}

/// Ensure the sidebar is running for the current session.
pub fn on_session(
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    let (_, host) = sidebar_mux()?;
    match &host {
        SidebarHost::Tmux(tmux) => tmux::on_session(tmux, position, width, height),
        SidebarHost::Zellij { instance_id } => {
            placed::on_zellij(&host, instance_id, position, width, height)
        }
        SidebarHost::Rex { instance_id } => placed::on_rex(
            &host,
            instance_id,
            Reach::CurrentSession,
            position,
            width,
            height,
        ),
    }
}

/// Ensure the sidebar is stopped for the current session.
pub fn off_session() -> Result<()> {
    let (_, host) = sidebar_mux()?;
    match &host {
        SidebarHost::Tmux(tmux) => tmux::off_session(tmux),
        SidebarHost::Zellij { instance_id } => placed::off_zellij(&host, instance_id),
        SidebarHost::Rex { instance_id } => {
            placed::off_rex(&host, instance_id, Reach::CurrentSession)
        }
    }
}

/// Toggle the sidebar for the current session only.
pub fn toggle_session(
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    let (_, host) = sidebar_mux()?;
    match &host {
        SidebarHost::Tmux(tmux) => tmux::toggle_session(tmux, position, width, height),
        SidebarHost::Zellij { instance_id } => {
            placed::toggle_zellij(&host, instance_id, position, width, height)
        }
        SidebarHost::Rex { instance_id } => placed::toggle_rex(
            &host,
            instance_id,
            Reach::CurrentSession,
            position,
            width,
            height,
        ),
    }
}

/// Sync sidebar into a window (called by tmux hooks for new windows/sessions,
/// and on Zellij and Rex to place a missing sidebar).
pub fn sync(window_id: Option<&str>) -> Result<()> {
    let Some(host) = SidebarHost::detect() else {
        return Ok(());
    };
    match &host {
        SidebarHost::Tmux(tmux) => tmux::sync(tmux, window_id),
        SidebarHost::Zellij { instance_id } => placed::sync_zellij(instance_id),
        SidebarHost::Rex { instance_id } => placed::sync_rex(&host, instance_id),
    }
}

/// Reflow sidebar layout after a window resize (called by tmux hook).
pub fn reflow(window_id: Option<&str>) -> Result<()> {
    match SidebarHost::detect() {
        Some(SidebarHost::Tmux(tmux)) => tmux::reflow(&tmux, window_id),
        // Zellij and Rex have no layout reflow, and no host has no sidebars.
        Some(SidebarHost::Zellij { .. } | SidebarHost::Rex { .. }) | None => Ok(()),
    }
}

/// Request a sidebar refresh if the daemon is running.
pub(crate) fn request_refresh_for(mux: &dyn crate::multiplexer::Multiplexer) {
    if let Ok(host) = SidebarHost::resolve(mux) {
        signal_daemon(&host);
    }
}

/// Run the sidebar daemon (called by the hidden `_sidebar-daemon` command).
pub fn run_daemon() -> Result<()> {
    daemon::run()
}

/// Run the sidebar TUI (called by the hidden `_sidebar-run` command).
pub fn run_sidebar() -> Result<()> {
    runtime::run_sidebar()
}

/// Turn rule-based views on or off for every sidebar, or toggle them.
pub fn set_views_enabled(mode: Option<&str>) -> Result<()> {
    let (_, host) = sidebar_mux()?;
    view::set_enabled(&host, mode)
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

fn parse_sidebar_filter_mode(raw: &str) -> Result<app::SidebarFilterMode> {
    match raw.trim().to_lowercase().as_str() {
        "none" | "all" => Ok(app::SidebarFilterMode::None),
        "session" | "project" => Ok(app::SidebarFilterMode::Session),
        other => bail!("invalid sidebar filter mode {other:?}; expected none or session"),
    }
}

fn read_sidebar_filter_mode(host: &SidebarHost) -> app::SidebarFilterMode {
    if let Some(mode) = host.store().filter() {
        return parse_sidebar_filter_mode(&mode).unwrap_or_default();
    }

    if let Ok(store) = crate::state::StateStore::new()
        && let Ok(settings) = store.load_settings()
        && let Some(ref mode) = settings.sidebar_filter
    {
        return parse_sidebar_filter_mode(mode).unwrap_or_default();
    }

    app::SidebarFilterMode::default()
}

/// Grouping accepted by the runtime override. `none` is the ungrouped list the
/// sidebar had before grouping existed.
pub(crate) fn parse_sidebar_group_by(raw: &str) -> Result<Option<crate::config::SidebarGroupBy>> {
    match raw.trim().to_lowercase().as_str() {
        "none" | "off" | "flat" => Ok(None),
        "project" => Ok(Some(crate::config::SidebarGroupBy::Project)),
        "session" => Ok(Some(crate::config::SidebarGroupBy::Session)),
        other => bail!("invalid sidebar grouping {other:?}; expected none, project or session"),
    }
}

pub(crate) fn group_by_option_value(
    group_by: Option<crate::config::SidebarGroupBy>,
) -> &'static str {
    match group_by {
        None | Some(crate::config::SidebarGroupBy::None) => "none",
        Some(crate::config::SidebarGroupBy::Project) => "project",
        Some(crate::config::SidebarGroupBy::Session) => "session",
    }
}

/// Set sidebar grouping from CLI. With no mode, toggles between the configured
/// grouping and a flat list. With `clear`, drops the runtime choice so the
/// config file decides again.
///
/// A runtime choice outlives the session that made it, so without a way back
/// an editor's `group_by` would silently stop taking effect.
pub fn set_group_by(mode: Option<&str>, clear: bool) -> Result<()> {
    let (_, host) = sidebar_mux()?;
    let configured = crate::config::Config::load(None)
        .map(|cfg| cfg.sidebar.group_by())
        .unwrap_or_default();

    let new_mode = if clear {
        None
    } else {
        match mode {
            Some(m) => Some(parse_sidebar_group_by(m)?),
            None => Some(match read_sidebar_group_by(&host, configured) {
                Some(_) => None,
                None => configured.or(Some(crate::config::SidebarGroupBy::Project)),
            }),
        }
    };

    host.store()
        .set_group_by(new_mode.map(group_by_option_value))?;

    let store = crate::state::StateStore::new()?;
    let mut settings = store.load_settings()?;
    settings.sidebar_group_by = new_mode.map(|mode| group_by_option_value(mode).to_string());
    store.save_settings(&settings)?;

    signal_daemon(&host);
    Ok(())
}

/// Grouping currently in effect: the backend override, else the persisted one,
/// else what the config asks for.
fn read_sidebar_group_by(
    host: &SidebarHost,
    configured: Option<crate::config::SidebarGroupBy>,
) -> Option<crate::config::SidebarGroupBy> {
    if let Some(mode) = host.store().group_by() {
        return parse_sidebar_group_by(&mode).unwrap_or(configured);
    }

    if let Ok(store) = crate::state::StateStore::new()
        && let Ok(settings) = store.load_settings()
        && let Some(ref mode) = settings.sidebar_group_by
    {
        return parse_sidebar_group_by(mode).unwrap_or(configured);
    }

    configured
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

/// Navigate to an agent in the daemon's ordered agent list. Respects the
/// sidebar filter mode: when set to "session", only navigates among agents in
/// the current session.
pub fn navigate(action: NavAction) -> Result<()> {
    let (mux, host) = sidebar_mux()?;
    match &host {
        SidebarHost::Tmux(tmux) => tmux::navigate(tmux, action),
        SidebarHost::Zellij { .. } | SidebarHost::Rex { .. } => {
            navigate_from_state(&host, mux.as_ref(), action)
        }
    }
}

/// Set sidebar filter mode from CLI. Toggles if no mode is given.
pub fn set_filter_mode(mode: Option<&str>) -> Result<()> {
    let (_, host) = sidebar_mux()?;
    let new_mode = match mode {
        Some(m) => parse_sidebar_filter_mode(m)?,
        None => read_sidebar_filter_mode(&host).toggle(),
    };

    host.store().set_filter(new_mode.as_str())?;

    // Persist to settings.json
    let store = crate::state::StateStore::new()?;
    let mut settings = store.load_settings()?;
    settings.sidebar_filter = Some(new_mode.as_str().to_string());
    store.save_settings(&settings)?;

    signal_daemon(&host);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sidebar_group_by_accepts_every_runtime_value() {
        assert_eq!(parse_sidebar_group_by("none").unwrap(), None);
        assert_eq!(parse_sidebar_group_by("Off").unwrap(), None);
        assert_eq!(
            parse_sidebar_group_by(" Project ").unwrap(),
            Some(crate::config::SidebarGroupBy::Project)
        );
        assert_eq!(
            parse_sidebar_group_by("session").unwrap(),
            Some(crate::config::SidebarGroupBy::Session)
        );
        assert!(parse_sidebar_group_by("porject").is_err());
    }

    #[test]
    fn group_by_option_values_round_trip() {
        for mode in [
            None,
            Some(crate::config::SidebarGroupBy::Project),
            Some(crate::config::SidebarGroupBy::Session),
        ] {
            assert_eq!(
                parse_sidebar_group_by(group_by_option_value(mode)).unwrap(),
                mode
            );
        }
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
    fn resolve_width_preserves_narrow_explicit_widths() {
        let mut config = crate::config::Config::default();
        for (width, expected) in [(0, 1), (1, 1), (8, 8)] {
            config.sidebar.width = Some(crate::config::SidebarWidth::Absolute(width));
            assert_eq!(resolve_width_for(&config, 200, Some(40)), expected);
        }
    }

    #[test]
    fn resolve_width_preserves_narrow_synced_widths() {
        let config = crate::config::Config::default();
        for (width, expected) in [(0, 1), (1, 1), (8, 8)] {
            assert_eq!(resolve_width_for(&config, 200, Some(width)), expected);
        }
    }

    #[test]
    fn resolve_width_preserves_narrow_percentages() {
        let mut config = crate::config::Config::default();
        config.sidebar.width = Some(crate::config::SidebarWidth::Percent(4));
        assert_eq!(resolve_width_for(&config, 200, None), 8);
        assert_eq!(resolve_width_for(&config, 20, None), 1);
    }

    #[test]
    fn resolve_width_reserves_content_space_in_small_windows() {
        let config = crate::config::Config::default();
        for (window, expected) in [(18, 8), (15, 5), (11, 1), (10, 1), (0, 1)] {
            assert_eq!(resolve_width_for(&config, window, Some(8)), expected);
        }
    }

    #[test]
    fn resolve_width_clamps_synced_to_window() {
        let config = crate::config::Config::default();
        assert_eq!(resolve_width_for(&config, 60, Some(80)), 50);
        assert_eq!(resolve_width_for(&config, 200, Some(5)), 5);
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
