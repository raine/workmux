//! The sidebar daemon: one background process per multiplexer server that
//! observes it, keeps sidebars placed, and publishes snapshots to every
//! sidebar.

use anyhow::Result;
use signal_hook::iterator::{Handle as SignalHandle, Signals};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::multiplexer::{create_backend, detect_backend};
use crate::state::StateStore;

use super::host::SidebarHost;
use super::store::PublishedViews;

mod observe;
mod publish;
mod scheduler;
mod workers;

use observe::{
    InactivityTracker, MuxState, PlacementObservation, gather_captures,
    include_unregistered_zellij_agents, query_mux_state,
};
use publish::{SocketServer, TickInput, apply_tick_effects, compute_tick};
use scheduler::{REX_SERVER_GRACE, Scheduler};
use workers::{
    GitWorkerPath, GithubWorkerPath, ProjectConfigCache, reload_daemon_config,
    snapshot_status_cache, spawn_config_watcher, spawn_git_worker, spawn_github_worker,
};

/// Compute the socket path for a multiplexer instance.
pub fn socket_path(instance_id: &str) -> PathBuf {
    super::session_state::socket_path(instance_id)
}

fn spawn_signal_listener(
    term: Arc<AtomicBool>,
    dirty_flag: Arc<AtomicBool>,
    wake_tx: mpsc::SyncSender<()>,
) -> Result<(SignalHandle, thread::JoinHandle<()>)> {
    let mut signals = Signals::new([signal_hook::consts::SIGTERM, signal_hook::consts::SIGUSR1])?;
    let handle = signals.handle();
    let thread = thread::spawn(move || {
        for signal in signals.forever() {
            match signal {
                signal_hook::consts::SIGTERM => term.store(true, Ordering::Relaxed),
                signal_hook::consts::SIGUSR1 => dirty_flag.store(true, Ordering::Relaxed),
                _ => continue,
            }
            let _ = wake_tx.try_send(());
            if signal == signal_hook::consts::SIGTERM {
                break;
            }
        }
    });
    Ok((handle, thread))
}

