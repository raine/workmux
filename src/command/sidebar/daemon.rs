//! Sidebar daemon: single process that polls tmux and pushes snapshots to clients.

use anyhow::Result;
use ignore::gitignore::Gitignore;
use notify::{EventKindMask, RecursiveMode, Watcher};
use signal_hook::iterator::{Handle as SignalHandle, Signals};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{Config, SidebarPosition};
use crate::git::GitStatus;
use crate::github::{CheckSummary, PrSummary};
use crate::multiplexer::{LivePaneInfo, Multiplexer, TmuxBackend, create_backend, detect_backend};
use crate::state::StateStore;

use super::app::{SidebarFilterMode, SidebarLayoutMode};
use super::snapshot::{CheckPathEntry, PrPathEntry, SnapshotInputs, build_snapshot};

/// Compute socket path from instance_id.
pub fn socket_path(instance_id: &str) -> PathBuf {
    let safe_id = instance_id.replace(['/', '\\'], "-");
    std::env::temp_dir().join(format!("workmux-sidebar-{}.sock", safe_id))
}

/// Result of a batched tmux query.
#[derive(Clone)]
struct TmuxState {
    live_panes: HashMap<String, LivePaneInfo>,
    window_statuses: HashMap<String, Option<String>>,
    active_windows: HashSet<(String, String)>,
    pane_window_ids: HashMap<String, String>,
    pane_window_indexes: HashMap<String, u32>,
    active_pane_ids: HashSet<String>,
    window_pane_counts: HashMap<String, usize>,
    server_boot_id: Option<String>,
    position: Option<String>,
    layout: Option<String>,
    filter: Option<String>,
    sleeping_panes: Option<String>,
    group_by: Option<String>,
    expanded_groups: Option<String>,
}

/// Query all sidebar-relevant tmux state in a single server observation.
fn query_tmux_state(tmux: &TmuxBackend) -> Result<TmuxState> {
    let snapshot = tmux.sidebar_snapshot()?;
    Ok(TmuxState {
        live_panes: snapshot.live_panes,
        window_statuses: snapshot.window_statuses,
        active_windows: snapshot.active_windows,
        pane_window_ids: snapshot.pane_window_ids,
        pane_window_indexes: snapshot.pane_window_indexes,
        active_pane_ids: snapshot.active_pane_ids,
        window_pane_counts: snapshot.window_pane_counts,
        server_boot_id: snapshot.server_boot_id,
        position: snapshot.position,
        layout: snapshot.layout,
        filter: snapshot.filter,
        sleeping_panes: snapshot.sleeping_panes,
        group_by: snapshot.group_by,
        expanded_groups: snapshot.expanded_groups,
    })
}

