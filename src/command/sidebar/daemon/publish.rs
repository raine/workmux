//! What the daemon publishes: one tick turns an observation into a snapshot,
//! and the socket server sends each changed snapshot to every sidebar.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::SidebarPosition;
use crate::git::GitStatus;
use crate::state::StateStore;

use super::super::app::{SidebarFilterMode, SidebarLayoutMode};
use super::super::snapshot::{
    CheckPathEntry, PrPathEntry, SidebarSnapshot, SnapshotInputs, build_snapshot,
};

use super::observe::{InactivityTracker, MuxState, shows_live_output};
use super::workers::git_status_semantically_equal;

struct BroadcastCache {
    snapshot: Option<SidebarSnapshot>,
    payload: Vec<u8>,
    generation: u64,
}

struct SocketState {
    clients: Vec<UnixStream>,
    cached: BroadcastCache,
}

fn git_status_maps_equal(
    left: &HashMap<PathBuf, GitStatus>,
    right: &HashMap<PathBuf, GitStatus>,
) -> bool {
    left.len() == right.len()
        && left.iter().all(|(path, status)| {
            right
                .get(path)
                .is_some_and(|other| git_status_semantically_equal(status, other))
        })
}

fn snapshots_equal(left: &SidebarSnapshot, right: &SidebarSnapshot) -> bool {
    let SidebarSnapshot {
        position: _,
        layout_mode: _,
        filter_mode: _,
        window_views: _,
        active_windows: _,
        active_pane_ids: _,
        window_pane_counts: _,
        git_statuses: _,
        pr_statuses: _,
        check_statuses: _,
        interrupted_pane_ids: _,
        sleeping_pane_ids: _,
        group_by: _,
        expanded_groups: _,
        stale_pane_ids: _,
        collapse_stale: _,
        stale_threshold_secs: _,
        agents: _,
        config_version: _,
    } = left;

    left.position == right.position
        && left.layout_mode == right.layout_mode
        && left.filter_mode == right.filter_mode
        && left.window_views == right.window_views
        && left.active_windows == right.active_windows
        && left.active_pane_ids == right.active_pane_ids
        && left.window_pane_counts == right.window_pane_counts
        && git_status_maps_equal(&left.git_statuses, &right.git_statuses)
        && left.pr_statuses == right.pr_statuses
        && left.check_statuses == right.check_statuses
        && left.interrupted_pane_ids == right.interrupted_pane_ids
        && left.sleeping_pane_ids == right.sleeping_pane_ids
        && left.group_by == right.group_by
        && left.expanded_groups == right.expanded_groups
        && left.stale_pane_ids == right.stale_pane_ids
        && left.collapse_stale == right.collapse_stale
        && left.stale_threshold_secs == right.stale_threshold_secs
        && left.agents == right.agents
        && left.config_version == right.config_version
}

