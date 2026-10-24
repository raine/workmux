//! `node watch`, `node send` and `node capture`: what the laptop needs to
//! show and drive the agents of running executions.
//!
//! Agent state comes from the node's own store (`$root/state`, where the
//! node-mode supervisor and the `isolation: none` wrapper write it),
//! reconciled against the node tmux server. Node tmux status options are not
//! used: the attach client keeps each session focused, which clears them.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::execution::{tmux, tmux_socket};
use super::{ExecutionState, NodeRoot, Origin, Workspace, refuse, validate_id};
use crate::config::RemoteIsolation;
use crate::git::{GitStatus, GitStatusCache};
use crate::multiplexer::{
    AgentPane, AgentStatus, EndReason, LivePaneInfo, Multiplexer, tmux::TmuxBackend,
};
use crate::remote::protocol::{WorkspaceIdentity, code, session_name};
use crate::state::{AgentStateCache, StateStore};

/// How often a streaming watch looks at the node.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How often a streaming watch proves it is alive when nothing changes.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Longest title or prompt passed on.
const MAX_TEXT: usize = 300;
/// Largest `send` payload.
const MAX_SEND: usize = 1 << 20;
/// Most lines `capture` returns.
const MAX_CAPTURE_LINES: u16 = 10_000;
/// How long an agent that has not reported yet gets to start before text
/// is typed into it.
const AGENT_STARTUP: Duration = Duration::from_secs(5);
/// How often a streaming watch looks at a workspace's git status.
const GIT_INTERVAL: Duration = Duration::from_secs(5);

/// `watch`: one snapshot, or with `stream` NDJSON [`WatchFrame`]s until the
/// laptop goes away.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatchRequest {
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatchFrame {
    /// Sent first and whenever the snapshot changes.
    Snapshot(Snapshot),
    /// Sent when nothing changed for [`HEARTBEAT_INTERVAL`].
    Heartbeat { now: u64 },
    /// The node could not be observed; the last snapshot still stands.
    Error { message: String },
}

/// The agents of every execution on the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Node clock, seconds since the epoch.
    pub now: u64,
    pub workspaces: Vec<ObservedWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedWorkspace {
    pub ws_id: String,
    pub ex_id: String,
    pub panes: Vec<ObservedPane>,
    /// Git status of the worktree. `ahead` counts the commits since the
    /// workspace's base; `behind` and `base_branch` are the laptop's to fill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitStatus>,
    /// Set while a workspace that a remote agent spawned runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sibling: Option<Sibling>,
    /// Set when the execution ended without being stopped; `start-execution`
    /// starts it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<EndReason>,
}

/// What the laptop needs to adopt a workspace a remote agent spawned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sibling {
    pub identity: WorkspaceIdentity,
    /// Worktree path on the node.
    pub worktree: String,
    /// Node binary that started the execution, under `bin/`.
    pub pin: String,
    #[serde(flatten)]
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedPane {
    /// Index of the configured pane.
    pub index: usize,
    /// Whether the pane's session exists.
    pub alive: bool,
    /// The agent registered in the pane, if any.
    pub agent: Option<ObservedAgent>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedAgent {
    pub status: Option<AgentStatus>,
    /// Node clock.
    pub status_ts: Option<u64>,
    pub title: Option<String>,
    pub agent_kind: Option<String>,
    pub prompt: Option<String>,
}

/// `send`: type `text` into an agent. Without `pane`, the lowest pane of
/// the workspace that has an agent, else its agent pane while a container
/// agent starts up and has not reported yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendRequest {
    pub ws_id: String,
    pub pane: Option<usize>,
    pub text: String,
}

/// Where the text went and the state of the agent there just before it,
/// which later states of the turn it starts are newer than. Nodes before
/// this answered `{}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SendResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentStatus>,
    /// Node clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_ts: Option<u64>,
}

/// `capture`: the last `lines` of an agent pane, chosen as for `send`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureRequest {
    pub ws_id: String,
    pub pane: Option<usize>,
    pub lines: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureResponse {
    pub text: String,
}

