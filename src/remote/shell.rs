//! `workmux shell`: an interactive shell in a remote workspace, relayed into
//! the calling terminal.
//!
//! The node starts the shell as a session of its own (see
//! `remote::node::shell`) and the relay that carries agent panes carries it,
//! reconnecting the same way. Exiting the shell ends the session. Leaving
//! the relay any other way (its close key, or the terminal going away) ends
//! the session too, since nothing else can return to it.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, anyhow, bail};

use super::RemoteSpec;
use super::attach::{Left, relay_session};
use super::cache::WorkspaceRecord;
use super::client::{Node, error_code};
use super::install::Pin;
use super::node::shell::{ShellRequest, ShellResponse};
use super::protocol::{self, code};
use super::transport::Transport;
use crate::config::Config;

pub fn run(config: &Config, record: &WorkspaceRecord, host: bool) -> Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("workmux shell needs a terminal");
    }
    let spec = RemoteSpec::resolve(config, &record.remote)?;
    let node = Node::connect(&spec).with_context(|| {
        format!(
            "Cannot reach '{}' to open a shell in '{}'",
            spec.name, record.handle
        )
    })?;
    let ex_id = record
        .execution
        .as_ref()
        .ok_or_else(|| anyhow!("run `workmux open {}` first", record.handle))?
        .ex_id
        .clone();
    let slot = super::node::shell::new_slot()?;
    let signals = ShellSignals::new()?;
    let mut cleanup = ShellCleanup {
        spec: spec.clone(),
        pin: node.pin.name.clone(),
        ws_id: record.ws_id.clone(),
        ex_id: ex_id.clone(),
        slot: slot.clone(),
        armed: true,
    };
    let request = ShellRequest::Open {
        ws_id: record.ws_id.clone(),
        ex_id,
        slot,
        host,
    };
    let opened: Result<ShellResponse> = node
        .call(protocol::SHELL, &request, false)
        .map_err(|err| refusal(err, record));
    if signals.canceled.load(Ordering::SeqCst) {
        return Ok(());
    }
    let opened = opened?;
    let ShellResponse::Opened {
        ex_id,
        slot,
        container,
        working_dir,
    } = opened
    else {
        bail!("'{}' answered an open request with {opened:?}", spec.name);
    };
    tracing::info!(remote = %spec.name, ws_id = %record.ws_id, %ex_id, %slot, container, %working_dir, "opened remote shell");
    anyhow::ensure!(
        ex_id == cleanup.ex_id && slot == cleanup.slot,
        "the node returned a different shell identity"
    );
    let left = relay_session(node, &ex_id, &slot, signals.canceled.clone());
    if matches!(left, Ok(Left::SessionEnded)) {
        cleanup.armed = false;
    }
    left.map(|_| ())
}

/// Capture termination before the open RPC can create a session. The relay
/// installs its own handlers before checking this flag, so the handoff has no gap.
struct ShellSignals {
    canceled: Arc<AtomicBool>,
    handlers: Vec<signal_hook::SigId>,
}

impl ShellSignals {
    fn new() -> Result<Self> {
        let mut guard = Self {
            canceled: Arc::new(AtomicBool::new(false)),
            handlers: Vec::new(),
        };
        for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
            guard
                .handlers
                .push(signal_hook::flag::register(signal, guard.canceled.clone())?);
        }
        Ok(guard)
    }
}

impl Drop for ShellSignals {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            signal_hook::low_level::unregister(handler);
        }
    }
}

/// Own the shell before its open RPC, including an error or a lost response.
struct ShellCleanup {
    spec: RemoteSpec,
    pin: String,
    ws_id: String,
    ex_id: String,
    slot: String,
    armed: bool,
}

impl Drop for ShellCleanup {
    fn drop(&mut self) {
        if self.armed {
            close(
                &self.spec,
                self.pin.clone(),
                self.ws_id.clone(),
                self.ex_id.clone(),
                self.slot.clone(),
            );
        }
    }
}

/// End a shell its relay left, best effort: a server that cannot be reached
/// ends it with the execution.
fn close(spec: &RemoteSpec, pin: String, ws_id: String, ex_id: String, slot: String) {
    // A terminal that went away hangs up its foreground process group, ssh
    // included; ssh keeps an inherited ignored SIGHUP.
    // SAFETY: plain signal(2); the relay no longer watches SIGHUP.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let result = Transport::open(spec).and_then(|transport| {
        let node = Node {
            spec: spec.clone(),
            transport,
            pin: Pin::installed(pin),
        };
        node.call::<_, ShellResponse>(
            protocol::SHELL,
            &ShellRequest::Close { ws_id, ex_id, slot },
            false,
        )
    });
    if let Err(err) = result {
        tracing::warn!(remote = %spec.name, "failed to close the remote shell: {err:#}");
    }
}

/// An open request's error, with the command that fixes a refusal.
fn refusal(err: anyhow::Error, record: &WorkspaceRecord) -> anyhow::Error {
    match error_code(&err) {
        Some(code::NOT_FOUND) | Some(code::NO_WORKTREE) => {
            anyhow!("{err}; run `workmux open {}` first", record.handle)
        }
        _ => err.context(format!(
            "Failed to open a shell in '{}' on '{}'",
            record.handle, record.remote
        )),
    }
}
