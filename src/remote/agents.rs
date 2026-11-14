//! Remote agents as local views and commands see them: one row per agent
//! the node reports, whether or not a local window relays it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use super::RemoteSpec;
use super::cache::{self, SentTurn, WorkspaceRecord};
use super::client::Node;
use super::node::observe::{
    CaptureRequest, CaptureResponse, ObservedAgent, SendRequest, SendResponse,
};
use super::observe::{self, Observation};
use super::protocol::{self, DiffMode, DiffRequest, DiffResponse};
use crate::agent_error::{AgentError, AgentErrorKind};
use crate::config::Config;
use crate::multiplexer::{AgentPane, EndReason, RemoteAgent, RemoteRef};
use crate::state::sent::TurnFence;

/// The remote workspace `target` names: a handle or branch of this
/// repository (local worktrees win), or `project:handle`.
pub fn find_target(target: &str) -> Option<WorkspaceRecord> {
    if let Some((project, handle)) = target.split_once(':')
        && !project.is_empty()
        && !handle.is_empty()
    {
        let mut matching = cache::all().into_iter().filter(|record| {
            record.handle == handle
                && record
                    .project_root
                    .file_name()
                    .is_some_and(|name| name == project)
        });
        let record = matching.next()?;
        return matching.next().is_none().then_some(record);
    }
    cache::find_here(target)
}

/// Placeholder pane id of a remote agent no local pane relays; no
/// multiplexer resolves it.
pub fn detached_pane_id(ws_id: &str, pane: usize) -> String {
    format!("%remote-{ws_id}-{pane}")
}

/// The workspace and configured pane of [`detached_pane_id`] `id`.
fn parse_detached_pane_id(id: &str) -> Option<(&str, usize)> {
    let (ws_id, pane) = id.strip_prefix("%remote-")?.rsplit_once('-')?;
    Some((ws_id, pane.parse().ok()?))
}

/// The remote workspace and configured pane that pane ID `pane_id` names:
/// a local pane of `attached` that relays it, or a [`detached_pane_id`].
pub fn find_pane(pane_id: &str, attached: &[AgentPane]) -> Option<(WorkspaceRecord, usize)> {
    let relayed = attached
        .iter()
        .find(|row| row.pane_id == pane_id)
        .and_then(|row| row.remote.as_ref());
    let (remote, ws_id, pane) = match relayed {
        Some(relay) => (
            Some(relay.at.remote.as_str()),
            relay.at.ws_id.as_str(),
            relay.at.pane,
        ),
        None => {
            let (ws_id, pane) = parse_detached_pane_id(pane_id)?;
            (None, ws_id, pane)
        }
    };
    let record = cache::all().into_iter().find(|record| {
        record.ws_id == ws_id
            && remote.is_none_or(|remote| record.remote == remote)
            && !record.cleanup_pending()
    })?;
    Some((record, pane))
}