/// Display text from the guest: control characters removed, length capped,
/// and terminal queries that tmux mistook for a title dropped.
pub fn clean_text(text: &str) -> Option<String> {
    let cleaned: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty() && !is_graphics_query(cleaned)).then(|| cleaned.to_string())
}

/// Whether `title` is a kitty graphics protocol command (`G<key>=<value>,…;
/// <payload>`), which tmux takes from an APC sequence as a pane title.
fn is_graphics_query(title: &str) -> bool {
    let Some(rest) = title.strip_prefix('G') else {
        return false;
    };
    let Some((keys, _payload)) = rest.split_once(';') else {
        return false;
    };
    keys.split(',').all(|pair| {
        pair.split_once('=').is_some_and(|(key, value)| {
            key.len() == 1
                && key.bytes().all(|b| b.is_ascii_alphabetic())
                && !value.is_empty()
                && value.bytes().all(|b| b.is_ascii_alphanumeric())
        })
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Reads the node across polls, keeping state files cached.
struct Observer {
    root: NodeRoot,
    socket: std::path::PathBuf,
    mux: TmuxBackend,
    cache: AgentStateCache,
    store: StateStore,
    host: Option<String>,
    git_cache: GitStatusCache,
    /// Last git status of each workspace and when it was read.
    git: HashMap<String, (Instant, Option<GitStatus>)>,
    /// Kernel boot this process runs in.
    boot: Option<String>,
}

impl Observer {
    fn new(root: &NodeRoot) -> Result<Self> {
        let socket = tmux_socket(root)?;
        Ok(Self {
            root: root.clone(),
            mux: TmuxBackend::for_socket(&socket.to_string_lossy()),
            socket,
            cache: AgentStateCache::default(),
            store: StateStore::new()?,
            host: hostname(),
            git_cache: GitStatusCache::default(),
            git: HashMap::new(),
            boot: super::boot_id(),
        })
    }

    /// Live panes of the node tmux server; none when no server runs (tmux
    /// exits with its last session).
    fn live_panes(&self) -> Result<HashMap<String, LivePaneInfo>> {
        match self.mux.get_all_live_pane_info() {
            Ok(panes) => Ok(panes),
            Err(err) if !server_running(&self.socket)? => {
                tracing::debug!("node tmux server not running: {err:#}");
                Ok(HashMap::new())
            }
            Err(err) => Err(err),
        }
    }

    /// The node as of `live`, the panes [`Observer::live_panes`] returned.
    fn snapshot(
        &mut self,
        live: &HashMap<String, LivePaneInfo>,
        only: Option<&str>,
    ) -> Result<Snapshot> {
        let mut workspaces: Vec<Workspace> = self
            .root
            .workspaces()
            .into_iter()
            .filter(|ws| ws.execution.is_some())
            .filter(|ws| only.is_none_or(|ws_id| ws.identity.ws_id == ws_id))
            .collect();
        workspaces.sort_by(|a, b| a.identity.ws_id.cmp(&b.identity.ws_id));
        let agents = if live.is_empty() {
            Vec::new()
        } else {
            let boot_id = self.mux.server_boot_id().unwrap_or(None);
            self.store
                .load_reconciled_agents_from_snapshot_cached(
                    &mut self.cache,
                    &self.mux,
                    live,
                    boot_id.as_deref(),
                )?
                .0
        };
        let mut snapshot = build_snapshot(
            now(),
            &workspaces,
            live,
            &agents,
            self.host.as_deref(),
            self.boot.as_deref(),
        );
        self.git
            .retain(|ws_id, _| workspaces.iter().any(|ws| ws.identity.ws_id == *ws_id));
        for observed in &mut snapshot.workspaces {
            let ws_id = &observed.ws_id;
            let Some(workspace) = workspaces.iter().find(|ws| ws.identity.ws_id == *ws_id) else {
                continue;
            };
            let fresh = self
                .git
                .get(ws_id)
                .filter(|(read_at, _)| read_at.elapsed() < GIT_INTERVAL);
            observed.git = match fresh {
                Some((_, status)) => status.clone(),
                None => {
                    let status = git_status(workspace, &mut self.git_cache);
                    self.git
                        .insert(ws_id.clone(), (Instant::now(), status.clone()));
                    status
                }
            };
        }
        Ok(snapshot)
    }
}

/// The git status of `workspace`'s worktree, measured against its base.
fn git_status(workspace: &Workspace, cache: &mut GitStatusCache) -> Option<GitStatus> {
    let worktree = &workspace.worktree_path;
    if !worktree.join(".git").exists() {
        return None;
    }
    let base = super::base_ref(&workspace.identity.ws_id);
    let mut status = crate::git::get_git_status_cached(worktree, Some(&base), cache);
    let range = format!("{base}..HEAD");
    status.ahead = crate::remote::git::run(worktree, &["rev-list", "--count", &range])
        .ok()?
        .parse()
        .ok()?;
    status.behind = 0;
    status.has_upstream = true;
    status.base_branch = String::new();
    // Unchanged statuses must compare equal so the stream stays quiet.
    status.cached_at = None;
    Some(status)
}

/// Whether a tmux server listens on `socket`.
fn server_running(socket: &Path) -> Result<bool> {
    if !socket.exists() {
        return Ok(false);
    }
    let output = tmux(socket)
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .context("Failed to run tmux")?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("no server running") || stderr.contains("error connecting to") {
        return Ok(false);
    }
    anyhow::bail!("tmux list-sessions failed: {}", stderr.trim())
}

/// The node's hostname, which tmux gives panes nothing titled.
fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer outlives the call, which writes at most its length.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// `host` is the title tmux gives untitled panes, which says nothing about
/// the agent; `boot` is the current kernel boot.
fn build_snapshot(
    now: u64,
    workspaces: &[Workspace],
    live: &HashMap<String, LivePaneInfo>,
    agents: &[AgentPane],
    host: Option<&str>,
    boot: Option<&str>,
) -> Snapshot {
    let session_panes = session_panes(live);
    let workspaces = workspaces
        .iter()
        .filter_map(|ws| {
            let execution = ws.execution.as_ref()?;
            let panes = execution
                .panes
                .iter()
                .map(|pane| {
                    let session = session_name(&execution.ex_id, pane.index);
                    let pane_id = session_panes.get(session.as_str()).copied();
                    let agent = pane_id
                        .and_then(|id| agents.iter().find(|agent| agent.pane_id == id))
                        .map(|agent| ObservedAgent {
                            status: agent.status,
                            status_ts: agent.status_ts,
                            title: agent
                                .pane_title
                                .as_deref()
                                .and_then(clean_text)
                                .filter(|title| Some(title.as_str()) != host),
                            // With container isolation the pane runs the
                            // supervisor, which names no agent. An execution
                            // with no recorded kind has the agent as named.
                            agent_kind: agent
                                .agent_kind
                                .clone()
                                .or_else(|| execution.agent_kind.clone())
                                .or_else(|| ws.agent.clone()),
                            prompt: agent.prompt.as_deref().and_then(clean_text),
                        });
                    ObservedPane {
                        index: pane.index,
                        alive: pane_id.is_some(),
                        agent,
                    }
                })
                .collect::<Vec<ObservedPane>>();
            let ended = (execution.ended_by_reboot(boot) && !panes.iter().any(|pane| pane.alive))
                .then_some(EndReason::HostReboot);
            Some(ObservedWorkspace {
                ws_id: ws.identity.ws_id.clone(),
                ex_id: execution.ex_id.clone(),
                panes,
                git: None,
                sibling: sibling(ws),
                ended,
            })
        })
        .collect();
    Snapshot { now, workspaces }
}

/// The adoption details of a spawned workspace whose execution runs.
fn sibling(ws: &Workspace) -> Option<Sibling> {
    let origin = ws.origin.as_ref()?;
    let execution = ws.execution.as_ref()?;
    if execution.state != ExecutionState::Running {
        return None;
    }
    Some(Sibling {
        identity: ws.identity.clone(),
        worktree: ws.worktree_path.to_string_lossy().into_owned(),
        pin: execution.pin.clone()?,
        origin: origin.clone(),
    })
}

/// The pane of each node session. Each has one; the lowest id wins if a
/// user split it.
fn session_panes(live: &HashMap<String, LivePaneInfo>) -> HashMap<&str, &str> {
    let mut panes: HashMap<&str, &str> = HashMap::new();
    let mut ids: Vec<&String> = live.keys().collect();
    ids.sort_by_key(|id| pane_number(id));
    for id in ids {
        if let Some(session) = live[id].session.as_deref() {
            panes.entry(session).or_insert(id.as_str());
        }
    }
    panes
}

fn pane_number(id: &str) -> u64 {
    id.trim_start_matches('%').parse().unwrap_or(u64::MAX)
}

/// `watch` without `stream`.
pub fn snapshot(root: &NodeRoot) -> Result<Snapshot> {
    let mut observer = Observer::new(root)?;
    let live = observer.live_panes()?;
    observer.snapshot(&live, None)
}

/// `watch` with `stream`: NDJSON frames on stdout until it is closed. A
/// failing poll repeats its error instead of heartbeats, and the next good
/// one is sent in full.
pub fn stream(root: &NodeRoot) -> Result<()> {
    let mut observer = Observer::new(root)?;
    let mut out = std::io::stdout().lock();
    let mut last: Option<Snapshot> = None;
    let mut last_error: Option<String> = None;
    let mut last_sent: Option<Instant> = None;
    loop {
        let quiet = last_sent.is_none_or(|sent| sent.elapsed() >= HEARTBEAT_INTERVAL);
        let frame = match observer
            .live_panes()
            .and_then(|live| observer.snapshot(&live, None))
        {
            Ok(snapshot) => {
                last_error = None;
                let changed = last
                    .as_ref()
                    .is_none_or(|last| last.workspaces != snapshot.workspaces);
                if changed {
                    last = Some(snapshot.clone());
                    Some(WatchFrame::Snapshot(snapshot))
                } else {
                    quiet.then(|| WatchFrame::Heartbeat { now: now() })
                }
            }
            Err(err) => {
                last = None;
                let message = format!("{err:#}");
                let repeated = last_error.as_deref() == Some(message.as_str());
                last_error = Some(message.clone());
                (!repeated || quiet).then_some(WatchFrame::Error { message })
            }
        };
        if let Some(frame) = frame {
            let line = serde_json::to_string(&frame)?;
            if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                return Ok(());
            }
            last_sent = Some(Instant::now());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The pane a request targets.
struct Target {
    mux: TmuxBackend,
    /// Index of the configured pane.
    index: usize,
    /// Its tmux pane id.
    pane_id: String,
    /// The agent registered in it; none when the agent has not reported
    /// yet (see [`unregistered_agent_pane`]).
    agent: Option<ObservedAgent>,
    /// For an agent that has not reported yet, when its container runtime
    /// started (node clock).
    started: Option<u64>,
}

/// The pane of `ws_id` a request targets: `pane`, else the lowest pane with
/// an agent, else the agent pane of a container agent that has not
/// reported yet.
fn target(root: &NodeRoot, ws_id: &str, pane: Option<usize>) -> Result<Target> {
    validate_id(ws_id, 'w', "workspace id")?;
    let workspace = root.load(ws_id)?.ok_or_else(|| {
        refuse(
            code::NOT_FOUND,
            format!("workspace {ws_id} does not exist on the node"),
        )
    })?;
    let mut observer = Observer::new(root)?;
    let live = observer.live_panes()?;
    let snapshot = observer.snapshot(&live, Some(ws_id))?;
    let observed = snapshot.workspaces.into_iter().next().ok_or_else(|| {
        refuse(
            code::NOT_FOUND,
            format!(
                "workspace '{}' has no running execution",
                workspace.identity.handle
            ),
        )
    })?;
    let not_running = || {
        refuse(
            code::NOT_FOUND,
            format!(
                "no agent is running in workspace '{}'",
                workspace.identity.handle
            ),
        )
    };
    let session_panes = session_panes(&live);
    let pane_id = |index: usize| {
        session_panes
            .get(session_name(&observed.ex_id, index).as_str())
            .map(|id| id.to_string())
    };
    let chosen = match pane {
        Some(index) => observed.panes.iter().find(|p| p.index == index),
        None => observed.panes.iter().find(|p| p.agent.is_some()),
    };
    let target = match chosen {
        Some(chosen) => {
            let agent = chosen.agent.clone().ok_or_else(not_running)?;
            pane_id(chosen.index).map(|id| (chosen.index, id, Some(agent), None))
        }
        None if pane.is_none() => {
            unregistered_agent_pane(root, &workspace, &observer.socket, &live, pane_id)
                .map(|(index, id, started)| (index, id, None, Some(started)))
        }
        None => None,
    };
    let (index, pane_id, agent, started) = target.ok_or_else(not_running)?;
    Ok(Target {
        mux: observer.mux,
        index,
        pane_id,
        agent,
        started,
    })
}

/// The agent pane of a container workspace whose agent has not reported
/// yet (Codex registers only at its first turn), with when its container
/// runtime started: the configured agent pane, while the supervisor the
/// node typed into it runs the runtime, which reads the pane's input. Text
/// typed before the runtime runs would wait in the terminal for whoever
/// reads next, which is the shell if the supervisor fails. The supervisor
/// ends with its container, which hands the pane back to its shell, so a
/// pane whose agent ended, or one that runs anything else, is never chosen.
/// Without a container the agent is the pane's own program, which nothing
/// but its hook tells apart from whatever runs after it.
fn unregistered_agent_pane(
    root: &NodeRoot,
    workspace: &Workspace,
    socket: &Path,
    live: &HashMap<String, LivePaneInfo>,
    pane_id: impl Fn(usize) -> Option<String>,
) -> Option<(usize, String, u64)> {
    if workspace.isolation != RemoteIsolation::Container {
        return None;
    }
    let execution = workspace.execution.as_ref()?;
    if execution.state != ExecutionState::Running {
        return None;
    }
    let pin = execution.pin.as_deref()?;
    let (config, _) =
        crate::frozen_config::load(&root.config_path(&workspace.identity.ws_id)).ok()?;
    let index =
        super::execution::agent_pane(&config, workspace.agent.as_deref(), workspace.working_dir())?;
    let id = pane_id(index)?;
    let command = live.get(&id)?.current_command.as_deref()?;
    if !runs_pin(command, pin) {
        return None;
    }
    // A marker older than the session belongs to a pane of an earlier
    // server that had the same id.
    let started = super::execution::AgentMarker::read(root, &id)?;
    (started >= session_created(socket, &id)?).then_some((index, id, started))
}

/// When the session of `pane_id` was created (node clock).
fn session_created(socket: &Path, pane_id: &str) -> Option<u64> {
    let output = tmux(socket)
        .args(["display-message", "-p", "-t", pane_id, "#{session_created}"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Whether tmux's name for a pane's foreground program, `command`, is the
/// node binary `pin`. Platforms that report the kernel's process name cut it
/// to 15 bytes.
fn runs_pin(command: &str, pin: &str) -> bool {
    const COMM_LEN: usize = 15;
    command == pin || (command.len() == COMM_LEN && pin.starts_with(command))
}

/// Whether a prompt typed now could leave a status change in the same
/// second as `baseline`, the agent's state before it, which would make the
/// change indistinguishable from it in whole-second timestamps.
fn same_second(baseline: &ObservedAgent, now: u64) -> bool {
    matches!(
        baseline.status,
        Some(AgentStatus::Done | AgentStatus::Waiting)
    ) && baseline.status_ts.is_some_and(|ts| ts >= now)
}

/// Sleep until the next whole second of the clock.
fn sleep_to_next_second() {
    let into = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    std::thread::sleep(Duration::from_nanos(u64::from(1_000_000_000 - into)));
}

/// Type the text into the agent and report the agent's state just before,
/// which `wait` holds later states against. An agent that has not reported
/// yet first gets [`AGENT_STARTUP`] from its start: text typed before it
/// reads its terminal can land unsubmitted (Codex takes a fast burst ending
/// in Enter for a paste). A `done` or `waiting` state from the current
/// second is left behind first, so every status the prompt causes carries
/// a later timestamp; a state that stays in the current second is not
/// reported.
pub fn send(root: &NodeRoot, request: SendRequest) -> Result<SendResponse> {
    if request.text.len() > MAX_SEND {
        return Err(refuse(code::BAD_REQUEST, "text is too long"));
    }
    let mut target = target(root, &request.ws_id, request.pane)?;
    let ready = target
        .started
        .map(|started| started + AGENT_STARTUP.as_secs());
    if let Some(left) = ready
        .and_then(|ready| ready.checked_sub(now()))
        .filter(|&s| s > 0)
    {
        std::thread::sleep(Duration::from_secs(left));
        target = self::target(root, &request.ws_id, request.pane)?;
    }
    for _ in 0..3 {
        if !target
            .agent
            .as_ref()
            .is_some_and(|agent| same_second(agent, now()))
        {
            break;
        }
        sleep_to_next_second();
        target = self::target(root, &request.ws_id, request.pane)?;
    }
    let agent = target.agent.unwrap_or_default();
    let baseline = !same_second(&agent, now());
    let kind = send_kind(root, &request.ws_id, agent.agent_kind);
    target
        .mux
        .send_prompt_to_agent(&target.pane_id, &request.text, kind.as_deref())?;
    Ok(SendResponse {
        pane: Some(target.index),
        status: agent.status.filter(|_| baseline),
        status_ts: agent.status_ts.filter(|_| baseline),
    })
}

/// The kind of agent a prompt goes to: the observed one, else the
/// configured agent, resolved through the workspace's frozen config so an
/// alias names its type.
fn send_kind(root: &NodeRoot, ws_id: &str, observed: Option<String>) -> Option<String> {
    crate::frozen_config::load(&root.config_path(ws_id))
        .ok()
        .and_then(|(config, _)| {
            crate::multiplexer::agent::resolve_selected_agent(&config, observed.as_deref())
        })
        .map(|agent| agent.kind().to_string())
        .or(observed)
}

pub fn capture(root: &NodeRoot, request: CaptureRequest) -> Result<CaptureResponse> {
    let target = target(root, &request.ws_id, request.pane)?;
    let text = target
        .mux
        .capture_pane(&target.pane_id, request.lines.min(MAX_CAPTURE_LINES))
        .ok_or_else(|| refuse(code::FAILED, "failed to capture the pane"))?;
    Ok(CaptureResponse { text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemoteIsolation;
    use crate::remote::node::{Execution, ExecutionState, PaneProgress};
    use crate::remote::protocol::WorkspaceIdentity;

    fn workspace(ws_id: &str, ex_id: &str, panes: &[usize], agent: Option<&str>) -> Workspace {
        let dir = std::env::temp_dir();
        let root = NodeRoot { path: dir };
        let mut ws = Workspace::new(
            &root,
            WorkspaceIdentity {
                ws_id: ws_id.into(),
                project_id: "repo-0123456789ab".into(),
                handle: "feature".into(),
                branch: "feature".into(),
                base_sha: "a".repeat(40),
            },
            RemoteIsolation::Container,
        );
        ws.agent = agent.map(str::to_string);
        ws.execution = Some(Execution {
            ex_id: ex_id.into(),
            state: ExecutionState::Running,
            panes: panes.iter().map(|&index| PaneProgress { index }).collect(),
            boot_id: None,
            pin: None,
            continues: false,
            agent_kind: None,
        });
        ws
    }

    fn live(session: &str) -> LivePaneInfo {
        LivePaneInfo {
            pid: Some(1),
            current_command: Some("workmux".into()),
            working_dir: "/".into(),
            title: None,
            session: Some(session.into()),
            window: None,
            session_id: None,
            window_id: None,
            window_index: None,
        }
    }

    fn agent(pane_id: &str, status: AgentStatus) -> AgentPane {
        AgentPane {
            session: String::new(),
            window_name: String::new(),
            pane_id: pane_id.into(),
            window_id: String::new(),
            window_index: None,
            path: "/".into(),
            pane_title: Some("Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA".into()),
            status: Some(status),
            status_ts: Some(100),
            activity_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
            prompt: Some("fix\nit".into()),
            remote: None,
        }
    }

    #[test]
    fn snapshot_maps_sessions_to_agents() {
        let ws = workspace("w0123456789ab", "x0123456789ab", &[0, 1], Some("codex"));
        let live = HashMap::from([
            ("%3".to_string(), live("x0123456789ab-0")),
            ("%4".to_string(), live("x0123456789ab-1")),
        ]);
        let agents = [agent("%3", AgentStatus::Done)];
        let snapshot = build_snapshot(7, &[ws], &live, &agents, Some("node-1"), None);
        let panes = &snapshot.workspaces[0].panes;
        assert_eq!(snapshot.now, 7);
        assert!(panes[0].alive && panes[1].alive);
        let observed = panes[0].agent.as_ref().unwrap();
        assert_eq!(observed.status, Some(AgentStatus::Done));
        assert_eq!(observed.status_ts, Some(100));
        assert_eq!(observed.agent_kind.as_deref(), Some("codex"));
        assert_eq!(observed.title, None);
        assert_eq!(observed.prompt.as_deref(), Some("fixit"));
        assert!(panes[1].agent.is_none());
    }

    #[test]
    fn an_agent_alias_is_observed_as_its_execution_kind() {
        let mut ws = workspace("w0123456789ab", "x0123456789ab", &[0], Some("cxnd"));
        ws.execution.as_mut().unwrap().agent_kind = Some("codex".into());
        let live = HashMap::from([("%3".to_string(), live("x0123456789ab-0"))]);
        let agents = [agent("%3", AgentStatus::Working)];
        let snapshot = build_snapshot(7, &[ws], &live, &agents, None, None);
        let observed = snapshot.workspaces[0].panes[0].agent.as_ref().unwrap();
        assert_eq!(observed.agent_kind.as_deref(), Some("codex"));
    }

    #[test]
    fn send_resolves_the_configured_agent_through_the_frozen_config() {
        let dir = tempfile::tempdir().unwrap();
        let root = NodeRoot {
            path: dir.path().to_path_buf(),
        };
        let ws_id = "w0123456789ab";
        let config: crate::config::Config = serde_yaml::from_str(
            "agent: cxnd\nagents:\n  cxnd:\n    command: codex --no-daemon\n    type: codex\n",
        )
        .unwrap();
        let path = root.config_path(ws_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::frozen_config::write_document(
            &path,
            &crate::frozen_config::snapshot_document(&config, None).unwrap(),
        )
        .unwrap();
        assert_eq!(send_kind(&root, ws_id, None).as_deref(), Some("codex"));
        assert_eq!(
            send_kind(&root, ws_id, Some("cxnd".into())).as_deref(),
            Some("codex")
        );
        assert_eq!(
            send_kind(&root, ws_id, Some("claude".into())).as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn the_default_title_is_not_relayed() {
        let ws = workspace("w0123456789ab", "x0123456789ab", &[0], None);
        let live = HashMap::from([("%3".to_string(), live("x0123456789ab-0"))]);
        let mut untitled = agent("%3", AgentStatus::Working);
        untitled.pane_title = Some("node-1".into());
        let snapshot = build_snapshot(7, &[ws], &live, &[untitled], Some("node-1"), None);
        let observed = snapshot.workspaces[0].panes[0].agent.as_ref().unwrap();
        assert_eq!(observed.title, None);
    }

    #[test]
    fn snapshot_without_server_marks_panes_dead() {
        let ws = workspace("w0123456789ab", "x0123456789ab", &[0], None);
        let snapshot = build_snapshot(7, &[ws], &HashMap::new(), &[], None, None);
        let pane = &snapshot.workspaces[0].panes[0];
        assert!(!pane.alive);
        assert!(pane.agent.is_none());
    }

    #[test]
    fn a_reboot_ends_executions_until_one_starts() {
        let mut ws = workspace("w0123456789ab", "x0123456789ab", &[0], None);
        let execution = ws.execution.as_mut().unwrap();
        execution.boot_id = Some("boot-1".into());
        let ended = |ws: &Workspace, live: &HashMap<String, LivePaneInfo>| {
            build_snapshot(7, std::slice::from_ref(ws), live, &[], None, Some("boot-2")).workspaces
                [0]
            .ended
        };
        let none = HashMap::new();
        assert_eq!(ended(&ws, &none), Some(EndReason::HostReboot));
        let session = HashMap::from([("%3".to_string(), live("x0123456789ab-0"))]);
        assert_eq!(ended(&ws, &session), None);

        // The same boot: the session died for another reason.
        ws.execution.as_mut().unwrap().boot_id = Some("boot-2".into());
        assert_eq!(ended(&ws, &none), None);

        // A resume that failed before its agents started.
        let execution = ws.execution.as_mut().unwrap();
        execution.continues = true;
        execution.state = ExecutionState::Starting;
        assert_eq!(ended(&ws, &none), Some(EndReason::HostReboot));
        ws.execution.as_mut().unwrap().state = ExecutionState::Running;
        assert_eq!(ended(&ws, &none), None);
    }

    #[test]
    fn send_responses_parse_across_versions() {
        // A node from before the turn baseline.
        let old: SendResponse = serde_json::from_str("{}").unwrap();
        assert_eq!((old.pane, old.status, old.status_ts), (None, None, None));
        let new = SendResponse {
            pane: Some(1),
            status: Some(AgentStatus::Done),
            status_ts: Some(100),
        };
        let json = serde_json::to_string(&new).unwrap();
        assert_eq!(json, r#"{"pane":1,"status":"done","status_ts":100}"#);
        #[derive(Deserialize)]
        struct Before {}
        serde_json::from_str::<Before>(&json).unwrap();
    }

    #[test]
    fn only_done_or_waiting_states_of_this_second_wait_for_the_next() {
        let state = |status, ts| ObservedAgent {
            status,
            status_ts: ts,
            ..Default::default()
        };
        assert!(same_second(&state(Some(AgentStatus::Done), Some(7)), 7));
        assert!(same_second(&state(Some(AgentStatus::Waiting), Some(8)), 7));
        assert!(!same_second(&state(Some(AgentStatus::Done), Some(6)), 7));
        assert!(!same_second(&state(Some(AgentStatus::Working), Some(7)), 7));
        assert!(!same_second(&state(None, None), 7));
    }

    #[test]
    fn a_pane_runs_the_pin_by_full_or_kernel_name() {
        let pin = "workmux-dev-0123456789ab";
        assert!(runs_pin(pin, pin));
        assert!(runs_pin("workmux-dev-012", pin));
        assert!(!runs_pin("workmux", pin));
        assert!(!runs_pin("bash", pin));
        assert!(!runs_pin("workmux-dev-999", pin));
    }

    #[test]
    fn missing_socket_means_no_server() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!server_running(&dir.path().join("tmux.sock")).unwrap());
    }

    #[test]
    fn titles_are_cleaned() {
        assert_eq!(clean_text("Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA"), None);
        assert_eq!(clean_text("  \u{1b}]0;x\u{7}  "), Some("]0;x".to_string()));
        assert_eq!(
            clean_text("Fix the parser").as_deref(),
            Some("Fix the parser")
        );
        assert_eq!(
            clean_text("Go: fast; really").as_deref(),
            Some("Go: fast; really")
        );
        assert_eq!(clean_text(&"x".repeat(1000)).unwrap().len(), MAX_TEXT);
        assert_eq!(clean_text(" \t "), None);
    }
}
