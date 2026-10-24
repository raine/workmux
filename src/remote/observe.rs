//! What the laptop knows about the agents on each remote.
//!
//! One process per remote at a time runs `node watch` as a stream over the
//! shared connection and keeps `remote/<name>/observed.v<N>.json` current;
//! it holds `remote/<name>/observer.v<N>.lock` while doing so. Relays, views
//! and commands read the file, so the remote carries one stream however
//! many panes and views look at it.
//!
//! `N` is [`GENERATION`], the format of the file. Dashboards, sidebars and
//! relays run for days, so an older build may still be observing when a
//! newer one starts; each generation elects its own observer and keeps its
//! own file, and no build reads what another generation writes as current.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::RemoteSpec;
use super::cache::{WorkspaceRecord, now};
use super::client::Node;
use super::node::observe::{ObservedPane, ObservedWorkspace, Snapshot, WatchFrame, WatchRequest};
use super::protocol::{self, ErrorEnvelope};
use crate::config::Config;

/// A remote that sent nothing for this long is disconnected.
pub const STALE_AFTER: Duration = Duration::from_secs(15);
/// How often a process waiting for another's observer tries to take over.
const TAKEOVER_INTERVAL: Duration = Duration::from_secs(3);
/// Format of the observation file and scope of the observer election. Bump
/// it whenever [`Observation`] or anything it contains changes shape, so
/// builds that disagree never share a file (`format_is_pinned` checks).
const GENERATION: u32 = 2;

/// Contents of `observed.v<N>.json`. Node timestamps in `workspaces` are raw;
/// [`Observation::local_ts`] moves them to the laptop clock.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Whether the observer's stream was live when it last wrote.
    #[serde(default)]
    pub connected: bool,
    /// When the node last answered (laptop clock).
    #[serde(default)]
    pub last_seen: Option<u64>,
    /// Laptop clock minus node clock, in seconds.
    #[serde(default)]
    pub skew: i64,
    /// The last snapshot the node sent; kept while disconnected.
    #[serde(default)]
    pub workspaces: Vec<ObservedWorkspace>,
}

impl Observation {
    /// Connected iff the stream was live and answered recently, so a dead
    /// observer reads as disconnected.
    pub fn is_connected(&self, now: u64) -> bool {
        self.connected
            && self
                .last_seen
                .is_some_and(|seen| now.saturating_sub(seen) <= STALE_AFTER.as_secs())
    }

    /// Execution `ex_id` of `ws_id`; an observation of another execution of
    /// the workspace says nothing about this one.
    pub fn execution(&self, ws_id: &str, ex_id: &str) -> Option<&ObservedWorkspace> {
        self.workspaces
            .iter()
            .find(|ws| ws.ws_id == ws_id && ws.ex_id == ex_id)
    }

    /// The current execution of `record`, as observed.
    pub fn of(&self, record: &WorkspaceRecord) -> Option<&ObservedWorkspace> {
        self.execution(&record.ws_id, &record.execution.as_ref()?.ex_id)
    }

    /// Pane `index` of execution `ex_id` of `ws_id`.
    pub fn pane(&self, ws_id: &str, ex_id: &str, index: usize) -> Option<&ObservedPane> {
        self.execution(ws_id, ex_id)?
            .panes
            .iter()
            .find(|pane| pane.index == index)
    }

    /// A node timestamp on the laptop clock.
    pub fn local_ts(&self, node_ts: u64) -> u64 {
        node_ts.saturating_add_signed(self.skew)
    }

    /// Fold one frame of the stream in, at laptop time `now`.
    fn apply(&mut self, frame: WatchFrame, now: u64) {
        match frame {
            WatchFrame::Snapshot(Snapshot {
                now: node_now,
                workspaces,
            }) => {
                self.workspaces = workspaces;
                self.seen(now, node_now);
            }
            WatchFrame::Heartbeat { now: node_now } => self.seen(now, node_now),
            WatchFrame::Error { .. } => self.connected = false,
        }
    }

