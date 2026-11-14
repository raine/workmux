//! Where the sidebar keeps its shared runtime preferences.
//!
//! tmux keeps them in global `@workmux_*` options. Backends without
//! user options (Zellij, Rex) keep them in the per-instance state file.

use anyhow::Result;
use std::collections::HashSet;

use crate::config::SidebarPosition;

use super::host::TmuxHost;
use super::session_state;

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
                    .option("@workmux_sleeping_panes")
                    .map(|value| value.split_whitespace().map(String::from).collect())
                    .unwrap_or_default();
                flip(&mut panes);
                let value = panes.iter().cloned().collect::<Vec<_>>().join(" ");
                tmux.set_option(
                    "@workmux_sleeping_panes",
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
            Self::TmuxOptions(tmux) => tmux.option("@workmux_sidebar_daemon_pid"),
            Self::File { instance_id } => session_state::read(instance_id)
                .ok()?
                .daemon_pid
                .map(|pid| pid.to_string()),
        }
    }

    pub(super) fn clear_daemon_pid(&self) {
        match self {
            Self::TmuxOptions(tmux) => {
                let _ = tmux.set_option("@workmux_sidebar_daemon_pid", None);
            }
            Self::File { instance_id } => {
                let _ = session_state::update(instance_id, |state| state.daemon_pid = None);
            }
        }
    }
}