/// Rows for the agents of `records`. `attached` are the reconciled rows of
/// local attach panes; they stand in for the agents they relay. An agent
/// the node no longer reports is not listed, attached or not; a workspace
/// that was never observed shows its attach panes, or one row without a
/// status.
pub fn rows(
    records: &[WorkspaceRecord],
    attached: &[AgentPane],
    observations: &HashMap<String, Observation>,
    hosts: &HashMap<String, String>,
    now: u64,
) -> Vec<AgentPane> {
    let mut rows = Vec::new();
    for record in records {
        let Some(execution) = &record.execution else {
            continue;
        };
        let observation = observations.get(&record.remote);
        let observed = observation.and_then(|o| o.execution(&record.ws_id, &execution.ex_id));
        let git = observed.and_then(|o| o.git.clone()).map(|mut status| {
            let base = record.base_branch();
            status.behind = behind(record, &base);
            status.base_branch = base;
            status
        });
        let remote = |pane: usize, attached: bool| RemoteAgent {
            at: RemoteRef {
                remote: record.remote.clone(),
                ws_id: record.ws_id.clone(),
                ex_id: execution.ex_id.clone(),
                pane,
            },
            handle: record.handle.clone(),
            project_root: record.project_root.clone(),
            host: hosts.get(&record.remote).cloned().unwrap_or_default(),
            attached,
            connected: observation.is_some_and(|o| o.is_connected(now)),
            last_seen: observation.and_then(|o| o.last_seen),
            git: git.clone(),
            ended: observed.and_then(|o| o.ended),
        };
        let local = |pane: usize| {
            attached.iter().find(|row| {
                row.remote.as_ref().is_some_and(|r| {
                    r.at.remote == record.remote
                        && r.at.ws_id == record.ws_id
                        && r.at.ex_id == execution.ex_id
                        && r.at.pane == pane
                })
            })
        };
        let Some(observed) = observed else {
            let mut panes: Vec<&AgentPane> = execution
                .panes
                .iter()
                .filter_map(|&pane| local(pane))
                .collect();
            if panes.is_empty() {
                rows.push(detached_row(
                    record,
                    None,
                    None,
                    remote(execution.panes.first().copied().unwrap_or(0), false),
                ));
                continue;
            }
            panes.sort_by_key(|row| row.remote.as_ref().map(|r| r.at.pane));
            for row in panes {
                let pane = row.remote.as_ref().map_or(0, |r| r.at.pane);
                rows.push(AgentPane {
                    remote: Some(remote(pane, true)),
                    ..row.clone()
                });
            }
            continue;
        };
        // An execution that ended has no agents left; one row says so.
        if observed.ended.is_some() {
            let pane = execution.panes.first().copied().unwrap_or(0);
            rows.push(match local(pane) {
                Some(row) => AgentPane {
                    remote: Some(remote(pane, true)),
                    status: None,
                    ..row.clone()
                },
                None => detached_row(record, None, observation, remote(pane, false)),
            });
            continue;
        }
        for pane in &observed.panes {
            let Some(agent) = &pane.agent else {
                continue;
            };
            let row = match local(pane.index) {
                // The relay pane carries the remote pane's title, which is the
                // node's hostname until the agent sets one; the observation
                // has the agent's title without it.
                Some(row) => AgentPane {
                    pane_title: agent.title.clone(),
                    agent_kind: agent.agent_kind.clone().or_else(|| row.agent_kind.clone()),
                    remote: Some(remote(pane.index, true)),
                    ..row.clone()
                },
                None => detached_row(record, Some(agent), observation, remote(pane.index, false)),
            };
            rows.push(row);
        }
    }
    rows
}

/// How long a [`behind`] count is reused.
const BEHIND_TTL: Duration = Duration::from_secs(10);

/// Commits `base` gained since `record`'s workspace was created from it.
/// Views ask on every refresh, so counts are reused for a while.
fn behind(record: &WorkspaceRecord, base: &str) -> usize {
    type Key = (PathBuf, String, String);
    static COUNTS: OnceLock<Mutex<HashMap<Key, (Instant, usize)>>> = OnceLock::new();
    let key = (
        record.project_root.clone(),
        record.base_sha.clone(),
        base.to_string(),
    );
    let mut counts = COUNTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    if let Some((at, count)) = counts.get(&key)
        && at.elapsed() < BEHIND_TTL
    {
        return *count;
    }
    let range = format!("{}..{base}", record.base_sha);
    let count = super::git::run(&record.project_root, &["rev-list", "--count", &range])
        .ok()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    counts.insert(key, (Instant::now(), count));
    count
}

fn detached_row(
    record: &WorkspaceRecord,
    agent: Option<&ObservedAgent>,
    observation: Option<&Observation>,
    remote: RemoteAgent,
) -> AgentPane {
    let local_ts = |ts: u64| observation.map_or(ts, |o| o.local_ts(ts));
    AgentPane {
        session: String::new(),
        window_name: record.window_name.clone(),
        pane_id: detached_pane_id(&record.ws_id, remote.at.pane),
        window_id: String::new(),
        window_index: None,
        path: record.local_path.clone(),
        pane_title: agent.and_then(|agent| agent.title.clone()),
        status: agent.and_then(|agent| agent.status),
        status_ts: agent.and_then(|agent| agent.status_ts).map(local_ts),
        activity_ts: None,
        updated_ts: None,
        window_cmd: None,
        agent_command: None,
        agent_kind: agent.and_then(|agent| agent.agent_kind.clone()),
        prompt: agent.and_then(|agent| agent.prompt.clone()),
        remote: Some(remote),
    }
}

/// `agents`, reconciled with the panes that relay remote agents, as views
/// list them: local agents followed by every remote workspace's agents,
/// attach panes standing in for the agents they relay. Starts observing the
/// remotes of those workspaces.
pub fn for_views(config: &Config, agents: Vec<AgentPane>, now: u64) -> Vec<AgentPane> {
    let (mut local, attached) = crate::state::store::split_remote(agents);
    let mut records = cache::all();
    records.retain(|record| !record.cleanup_pending());
    if records.is_empty() {
        return local;
    }
    observe::spawn(config, &remotes_in_use(&records));
    let observations = observations(config, &records, false);
    local.extend(rows(
        &records,
        &attached,
        &observations,
        &hosts(config),
        now,
    ));
    local
}