    fn seen(&mut self, now: u64, node_now: u64) {
        self.connected = true;
        self.last_seen = Some(now);
        self.skew = now as i64 - node_now as i64;
    }
}

fn path(name: &str) -> Result<PathBuf> {
    Ok(super::cache::remote_dir(name)?.join(format!("observed.v{GENERATION}.json")))
}

/// The file the first generation wrote.
fn legacy_path(name: &str) -> Result<PathBuf> {
    Ok(super::cache::remote_dir(name)?.join("observed.json"))
}

/// The last observation of remote `name`, if any was ever written. Before
/// this generation first wrote one, the first generation's last snapshot
/// stands in, as of when it was seen and never as connected.
pub fn load(name: &str) -> Option<Observation> {
    let parse = |bytes: Vec<u8>| {
        serde_json::from_slice::<Observation>(&bytes)
            .map_err(|err| tracing::warn!(remote = name, "unreadable observation: {err}"))
            .ok()
    };
    match fs::read(path(name).ok()?) {
        Ok(bytes) => parse(bytes),
        Err(_) => {
            let mut legacy = parse(fs::read(legacy_path(name).ok()?).ok()?)?;
            legacy.connected = false;
            Some(legacy)
        }
    }
}

/// The observation of remote `name` while it is connected.
pub fn load_connected(name: &str) -> Option<Observation> {
    load(name).filter(|observation| observation.is_connected(now()))
}

fn save(name: &str, observation: &Observation) -> Result<()> {
    let path = path(name)?;
    let dir = path.parent().expect("observation path has a parent");
    fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    crate::util::write_atomic(&path, &serde_json::to_vec(observation)?)
}

/// Keep the observations of `names` current from background threads of
/// this process, for as long as it runs. Remotes this process already
/// observes, and names not in `config`, are skipped.
pub fn spawn(config: &Config, names: &[String]) {
    static STARTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut started = STARTED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    for name in names {
        if started.contains(name) {
            continue;
        }
        let spec = match RemoteSpec::resolve(config, name) {
            Ok(spec) => spec,
            Err(err) => {
                tracing::debug!(remote = name, "not observing: {err:#}");
                continue;
            }
        };
        started.insert(name.clone());
        // Elect now, so that on return the file is either kept current by
        // another process or marked as not current.
        let lock = take_lock(name).unwrap_or_else(|err| {
            tracing::warn!(remote = name, "observer lock: {err:#}");
            None
        });
        if lock.is_some() {
            mark_stale(name);
        }
        let spawned = thread::Builder::new()
            .name(format!("observe-{name}"))
            .spawn(move || observe(spec, lock));
        if let Err(err) = spawned {
            tracing::warn!(remote = name, "failed to start observer: {err}");
        }
    }
}

/// What the previous observer wrote is not current until the node answers.
fn mark_stale(name: &str) {
    let mut observation = load(name).unwrap_or_default();
    observation.connected = false;
    if let Err(err) = save(name, &observation) {
        tracing::warn!(remote = name, "failed to save observation: {err:#}");
    }
}

/// Become the remote's observer unless `lock` already makes it so, then
/// observe until the process ends.
fn observe(spec: RemoteSpec, lock: Option<File>) {
    let _lock = match lock {
        Some(lock) => lock,
        None => loop {
            thread::sleep(TAKEOVER_INTERVAL);
            match take_lock(&spec.name) {
                Ok(Some(lock)) => {
                    mark_stale(&spec.name);
                    break lock;
                }
                Ok(None) => {}
                Err(err) => tracing::warn!(remote = %spec.name, "observer lock: {err:#}"),
            }
        },
    };
    tracing::info!(remote = %spec.name, "observing remote");
    let mut observation = load(&spec.name).unwrap_or_default();
    let mut attempt = 0;
    loop {
        let started = Instant::now();
        let reason = match stream(&spec, &mut observation) {
            Ok(reason) => reason,
            Err(err) => format!("{err:#}"),
        };
        tracing::info!(remote = %spec.name, reason, "remote observation lost");
        observation.connected = false;
        if let Err(err) = save(&spec.name, &observation) {
            tracing::warn!(remote = %spec.name, "failed to save observation: {err:#}");
        }
        if started.elapsed() >= super::attach::STABLE {
            attempt = 0;
        }
        thread::sleep(super::attach::backoff(attempt));
        attempt += 1;
    }
}