fn stream_is_connected(stream: &UnixStream) -> bool {
    let mut byte = 0u8;
    // SAFETY: recv receives a valid stream fd and a writable one-byte buffer.
    let result = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if result >= 0 {
        if result > 0 {
            tracing::warn!("sidebar client sent unexpected data");
        }
        return false;
    }

    matches!(
        std::io::Error::last_os_error().kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}

/// Unix socket server for broadcasting snapshots to clients.
pub(super) struct SocketServer {
    state: Arc<Mutex<SocketState>>,
}

impl SocketServer {
    pub(super) fn bind(path: &Path) -> std::io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        // Restrict socket to owner only (prevent other local users from reading snapshots)
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let state = Arc::new(Mutex::new(SocketState {
            clients: Vec::new(),
            cached: BroadcastCache {
                snapshot: None,
                payload: Vec::new(),
                generation: 0,
            },
        }));
        let accept_state = state.clone();

        thread::spawn(move || {
            loop {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        tracing::error!(%error, "sidebar socket accept failed");
                        break;
                    }
                };
                let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                loop {
                    let (generation, payload) = {
                        let state = accept_state.lock().unwrap();
                        (state.cached.generation, state.cached.payload.clone())
                    };
                    if !payload.is_empty() && stream.write_all(&payload).is_err() {
                        break;
                    }

                    let mut state = accept_state.lock().unwrap();
                    if state.cached.generation != generation {
                        continue;
                    }
                    state.clients.push(stream);
                    tracing::debug!(clients = state.clients.len(), "sidebar client connected");
                    break;
                }
            }
        });

        Ok(Self { state })
    }

    /// Publish a snapshot when its sidebar-visible contents change.
    pub(super) fn broadcast(&self, snapshot: &SidebarSnapshot) -> bool {
        let mut state = self.state.lock().unwrap();
        if state
            .cached
            .snapshot
            .as_ref()
            .is_some_and(|cached| snapshots_equal(cached, snapshot))
        {
            return false;
        }

        let data = match serde_json::to_vec(snapshot) {
            Ok(data) => data,
            Err(error) => {
                tracing::error!(%error, "failed to serialize sidebar snapshot");
                return false;
            }
        };
        if data.len() > 1024 * 1024 {
            tracing::error!(
                payload_bytes = data.len(),
                "sidebar snapshot exceeds client limit"
            );
            return false;
        }
        let mut payload = Vec::with_capacity(4 + data.len());
        payload.extend_from_slice(&(data.len() as u32).to_be_bytes());
        payload.extend_from_slice(&data);

        state.cached.snapshot = Some(snapshot.clone());
        state.cached.payload.clone_from(&payload);
        state.cached.generation = state.cached.generation.wrapping_add(1);
        let mut clients = std::mem::take(&mut state.clients);
        drop(state);

        let before = clients.len();
        clients.retain_mut(|stream| stream.write_all(&payload).is_ok());
        let dropped = before - clients.len();
        let mut state = self.state.lock().unwrap();
        let remaining = state.clients.len() + clients.len();
        state.clients.append(&mut clients);
        if dropped > 0 {
            tracing::info!(
                dropped,
                remaining,
                payload_bytes = data.len(),
                "sidebar broadcast: clients disconnected"
            );
        }
        true
    }

    pub(super) fn client_count(&self) -> usize {
        let mut state = self.state.lock().unwrap();
        state.clients.retain(stream_is_connected);
        state.clients.len()
    }
}

/// Inputs gathered from the environment for one daemon tick.
pub(super) struct TickInput {
    pub(super) agents: Vec<crate::multiplexer::AgentPane>,
    pub(super) tmux_state: MuxState,
    pub(super) captured_panes: HashMap<String, String>,
    pub(super) now: Instant,
    pub(super) now_ts: u64,
    pub(super) position: SidebarPosition,
    pub(super) layout_mode: SidebarLayoutMode,
    pub(super) filter_mode: SidebarFilterMode,
    pub(super) sort: crate::config::SidebarSort,
    pub(super) group_by: Option<crate::config::SidebarGroupBy>,
    pub(super) collapse_stale: bool,
    pub(super) stale_threshold_secs: u64,
    pub(super) expanded_groups: Vec<String>,
    pub(super) git_statuses: HashMap<PathBuf, GitStatus>,
    pub(super) pr_statuses: HashMap<PathBuf, PrPathEntry>,
    pub(super) check_statuses: HashMap<PathBuf, CheckPathEntry>,
    pub(super) sleeping_pane_ids: HashSet<String>,
}

/// A state-file write to apply after computing the tick.
pub(super) struct AgentWrite {
    pane_id: String,
    resumed_ts: u64,
}

/// Output of a single tick computation.
pub(super) struct TickOutput {
    pub(super) snapshot: SidebarSnapshot,
    agent_writes: Vec<AgentWrite>,
    runtime_write: Option<crate::state::RuntimeState>,
    /// The new interrupted set. Caller should commit to `last_interrupted`
    /// only after side effects are applied successfully.
    pub(super) next_interrupted: HashSet<String>,
}