/// Whether `agent` is a remote agent no local pane relays.
pub fn is_detached(agent: &AgentPane) -> bool {
    agent.remote.as_ref().is_some_and(|remote| !remote.attached)
}

/// The worktree label of a remote agent: `handle@remote`.
pub fn label(worktree: &str, remote: &str) -> String {
    format!("{worktree}@{remote}")
}

/// What views say about a remote they cannot reach or an execution that
/// ended under its agent; `None` while it runs and the remote answers.
pub fn connectivity(remote: &RemoteAgent, now: u64) -> Option<String> {
    if remote.connected {
        return remote.ended.map(|reason| match reason {
            EndReason::HostReboot => "exited · host rebooted · `workmux open` resumes".to_string(),
        });
    }
    Some(match remote.last_seen {
        Some(seen) => format!(
            "disconnected · seen {} ago",
            crate::util::format_elapsed_secs(now.saturating_sub(seen))
        ),
        None => "disconnected".to_string(),
    })
}

/// The short form of [`connectivity`] that rows show ahead of the agent's
/// last title, which they keep.
pub fn row_title(remote: &RemoteAgent, now: u64, title: Option<&str>) -> Option<String> {
    let state = if remote.connected {
        remote.ended?.label().to_string()
    } else {
        match remote.last_seen {
            Some(seen) => format!(
                "disconnected {}",
                crate::util::format_elapsed_secs(now.saturating_sub(seen))
            ),
            None => "disconnected".to_string(),
        }
    };
    Some(match title.filter(|title| !title.is_empty()) {
        Some(title) => format!("{state} · {title}"),
        None => state,
    })
}

/// Open a window relaying the workspace of `remote` with `workmux open`,
/// which attaches to its running agents.
pub fn open_window(remote: &RemoteAgent) -> Result<()> {
    let exe = std::env::current_exe()?;
    let output = std::process::Command::new(exe)
        .args(["open", &remote.handle])
        .current_dir(&remote.project_root)
        .stdin(std::process::Stdio::null())
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(anyhow!(
        "{}",
        stderr
            .lines()
            .rfind(|line| !line.trim().is_empty())
            .unwrap_or("workmux open failed")
    ))
}

/// ssh destinations of the configured remotes, for display.
pub fn hosts(config: &Config) -> HashMap<String, String> {
    config
        .remotes
        .iter()
        .map(|(name, remote)| (name.clone(), remote.host.clone()))
        .collect()
}

