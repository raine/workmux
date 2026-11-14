//! Where the sidebar keeps its shared runtime preferences and what the
//! daemon publishes for commands and clients.
//!
//! tmux keeps them in global `@workmux_*` options. Backends without
//! user options (Zellij, Rex) keep them in the per-instance state file.

use anyhow::Result;
use std::collections::{HashMap, HashSet};

use crate::config::SidebarPosition;

use super::host::TmuxHost;
use super::session_state;
use super::view::WindowView;

const AGENTS_OPTION: &str = "@workmux_sidebar_agents";
const DAEMON_PID_OPTION: &str = "@workmux_sidebar_daemon_pid";
const SLEEPING_OPTION: &str = "@workmux_sleeping_panes";

/// Window views as last published, so the daemon writes only changes.
#[derive(Default)]
pub(super) struct PublishedViews {
    /// tmux keeps one option per window.
    windows: HashMap<String, Option<WindowView>>,
    /// The state file keeps every window's view in one field.
    file: Option<HashMap<String, WindowView>>,
}

pub(super) enum SidebarStore {
    TmuxOptions(TmuxHost),
    File { instance_id: String },
}

impl SidebarStore {
    pub(super) fn position(&self) -> Option<SidebarPosition> {
        match self {
            Self::TmuxOptions(tmux) => match tmux.option("@workmux_sidebar_position")?.as_str() {
                "top" => Some(SidebarPosition::Top),
                "left" => Some(SidebarPosition::Left),
                _ => None,
            },
            Self::File { instance_id } => session_state::read(instance_id).ok()?.position,
        }
    }

