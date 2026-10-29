//! `node shell`: open or close an interactive shell session of a workspace.
//!
//! A shell is a session `<ex-id>-shell<n>` of the node tmux server, beside
//! the configured panes of the workspace's execution. It is not one of the
//! execution's panes: the agent never depends on it, and it ends with the
//! execution's sessions when the execution stops or is replaced. It runs where
//! the workspace's isolation puts its agent: on the host, or in a new
//! container of the workspace. A container workspace's shell runs on the host
//! only when the request asks for that.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::execution::{self, tmux, tmux_socket};
use super::{NodeRoot, refuse, secrets, validate_id};
use crate::config::RemoteIsolation;
use crate::multiplexer::{Multiplexer, tmux::TmuxBackend};
use crate::remote::protocol::code;
use crate::shell::shell_quote;

/// Slot prefix of shell sessions; pane sessions use a bare index.
const SHELL_SLOT: &str = "shell";

/// What a container shell runs: bash where the image has it.
const CONTAINER_SHELL: &str =
    "if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ShellRequest {
    /// Start a shell in the workspace's working directory.
    Open {
        ws_id: String,
        ex_id: String,
        /// Chosen before opening so the caller can close a lost response.
        slot: String,
        /// Run on the host even when the workspace's agent runs in a
        /// container.
        #[serde(default)]
        host: bool,
    },
    /// End a shell nobody views any more.
    Close {
        ws_id: String,
        ex_id: String,
        slot: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ShellResponse {
    Opened {
        /// Attach with `node attach <ex_id> <slot>`.
        ex_id: String,
        slot: String,
        /// Whether the shell runs in a container.
        container: bool,
        working_dir: String,
    },
    Closed {
        /// The session was still running.
        closed: bool,
    },
}

/// Whether `slot` names a session `node attach` may attach to: a pane
/// index, or a shell.
pub fn valid_slot(slot: &str) -> bool {
    let digits = slot.strip_prefix(SHELL_SLOT).unwrap_or(slot);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `name` is a shell session, and of which execution.
pub fn shell_session_execution(name: &str) -> Option<&str> {
    let (ex_id, slot) = name.rsplit_once('-')?;
    (slot.starts_with(SHELL_SLOT) && valid_slot(slot)).then_some(ex_id)
}

pub fn shell(root: &NodeRoot, request: ShellRequest) -> Result<ShellResponse> {
    match request {
        ShellRequest::Open {
            ws_id,
            ex_id,
            slot,
            host,
        } => open(root, &ws_id, &ex_id, &slot, host),
        ShellRequest::Close { ws_id, ex_id, slot } => close(root, &ws_id, &ex_id, &slot),
    }
}

fn open(
    root: &NodeRoot,
    ws_id: &str,
    ex_id: &str,
    slot: &str,
    host: bool,
) -> Result<ShellResponse> {
    validate_request(ws_id, ex_id, slot)?;
    let _lock = root.lock_workspace(ws_id)?;
    let workspace = root.load(ws_id)?.ok_or_else(|| {
        refuse(
            code::NOT_FOUND,
            format!("workspace {ws_id} does not exist on the node"),
        )
    })?;
    let handle = &workspace.identity.handle;
    if !workspace.progress.ready || !workspace.worktree_path.is_dir() {
        return Err(refuse(
            code::NO_WORKTREE,
            format!("'{handle}' has no worktree on the node yet"),
        ));
    }
    let Some(execution) = &workspace.execution else {
        return Err(refuse(
            code::NOT_FOUND,
            format!("'{handle}' has never started on the node"),
        ));
    };
    if execution.ex_id != ex_id {
        return Err(refuse(code::NOT_FOUND, "the workspace execution changed"));
    }
    if execution.closed_shells.contains(slot) {
        return Err(refuse(code::BAD_REQUEST, "this shell request was canceled"));
    }
    let socket = tmux_socket(root)?;
    let conf = execution::tmux_conf(root)?;
    let name = format!("{ex_id}-{slot}");
    let config_path = root.config_path(ws_id);
    let working_dir = workspace.working_dir().to_path_buf();
    let container = workspace.isolation == RemoteIsolation::Container && !host;
    let place = if container {
        "in a new container"
    } else {
        "on the host"
    };
    let banner = shell_quote(&format!(
        "workmux: shell for '{handle}' {place}; the agent keeps running when it exits"
    ));

    // Sessions get the environment configured panes get under the same
    // isolation: without a container, the node's workmux, the workspace's
    // secrets and its spawn target.
    let host_env = workspace.isolation == RemoteIsolation::None;
    let spawn_env = host_env.then_some((root.path.as_path(), ws_id));
    let command = if container {
        let pin = std::env::current_exe()?;
        let supervisor = format!(
            "{} sandbox run --node-root {} --node-workspace {ws_id}",
            shell_quote(&pin.to_string_lossy()),
            shell_quote(&root.path.to_string_lossy()),
        );
        Some(crate::sandbox::wrap_for_container(
            &format!("printf '%s\\n' {banner}; {CONTAINER_SHELL}"),
            Some(&supervisor),
            &workspace.worktree_path,
            &working_dir,
        )?)
    } else {
        None
    };
    let pane_id = execution::new_session(
        &socket,
        &conf,
        &name,
        &working_dir,
        &config_path,
        spawn_env,
        command.as_deref(),
    )?;
    let started = (|| -> Result<()> {
        if host_env {
            let config = crate::frozen_config::load(&config_path)?.0;
            let agent_kind = workspace.agent_kind(&config);
            let session_secrets = secrets::for_execution(root, ws_id, agent_kind)?;
            if !session_secrets.0.is_empty() {
                execution::set_session_env(root, &socket, &name, &session_secrets)?;
                // tmux environment updates apply only to newly spawned processes.
                let output = tmux(&socket)
                    .args(["respawn-pane", "-k", "-t", &pane_id])
                    .output()
                    .context("Failed to restart the shell with its environment")?;
                anyhow::ensure!(
                    output.status.success(),
                    "Failed to restart the shell: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        if !container {
            let path = if host_env {
                let pin = std::env::current_exe()?;
                let dir = execution::write_status_wrapper(root, &pin)?;
                format!(
                    "export PATH={}:\"$PATH\"; ",
                    shell_quote(&dir.to_string_lossy())
                )
            } else {
                String::new()
            };
            // The keys reach the shell once it starts reading input.
            TmuxBackend::for_socket(&socket.to_string_lossy()).send_keys(
                &pane_id,
                &format!(" {path}printf '\\033[H\\033[2J\\033[3J'; printf '%s\\n' {banner}"),
            )?;
        }
        Ok(())
    })();
    if let Err(err) = started {
        let _ = tmux(&socket)
            .args(["kill-session", "-t", &format!("={name}")])
            .output();
        return Err(err);
    }
    Ok(ShellResponse::Opened {
        ex_id: ex_id.to_string(),
        slot: slot.to_string(),
        container,
        working_dir: working_dir.to_string_lossy().into_owned(),
    })
}

fn validate_request(ws_id: &str, ex_id: &str, slot: &str) -> Result<()> {
    validate_id(ws_id, 'w', "workspace id")?;
    validate_id(ex_id, 'x', "execution id")?;
    if !slot.starts_with(SHELL_SLOT) || !valid_slot(slot) || slot.len() > SHELL_SLOT.len() + 39 {
        return Err(refuse(
            code::BAD_REQUEST,
            format!("'{slot}' is not a shell session"),
        ));
    }
    Ok(())
}

fn close(root: &NodeRoot, ws_id: &str, ex_id: &str, slot: &str) -> Result<ShellResponse> {
    validate_request(ws_id, ex_id, slot)?;
    let _lock = root.lock_workspace(ws_id)?;
    let Some(mut workspace) = root.load(ws_id)? else {
        return Ok(ShellResponse::Closed { closed: false });
    };
    let Some(execution) = workspace.execution.as_mut().filter(|ex| ex.ex_id == ex_id) else {
        return Ok(ShellResponse::Closed { closed: false });
    };
    // Closing can reach the node before the corresponding open request.
    // Persist cancellation under the same lock before removing any session.
    execution.closed_shells.insert(slot.to_string());
    root.save(&workspace)?;
    let socket = tmux_socket(root)?;
    let closed = tmux(&socket)
        .args(["kill-session", "-t", &format!("={ex_id}-{slot}")])
        .output()
        .is_ok_and(|output| output.status.success());
    Ok(ShellResponse::Closed { closed })
}

/// Random identity chosen before the open RPC, never reused for a later shell.
pub fn new_slot() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("Failed to generate a shell identity")?;
    Ok(format!("{SHELL_SLOT}{}", u128::from_le_bytes(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_pane_indices_or_shells() {
        for slot in ["0", "12", "shell1", "shell20"] {
            assert!(valid_slot(slot), "{slot}");
        }
        for slot in ["", "shell", "-1", "1a", "shellx", "../0", "shell1 "] {
            assert!(!valid_slot(slot), "{slot}");
        }
    }

    #[test]
    fn shell_sessions_name_their_execution() {
        assert_eq!(
            shell_session_execution("x0123456789ab-shell3"),
            Some("x0123456789ab")
        );
        assert_eq!(shell_session_execution("x0123456789ab-0"), None);
        assert_eq!(shell_session_execution("x0123456789ab-shell"), None);
        assert_eq!(shell_session_execution("other"), None);
    }

    #[test]
    fn new_slots_are_valid_shell_requests() {
        let slot = new_slot().unwrap();
        validate_request("w0123456789ab", "x0123456789ab", &slot).unwrap();
        assert!(valid_slot(&slot) && shell_session_execution(&format!("x1-{slot}")) == Some("x1"));
    }

    #[test]
    fn requests_are_tagged_by_action() {
        let request: ShellRequest = serde_json::from_str(
            r#"{"action":"open","ws_id":"w0123456789ab","ex_id":"x0123456789ab","slot":"shell1"}"#,
        )
        .unwrap();
        assert!(matches!(request, ShellRequest::Open { host: false, .. }));
        let request: ShellRequest = serde_json::from_str(
            r#"{"action":"close","ws_id":"w0123456789ab","ex_id":"x0123456789ab","slot":"shell2"}"#,
        )
        .unwrap();
        assert!(matches!(request, ShellRequest::Close { .. }));
    }
}
