//! Where the sidebar keeps its shared runtime preferences and what the
//! daemon publishes for commands and clients.
//!
//! tmux keeps them in global `@workmux_*` options. Backends without
//! user options (Zellij, Rex) keep them in the per-instance state file.

use anyhow::Result;
use std::collections::{HashMap, HashSet};

use crate::config::SidebarPosition;
use crate::multiplexer::tmux::TmuxSidebarSnapshot;

use super::app::{SidebarFilterMode, SidebarLayoutMode};
use super::host::TmuxHost;
use super::preferences::{GroupingOverride, Preferences, parse_layout, parse_position};
use super::session_state;
use super::view::WindowView;

const AGENTS_OPTION: &str = "@workmux_sidebar_agents";
const DAEMON_PID_OPTION: &str = "@workmux_sidebar_daemon_pid";
const POSITION_OPTION: &str = "@workmux_sidebar_position";
const LAYOUT_OPTION: &str = "@workmux_sidebar_layout";
const FILTER_OPTION: &str = "@workmux_sidebar_filter";
const GROUP_BY_OPTION: &str = "@workmux_sidebar_group_by";
const EXPANDED_OPTION: &str = "@workmux_sidebar_expanded";
const SLEEPING_OPTION: &str = "@workmux_sleeping_panes";

/// Decode the tmux preference options, as one `sidebar_snapshot` read them.
pub(super) fn tmux_preferences(snapshot: &TmuxSidebarSnapshot) -> Preferences {
    Preferences {
        position: snapshot.position.as_deref().and_then(parse_position),
        layout: snapshot.layout.as_deref().and_then(parse_layout),
        filter: snapshot.filter.as_deref().map(SidebarFilterMode::from_str),
        views_enabled: snapshot
            .views_enabled
            .as_deref()
            .map(|value| super::view::is_enabled(Some(value))),
        sleeping_panes: parse_sleeping_panes(snapshot.sleeping_panes.as_deref()),
        grouping: snapshot
            .group_by
            .as_deref()
            .map_or(GroupingOverride::Unset, GroupingOverride::parse),
        expanded_groups: parse_expanded_groups(snapshot.expanded_groups.as_deref()),
    }
}

fn parse_sleeping_panes(value: Option<&str>) -> HashSet<String> {
    value
        .map(|value| value.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

/// Labels are tab separated because a project or session name can contain
/// spaces.
fn parse_expanded_groups(value: Option<&str>) -> Vec<String> {
    value
        .map(|value| {
            value
                .split('\t')
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn update_prefs(instance_id: &str, update: impl FnOnce(&mut Preferences)) -> Result<()> {
    session_state::update(instance_id, |state| update(&mut state.prefs)).map(|_| ())
}

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
            Self::TmuxOptions(tmux) => parse_position(&tmux.option(POSITION_OPTION)?),
            Self::File { instance_id } => session_state::read(instance_id).ok()?.prefs.position,
        }
    }

    pub(super) fn set_position(&self, position: SidebarPosition) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => tmux.set_option(
                POSITION_OPTION,
                Some(match position {
                    SidebarPosition::Left => "left",
                    SidebarPosition::Top => "top",
                }),
            ),
            Self::File { instance_id } => {
                update_prefs(instance_id, |prefs| prefs.position = Some(position))
            }
        }
    }

    pub(super) fn filter(&self) -> Option<SidebarFilterMode> {
        match self {
            Self::TmuxOptions(tmux) => tmux
                .option(FILTER_OPTION)
                .map(|mode| SidebarFilterMode::from_str(&mode)),
            Self::File { instance_id } => session_state::read(instance_id).ok()?.prefs.filter,
        }
    }

    pub(super) fn set_filter(&self, filter: SidebarFilterMode) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => tmux.set_option(FILTER_OPTION, Some(filter.as_str())),
            Self::File { instance_id } => {
                update_prefs(instance_id, |prefs| prefs.filter = Some(filter))
            }
        }
    }

    pub(super) fn views_enabled(&self) -> bool {
        match self {
            Self::TmuxOptions(tmux) => {
                super::view::is_enabled(tmux.option(super::view::OPTION).as_deref())
            }
            Self::File { instance_id } => session_state::read(instance_id)
                .unwrap_or_default()
                .prefs
                .views_enabled(),
        }
    }

    pub(super) fn set_views_enabled(&self, enabled: bool) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => tmux.set_option(
                super::view::OPTION,
                Some(if enabled { "on" } else { "off" }),
            ),
            Self::File { instance_id } => {
                update_prefs(instance_id, |prefs| prefs.views_enabled = Some(enabled))
            }
        }
    }

    pub(super) fn grouping(&self) -> GroupingOverride {
        match self {
            Self::TmuxOptions(tmux) => tmux
                .option(GROUP_BY_OPTION)
                .map_or(GroupingOverride::Unset, |value| {
                    GroupingOverride::parse(&value)
                }),
            Self::File { instance_id } => {
                session_state::read(instance_id)
                    .unwrap_or_default()
                    .prefs
                    .grouping
            }
        }
    }

    /// `Unset` drops the runtime choice so the config decides again.
    pub(super) fn set_grouping(&self, grouping: GroupingOverride) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => tmux.set_option(GROUP_BY_OPTION, grouping.value()),
            Self::File { instance_id } => {
                update_prefs(instance_id, |prefs| prefs.grouping = grouping)
            }
        }
    }

    pub(super) fn set_layout(&self, layout: SidebarLayoutMode) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => tmux.set_option(LAYOUT_OPTION, Some(layout.as_str())),
            Self::File { instance_id } => {
                update_prefs(instance_id, |prefs| prefs.layout = Some(layout))
            }
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
                let mut panes = parse_sleeping_panes(tmux.option(SLEEPING_OPTION).as_deref());
                flip(&mut panes);
                let value = panes.iter().cloned().collect::<Vec<_>>().join(" ");
                tmux.set_option(
                    SLEEPING_OPTION,
                    (!value.is_empty()).then_some(value.as_str()),
                )?;
                Ok(panes)
            }
            Self::File { instance_id } => {
                session_state::update(instance_id, |state| flip(&mut state.prefs.sleeping_panes))
                    .map(|state| state.prefs.sleeping_panes)
            }
        }
    }

    pub(super) fn set_expanded_groups(&self, labels: &[&str]) -> Result<()> {
        match self {
            Self::TmuxOptions(tmux) => {
                let value = labels.join("\t");
                tmux.set_option(
                    EXPANDED_OPTION,
                    (!value.is_empty()).then_some(value.as_str()),
                )
            }
            Self::File { instance_id } => update_prefs(instance_id, |prefs| {
                prefs.expanded_groups = labels.iter().map(|label| (*label).to_string()).collect();
            }),
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
