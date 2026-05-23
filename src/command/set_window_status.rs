use anyhow::Result;
use clap::ValueEnum;
use tracing::warn;

use crate::config::Config;
use crate::multiplexer::{AgentStatus, create_backend, detect_backend};

#[derive(ValueEnum, Debug, Clone)]
pub enum SetWindowStatusCommand {
    /// Set status to "working" (agent is processing)
    Working,
    /// Set status to "waiting" (agent needs user input) - auto-clears on window focus
    Waiting,
    /// Set status to "done" (agent finished) - auto-clears on window focus
    Done,
    /// Clear the status
    Clear,
}

pub fn run(cmd: SetWindowStatusCommand) -> Result<()> {
    if std::env::var_os("WORKMUX_DISABLE_SET_WINDOW_STATUS").is_some() {
        return Ok(());
    }

    // Codex compatibility: hook stdin may identify a specific parent/subagent
    // turn. Use it to avoid child cleanup marking the pane done early.
    let codex_run_id = crate::state::codex_status::detect_run_id_from_stdin();

    // Inside a sandbox guest, route through RPC to the host supervisor
    if crate::sandbox::guest::is_sandbox_guest() {
        // Codex nested hook workaround is host-path only in v1.
        return run_via_rpc(cmd);
    }

    let config = Config::load(None)?;
    let mux = create_backend(detect_backend());

    // Fail silently if not in a multiplexer session
    let Some(pane_id) = mux.current_pane_id() else {
        return Ok(());
    };

    let pane_key = crate::state::PaneKey {
        backend: mux.name().to_string(),
        instance: mux.instance_id(),
        pane_id: pane_id.to_string(),
    };

    match cmd {
        SetWindowStatusCommand::Clear => {
            if let Err(error) = crate::state::codex_status::clear_pane(&pane_key) {
                warn!(%pane_id, error = %error, "failed to clear Codex status workaround state");
            }
            mux.clear_status(&pane_id)?;
        }
        SetWindowStatusCommand::Working
        | SetWindowStatusCommand::Waiting
        | SetWindowStatusCommand::Done => {
            let requested_status = match cmd {
                SetWindowStatusCommand::Working => AgentStatus::Working,
                SetWindowStatusCommand::Waiting => AgentStatus::Waiting,
                SetWindowStatusCommand::Done => AgentStatus::Done,
                SetWindowStatusCommand::Clear => unreachable!(),
            };

            let status = if let Some(run_id) = codex_run_id {
                match crate::state::codex_status::apply_status(&pane_key, &run_id, requested_status)
                {
                    Ok(status) => status,
                    Err(error) => {
                        warn!(%pane_id, ?requested_status, error = %error, "failed to update Codex status workaround state");
                        requested_status
                    }
                }
            } else {
                requested_status
            };

            let (icon, auto_clear) = match status {
                AgentStatus::Working => (config.status_icons.working(), false),
                AgentStatus::Waiting => (config.status_icons.waiting(), true),
                AgentStatus::Done => (config.status_icons.done(), true),
            };

            // Ensure the status format is applied so the icon actually shows up
            if config.status_format.unwrap_or(true) {
                let _ = mux.ensure_status_format(&pane_id);
            }

            // Update backend UI (status bar icon)
            mux.set_status(&pane_id, icon, auto_clear)?;

            // Persist to state store so the dashboard sees this agent
            crate::state::persist_agent_update(&*mux, &pane_id, Some(status), None);
        }
    }

    Ok(())
}

/// Send a status update via RPC when running inside a sandbox guest.
fn run_via_rpc(cmd: SetWindowStatusCommand) -> Result<()> {
    use crate::sandbox::rpc::{RpcClient, RpcRequest, RpcResponse};

    let status = match cmd {
        SetWindowStatusCommand::Working => "working",
        SetWindowStatusCommand::Waiting => "waiting",
        SetWindowStatusCommand::Done => "done",
        SetWindowStatusCommand::Clear => "clear",
    };

    let mut client = RpcClient::from_env()?;
    let response = client.call(&RpcRequest::SetStatus {
        status: status.to_string(),
    })?;

    match response {
        RpcResponse::Ok => Ok(()),
        RpcResponse::Error { message } => {
            warn!(error = %message, "RPC SetStatus failed");
            Ok(()) // Fail silently like the host path does
        }
        _ => Ok(()),
    }
}
