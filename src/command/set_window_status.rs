use anyhow::Result;
use clap::ValueEnum;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
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
    if std::env::var_os("WORKMUX_DISABLE_SET_WINDOW_STATUS").is_some() {
        return Ok(());
    }

    // Inside a sandbox guest, route through RPC to the host supervisor
    if crate::sandbox::guest::is_sandbox_guest() {
        return run_via_rpc(cmd);
    }

    let config = Config::load(None)?;
    let agent_session_id = read_hook_session_id();

    match StatusTarget::from_env() {
        Ok(Some(target)) => {
            let mux = create_backend_for_instance(target.backend, &target.instance);
            match mux.get_live_pane_info(&target.pane_id) {
                Ok(Some(_)) => {
                    return apply_status_update(
                        &cmd,
                        &config,
                        &*mux,
                        &target.pane_id,
                        agent_session_id.as_deref(),
                    );
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

    // A status update requires identity tied to a live pane. Tmux can recover
    // pane-less hooks through process ancestry, a session binding, or one live
    // registered agent that matches the hook context. Raw cwd never selects a pane.
    for backend in status_backend_candidates() {
        let mux = create_backend(backend);
        if let Some(pane_id) = resolve_status_pane_id(&*mux, agent_session_id.as_deref()) {
            return apply_status_update(
                &cmd,
                &config,
                &*mux,
                &pane_id,
                agent_session_id.as_deref(),
            );
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
            crate::state::persist_agent_update(
                mux,
                pane_id,
                Some(status),
                None,
                agent_session_id.map(str::to_string),
            );
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

#[derive(Deserialize)]
struct HookInput {
    session_id: Option<String>,
}

fn read_hook_session_id() -> Option<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }

    let mut input = String::new();
    stdin.lock().read_to_string(&mut input).ok()?;
    parse_hook_session_id(&input)
}

fn parse_hook_session_id(input: &str) -> Option<String> {
    serde_json::from_str::<HookInput>(input)
        .ok()?
        .session_id
        .filter(|session_id| !session_id.is_empty())
}

#[derive(Debug, Deserialize)]
struct ClaudeJobState {
    cwd: PathBuf,
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(Debug, PartialEq, Eq)]
enum ClaudeJobCwd {
    NotClaudeJob,
    InvalidClaudeJob,
    Remapped { cwd: PathBuf, session_id: String },
}

const MAX_CLAUDE_JOB_STATE_SIZE: u64 = 64 * 1024;

fn read_claude_job_state(path: &Path) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_CLAUDE_JOB_STATE_SIZE {
        return None;
    }

    let file = std::fs::File::open(path).ok()?;
    let mut contents = String::new();
    file.take(MAX_CLAUDE_JOB_STATE_SIZE + 1)
        .read_to_string(&mut contents)
        .ok()?;
    (contents.len() as u64 <= MAX_CLAUDE_JOB_STATE_SIZE).then_some(contents)
}

fn valid_claude_job_session_id(session_id: &str, short_id: &str) -> bool {
    if session_id.len() != 36 || session_id.get(..8) != Some(short_id) {
        return false;
    }

    session_id.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

fn classify_claude_job_cwd(hook_cwd: &Path, claude_dir: &Path) -> ClaudeJobCwd {
    let jobs_dir = normalized_path(&claude_dir.join("jobs"));
    let hook_cwd = normalized_path(hook_cwd);
    let Ok(relative_job_path) = hook_cwd.strip_prefix(&jobs_dir) else {
        return ClaudeJobCwd::NotClaudeJob;
    };
    let Some(short_id) = relative_job_path.components().next() else {
        return ClaudeJobCwd::InvalidClaudeJob;
    };
    let Some(short_id) = short_id.as_os_str().to_str() else {
        return ClaudeJobCwd::InvalidClaudeJob;
    };
    if short_id.len() != 8
        || !short_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return ClaudeJobCwd::InvalidClaudeJob;
    }

    let state_path = jobs_dir.join(short_id).join("state.json");
    let Some(state) = read_claude_job_state(&state_path) else {
        return ClaudeJobCwd::InvalidClaudeJob;
    };
    let Ok(state) = serde_json::from_str::<ClaudeJobState>(&state) else {
        return ClaudeJobCwd::InvalidClaudeJob;
    };
    if !state.cwd.is_absolute()
        || !state.cwd.is_dir()
        || !valid_claude_job_session_id(&state.session_id, short_id)
    {
        return ClaudeJobCwd::InvalidClaudeJob;
    }

    ClaudeJobCwd::Remapped {
        cwd: state.cwd,
        session_id: state.session_id,
    }
}

fn normalized_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_status_pane_id(mux: &dyn Multiplexer, agent_session_id: Option<&str>) -> Option<String> {
    if let Some(pane_id) = mux.current_pane_id().filter(|pane_id| !pane_id.is_empty()) {
        return Some(pane_id);
    }

    if mux.name() != "tmux" {
        return None;
    }

    let live_panes = mux.get_all_live_pane_info().ok()?;
    if let Ok(parents) = process_parent_snapshot()
        && let Some(pane_id) =
            select_pane_for_process_ancestry(&live_panes, &parents, std::process::id())
    {
        return Some(pane_id);
    }

    let agent_session_id = agent_session_id?;
    let agents = StateStore::new().ok()?.list_all_agents().ok()?;
    let server_boot_id = mux.server_boot_id().ok().flatten();
    let instance = mux.instance_id();
    if let Some(pane_id) = select_pane_for_agent_session(
        &agents,
        &live_panes,
        mux.name(),
        &instance,
        agent_session_id,
        server_boot_id.as_deref(),
    ) {
        return Some(pane_id);
    }

    let hook_cwd = std::env::current_dir().ok()?;
    let (status_cwd, required_agent_kind) = match crate::agent_setup::claude::claude_dir()
        .map(|claude_dir| classify_claude_job_cwd(&hook_cwd, &claude_dir))
        .unwrap_or(ClaudeJobCwd::NotClaudeJob)
    {
        ClaudeJobCwd::NotClaudeJob => (hook_cwd, None),
        ClaudeJobCwd::InvalidClaudeJob => return None,
        ClaudeJobCwd::Remapped { cwd, session_id } if session_id == agent_session_id => {
            (cwd, Some("claude"))
        }
        ClaudeJobCwd::Remapped { .. } => return None,
    };

    select_registered_pane_for_cwd(
        &agents,
        &live_panes,
        &RegisteredPaneQuery {
            backend: mux.name(),
            instance: &instance,
            cwd: &status_cwd,
            required_agent_kind,
            agent_session_id,
            server_boot_id: server_boot_id.as_deref(),
        },
    )
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

struct RegisteredPaneQuery<'a> {
    backend: &'a str,
    instance: &'a str,
    cwd: &'a Path,
    required_agent_kind: Option<&'a str>,
    agent_session_id: &'a str,
    server_boot_id: Option<&'a str>,
}

fn select_registered_pane_for_cwd(
    agents: &[AgentState],
    live_panes: &HashMap<String, LivePaneInfo>,
    query: &RegisteredPaneQuery<'_>,
) -> Option<String> {
    let cwd = normalized_path(query.cwd);
    let mut best_score = 0;
    let mut candidates = Vec::new();

    for agent in agents {
        let Some(agent_kind) = agent.agent_kind.as_deref() else {
            continue;
        };
        if agent.pane_key.backend != query.backend
            || agent.pane_key.instance != query.instance
            || query
                .required_agent_kind
                .is_some_and(|kind| agent_kind != kind)
            || agent
                .agent_session_id
                .as_deref()
                .is_some_and(|session_id| session_id != query.agent_session_id)
            || query
                .server_boot_id
                .is_none_or(|live| agent.boot_id.as_deref() != Some(live))
            || !live_panes.get(&agent.pane_key.pane_id).is_some_and(|pane| {
                agent.pane_pid != 0
                    && pane.pid == Some(agent.pane_pid)
                    && pane.current_command.as_deref() == Some(agent.command.as_str())
            })
        {
            continue;
        }

        let workdir = normalized_path(&agent.workdir);
        if !cwd.starts_with(&workdir) {
            continue;
        }

        let score = workdir.components().count();
        match score.cmp(&best_score) {
            std::cmp::Ordering::Greater => {
                best_score = score;
                candidates.clear();
                candidates.push(agent.pane_key.pane_id.clone());
            }
            std::cmp::Ordering::Equal => candidates.push(agent.pane_key.pane_id.clone()),
            std::cmp::Ordering::Less => {}
        }
    }

    (candidates.len() == 1).then(|| candidates.remove(0))
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

    fn live_pane(pid: u32, command: &str) -> LivePaneInfo {
        live_pane_at(pid, command, "/repo", None)
    }

    fn live_pane_at(pid: u32, command: &str, path: &str, title: Option<&str>) -> LivePaneInfo {
        LivePaneInfo {
            pid: Some(pid),
            current_command: Some(command.to_string()),
            working_dir: std::path::PathBuf::from(path),
            title: title.map(str::to_string),
            session: Some("test".to_string()),
            window: Some("wm-test".to_string()),
            session_id: Some("$1".to_string()),
            window_id: Some("@1".to_string()),
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

    fn claude_job_fixture(state: Option<&str>) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let claude_dir = temp.path().join(".claude");
        let job_dir = claude_dir.join("jobs/deadbeef");
        let worktree = temp.path().join("worktree");
        std::fs::create_dir_all(&job_dir).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        if let Some(state) = state {
            std::fs::write(job_dir.join("state.json"), state).unwrap();
        }
        (temp, claude_dir, job_dir, worktree)
    }

    fn valid_claude_job_state(cwd: &Path) -> String {
        serde_json::json!({
            "cwd": cwd,
            "sessionId": "deadbeef-0000-4000-8000-000000000000"
        })
        .to_string()
    }

    #[test]
    fn classify_claude_job_cwd_remaps_job_root_and_nested_directory() {
        let (_temp, claude_dir, job_dir, worktree) = claude_job_fixture(None);
        std::fs::write(
            job_dir.join("state.json"),
            valid_claude_job_state(&worktree),
        )
        .unwrap();
        let nested = job_dir.join("tmp/checkout");
        std::fs::create_dir_all(&nested).unwrap();

        for hook_cwd in [&job_dir, &nested] {
            assert_eq!(
                classify_claude_job_cwd(hook_cwd, &claude_dir),
                ClaudeJobCwd::Remapped {
                    cwd: worktree.clone(),
                    session_id: "deadbeef-0000-4000-8000-000000000000".to_string(),
                }
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn classify_claude_job_cwd_resolves_symlinked_config_root() {
        use std::os::unix::fs::symlink;

        let (temp, claude_dir, job_dir, worktree) = claude_job_fixture(None);
        std::fs::write(
            job_dir.join("state.json"),
            valid_claude_job_state(&worktree),
        )
        .unwrap();
        let hook_cwd = job_dir.join("tmp/checkout");
        std::fs::create_dir_all(&hook_cwd).unwrap();
        let claude_dir_link = temp.path().join("claude-config-link");
        symlink(&claude_dir, &claude_dir_link).unwrap();

        assert!(matches!(
            classify_claude_job_cwd(&hook_cwd, &claude_dir_link),
            ClaudeJobCwd::Remapped { cwd, .. } if cwd == worktree
        ));
    }

    #[test]
    fn classify_claude_job_cwd_rejects_invalid_state() {
        let invalid_states = [
            None,
            Some("not json"),
            Some(r#"{"sessionId":"deadbeef-session"}"#),
            Some(r#"{"cwd":"/"}"#),
            Some(r#"{"cwd":"relative/path","sessionId":"deadbeef-session"}"#),
            Some(r#"{"cwd":"/","sessionId":"cafebabe-0000-4000-8000-000000000000"}"#),
            Some(r#"{"cwd":"/","sessionId":"deadbeef-not-a-valid-session-id"}"#),
        ];
        for state in invalid_states {
            let (_temp, claude_dir, job_dir, _worktree) = claude_job_fixture(state);
            assert_eq!(
                classify_claude_job_cwd(&job_dir, &claude_dir),
                ClaudeJobCwd::InvalidClaudeJob
            );
        }
    }

    #[test]
    fn classify_claude_job_cwd_rejects_oversized_state() {
        let (_temp, claude_dir, job_dir, _worktree) = claude_job_fixture(None);
        std::fs::write(
            job_dir.join("state.json"),
            vec![b' '; MAX_CLAUDE_JOB_STATE_SIZE as usize + 1],
        )
        .unwrap();

        assert_eq!(
            classify_claude_job_cwd(&job_dir, &claude_dir),
            ClaudeJobCwd::InvalidClaudeJob
        );
    }

    #[cfg(unix)]
    #[test]
    fn classify_claude_job_cwd_rejects_symlinked_state() {
        use std::os::unix::fs::symlink;

        let (temp, claude_dir, job_dir, worktree) = claude_job_fixture(None);
        let state_target = temp.path().join("state-target.json");
        std::fs::write(&state_target, valid_claude_job_state(&worktree)).unwrap();
        symlink(state_target, job_dir.join("state.json")).unwrap();

        assert_eq!(
            classify_claude_job_cwd(&job_dir, &claude_dir),
            ClaudeJobCwd::InvalidClaudeJob
        );
    }

    #[test]
    fn registered_cwd_fallback_requires_one_live_matching_agent() {
        let unbound_agent = |pane_id, pane_pid| {
            let mut agent = agent_state(pane_id, pane_pid, "unused-session");
            agent.agent_session_id = None;
            agent
        };
        let query = RegisteredPaneQuery {
            backend: "tmux",
            instance: "default",
            cwd: Path::new("/repo/subdir"),
            required_agent_kind: Some("claude"),
            agent_session_id: "session-1",
            server_boot_id: Some("boot-1"),
        };
        let panes = HashMap::from([
            ("%1".to_string(), live_pane(100, "claude")),
            ("%2".to_string(), live_pane(200, "claude")),
        ]);

        assert_eq!(
            select_registered_pane_for_cwd(&[unbound_agent("%1", 100)], &panes, &query),
            Some("%1".to_string())
        );
        assert_eq!(
            select_registered_pane_for_cwd(
                &[unbound_agent("%1", 100), unbound_agent("%2", 200)],
                &panes,
                &query,
            ),
            None
        );

        let conflicting = agent_state("%1", 100, "different-session");
        assert_eq!(
            select_registered_pane_for_cwd(&[conflicting], &panes, &query),
            None
        );
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
    fn parses_hook_session_identity() {
        assert_eq!(
            parse_hook_session_id(r#"{"session_id":"session-1","cwd":"/repo"}"#),
            Some("session-1".to_string())
        );
        assert_eq!(parse_hook_session_id(r#"{"session_id":""}"#), None);
        assert_eq!(parse_hook_session_id("not json"), None);
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
