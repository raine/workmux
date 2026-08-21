use anyhow::Result;
use clap::ValueEnum;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::warn;

use crate::config::Config;
use crate::multiplexer::{
    AgentStatus, BackendType, LivePaneInfo, Multiplexer, STATUS_TARGET_BACKEND_ENV,
    STATUS_TARGET_INSTANCE_ENV, STATUS_TARGET_PANE_ENV, create_backend,
    create_backend_for_instance, detect_backend,
};
use crate::state::StateStore;

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

#[derive(Debug, PartialEq, Eq)]
struct StatusTarget {
    backend: BackendType,
    instance: String,
    pane_id: String,
}

impl StatusTarget {
    fn from_env() -> Result<Option<Self>> {
        Self::from_values(
            std::env::var(STATUS_TARGET_BACKEND_ENV).ok(),
            std::env::var(STATUS_TARGET_INSTANCE_ENV).ok(),
            std::env::var(STATUS_TARGET_PANE_ENV).ok(),
        )
    }

    fn from_values(
        backend: Option<String>,
        instance: Option<String>,
        pane_id: Option<String>,
    ) -> Result<Option<Self>> {
        if backend.is_none() && instance.is_none() && pane_id.is_none() {
            return Ok(None);
        }

        let backend = backend
            .ok_or_else(|| anyhow::anyhow!("{} is missing", STATUS_TARGET_BACKEND_ENV))?
            .parse::<BackendType>()
            .map_err(anyhow::Error::msg)?;
        if backend != BackendType::Zellij {
            return Err(anyhow::anyhow!(
                "status targets do not support the {} backend",
                backend
            ));
        }
        let instance = instance
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("{} is missing", STATUS_TARGET_INSTANCE_ENV))?;
        let pane_id = pane_id
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("{} is missing", STATUS_TARGET_PANE_ENV))?;

        Ok(Some(Self {
            backend,
            instance,
            pane_id,
        }))
    }
}

pub fn run(cmd: SetWindowStatusCommand) -> Result<()> {
    if std::env::var_os("WORKMUX_DISABLE_SET_WINDOW_STATUS").is_some() {
        return Ok(());
    }

    // Inside a sandbox guest, route through RPC to the host supervisor
    if crate::sandbox::guest::is_sandbox_guest() {
        return run_via_rpc(cmd);
    }

    let config = Config::load(None)?;

    match StatusTarget::from_env() {
        Ok(Some(target)) => {
            let mux = create_backend_for_instance(target.backend, &target.instance);
            match mux.get_live_pane_info(&target.pane_id) {
                Ok(Some(_)) => {
                    return apply_status_update(&cmd, &config, &*mux, &target.pane_id);
                }
                Ok(None) => {
                    warn!(
                        backend = %target.backend,
                        instance = %target.instance,
                        pane_id = %target.pane_id,
                        "status target pane is unavailable"
                    );
                }
                Err(error) => {
                    warn!(
                        backend = %target.backend,
                        instance = %target.instance,
                        pane_id = %target.pane_id,
                        error = %error,
                        "failed to validate status target pane"
                    );
                }
            }
            return Ok(());
        }
        Ok(None) => {}
        Err(error) => {
            warn!(error = %error, "invalid status target environment");
            return Ok(());
        }
    }

    // Fail silently if not in a multiplexer session. Some agents, including
    // Codex, strip multiplexer env vars from hook command environments; in that
    // case fall back to matching the hook cwd to a live pane cwd.
    for backend in status_backend_candidates() {
        let mux = create_backend(backend);
        if let Some(pane_id) = resolve_status_pane_id(&*mux) {
            return apply_status_update(&cmd, &config, &*mux, &pane_id);
        }
    }

    Ok(())
}

