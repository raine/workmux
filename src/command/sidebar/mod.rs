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
//! - `navigation` - agent navigation shared by every host
//! - `placed` - entry points on Zellij and Rex, where workmux places sidebars
//! - `preferences` - preferences every sidebar on a server shares
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
mod navigation;
mod placed;
mod preferences;
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

pub use self::navigation::NavAction;

use self::daemon_ctrl::signal_daemon;
use self::host::SidebarHost;
use self::placed::Reach;
use self::preferences::GroupingOverride;

const MIN_WIDTH: u16 = 25;
const MAX_WIDTH: u16 = 50;
const MAX_SANE_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 1;
const MAX_HEIGHT: u16 = 5;

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
    preferences::Preferences {
        position: host.store().position(),
        ..Default::default()
    }
    .position_or(config)
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

/// Sidebar extent along `position`'s axis for a window `window_cells` wide
/// (left) or tall (top).
fn resolve_extent_for(
    config: &crate::config::Config,
    position: SidebarPosition,
    window_cells: u16,
    synced: Option<u16>,
) -> u16 {
    match position {
        SidebarPosition::Left => resolve_width_for(config, window_cells, synced),
        SidebarPosition::Top => resolve_height_for(config, window_cells, synced),
    }
}

/// The extent along `position`'s axis the user last gave a sidebar by
/// resizing it, as settings.json keeps it.
fn saved_extent(position: SidebarPosition) -> Option<u16> {
    let settings = crate::state::StateStore::new().ok()?.load_settings().ok()?;
    match position {
        SidebarPosition::Left => settings.sidebar_width,
        SidebarPosition::Top => settings.sidebar_height,
    }
    .filter(|cells| *cells > 0)
}

fn save_extent(position: SidebarPosition, cells: u16) -> Result<()> {
    let store = crate::state::StateStore::new()?;
    let mut settings = store.load_settings()?;
    match position {
        SidebarPosition::Left => settings.sidebar_width = Some(cells),
        SidebarPosition::Top => settings.sidebar_height = Some(cells),
    }
    store.save_settings(&settings)
}

/// Reflow sidebar layouts in all windows (called by a hidden command).
pub fn reflow_all(exclude_window: Option<&str>) -> Result<()> {
    match SidebarHost::detect() {
        Some(SidebarHost::Tmux(tmux)) => tmux::reflow_all(&tmux, exclude_window),
        // Zellij and Rex have no layout reflow, and no host has no sidebars.
        Some(SidebarHost::Zellij { .. } | SidebarHost::Rex(_)) | None => Ok(()),
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
        SidebarHost::Rex(rex) => placed::on_rex(rex, Reach::All, position, width, height),
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
        SidebarHost::Rex(rex) => placed::off_rex(rex, Reach::All),
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
        SidebarHost::Rex(rex) => placed::toggle_rex(rex, Reach::All, position, width, height),
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
        SidebarHost::Rex(rex) => {
            placed::on_rex(rex, Reach::CurrentSession, position, width, height)
        }
    }
}

/// Ensure the sidebar is stopped for the current session.
pub fn off_session() -> Result<()> {
    let (_, host) = sidebar_mux()?;
    match &host {
        SidebarHost::Tmux(tmux) => tmux::off_session(tmux),
        SidebarHost::Zellij { instance_id } => placed::off_zellij(&host, instance_id),
        SidebarHost::Rex(rex) => placed::off_rex(rex, Reach::CurrentSession),
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
        SidebarHost::Rex(rex) => {
            placed::toggle_rex(rex, Reach::CurrentSession, position, width, height)
        }
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
        SidebarHost::Rex(rex) => placed::sync_rex(rex),
    }
}

/// Reflow sidebar layout after a window resize (called by tmux hook).
pub fn reflow(window_id: Option<&str>) -> Result<()> {
    match SidebarHost::detect() {
        Some(SidebarHost::Tmux(tmux)) => tmux::reflow(&tmux, window_id),
        // Zellij and Rex have no layout reflow, and no host has no sidebars.
        Some(SidebarHost::Zellij { .. } | SidebarHost::Rex(_)) | None => Ok(()),
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

fn parse_sidebar_filter_mode(raw: &str) -> Result<app::SidebarFilterMode> {
    match raw.trim().to_lowercase().as_str() {
        "none" | "all" => Ok(app::SidebarFilterMode::None),
        "session" | "project" => Ok(app::SidebarFilterMode::Session),
        other => bail!("invalid sidebar filter mode {other:?}; expected none or session"),
    }
}

fn read_sidebar_filter_mode(host: &SidebarHost) -> app::SidebarFilterMode {
    preferences::filter_mode(host.store().filter())
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

    let grouping = if clear {
        GroupingOverride::Unset
    } else {
        GroupingOverride::from_group_by(match mode {
            Some(m) => parse_sidebar_group_by(m)?,
            None => match host.store().grouping().resolve(configured) {
                Some(_) => None,
                None => configured.or(Some(crate::config::SidebarGroupBy::Project)),
            },
        })
    };

    host.store().set_grouping(grouping)?;

    let store = crate::state::StateStore::new()?;
    let mut settings = store.load_settings()?;
    settings.sidebar_group_by = grouping.value().map(String::from);
    store.save_settings(&settings)?;

    signal_daemon(&host);
    Ok(())
}

/// Navigate to an agent in the daemon's ordered agent list. Respects the
/// sidebar filter mode: when set to "session", only navigates among agents in
/// the current session.
pub fn navigate(action: NavAction) -> Result<()> {
    let (mux, host) = sidebar_mux()?;
    let context = match &host {
        SidebarHost::Tmux(tmux) => tmux::navigation_context(tmux)?,
        SidebarHost::Zellij { .. } | SidebarHost::Rex(_) => {
            placed::navigation_context(&host, mux.as_ref())?
        }
    };
    let target = navigation::select_target(&context, &action)?;
    match &host {
        SidebarHost::Tmux(tmux) => tmux::switch_client(tmux, &target)?,
        SidebarHost::Zellij { .. } | SidebarHost::Rex(_) => mux.switch_to_pane(&target, None)?,
    }
    signal_daemon(&host);
    Ok(())
}

/// Set sidebar filter mode from CLI. Toggles if no mode is given.
pub fn set_filter_mode(mode: Option<&str>) -> Result<()> {
    let (_, host) = sidebar_mux()?;
    let new_mode = match mode {
        Some(m) => parse_sidebar_filter_mode(m)?,
        None => read_sidebar_filter_mode(&host).toggle(),
    };

    host.store().set_filter(new_mode)?;

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
}
