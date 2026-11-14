//! Sidebar entry points on backends where workmux places sidebars itself
//! instead of through multiplexer hooks: Zellij and Rex.

use std::collections::HashMap;

use anyhow::{Result, anyhow};

use crate::config::{Config, SidebarHeight, SidebarPosition, SidebarWidth};
use crate::multiplexer::Multiplexer;

use super::app::HostIdentity;
use super::app::SidebarFilterMode;
use super::daemon_ctrl::{ensure_daemon_running, kill_daemon};
use super::host::{RexHost, SidebarHost};
use super::navigation::NavigationContext;
use super::{apply_cli_dimensions, configured_position, rex_panes, session_state, zellij_panes};

/// Which sessions a scope change reaches.
#[derive(Clone, Copy)]
pub(super) enum Reach {
    All,
    CurrentSession,
}

pub(super) fn on_zellij(
    host: &SidebarHost,
    instance_id: &str,
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    let mut config = Config::load(None)?;
    apply_cli_dimensions(&mut config, width, height);
    let position = configured_position(&config, position);
    let previous = session_state::read(instance_id).unwrap_or_default();
    if previous
        .prefs
        .position
        .is_some_and(|current| current != position)
    {
        zellij_panes::kill_all_sidebars(instance_id)?;
    }
    let width = config.sidebar.width.clone();
    let height = config.sidebar.height.clone();
    session_state::update(instance_id, |state| {
        state.scope.include(None);
        state.prefs.position = Some(position);
        state.width.clone_from(&width);
        state.height.clone_from(&height);
    })?;
    let _ = std::thread::spawn(crate::tips::mark_sidebar_used);
    ensure_daemon_running(host)?;
    zellij_panes::create_sidebars_in_all_tabs(instance_id, position, true)
}

pub(super) fn off_zellij(host: &SidebarHost, instance_id: &str) -> Result<()> {
    session_state::update(instance_id, |state| state.scope.exclude(None))?;
    kill_daemon(host);
    zellij_panes::kill_all_sidebars(instance_id)
}

/// Zellij sidebars always cover the whole session, so a toggle looks at
/// the current tab only.
pub(super) fn toggle_zellij(
    host: &SidebarHost,
    instance_id: &str,
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    if zellij_panes::current_tab_has_sidebar(instance_id).unwrap_or(false) {
        off_zellij(host, instance_id)
    } else {
        on_zellij(host, instance_id, position, width, height)
    }
}

pub(super) fn sync_zellij(instance_id: &str) -> Result<()> {
    zellij_panes::create_sidebar_in_active_tab(instance_id)
}

fn current_rex_session() -> Result<String> {
    std::env::var("REX_SESSION")
        .ok()
        .filter(|session| !session.trim().is_empty())
        .ok_or_else(|| anyhow!("run this inside a Rex terminal to target its session"))
}

fn reach_session(reach: Reach) -> Result<Option<String>> {
    match reach {
        Reach::All => Ok(None),
        Reach::CurrentSession => current_rex_session().map(Some),
    }
}

pub(super) fn on_rex(
    rex: &RexHost,
    reach: Reach,
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    let instance_id = rex.instance_id();
    let session = reach_session(reach)?;
    let previous = session_state::read(instance_id)?;
    // A running sidebar keeps its size unless the command names one.
    let set_width = width.is_some() || !previous.is_active();
    let set_height = height.is_some() || !previous.is_active();
    let mut config = Config::load(None)?;
    apply_cli_dimensions(&mut config, width, height);
    let position = match position {
        Some(position) => position,
        None if previous.is_active() => configured_position(&config, previous.prefs.position),
        None => configured_position(&config, None),
    };
    let observation = rex.backend().sidebar_observation()?;
    session_state::update(instance_id, |state| {
        state.scope.include(session.as_deref());
        rex_panes::clear_dismissals(state, &observation, session.as_deref());
        state.prefs.position = Some(position);
        if set_width {
            state.width.clone_from(&config.sidebar.width);
        }
        if set_height {
            state.height.clone_from(&config.sidebar.height);
        }
    })?;
    let _ = std::thread::spawn(crate::tips::mark_sidebar_used);
    ensure_daemon_running(&rex.host())?;
    rex_panes::sync(rex)?;
    Ok(())
}