fn apply_status_update(
    cmd: &SetWindowStatusCommand,
    config: &Config,
    mux: &dyn Multiplexer,
    pane_id: &str,
) -> Result<()> {
    match cmd {
        SetWindowStatusCommand::Clear => mux.clear_status(pane_id)?,
        SetWindowStatusCommand::Working
        | SetWindowStatusCommand::Waiting
        | SetWindowStatusCommand::Done => {
            let status = match cmd {
                SetWindowStatusCommand::Working => AgentStatus::Working,
                SetWindowStatusCommand::Waiting => AgentStatus::Waiting,
                SetWindowStatusCommand::Done => AgentStatus::Done,
                SetWindowStatusCommand::Clear => unreachable!(),
            };

            let (icon, auto_clear) = match status {
                AgentStatus::Working => (config.status_icons.working(), false),
                AgentStatus::Waiting => (config.status_icons.waiting(), true),
                AgentStatus::Done => (config.status_icons.done(), true),
            };

            // Ensure the status format is applied so the icon actually shows up
            if config.status_format.unwrap_or(true) {
                let _ = mux.ensure_status_format(pane_id);
            }

            // Update backend UI (status bar icon)
            mux.set_status(pane_id, icon, auto_clear)?;

            // Persist to state store so the dashboard sees this agent
            crate::state::persist_agent_update(mux, pane_id, Some(status), None);
        }
    }

    Ok(())
}

#[derive(Debug, Default)]
struct StatusBackendSignals {
    workmux_backend: bool,
    tmux: bool,
    wezterm: bool,
    zellij: bool,
    kitty: bool,
}

impl StatusBackendSignals {
    fn from_env() -> Self {
        Self {
            workmux_backend: std::env::var_os("WORKMUX_BACKEND").is_some(),
            tmux: std::env::var_os("TMUX").is_some() || std::env::var_os("TMUX_PANE").is_some(),
            wezterm: std::env::var_os("WEZTERM_PANE").is_some(),
            zellij: std::env::var_os("ZELLIJ").is_some()
                || std::env::var_os("ZELLIJ_PANE_ID").is_some()
                || std::env::var_os("ZELLIJ_SESSION_NAME").is_some(),
            kitty: std::env::var_os("KITTY_WINDOW_ID").is_some(),
        }
    }

    fn has_any_signal(&self) -> bool {
        self.workmux_backend || self.tmux || self.wezterm || self.zellij || self.kitty
    }
}

fn status_backend_candidates() -> Vec<BackendType> {
    let signals = StatusBackendSignals::from_env();
    status_backend_candidates_for(detect_backend(), &signals)
}

fn status_backend_candidates_for(
    detected: BackendType,
    signals: &StatusBackendSignals,
) -> Vec<BackendType> {
    let mut backends = vec![detected];

    if !signals.has_any_signal() && detected != BackendType::Zellij {
        backends.push(BackendType::Zellij);
    }

    backends
}

fn resolve_status_pane_id(mux: &dyn Multiplexer) -> Option<String> {
    mux.current_pane_id()
        .or_else(|| resolve_status_pane_id_from_cwd(mux).ok().flatten())
}

fn resolve_status_pane_id_from_cwd(mux: &dyn Multiplexer) -> Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    let live_panes = mux.get_all_live_pane_info()?;
    let backend = mux.name();
    let instance = mux.instance_id();
    let registered_panes = StateStore::new()
        .and_then(|store| store.list_all_agents())
        .map(|agents| {
            agents
                .into_iter()
                .filter(|agent| {
                    agent.pane_key.backend == backend && agent.pane_key.instance == instance
                })
                .map(|agent| agent.pane_key.pane_id)
                .collect()
        })
        .unwrap_or_default();
    Ok(select_pane_for_cwd(&live_panes, &cwd, &registered_panes))
}

fn select_pane_for_cwd(
    live_panes: &HashMap<String, LivePaneInfo>,
    cwd: &Path,
    registered_panes: &HashSet<String>,
) -> Option<String> {
    let cwd = normalized_path(cwd);
    let mut best_score = 0;
    let mut candidates = Vec::new();

    for (pane_id, pane) in live_panes {
        let pane_cwd = normalized_path(&pane.working_dir);
        if !cwd.starts_with(&pane_cwd) {
            continue;
        }

        let score = pane_cwd.components().count();
        match score.cmp(&best_score) {
            std::cmp::Ordering::Greater => {
                best_score = score;
                candidates.clear();
                candidates.push(pane_id);
            }
            std::cmp::Ordering::Equal => candidates.push(pane_id),
            std::cmp::Ordering::Less => {}
        }
    }

    if candidates.len() == 1 {
        return candidates.first().map(|pane_id| (*pane_id).clone());
    }

    let mut registered = candidates
        .into_iter()
        .filter(|pane_id| registered_panes.contains(*pane_id));
    let pane_id = registered.next()?;
    registered.next().is_none().then(|| pane_id.clone())
}