struct BroadcastCache {
    snapshot: Option<super::snapshot::SidebarSnapshot>,
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

fn snapshots_equal(
    left: &super::snapshot::SidebarSnapshot,
    right: &super::snapshot::SidebarSnapshot,
) -> bool {
    let super::snapshot::SidebarSnapshot {
        position: _,
        layout_mode: _,
        filter_mode: _,
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
        agents: _,
        config_version: _,
    } = left;

    left.position == right.position
        && left.layout_mode == right.layout_mode
        && left.filter_mode == right.filter_mode
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
struct SocketServer {
    state: Arc<Mutex<SocketState>>,
}

impl SocketServer {
    fn bind(path: &Path) -> std::io::Result<Self> {
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
    fn broadcast(&self, snapshot: &super::snapshot::SidebarSnapshot) -> bool {
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

    fn client_count(&self) -> usize {
        let mut state = self.state.lock().unwrap();
        state.clients.retain(stream_is_connected);
        state.clients.len()
    }
}

/// Read the sidebar layout mode from tmux global, falling back to settings.json, then config.
fn read_sidebar_layout_mode(
    config: &Config,
    tmux_value: Option<&str>,
) -> Option<SidebarLayoutMode> {
    match tmux_value.map(str::trim) {
        Some("tiles") => return Some(SidebarLayoutMode::Tiles),
        Some("compact") => return Some(SidebarLayoutMode::Compact),
        _ => {}
    }

    // Fall back to persisted setting (user toggled layout in a previous tmux session)
    if let Ok(store) = StateStore::new()
        && let Ok(settings) = store.load_settings()
    {
        match settings.sidebar_layout.as_deref() {
            Some("tiles") => return Some(SidebarLayoutMode::Tiles),
            Some("compact") => return Some(SidebarLayoutMode::Compact),
            _ => {}
        }
    }

    // Fall back to config file
    match config.sidebar.layout.as_deref() {
        Some("tiles") => return Some(SidebarLayoutMode::Tiles),
        Some("compact") => return Some(SidebarLayoutMode::Compact),
        _ => {}
    }

    None
}

/// Read the sidebar filter mode from tmux global, falling back to settings.json.
fn read_sidebar_filter_mode(tmux_value: Option<&str>) -> SidebarFilterMode {
    if let Some(value) = tmux_value {
        return SidebarFilterMode::from_str(value);
    }

    // Fall back to persisted setting
    if let Ok(store) = StateStore::new()
        && let Ok(settings) = store.load_settings()
        && let Some(ref mode) = settings.sidebar_filter
    {
        return SidebarFilterMode::from_str(mode);
    }

    SidebarFilterMode::default()
}

/// Read the sidebar grouping from the tmux global override, falling back to
/// settings.json and then to the configured grouping. The override is what the
/// `t` key and `workmux sidebar group` write, so every client agrees.
fn read_sidebar_group_by(
    cfg: &Config,
    tmux_value: Option<&str>,
) -> Option<crate::config::SidebarGroupBy> {
    if let Some(value) = tmux_value {
        return super::parse_sidebar_group_by(value).unwrap_or(cfg.sidebar.group_by());
    }

    if let Ok(store) = StateStore::new()
        && let Ok(settings) = store.load_settings()
        && let Some(ref mode) = settings.sidebar_group_by
    {
        return super::parse_sidebar_group_by(mode).unwrap_or(cfg.sidebar.group_by());
    }

    cfg.sidebar.group_by()
}

/// Group labels the user expanded, from the tmux global option. Labels are
/// tab separated because a project or session name can contain spaces.
fn read_expanded_groups(tmux_value: Option<&str>) -> Vec<String> {
    tmux_value
        .map(|value| {
            value
                .split('\t')
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Read pane IDs manually marked as sleeping from the tmux global option.
fn read_sleeping_panes(tmux_value: Option<&str>) -> HashSet<String> {
    tmux_value
        .map(|value| value.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

fn read_sidebar_position(config: &Config, tmux_value: Option<&str>) -> SidebarPosition {
    match tmux_value.map(str::trim) {
        Some("top") => SidebarPosition::Top,
        Some("left") => SidebarPosition::Left,
        _ => config.sidebar.position.unwrap_or_default(),
    }
}

/// Shared git status cache, updated by a background worker thread.
type GitCache = Arc<Mutex<HashMap<PathBuf, GitStatus>>>;

/// Resolve the .git directory for a worktree path.
/// For linked worktrees, .git is a file containing "gitdir: /path/to/real/gitdir".
fn resolve_git_dir(worktree_path: &Path) -> Option<PathBuf> {
    let dot_git = worktree_path.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    if dot_git.is_file() {
        // Linked worktree: read the gitdir pointer
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let gitdir = content.strip_prefix("gitdir: ")?.trim();
        let path = PathBuf::from(gitdir);
        if path.is_absolute() {
            return Some(path);
        }
        // Relative path: resolve relative to worktree
        Some(worktree_path.join(path))
    } else {
        None
    }
}

/// Resolve the common git directory for linked worktrees.
/// Returns None for normal (non-linked) worktrees.
fn resolve_common_git_dir(gitdir: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let rel = content.trim();
    let path = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        gitdir.join(rel)
    };
    path.canonicalize().ok().or(Some(path))
}

/// Build a gitignore matcher for a worktree root.
/// Loads the root .gitignore (covers the vast majority of ignored paths like
/// target/, node_modules/, .venv/, build/, etc.) without needing to walk
/// nested .gitignore files.
fn build_gitignore(worktree: &Path) -> Gitignore {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(worktree);
    if let Some(err) = builder.add(worktree.join(".gitignore")) {
        tracing::debug!(
            "failed to parse .gitignore for {}: {}",
            worktree.display(),
            err
        );
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Find tracked files that match ignore rules so their mutation events remain visible.
fn load_ignored_tracked_paths(worktree: &Path) -> HashSet<PathBuf> {
    let Ok(mut command) = crate::git::unattended_git(Some(worktree)) else {
        return HashSet::new();
    };
    let Ok(output) = command
        .args(["ls-files", "-ci", "--exclude-standard", "-z"])
        .output()
    else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| worktree.join(String::from_utf8_lossy(path).as_ref()))
        .collect()
}

/// Check if a filesystem event path should be skipped based on gitignore rules.
/// Returns true if the path is inside a .git directory (non-working-tree change)
/// or matches the worktree's .gitignore patterns.
fn is_event_ignored(
    event_path: &Path,
    worktree: &Path,
    gitignores: &HashMap<PathBuf, Gitignore>,
    ignored_tracked_paths: &HashMap<PathBuf, HashSet<PathBuf>>,
) -> bool {
    // Linked-worktree git metadata events (e.g. shared gitdir, common refs)
    // live outside the worktree root. They are git events, not working-tree
    // files, so they should never be ignored.
    let Ok(rel) = event_path.strip_prefix(worktree) else {
        return false;
    };

    let rel_str = rel.to_string_lossy();
    // Always process .git metadata changes (HEAD, index, refs) - they affect git status
    if rel_str.starts_with(".git/") || rel_str == ".git" {
        // Skip .git/objects and .git/logs (high volume, don't affect status)
        // but allow .git/index, .git/HEAD, .git/refs, etc.
        return rel_str.starts_with(".git/objects/") || rel_str.starts_with(".git/logs/");
    }

    if ignored_tracked_paths
        .get(worktree)
        .is_some_and(|paths| paths.contains(event_path))
    {
        return false;
    }

    if let Some(gi) = gitignores.get(worktree) {
        // Pass false for is_dir to avoid a synchronous stat syscall per event.
        // Directory-level ignore rules (e.g. "target/") still match because
        // matched_path_or_any_parents checks ancestor components.
        gi.matched_path_or_any_parents(event_path, false)
            .is_ignore()
    } else {
        false
    }
}

/// Compare sidebar-visible Git state while ignoring cache freshness.
fn git_status_semantically_equal(a: &GitStatus, b: &GitStatus) -> bool {
    let GitStatus {
        ahead: _,
        behind: _,
        has_conflict: _,
        is_dirty: _,
        lines_added: _,
        lines_removed: _,
        uncommitted_added: _,
        uncommitted_removed: _,
        cached_at: _,
        base_branch: _,
        branch: _,
        has_upstream: _,
        is_rebasing: _,
    } = a;

    a.ahead == b.ahead
        && a.behind == b.behind
        && a.has_conflict == b.has_conflict
        && a.is_dirty == b.is_dirty
        && a.lines_added == b.lines_added
        && a.lines_removed == b.lines_removed
        && a.uncommitted_added == b.uncommitted_added
        && a.uncommitted_removed == b.uncommitted_removed
        && a.base_branch == b.base_branch
        && a.branch == b.branch
        && a.has_upstream == b.has_upstream
        && a.is_rebasing == b.is_rebasing
}

/// Find which worktrees are affected by a filesystem event at the given path.
fn find_worktrees_for_path(
    event_path: &Path,
    watch_to_worktrees: &HashMap<PathBuf, HashSet<PathBuf>>,
) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for (watched_dir, worktrees) in watch_to_worktrees {
        if event_path.starts_with(watched_dir) {
            result.extend(worktrees.iter().cloned());
        }
    }
    result
}

/// Register a watch path and associate it with a worktree.
/// If the path is already watched by another worktree, just adds the mapping.
/// Only records the mapping after the OS watch succeeds (or was already active).
fn add_watch(
    watcher: &mut notify::RecommendedWatcher,
    path: &Path,
    mode: RecursiveMode,
    worktree: &Path,
    watch_to_worktrees: &mut HashMap<PathBuf, HashSet<PathBuf>>,
) -> bool {
    let already_watching = watch_to_worktrees.get(path).is_some_and(|s| !s.is_empty());

    if !already_watching && let Err(e) = watcher.watch(path, mode) {
        tracing::warn!("failed to watch {}: {}", path.display(), e);
        return false;
    }

    watch_to_worktrees
        .entry(path.to_path_buf())
        .or_default()
        .insert(worktree.to_path_buf());
    true
}

/// Remove watch association for a worktree. Unwatches the path if no other worktree needs it.
fn remove_worktree_watch(
    watcher: &mut notify::RecommendedWatcher,
    watch_path: &Path,
    worktree: &Path,
    watch_to_worktrees: &mut HashMap<PathBuf, HashSet<PathBuf>>,
) {
    if let Some(worktrees) = watch_to_worktrees.get_mut(watch_path) {
        worktrees.remove(worktree);
        if worktrees.is_empty() {
            watch_to_worktrees.remove(watch_path);
            let _ = watcher.unwatch(watch_path);
        }
    }
}

/// Whether the platform can handle recursive worktree watches efficiently.
/// macOS FSEvents aggregates events at the directory level in the kernel and
/// handles heavy I/O well. Linux inotify sets a watch per directory and
/// generates an event per file operation, which overwhelms the system under
/// heavy AI/MCP file activity.
fn platform_supports_worktree_watches() -> bool {
    cfg!(target_os = "macos")
}

#[derive(Debug)]
struct WorktreeWatchSpec {
    path: PathBuf,
    mode: RecursiveMode,
}

/// Describe the filesystem watches needed for a verified worktree root.
fn worktree_watch_specs(worktree: &Path, watch_worktree_files: bool) -> Vec<WorktreeWatchSpec> {
    let mut specs = Vec::new();
    let dot_git = worktree.join(".git");

    if dot_git.is_file() {
        if let Some(git_dir) = resolve_git_dir(worktree) {
            specs.push(WorktreeWatchSpec {
                path: git_dir.clone(),
                mode: RecursiveMode::NonRecursive,
            });

            if let Some(common_dir) = resolve_common_git_dir(&git_dir) {
                let refs_dir = common_dir.join("refs");
                if refs_dir.is_dir() {
                    specs.push(WorktreeWatchSpec {
                        path: refs_dir,
                        mode: RecursiveMode::Recursive,
                    });
                }
                specs.push(WorktreeWatchSpec {
                    path: common_dir,
                    mode: RecursiveMode::NonRecursive,
                });
            }
        }
    } else if dot_git.is_dir() {
        specs.push(WorktreeWatchSpec {
            path: dot_git.clone(),
            mode: RecursiveMode::NonRecursive,
        });
        let refs_dir = dot_git.join("refs");
        if refs_dir.is_dir() {
            specs.push(WorktreeWatchSpec {
                path: refs_dir,
                mode: RecursiveMode::Recursive,
            });
        }
    }

    if watch_worktree_files {
        specs.push(WorktreeWatchSpec {
            path: worktree.to_path_buf(),
            mode: RecursiveMode::Recursive,
        });
    }

    specs
}

/// Set up filesystem watches for a verified worktree root.
///
/// Git metadata watches detect commits, staging, and branch changes. macOS also
/// watches worktree files recursively, while Linux detects them through polling.
fn setup_worktree_watches(
    watcher: &mut notify::RecommendedWatcher,
    worktree: &Path,
    watch_to_worktrees: &mut HashMap<PathBuf, HashSet<PathBuf>>,
) -> (Vec<PathBuf>, bool) {
    let mut watched = Vec::new();
    let specs = worktree_watch_specs(worktree, platform_supports_worktree_watches());
    let mut complete = !specs.is_empty();
    for spec in specs {
        if add_watch(watcher, &spec.path, spec.mode, worktree, watch_to_worktrees) {
            watched.push(spec.path);
        } else {
            complete = false;
        }
    }
    (watched, complete)
}

fn detach_worktree_watches(
    watcher: &mut notify::RecommendedWatcher,
    worktree: &Path,
    worktree_watches: &mut HashMap<PathBuf, Vec<PathBuf>>,
    watch_to_worktrees: &mut HashMap<PathBuf, HashSet<PathBuf>>,
) {
    if let Some(watched_paths) = worktree_watches.remove(worktree) {
        for watched_path in watched_paths {
            remove_worktree_watch(watcher, &watched_path, worktree, watch_to_worktrees);
        }
    }
}

fn replace_worktree_watches(
    watcher: &mut notify::RecommendedWatcher,
    worktree: &Path,
    worktree_watches: &mut HashMap<PathBuf, Vec<PathBuf>>,
    watch_complete: &mut HashMap<PathBuf, bool>,
    watch_to_worktrees: &mut HashMap<PathBuf, HashSet<PathBuf>>,
) {
    detach_worktree_watches(watcher, worktree, worktree_watches, watch_to_worktrees);
    let (watched, complete) = setup_worktree_watches(watcher, worktree, watch_to_worktrees);
    worktree_watches.insert(worktree.to_path_buf(), watched);
    watch_complete.insert(worktree.to_path_buf(), complete);
}

/// Reconcile root watches, preserving watches for repositories that remain active.
/// Returns the roots whose watches were added.
fn reconcile_worktree_watches(
    watcher: &mut notify::RecommendedWatcher,
    active_roots: &[PathBuf],
    worktree_watches: &mut HashMap<PathBuf, Vec<PathBuf>>,
    watch_complete: &mut HashMap<PathBuf, bool>,
    watch_to_worktrees: &mut HashMap<PathBuf, HashSet<PathBuf>>,
) -> Vec<PathBuf> {
    let active: HashSet<&PathBuf> = active_roots.iter().collect();
    let removed: Vec<PathBuf> = worktree_watches
        .keys()
        .filter(|root| !active.contains(*root))
        .cloned()
        .collect();
    for root in removed {
        detach_worktree_watches(watcher, &root, worktree_watches, watch_to_worktrees);
        watch_complete.remove(&root);
    }
    let mut added = Vec::new();
    for root in active_roots {
        if !worktree_watches.contains_key(root) {
            replace_worktree_watches(
                watcher,
                root,
                worktree_watches,
                watch_complete,
                watch_to_worktrees,
            );
            added.push(root.clone());
        }
    }
    added
}

/// Calculate the next timeout for debounced work, capped at one second so
/// termination and maintenance remain responsive.
fn next_worker_timeout(pending: &HashMap<PathBuf, Instant>, debounce: Duration) -> Duration {
    let now = Instant::now();
    let mut min_wait = Duration::from_secs(1);

    for last_event in pending.values() {
        let ready_at = *last_event + debounce;
        if ready_at <= now {
            return Duration::from_millis(1);
        }
        let wait = ready_at - now;
        if wait < min_wait {
            min_wait = wait;
        }
    }

    // Cap at 1s to check term flag periodically
    min_wait.min(Duration::from_secs(1))
}

/// Refresh git status once for a worktree and publish it for every agent path.
/// Returns true if any published status changed, ignoring cached_at.
fn refresh_git_status(worktree: &Path, agent_paths: &[PathBuf], cache: &GitCache) -> bool {
    let new_status = crate::git::get_git_status(worktree, None);
    let Ok(mut cache) = cache.lock() else {
        return true;
    };
    let mut changed = false;
    for path in agent_paths {
        if cache
            .get(path)
            .is_some_and(|old| git_status_semantically_equal(old, &new_status))
        {
            continue;
        }
        cache.insert(path.clone(), new_status.clone());
        changed = true;
    }
    changed
}

fn reconcile_git_cache(
    previous: &HashMap<PathBuf, ResolvedGitWorktree>,
    current: &HashMap<PathBuf, ResolvedGitWorktree>,
    cache: &mut HashMap<PathBuf, GitStatus>,
) -> (bool, Vec<PathBuf>) {
    let previous_roots: HashMap<&PathBuf, &PathBuf> = previous
        .iter()
        .flat_map(|(root, entry)| entry.agent_paths.iter().map(move |path| (path, root)))
        .collect();
    let current_roots: HashMap<&PathBuf, &PathBuf> = current
        .iter()
        .flat_map(|(root, entry)| entry.agent_paths.iter().map(move |path| (path, root)))
        .collect();
    let root_statuses: HashMap<PathBuf, GitStatus> = previous
        .iter()
        .filter_map(|(root, old)| {
            old.agent_paths
                .iter()
                .find_map(|path| cache.get(path).cloned())
                .map(|status| (root.clone(), status))
        })
        .collect();

    let before = cache.len();
    // A status belongs to a repository root, even when its agent path is unchanged.
    cache.retain(|path, _| {
        current_roots.contains_key(path) && previous_roots.get(path) == current_roots.get(path)
    });
    let mut changed = cache.len() != before;
    let mut missing = Vec::new();
    for (root, worktree) in current {
        if let Some(status) = root_statuses.get(root) {
            for path in &worktree.agent_paths {
                if !cache.contains_key(path) {
                    cache.insert(path.clone(), status.clone());
                    changed = true;
                }
            }
        } else {
            missing.push(root.clone());
        }
    }
    (changed, missing)
}

/// Info about an active agent path sent to the git worker.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct GitWorkerPath {
    path: PathBuf,
    is_stale: bool,
    is_focused: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct ResolvedGitWorktree {
    agent_paths: Vec<PathBuf>,
    is_stale: bool,
    is_focused: bool,
}

const GIT_ROOT_REVALIDATION_INTERVAL: Duration = Duration::from_secs(30);

/// Expire both positive and negative discoveries independently of agent updates.
fn expire_git_roots(
    roots_by_agent: &mut HashMap<PathBuf, Option<PathBuf>>,
    last_revalidation: &mut Instant,
    now: Instant,
) -> bool {
    if now.saturating_duration_since(*last_revalidation) < GIT_ROOT_REVALIDATION_INTERVAL {
        return false;
    }
    *last_revalidation = now;
    let expired = !roots_by_agent.is_empty();
    roots_by_agent.clear();
    expired
}

/// Resolve agent directories to verified worktree roots and group shared roots.
/// Paths outside a non-bare Git worktree are absent from the result.
fn resolve_git_worktrees_cached(
    entries: &[GitWorkerPath],
    roots_by_agent: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> HashMap<PathBuf, ResolvedGitWorktree> {
    let active_paths: HashSet<&PathBuf> = entries.iter().map(|entry| &entry.path).collect();
    roots_by_agent.retain(|path, _| active_paths.contains(path));

    let mut worktrees: HashMap<PathBuf, ResolvedGitWorktree> = HashMap::new();
    for entry in entries {
        let root = roots_by_agent.entry(entry.path.clone()).or_insert_with(|| {
            crate::git::get_repo_root_for(&entry.path)
                .ok()
                .map(|root| crate::util::canon_or_self(&root))
        });
        let Some(root) = root else {
            continue;
        };
        let worktree = worktrees
            .entry(root.clone())
            .or_insert_with(|| ResolvedGitWorktree {
                agent_paths: Vec::new(),
                is_stale: true,
                is_focused: false,
            });
        if !worktree.agent_paths.contains(&entry.path) {
            worktree.agent_paths.push(entry.path.clone());
        }
        worktree.is_stale &= entry.is_stale;
        worktree.is_focused |= entry.is_focused;
    }
    for worktree in worktrees.values_mut() {
        worktree.agent_paths.sort();
    }
    worktrees
}

/// Info about an active agent path sent to the GitHub worker.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct GithubWorkerPath {
    path: PathBuf,
    branch: String,
}

type PrPathCache = Arc<Mutex<HashMap<PathBuf, PrPathEntry>>>;
type PrRepoCache = Arc<Mutex<HashMap<PathBuf, HashMap<String, PrSummary>>>>;
type CheckPathCache = Arc<Mutex<HashMap<PathBuf, CheckPathEntry>>>;
type CheckRepoCache = Arc<Mutex<HashMap<PathBuf, HashMap<String, CheckSummary>>>>;

const GITHUB_FETCH_INTERVAL: Duration = Duration::from_secs(30);

fn github_fetch_due(branch_set_changed: bool, elapsed: Duration) -> bool {
    branch_set_changed || elapsed >= GITHUB_FETCH_INTERVAL
}

fn github_repo_key(path: &Path) -> Option<PathBuf> {
    crate::git::get_git_common_dir_in(Some(path))
        .ok()
        .and_then(|git_dir| git_dir.canonicalize().ok().or(Some(git_dir)))
}

fn group_github_branches(
    entries: &[GithubWorkerPath],
    repo_keys: &HashMap<PathBuf, PathBuf>,
) -> HashMap<PathBuf, (PathBuf, Vec<String>)> {
    let mut grouped = HashMap::new();
    for entry in entries {
        if let Some(repo_key) = repo_keys.get(&entry.path) {
            let (_, branches) = grouped
                .entry(repo_key.clone())
                .or_insert_with(|| (entry.path.clone(), Vec::new()));
            branches.push(entry.branch.clone());
        }
    }
    for (_, branches) in grouped.values_mut() {
        branches.sort();
        branches.dedup();
    }
    grouped
}

fn clear_pr_path_cache(path_cache: &PrPathCache) -> bool {
    if let Ok(mut cache) = path_cache.lock() {
        let changed = !cache.is_empty();
        cache.clear();
        changed
    } else {
        false
    }
}

fn clear_check_path_cache(path_cache: &CheckPathCache) -> bool {
    if let Ok(mut cache) = path_cache.lock() {
        let changed = !cache.is_empty();
        cache.clear();
        changed
    } else {
        false
    }
}

fn publish_pr_path_cache(
    entries: &[GithubWorkerPath],
    repo_keys: &HashMap<PathBuf, PathBuf>,
    repo_cache: &HashMap<PathBuf, HashMap<String, PrSummary>>,
    path_cache: &PrPathCache,
    dirty_flag: &Arc<AtomicBool>,
    wake_tx: &std::sync::mpsc::SyncSender<()>,
) {
    let mut next = HashMap::new();
    for entry in entries {
        if let Some(repo_root) = repo_keys.get(&entry.path)
            && let Some(pr) = repo_cache
                .get(repo_root)
                .and_then(|prs| prs.get(&entry.branch))
        {
            next.insert(
                entry.path.clone(),
                PrPathEntry {
                    branch: entry.branch.clone(),
                    summary: pr.clone(),
                },
            );
        }
    }
    let changed = if let Ok(mut cache) = path_cache.lock() {
        if *cache == next {
            false
        } else {
            *cache = next;
            true
        }
    } else {
        false
    };
    if changed {
        dirty_flag.store(true, Ordering::Relaxed);
        let _ = wake_tx.try_send(());
    }
}

fn publish_check_path_cache(
    entries: &[GithubWorkerPath],
    repo_keys: &HashMap<PathBuf, PathBuf>,
    repo_cache: &HashMap<PathBuf, HashMap<String, CheckSummary>>,
    path_cache: &CheckPathCache,
    dirty_flag: &Arc<AtomicBool>,
    wake_tx: &std::sync::mpsc::SyncSender<()>,
) {
    let mut next = HashMap::new();
    for entry in entries {
        if let Some(repo_root) = repo_keys.get(&entry.path)
            && let Some(checks) = repo_cache
                .get(repo_root)
                .and_then(|checks| checks.get(&entry.branch))
        {
            next.insert(
                entry.path.clone(),
                CheckPathEntry {
                    branch: entry.branch.clone(),
                    summary: checks.clone(),
                },
            );
        }
    }
    let changed = if let Ok(mut cache) = path_cache.lock() {
        if *cache == next {
            false
        } else {
            *cache = next;
            true
        }
    } else {
        false
    };
    if changed {
        dirty_flag.store(true, Ordering::Relaxed);
        let _ = wake_tx.try_send(());
    }
}

fn merge_github_outcome(
    mut prs: HashMap<String, PrSummary>,
    mut checks: HashMap<String, CheckSummary>,
    outcome: crate::github::BranchQueryOutcome,
) -> (HashMap<String, PrSummary>, HashMap<String, CheckSummary>) {
    prs.retain(|branch, _| outcome.requested.contains(branch));
    checks.retain(|branch, _| outcome.requested.contains(branch));
    for (branch, summary) in outcome.answered {
        if let Some(pr) = summary.pr {
            prs.insert(branch.clone(), pr);
        } else {
            prs.remove(&branch);
        }
        if let Some(check_summary) = summary.checks {
            checks.insert(branch, check_summary);
        } else {
            checks.remove(&branch);
        }
    }
    (prs, checks)
}

fn spawn_github_worker(
    term: Arc<AtomicBool>,
    dirty_flag: Arc<AtomicBool>,
    wake_tx: std::sync::mpsc::SyncSender<()>,
) -> (
    PrPathCache,
    CheckPathCache,
    std::sync::mpsc::Sender<Vec<GithubWorkerPath>>,
) {
    let path_cache: PrPathCache = Arc::new(Mutex::new(HashMap::new()));
    let path_cache_clone = path_cache.clone();
    let check_path_cache: CheckPathCache = Arc::new(Mutex::new(HashMap::new()));
    let check_path_cache_clone = check_path_cache.clone();
    let repo_cache: PrRepoCache = Arc::new(Mutex::new(crate::github::load_pr_cache()));
    let check_repo_cache: CheckRepoCache = Arc::new(Mutex::new(crate::github::load_check_cache()));
    let (tx, rx) = std::sync::mpsc::channel::<Vec<GithubWorkerPath>>();

    thread::spawn(move || {
        let mut active_entries: Vec<GithubWorkerPath> = Vec::new();
        let mut repo_keys: HashMap<PathBuf, PathBuf> = HashMap::new();
        let mut last_key: Vec<(PathBuf, String)> = Vec::new();
        let mut last_fetch = Instant::now() - GITHUB_FETCH_INTERVAL;

        while !term.load(Ordering::Relaxed) {
            let mut paths_changed = false;
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(entries) => {
                    active_entries = entries;
                    paths_changed = true;
                    while let Ok(entries) = rx.try_recv() {
                        active_entries = entries;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if active_entries.is_empty() {
                if paths_changed
                    && (clear_pr_path_cache(&path_cache_clone)
                        | clear_check_path_cache(&check_path_cache_clone))
                {
                    dirty_flag.store(true, Ordering::Relaxed);
                    let _ = wake_tx.try_send(());
                }
                continue;
            }

            active_entries.sort();
            active_entries.dedup();
            let key: Vec<(PathBuf, String)> = active_entries
                .iter()
                .map(|entry| (entry.path.clone(), entry.branch.clone()))
                .collect();
            let branch_set_changed = key != last_key;

            if paths_changed {
                let active_paths: HashSet<&PathBuf> =
                    active_entries.iter().map(|entry| &entry.path).collect();
                repo_keys.retain(|path, _| active_paths.contains(path));
                for entry in &active_entries {
                    if !repo_keys.contains_key(&entry.path)
                        && let Some(repo_key) = github_repo_key(&entry.path)
                    {
                        repo_keys.insert(entry.path.clone(), repo_key);
                    }
                }
                let snapshot = repo_cache
                    .lock()
                    .ok()
                    .map(|c| c.clone())
                    .unwrap_or_default();
                publish_pr_path_cache(
                    &active_entries,
                    &repo_keys,
                    &snapshot,
                    &path_cache_clone,
                    &dirty_flag,
                    &wake_tx,
                );
                let check_snapshot = check_repo_cache
                    .lock()
                    .ok()
                    .map(|cache| cache.clone())
                    .unwrap_or_default();
                publish_check_path_cache(
                    &active_entries,
                    &repo_keys,
                    &check_snapshot,
                    &check_path_cache_clone,
                    &dirty_flag,
                    &wake_tx,
                );
            }

            if !github_fetch_due(branch_set_changed, last_fetch.elapsed()) {
                continue;
            }

            let repo_branches = group_github_branches(&active_entries, &repo_keys);
            if repo_branches.is_empty() {
                last_key = key;
                continue;
            }

            let requests = repo_branches
                .into_iter()
                .map(
                    |(repo_key, (repo_root, branches))| crate::github::BranchQueryRequest {
                        repo_key,
                        repo_root,
                        branches,
                    },
                )
                .collect();
            let outcomes = crate::github::list_branch_summaries_batch(requests);
            let previous_prs = repo_cache
                .lock()
                .ok()
                .map(|cache| cache.clone())
                .unwrap_or_default();
            let previous_checks = check_repo_cache
                .lock()
                .ok()
                .map(|cache| cache.clone())
                .unwrap_or_default();
            let mut fetched_prs = HashMap::new();
            let mut fetched_checks = HashMap::new();
            for (repo_key, outcome) in outcomes {
                let (prs, checks) = merge_github_outcome(
                    previous_prs.get(&repo_key).cloned().unwrap_or_default(),
                    previous_checks.get(&repo_key).cloned().unwrap_or_default(),
                    outcome,
                );
                fetched_prs.insert(repo_key.clone(), prs);
                fetched_checks.insert(repo_key, checks);
            }
            if !fetched_prs.is_empty()
                && let Ok(mut cache) = repo_cache.lock()
            {
                for (repo_root, prs) in &fetched_prs {
                    if prs.is_empty() {
                        cache.remove(repo_root);
                    } else {
                        cache.insert(repo_root.clone(), prs.clone());
                    }
                }
                crate::github::save_pr_cache(&fetched_prs);
                publish_pr_path_cache(
                    &active_entries,
                    &repo_keys,
                    &cache,
                    &path_cache_clone,
                    &dirty_flag,
                    &wake_tx,
                );
            }
            if !fetched_checks.is_empty()
                && let Ok(mut cache) = check_repo_cache.lock()
            {
                for (repo_root, checks) in &fetched_checks {
                    if checks.is_empty() {
                        cache.remove(repo_root);
                    } else {
                        cache.insert(repo_root.clone(), checks.clone());
                    }
                }
                crate::github::save_check_cache(&fetched_checks);
                publish_check_path_cache(
                    &active_entries,
                    &repo_keys,
                    &cache,
                    &check_path_cache_clone,
                    &dirty_flag,
                    &wake_tx,
                );
            }
            last_key = key;
            last_fetch = Instant::now();
        }
    });

    (path_cache, check_path_cache, tx)
}

/// Configure filesystem watchers for mutation events without read noise.
fn mutation_watcher_config() -> notify::Config {
    notify::Config::default().with_event_kinds(EventKindMask::CORE)
}

fn git_event_requires_recovery(event: &notify::Event) -> bool {
    event.need_rescan()
}

fn recovery_ready(due: bool, last_recovery: Instant, cooldown: Duration) -> bool {
    due && last_recovery.elapsed() >= cooldown
}

fn next_audit_path(
    paths: &[PathBuf],
    watch_complete: &HashMap<PathBuf, bool>,
    cursor: &mut usize,
) -> Option<PathBuf> {
    for _ in 0..paths.len() {
        *cursor %= paths.len();
        let path = paths[*cursor].clone();
        *cursor = (*cursor + 1) % paths.len();
        if watch_complete.get(&path).copied().unwrap_or(false) {
            return Some(path);
        }
    }
    None
}

/// Spawn a background thread that watches for git changes and updates the cache.
///
/// Uses the `notify` crate for OS-level filesystem event detection (FSEvents on macOS).
/// Watches Git metadata and, where efficient, worktree roots. Events are debounced
/// per worktree before status refresh. Polling covers platforms or roots without
/// complete worktree watches, and a rolling audit detects silent event loss.
fn spawn_git_worker(
    term: Arc<AtomicBool>,
    dirty_flag: Arc<AtomicBool>,
    wake_tx: std::sync::mpsc::SyncSender<()>,
) -> (GitCache, std::sync::mpsc::Sender<Vec<GitWorkerPath>>) {
    let cache: GitCache = Arc::new(Mutex::new(HashMap::new()));
    let cache_clone = cache.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<GitWorkerPath>>();

    thread::spawn(move || {
        // Bounded filesystem event channel to prevent unbounded memory growth
        // under heavy file I/O (e.g. MCP servers, Claude sessions).
        // On overflow, all worktrees are marked pending for an early refresh.
        let (fs_tx, fs_rx) = std::sync::mpsc::sync_channel(256);
        let fs_overflow = Arc::new(AtomicBool::new(false));
        let fs_overflow_clone = fs_overflow.clone();
        let mut watcher: Option<notify::RecommendedWatcher> = match notify::RecommendedWatcher::new(
            move |event: notify::Result<notify::Event>| {
                if let Ok(ref e) = event {
                    // Filter out .git internal traffic that doesn't affect status.
                    // Gitignore-based filtering (node_modules, target, etc.) happens
                    // in the worker thread where matchers are available.
                    let dominated_by_noise = e.paths.iter().all(|p| {
                        let s = p.to_string_lossy();
                        s.contains("/.git/objects/") || s.contains("/.git/logs/")
                    });
                    if dominated_by_noise {
                        return;
                    }
                }
                if let Err(std::sync::mpsc::TrySendError::Full(_)) = fs_tx.try_send(event) {
                    fs_overflow_clone.store(true, Ordering::Relaxed);
                }
            },
            mutation_watcher_config(),
        ) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!(
                    "filesystem watcher unavailable, falling back to polling: {}",
                    e
                );
                None
            }
        };

        let mut active_entries: Vec<GitWorkerPath> = Vec::new();
        let mut roots_by_agent: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
        let mut resolved_worktrees: HashMap<PathBuf, ResolvedGitWorktree> = HashMap::new();
        let mut watch_to_worktrees: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        let mut worktree_watches: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        let mut watch_complete: HashMap<PathBuf, bool> = HashMap::new();
        let mut gitignores: HashMap<PathBuf, Gitignore> = HashMap::new();
        let mut ignored_tracked_paths: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        let mut pending_worktrees: HashMap<PathBuf, Instant> = HashMap::new();
        let mut last_refreshed: HashMap<PathBuf, Instant> = HashMap::new();
        let mut unique_active: Vec<PathBuf> = Vec::new();
        let mut audit_cursor = 0usize;
        let mut last_maintenance = Instant::now();
        let mut last_audit = Instant::now();
        let mut last_recovery = Instant::now() - Duration::from_secs(30);
        let mut last_root_revalidation = Instant::now();
        let mut recovery_due = false;
        let mut watcher_degraded = watcher.is_none();
        let debounce_duration = Duration::from_millis(300);
        let min_refresh_interval = Duration::from_secs(2);
        let recovery_cooldown = Duration::from_secs(30);
        let audit_interval = Duration::from_secs(5);

        while !term.load(Ordering::Relaxed) {
            if watcher.is_some() {
                let timeout = next_worker_timeout(&pending_worktrees, debounce_duration);
                let mut process_event = |event: notify::Event| -> bool {
                    if git_event_requires_recovery(&event) {
                        return true;
                    }
                    for path in &event.paths {
                        let worktrees = find_worktrees_for_path(path, &watch_to_worktrees);
                        if path.file_name().is_some_and(|name| name == ".gitignore") {
                            for worktree in &worktrees {
                                gitignores.insert(worktree.clone(), build_gitignore(worktree));
                                ignored_tracked_paths
                                    .insert(worktree.clone(), load_ignored_tracked_paths(worktree));
                            }
                        }
                        for worktree in worktrees {
                            if is_event_ignored(
                                path,
                                &worktree,
                                &gitignores,
                                &ignored_tracked_paths,
                            ) {
                                continue;
                            }
                            pending_worktrees
                                .entry(worktree)
                                .or_insert_with(Instant::now);
                        }
                    }
                    false
                };

                match fs_rx.recv_timeout(timeout) {
                    Ok(Ok(event)) => recovery_due |= process_event(event),
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "filesystem watch error; using polling fallback");
                        watcher_degraded = true;
                        recovery_due = true;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                while let Ok(event_result) = fs_rx.try_recv() {
                    match event_result {
                        Ok(event) => recovery_due |= process_event(event),
                        Err(error) => {
                            tracing::warn!(%error, "filesystem watch error; using polling fallback");
                            watcher_degraded = true;
                            recovery_due = true;
                        }
                    }
                }
                if fs_overflow.swap(false, Ordering::Relaxed) {
                    recovery_due = true;
                }
            } else {
                thread::sleep(Duration::from_secs(1));
            }

            let roots_expired = expire_git_roots(
                &mut roots_by_agent,
                &mut last_root_revalidation,
                Instant::now(),
            );

            let mut latest_entries = None;
            while let Ok(entries) = rx.try_recv() {
                latest_entries = Some(entries);
            }
            let entries_changed = if let Some(mut entries) = latest_entries {
                entries.sort();
                let changed = entries != active_entries;
                active_entries = entries;
                changed
            } else {
                false
            };
            if entries_changed || roots_expired {
                let current = resolve_git_worktrees_cached(&active_entries, &mut roots_by_agent);
                if current != resolved_worktrees {
                    let previous = std::mem::replace(&mut resolved_worktrees, current);
                    unique_active = resolved_worktrees.keys().cloned().collect();
                    unique_active.sort();
                    let unique_set: HashSet<PathBuf> = unique_active.iter().cloned().collect();
                    gitignores.retain(|path, _| unique_set.contains(path));
                    ignored_tracked_paths.retain(|path, _| unique_set.contains(path));
                    pending_worktrees.retain(|path, _| unique_set.contains(path));
                    last_refreshed.retain(|path, _| unique_set.contains(path));
                    if let Some(ref mut active_watcher) = watcher {
                        let added = reconcile_worktree_watches(
                            active_watcher,
                            &unique_active,
                            &mut worktree_watches,
                            &mut watch_complete,
                            &mut watch_to_worktrees,
                        );
                        for path in added {
                            gitignores.insert(path.clone(), build_gitignore(&path));
                            ignored_tracked_paths
                                .insert(path.clone(), load_ignored_tracked_paths(&path));
                        }
                    }

                    let (projected_change, missing_roots) = cache_clone
                        .lock()
                        .ok()
                        .map(|mut cache| {
                            reconcile_git_cache(&previous, &resolved_worktrees, &mut cache)
                        })
                        .unwrap_or_default();
                    for root in missing_roots {
                        pending_worktrees.insert(root, Instant::now() - debounce_duration);
                    }
                    if projected_change {
                        dirty_flag.store(true, Ordering::Relaxed);
                        let _ = wake_tx.try_send(());
                    }

                    for (root, worktree) in &resolved_worktrees {
                        if let Some(old) = previous.get(root) {
                            let became_recent = old.is_stale && !worktree.is_stale;
                            let became_focused = !old.is_focused && worktree.is_focused;
                            if became_recent || became_focused {
                                pending_worktrees
                                    .insert(root.clone(), Instant::now() - debounce_duration);
                            }
                        }
                    }
                }
            }

            let now = Instant::now();
            if recovery_ready(recovery_due, last_recovery, recovery_cooldown) {
                recovery_due = false;
                last_recovery = now;
                for path in &unique_active {
                    pending_worktrees
                        .entry(path.clone())
                        .or_insert(now - debounce_duration);
                }
            }

            let poll_interval = if watcher_degraded || watcher.is_none() {
                Duration::from_secs(2)
            } else {
                Duration::from_secs(5)
            };
            if last_maintenance.elapsed() >= poll_interval {
                last_maintenance = now;
                if watcher.is_none() || !platform_supports_worktree_watches() || watcher_degraded {
                    for path in &unique_active {
                        pending_worktrees
                            .entry(path.clone())
                            .or_insert(now - debounce_duration);
                    }
                } else {
                    let incomplete: Vec<PathBuf> = unique_active
                        .iter()
                        .filter(|path| !watch_complete.get(*path).copied().unwrap_or(false))
                        .cloned()
                        .collect();
                    for path in &incomplete {
                        pending_worktrees
                            .entry(path.clone())
                            .or_insert(now - debounce_duration);
                    }
                    if let Some(ref mut active_watcher) = watcher {
                        for path in incomplete {
                            replace_worktree_watches(
                                active_watcher,
                                &path,
                                &mut worktree_watches,
                                &mut watch_complete,
                                &mut watch_to_worktrees,
                            );
                        }
                    }
                }
            }

            if watcher.is_some()
                && platform_supports_worktree_watches()
                && !watcher_degraded
                && last_audit.elapsed() >= audit_interval
                && !unique_active.is_empty()
            {
                last_audit = now;
                if let Some(path) =
                    next_audit_path(&unique_active, &watch_complete, &mut audit_cursor)
                {
                    pending_worktrees
                        .entry(path)
                        .or_insert(now - debounce_duration);
                }
            }

            let ready: Vec<PathBuf> = pending_worktrees
                .iter()
                .filter(|(_, event_at)| {
                    now.saturating_duration_since(**event_at) >= debounce_duration
                })
                .map(|(path, _)| path.clone())
                .collect();
            let mut any_changed = false;
            for path in ready {
                if let Some(last) = last_refreshed.get(&path)
                    && last.elapsed() < min_refresh_interval
                {
                    let ready_at = *last + min_refresh_interval;
                    let event_at = ready_at.checked_sub(debounce_duration).unwrap_or(ready_at);
                    pending_worktrees.insert(path, event_at);
                    continue;
                }
                pending_worktrees.remove(&path);
                if let Some(worktree) = resolved_worktrees.get(&path) {
                    if refresh_git_status(&path, &worktree.agent_paths, &cache_clone) {
                        any_changed = true;
                    }
                    last_refreshed.insert(path, Instant::now());
                }
            }

            if any_changed {
                dirty_flag.store(true, Ordering::Relaxed);
                let _ = wake_tx.try_send(());
            }
        }
    });

    (cache, tx)
}

const CONFIG_BASENAMES: [&str; 4] = ["config.yaml", "config.yml", ".workmux.yaml", ".workmux.yml"];

/// Whether a filesystem event on a watched config dir should schedule a
/// config reload.
///
/// Access events must be ignored because reading a config file produces them
/// on Linux. Reacting to those reads makes each reload schedule another one.
/// Rescan events reload defensively because their paths may be incomplete.
fn config_event_triggers_reload(event: &notify::Event) -> bool {
    if event.need_rescan() {
        return true;
    }

    !matches!(event.kind, notify::EventKind::Access(_))
        && event.paths.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| CONFIG_BASENAMES.contains(&n))
        })
}

/// Spawn a thread that watches the global config file and per-project
/// `.workmux.yaml` files and bumps `config_version` whenever a reload succeeds.
///
/// Returns a channel for the daemon main loop to send the current set of
/// project config directories (parents of `.workmux.yaml`) to watch.
fn spawn_config_watcher(
    term: Arc<AtomicBool>,
    config: Arc<Mutex<Config>>,
    config_version: Arc<AtomicU64>,
    dirty_flag: Arc<AtomicBool>,
    wake_tx: mpsc::SyncSender<()>,
) -> mpsc::Sender<HashSet<PathBuf>> {
    let (paths_tx, paths_rx) = mpsc::channel::<HashSet<PathBuf>>();
    thread::spawn(move || {
        // Bounded fs event channel; on overflow force a reload.
        let (fs_tx, fs_rx) = mpsc::sync_channel::<notify::Result<notify::Event>>(64);
        let overflow = Arc::new(AtomicBool::new(false));
        let overflow_clone = overflow.clone();
        let mut watcher: notify::RecommendedWatcher = match notify::RecommendedWatcher::new(
            move |event: notify::Result<notify::Event>| {
                if event
                    .as_ref()
                    .is_ok_and(|event| !config_event_triggers_reload(event))
                {
                    return;
                }
                if let Err(mpsc::TrySendError::Full(_)) = fs_tx.try_send(event) {
                    overflow_clone.store(true, Ordering::Relaxed);
                }
            },
            mutation_watcher_config(),
        ) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("config watcher unavailable: {}", e);
                return;
            }
        };

        // Track watched directories so we can reconcile add/remove and avoid
        // re-watching the same path twice.
        let mut watched_global: Option<PathBuf> = None;
        let mut watched_project_dirs: HashSet<PathBuf> = HashSet::new();
        let mut pending_reload_at: Option<Instant> = None;
        let debounce = Duration::from_millis(200);

        // Watch the global config dir non-recursively. Watching the parent dir
        // (rather than the file) catches atomic-rename saves: write to a
        // sibling temp file, then rename(temp, target). This is what vim,
        // claude-code's Edit/Write tools, and most editors do. A direct file
        // watch would lose the inode on rename and miss subsequent edits. It
        // also fires on first-time creation when no config exists yet.
        if let Some(p) = crate::config::global_config_path()
            && let Some(dir) = p.parent()
        {
            match watcher.watch(dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    tracing::info!(
                        op = "watch",
                        path = %dir.display(),
                        kind = "global",
                        "fd-leak debug (config)"
                    );
                    watched_global = Some(dir.to_path_buf());
                }
                Err(e) => {
                    tracing::warn!("failed to watch global config dir {}: {}", dir.display(), e);
                }
            }
        }

        while !term.load(Ordering::Relaxed) {
            // 1. Reconcile per-project watches from incoming path sets.
            while let Ok(new_dirs) = paths_rx.try_recv() {
                let to_remove: Vec<PathBuf> = watched_project_dirs
                    .difference(&new_dirs)
                    .cloned()
                    .collect();
                for dir in &to_remove {
                    // Never unwatch the global config dir, even if it was
                    // tracked under watched_project_dirs (we never issued an
                    // OS-level watch for it from the project path; it's still
                    // watched as the global watch).
                    if Some(dir) == watched_global.as_ref() {
                        watched_project_dirs.remove(dir);
                        continue;
                    }
                    let res = watcher.unwatch(dir);
                    tracing::info!(
                        op = "unwatch",
                        path = %dir.display(),
                        ok = res.is_ok(),
                        kind = "project",
                        total = watched_project_dirs.len() - 1,
                        "fd-leak debug (config)"
                    );
                    watched_project_dirs.remove(dir);
                }
                let to_add: Vec<PathBuf> = new_dirs
                    .difference(&watched_project_dirs)
                    .cloned()
                    .collect();
                for dir in to_add {
                    // Skip if it's the same as the global watched dir to avoid
                    // double-watching the same path.
                    if Some(&dir) == watched_global.as_ref() {
                        watched_project_dirs.insert(dir);
                        continue;
                    }
                    match watcher.watch(&dir, RecursiveMode::NonRecursive) {
                        Ok(()) => {
                            tracing::info!(
                                op = "watch",
                                path = %dir.display(),
                                kind = "project",
                                total = watched_project_dirs.len() + 1,
                                "fd-leak debug (config)"
                            );
                            watched_project_dirs.insert(dir);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "failed to watch project config dir {}: {}",
                                dir.display(),
                                e
                            );
                        }
                    }
                }
            }

            // 2. Wait for the next event, capped by the pending debounce deadline.
            let timeout = pending_reload_at
                .map(|t| t.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|| Duration::from_millis(500));

            match fs_rx.recv_timeout(timeout) {
                Ok(Ok(event)) => {
                    if config_event_triggers_reload(&event) {
                        // Lock the deadline on the FIRST event in a burst; do
                        // not slide it forward on every subsequent event.
                        pending_reload_at.get_or_insert(Instant::now() + debounce);
                    }
                }
                Ok(Err(e)) => tracing::warn!("config watch error: {}", e),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if overflow.swap(false, Ordering::Relaxed) {
                pending_reload_at.get_or_insert(Instant::now() + debounce);
            }

            // 3. Reload if the debounce deadline has passed.
            if let Some(t) = pending_reload_at
                && Instant::now() >= t
            {
                pending_reload_at = None;
                // Always bump the version so clients try their own per-project
                // load (their anchor path may differ from the daemon CWD; a
                // failure here doesn't necessarily mean clients will fail).
                // Only update the daemon-side cached Config on success.
                match Config::load(None) {
                    Ok(new_cfg) => {
                        if let Ok(mut slot) = config.lock() {
                            *slot = new_cfg;
                        }
                        tracing::debug!("daemon config reloaded");
                    }
                    Err(e) => {
                        tracing::warn!("daemon-side config load failed, keeping previous: {}", e);
                    }
                }
                let v = config_version.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::info!(version = v, "sidebar config_version bumped");
                dirty_flag.store(true, Ordering::Relaxed);
                let _ = wake_tx.try_send(());
            }
        }
    });

    paths_tx
}

/// Detects working agents that have stopped producing output.
///
/// # Behavior
/// - A working agent with no pane output and no RPC activity for >= timeout
///   is considered interrupted.
/// - Interrupted state is sticky: only an RPC update from the agent clears
///   it. User typing or cursor movement in the pane does not.
/// - After clearing, the agent gets a fresh timeout window before it can
///   be marked interrupted again.
/// - Interrupted agents show no icon and no timer in the sidebar.
/// - When an agent resumes, the timer resets to zero.
struct InactivityTracker {
    /// pane_id -> (server lifecycle, pane PID)
    identities: HashMap<String, (Option<String>, u32)>,
    /// pane_id -> (content_hash, first_seen_at, updated_ts at recording time)
    entries: HashMap<String, (u64, Instant, u64)>,
    /// pane_id -> updated_ts at the time interruption was confirmed.
    /// Cleared when updated_ts changes (agent sent a new RPC status update).
    confirmed: HashMap<String, u64>,
    /// How long content must be unchanged before marking as interrupted.
    timeout: Duration,
}

impl InactivityTracker {
    fn new(timeout: Duration) -> Self {
        Self {
            identities: HashMap::new(),
            entries: HashMap::new(),
            confirmed: HashMap::new(),
            timeout,
        }
    }

    fn reconcile_identities(
        &mut self,
        live_panes: &HashMap<String, LivePaneInfo>,
        server_boot_id: Option<&str>,
    ) {
        let current: HashMap<String, (Option<String>, u32)> = live_panes
            .iter()
            .filter_map(|(pane_id, pane)| {
                pane.pid
                    .map(|pid| (pane_id.clone(), (server_boot_id.map(str::to_string), pid)))
            })
            .collect();
        for (pane_id, identity) in &current {
            if self
                .identities
                .get(pane_id)
                .is_some_and(|previous| previous != identity)
            {
                self.entries.remove(pane_id);
                self.confirmed.remove(pane_id);
            }
        }
        self.entries
            .retain(|pane_id, _| current.contains_key(pane_id));
        self.confirmed
            .retain(|pane_id, _| current.contains_key(pane_id));
        self.identities = current;
    }

    /// Whether this pane is confirmed interrupted and capture can be skipped.
    fn is_confirmed(&self, pane_id: &str, updated_ts: u64) -> bool {
        self.confirmed
            .get(pane_id)
            .is_some_and(|&ts| updated_ts <= ts)
    }

    /// Check all working agents for inactivity. Returns the set of pane IDs
    /// that appear interrupted (content unchanged for longer than timeout).
    fn check_with(
        &mut self,
        agents: &[crate::multiplexer::AgentPane],
        now: Instant,
        capture: impl Fn(&str) -> Option<String>,
    ) -> HashSet<String> {
        use std::hash::{Hash, Hasher};

        // Build lookup of working agents
        let working: HashMap<&str, &crate::multiplexer::AgentPane> = agents
            .iter()
            .filter(|a| a.status == Some(crate::multiplexer::AgentStatus::Working))
            .map(|a| (a.pane_id.as_str(), a))
            .collect();

        // Remove entries for agents no longer in Working status
        self.entries
            .retain(|id, _| working.contains_key(id.as_str()));
        self.confirmed
            .retain(|id, _| working.contains_key(id.as_str()));

        // Clear interrupted state if the agent's state was updated via RPC
        // (updated_ts changed since we confirmed the interruption).
        // Collect resumed pane IDs first, then clear their entries for a fresh
        // inactivity window.
        let resumed: Vec<String> = self
            .confirmed
            .iter()
            .filter(|(id, confirmed_ts)| {
                working
                    .get(id.as_str())
                    .is_some_and(|a| a.updated_ts.unwrap_or(0) > **confirmed_ts)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &resumed {
            if let Some(confirmed_ts) = self.confirmed.remove(id) {
                let updated_ts = working
                    .get(id.as_str())
                    .and_then(|a| a.updated_ts)
                    .unwrap_or(0);
                tracing::info!(
                    pane_id = %id,
                    confirmed_ts,
                    updated_ts,
                    "agent inactivity cleared"
                );
            }
            self.entries.remove(id);
        }

        for (pane_id, agent) in &working {
            // Already confirmed interrupted - skip capture
            if self.confirmed.contains_key(*pane_id) {
                continue;
            }

            let Some(raw) = capture(pane_id) else {
                continue;
            };

            // Strip ANSI escapes and normalize whitespace for stable hashing
            let stripped = console::strip_ansi_codes(&raw);
            let normalized = stripped.trim();

            let mut hasher = std::hash::DefaultHasher::new();
            normalized.hash(&mut hasher);
            let hash = hasher.finish();

            let current_rpc = agent.updated_ts.unwrap_or(0);

            match self.entries.get(*pane_id) {
                Some(&(prev_hash, first_seen, prev_rpc))
                    if prev_hash == hash && prev_rpc == current_rpc =>
                {
                    // Same content and same RPC state: check timeout
                    let idle_for = now.duration_since(first_seen);
                    if idle_for >= self.timeout
                        && self
                            .confirmed
                            .insert(pane_id.to_string(), current_rpc)
                            .is_none()
                    {
                        tracing::info!(
                            pane_id = %pane_id,
                            updated_ts = current_rpc,
                            idle_for_ms = idle_for.as_millis(),
                            timeout_ms = self.timeout.as_millis(),
                            "agent inactivity detected"
                        );
                    }
                }
                _ => {
                    // Content changed or RPC updated: reset inactivity window
                    self.entries
                        .insert(pane_id.to_string(), (hash, now, current_rpc));
                }
            }
        }

        self.confirmed.keys().cloned().collect()
    }
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

const OBSERVATION_INTERVAL: Duration = Duration::from_secs(2);
const CAPTURE_INTERVAL: Duration = Duration::from_secs(2);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const HEARTBEAT_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
const EVENT_COALESCE_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug)]
struct Scheduler {
    next_observation: Instant,
    next_capture: Instant,
    next_heartbeat: Instant,
    next_maintenance: Instant,
    event_observation: Option<Instant>,
}

fn advance_deadline(deadline: &mut Instant, interval: Duration, now: Instant) {
    while *deadline <= now {
        *deadline += interval;
    }
}

impl Scheduler {
    fn new(now: Instant) -> Self {
        Self {
            next_observation: now,
            next_capture: now,
            next_heartbeat: now + HEARTBEAT_INTERVAL,
            next_maintenance: now + MAINTENANCE_INTERVAL,
            event_observation: None,
        }
    }

    fn notify_tmux_event(&mut self, now: Instant) {
        self.event_observation
            .get_or_insert(now + EVENT_COALESCE_INTERVAL);
    }

    fn observation_due(&self, now: Instant) -> bool {
        self.next_observation <= now
            || self
                .event_observation
                .is_some_and(|deadline| deadline <= now)
    }

    fn finish_observation(&mut self, now: Instant) {
        if self.next_observation <= now {
            advance_deadline(&mut self.next_observation, OBSERVATION_INTERVAL, now);
        }
        self.event_observation = None;
    }

    fn capture_due(&self, now: Instant) -> bool {
        self.next_capture <= now
    }

    fn finish_capture(&mut self, now: Instant) {
        advance_deadline(&mut self.next_capture, CAPTURE_INTERVAL, now);
    }

    fn heartbeat_due(&self, now: Instant) -> bool {
        self.next_heartbeat <= now
    }

    fn finish_heartbeat(&mut self, now: Instant, success: bool) {
        self.next_heartbeat = now
            + if success {
                HEARTBEAT_INTERVAL
            } else {
                HEARTBEAT_RETRY_INTERVAL
            };
    }

    fn maintenance_due(&self, now: Instant) -> bool {
        self.next_maintenance <= now
    }

    fn finish_maintenance(&mut self, now: Instant) {
        advance_deadline(&mut self.next_maintenance, MAINTENANCE_INTERVAL, now);
    }

    fn wait(&self, now: Instant) -> Duration {
        [
            self.next_observation,
            self.next_capture,
            self.next_heartbeat,
            self.next_maintenance,
            self.event_observation.unwrap_or(self.next_observation),
        ]
        .into_iter()
        .min()
        .unwrap()
        .saturating_duration_since(now)
    }
}

/// Run the sidebar daemon (headless, no TUI).
pub fn run() -> Result<()> {
    let mux = create_backend(detect_backend());
    let instance_id = mux.instance_id();
    let tmux = TmuxBackend::for_socket(&instance_id);
    let config = Arc::new(Mutex::new(Config::load(None)?));
    // Captured at startup and intentionally not live-reloaded. tmux's
    // @workmux_pane_status holds the icon string itself; build_snapshot
    // compares pane statuses to these exact strings to suppress stale
    // done/waiting markers, so swapping the icons mid-run would mis-suppress.
    let status_icons = config.lock().unwrap().status_icons.clone();
    let config_version = Arc::new(AtomicU64::new(0));

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

    tmux.set_global_option(
        "@workmux_sidebar_daemon_pid",
        &std::process::id().to_string(),
    )?;

    let mut scheduler = Scheduler::new(Instant::now());
    let mut inactivity_tracker = InactivityTracker::new(Duration::from_secs(10));
    let mut last_interrupted: HashSet<String> = HashSet::new();
    let backend_name = mux.name().to_string();
    let store = StateStore::new()?;
    let mut agent_state_cache = crate::state::AgentStateCache::default();
    let mut compacted_boot_id: Option<String> = None;
    let mut cached_inputs: Option<(Vec<crate::multiplexer::AgentPane>, TmuxState)> = None;
    let mut pending_captures: Option<HashMap<String, String>> = None;
    let mut publish_pending = false;
    let mut last_client_seen = Instant::now();
    let mut last_agent_list = String::new();
    let mut last_health_log = Instant::now();
    let mut project_config_cache: HashMap<PathBuf, PathBuf> = HashMap::new();
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
            match query_tmux_state(&tmux) {
                Ok(tmux_state) => {
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
                Err(error) => tracing::warn!(%error, "failed to query sidebar tmux state"),
            }
        }

        let now = Instant::now();
        if scheduler.capture_due(now) {
            scheduler.finish_capture(now);
            if let Some((agents, _)) = &cached_inputs {
                pending_captures = Some(gather_captures(agents, mux.as_ref(), &inactivity_tracker));
                publish_pending = true;
            }
        }

        if publish_pending && let Some((agents, tmux_state)) = &cached_inputs {
            publish_pending = false;
            let (position, layout_mode, sort, group_by, collapse_stale) = {
                let cfg = config.lock().unwrap();
                (
                    read_sidebar_position(&cfg, tmux_state.position.as_deref()),
                    read_sidebar_layout_mode(&cfg, tmux_state.layout.as_deref())
                        .unwrap_or_default(),
                    cfg.sidebar.sort.unwrap_or_default(),
                    read_sidebar_group_by(&cfg, tmux_state.group_by.as_deref()),
                    cfg.sidebar.collapse_stale(),
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
                    filter_mode: read_sidebar_filter_mode(tmux_state.filter.as_deref()),
                    sort,
                    group_by,
                    collapse_stale,
                    expanded_groups: read_expanded_groups(tmux_state.expanded_groups.as_deref()),
                    git_statuses: git_cache.lock().ok().map(|c| c.clone()).unwrap_or_default(),
                    pr_statuses: pr_cache.lock().ok().map(|c| c.clone()).unwrap_or_default(),
                    check_statuses: check_cache
                        .lock()
                        .ok()
                        .map(|c| c.clone())
                        .unwrap_or_default(),
                    sleeping_pane_ids: read_sleeping_panes(tmux_state.sleeping_panes.as_deref()),
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
            server.broadcast(&output.snapshot);

            let stale_threshold = super::snapshot::STALE_THRESHOLD_SECS;
            let entries: Vec<GitWorkerPath> = output
                .snapshot
                .agents
                .iter()
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
                .collect();
            let _ = git_path_tx.send(entries);

            let github_entries: Vec<GithubWorkerPath> = output
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
                .collect();
            let _ = github_path_tx.send(github_entries);

            let live_paths: HashSet<PathBuf> = output
                .snapshot
                .agents
                .iter()
                .map(|agent| agent.path.clone())
                .collect();
            project_config_cache.retain(|path, _| live_paths.contains(path));
            let mut config_dirs = HashSet::new();
            for agent in &output.snapshot.agents {
                let dir = project_config_cache.get(&agent.path).cloned().or_else(|| {
                    let found = crate::config::find_project_config(&agent.path)
                        .ok()
                        .flatten()
                        .map(|location| location.config_dir);
                    if let Some(dir) = &found {
                        project_config_cache.insert(agent.path.clone(), dir.clone());
                    }
                    found
                });
                if let Some(dir) = dir {
                    config_dirs.insert(dir);
                }
            }
            if config_dirs != last_config_dirs {
                let _ = config_paths_tx.send(config_dirs.clone());
                last_config_dirs = config_dirs;
            }

            // Navigation follows what the sidebar draws, so an agent folded
            // behind a collapsed toggle is not a jump target.
            let snapshot = &output.snapshot;
            let agent_list = snapshot
                .agents
                .iter()
                .filter(|agent| {
                    !super::snapshot::is_folded(
                        agent,
                        snapshot.group_by,
                        &snapshot.stale_pane_ids,
                        &snapshot.expanded_groups,
                    )
                })
                .map(|agent| agent.pane_id.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            if agent_list != last_agent_list {
                let result = if agent_list.is_empty() {
                    tmux.unset_global_option("@workmux_sidebar_agents")
                } else {
                    tmux.set_global_option("@workmux_sidebar_agents", &agent_list)
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "failed to publish sidebar agent inventory");
                }
                last_agent_list = agent_list;
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
            if client_count > 0 {
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
    let _ = std::fs::remove_file(&sock_path);
    if let Ok(store) = StateStore::new() {
        store.delete_runtime(&backend_name, &instance_id);
    }
    let _ = tmux.unset_global_option("@workmux_sidebar_daemon_pid");
    let _ = tmux.unset_global_option("@workmux_sidebar_agents");
    let _ = tmux.unset_global_option("@workmux_sleeping_panes");
    let _ = tmux.unset_global_option("@workmux_sidebar_scope");
    Ok(())
}

// ── Tick core ────────────────────────────────────────────────────────────

/// Inputs gathered from the environment for one daemon tick.
struct TickInput {
    agents: Vec<crate::multiplexer::AgentPane>,
    tmux_state: TmuxState,
    captured_panes: HashMap<String, String>,
    now: Instant,
    now_ts: u64,
    position: SidebarPosition,
    layout_mode: SidebarLayoutMode,
    filter_mode: SidebarFilterMode,
    sort: crate::config::SidebarSort,
    group_by: Option<crate::config::SidebarGroupBy>,
    collapse_stale: bool,
    expanded_groups: Vec<String>,
    git_statuses: HashMap<PathBuf, GitStatus>,
    pr_statuses: HashMap<PathBuf, PrPathEntry>,
    check_statuses: HashMap<PathBuf, CheckPathEntry>,
    sleeping_pane_ids: HashSet<String>,
}

/// A state-file write to apply after computing the tick.
struct AgentWrite {
    pane_id: String,
    resumed_ts: u64,
}

/// Output of a single tick computation.
struct TickOutput {
    snapshot: super::snapshot::SidebarSnapshot,
    agent_writes: Vec<AgentWrite>,
    runtime_write: Option<crate::state::RuntimeState>,
    /// The new interrupted set. Caller should commit to `last_interrupted`
    /// only after side effects are applied successfully.
    next_interrupted: HashSet<String>,
}

/// Compute one daemon tick from in-memory inputs.
///
/// 1. Runs inactivity detection
/// 2. Mutates agents in memory (status and activity timestamps reset for resumed agents)
/// 3. Builds the snapshot from the already-mutated agents
/// 4. Returns side effects (state file writes, runtime file write)
fn compute_tick(
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
        expanded_groups,
        git_statuses,
        pr_statuses,
        check_statuses,
        sleeping_pane_ids,
    } = input;

    // Phase 1: Inactivity detection
    let interrupted =
        tracker.check_with(&agents, now, |pane_id| captured_panes.get(pane_id).cloned());

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
fn apply_tick_effects(
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

/// Capture pane content for working agents that need checking.
/// Skips agents already confirmed as interrupted (no I/O needed until they resume).
fn gather_captures(
    agents: &[crate::multiplexer::AgentPane],
    mux: &dyn Multiplexer,
    tracker: &InactivityTracker,
) -> HashMap<String, String> {
    agents
        .iter()
        .filter(|a| a.status == Some(crate::multiplexer::AgentStatus::Working))
        .filter(|a| !tracker.is_confirmed(&a.pane_id, a.updated_ts.unwrap_or(0)))
        .filter_map(|a| {
            mux.capture_pane(&a.pane_id, 5)
                .map(|content| (a.pane_id.clone(), content))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::CheckState;
    use crate::multiplexer::{AgentPane, AgentStatus};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::process::Command;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        run_git(path, &["init", "-q"]);
    }

    fn working_agent(pane_id: &str, updated_ts: u64) -> AgentPane {
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
        }
    }

    fn done_agent(pane_id: &str) -> AgentPane {
        AgentPane {
            status: Some(AgentStatus::Done),
            ..working_agent(pane_id, 1)
        }
    }

    #[test]
    fn scheduler_observes_and_captures_immediately_at_startup() {
        let now = Instant::now();
        let scheduler = Scheduler::new(now);

        assert!(scheduler.observation_due(now));
        assert!(scheduler.capture_due(now));
        assert!(!scheduler.heartbeat_due(now));
    }

    #[test]
    fn worker_and_config_publications_schedule_no_tmux_or_capture_work() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);
        scheduler.finish_capture(start);
        for publication_wakeup in [
            start + Duration::from_millis(100),
            start + Duration::from_millis(200),
        ] {
            assert!(!scheduler.observation_due(publication_wakeup));
            assert!(!scheduler.capture_due(publication_wakeup));
        }
    }

    #[test]
    fn failed_observation_keeps_the_periodic_recovery_deadline() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);

        assert!(!scheduler.observation_due(start + Duration::from_secs(1)));
        assert!(scheduler.observation_due(start + OBSERVATION_INTERVAL));
    }

    #[test]
    fn tmux_events_coalesce_without_postponing_the_first_deadline() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);
        scheduler.finish_capture(start);
        scheduler.notify_tmux_event(start + Duration::from_millis(10));
        scheduler.notify_tmux_event(start + Duration::from_millis(40));

        assert!(!scheduler.observation_due(start + Duration::from_millis(59)));
        assert!(scheduler.observation_due(start + Duration::from_millis(60)));
    }

    #[test]
    fn event_arriving_during_observation_gets_its_own_coalesced_run() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);
        scheduler.notify_tmux_event(start + Duration::from_millis(20));
        scheduler.finish_observation(start + Duration::from_millis(70));
        scheduler.notify_tmux_event(start + Duration::from_millis(80));

        assert!(!scheduler.observation_due(start + Duration::from_millis(129)));
        assert!(scheduler.observation_due(start + Duration::from_millis(130)));
    }

    #[test]
    fn periodic_observation_absorbs_a_pending_event() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);
        scheduler.notify_tmux_event(start + OBSERVATION_INTERVAL - Duration::from_millis(10));
        scheduler.finish_observation(start + OBSERVATION_INTERVAL);