/// Take the daemon lock, waiting up to `timeout` for a daemon that is
/// shutting down to release it. `None` means another daemon holds it.
fn wait_for_daemon_lock(instance_id: &str, timeout: Duration) -> Result<Option<std::fs::File>> {
    let start = Instant::now();
    loop {
        if let Some(lock) = super::session_state::try_lock_daemon(instance_id)? {
            return Ok(Some(lock));
        }
        if start.elapsed() >= timeout {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run the sidebar daemon (headless, no TUI).
pub fn run() -> Result<()> {
    let mux = create_backend(detect_backend());
    let host = SidebarHost::resolve(mux.as_ref())?;
    let instance_id = host.instance_id();
    let sidebar_store = host.store();
    let config = Arc::new(Mutex::new(Config::load(None)?));
    let mut view_projects = super::view::ProjectCache::default();
    // Captured at startup and intentionally not live-reloaded. tmux's
    // @workmux_pane_status holds the icon string itself; build_snapshot
    // compares pane statuses to these exact strings to suppress stale
    // done/waiting markers, so swapping the icons mid-run would mis-suppress.
    let status_icons = config.lock().unwrap().status_icons.clone();
    let config_version = Arc::new(AtomicU64::new(0));

    // A daemon that is still shutting down holds the lock briefly
    let Some(_daemon_lock) = wait_for_daemon_lock(&instance_id, Duration::from_secs(2))? else {
        tracing::info!(instance_id = %instance_id, "sidebar daemon already running");
        return Ok(());
    };
    tracing::info!(instance_id = %instance_id, "sidebar daemon starting");

    let term = Arc::new(AtomicBool::new(false));
    let observation_dirty = Arc::new(AtomicBool::new(false));
    let publication_dirty = Arc::new(AtomicBool::new(false));
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (signal_handle, signal_thread) =
        spawn_signal_listener(term.clone(), observation_dirty.clone(), wake_tx.clone())?;
    let _wake_tx_keepalive = wake_tx.clone();

    let sock_path = socket_path(&instance_id);
    let _ = std::fs::remove_file(&sock_path);
    let server = SocketServer::bind(&sock_path)?;

    // Rex has no hooks: its event stream wakes observations early
    let rex_events = matches!(host, SidebarHost::Rex { .. }).then(|| {
        let observation_dirty = observation_dirty.clone();
        let wake_tx = wake_tx.clone();
        crate::multiplexer::rex::sidebar::EventFollower::spawn(move || {
            observation_dirty.store(true, Ordering::Relaxed);
            let _ = wake_tx.try_send(());
        })
    });

    let config_paths_tx = spawn_config_watcher(
        term.clone(),
        config.clone(),
        config_version.clone(),
        publication_dirty.clone(),
        wake_tx.clone(),
    );
    let (git_cache, git_path_tx) =
        spawn_git_worker(term.clone(), publication_dirty.clone(), wake_tx.clone());
    let (pr_cache, check_cache, github_path_tx) =
        spawn_github_worker(term.clone(), publication_dirty.clone(), wake_tx);

    sidebar_store.record_daemon_pid()?;

    let mut scheduler = Scheduler::new(Instant::now());
    let mut inactivity_tracker = InactivityTracker::new(Duration::from_secs(10));
    let mut last_interrupted: HashSet<String> = HashSet::new();
    let backend_name = mux.name().to_string();
    let store = StateStore::new()?;
    let mut agent_state_cache = crate::state::AgentStateCache::default();
    let mut compacted_boot_id: Option<String> = None;
    let mut cached_inputs: Option<(Vec<crate::multiplexer::AgentPane>, MuxState)> = None;
    let mut pending_captures: Option<HashMap<String, String>> = None;
    let mut publish_pending = false;
    let mut last_client_seen = Instant::now();
    let mut last_agent_list: Vec<String> = Vec::new();
    let mut published_views = PublishedViews::default();
    let mut last_health_log = Instant::now();
    let mut last_observed = Instant::now();
    let mut project_config_cache = ProjectConfigCache::default();
    let mut seen_config_version = 0;
    let mut last_config_dirs: HashSet<PathBuf> = HashSet::new();

    while !term.load(Ordering::Relaxed) {
        let now = Instant::now();
        if observation_dirty.swap(false, Ordering::Relaxed) {
            scheduler.notify_tmux_event(now);
        }
        if publication_dirty.swap(false, Ordering::Relaxed) {
            publish_pending = true;
        }

        if scheduler.observation_due(now) {
            scheduler.finish_observation(now);
            // Held through observe and reconcile, so a command placing
            // sidebars never races this pass.
            let layout_lock = match host.observation_lock() {
                Ok(Some(lock)) => lock,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(%error, "failed to check sidebar layout lock");
                    continue;
                }
            };
            match query_mux_state(&host) {
                Ok((tmux_state, placement)) => {
                    last_observed = Instant::now();
                    match placement {
                        PlacementObservation::Zellij(tabs) if server.client_count() > 0 => {
                            if let Err(error) =
                                super::zellij_panes::create_sidebar_in_active_tab_from_snapshot(
                                    &instance_id,
                                    &tabs,
                                )
                            {
                                tracing::warn!(%error, "failed to synchronize Zellij sidebars");
                            }
                        }
                        PlacementObservation::Rex(observation) => {
                            if let Err(error) =
                                super::rex_panes::reconcile(&instance_id, &observation)
                            {
                                tracing::warn!(%error, "failed to reconcile Rex sidebars");
                            }
                        }
                        _ => {}
                    }
                    drop(layout_lock);
                    if tmux_state.server_boot_id != compacted_boot_id {
                        match store.compact_context(
                            &backend_name,
                            &instance_id,
                            tmux_state.server_boot_id.as_deref(),
                        ) {
                            Ok(stats) => {
                                tracing::info!(
                                    scanned = stats.scanned,
                                    compacted = stats.compacted,
                                    retained_flat = stats.retained_flat,
                                    recovery_entries = stats.recovery_entries,
                                    "agent state compaction complete"
                                );
                                compacted_boot_id = tmux_state.server_boot_id.clone();
                            }
                            Err(error) => {
                                tracing::warn!(%error, "agent state compaction failed");
                            }
                        }
                    }

                    match store.load_reconciled_agents_from_snapshot_cached(
                        &mut agent_state_cache,
                        mux.as_ref(),
                        &tmux_state.live_panes,
                        tmux_state.server_boot_id.as_deref(),
                    ) {
                        Ok((agents, stats)) => {
                            let mut agents = agents;
                            if matches!(host, SidebarHost::Zellij { .. }) {
                                include_unregistered_zellij_agents(
                                    &mut agents,
                                    &tmux_state.live_panes,
                                );
                            }
                            let agents = {
                                let cfg = config.lock().unwrap();
                                let now_ts = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs();
                                crate::remote::agents::for_views(&cfg, agents, now_ts)
                            };
                            tracing::trace!(
                                listed = stats.listed,
                                metadata = stats.metadata,
                                reads = stats.reads,
                                parses = stats.parses,
                                "sidebar agent state load"
                            );
                            inactivity_tracker.reconcile_identities(
                                &tmux_state.live_panes,
                                tmux_state.server_boot_id.as_deref(),
                            );
                            cached_inputs = Some((agents, tmux_state));
                            publish_pending = true;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "failed to reconcile sidebar agent state");
                        }
                    }
                }
                Err(error) => tracing::warn!(%error, "failed to query sidebar multiplexer state"),
            }
        }

        let now = Instant::now();
        if scheduler.capture_due(now) {
            scheduler.finish_capture(now);
            if let Some((agents, mux_state)) = &cached_inputs {
                pending_captures =
                    Some(gather_captures(agents, &mux_state.live_panes, mux.as_ref()));
                publish_pending = true;
            }
        }

        if publish_pending && let Some((agents, tmux_state)) = &cached_inputs {
            publish_pending = false;
            let live_paths = agents
                .iter()
                .filter(|agent| agent.remote.is_none())
                .map(|agent| agent.path.clone())
                .collect();
            let version = config_version.load(Ordering::Relaxed);
            let invalidated = version != seen_config_version;
            seen_config_version = version;
            let resolution = project_config_cache.resolve(
                &live_paths,
                now,
                invalidated,
                crate::config::discover_project_config,
            );
            if resolution.changed {
                reload_daemon_config(&config);
                let previous = config_version.fetch_add(1, Ordering::Relaxed);
                // Do not consume a concurrent watcher invalidation as our own bump.
                if previous == seen_config_version {
                    seen_config_version = previous + 1;
                }
            }
            if resolution.reconciled || resolution.directories != last_config_dirs {
                let _ = config_paths_tx.send(resolution.directories.clone().into());
                last_config_dirs = resolution.directories;
            }
            let (
                position,
                layout_mode,
                sort,
                group_by,
                collapse_stale,
                stale_threshold_secs,
                git_status_enabled,
            ) = {
                let cfg = config.lock().unwrap();
                (
                    tmux_state.prefs.position_or(&cfg),
                    tmux_state.prefs.layout_mode(&cfg),
                    cfg.sidebar.sort.unwrap_or_default(),
                    tmux_state.prefs.grouping.resolve(cfg.sidebar.group_by()),
                    cfg.sidebar.collapse_stale(),
                    cfg.stale_after_secs(),
                    cfg.sidebar.git_status(),
                )
            };
            // Folding is part of the grouped presentation: a flat list shows
            // every agent it carries.
            let collapse_stale = collapse_stale && group_by.is_some();
            let now = Instant::now();
            let now_ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let mut output = compute_tick(
                TickInput {
                    agents: agents.clone(),
                    tmux_state: tmux_state.clone(),
                    captured_panes: pending_captures.take().unwrap_or_default(),
                    now,
                    now_ts,
                    position,
                    layout_mode,
                    filter_mode: tmux_state.prefs.filter_mode(),
                    sort,
                    group_by,
                    collapse_stale,
                    stale_threshold_secs,
                    expanded_groups: tmux_state.prefs.expanded_groups.clone(),
                    git_statuses: snapshot_status_cache(git_status_enabled, &git_cache),
                    pr_statuses: snapshot_status_cache(git_status_enabled, &pr_cache),
                    check_statuses: snapshot_status_cache(git_status_enabled, &check_cache),
                    sleeping_pane_ids: tmux_state.prefs.sleeping_panes.clone(),
                },
                &mut inactivity_tracker,
                &last_interrupted,
                &status_icons,
                false,
            );

            if apply_tick_effects(&output, &store, &backend_name, &instance_id) {
                scheduler.finish_heartbeat(Instant::now(), true);
            }
            last_interrupted = output.next_interrupted;
            output.snapshot.config_version = config_version.load(Ordering::Relaxed);
            {
                let cfg = config.lock().unwrap();
                let views = cfg.sidebar.views.as_deref().unwrap_or_default();
                output.snapshot.window_views = view_projects.window_views(
                    views,
                    &output.snapshot.agents,
                    &tmux_state.live_panes,
                    &tmux_state.active_pane_ids,
                    tmux_state.prefs.views_enabled(),
                );
            }
            sidebar_store.set_window_views(
                &mut published_views,
                tmux_state.window_pane_counts.keys(),
                &output.snapshot.window_views,
            );
            server.broadcast(&output.snapshot);

            let stale_threshold = output.snapshot.stale_threshold_secs;
            let entries: Vec<GitWorkerPath> = if git_status_enabled {
                output
                    .snapshot
                    .agents
                    .iter()
                    .filter(|agent| agent.remote.is_none())
                    .map(|agent| GitWorkerPath {
                        path: agent.path.clone(),
                        is_stale: agent
                            .activity_ts()
                            .map(|ts| now_ts.saturating_sub(ts) > stale_threshold)
                            .unwrap_or(false),
                        is_focused: output.snapshot.active_pane_ids.contains(&agent.pane_id)
                            || (!agent.window_id.is_empty()
                                && output
                                    .snapshot
                                    .active_windows
                                    .contains(&(agent.session.clone(), agent.window_id.clone()))),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let _ = git_path_tx.send(entries);

            let github_entries: Vec<GithubWorkerPath> = if git_status_enabled {
                output
                    .snapshot
                    .agents
                    .iter()
                    .filter_map(|agent| {
                        let branch = output
                            .snapshot
                            .git_statuses
                            .get(&agent.path)?
                            .branch
                            .as_ref()?;
                        Some(GithubWorkerPath {
                            path: agent.path.clone(),
                            branch: branch.clone(),
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let _ = github_path_tx.send(github_entries);

            // Navigation reaches live work: an agent nobody is waiting on is
            // not worth a hotkey, folded away or not.
            let snapshot = &output.snapshot;
            let agent_list = snapshot
                .agents
                .iter()
                .filter(|agent| super::snapshot::is_jump_target(snapshot, agent))
                .map(|agent| agent.pane_id.as_str())
                .collect::<Vec<_>>();
            if agent_list != last_agent_list {
                if let Err(error) = sidebar_store.set_ordered_agents(&agent_list) {
                    tracing::warn!(%error, "failed to publish sidebar agent inventory");
                }
                last_agent_list = agent_list.into_iter().map(String::from).collect();
            }
        }

        let now = Instant::now();
        if scheduler.heartbeat_due(now) {
            let success = StateStore::new()
                .and_then(|store| {
                    store.write_runtime(
                        &backend_name,
                        &instance_id,
                        &crate::state::RuntimeState {
                            interrupted_pane_ids: last_interrupted.clone(),
                            updated_ts: SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                        },
                    )
                })
                .is_ok();
            scheduler.finish_heartbeat(Instant::now(), success);
        }

        let now = Instant::now();
        if scheduler.maintenance_due(now) {
            scheduler.finish_maintenance(now);
            let client_count = server.client_count();
            if matches!(host, SidebarHost::Rex { .. }) {
                // Rex sidebars are placed by this daemon, and Rex has no hooks
                // to start a daemon when a window needs one. So unlike the
                // tmux and Zellij daemons, which exit 10s after their last
                // client leaves, it runs while the scope covers anything,
                // sidebars or not.
                if super::session_state::read(&instance_id).is_ok_and(|state| !state.is_active()) {
                    tracing::info!("sidebar daemon exiting: sidebar turned off");
                    break;
                }
                // A server restart keeps the scope, and sidebars Rex brings
                // back start a new daemon.
                if now.duration_since(last_observed) > REX_SERVER_GRACE {
                    tracing::info!("sidebar daemon exiting: Rex server unreachable");
                    break;
                }
            } else if client_count > 0 {
                last_client_seen = now;
            } else if now.duration_since(last_client_seen) > Duration::from_secs(10) {
                tracing::info!("sidebar daemon exiting: no clients for 10s");
                break;
            }
            if now.duration_since(last_health_log) >= Duration::from_secs(60) {
                tracing::info!(clients = client_count, "sidebar daemon alive");
                last_health_log = now;
            }
        }

        let _ = wake_rx.recv_timeout(scheduler.wait(Instant::now()));
    }

    if term.load(Ordering::Relaxed) {
        tracing::info!("sidebar daemon exiting: SIGTERM received");
    }
    signal_handle.close();
    let _ = signal_thread.join();
    if let Some(follower) = rex_events {
        follower.stop();
    }
    let _ = std::fs::remove_file(&sock_path);
    if let Ok(store) = StateStore::new() {
        store.delete_runtime(&backend_name, &instance_id);
    }
    sidebar_store.clear_runtime();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::multiplexer::{AgentPane, AgentStatus};

    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    #[test]
    fn socket_path_uses_a_stable_fixed_width_instance_key() {
        let default = socket_path("/private/tmp/tmux-501/default");
        let long = socket_path("/private/tmp/tmux-501/sidebar-repro-socket");
        let unicode = socket_path("/private/tmp/tmux-501/サイドバー");

        assert_eq!(default.parent(), Some(std::env::temp_dir().as_path()));
        assert_eq!(
            default.file_name().unwrap(),
            "workmux-sidebar-3d3d4a9387e30f49.sock"
        );
        assert_eq!(
            long.file_name().unwrap(),
            "workmux-sidebar-51c46e2d6ee045f1.sock"
        );
        assert_eq!(
            unicode.file_name().unwrap(),
            "workmux-sidebar-b596df9918d556c8.sock"
        );
    }

    #[test]
    fn socket_path_for_a_long_instance_can_be_bound() {
        let unique = tempfile::tempdir().unwrap();
        let instance_id = format!(
            "/private/tmp/tmux-501/{}/{}",
            unique.path().display(),
            "long-unicode-name-サイドバー".repeat(8)
        );
        let path = socket_path(&instance_id);

        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
        std::fs::remove_file(path).unwrap();
    }

    pub(super) fn working_agent(pane_id: &str, updated_ts: u64) -> AgentPane {
        AgentPane {
            session: String::new(),
            window_name: String::new(),
            pane_id: pane_id.to_string(),
            window_id: String::new(),
            window_index: None,
            path: PathBuf::new(),
            pane_title: None,
            status: Some(AgentStatus::Working),
            status_ts: Some(100),
            activity_ts: Some(100),
            updated_ts: Some(updated_ts),
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
            prompt: None,
            remote: None,
        }
    }

    pub(super) fn done_agent(pane_id: &str) -> AgentPane {
        AgentPane {
            status: Some(AgentStatus::Done),
            ..working_agent(pane_id, 1)
        }
    }
}