/// Compute one daemon tick from in-memory inputs.
///
/// 1. Runs inactivity detection
/// 2. Mutates agents in memory (status and activity timestamps reset for resumed agents)
/// 3. Builds the snapshot from the already-mutated agents
/// 4. Returns side effects (state file writes, runtime file write)
pub(super) fn compute_tick(
    input: TickInput,
    tracker: &mut InactivityTracker,
    last_interrupted: &HashSet<String>,
    status_icons: &crate::config::StatusIcons,
    heartbeat_due: bool,
) -> TickOutput {
    let TickInput {
        mut agents,
        tmux_state,
        captured_panes,
        now,
        now_ts,
        position,
        layout_mode,
        filter_mode,
        sort,
        group_by,
        collapse_stale,
        stale_threshold_secs,
        expanded_groups,
        git_statuses,
        pr_statuses,
        check_statuses,
        sleeping_pane_ids,
    } = input;

    // Phase 1: Inactivity detection
    let observable: Vec<_> = agents
        .iter()
        .filter(|agent| shows_live_output(agent))
        .cloned()
        .collect();
    let interrupted = tracker.check_with(&observable, now, |pane_id| {
        captured_panes.get(pane_id).cloned()
    });

    // Phase 2: Mutate agents in memory for resumed agents
    let mut agent_writes = Vec::new();
    if !last_interrupted.is_empty() {
        for agent in &mut agents {
            if last_interrupted.contains(&agent.pane_id) && !interrupted.contains(&agent.pane_id) {
                agent.status_ts = Some(now_ts);
                agent.activity_ts = Some(now_ts);
                agent_writes.push(AgentWrite {
                    pane_id: agent.pane_id.clone(),
                    resumed_ts: now_ts,
                });
            }
        }
    }

    // Phase 3: Build snapshot from already-mutated agents. Interruption is
    // known from phase 1, so priority ordering sees the same state clients do.
    let snapshot = build_snapshot(SnapshotInputs {
        agents,
        tmux_statuses: tmux_state.window_statuses,
        pane_window_ids: tmux_state.pane_window_ids,
        pane_window_indexes: tmux_state.pane_window_indexes,
        active_windows: tmux_state.active_windows,
        active_pane_ids: tmux_state.active_pane_ids,
        window_pane_counts: tmux_state.window_pane_counts,
        position,
        layout_mode,
        filter_mode,
        sort,
        group_by,
        collapse_stale,
        stale_threshold_secs: Some(stale_threshold_secs),
        expanded_groups,
        status_icons: status_icons.clone(),
        git_statuses,
        pr_statuses,
        check_statuses,
        sleeping_pane_ids,
        interrupted_pane_ids: interrupted.clone(),
    });

    // Phase 4: Determine runtime write side effect
    let runtime_write = if interrupted != *last_interrupted || heartbeat_due {
        Some(crate::state::RuntimeState {
            interrupted_pane_ids: interrupted.clone(),
            updated_ts: now_ts,
        })
    } else {
        None
    };

    TickOutput {
        snapshot,
        agent_writes,
        runtime_write,
        next_interrupted: interrupted,
    }
}

