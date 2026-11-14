//! The multiplexer hosting a process's sidebars.
//!
//! Backend-specific sidebar behaviour goes through `SidebarHost`. Code that
//! only works on tmux takes a `TmuxHost`, and code that only works on Rex a
//! `RexHost`. Only `SidebarHost::resolve` creates them, so shared code paths
//! cannot call either under another backend.

use anyhow::{Result, anyhow};

use crate::cmd::Cmd;
use crate::multiplexer::rex::RexBackend;
use crate::multiplexer::{LivePaneInfo, Multiplexer, TmuxBackend, create_backend, detect_backend};

use super::app::HostIdentity;
use super::store::SidebarStore;
use super::tmux::panes;
use super::{placed, rex_panes, zellij_panes};

/// A tmux server hosting sidebars.
#[derive(Clone)]
pub(crate) struct TmuxHost(TmuxBackend);

impl TmuxHost {
    pub(super) fn backend(&self) -> &TmuxBackend {
        &self.0
    }

    /// A tmux command addressed to this server.
    pub(super) fn cmd(&self) -> Cmd<'_> {
        self.0.tmux_command()
    }

    /// A global option's value, or `None` when it is unset or blank.
    pub(super) fn option(&self, name: &str) -> Option<String> {
        self.0.global_option(name).ok().flatten()
    }

    /// Set a global option, or unset it when `value` is `None`.
    pub(super) fn set_option(&self, name: &str, value: Option<&str>) -> Result<()> {
        match value {
            Some(value) => self.0.set_global_option(name, value),
            None => self.0.unset_global_option(name),
        }
    }

    pub(super) fn host(&self) -> SidebarHost {
        SidebarHost::Tmux(self.clone())
    }
}

/// A Rex server hosting sidebars.
#[derive(Clone)]
pub(crate) struct RexHost {
    instance_id: String,
    backend: RexBackend,
}

impl RexHost {
    /// The backend for this server. Rex instances are named by
    /// `$REX_SERVER`, which the backend addresses.
    pub(super) fn backend(&self) -> &RexBackend {
        &self.backend
    }

    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) fn host(&self) -> SidebarHost {
        SidebarHost::Rex(self.clone())
    }
}

/// The multiplexer hosting this process's sidebars.
#[derive(Clone)]
pub(crate) enum SidebarHost {
    Tmux(TmuxHost),
    Zellij { instance_id: String },
    Rex(RexHost),
}

impl SidebarHost {
    pub(super) fn resolve(mux: &dyn Multiplexer) -> Result<Self> {
        match mux.name() {
            "tmux" => Ok(Self::Tmux(TmuxHost(TmuxBackend::for_socket(
                &mux.instance_id(),
            )))),
            "zellij" => Ok(Self::Zellij {
                instance_id: mux.instance_id(),
            }),
            "rex" => Ok(Self::Rex(RexHost {
                instance_id: mux.instance_id(),
                backend: RexBackend::new(),
            })),
            name => Err(anyhow!(
                "Sidebar is supported only by tmux, Zellij and Rex, not {name}"
            )),
        }
    }

    /// The host for a hook or helper process. Never fails: a backend
    /// without sidebars has no host.
    pub(super) fn detect() -> Option<Self> {
        Self::resolve(create_backend(detect_backend()).as_ref()).ok()
    }

    pub(super) fn instance_id(&self) -> String {
        match self {
            Self::Tmux(tmux) => tmux.backend().instance_id(),
            Self::Zellij { instance_id } => instance_id.clone(),
            Self::Rex(rex) => rex.instance_id.clone(),
        }
    }

    /// A tmux host on the default server, for tests that need a host but run
    /// no commands.
    #[cfg(test)]
    pub(super) fn test_tmux() -> Self {
        Self::Tmux(TmuxHost(TmuxBackend::new()))
    }

    pub(super) fn tmux(&self) -> Option<&TmuxHost> {
        match self {
            Self::Tmux(tmux) => Some(tmux),
            Self::Zellij { .. } | Self::Rex(_) => None,
        }
    }

    pub(super) fn store(&self) -> SidebarStore {
        match self {
            Self::Tmux(tmux) => SidebarStore::TmuxOptions(tmux.clone()),
            Self::Zellij { .. } | Self::Rex(_) => SidebarStore::File {
                instance_id: self.instance_id(),
            },
        }
    }
}

/// What focusing an agent pane did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FocusOutcome {
    Focused,
    /// The host brought the pane forward but could not switch the app's
    /// window to it (Rex with Remote Control off).
    WindowNotSwitched,
}

impl SidebarHost {
    /// The sidebar pane this process runs in.
    pub(super) fn identity(&self, mux: &dyn Multiplexer) -> Option<HostIdentity> {
        match self {
            Self::Tmux(tmux) => panes::host_identity(tmux),
            Self::Zellij { .. } | Self::Rex(_) => {
                let pane_id = mux.current_pane_id()?;
                let info = mux.get_live_pane_info(&pane_id).ok()??;
                let session_name = info.session.unwrap_or_else(|| self.instance_id());
                Some(HostIdentity {
                    session_id: info.session_id.unwrap_or_else(|| session_name.clone()),
                    session_name,
                    window_id: info.window_id?,
                    pane_id,
                })
            }
        }
    }

    /// Bring an agent pane forward.
    pub(super) fn focus(&self, mux: &dyn Multiplexer, pane_id: &str) -> Result<FocusOutcome> {
        match self {
            Self::Rex(rex) => {
                if rex.backend().show_block(pane_id)? {
                    Ok(FocusOutcome::Focused)
                } else {
                    Ok(FocusOutcome::WindowNotSwitched)
                }
            }
            Self::Tmux(_) | Self::Zellij { .. } => {
                mux.switch_to_pane(pane_id, None)?;
                Ok(FocusOutcome::Focused)
            }
        }
    }

