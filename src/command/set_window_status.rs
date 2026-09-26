use anyhow::Result;
use clap::ValueEnum;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{IsTerminal, Read, Seek, SeekFrom};
use std::path::Path;
use tracing::warn;

use crate::config::Config;
use crate::multiplexer::{
    AgentStatus, BackendType, LivePaneInfo, Multiplexer, STATUS_TARGET_BACKEND_ENV,
    STATUS_TARGET_INSTANCE_ENV, STATUS_TARGET_PANE_ENV, create_backend,
    create_backend_for_instance, detect_backend,
};
use crate::state::{AgentState, StateStore};

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
        if !matches!(backend, BackendType::Tmux | BackendType::Zellij) {
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
    if status_tracking_disabled() {
        return Ok(());
    }

    // Inside a sandbox guest, route through RPC to the host supervisor
    if crate::sandbox::guest::is_sandbox_guest() {
        return run_via_rpc(cmd);
    }

    let config = Config::load(None)?;
    let hook = read_hook_input();
    run_for_status_target(hook.as_ref(), |mux, pane_id| {
        apply_status_update(
            &cmd,
            &config,
            mux,
            pane_id,
            hook.as_ref().and_then(HookInput::session_id),
        )
    })
}

pub fn register_agent() -> Result<()> {
    if status_tracking_disabled() {
        return Ok(());
    }

    if crate::sandbox::guest::is_sandbox_guest() {
        return register_via_rpc();
    }

    let hook = read_hook_input();
    run_for_status_target(hook.as_ref(), |mux, pane_id| {
        let _ = mux.clear_status(pane_id);
        crate::state::persist_agent_registration(
            mux,
            pane_id,
            hook.as_ref()
                .and_then(HookInput::session_id)
                .map(str::to_string),
        );
        crate::command::sidebar::request_refresh_for(mux);
        Ok(())
    })
}

fn status_tracking_disabled() -> bool {
    std::env::var_os("WORKMUX_DISABLE_SET_WINDOW_STATUS").is_some()
}

fn run_for_status_target(
    hook: Option<&HookInput>,
    mut update: impl FnMut(&dyn Multiplexer, &str) -> Result<()>,
) -> Result<()> {
    match StatusTarget::from_env() {
        Ok(Some(target)) => {
            let mux = create_backend_for_instance(target.backend, &target.instance);
            match mux.get_live_pane_info(&target.pane_id) {
                Ok(Some(_)) => return update(&*mux, &target.pane_id),
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

    // A status update requires identity tied to a live pane. Hooks can lose
    // multiplexer variables, so tmux additionally accepts an exact agent
    // session binding or process ancestry with agent-specific ownership checks.
    for backend in status_backend_candidates() {
        let mux = create_backend(backend);
        if let Some(pane_id) = resolve_status_pane_id(&*mux, hook) {
            return update(&*mux, &pane_id);
        }
    }

    Ok(())
}

fn apply_status_update(
    cmd: &SetWindowStatusCommand,
    config: &Config,
    mux: &dyn Multiplexer,
    pane_id: &str,
    agent_session_id: Option<&str>,
) -> Result<()> {
    match cmd {
        SetWindowStatusCommand::Clear => {
            mux.clear_status(pane_id)?;
            crate::state::clear_agent_status(mux, pane_id);
        }
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
            crate::state::persist_agent_update(
                mux,
                pane_id,
                Some(status),
                None,
                agent_session_id.map(str::to_string),
            );
        }
    }

    crate::command::sidebar::request_refresh_for(mux);
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

#[derive(Deserialize)]
struct HookInput {
    session_id: Option<String>,
    transcript_path: Option<String>,
}

impl HookInput {
    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref().filter(|value| !value.is_empty())
    }

    fn transcript_path(&self) -> Option<&Path> {
        self.transcript_path
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(Path::new)
    }
}

fn read_hook_input() -> Option<HookInput> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }

    let mut input = String::new();
    stdin.lock().read_to_string(&mut input).ok()?;
    parse_hook_input(&input)
}