pub(super) fn off_rex(rex: &RexHost, reach: Reach) -> Result<()> {
    let session = reach_session(reach)?;
    let state = session_state::update(rex.instance_id(), |state| {
        state.scope.exclude(session.as_deref());
    })?;
    rex_panes::sync(rex)?;
    if !state.is_active() {
        kill_daemon(&rex.host());
    }
    Ok(())
}

/// Whether the sidebar shows where a toggle with this reach looks: the
/// current window, or the current session's scope.
fn rex_shown(rex: &RexHost, reach: Reach) -> Result<bool> {
    let state = session_state::read(rex.instance_id())?;
    match reach {
        Reach::CurrentSession => Ok(state.covers_session(&current_rex_session()?)),
        Reach::All if std::env::var_os("REX_BLOCK").is_none() => Ok(state.is_active()),
        Reach::All => {
            let observation = rex.backend().sidebar_observation()?;
            Ok(rex_panes::current_window_has_sidebar(&observation, &state))
        }
    }
}

pub(super) fn toggle_rex(
    rex: &RexHost,
    reach: Reach,
    position: Option<SidebarPosition>,
    width: Option<SidebarWidth>,
    height: Option<SidebarHeight>,
) -> Result<()> {
    if rex_shown(rex, reach)? {
        off_rex(rex, reach)
    } else {
        on_rex(rex, reach, position, width, height)
    }
}

pub(super) fn sync_rex(rex: &RexHost) -> Result<()> {
    if session_state::read(rex.instance_id())?.is_active() {
        let _ = ensure_daemon_running(&rex.host());
        rex_panes::sync(rex)?;
    }
    Ok(())
}

/// Turn the sidebar off after the user quit one, from a helper process.
///
/// Under a Rex session scope, quitting leaves the other sessions' sidebars.
pub(super) fn stop_after_quit(host: &SidebarHost, identity: &HostIdentity) {
    use std::os::unix::process::CommandExt;
    let session_scoped = matches!(host, SidebarHost::Rex(_))
        && session_state::read(&host.instance_id()).is_ok_and(|state| {
            !state.scope.enabled && state.scope.sessions.contains(&identity.session_id)
        });
    // The helper must survive closing this pane so it can restore this tab too.
    let result = std::env::current_exe().and_then(|exe| {
        let mut command = std::process::Command::new(exe);
        if session_scoped {
            command.args(["sidebar", "--session", "off"]);
        } else {
            command.args(["sidebar", "off"]);
        }
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
            "failed to stop sidebars"
        ),
        Err(error) => tracing::warn!(%error, "failed to stop sidebars"),
    }
}

/// What navigation needs from a Zellij or Rex server.
pub(super) fn navigation_context(
    host: &SidebarHost,
    mux: &dyn Multiplexer,
) -> Result<NavigationContext> {
    let live = mux.get_all_live_pane_info()?;
    let current_pane = mux
        .active_pane_id()
        .or_else(|| mux.current_pane_id())
        .unwrap_or_default();
    let current_window = live
        .get(&current_pane)
        .and_then(|info| info.window_id.clone())
        .unwrap_or_default();
    let pane_windows: HashMap<String, String> = live
        .iter()
        .filter_map(|(pane, info)| Some((pane.clone(), info.window_id.clone()?)))
        .collect();
    let pane_sessions: HashMap<String, String> = live
        .iter()
        .filter_map(|(pane, info)| Some((pane.clone(), host.session_key(info)?.to_string())))
        .collect();
    // One read, so the filter, agents and view come from the same state.
    let mut state = session_state::read(&host.instance_id()).unwrap_or_default();
    // An unknown caller session matches no agent.
    let session_filter = (state.prefs.filter_mode() == SidebarFilterMode::Session)
        .then(|| host.current_session().unwrap_or_default());
    Ok(NavigationContext {
        agents: std::mem::take(&mut state.ordered_agents),
        view: state.window_views.remove(&current_window),
        current_pane,
        current_window,
        pane_windows,
        pane_sessions,
        session_filter,
    })
}