fn take_lock(name: &str) -> Result<Option<File>> {
    let file = super::cache::lock_file(name, &format!("observer.v{GENERATION}.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(err)) => Err(err).context("Failed to lock"),
    }
}

/// Kills the stream's ssh when dropped.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Run one `node watch` stream, folding frames into `observation` and
/// saving it, until the stream ends or goes quiet. Returns why it ended.
fn stream(spec: &RemoteSpec, observation: &mut Observation) -> Result<String> {
    let node = Node::connect_reader(spec, protocol::WATCH)?;
    let request = serde_json::to_vec(&WatchRequest { stream: true })?;
    let mut child = Child(
        node.transport
            .spawn(&node.verb_script(protocol::WATCH), request)?,
    );
    let stdout = child.0.stdout.take().expect("stdout is piped");
    let mut stderr = child.0.stderr.take().expect("stderr is piped");
    let stderr = thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr.read_to_end(&mut text);
        text
    });
    let (lines, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    // Wall clock: a monotonic clock stops while the laptop sleeps.
    let mut deadline = now() + STALE_AFTER.as_secs();
    loop {
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(line) => {
                if let Ok(frame) = serde_json::from_str::<WatchFrame>(&line) {
                    if let WatchFrame::Error { message } = &frame {
                        tracing::info!(remote = %spec.name, message, "remote observation failed");
                    }
                    let snapshot = matches!(frame, WatchFrame::Snapshot(_));
                    observation.apply(frame, now());
                    save(&spec.name, observation)?;
                    if snapshot {
                        super::siblings::adopt(&spec.name, observation, false);
                    }
                    deadline = now() + STALE_AFTER.as_secs();
                } else if let Ok(envelope) = serde_json::from_str::<ErrorEnvelope>(&line) {
                    return Ok(envelope.error.message);
                } else {
                    tracing::debug!(remote = %spec.name, line, "unexpected watch output");
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if now() > deadline {
                    return Ok(format!(
                        "no answer from '{}' for {}s",
                        spec.name,
                        STALE_AFTER.as_secs()
                    ));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                let status = child.0.wait().ok().and_then(|status| status.code());
                let stderr = stderr.join().unwrap_or_default();
                if let Some(failure) = node.transport.terminal_failure(status, &stderr) {
                    return Ok(failure.message);
                }
                let detail = protocol::first_line(&String::from_utf8_lossy(&stderr))
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("exit status {status:?}"));
                return Ok(format!("watch on '{}' ended: {detail}", spec.name));
            }
        }
    }
}

/// A current observation of `name`: the file while an observer keeps it
/// current, else one `node watch` call. Disconnected (keeping the last
/// known workspaces) when the node cannot be reached. Adopts the siblings a
/// connected observation reports.
pub fn current(config: &Config, name: &str) -> Observation {
    let observation = fetch(config, name);
    if observation.is_connected(now()) {
        super::siblings::adopt(name, &observation, true);
    }
    observation
}