/// Apply side effects computed by `compute_tick`.
/// Returns true if runtime state was written.
pub(super) fn apply_tick_effects(
    output: &TickOutput,
    store: &StateStore,
    backend: &str,
    instance: &str,
) -> bool {
    for write in &output.agent_writes {
        let pane_key = crate::state::PaneKey {
            backend: backend.to_string(),
            instance: instance.to_string(),
            pane_id: write.pane_id.clone(),
        };
        if let Ok(Some((mut state, revision))) = store.get_agent_with_revision(&pane_key) {
            state.status_ts = Some(write.resumed_ts);
            state.activity_ts = Some(write.resumed_ts);
            let _ = store.upsert_agent_if_revision(&state, &revision);
        }
    }

    if let Some(ref runtime) = output.runtime_write {
        store.write_runtime(backend, instance, runtime).is_ok()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::working_agent;
    use super::*;

    use super::super::super::preferences::Preferences;
    use super::super::super::snapshot::STALE_THRESHOLD_SECS;
    use super::super::super::view::WindowView;
    use crate::multiplexer::AgentStatus;

    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn empty_snapshot() -> SidebarSnapshot {
        SidebarSnapshot {
            window_views: HashMap::new(),
            position: SidebarPosition::Left,
            layout_mode: SidebarLayoutMode::Tiles,
            filter_mode: SidebarFilterMode::None,
            group_by: None,
            expanded_groups: Vec::new(),
            stale_pane_ids: std::collections::HashSet::new(),
            collapse_stale: false,
            stale_threshold_secs: STALE_THRESHOLD_SECS,
            active_windows: HashSet::new(),
            active_pane_ids: HashSet::new(),
            window_pane_counts: HashMap::new(),
            git_statuses: HashMap::new(),
            pr_statuses: HashMap::new(),
            check_statuses: HashMap::new(),
            interrupted_pane_ids: HashSet::new(),
            sleeping_pane_ids: HashSet::new(),
            agents: Vec::new(),
            config_version: 0,
        }
    }

    fn read_snapshot(stream: &mut UnixStream) -> SidebarSnapshot {
        use std::io::Read;

        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let mut payload = vec![0; u32::from_be_bytes(header) as usize];
        stream.read_exact(&mut payload).unwrap();
        serde_json::from_slice(&payload).unwrap()
    }

    fn wait_for_clients(server: &SocketServer, expected: usize) {
        for _ in 0..100 {
            if server.client_count() == expected {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(server.client_count(), expected);
    }

    #[test]
    fn snapshot_comparison_covers_every_sidebar_state_category() {
        use crate::github::{CheckState, CheckSummary, PrSummary};

        let original = empty_snapshot();
        let mut variants = Vec::new();

        let mut changed = original.clone();
        changed.position = SidebarPosition::Top;
        variants.push(changed);
        let mut changed = original.clone();
        changed.layout_mode = SidebarLayoutMode::Compact;
        variants.push(changed);
        let mut changed = original.clone();
        changed.filter_mode = SidebarFilterMode::Session;
        variants.push(changed);
        let mut changed = original.clone();
        changed.window_views.insert(
            "@1".into(),
            WindowView {
                name: "Work".into(),
                pane_ids: HashSet::from(["%1".into()]),
            },
        );
        variants.push(changed);
        let mut changed = original.clone();
        changed.active_windows.insert(("s".into(), "@1".into()));
        variants.push(changed);
        let mut changed = original.clone();
        changed.active_pane_ids.insert("%1".into());
        variants.push(changed);
        let mut changed = original.clone();
        changed.window_pane_counts.insert("@1".into(), 2);
        variants.push(changed);
        let mut changed = original.clone();
        changed.git_statuses.insert(
            PathBuf::from("/repo"),
            GitStatus {
                is_dirty: true,
                ..GitStatus::default()
            },
        );
        variants.push(changed);
        let mut changed = original.clone();
        changed.pr_statuses.insert(
            PathBuf::from("/repo"),
            PrSummary {
                number: 1,
                title: "title".into(),
                state: "OPEN".into(),
                is_draft: false,
                checks: None,
                check_meta: None,
                url: None,
                closing_issues: Vec::new(),
            },
        );
        variants.push(changed);
        let mut changed = original.clone();
        changed.check_statuses.insert(
            PathBuf::from("/repo"),
            CheckSummary {
                state: CheckState::Success,
                meta: None,
            },
        );
        variants.push(changed);
        let mut changed = original.clone();
        changed.interrupted_pane_ids.insert("%1".into());
        variants.push(changed);
        let mut changed = original.clone();
        changed.sleeping_pane_ids.insert("%1".into());
        variants.push(changed);
        let mut changed = original.clone();
        changed.agents.push(working_agent("%1", 1));
        variants.push(changed);
        let mut changed = original.clone();
        changed.config_version = 1;
        variants.push(changed);

        assert!(
            variants
                .iter()
                .all(|changed| !snapshots_equal(&original, changed))
        );
    }

    #[test]
    fn agent_display_and_sorting_changes_are_meaningful() {
        let mut first = empty_snapshot();
        first.agents.push(working_agent("%1", 1));

        let mut prompt = first.clone();
        prompt.agents[0].pane_title = Some("updated prompt".into());
        assert!(!snapshots_equal(&first, &prompt));

        let mut status = first.clone();
        status.agents[0].status = Some(AgentStatus::Waiting);
        assert!(!snapshots_equal(&first, &status));

        let mut activity = first.clone();
        activity.agents[0].activity_ts = Some(200);
        assert!(!snapshots_equal(&first, &activity));

        let mut window_order = first.clone();
        window_order.agents[0].window_index = Some(2);
        assert!(!snapshots_equal(&first, &window_order));
    }

    #[test]
    fn snapshot_comparison_is_set_order_independent() {
        let mut first = empty_snapshot();
        first.active_pane_ids.extend(["%1".into(), "%2".into()]);
        first
            .active_windows
            .extend([("one".into(), "@1".into()), ("two".into(), "@2".into())]);
        let mut second = empty_snapshot();
        second.active_pane_ids.extend(["%2".into(), "%1".into()]);
        second
            .active_windows
            .extend([("two".into(), "@2".into()), ("one".into(), "@1".into())]);

        assert!(snapshots_equal(&first, &second));
    }

    #[test]
    fn git_snapshot_comparison_ignores_only_cache_freshness() {
        let mut first = empty_snapshot();
        first.git_statuses.insert(
            PathBuf::from("/repo"),
            GitStatus {
                cached_at: Some(1),
                ..GitStatus::default()
            },
        );
        let mut second = first.clone();
        second
            .git_statuses
            .get_mut(Path::new("/repo"))
            .unwrap()
            .cached_at = Some(2);
        assert!(snapshots_equal(&first, &second));

        second
            .git_statuses
            .get_mut(Path::new("/repo"))
            .unwrap()
            .is_rebasing = true;
        assert!(!snapshots_equal(&first, &second));
    }

    #[test]
    fn socket_server_delivers_initial_cache_and_changed_state_only() {
        use std::io::{Read, Write};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sidebar.sock");
        let server = SocketServer::bind(&socket).unwrap();
        let mut first = UnixStream::connect(&socket).unwrap();
        first
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();

        let mut snapshot = empty_snapshot();
        assert!(server.broadcast(&snapshot));
        assert_eq!(read_snapshot(&mut first).config_version, 0);

        let mut late = UnixStream::connect(&socket).unwrap();
        late.set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        assert_eq!(read_snapshot(&mut late).config_version, 0);

        assert!(!server.broadcast(&snapshot));
        let mut header = [0; 4];
        let error = first.read_exact(&mut header).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));

        snapshot.git_statuses.insert(
            PathBuf::from("/repo"),
            GitStatus {
                cached_at: Some(1),
                ..GitStatus::default()
            },
        );
        assert!(server.broadcast(&snapshot));
        let _ = read_snapshot(&mut first);
        let _ = read_snapshot(&mut late);
        snapshot
            .git_statuses
            .get_mut(Path::new("/repo"))
            .unwrap()
            .cached_at = Some(2);
        assert!(!server.broadcast(&snapshot));

        snapshot.active_pane_ids.insert("%1".into());
        assert!(server.broadcast(&snapshot));
        assert!(read_snapshot(&mut first).active_pane_ids.contains("%1"));
        wait_for_clients(&server, 2);

        drop(first);
        drop(late);
        wait_for_clients(&server, 0);

        let mut malformed = UnixStream::connect(&socket).unwrap();
        wait_for_clients(&server, 1);
        malformed.write_all(b"unexpected").unwrap();
        wait_for_clients(&server, 0);
    }

    #[test]
    fn concurrent_accept_observes_latest_published_generation() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sidebar.sock");
        let server = Arc::new(SocketServer::bind(&socket).unwrap());
        assert!(server.broadcast(&empty_snapshot()));

        let publisher = Arc::clone(&server);
        let publish = thread::spawn(move || {
            for version in 1..=20 {
                let mut snapshot = empty_snapshot();
                snapshot.config_version = version;
                assert!(publisher.broadcast(&snapshot));
                thread::sleep(Duration::from_millis(1));
            }
        });
        let mut client = UnixStream::connect(&socket).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let mut seen = 0;
        while seen < 20 {
            let next = read_snapshot(&mut client).config_version;
            assert!(next >= seen);
            seen = next;
        }
        publish.join().unwrap();
        assert_eq!(seen, 20);
    }

    mod tick {
        use super::*;
        use crate::config::StatusIcons;
        use crate::multiplexer::AgentStatus;
        use crate::state::{PaneKey, StateStore};

        const BACKEND: &str = "tmux";
        const INSTANCE: &str = "test";

        fn test_store() -> (StateStore, tempfile::TempDir) {
            let dir = tempfile::TempDir::new().unwrap();
            let store = StateStore::with_path(dir.path().to_path_buf()).unwrap();
            (store, dir)
        }

        fn pane_key(pane_id: &str) -> PaneKey {
            PaneKey {
                backend: BACKEND.to_string(),
                instance: INSTANCE.to_string(),
                pane_id: pane_id.to_string(),
            }
        }

        fn seed_agent(store: &StateStore, pane_id: &str, status_ts: u64, updated_ts: u64) {
            let state = crate::state::AgentState {
                pane_key: pane_key(pane_id),
                workdir: PathBuf::from("/tmp"),
                status: Some(AgentStatus::Working),
                status_ts: Some(status_ts),
                activity_ts: Some(status_ts),
                pane_title: None,
                pane_pid: 1,
                command: "node".to_string(),
                updated_ts,
                window_name: None,
                session_name: None,
                boot_id: None,
                agent_kind: None,
                agent_session_id: None,
                prompt: None,
                remote: None,
            };
            store.upsert_agent(&state).unwrap();
        }

        fn do_tick(
            tracker: &mut InactivityTracker,
            last: &mut HashSet<String>,
            agents: Vec<crate::multiplexer::AgentPane>,
            captures: HashMap<String, String>,
            now: Instant,
            now_ts: u64,
        ) -> TickOutput {
            let output = compute_tick(
                TickInput {
                    agents,
                    tmux_state: MuxState {
                        live_panes: HashMap::new(),
                        window_statuses: HashMap::new(),
                        active_windows: HashSet::new(),
                        pane_window_ids: HashMap::new(),
                        pane_window_indexes: HashMap::new(),
                        active_pane_ids: HashSet::new(),
                        window_pane_counts: HashMap::new(),
                        server_boot_id: None,
                        prefs: Preferences::default(),
                    },
                    captured_panes: captures,
                    sort: crate::config::SidebarSort::default(),
                    group_by: None,
                    collapse_stale: false,
                    stale_threshold_secs: crate::command::sidebar::snapshot::STALE_THRESHOLD_SECS,
                    expanded_groups: Vec::new(),
                    now,
                    now_ts,
                    position: SidebarPosition::Left,
                    layout_mode: SidebarLayoutMode::default(),
                    filter_mode: SidebarFilterMode::default(),
                    git_statuses: HashMap::new(),
                    pr_statuses: HashMap::new(),
                    check_statuses: HashMap::new(),
                    sleeping_pane_ids: HashSet::new(),
                },
                tracker,
                last,
                &StatusIcons::default(),
                false,
            );
            // Commit state like the daemon loop does after apply_tick_effects
            *last = output.next_interrupted.clone();
            output
        }

        fn cap(content: &str) -> HashMap<String, String> {
            HashMap::from([("%1".to_string(), content.to_string())])
        }

        fn cap2(content: &str) -> HashMap<String, String> {
            HashMap::from([
                ("%1".to_string(), content.to_string()),
                ("%2".to_string(), content.to_string()),
            ])
        }

        #[test]
        fn resumed_agent_gets_status_ts_reset() {
            let (store, _dir) = test_store();
            seed_agent(&store, "%1", 100, 1);

            let mut tracker = InactivityTracker::new(Duration::from_secs(10));
            let mut last = HashSet::new();
            let t0 = Instant::now();

            // Tick 1: start observing
            do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 1)],
                cap("hello"),
                t0,
                1000,
            );

            // Tick 2: interrupted
            let output = do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 1)],
                cap("hello"),
                t0 + Duration::from_secs(11),
                1011,
            );
            assert!(output.snapshot.interrupted_pane_ids.contains("%1"));

            // Tick 3: agent resumes (updated_ts 1 -> 2)
            let output = do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 2)],
                cap("hello"),
                t0 + Duration::from_secs(12),
                1012,
            );
            assert!(output.snapshot.interrupted_pane_ids.is_empty());

            // Snapshot has corrected status_ts (no stale one-tick race)
            let agent = output
                .snapshot
                .agents
                .iter()
                .find(|a| a.pane_id == "%1")
                .unwrap();
            assert_eq!(agent.status_ts, Some(1012));
            assert_eq!(agent.activity_ts, Some(1012));

            // Side effect says to write it to disk
            assert_eq!(output.agent_writes.len(), 1);
            assert_eq!(output.agent_writes[0].resumed_ts, 1012);

            // Apply effects and verify store
            apply_tick_effects(&output, &store, BACKEND, INSTANCE);
            let persisted = store.get_agent(&pane_key("%1")).unwrap().unwrap();
            assert_eq!(persisted.status_ts, Some(1012));
            assert_eq!(persisted.activity_ts, Some(1012));
        }

        #[test]
        fn old_working_agent_resumes_output_without_status_update() {
            let (store, _dir) = test_store();
            seed_agent(&store, "%1", 100, 1);
            let mut tracker = InactivityTracker::new(Duration::from_secs(10));
            let mut last = HashSet::new();
            let t0 = Instant::now();
            let now_ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            // Status and activity are hours old, but changing output is live work.
            for (elapsed, content) in [(0, "first output"), (8, "more output")] {
                let output = do_tick(
                    &mut tracker,
                    &mut last,
                    vec![working_agent("%1", 1)],
                    cap(content),
                    t0 + Duration::from_secs(elapsed),
                    now_ts + elapsed,
                );
                assert!(output.snapshot.interrupted_pane_ids.is_empty());
                assert!(output.snapshot.stale_pane_ids.is_empty());
            }

            // A quiet pane can still be classified as interrupted and stale.
            let output = do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 1)],
                cap("more output"),
                t0 + Duration::from_secs(19),
                now_ts + 19,
            );
            assert!(output.snapshot.interrupted_pane_ids.contains("%1"));
            assert!(output.snapshot.stale_pane_ids.contains("%1"));

            // New output clears both classifications without another status hook.
            let output = do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 1)],
                cap("resumed output"),
                t0 + Duration::from_secs(20),
                now_ts + 20,
            );
            assert!(output.snapshot.interrupted_pane_ids.is_empty());
            assert!(output.snapshot.stale_pane_ids.is_empty());
            assert_eq!(output.snapshot.agents[0].activity_ts, Some(now_ts + 20));

            apply_tick_effects(&output, &store, BACKEND, INSTANCE);
            let persisted = store.get_agent(&pane_key("%1")).unwrap().unwrap();
            assert_eq!(persisted.activity_ts, Some(now_ts + 20));
            assert_eq!(persisted.updated_ts, 1);
            let runtime = store.read_runtime(BACKEND, INSTANCE);
            assert!(runtime.interrupted_pane_ids.is_empty());
            assert_eq!(runtime.updated_ts, now_ts + 20);
        }

        #[test]
        fn only_resumed_agent_gets_reset() {
            let (store, _dir) = test_store();
            seed_agent(&store, "%1", 100, 1);
            seed_agent(&store, "%2", 200, 1);

            let mut tracker = InactivityTracker::new(Duration::from_secs(10));
            let mut last = HashSet::new();
            let t0 = Instant::now();

            let agents = vec![working_agent("%1", 1), working_agent("%2", 1)];

            // Tick 1 + 2: both interrupted
            do_tick(
                &mut tracker,
                &mut last,
                agents.clone(),
                cap2("hello"),
                t0,
                1000,
            );
            do_tick(
                &mut tracker,
                &mut last,
                agents,
                cap2("hello"),
                t0 + Duration::from_secs(11),
                1011,
            );

            // Tick 3: only %1 resumes
            let mixed = vec![working_agent("%1", 2), working_agent("%2", 1)];
            let output = do_tick(
                &mut tracker,
                &mut last,
                mixed,
                cap2("hello"),
                t0 + Duration::from_secs(12),
                1012,
            );

            // Only %1 in agent_writes
            assert_eq!(output.agent_writes.len(), 1);
            assert_eq!(output.agent_writes[0].pane_id, "%1");

            // Apply and verify
            apply_tick_effects(&output, &store, BACKEND, INSTANCE);
            let resumed = store.get_agent(&pane_key("%1")).unwrap().unwrap();
            assert_eq!(resumed.status_ts, Some(1012));
            assert_eq!(resumed.activity_ts, Some(1012));
            let untouched = store.get_agent(&pane_key("%2")).unwrap().unwrap();
            assert_eq!(untouched.status_ts, Some(200));
            assert_eq!(untouched.activity_ts, Some(200));
        }

        #[test]
        fn runtime_file_reflects_interrupted_set() {
            let (store, _dir) = test_store();

            let mut tracker = InactivityTracker::new(Duration::from_secs(10));
            let mut last = HashSet::new();
            let t0 = Instant::now();

            // Tick 1: not interrupted yet
            let output = do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 1)],
                cap("hello"),
                t0,
                1000,
            );
            apply_tick_effects(&output, &store, BACKEND, INSTANCE);

            // Tick 2: interrupted
            let output = do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 1)],
                cap("hello"),
                t0 + Duration::from_secs(11),
                1011,
            );
            apply_tick_effects(&output, &store, BACKEND, INSTANCE);
            assert!(
                store
                    .read_runtime(BACKEND, INSTANCE)
                    .interrupted_pane_ids
                    .contains("%1")
            );

            // Tick 3: resumes
            let output = do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 2)],
                cap("hello"),
                t0 + Duration::from_secs(12),
                1012,
            );
            apply_tick_effects(&output, &store, BACKEND, INSTANCE);
            assert!(
                store
                    .read_runtime(BACKEND, INSTANCE)
                    .interrupted_pane_ids
                    .is_empty()
            );
        }

        #[test]
        fn missing_agent_file_does_not_panic() {
            let (store, _dir) = test_store();

            let mut tracker = InactivityTracker::new(Duration::from_secs(10));
            let mut last = HashSet::new();
            let t0 = Instant::now();

            // Tick 1 + 2: become interrupted
            do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 1)],
                cap("hello"),
                t0,
                1000,
            );
            do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 1)],
                cap("hello"),
                t0 + Duration::from_secs(11),
                1011,
            );

            // Tick 3: resume with no agent file - should not panic
            let output = do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 2)],
                cap("hello"),
                t0 + Duration::from_secs(12),
                1012,
            );
            assert!(output.snapshot.interrupted_pane_ids.is_empty());
            apply_tick_effects(&output, &store, BACKEND, INSTANCE);
        }

        #[test]
        fn snapshot_has_correct_status_ts_on_resume_tick() {
            // Proves the one-tick race is structurally impossible: agents
            // are mutated before build_snapshot, not patched after.
            let mut tracker = InactivityTracker::new(Duration::from_secs(10));
            let mut last = HashSet::new();
            let t0 = Instant::now();

            do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 1)],
                cap("hello"),
                t0,
                1000,
            );
            do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 1)],
                cap("hello"),
                t0 + Duration::from_secs(11),
                1011,
            );

            // Resume tick: snapshot must have the fresh status_ts, not the stale 100
            let output = do_tick(
                &mut tracker,
                &mut last,
                vec![working_agent("%1", 2)],
                cap("hello"),
                t0 + Duration::from_secs(12),
                1012,
            );
            let agent = output
                .snapshot
                .agents
                .iter()
                .find(|a| a.pane_id == "%1")
                .unwrap();
            assert_eq!(agent.status_ts, Some(1012));
            assert_eq!(agent.activity_ts, Some(1012));
            assert!(!output.snapshot.interrupted_pane_ids.contains("%1"));
        }
    }
}