/// The remotes of `records` that run an execution, which are the ones
/// worth observing.
fn remotes_in_use(records: &[WorkspaceRecord]) -> Vec<String> {
    let mut names: Vec<String> = records
        .iter()
        .filter(|record| record.execution.is_some())
        .map(|record| record.remote.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The observations of the remotes of `records`: as last written, or with
/// `fetch` made current by asking nodes whose observation is stale.
pub fn observations(
    config: &Config,
    records: &[WorkspaceRecord],
    fetch: bool,
) -> HashMap<String, Observation> {
    remotes_in_use(records)
        .into_iter()
        .filter_map(|name| {
            let observation = if fetch {
                Some(observe::current(config, &name))
            } else {
                observe::load(&name)
            };
            observation.map(|o| (name, o))
        })
        .collect()
}

/// Type `text` into the workspace's agent in configured pane `pane`, else
/// its only agent, through the node, and remember the agent's state before
/// it so `wait` holds out for the turn it starts. With `require_status`,
/// an agent that has reported no status is refused. Returns that turn,
/// unless the node is too old to report it.
pub fn send(
    config: &Config,
    record: &WorkspaceRecord,
    pane: Option<usize>,
    text: &str,
    require_status: bool,
) -> Result<Option<SentTurn>> {
    let Some(execution) = &record.execution else {
        return Err(no_execution(record));
    };
    let spec = RemoteSpec::resolve(config, &record.remote)?;
    let node = Node::connect_reader(&spec, protocol::SEND)?;
    let response: SendResponse = node
        .call(
            protocol::SEND,
            &SendRequest {
                ws_id: record.ws_id.clone(),
                pane,
                text: text.to_string(),
                require_status,
            },
            false,
        )
        .map_err(|err| agent_error(config, record, err))?;
    // Nodes from before the baseline report nothing to tell the turn by.
    let turn = response.pane.map(|pane| SentTurn {
        ex_id: execution.ex_id.clone(),
        pane,
        fence: TurnFence {
            status: response.status,
            status_ts: response.status_ts,
            sent_at: cache::now(),
        },
    });
    if let Err(err) = cache::set_sent_turn(&record.remote, &record.ws_id, turn.as_ref()) {
        tracing::warn!(handle = %record.handle, "failed to record the sent turn: {err:#}");
    }
    Ok(turn)
}

/// The agent a [`SentTurn`] went to, as the latest observation shows it.
pub enum SentAgent {
    /// The remote is not observed right now.
    Unobserved,
    /// The agent, its execution or its workspace is gone.
    Gone,
    Present(ObservedAgent),
}

/// Where the agent `turn` went to in `record`'s workspace stands.
pub fn sent_agent(record: &WorkspaceRecord, turn: &SentTurn) -> SentAgent {
    let current = cache::all()
        .into_iter()
        .find(|r| r.remote == record.remote && r.ws_id == record.ws_id && !r.cleanup_pending());
    if current
        .and_then(|current| current.execution)
        .is_none_or(|execution| execution.ex_id != turn.ex_id)
    {
        return SentAgent::Gone;
    }
    let Some(observation) = observe::load_connected(&record.remote) else {
        return SentAgent::Unobserved;
    };
    let Some(workspace) = observation.execution(&record.ws_id, &turn.ex_id) else {
        return SentAgent::Unobserved;
    };
    if workspace.ended.is_some() {
        return SentAgent::Gone;
    }
    match workspace
        .panes
        .iter()
        .find(|pane| pane.index == turn.pane)
        .and_then(|pane| pane.agent.clone())
    {
        Some(agent) => SentAgent::Present(agent),
        None => SentAgent::Gone,
    }
}

/// The last `lines` of pane `pane` of the workspace, or of its only agent
/// pane, from the node.
pub fn capture(
    config: &Config,
    record: &WorkspaceRecord,
    pane: Option<usize>,
    lines: u16,
) -> Result<String> {
    if record.execution.is_none() {
        return Err(no_execution(record));
    }
    capture_from(config, &record.remote, &record.ws_id, pane, lines)
        .map_err(|err| agent_error(config, record, err))
}

/// [`capture`] of workspace `ws_id` on `remote`.
pub fn capture_from(
    config: &Config,
    remote: &str,
    ws_id: &str,
    pane: Option<usize>,
    lines: u16,
) -> Result<String> {
    let spec = RemoteSpec::resolve(config, remote)?;
    let node = Node::connect_reader(&spec, protocol::CAPTURE)?;
    let response: CaptureResponse = node.call(
        protocol::CAPTURE,
        &CaptureRequest {
            ws_id: ws_id.to_string(),
            pane,
            lines,
        },
        false,
    )?;
    Ok(response.text)
}

/// The [`AgentError`] for a node's refusal to drive an agent of `record`,
/// else `err` itself.
fn agent_error(config: &Config, record: &WorkspaceRecord, err: anyhow::Error) -> anyhow::Error {
    let kind = match super::client::error_code(&err) {
        Some(protocol::code::AGENT_WAITING) => AgentErrorKind::Waiting,
        Some(protocol::code::NOT_FOUND) => AgentErrorKind::NotFound,
        Some(protocol::code::AGENT_STATUS_UNKNOWN) => AgentErrorKind::StatusUnknown,
        Some(protocol::code::AMBIGUOUS_TARGET) => {
            let pane_ids = pane_ids(config, record);
            return if pane_ids.len() > 1 {
                crate::workflow::ambiguous(&record.handle, pane_ids.iter().map(String::as_str))
            } else {
                AgentError::error(AgentErrorKind::AmbiguousTarget, err.to_string())
            };
        }
        _ => return err,
    };
    AgentError::error(kind, err.to_string())
}

/// The pane IDs views give the agents of `record`'s workspace, as its node
/// reports them now.
fn pane_ids(config: &Config, record: &WorkspaceRecord) -> Vec<String> {
    let mux = crate::multiplexer::create_backend(crate::multiplexer::detect_backend());
    let attached = crate::state::StateStore::new()
        .and_then(|store| store.load_all_reconciled_agents(mux.as_ref()))
        .map(|agents| crate::state::store::split_remote(agents).1)
        .unwrap_or_default();
    let observations = HashMap::from([(
        record.remote.clone(),
        observe::current(config, &record.remote),
    )]);
    rows(
        std::slice::from_ref(record),
        &attached,
        &observations,
        &HashMap::new(),
        cache::now(),
    )
    .into_iter()
    .map(|row| row.pane_id)
    .collect()
}

/// A workspace's diff as its node last returned it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedDiff {
    pub text: String,
    pub truncated: bool,
    /// When it was fetched (laptop clock).
    pub fetched_at: u64,
}

/// The diff of workspace `ws_id` last fetched from `remote`.
pub fn cached_diff(remote: &str, ws_id: &str, mode: DiffMode) -> Option<CachedDiff> {
    let bytes = std::fs::read(cache::diff_path(remote, ws_id, mode).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Fetch the diff of workspace `ws_id` from `remote` and keep it for
/// [`cached_diff`].
pub fn fetch_diff(
    config: &Config,
    remote: &str,
    ws_id: &str,
    mode: DiffMode,
) -> Result<CachedDiff> {
    let spec = RemoteSpec::resolve(config, remote)?;
    let node = Node::connect_reader(&spec, protocol::DIFF)?;
    let response: DiffResponse = node.call(
        protocol::DIFF,
        &DiffRequest {
            ws_id: ws_id.to_string(),
            mode,
        },
        false,
    )?;
    let diff = CachedDiff {
        text: response.text,
        truncated: response.truncated,
        fetched_at: cache::now(),
    };
    let path = cache::diff_path(remote, ws_id, mode)?;
    let saved = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .map_err(anyhow::Error::from)
        .and_then(|()| Ok(serde_json::to_vec(&diff)?))
        .and_then(|bytes| crate::util::write_atomic(&path, &bytes));
    if let Err(err) = saved {
        tracing::debug!(path = %path.display(), "failed to cache a remote diff: {err:#}");
    }
    Ok(diff)
}

fn no_execution(record: &WorkspaceRecord) -> anyhow::Error {
    AgentError::error(
        AgentErrorKind::NotFound,
        format!(
            "remote workspace '{}' has no running agent; `workmux open {}` starts it",
            record.handle, record.handle
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::AgentStatus;
    use crate::remote::cache::ExecutionRecord;
    use crate::remote::node::observe::{ObservedPane, ObservedWorkspace};
    use std::path::PathBuf;

    fn record() -> WorkspaceRecord {
        let mut record = WorkspaceRecord::new(
            "w1".into(),
            "box",
            "p".into(),
            PathBuf::from("/src/app"),
            "feat",
            "feat",
            "0".repeat(40),
            None,
            "wm-feat".into(),
            PathBuf::from("/src/app__worktrees/feat"),
        );
        record.execution = Some(ExecutionRecord {
            ex_id: "x1".into(),
            panes: vec![0, 1],
            node_bin: "workmux-1".into(),
        });
        record
    }

    fn observation(connected_at: u64, panes: Vec<ObservedPane>) -> Observation {
        Observation {
            connected: true,
            last_seen: Some(connected_at),
            skew: 100,
            workspaces: vec![ObservedWorkspace {
                ws_id: "w1".into(),
                ex_id: "x1".into(),
                panes,
                git: None,
                sibling: None,
                operation: None,
                ended: None,
            }],
        }
    }

    fn agent_pane(index: usize, status: AgentStatus) -> ObservedPane {
        ObservedPane {
            index,
            alive: true,
            agent: Some(ObservedAgent {
                status: Some(status),
                status_ts: Some(10),
                title: Some("Fixing".into()),
                agent_kind: Some("claude".into()),
                prompt: None,
            }),
        }
    }

    fn attach_row(pane: usize) -> AgentPane {
        let mut remote = RemoteAgent::new(RemoteRef {
            remote: "box".into(),
            ws_id: "w1".into(),
            ex_id: "x1".into(),
            pane,
        });
        remote.handle = String::new();
        AgentPane {
            session: "main".into(),
            window_name: "wm-feat".into(),
            pane_id: "%7".into(),
            window_id: "@3".into(),
            window_index: Some(2),
            path: "/src/app__worktrees/feat".into(),
            pane_title: Some("Gi=31;AAAA".into()),
            status: Some(AgentStatus::Done),
            status_ts: Some(110),
            activity_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
            prompt: None,
            remote: Some(remote),
        }
    }

    fn hosts() -> HashMap<String, String> {
        HashMap::from([("box".to_string(), "me@box".to_string())])
    }

    #[test]
    fn detached_agents_get_rows_from_the_observation() {
        let observations = HashMap::from([(
            "box".to_string(),
            observation(
                1000,
                vec![
                    agent_pane(0, AgentStatus::Working),
                    ObservedPane {
                        index: 1,
                        alive: true,
                        agent: None,
                    },
                ],
            ),
        )]);
        let rows = rows(&[record()], &[], &observations, &hosts(), 1005);
        let [row] = rows.as_slice() else {
            panic!("{rows:?}")
        };
        assert_eq!(row.pane_id, "%remote-w1-0");
        assert_eq!(row.path, PathBuf::from("/src/app__worktrees/feat"));
        assert_eq!(row.status, Some(AgentStatus::Working));
        assert_eq!(row.status_ts, Some(110));
        let remote = row.remote.as_ref().unwrap();
        assert!(!remote.attached && remote.connected);
        assert_eq!(remote.host, "me@box");
        assert_eq!(remote.handle, "feat");

        // Stale: still listed, disconnected since the last answer.
        let rows = super::rows(&[record()], &[], &observations, &hosts(), 1100);
        let remote = rows[0].remote.as_ref().unwrap();
        assert!(!remote.connected);
        assert_eq!(remote.last_seen, Some(1000));
    }

    #[test]
    fn attach_panes_stand_in_for_their_agents() {
        let observations = HashMap::from([(
            "box".to_string(),
            observation(1000, vec![agent_pane(0, AgentStatus::Done)]),
        )]);
        let rows = rows(
            &[record()],
            &[attach_row(0), attach_row(1)],
            &observations,
            &hosts(),
            1001,
        );
        let [row] = rows.as_slice() else {
            panic!("{rows:?}")
        };
        assert_eq!(row.pane_id, "%7");
        assert_eq!(row.pane_title.as_deref(), Some("Fixing"));
        assert_eq!(row.agent_kind.as_deref(), Some("claude"));
        assert!(row.remote.as_ref().unwrap().attached);

        // An untitled agent's relay pane carries the node's hostname.
        let mut untitled = agent_pane(0, AgentStatus::Done);
        untitled.agent.as_mut().unwrap().title = None;
        let observations = HashMap::from([("box".to_string(), observation(1000, vec![untitled]))]);
        let mut relay = attach_row(0);
        relay.pane_title = Some("node-1".into());
        let rows = super::rows(&[record()], &[relay], &observations, &hosts(), 1001);
        assert_eq!(rows[0].pane_title, None);
    }

    #[test]
    fn unobserved_workspaces_still_show() {
        let rows_without = rows(&[record()], &[], &HashMap::new(), &hosts(), 5);
        let [row] = rows_without.as_slice() else {
            panic!("{rows_without:?}")
        };
        assert_eq!(row.status, None);
        assert!(!row.remote.as_ref().unwrap().connected);

        let with_pane = rows(&[record()], &[attach_row(1)], &HashMap::new(), &hosts(), 5);
        assert_eq!(with_pane.len(), 1);
        assert_eq!(with_pane[0].pane_id, "%7");

        let mut ended = record();
        ended.execution = None;
        assert!(rows(&[ended], &[], &HashMap::new(), &hosts(), 5).is_empty());
    }

    #[test]
    fn detached_pane_ids_round_trip() {
        let id = detached_pane_id("w0123456789ab", 2);
        assert_eq!(parse_detached_pane_id(&id), Some(("w0123456789ab", 2)));
        for other in ["%7", "%remote-w1", "%remote-w1-x", "feature"] {
            assert_eq!(parse_detached_pane_id(other), None, "{other}");
        }
    }

    #[test]
    fn rows_keep_the_title_beside_connectivity() {
        let rows = rows(&[record()], &[], &HashMap::new(), &hosts(), 5);
        let mut remote = rows[0].remote.clone().unwrap();
        remote.last_seen = Some(1000);
        assert_eq!(
            row_title(&remote, 1120, Some("Fixing")).as_deref(),
            Some("disconnected 2m · Fixing")
        );
        remote.last_seen = None;
        assert_eq!(
            row_title(&remote, 5, Some("")).as_deref(),
            Some("disconnected")
        );
        remote.connected = true;
        assert_eq!(row_title(&remote, 5, Some("Fixing")), None);
        remote.ended = Some(EndReason::HostReboot);
        assert_eq!(
            row_title(&remote, 5, None).as_deref(),
            Some("exited (host rebooted)")
        );
    }
}