fn parse_hook_input(input: &str) -> Option<HookInput> {
    serde_json::from_str(input).ok()
}

fn resolve_status_pane_id(mux: &dyn Multiplexer, hook: Option<&HookInput>) -> Option<String> {
    if let Some(pane_id) = mux.current_pane_id().filter(|pane_id| !pane_id.is_empty()) {
        return Some(pane_id);
    }

    if mux.name() != "tmux" {
        return None;
    }

    let live_panes = mux.get_all_live_pane_info().ok()?;
    let agents = StateStore::new().ok()?.list_all_agents().ok()?;
    let server_boot_id = mux.server_boot_id().ok().flatten();
    let instance = mux.instance_id();

    if let Some(agent_session_id) = hook.and_then(HookInput::session_id)
        && let Some(pane_id) = select_pane_for_agent_session(
            &agents,
            &live_panes,
            mux.name(),
            &instance,
            agent_session_id,
            server_boot_id.as_deref(),
        )
    {
        return Some(pane_id);
    }

    let parents = process_parent_snapshot().ok()?;
    let pane_id = select_pane_for_process_ancestry(&live_panes, &parents, std::process::id())?;
    if !claude_background_hook() {
        return Some(pane_id);
    }

    // Claude background workers share their supervisor's process ancestry.
    // Only a transcript continuation proves that the worker is attached to
    // the pane rather than an unrelated background session.
    let hook = hook?;
    continuation_owns_ancestry_pane(
        &agents,
        &live_panes,
        mux.name(),
        &instance,
        &pane_id,
        hook,
        server_boot_id.as_deref(),
    )
    .then_some(pane_id)
}

fn claude_background_hook() -> bool {
    std::env::var_os("CLAUDE_JOB_DIR").is_some_and(|value| !value.is_empty())
}

fn continuation_owns_ancestry_pane(
    agents: &[AgentState],
    live_panes: &HashMap<String, LivePaneInfo>,
    backend: &str,
    instance: &str,
    pane_id: &str,
    hook: &HookInput,
    server_boot_id: Option<&str>,
) -> bool {
    let Some(new_session_id) = hook.session_id() else {
        return false;
    };
    let Some(transcript_path) = hook.transcript_path() else {
        return false;
    };
    let Some(agent) = agents.iter().find(|agent| {
        agent.pane_key.backend == backend
            && agent.pane_key.instance == instance
            && agent.pane_key.pane_id == pane_id
            && server_boot_id.is_some_and(|live| agent.boot_id.as_deref() == Some(live))
            && live_panes.get(pane_id).is_some_and(|pane| {
                agent.pane_pid != 0
                    && pane.pid == Some(agent.pane_pid)
                    && pane.current_command.as_deref() == Some(agent.command.as_str())
            })
    }) else {
        return false;
    };
    let Some(previous_session_id) = agent.agent_session_id.as_deref() else {
        return false;
    };

    transcript_records_continuation(transcript_path, previous_session_id, new_session_id)
}

fn transcript_records_continuation(
    current_transcript: &Path,
    previous_session_id: &str,
    new_session_id: &str,
) -> bool {
    const MAX_TAIL_BYTES: u64 = 64 * 1024;

    if previous_session_id == new_session_id
        || !valid_session_id(previous_session_id)
        || !valid_session_id(new_session_id)
        || current_transcript.file_name() != Some(OsStr::new(&format!("{new_session_id}.jsonl")))
    {
        return false;
    }

    let Some(parent) = current_transcript.parent() else {
        return false;
    };
    let previous_transcript = parent.join(format!("{previous_session_id}.jsonl"));
    let Ok(mut file) = File::open(previous_transcript) else {
        return false;
    };
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
        return false;
    };
    let start = length.saturating_sub(MAX_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut tail = Vec::new();
    if file.read_to_end(&mut tail).is_err() {
        return false;
    }
    let tail = String::from_utf8_lossy(&tail);

    tail.lines().any(|line| {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        record.get("type").and_then(|value| value.as_str()) == Some("continued-in")
            && record.get("sessionId").and_then(|value| value.as_str()) == Some(previous_session_id)
            && record
                .get("continuedInSessionId")
                .and_then(|value| value.as_str())
                == Some(new_session_id)
    })
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn process_parent_snapshot() -> Result<HashMap<u32, u32>> {
    let output = crate::cmd::Cmd::new("ps")
        .args(&["-axo", "pid=,ppid="])
        .run_and_capture_stdout()?;
    Ok(parse_process_parents(&output))
}

fn parse_process_parents(output: &str) -> HashMap<u32, u32> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent = fields.next()?.parse().ok()?;
            Some((pid, parent))
        })
        .collect()
}