fn normalized_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn live_pane(path: &str) -> LivePaneInfo {
        LivePaneInfo {
            pid: Some(1),
            current_command: Some("codex".to_string()),
            working_dir: PathBuf::from(path),
            title: None,
            session: Some("test".to_string()),
            window: Some("wm-test".to_string()),
            session_id: Some("$1".to_string()),
            window_id: Some("@1".to_string()),
        }
    }

    fn no_backend_signals() -> StatusBackendSignals {
        StatusBackendSignals::default()
    }

    #[test]
    fn status_target_accepts_complete_identity() {
        assert_eq!(
            StatusTarget::from_values(
                Some("zellij".to_string()),
                Some("dev session".to_string()),
                Some("terminal_7".to_string()),
            )
            .unwrap(),
            Some(StatusTarget {
                backend: BackendType::Zellij,
                instance: "dev session".to_string(),
                pane_id: "terminal_7".to_string(),
            })
        );
    }

    #[test]
    fn status_target_rejects_partial_identity() {
        assert!(
            StatusTarget::from_values(Some("zellij".to_string()), Some("dev".to_string()), None,)
                .is_err()
        );
    }

    #[test]
    fn status_target_is_absent_without_identity_variables() {
        assert_eq!(StatusTarget::from_values(None, None, None).unwrap(), None);
    }

    #[test]
    fn select_pane_for_cwd_prefers_exact_match() {
        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), live_pane("/repo"));
        panes.insert("%2".to_string(), live_pane("/repo/subdir"));

        assert_eq!(
            select_pane_for_cwd(&panes, Path::new("/repo/subdir"), &HashSet::new()),
            Some("%2".to_string())
        );
    }

    #[test]
    fn select_pane_for_cwd_accepts_closest_ancestor() {
        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), live_pane("/repo"));
        panes.insert("%2".to_string(), live_pane("/other"));

        assert_eq!(
            select_pane_for_cwd(&panes, Path::new("/repo/nested/package"), &HashSet::new()),
            Some("%1".to_string())
        );
    }

    #[test]
    fn select_pane_for_cwd_rejects_ambiguous_matches() {
        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), live_pane("/repo"));
        panes.insert("%2".to_string(), live_pane("/repo"));

        assert_eq!(
            select_pane_for_cwd(&panes, Path::new("/repo"), &HashSet::new()),
            None
        );
    }

    #[test]
    fn select_pane_for_cwd_prefers_registered_agent_when_shell_panes_match() {
        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), live_pane("/repo"));
        panes.insert("%2".to_string(), live_pane("/repo"));
        panes.insert("%3".to_string(), live_pane("/repo"));
        let registered = HashSet::from(["%1".to_string()]);

        assert_eq!(
            select_pane_for_cwd(&panes, Path::new("/repo"), &registered),
            Some("%1".to_string())
        );
    }

    #[test]
    fn select_pane_for_cwd_rejects_multiple_registered_agents() {
        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), live_pane("/repo"));
        panes.insert("%2".to_string(), live_pane("/repo"));
        let registered = HashSet::from(["%1".to_string(), "%2".to_string()]);

        assert_eq!(
            select_pane_for_cwd(&panes, Path::new("/repo"), &registered),
            None
        );
    }

    #[test]
    fn status_backend_candidates_preserve_detected_backend_when_signaled() {
        let signals = StatusBackendSignals {
            tmux: true,
            ..Default::default()
        };

        assert_eq!(
            status_backend_candidates_for(BackendType::Tmux, &signals),
            vec![BackendType::Tmux]
        );
    }

    #[test]
    fn status_backend_candidates_use_zellij_when_zellij_env_is_detected() {
        let signals = StatusBackendSignals {
            zellij: true,
            ..Default::default()
        };

        assert_eq!(
            status_backend_candidates_for(BackendType::Zellij, &signals),
            vec![BackendType::Zellij]
        );
    }

    #[test]
    fn status_backend_candidates_try_zellij_after_default_tmux_without_env() {
        assert_eq!(
            status_backend_candidates_for(BackendType::Tmux, &no_backend_signals()),
            vec![BackendType::Tmux, BackendType::Zellij]
        );
    }
}
