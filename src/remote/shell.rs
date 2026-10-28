//! `workmux shell`: an interactive shell in a remote workspace, relayed into
//! the calling terminal.
//!
//! The node starts the shell as a session of its own (see
//! `remote::node::shell`) and the relay that carries agent panes carries it,
//! reconnecting the same way. Exiting the shell ends the session. Leaving
//! the relay any other way (its close key, or the terminal going away) ends
//! the session too, since nothing else can return to it.

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
    let request = ShellRequest::Open {
        ws_id: record.ws_id.clone(),
        host,
    };
    let opened = node
        .call(protocol::SHELL, &request, false)
        .map_err(|err| refusal(err, record))?;
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
    let pin = node.pin.name.clone();
    let left = relay_session(node, &ex_id, &slot);
    if !matches!(left, Ok(Left::SessionEnded)) {
        close(&spec, pin, ex_id, slot);
    }
    left.map(|_| ())
}

/// End a shell its relay left, best effort: a server that cannot be reached
/// ends it with the execution.
fn close(spec: &RemoteSpec, pin: String, ex_id: String, slot: String) {
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
        node.call::<_, ShellResponse>(protocol::SHELL, &ShellRequest::Close { ex_id, slot }, false)
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