fn select_pane_for_process_ancestry(
    live_panes: &HashMap<String, LivePaneInfo>,
    parents: &HashMap<u32, u32>,
    start_pid: u32,
) -> Option<String> {
    let panes_by_pid = live_panes.iter().fold(
        HashMap::<u32, Vec<&String>>::new(),
        |mut panes_by_pid, (pane_id, pane)| {
            if let Some(pid) = pane.pid {
                panes_by_pid.entry(pid).or_default().push(pane_id);
            }
            panes_by_pid
        },
    );

    let mut seen = HashSet::new();
    let mut pid = start_pid;
    for _ in 0..64 {
        if pid <= 1 || !seen.insert(pid) {
            break;
        }
        if let Some(panes) = panes_by_pid.get(&pid) {
            return (panes.len() == 1).then(|| panes[0].to_string());
        }
        pid = *parents.get(&pid)?;
    }
    None
}

fn select_pane_for_agent_session(
    agents: &[AgentState],
    live_panes: &HashMap<String, LivePaneInfo>,
    backend: &str,
    instance: &str,
    agent_session_id: &str,
    server_boot_id: Option<&str>,
) -> Option<String> {
    let mut candidates = agents.iter().filter(|agent| {
        agent.pane_key.backend == backend
            && agent.pane_key.instance == instance
            && agent.agent_session_id.as_deref() == Some(agent_session_id)
            && server_boot_id.is_some_and(|live| agent.boot_id.as_deref() == Some(live))
            && live_panes.get(&agent.pane_key.pane_id).is_some_and(|pane| {
                agent.pane_pid != 0
                    && pane.pid == Some(agent.pane_pid)
                    && pane.current_command.as_deref() == Some(agent.command.as_str())
            })
    });
    let pane_id = candidates.next()?.pane_key.pane_id.clone();
    candidates.next().is_none().then_some(pane_id)
}

fn register_via_rpc() -> Result<()> {
    run_status_via_rpc("register")
}

fn run_via_rpc(cmd: SetWindowStatusCommand) -> Result<()> {
    let status = match cmd {
        SetWindowStatusCommand::Working => "working",
        SetWindowStatusCommand::Waiting => "waiting",
        SetWindowStatusCommand::Done => "done",
        SetWindowStatusCommand::Clear => "clear",
    };

    run_status_via_rpc(status)
}