    /// Show an error to the user outside the sidebar, where the host can.
    pub(super) fn notify_error(&self, message: &str) {
        match self {
            Self::Tmux(tmux) => {
                let _ = tmux.cmd().args(&["display-message", message]).run();
            }
            Self::Zellij { .. } | Self::Rex(_) => {}
        }
    }

    /// Hand a selected agent to the sidebar in the agent's window, then
    /// switch to that sidebar. Returns whether it switched.
    pub(super) fn hand_off_selection(
        &self,
        mux: &dyn Multiplexer,
        window_id: &str,
        agent_pane_id: &str,
    ) -> bool {
        let Self::Tmux(tmux) = self else {
            return false;
        };
        if window_id.is_empty() {
            return false;
        }
        let Some(sidebar_pane_id) = panes::find_sidebar_pane_in_window(tmux, window_id)
            .ok()
            .flatten()
        else {
            return false;
        };
        if panes::set_pending_selection(tmux, &sidebar_pane_id, agent_pane_id).is_err() {
            return false;
        }
        if mux.switch_to_pane(&sidebar_pane_id, None).is_ok() {
            return true;
        }
        panes::clear_pending_selection(tmux, &sidebar_pane_id);
        false
    }

    /// The agent pane another sidebar handed to this one, if any.
    pub(super) fn take_handed_off_selection(&self, identity: &HostIdentity) -> Option<String> {
        match self {
            Self::Tmux(tmux) => panes::take_pending_selection(tmux, &identity.pane_id),
            Self::Zellij { .. } | Self::Rex(_) => None,
        }
    }

    /// Clean up after a sidebar client exits. A silent exit closes only this
    /// sidebar's pane; otherwise the user quit, which stops every sidebar.
    pub(super) fn finish_sidebar_exit(&self, identity: &HostIdentity, silent: bool) {
        match (self, silent) {
            (Self::Tmux(tmux), true) => panes::close_pane_after_exit(tmux, &identity.pane_id),
            // Zellij and Rex close the pane when its process exits.
            (Self::Zellij { .. } | Self::Rex(_), true) => {}
            (Self::Tmux(tmux), false) => panes::shutdown_all_sidebars(tmux, identity),
            (Self::Zellij { .. } | Self::Rex(_), false) => placed::stop_after_quit(self, identity),
        }
    }

    /// Whether the sidebar pane is the only pane left in its window.
    pub(super) fn is_last_pane(&self, window_id: &str, pane_id: &str) -> bool {
        match self {
            Self::Tmux(tmux) => panes::sidebar_is_only_pane(tmux, window_id, pane_id),
            Self::Zellij { instance_id } => {
                zellij_panes::sidebar_is_only_pane(instance_id, window_id, pane_id)
            }
            Self::Rex(rex) => rex_panes::sidebar_is_only_pane(rex, window_id, pane_id),
        }
    }

    /// Prepare the command that starts this host's sidebar daemon.
    pub(super) fn daemon_command(&self, command: &mut std::process::Command) {
        match self {
            Self::Rex(_) => {
                use std::os::unix::process::CommandExt;
                // The daemon serves the whole server, not the block that
                // started it: it must outlive that block's terminal and act
                // on no session of its own.
                command
                    .env("WORKMUX_BACKEND", "rex")
                    .env_remove("REX_SESSION")
                    .env_remove("REX_BLOCK");
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            Self::Tmux(_) | Self::Zellij { .. } => {}
        }
    }
}

impl SidebarHost {
    /// Take what a daemon observation pass must hold so it never races a
    /// command placing sidebars. `None` means such a command is running and
    /// the pass should wait for the next one.
    ///
    /// Rex windows are observed and reconciled under one hold of the layout
    /// lock. A Zellij sidebar holds the lock while it starts, so the daemon
    /// only waits for it to be free. tmux places sidebars through hooks and
    /// needs no lock.
    pub(super) fn observation_lock(&self) -> Result<Option<Option<std::fs::File>>> {
        match self {
            Self::Tmux(_) => Ok(Some(None)),
            Self::Zellij { instance_id } => {
                Ok(super::session_state::try_lock_layout(instance_id)?.map(|_| None))
            }
            Self::Rex(rex) => {
                Ok(super::session_state::try_lock_layout(rex.instance_id())?.map(Some))
            }
        }
    }
}

impl SidebarHost {
    /// The session a pane belongs to, in the form the session filter
    /// compares. Rex session labels need not be unique, so Rex compares IDs.
    pub(super) fn session_key<'a>(&self, info: &'a LivePaneInfo) -> Option<&'a str> {
        match self {
            Self::Rex(_) => info.session_id.as_deref(),
            Self::Tmux(_) | Self::Zellij { .. } => info.session.as_deref(),
        }
    }

    /// The caller's session, in `session_key` form. A Zellij server is one
    /// session.
    pub(super) fn current_session(&self) -> Option<String> {
        match self {
            Self::Tmux(tmux) => {
                let target = super::tmux::current_window(tmux).ok()?;
                let session = tmux
                    .cmd()
                    .args(&["display-message", "-p", "-t", &target, "#{session_name}"])
                    .run_and_capture_stdout()
                    .ok()?;
                Some(session.trim().to_string())
            }
            Self::Zellij { instance_id } => Some(instance_id.clone()),
            Self::Rex(_) => std::env::var("REX_SESSION").ok(),
        }
    }
}