        assert_eq!(scheduler.event_observation, None);
        assert!(!scheduler.observation_due(start + OBSERVATION_INTERVAL + EVENT_COALESCE_INTERVAL));
    }

    #[test]
    fn event_work_does_not_postpone_periodic_deadlines() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);
        scheduler.finish_capture(start);
        scheduler.notify_tmux_event(start + Duration::from_millis(500));
        scheduler.finish_observation(start + Duration::from_millis(550));

        assert!(scheduler.observation_due(start + OBSERVATION_INTERVAL));
        assert!(scheduler.capture_due(start + CAPTURE_INTERVAL));
    }

    #[test]
    fn overdue_deadlines_advance_past_long_running_work() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        let after_stall = start + Duration::from_secs(7);
        scheduler.finish_observation(after_stall);
        scheduler.finish_capture(after_stall);

        assert!(!scheduler.observation_due(after_stall));
        assert!(!scheduler.capture_due(after_stall));
        assert_eq!(scheduler.next_observation, start + Duration::from_secs(8));
        assert_eq!(scheduler.next_capture, start + Duration::from_secs(8));
    }

    #[test]
    fn heartbeat_retry_and_maintenance_are_independent() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        let heartbeat = start + HEARTBEAT_INTERVAL;
        assert!(scheduler.heartbeat_due(heartbeat));
        scheduler.finish_heartbeat(heartbeat, false);
        scheduler.finish_maintenance(start + MAINTENANCE_INTERVAL);

        assert_eq!(
            scheduler.next_heartbeat,
            heartbeat + HEARTBEAT_RETRY_INTERVAL
        );
        assert_eq!(scheduler.next_maintenance, start + MAINTENANCE_INTERVAL * 2);
    }

    #[test]
    fn partial_github_outcome_updates_answered_and_retains_unanswered_branches() {
        let pr = |number| PrSummary {
            number,
            title: format!("PR {number}"),
            state: "OPEN".to_string(),
            is_draft: false,
            checks: None,
            check_meta: None,
            url: None,
        };
        let previous_prs = HashMap::from([
            ("answered".to_string(), pr(1)),
            ("unanswered".to_string(), pr(2)),
            ("inactive".to_string(), pr(3)),
        ]);
        let previous_checks = HashMap::from([
            (
                "answered".to_string(),
                CheckSummary {
                    state: CheckState::Success,
                    meta: None,
                },
            ),
            (
                "unanswered".to_string(),
                CheckSummary {
                    state: CheckState::Failure {
                        passed: 0,
                        total: 1,
                    },
                    meta: None,
                },
            ),
            (
                "inactive".to_string(),
                CheckSummary {
                    state: CheckState::Success,
                    meta: None,
                },
            ),
        ]);
        let outcome = crate::github::BranchQueryOutcome {
            requested: HashSet::from(["answered".to_string(), "unanswered".to_string()]),
            answered: HashMap::from([(
                "answered".to_string(),
                crate::github::BranchSummary {
                    pr: None,
                    checks: None,
                },
            )]),
        };

        let (prs, checks) = merge_github_outcome(previous_prs, previous_checks, outcome);

        assert!(!prs.contains_key("answered"));
        assert!(!checks.contains_key("answered"));
        assert_eq!(prs.get("unanswered").map(|pr| pr.number), Some(2));
        assert!(matches!(
            checks.get("unanswered").map(|check| &check.state),
            Some(CheckState::Failure { .. })
        ));
        assert!(!prs.contains_key("inactive"));
        assert!(!checks.contains_key("inactive"));
    }

    #[test]
    fn github_fetch_is_throttled_between_intervals() {
        assert!(!github_fetch_due(false, Duration::from_secs(29)));
        assert!(github_fetch_due(false, Duration::from_secs(30)));
        assert!(github_fetch_due(true, Duration::ZERO));
    }

    fn empty_snapshot() -> super::super::snapshot::SidebarSnapshot {
        super::super::snapshot::SidebarSnapshot {
            position: SidebarPosition::Left,
            layout_mode: SidebarLayoutMode::Tiles,
            filter_mode: SidebarFilterMode::None,
            group_by: None,
            expanded_groups: Vec::new(),
            stale_pane_ids: std::collections::HashSet::new(),
            collapse_stale: false,
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

    fn read_snapshot(stream: &mut UnixStream) -> super::super::snapshot::SidebarSnapshot {
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
    fn mutation_watchers_exclude_access_events() {
        assert_eq!(mutation_watcher_config().event_kinds(), EventKindMask::CORE);
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

    #[test]
    fn rescan_events_request_recovery() {
        let event =
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert!(git_event_requires_recovery(&event));
        assert!(!git_event_requires_recovery(&notify::Event::new(
            notify::EventKind::Other,
        )));
    }

    #[test]
    fn recovery_cooldown_coalesces_repeated_failures() {
        let cooldown = Duration::from_secs(30);
        assert!(recovery_ready(true, Instant::now() - cooldown, cooldown));
        assert!(!recovery_ready(true, Instant::now(), cooldown));
        assert!(!recovery_ready(false, Instant::now() - cooldown, cooldown));
    }

    #[test]
    fn rolling_audit_is_fair_and_skips_incomplete_watches() {
        let paths = vec![
            PathBuf::from("/one"),
            PathBuf::from("/two"),
            PathBuf::from("/three"),
        ];
        let complete = HashMap::from([
            (paths[0].clone(), true),
            (paths[1].clone(), false),
            (paths[2].clone(), true),
        ]);
        let mut cursor = 0;

        assert_eq!(
            next_audit_path(&paths, &complete, &mut cursor),
            Some(paths[0].clone())
        );
        assert_eq!(
            next_audit_path(&paths, &complete, &mut cursor),
            Some(paths[2].clone())
        );
        assert_eq!(
            next_audit_path(&paths, &complete, &mut cursor),
            Some(paths[0].clone())
        );
    }

    #[test]
    fn ignored_tracked_files_still_trigger_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo);
        std::fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(repo.join("tracked.log"), "tracked\n").unwrap();
        run_git(&repo, &["add", ".gitignore"]);
        run_git(&repo, &["add", "-f", "tracked.log"]);

        let root = repo.canonicalize().unwrap();
        let gitignores = HashMap::from([(root.clone(), build_gitignore(&root))]);
        let ignored_tracked_paths =
            HashMap::from([(root.clone(), load_ignored_tracked_paths(&root))]);

        assert!(!is_event_ignored(
            &root.join("tracked.log"),
            &root,
            &gitignores,
            &ignored_tracked_paths,
        ));
        assert!(is_event_ignored(
            &root.join("generated.log"),
            &root,
            &gitignores,
            &ignored_tracked_paths,
        ));
    }

    #[test]
    fn cache_projects_status_when_agent_path_changes_under_same_root() {
        let root = PathBuf::from("/repo");
        let old_path = root.clone();
        let nested = root.join("nested");
        let previous = HashMap::from([(
            root.clone(),
            ResolvedGitWorktree {
                agent_paths: vec![old_path.clone()],
                is_stale: true,
                is_focused: false,
            },
        )]);
        let current = HashMap::from([(
            root,
            ResolvedGitWorktree {
                agent_paths: vec![nested.clone()],
                is_stale: true,
                is_focused: false,
            },
        )]);
        let status = GitStatus {
            branch: Some("main".to_string()),
            ..GitStatus::default()
        };
        let mut cache = HashMap::from([(old_path.clone(), status.clone())]);

        let (changed, missing) = reconcile_git_cache(&previous, &current, &mut cache);

        assert!(changed);
        assert!(missing.is_empty());
        assert_eq!(cache, HashMap::from([(nested, status)]));
        assert!(!cache.contains_key(&old_path));
    }

    #[test]
    fn positive_root_cache_rediscovers_nested_repository() {
        for keep_parent_agent in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().join("parent");
            let child = parent.join("child");
            init_repo(&parent);
            run_git(&parent, &["symbolic-ref", "HEAD", "refs/heads/parent"]);
            std::fs::create_dir(&child).unwrap();
            let parent = parent.canonicalize().unwrap();
            let child = child.canonicalize().unwrap();
            let mut entries = vec![GitWorkerPath {
                path: child.clone(),
                is_stale: false,
                is_focused: false,
            }];
            if keep_parent_agent {
                entries.push(GitWorkerPath {
                    path: parent.clone(),
                    is_stale: true,
                    is_focused: false,
                });
            }
            let mut roots = HashMap::new();
            let initial = resolve_git_worktrees_cached(&entries, &mut roots);
            assert_eq!(initial.len(), 1);
            assert!(initial.contains_key(&parent));
            let cache: GitCache = Arc::new(Mutex::new(HashMap::new()));
            assert!(refresh_git_status(
                &parent,
                &initial[&parent].agent_paths,
                &cache
            ));
            assert_eq!(
                cache.lock().unwrap()[&child].branch.as_deref(),
                Some("parent")
            );

            let mut watcher = notify::RecommendedWatcher::new(
                |_: notify::Result<notify::Event>| {},
                mutation_watcher_config(),
            )
            .unwrap();
            let mut watches = HashMap::new();
            let mut complete = HashMap::new();
            let mut reverse = HashMap::new();
            reconcile_worktree_watches(
                &mut watcher,
                std::slice::from_ref(&parent),
                &mut watches,
                &mut complete,
                &mut reverse,
            );

            init_repo(&child);
            run_git(&child, &["symbolic-ref", "HEAD", "refs/heads/child"]);
            assert_eq!(
                crate::git::get_repo_root_for(&child)
                    .unwrap()
                    .canonicalize()
                    .unwrap(),
                child,
            );
            let start = Instant::now();
            let mut last_revalidation = start;
            assert!(!expire_git_roots(
                &mut roots,
                &mut last_revalidation,
                start + GIT_ROOT_REVALIDATION_INTERVAL - Duration::from_millis(1),
            ));
            assert_eq!(resolve_git_worktrees_cached(&entries, &mut roots), initial);
            assert!(expire_git_roots(
                &mut roots,
                &mut last_revalidation,
                start + GIT_ROOT_REVALIDATION_INTERVAL,
            ));
            let resolved = resolve_git_worktrees_cached(&entries, &mut roots);
            assert!(resolved.contains_key(&child));
            assert_eq!(resolved.contains_key(&parent), keep_parent_agent);
            let (changed, missing) =
                reconcile_git_cache(&initial, &resolved, &mut cache.lock().unwrap());
            assert!(changed);
            assert_eq!(missing, vec![child.clone()]);
            assert!(!cache.lock().unwrap().contains_key(&child));

            let active = resolved.keys().cloned().collect::<Vec<_>>();
            assert_eq!(
                reconcile_worktree_watches(
                    &mut watcher,
                    &active,
                    &mut watches,
                    &mut complete,
                    &mut reverse,
                ),
                vec![child.clone()]
            );
            assert_eq!(watches.contains_key(&parent), keep_parent_agent);
            assert_eq!(complete.contains_key(&parent), keep_parent_agent);
            assert!(complete[&child]);
            assert!(reverse[&child.join(".git")].contains(&child));
            assert_eq!(
                reverse.values().any(|roots| roots.contains(&parent)),
                keep_parent_agent
            );
            assert!(find_worktrees_for_path(&child.join(".git/HEAD"), &reverse).contains(&child));
            assert!(refresh_git_status(
                &child,
                &resolved[&child].agent_paths,
                &cache
            ));
            assert_eq!(
                cache.lock().unwrap()[&child].branch.as_deref(),
                Some("child")
            );
            assert!(
                reconcile_worktree_watches(
                    &mut watcher,
                    &active,
                    &mut watches,
                    &mut complete,
                    &mut reverse,
                )
                .is_empty()
            );
            let (changed, missing) =
                reconcile_git_cache(&resolved, &resolved, &mut cache.lock().unwrap());
            assert!(!changed);
            assert!(missing.is_empty());

            std::fs::remove_dir_all(child.join(".git")).unwrap();
            assert!(expire_git_roots(
                &mut roots,
                &mut last_revalidation,
                start + GIT_ROOT_REVALIDATION_INTERVAL * 2,
            ));
            let merged = resolve_git_worktrees_cached(&entries, &mut roots);
            assert_eq!(merged, initial);
            let (changed, missing) =
                reconcile_git_cache(&resolved, &merged, &mut cache.lock().unwrap());
            assert!(changed);
            if keep_parent_agent {
                assert!(missing.is_empty());
            } else {
                assert_eq!(missing, vec![parent.clone()]);
                assert!(refresh_git_status(
                    &parent,
                    &merged[&parent].agent_paths,
                    &cache
                ));
            }
            assert_eq!(
                cache.lock().unwrap()[&child].branch.as_deref(),
                Some("parent")
            );
            reconcile_worktree_watches(
                &mut watcher,
                std::slice::from_ref(&parent),
                &mut watches,
                &mut complete,
                &mut reverse,
            );
            assert!(!watches.contains_key(&child));
            assert!(!complete.contains_key(&child));
            assert!(!reverse.values().any(|roots| roots.contains(&child)));
            assert!(reverse[&parent.join(".git")].contains(&parent));
        }
    }

    #[test]
    fn git_worker_revalidates_roots_without_path_updates() {
        struct StopWorker(Arc<AtomicBool>);
        impl Drop for StopWorker {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let child = parent.join("child");
        init_repo(&parent);
        run_git(&parent, &["symbolic-ref", "HEAD", "refs/heads/parent"]);
        std::fs::create_dir(&child).unwrap();
        let term = Arc::new(AtomicBool::new(false));
        let _stop = StopWorker(term.clone());
        let dirty = Arc::new(AtomicBool::new(false));
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let (cache, paths_tx) = spawn_git_worker(term, dirty.clone(), wake_tx);
        paths_tx
            .send(vec![GitWorkerPath {
                path: child.clone(),
                is_stale: true,
                is_focused: false,
            }])
            .unwrap();

        let wait_for_branch = |branch: &str, timeout: Duration| {
            let deadline = Instant::now() + timeout;
            loop {
                if cache
                    .lock()
                    .unwrap()
                    .get(&child)
                    .is_some_and(|status| status.branch.as_deref() == Some(branch))
                    && dirty.load(Ordering::Relaxed)
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "worker did not publish branch {branch}"
                );
                let _ = wake_rx.recv_timeout(Duration::from_millis(50));
            }
        };
        wait_for_branch("parent", Duration::from_secs(5));
        dirty.store(false, Ordering::Relaxed);
        while wake_rx.try_recv().is_ok() {}
        init_repo(&child);
        run_git(&child, &["symbolic-ref", "HEAD", "refs/heads/child"]);
        // The worker must rediscover this root without another path-channel message.
        wait_for_branch(
            "child",
            GIT_ROOT_REVALIDATION_INTERVAL + Duration::from_secs(5),
        );
    }

    #[test]
    fn root_revalidation_retries_negative_discoveries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        let entries = [GitWorkerPath {
            path: path.clone(),
            is_stale: true,
            is_focused: false,
        }];
        let mut roots = HashMap::new();
        assert!(resolve_git_worktrees_cached(&entries, &mut roots).is_empty());
        init_repo(&path);
        let start = Instant::now();
        let mut last_revalidation = start;
        assert!(expire_git_roots(
            &mut roots,
            &mut last_revalidation,
            start + GIT_ROOT_REVALIDATION_INTERVAL,
        ));
        let resolved = resolve_git_worktrees_cached(&entries, &mut roots);
        assert!(resolved.contains_key(&path));
        assert!(!expire_git_roots(
            &mut roots,
            &mut last_revalidation,
            start + GIT_ROOT_REVALIDATION_INTERVAL + Duration::from_secs(1),
        ));
        assert_eq!(resolve_git_worktrees_cached(&entries, &mut roots), resolved);
    }

    #[test]
    fn cached_resolution_updates_relevance_without_repository_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo);
        let root = repo.canonicalize().unwrap();
        let mut roots = HashMap::from([(repo.clone(), Some(root.clone()))]);

        let resolved = resolve_git_worktrees_cached(
            &[GitWorkerPath {
                path: repo.clone(),
                is_stale: false,
                is_focused: true,
            }],
            &mut roots,
        );

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[&root].agent_paths, vec![repo]);
        assert!(!resolved[&root].is_stale);
        assert!(resolved[&root].is_focused);
    }

    #[test]
    fn non_repository_agent_path_produces_no_watch_or_refresh_roots() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![GitWorkerPath {
            path: dir.path().to_path_buf(),
            is_stale: false,
            is_focused: false,
        }];

        let resolved = resolve_git_worktrees_cached(&entries, &mut HashMap::new());
        let watch_specs: Vec<_> = resolved
            .keys()
            .flat_map(|root| worktree_watch_specs(root, true))
            .collect();

        assert!(resolved.is_empty());
        assert!(watch_specs.is_empty());
    }

    #[test]
    fn nested_agent_paths_share_the_repository_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let nested = repo.join("src/nested");
        init_repo(&repo);
        std::fs::create_dir_all(&nested).unwrap();
        let entries = vec![
            GitWorkerPath {
                path: repo.clone(),
                is_stale: true,
                is_focused: false,
            },
            GitWorkerPath {
                path: nested.clone(),
                is_stale: false,
                is_focused: true,
            },
        ];

        let resolved = resolve_git_worktrees_cached(&entries, &mut HashMap::new());
        let root = repo.canonicalize().unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved.get(&root),
            Some(&ResolvedGitWorktree {
                agent_paths: vec![repo, nested],
                is_stale: false,
                is_focused: true,
            })
        );
    }

    #[test]
    fn linked_worktree_uses_its_root_and_shared_git_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let linked = dir.path().join("linked");
        init_repo(&repo);
        run_git(&repo, &["config", "user.name", "Workmux Tests"]);
        run_git(&repo, &["config", "user.email", "workmux@example.com"]);
        std::fs::write(repo.join("tracked"), "content").unwrap();
        run_git(&repo, &["add", "tracked"]);
        run_git(&repo, &["commit", "-q", "-m", "initial"]);
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "linked-test",
                linked.to_str().unwrap(),
            ],
        );
        let nested = linked.join("nested");
        std::fs::create_dir(&nested).unwrap();

        let entries = [GitWorkerPath {
            path: nested.clone(),
            is_stale: false,
            is_focused: false,
        }];
        let resolved = resolve_git_worktrees_cached(&entries, &mut HashMap::new());
        let linked_root = linked.canonicalize().unwrap();
        assert_eq!(resolved.keys().collect::<Vec<_>>(), vec![&linked_root]);

        let git_dir = resolve_git_dir(&linked_root)
            .unwrap()
            .canonicalize()
            .unwrap();
        let common_dir = resolve_common_git_dir(&git_dir).unwrap();
        let specs = worktree_watch_specs(&linked_root, true);

        assert!(specs.iter().any(|spec| {
            spec.path == git_dir && matches!(spec.mode, RecursiveMode::NonRecursive)
        }));
        assert!(specs.iter().any(|spec| {
            spec.path == common_dir && matches!(spec.mode, RecursiveMode::NonRecursive)
        }));
        assert!(specs.iter().any(|spec| {
            spec.path == common_dir.join("refs") && matches!(spec.mode, RecursiveMode::Recursive)
        }));
        assert!(specs.iter().any(|spec| {
            spec.path == linked_root && matches!(spec.mode, RecursiveMode::Recursive)
        }));
    }

    #[test]
    fn config_reload_triggers_on_mutations_of_config_files() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};

        let config = PathBuf::from("/home/u/.config/workmux/config.yaml");
        for kind in [
            notify::EventKind::Modify(ModifyKind::Any),
            notify::EventKind::Create(CreateKind::File),
            notify::EventKind::Remove(RemoveKind::File),
        ] {
            let event = notify::Event::new(kind).add_path(config.clone());
            assert!(config_event_triggers_reload(&event), "kind: {kind:?}");
        }

        let project = notify::Event::new(notify::EventKind::Modify(ModifyKind::Any))
            .add_path(PathBuf::from("/repo/.workmux.yaml"));
        assert!(config_event_triggers_reload(&project));

        let atomic_rename = notify::Event::new(notify::EventKind::Modify(ModifyKind::Name(
            RenameMode::Both,
        )))
        .add_path(PathBuf::from("/home/u/.config/workmux/config.yaml.tmp"))
        .add_path(config);
        assert!(config_event_triggers_reload(&atomic_rename));
    }

    #[test]
    fn config_reload_ignores_access_events_and_other_files() {
        use notify::event::{AccessKind, AccessMode, ModifyKind};

        let config = PathBuf::from("/home/u/.config/workmux/config.yaml");
        for kind in [
            notify::EventKind::Access(AccessKind::Open(AccessMode::Any)),
            notify::EventKind::Access(AccessKind::Close(AccessMode::Read)),
            notify::EventKind::Access(AccessKind::Close(AccessMode::Write)),
        ] {
            let event = notify::Event::new(kind).add_path(config.clone());
            assert!(!config_event_triggers_reload(&event), "kind: {kind:?}");
        }

        let unrelated = notify::Event::new(notify::EventKind::Modify(ModifyKind::Any))
            .add_path(PathBuf::from("/home/u/.config/workmux/other.txt"));
        assert!(!config_event_triggers_reload(&unrelated));
    }

    #[test]
    fn config_reload_triggers_on_rescan() {
        let event =
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan);

        assert!(config_event_triggers_reload(&event));
    }

    #[test]
    fn github_branches_are_grouped_by_common_repository() {
        let first = PathBuf::from("/repo__worktrees/first");
        let second = PathBuf::from("/repo__worktrees/second");
        let other = PathBuf::from("/other");
        let common = PathBuf::from("/repo/.git");
        let other_common = PathBuf::from("/other/.git");
        let entries = vec![
            GithubWorkerPath {
                path: first.clone(),
                branch: "feature-a".to_string(),
            },
            GithubWorkerPath {
                path: second.clone(),
                branch: "feature-b".to_string(),
            },
            GithubWorkerPath {
                path: other.clone(),
                branch: "main".to_string(),
            },
        ];
        let repo_keys = HashMap::from([
            (first.clone(), common.clone()),
            (second, common.clone()),
            (other.clone(), other_common.clone()),
        ]);

        let grouped = group_github_branches(&entries, &repo_keys);

        assert_eq!(grouped.len(), 2);
        assert_eq!(
            grouped.get(&common),
            Some(&(
                first,
                vec!["feature-a".to_string(), "feature-b".to_string()]
            ))
        );
        assert_eq!(
            grouped.get(&other_common),
            Some(&(other, vec!["main".to_string()]))
        );
    }

    #[test]
    fn clear_pr_path_cache_only_reports_content_changes() {
        let cache: PrPathCache = Arc::new(Mutex::new(HashMap::new()));

        assert!(!clear_pr_path_cache(&cache));

        cache.lock().unwrap().insert(
            PathBuf::from("/repo"),
            PrPathEntry {
                branch: "feature".to_string(),
                summary: PrSummary {
                    number: 123,
                    title: "test".to_string(),
                    state: "OPEN".to_string(),
                    is_draft: false,
                    checks: None,
                    check_meta: None,
                    url: None,
                },
            },
        );

        assert!(clear_pr_path_cache(&cache));
        assert!(cache.lock().unwrap().is_empty());
        assert!(!clear_pr_path_cache(&cache));
    }

    #[test]
    fn pr_path_cache_records_branch() {
        let path = PathBuf::from("/repo");
        let repo_root = PathBuf::from("/repo");
        let summary = PrSummary {
            number: 123,
            title: "test".to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            checks: None,
            check_meta: None,
            url: None,
        };
        let entries = vec![GithubWorkerPath {
            path: path.clone(),
            branch: "feature".to_string(),
        }];
        let repo_keys = HashMap::from([(path.clone(), repo_root.clone())]);
        let repo_cache =
            HashMap::from([(repo_root, HashMap::from([("feature".to_string(), summary)]))]);
        let path_cache: PrPathCache = Arc::new(Mutex::new(HashMap::new()));
        let dirty_flag = Arc::new(AtomicBool::new(false));
        let (wake_tx, _wake_rx) = std::sync::mpsc::sync_channel(1);

        publish_pr_path_cache(
            &entries,
            &repo_keys,
            &repo_cache,
            &path_cache,
            &dirty_flag,
            &wake_tx,
        );

        let cache = path_cache.lock().unwrap();
        let entry = cache.get(&path).unwrap();
        assert_eq!(entry.branch, "feature");
        assert_eq!(entry.summary.number, 123);
        assert!(dirty_flag.load(Ordering::Relaxed));
    }

    #[test]
    fn pane_lifecycle_replacement_resets_inactivity_tracking() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();
        let pane = |pid| LivePaneInfo {
            pid: Some(pid),
            current_command: Some("agent".into()),
            working_dir: PathBuf::new(),
            title: None,
            session: Some("session".into()),
            window: Some("window".into()),
            session_id: Some("$1".into()),
            window_id: Some("@1".into()),
        };

        tracker.reconcile_identities(
            &HashMap::from([("%1".to_string(), pane(10))]),
            Some("server-a"),
        );
        tracker.check_with(&agents, t0, |_| Some("same".into()));
        assert!(
            tracker
                .check_with(&agents, t0 + Duration::from_secs(11), |_| Some(
                    "same".into()
                ))
                .contains("%1")
        );

        tracker.reconcile_identities(
            &HashMap::from([("%1".to_string(), pane(11))]),
            Some("server-a"),
        );
        assert!(
            tracker
                .check_with(&agents, t0 + Duration::from_secs(12), |_| Some(
                    "same".into()
                ))
                .is_empty()
        );
    }

    #[test]
    fn server_lifecycle_replacement_resets_sticky_interruption() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();
        let panes = HashMap::from([(
            "%1".to_string(),
            LivePaneInfo {
                pid: Some(10),
                current_command: Some("agent".into()),
                working_dir: PathBuf::new(),
                title: None,
                session: Some("session".into()),
                window: Some("window".into()),
                session_id: Some("$1".into()),
                window_id: Some("@1".into()),
            },
        )]);

        tracker.reconcile_identities(&panes, Some("server-a"));
        tracker.check_with(&agents, t0, |_| Some("same".into()));
        tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("same".into())
        });
        tracker.reconcile_identities(&panes, Some("server-b"));

        assert!(
            tracker
                .check_with(&agents, t0 + Duration::from_secs(12), |_| Some(
                    "same".into()
                ))
                .is_empty()
        );
    }

    #[test]
    fn no_interruption_before_timeout() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // First check: records the hash
        let result = tracker.check_with(&agents, t0, |_| Some("hello".into()));
        assert!(result.is_empty());

        // 5s later, same content: not yet interrupted
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(5), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());
    }

    #[test]
    fn interruption_after_timeout() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // First check records hash
        tracker.check_with(&agents, t0, |_| Some("hello".into()));

        // 11s later, same content: interrupted
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });
        assert!(result.contains("%1"));
    }

    #[test]
    fn changing_content_resets_window() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // First check
        tracker.check_with(&agents, t0, |_| Some("hello".into()));

        // 8s later, content changes: resets the window
        tracker.check_with(&agents, t0 + Duration::from_secs(8), |_| {
            Some("world".into())
        });

        // 5s after the change (13s total): not interrupted (only 5s since reset)
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(13), |_| {
            Some("world".into())
        });
        assert!(result.is_empty());

        // 11s after the change (19s total): now interrupted
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(19), |_| {
            Some("world".into())
        });
        assert!(result.contains("%1"));
    }

    #[test]
    fn sticky_despite_content_change() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Become interrupted
        tracker.check_with(&agents, t0, |_| Some("hello".into()));
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });
        assert!(result.contains("%1"));

        // Content changes (user typing): still interrupted
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(12), |_| {
            Some("user typed something".into())
        });
        assert!(result.contains("%1"));
    }

    #[test]
    fn clears_on_updated_ts_change() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Become interrupted
        tracker.check_with(&agents, t0, |_| Some("hello".into()));
        tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });

        // Agent sends new RPC (updated_ts changes): clears interrupted
        let resumed_agents = vec![working_agent("%1", 2)];
        let result = tracker.check_with(&resumed_agents, t0 + Duration::from_secs(12), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());
    }

    #[test]
    fn fresh_window_after_resume() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Become interrupted
        tracker.check_with(&agents, t0, |_| Some("hello".into()));
        tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });

        // Resume (updated_ts changes) at t=12s
        let resumed = vec![working_agent("%1", 2)];
        tracker.check_with(&resumed, t0 + Duration::from_secs(12), |_| {
            Some("hello".into())
        });

        // 5s after resume (t=17s): same content but not interrupted yet (fresh window)
        let result = tracker.check_with(&resumed, t0 + Duration::from_secs(17), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());

        // 11s after resume (t=23s): now interrupted again
        let result = tracker.check_with(&resumed, t0 + Duration::from_secs(23), |_| {
            Some("hello".into())
        });
        assert!(result.contains("%1"));
    }

    #[test]
    fn non_working_agents_ignored() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![done_agent("%1")];
        let t0 = Instant::now();

        tracker.check_with(&agents, t0, |_| Some("hello".into()));
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());
    }

    #[test]
    fn leaves_working_clears_tracking() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let working = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Become interrupted
        tracker.check_with(&working, t0, |_| Some("hello".into()));
        tracker.check_with(&working, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });

        // Agent transitions to Done
        let done = vec![done_agent("%1")];
        let result = tracker.check_with(&done, t0 + Duration::from_secs(12), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());

        // Comes back as Working: starts fresh
        let working_again = vec![working_agent("%1", 3)];
        let result = tracker.check_with(&working_again, t0 + Duration::from_secs(13), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty()); // just recorded, not yet timed out
    }

    #[test]
    fn capture_failure_skips_pane() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Capture fails: no entry recorded
        tracker.check_with(&agents, t0, |_| None);
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| None);
        assert!(result.is_empty());
    }

    #[test]
    fn multiple_agents_tracked_independently() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1), working_agent("%2", 1)];
        let t0 = Instant::now();

        let content = RefCell::new(HashMap::from([
            ("%1".to_string(), "static".to_string()),
            ("%2".to_string(), "changing".to_string()),
        ]));

        // First check
        tracker.check_with(&agents, t0, |id| content.borrow().get(id).cloned());

        // Change %2's content at 5s
        content
            .borrow_mut()
            .insert("%2".to_string(), "new output".into());
        tracker.check_with(&agents, t0 + Duration::from_secs(5), |id| {
            content.borrow().get(id).cloned()
        });

        // At 11s: %1 is interrupted (11s unchanged), %2 is not (only 6s since change)
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |id| {
            content.borrow().get(id).cloned()
        });
        assert_eq!(result, HashSet::from(["%1".to_string()]));
    }

    #[test]
    fn rpc_update_before_timeout_resets_window() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let t0 = Instant::now();

        // Start tracking
        tracker.check_with(&[working_agent("%1", 1)], t0, |_| Some("hello".into()));

        // Agent sends RPC at 5s (updated_ts changes) but content unchanged
        tracker.check_with(
            &[working_agent("%1", 2)],
            t0 + Duration::from_secs(5),
            |_| Some("hello".into()),
        );

        // At 11s: only 6s since RPC update, should NOT be interrupted
        let result = tracker.check_with(
            &[working_agent("%1", 2)],
            t0 + Duration::from_secs(11),
            |_| Some("hello".into()),
        );
        assert!(result.is_empty());

        // At 16s: 11s since RPC update, now interrupted
        let result = tracker.check_with(
            &[working_agent("%1", 2)],
            t0 + Duration::from_secs(16),
            |_| Some("hello".into()),
        );
        assert_eq!(result, HashSet::from(["%1".to_string()]));
    }

    #[test]
    fn interruption_at_exact_timeout() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        tracker.check_with(&agents, t0, |_| Some("hello".into()));
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(10), |_| {
            Some("hello".into())
        });
        assert_eq!(result, HashSet::from(["%1".to_string()]));
    }

    #[test]
    fn ansi_and_whitespace_normalized() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Plain text first
        tracker.check_with(&agents, t0, |_| Some("hello\n".into()));

        // Same text wrapped in ANSI codes + trailing whitespace: should hash the same
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("\x1b[31mhello\x1b[0m   ".into())
        });
        assert_eq!(result, HashSet::from(["%1".to_string()]));
    }

    #[test]
    fn capture_failure_does_not_create_baseline() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Capture fails: no baseline recorded
        tracker.check_with(&agents, t0, |_| None);

        // Capture succeeds later: this is the first successful capture, not a timeout
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());
    }

    // ── Tick-level tests (tracker + state store + runtime) ──────────────

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
                    tmux_state: TmuxState {
                        live_panes: HashMap::new(),
                        window_statuses: HashMap::new(),
                        active_windows: HashSet::new(),
                        pane_window_ids: HashMap::new(),
                        pane_window_indexes: HashMap::new(),
                        active_pane_ids: HashSet::new(),
                        window_pane_counts: HashMap::new(),
                        server_boot_id: None,
                        position: None,
                        layout: None,
                        filter: None,
                        sleeping_panes: None,
                        group_by: None,
                        expanded_groups: None,
                    },
                    captured_panes: captures,
                    sort: crate::config::SidebarSort::default(),
                    group_by: None,
                    collapse_stale: false,
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