fn fetch(config: &Config, name: &str) -> Observation {
    let now = now();
    let mut observation = load(name).unwrap_or_default();
    let observed = matches!(take_lock(name), Ok(None));
    if observed {
        return observation;
    }
    let fetched = RemoteSpec::resolve(config, name).and_then(|spec| {
        let node = Node::connect_reader(&spec, protocol::WATCH)?;
        node.call::<_, Snapshot>(protocol::WATCH, &WatchRequest { stream: false }, false)
    });
    match fetched {
        Ok(snapshot) => observation.apply(WatchFrame::Snapshot(snapshot), now),
        Err(err) => {
            tracing::debug!(remote = name, "remote not reachable: {err:#}");
            observation.connected = false;
        }
    }
    observation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::AgentStatus;
    use crate::remote::node::observe::ObservedAgent;

    fn snapshot(now: u64, status: AgentStatus) -> WatchFrame {
        WatchFrame::Snapshot(Snapshot {
            now,
            workspaces: vec![ObservedWorkspace {
                ws_id: "w1".into(),
                ex_id: "x1".into(),
                panes: vec![ObservedPane {
                    index: 0,
                    alive: true,
                    agent: Some(ObservedAgent {
                        status: Some(status),
                        status_ts: Some(now - 3),
                        title: None,
                        agent_kind: Some("claude".into()),
                        prompt: None,
                    }),
                }],
                git: None,
                sibling: None,
                ended: None,
            }],
        })
    }

    #[test]
    fn frames_update_connectivity_and_skew() {
        let mut observation = Observation::default();
        assert!(!observation.is_connected(100));

        observation.apply(snapshot(90, AgentStatus::Working), 100);
        assert!(observation.is_connected(100));
        assert_eq!(observation.skew, 10);
        let pane = observation.pane("w1", "x1", 0).unwrap();
        let ts = pane.agent.as_ref().unwrap().status_ts.unwrap();
        assert_eq!(observation.local_ts(ts), 97);
        assert!(observation.pane("w1", "x2", 0).is_none());

        // Quiet past the deadline reads as disconnected without a writer.
        assert!(observation.is_connected(100 + STALE_AFTER.as_secs()));
        assert!(!observation.is_connected(101 + STALE_AFTER.as_secs()));

        observation.apply(WatchFrame::Heartbeat { now: 140 }, 150);
        assert!(observation.is_connected(150));

        observation.apply(
            WatchFrame::Error {
                message: "tmux failed".into(),
            },
            151,
        );
        assert!(!observation.is_connected(151));
        // The last snapshot is kept while disconnected.
        assert!(observation.pane("w1", "x1", 0).is_some());

        observation.apply(WatchFrame::Heartbeat { now: 150 }, 160);
        assert!(observation.is_connected(160));
    }

    #[test]
    fn negative_skew_moves_timestamps_back() {
        let mut observation = Observation::default();
        observation.apply(WatchFrame::Heartbeat { now: 110 }, 100);
        assert_eq!(observation.skew, -10);
        assert_eq!(observation.local_ts(50), 40);
        assert_eq!(observation.local_ts(5), 0);
    }

    /// Every key path of `value`, arrays looked into through their first
    /// element.
    fn key_paths(value: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    let path = format!("{prefix}.{key}");
                    out.push(path.clone());
                    key_paths(value, &path, out);
                }
            }
            serde_json::Value::Array(items) => {
                if let Some(first) = items.first() {
                    key_paths(first, &format!("{prefix}[]"), out);
                }
            }
            _ => {}
        }
    }

    /// Fails when the observation format changes. Readers of one generation
    /// share a file, so a change needs a new [`GENERATION`]: bump it, then
    /// update the lists here.
    #[test]
    fn format_is_pinned() {
        use crate::git::GitStatus;
        use crate::multiplexer::EndReason;
        use crate::remote::cache::LayoutPane;
        use crate::remote::node::Origin;
        use crate::remote::node::observe::Sibling;
        use crate::remote::protocol::WorkspaceIdentity;

        // Values travel as strings; a variant an older reader lacks makes
        // it drop the whole file.
        let variants = |status: AgentStatus, reason: EndReason| {
            let status = match status {
                AgentStatus::Working | AgentStatus::Waiting | AgentStatus::Done => 3,
            };
            let reason = match reason {
                EndReason::HostReboot => 1,
            };
            (status, reason)
        };
        assert_eq!(variants(AgentStatus::Done, EndReason::HostReboot), (3, 1));

        let mut full = snapshot(90, AgentStatus::Done);
        if let WatchFrame::Snapshot(snapshot) = &mut full {
            let ws = &mut snapshot.workspaces[0];
            if let Some(agent) = ws.panes[0].agent.as_mut() {
                agent.title = Some("t".into());
                agent.prompt = Some("p".into());
            }
            ws.git = Some(GitStatus {
                cached_at: Some(1),
                branch: Some("b".into()),
                ..GitStatus::default()
            });
            ws.ended = Some(EndReason::HostReboot);
            ws.sibling = Some(Sibling {
                identity: WorkspaceIdentity {
                    ws_id: "w2".into(),
                    project_id: "p".into(),
                    handle: "h".into(),
                    branch: "b".into(),
                    base_sha: "0".into(),
                },
                worktree: "/w".into(),
                pin: "workmux-1".into(),
                origin: Origin {
                    parent: "w1".into(),
                    base_label: "main".into(),
                    background: false,
                    spawned_at: 1,
                    layout: vec![LayoutPane {
                        index: 0,
                        pane: Default::default(),
                    }],
                },
            });
        }
        let mut observation = Observation::default();
        observation.apply(full, 100);
        let mut paths = Vec::new();
        key_paths(&serde_json::to_value(&observation).unwrap(), "", &mut paths);
        paths.sort();
        let expected = [
            ".connected",
            ".last_seen",
            ".skew",
            ".workspaces",
            ".workspaces[].ended",
            ".workspaces[].ex_id",
            ".workspaces[].git",
            ".workspaces[].git.ahead",
            ".workspaces[].git.base_branch",
            ".workspaces[].git.behind",
            ".workspaces[].git.branch",
            ".workspaces[].git.cached_at",
            ".workspaces[].git.has_conflict",
            ".workspaces[].git.has_upstream",
            ".workspaces[].git.is_dirty",
            ".workspaces[].git.is_rebasing",
            ".workspaces[].git.lines_added",
            ".workspaces[].git.lines_removed",
            ".workspaces[].git.uncommitted_added",
            ".workspaces[].git.uncommitted_removed",
            ".workspaces[].panes",
            ".workspaces[].panes[].agent",
            ".workspaces[].panes[].agent.agent_kind",
            ".workspaces[].panes[].agent.prompt",
            ".workspaces[].panes[].agent.status",
            ".workspaces[].panes[].agent.status_ts",
            ".workspaces[].panes[].agent.title",
            ".workspaces[].panes[].alive",
            ".workspaces[].panes[].index",
            ".workspaces[].sibling",
            ".workspaces[].sibling.background",
            ".workspaces[].sibling.base_label",
            ".workspaces[].sibling.identity",
            ".workspaces[].sibling.identity.base_sha",
            ".workspaces[].sibling.identity.branch",
            ".workspaces[].sibling.identity.handle",
            ".workspaces[].sibling.identity.project_id",
            ".workspaces[].sibling.identity.ws_id",
            ".workspaces[].sibling.layout",
            ".workspaces[].sibling.layout[].index",
            ".workspaces[].sibling.layout[].pane",
            ".workspaces[].sibling.layout[].pane.command",
            ".workspaces[].sibling.layout[].pane.focus",
            ".workspaces[].sibling.layout[].pane.name",
            ".workspaces[].sibling.layout[].pane.percentage",
            ".workspaces[].sibling.layout[].pane.size",
            ".workspaces[].sibling.layout[].pane.split",
            ".workspaces[].sibling.layout[].pane.target",
            ".workspaces[].sibling.layout[].pane.zoom",
            ".workspaces[].sibling.parent",
            ".workspaces[].sibling.pin",
            ".workspaces[].sibling.spawned_at",
            ".workspaces[].sibling.worktree",
            ".workspaces[].ws_id",
        ];
        assert_eq!(paths, expected, "bump GENERATION, then update this list");
        assert_eq!(GENERATION, 2);
    }

    #[test]
    fn observation_file_round_trips() {
        let mut observation = Observation::default();
        observation.apply(snapshot(90, AgentStatus::Done), 100);
        let json = serde_json::to_string(&observation).unwrap();
        let back: Observation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, observation);
        let empty: Observation = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Observation::default());
    }
}