fn run_status_via_rpc(status: &str) -> Result<()> {
    use crate::sandbox::rpc::{RpcClient, RpcRequest, RpcResponse};

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

    fn live_pane(pid: u32, command: &str) -> LivePaneInfo {
        LivePaneInfo {
            pid: Some(pid),
            current_command: Some(command.to_string()),
            working_dir: std::path::PathBuf::from("/repo"),
            title: None,
            session: Some("test".to_string()),
            window: Some("wm-test".to_string()),
            session_id: Some("$1".to_string()),
            window_id: Some("@1".to_string()),
            window_index: Some(1),
        }
    }

    fn agent_state(pane_id: &str, pane_pid: u32, agent_session_id: &str) -> AgentState {
        AgentState {
            pane_key: crate::state::PaneKey {
                backend: "tmux".to_string(),
                instance: "default".to_string(),
                pane_id: pane_id.to_string(),
            },
            workdir: std::path::PathBuf::from("/repo"),
            status: Some(AgentStatus::Working),
            status_ts: Some(1),
            activity_ts: Some(1),
            pane_title: None,
            pane_pid,
            command: "claude".to_string(),
            updated_ts: 1,
            window_name: Some("wm-test".to_string()),
            session_name: Some("test".to_string()),
            boot_id: Some("boot-1".to_string()),
            agent_kind: Some("claude".to_string()),
            agent_session_id: Some(agent_session_id.to_string()),
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
    fn status_target_accepts_tmux_identity() {
        assert_eq!(
            StatusTarget::from_values(
                Some("tmux".to_string()),
                Some("/tmp/tmux.sock".to_string()),
                Some("%7".to_string()),
            )
            .unwrap(),
            Some(StatusTarget {
                backend: BackendType::Tmux,
                instance: "/tmp/tmux.sock".to_string(),
                pane_id: "%7".to_string(),
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
    fn parses_hook_identity_and_transcript() {
        let hook = parse_hook_input(
            r#"{"session_id":"session-1","transcript_path":"/repo/session-1.jsonl"}"#,
        )
        .unwrap();
        assert_eq!(hook.session_id(), Some("session-1"));
        assert_eq!(
            hook.transcript_path(),
            Some(Path::new("/repo/session-1.jsonl"))
        );

        let empty = parse_hook_input(r#"{"session_id":"","transcript_path":""}"#).unwrap();
        assert_eq!(empty.session_id(), None);
        assert_eq!(empty.transcript_path(), None);
        assert!(parse_hook_input("not json").is_none());
    }

    #[test]
    fn transcript_continuation_requires_exact_lineage() {
        let dir = tempfile::tempdir().unwrap();
        let old_transcript = dir.path().join("session-old.jsonl");
        let new_transcript = dir.path().join("session-new.jsonl");
        std::fs::write(
            &old_transcript,
            concat!(
                "{\"type\":\"user\"}\n",
                "{\"type\":\"continued-in\",\"sessionId\":\"session-old\",",
                "\"continuedInSessionId\":\"session-new\"}\n"
            ),
        )
        .unwrap();
        std::fs::write(&new_transcript, "").unwrap();

        assert!(transcript_records_continuation(
            &new_transcript,
            "session-old",
            "session-new"
        ));
        assert!(!transcript_records_continuation(
            &new_transcript,
            "another-session",
            "session-new"
        ));
        assert!(!transcript_records_continuation(
            &new_transcript,
            "session-old",
            "another-session"
        ));
    }

    #[test]
    fn transcript_continuation_rejects_untrusted_session_paths() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("session-new.jsonl");
        std::fs::write(dir.path().join("session-old.jsonl"), "").unwrap();

        assert!(!transcript_records_continuation(
            &transcript,
            "../session-old",
            "session-new"
        ));
        assert!(!transcript_records_continuation(
            &dir.path().join("wrong-name.jsonl"),
            "session-old",
            "session-new"
        ));
    }

    #[test]
    fn parse_process_snapshot_ignores_malformed_rows() {
        assert_eq!(
            parse_process_parents("  10  7\nmalformed\n  7  1\n"),
            HashMap::from([(10, 7), (7, 1)])
        );
    }

    #[test]
    fn process_ancestry_resolves_exact_pane() {
        let panes = HashMap::from([
            ("%1".to_string(), live_pane(100, "claude")),
            ("%2".to_string(), live_pane(200, "zsh")),
        ]);
        let parents = HashMap::from([(900, 800), (800, 700), (700, 100), (100, 1)]);

        assert_eq!(
            select_pane_for_process_ancestry(&panes, &parents, 900),
            Some("%1".to_string())
        );
    }

    #[test]
    fn process_ancestry_prefers_nearest_pane_root() {
        let panes = HashMap::from([
            ("%1".to_string(), live_pane(100, "claude")),
            ("%2".to_string(), live_pane(700, "claude")),
        ]);
        let parents = HashMap::from([(900, 700), (700, 100), (100, 1)]);

        assert_eq!(
            select_pane_for_process_ancestry(&panes, &parents, 900),
            Some("%2".to_string())
        );
    }

    #[test]
    fn process_ancestry_refuses_unrelated_process() {
        let panes = HashMap::from([("%1".to_string(), live_pane(100, "claude"))]);
        let parents = HashMap::from([(900, 800), (800, 1)]);

        assert_eq!(
            select_pane_for_process_ancestry(&panes, &parents, 900),
            None
        );
    }

    #[test]
    fn agent_session_resolves_same_live_process() {
        let agents = vec![agent_state("%1", 100, "session-1")];
        let panes = HashMap::from([("%1".to_string(), live_pane(100, "claude"))]);

        assert_eq!(
            select_pane_for_agent_session(
                &agents,
                &panes,
                "tmux",
                "default",
                "session-1",
                Some("boot-1"),
            ),
            Some("%1".to_string())
        );
    }

    #[test]
    fn agent_session_refuses_reused_or_ambiguous_pane() {
        let agents = vec![
            agent_state("%1", 100, "session-1"),
            agent_state("%2", 200, "session-1"),
        ];
        let panes = HashMap::from([
            ("%1".to_string(), live_pane(100, "claude")),
            ("%2".to_string(), live_pane(200, "claude")),
        ]);

        assert_eq!(
            select_pane_for_agent_session(
                &agents,
                &panes,
                "tmux",
                "default",
                "session-1",
                Some("boot-1"),
            ),
            None
        );

        let changed_pid = HashMap::from([("%1".to_string(), live_pane(999, "claude"))]);
        assert_eq!(
            select_pane_for_agent_session(
                &agents[..1],
                &changed_pid,
                "tmux",
                "default",
                "session-1",
                Some("boot-1"),
            ),
            None
        );

        let changed_command = HashMap::from([("%1".to_string(), live_pane(100, "zsh"))]);
        assert_eq!(
            select_pane_for_agent_session(
                &agents[..1],
                &changed_command,
                "tmux",
                "default",
                "session-1",
                Some("boot-1"),
            ),
            None
        );

        assert_eq!(
            select_pane_for_agent_session(
                &agents[..1],
                &panes,
                "tmux",
                "default",
                "session-1",
                None,
            ),
            None
        );
    }

    #[test]
    fn ancestry_candidate_requires_continuation_for_background_hook() {
        let dir = tempfile::tempdir().unwrap();
        let old_transcript = dir.path().join("session-old.jsonl");
        let new_transcript = dir.path().join("session-new.jsonl");
        std::fs::write(
            old_transcript,
            concat!(
                "{\"type\":\"continued-in\",\"sessionId\":\"session-old\",",
                "\"continuedInSessionId\":\"session-new\"}\n"
            ),
        )
        .unwrap();
        std::fs::write(&new_transcript, "").unwrap();

        let agents = vec![agent_state("%1", 100, "session-old")];
        let panes = HashMap::from([("%1".to_string(), live_pane(100, "claude"))]);
        let hook = HookInput {
            session_id: Some("session-new".to_string()),
            transcript_path: Some(new_transcript.display().to_string()),
        };

        assert!(continuation_owns_ancestry_pane(
            &agents,
            &panes,
            "tmux",
            "default",
            "%1",
            &hook,
            Some("boot-1"),
        ));

        let unrelated = HookInput {
            session_id: Some("unrelated-session".to_string()),
            transcript_path: Some(
                dir.path()
                    .join("unrelated-session.jsonl")
                    .display()
                    .to_string(),
            ),
        };
        assert!(!continuation_owns_ancestry_pane(
            &agents,
            &panes,
            "tmux",
            "default",
            "%1",
            &unrelated,
            Some("boot-1"),
        ));
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