    pub(super) fn set_position(&self, position: SidebarPosition) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => tmux.set_option(
                "@workmux_sidebar_position",
                Some(match position {
                    SidebarPosition::Left => "left",
                    SidebarPosition::Top => "top",
                }),
            ),
            Self::File { instance_id } => {
                session_state::update(instance_id, |state| state.position = Some(position))
                    .map(|_| ())
            }
        }
    }

    pub(super) fn filter(&self) -> Option<String> {
        match self {
            Self::TmuxOptions(tmux) => tmux.option("@workmux_sidebar_filter"),
            Self::File { instance_id } => session_state::read(instance_id).ok()?.filter,
        }
    }

    pub(super) fn set_filter(&self, filter: &str) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => tmux.set_option("@workmux_sidebar_filter", Some(filter)),
            Self::File { instance_id } => session_state::update(instance_id, |state| {
                state.filter = Some(filter.to_string());
            })
            .map(|_| ()),
        }
    }

    pub(super) fn views_enabled(&self) -> bool {
        match self {
            Self::TmuxOptions(tmux) => {
                super::view::is_enabled(tmux.option(super::view::OPTION).as_deref())
            }
            Self::File { instance_id } => session_state::read(instance_id)
                .ok()
                .and_then(|state| state.views_enabled)
                .unwrap_or(true),
        }
    }

    pub(super) fn set_views_enabled(&self, enabled: bool) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => tmux.set_option(
                super::view::OPTION,
                Some(if enabled { "on" } else { "off" }),
            ),
            Self::File { instance_id } => session_state::update(instance_id, |state| {
                state.views_enabled = Some(enabled);
            })
            .map(|_| ()),
        }
    }

    pub(super) fn group_by(&self) -> Option<String> {
        match self {
            Self::TmuxOptions(tmux) => tmux.option("@workmux_sidebar_group_by"),
            Self::File { instance_id } => session_state::read(instance_id).ok()?.group_by,
        }
    }

    /// `None` drops the runtime choice so the config decides again.
    pub(super) fn set_group_by(&self, group_by: Option<&str>) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => tmux.set_option("@workmux_sidebar_group_by", group_by),
            Self::File { instance_id } => session_state::update(instance_id, |state| {
                state.group_by = group_by.map(str::to_string);
            })
            .map(|_| ()),
        }
    }

    pub(super) fn set_layout(&self, layout: &str) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => tmux.set_option("@workmux_sidebar_layout", Some(layout)),
            Self::File { instance_id } => session_state::update(instance_id, |state| {
                state.layout = Some(layout.to_string());
            })
            .map(|_| ()),
        }
    }

    /// Flip one pane's sleeping state and return the new set.
    pub(super) fn toggle_sleeping(&self, pane_id: &str) -> Result<HashSet<String>> {
        let flip = |panes: &mut HashSet<String>| {
            if !panes.insert(pane_id.to_string()) {
                panes.remove(pane_id);
            }
        };
        match self {
            Self::TmuxOptions(tmux) => {
                let mut panes: HashSet<String> = tmux
                    .option(SLEEPING_OPTION)
                    .map(|value| value.split_whitespace().map(String::from).collect())
                    .unwrap_or_default();
                flip(&mut panes);
                let value = panes.iter().cloned().collect::<Vec<_>>().join(" ");
                tmux.set_option(
                    SLEEPING_OPTION,
                    (!value.is_empty()).then_some(value.as_str()),
                )?;
                Ok(panes)
            }
            Self::File { instance_id } => {
                session_state::update(instance_id, |state| flip(&mut state.sleeping_panes))
                    .map(|state| state.sleeping_panes)
            }
        }
    }

    pub(super) fn set_expanded_groups(&self, labels: &[&str]) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => {
                let value = labels.join("\t");
                tmux.set_option(
                    "@workmux_sidebar_expanded",
                    (!value.is_empty()).then_some(value.as_str()),
                )
            }
            Self::File { instance_id } => session_state::update(instance_id, |state| {
                state.expanded_groups = labels.iter().map(|label| (*label).to_string()).collect();
            })
            .map(|_| ()),
        }
    }

    pub(super) fn daemon_pid(&self) -> Option<String> {
        match self {
            Self::TmuxOptions(tmux) => tmux.option(DAEMON_PID_OPTION),
            Self::File { instance_id } => session_state::read(instance_id)
                .ok()?
                .daemon_pid
                .map(|pid| pid.to_string()),
        }
    }

    pub(super) fn clear_daemon_pid(&self) {
        match self {
            Self::TmuxOptions(tmux) => {
                let _ = tmux.set_option(DAEMON_PID_OPTION, None);
            }
            Self::File { instance_id } => {
                let _ = session_state::update(instance_id, |state| state.daemon_pid = None);
            }
        }
    }

    /// Record this process as the running daemon.
    pub(super) fn record_daemon_pid(&self) -> Result<()> {
        let pid = std::process::id();
        match self {
            Self::TmuxOptions(tmux) => tmux.set_option(DAEMON_PID_OPTION, Some(&pid.to_string())),
            Self::File { instance_id } => {
                session_state::update(instance_id, |state| state.daemon_pid = Some(pid)).map(|_| ())
            }
        }
    }

    /// The agent panes navigation steps through, in sidebar order.
    pub(super) fn ordered_agents(&self) -> Vec<String> {
        match self {
            Self::TmuxOptions(tmux) => tmux
                .option(AGENTS_OPTION)
                .map(|value| value.split_whitespace().map(String::from).collect())
                .unwrap_or_default(),
            Self::File { instance_id } => session_state::read(instance_id)
                .map(|state| state.ordered_agents)
                .unwrap_or_default(),
        }
    }

    pub(super) fn set_ordered_agents(&self, panes: &[&str]) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => {
                let value = panes.join(" ");
                tmux.set_option(AGENTS_OPTION, (!value.is_empty()).then_some(value.as_str()))
            }
            Self::File { instance_id } => session_state::update(instance_id, |state| {
                state.ordered_agents = panes.iter().map(|pane| (*pane).to_string()).collect();
            })
            .map(|_| ()),
        }
    }

    /// The view a window's sidebar shows, if a view rule applies to it.
    pub(super) fn window_view(&self, window_id: &str) -> Option<WindowView> {
        match self {
            Self::TmuxOptions(tmux) => super::tmux::window_view(tmux, window_id),
            Self::File { instance_id } => session_state::read(instance_id)
                .ok()?
                .window_views
                .remove(window_id),
        }
    }

    /// Publish every window's view. `windows` lists the live windows, so a
    /// window without a view clears the one it had.
    pub(super) fn set_window_views<'a>(
        &self,
        published: &mut PublishedViews,
        windows: impl Iterator<Item = &'a String>,
        views: &HashMap<String, WindowView>,
    ) {
        match self {
            Self::TmuxOptions(tmux) => {
                let mut live = HashSet::new();
                for window in windows {
                    live.insert(window.clone());
                    let view = views.get(window).cloned();
                    if published.windows.get(window) == Some(&view) {
                        continue;
                    }
                    match super::tmux::set_window_view(tmux, window, view.as_ref()) {
                        Ok(()) => {
                            published.windows.insert(window.clone(), view);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "failed to publish sidebar window view")
                        }
                    }
                }
                published.windows.retain(|window, _| live.contains(window));
            }
            Self::File { instance_id } => {
                if published.file.as_ref() == Some(views) {
                    return;
                }
                let next = views.clone();
                match session_state::update(instance_id, |state| {
                    state.window_views = next.into_iter().collect();
                }) {
                    Ok(_) => published.file = Some(views.clone()),
                    Err(error) => tracing::warn!(%error, "failed to publish sidebar window views"),
                }
            }
        }
    }

    /// Drop what the daemon published, when it exits.
    pub(super) fn clear_runtime(&self) {
        match self {
            Self::TmuxOptions(tmux) => {
                for option in [
                    DAEMON_PID_OPTION,
                    AGENTS_OPTION,
                    SLEEPING_OPTION,
                    super::tmux::SCOPE_OPTION,
                ] {
                    let _ = tmux.set_option(option, None);
                }
            }
            Self::File { instance_id } => {
                let _ = session_state::update(instance_id, |state| {
                    state.daemon_pid = None;
                    state.ordered_agents.clear();
                });
            }
        }
    }
}
