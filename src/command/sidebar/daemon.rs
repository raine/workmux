//! Sidebar daemon: single process that polls tmux and pushes snapshots to clients.

use anyhow::Result;
use ignore::gitignore::Gitignore;
use notify::{RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cmd::Cmd;
use crate::config::{Config, SidebarPosition};
use crate::git::GitStatus;
use crate::github::PrSummary;
use crate::multiplexer::{Multiplexer, create_backend, detect_backend};
use crate::state::StateStore;

use super::app::{SidebarFilterMode, SidebarLayoutMode};
use super::snapshot::{PrPathEntry, build_snapshot};

/// Compute socket path from instance_id.
pub fn socket_path(instance_id: &str) -> PathBuf {
    let safe_id = instance_id.replace(['/', '\\'], "-");
    std::env::temp_dir().join(format!("workmux-sidebar-{}.sock", safe_id))
}

/// Result of a batched tmux query.
struct TmuxState {
    window_statuses: HashMap<String, Option<String>>,
    active_windows: HashSet<(String, String)>,
    pane_window_ids: HashMap<String, String>,
    active_pane_ids: HashSet<String>,
    window_pane_counts: HashMap<String, usize>,
}

/// Query all sidebar-relevant tmux state in a single command.
fn query_tmux_state() -> TmuxState {
    let format = "#{pane_id}\t#{session_name}\t#{window_id}\t#{@workmux_pane_status}\t#{window_active}\t#{session_attached}\t#{pane_active}";
    let output = Cmd::new("tmux")
        .args(&["list-panes", "-a", "-F", format])
        .run_and_capture_stdout()
        .unwrap_or_default();

    let mut window_statuses = HashMap::new();
    let mut active_windows = HashSet::new();
    let mut pane_window_ids = HashMap::new();
    let mut active_pane_ids = HashSet::new();
    let mut window_pane_counts: HashMap<String, usize> = HashMap::new();

    for line in output.lines() {
        let mut parts = line.split('\t');
        let (
            Some(pane_id),
            Some(session),
            Some(window_id),
            Some(status),
            Some(win_active),
            Some(sess_attached),
            Some(pane_active),
        ) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        )
        else {
            continue;
        };
        let win_active = win_active == "1";
        let sess_attached = sess_attached == "1";
        let pane_active = pane_active == "1";

        let status_val = if status.is_empty() {
            None
        } else {
            Some(status.to_string())
        };
        window_statuses.insert(pane_id.to_string(), status_val);
        pane_window_ids.insert(pane_id.to_string(), window_id.to_string());
        *window_pane_counts.entry(window_id.to_string()).or_default() += 1;

        if win_active && sess_attached {
            active_windows.insert((session.to_string(), window_id.to_string()));
        }
        if pane_active {
            active_pane_ids.insert(pane_id.to_string());
        }
    }

    TmuxState {
        window_statuses,
        active_windows,
        pane_window_ids,
        active_pane_ids,
        window_pane_counts,
    }
}

/// Unix socket server for broadcasting snapshots to clients.
struct SocketServer {
    clients: Arc<Mutex<Vec<UnixStream>>>,
    /// Cached last broadcast payload (length-prefixed) for immediate delivery to new clients.
    cached_payload: Arc<Mutex<Vec<u8>>>,
}

impl SocketServer {
    fn bind(path: &Path) -> std::io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        // Restrict socket to owner only (prevent other local users from reading snapshots)
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let clients: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
        let clients_clone = clients.clone();
        let cached_payload: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let cached_clone = cached_payload.clone();

        thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Clear inherited O_NONBLOCK from the listener.
                        // Without this, write_all fails with WouldBlock when
                        // the payload exceeds the socket buffer, and
                        // set_write_timeout has no effect on non-blocking sockets.
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                        // Send cached snapshot immediately so the client doesn't
                        // wait for the next tick (no dirty_flag needed).
                        // Clone under lock and drop before the blocking write to
                        // avoid holding the mutex during I/O.
                        let payload = cached_clone.lock().ok().map(|p| p.clone());
                        let cache_ok = match payload {
                            Some(ref p) if !p.is_empty() => stream.write_all(p).is_ok(),
                            _ => true,
                        };
                        if cache_ok {
                            let mut clients = clients_clone.lock().unwrap();
                            clients.push(stream);
                            tracing::debug!(clients = clients.len(), "sidebar client connected");
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            clients,
            cached_payload,
        })
    }

    fn broadcast(&self, snapshot: &super::snapshot::SidebarSnapshot) {
        let data = serde_json::to_vec(snapshot).unwrap_or_default();
        let len = (data.len() as u32).to_be_bytes();

        // Cache the length-prefixed payload for new client connections
        if let Ok(mut cached) = self.cached_payload.lock() {
            cached.clear();
            cached.extend_from_slice(&len);
            cached.extend_from_slice(&data);
        }

        // Take clients out of mutex to avoid holding lock during writes
        let mut clients = std::mem::take(&mut *self.clients.lock().unwrap());
        let before = clients.len();
        clients
            .retain_mut(|stream| stream.write_all(&len).is_ok() && stream.write_all(&data).is_ok());
        let dropped = before - clients.len();
        if dropped > 0 {
            tracing::info!(
                dropped,
                remaining = clients.len(),
                payload_bytes = data.len(),
                "sidebar broadcast: clients disconnected"
            );
        }
        // Merge surviving clients back (append to preserve any new connections accepted during writes)
        self.clients.lock().unwrap().append(&mut clients);
    }

    fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }
}

/// Read the sidebar layout mode from tmux global, falling back to settings.json, then config.
fn read_sidebar_layout_mode(config: &Config) -> Option<SidebarLayoutMode> {
    // Check tmux global first (set by toggle_layout_mode during this session)
    if let Ok(output) = Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sidebar_layout"])
        .run_and_capture_stdout()
    {
        match output.trim() {
            "tiles" => return Some(SidebarLayoutMode::Tiles),
            "compact" => return Some(SidebarLayoutMode::Compact),
            _ => {}
        }
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
fn read_sidebar_filter_mode() -> SidebarFilterMode {
    if let Ok(output) = Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sidebar_filter"])
        .run_and_capture_stdout()
    {
        let trimmed = output.trim();
        if !trimmed.is_empty() {
            return SidebarFilterMode::from_str(trimmed);
        }
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

/// Read pane IDs manually marked as sleeping from the tmux global option.
fn read_sleeping_panes() -> HashSet<String> {
    Cmd::new("tmux")
        .args(&["show-option", "-gqv", "@workmux_sleeping_panes"])
        .run_and_capture_stdout()
        .ok()
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
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

/// Check if a filesystem event path should be skipped based on gitignore rules.
/// Returns true if the path is inside a .git directory (non-working-tree change)
/// or matches the worktree's .gitignore patterns.
fn is_event_ignored(
    event_path: &Path,
    worktree: &Path,
    gitignores: &HashMap<PathBuf, Gitignore>,
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

/// Compare two GitStatus values ignoring the cached_at timestamp.
fn git_status_semantically_equal(a: &GitStatus, b: &GitStatus) -> bool {
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
    watched_for_worktree: &mut Vec<PathBuf>,
) {
    let already_watching = watch_to_worktrees.get(path).is_some_and(|s| !s.is_empty());

    if !already_watching && let Err(e) = watcher.watch(path, mode) {
        tracing::warn!("failed to watch {}: {}", path.display(), e);
        return;
    }

    watch_to_worktrees
        .entry(path.to_path_buf())
        .or_default()
        .insert(worktree.to_path_buf());
    watched_for_worktree.push(path.to_path_buf());
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

/// Set up filesystem watches for a worktree.
///
/// Always watches .git metadata (non-recursively for HEAD/index/etc., recursively
/// for refs/) to detect commits, staging, and branch changes instantly.
///
/// On macOS (FSEvents), also watches the worktree root recursively for near-instant
/// uncommitted change detection. On Linux (inotify), working tree changes are
/// detected by the periodic poll sweep instead, avoiding the massive inotify event
/// volume that recursive watches generate under heavy file I/O.
fn setup_worktree_watches(
    watcher: &mut notify::RecommendedWatcher,
    worktree: &Path,
    watch_to_worktrees: &mut HashMap<PathBuf, HashSet<PathBuf>>,
) -> Vec<PathBuf> {
    let mut watched = Vec::new();
    let dot_git = worktree.join(".git");
    let is_linked = dot_git.is_file();

    if is_linked {
        // Linked worktree: gitdir is outside the worktree root
        if let Some(git_dir) = resolve_git_dir(worktree) {
            // Watch per-worktree gitdir non-recursively (HEAD, index, etc.)
            add_watch(
                watcher,
                &git_dir,
                RecursiveMode::NonRecursive,
                worktree,
                watch_to_worktrees,
                &mut watched,
            );

            // Watch common dir's refs/ for shared branch updates
            if let Some(common_dir) = resolve_common_git_dir(&git_dir) {
                let refs_dir = common_dir.join("refs");
                if refs_dir.is_dir() {
                    add_watch(
                        watcher,
                        &refs_dir,
                        RecursiveMode::Recursive,
                        worktree,
                        watch_to_worktrees,
                        &mut watched,
                    );
                }
                // Watch common dir non-recursively for packed-refs
                add_watch(
                    watcher,
                    &common_dir,
                    RecursiveMode::NonRecursive,
                    worktree,
                    watch_to_worktrees,
                    &mut watched,
                );
            }
        }
    } else if dot_git.is_dir() {
        // Normal worktree: watch .git/ non-recursively (HEAD, index, MERGE_HEAD, etc.)
        add_watch(
            watcher,
            &dot_git,
            RecursiveMode::NonRecursive,
            worktree,
            watch_to_worktrees,
            &mut watched,
        );
        // Watch refs/ recursively (low volume: branch creates/deletes/updates)
        let refs_dir = dot_git.join("refs");
        if refs_dir.is_dir() {
            add_watch(
                watcher,
                &refs_dir,
                RecursiveMode::Recursive,
                worktree,
                watch_to_worktrees,
                &mut watched,
            );
        }
    }

    // On macOS, also watch the worktree root for near-instant uncommitted change
    // detection. FSEvents handles heavy I/O efficiently via kernel-level aggregation.
    // On Linux, skip this and rely on the periodic poll sweep instead.
    if platform_supports_worktree_watches() {
        add_watch(
            watcher,
            worktree,
            RecursiveMode::Recursive,
            worktree,
            watch_to_worktrees,
            &mut watched,
        );
    }

    watched
}

/// Calculate the next timeout for the worker's recv_timeout.
/// Returns the shortest wait until either a debounced worktree is ready,
/// the full sweep is due, or a 1s cap for checking the term flag.
fn next_worker_timeout(
    pending: &HashMap<PathBuf, Instant>,
    debounce: Duration,
    last_sweep: Instant,
    sweep_interval: Duration,
) -> Duration {
    let now = Instant::now();
    let sweep_wait = sweep_interval.saturating_sub(last_sweep.elapsed());
    let mut min_wait = sweep_wait;

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

/// Refresh git status for a worktree path, updating the cache.
/// Returns true if the status actually changed (semantically, ignoring cached_at).
fn refresh_git_status(path: &Path, cache: &GitCache) -> bool {
    let new_status = crate::git::get_git_status(path, None);
    let changed = cache
        .lock()
        .ok()
        .map(|c| {
            c.get(path)
                .is_none_or(|old| !git_status_semantically_equal(old, &new_status))
        })
        .unwrap_or(true);
    if let Ok(mut c) = cache.lock() {
        c.insert(path.to_path_buf(), new_status);
    }
    changed
}

/// Info about an active agent path sent to the git worker.
struct GitWorkerPath {
    path: PathBuf,
    /// Whether this agent is stale (idle > threshold). Stale agents only
    /// get git status on the full sweep, not on every poll cycle.
    is_stale: bool,
}

/// Info about an active agent path sent to the PR worker.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct PrWorkerPath {
    path: PathBuf,
    branch: String,
}

type PrPathCache = Arc<Mutex<HashMap<PathBuf, PrPathEntry>>>;
type PrRepoCache = Arc<Mutex<HashMap<PathBuf, HashMap<String, PrSummary>>>>;

fn publish_pr_path_cache(
    entries: &[PrWorkerPath],
    repo_roots: &HashMap<PathBuf, PathBuf>,
    repo_cache: &HashMap<PathBuf, HashMap<String, PrSummary>>,
    path_cache: &PrPathCache,
    dirty_flag: &Arc<AtomicBool>,
    wake_tx: &std::sync::mpsc::SyncSender<()>,
) {
    let mut next = HashMap::new();
    for entry in entries {
        if let Some(repo_root) = repo_roots.get(&entry.path)
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

fn spawn_pr_worker(
    term: Arc<AtomicBool>,
    dirty_flag: Arc<AtomicBool>,
    wake_tx: std::sync::mpsc::SyncSender<()>,
) -> (PrPathCache, std::sync::mpsc::Sender<Vec<PrWorkerPath>>) {
    let path_cache: PrPathCache = Arc::new(Mutex::new(HashMap::new()));
    let path_cache_clone = path_cache.clone();
    let repo_cache: PrRepoCache = Arc::new(Mutex::new(crate::github::load_pr_cache()));
    let (tx, rx) = std::sync::mpsc::channel::<Vec<PrWorkerPath>>();

    thread::spawn(move || {
        let mut active_entries: Vec<PrWorkerPath> = Vec::new();
        let mut repo_roots: HashMap<PathBuf, PathBuf> = HashMap::new();
        let mut last_key: Vec<(PathBuf, String)> = Vec::new();
        let mut last_fetch = Instant::now() - Duration::from_secs(30);

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
                if paths_changed {
                    if let Ok(mut cache) = path_cache_clone.lock() {
                        cache.clear();
                    }
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
                for entry in &active_entries {
                    if !repo_roots.contains_key(&entry.path)
                        && let Ok(root) = crate::git::get_repo_root_for(&entry.path)
                    {
                        repo_roots.insert(entry.path.clone(), root);
                    }
                }
                let snapshot = repo_cache
                    .lock()
                    .ok()
                    .map(|c| c.clone())
                    .unwrap_or_default();
                publish_pr_path_cache(
                    &active_entries,
                    &repo_roots,
                    &snapshot,
                    &path_cache_clone,
                    &dirty_flag,
                    &wake_tx,
                );
            }

            if !branch_set_changed && last_fetch.elapsed() < Duration::from_secs(30) {
                continue;
            }

            let mut repo_branches: HashMap<PathBuf, Vec<String>> = HashMap::new();
            for entry in &active_entries {
                if let Some(repo_root) = repo_roots.get(&entry.path) {
                    repo_branches
                        .entry(repo_root.clone())
                        .or_default()
                        .push(entry.branch.clone());
                }
            }
            for branches in repo_branches.values_mut() {
                branches.sort();
                branches.dedup();
            }
            if repo_branches.is_empty() {
                last_key = key;
                continue;
            }

            let mut fetched = HashMap::new();
            for (repo_root, branches) in repo_branches {
                match crate::github::list_prs_for_branches(&repo_root, &branches) {
                    Ok(prs) => {
                        fetched.insert(repo_root, prs);
                    }
                    Err(e) => {
                        tracing::warn!("failed to fetch PRs for {:?}: {}", repo_root, e);
                    }
                }
            }
            if !fetched.is_empty()
                && let Ok(mut cache) = repo_cache.lock()
            {
                for (repo_root, prs) in &fetched {
                    if prs.is_empty() {
                        cache.remove(repo_root);
                    } else {
                        cache.insert(repo_root.clone(), prs.clone());
                    }
                }
                crate::github::save_pr_cache(&fetched);
                publish_pr_path_cache(
                    &active_entries,
                    &repo_roots,
                    &cache,
                    &path_cache_clone,
                    &dirty_flag,
                    &wake_tx,
                );
            }
            last_key = key;
            last_fetch = Instant::now();
        }
    });

    (path_cache, tx)
}

/// Spawn a background thread that watches for git changes and updates the cache.
///
/// Uses the `notify` crate for OS-level filesystem event detection (FSEvents on macOS).
/// Watches .git internals and worktree roots for each active worktree. Events are
/// debounced per-worktree (300ms) before triggering `get_git_status()`. A fallback
/// sweep runs every 30s for edge cases where the watcher might miss events.
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
            notify::Config::default(),
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
        // Maps: watched directory -> set of worktrees it covers
        let mut watch_to_worktrees: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        // Maps: worktree path -> list of watched paths for it
        let mut worktree_watches: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        // Per-worktree gitignore matchers for filtering irrelevant events
        let mut gitignores: HashMap<PathBuf, Gitignore> = HashMap::new();
        // Per-worktree: timestamp of last fs event (for debouncing)
        let mut pending_worktrees: HashMap<PathBuf, Instant> = HashMap::new();
        // Stale status per path (true = all agents at path are stale)
        let mut path_stale: HashMap<PathBuf, bool> = HashMap::new();
        // Track unique active paths for fallback polling
        let mut unique_active: Vec<PathBuf> = Vec::new();
        let mut last_full_sweep = Instant::now();
        let full_sweep_interval = if watcher.is_none() {
            // No watcher available: poll frequently as the only change detection
            Duration::from_secs(2)
        } else if platform_supports_worktree_watches() {
            // macOS: worktree watches give instant detection, sweep is just a safety net
            Duration::from_secs(30)
        } else {
            // Linux: only .git metadata is watched, working tree changes need polling.
            // 5s balances responsiveness with CPU cost (one git-status per worktree).
            Duration::from_secs(5)
        };
        let debounce_duration = Duration::from_millis(300);

        while !term.load(Ordering::Relaxed) {
            // Block on filesystem events (zero CPU when idle), or sleep briefly in poll mode
            if watcher.is_some() {
                let timeout = next_worker_timeout(
                    &pending_worktrees,
                    debounce_duration,
                    last_full_sweep,
                    full_sweep_interval,
                );
                // Process a single FS event: find affected worktrees,
                // skip gitignored paths, and mark worktrees as pending.
                let mut process_event = |event: notify::Event| {
                    for path in &event.paths {
                        // Rebuild gitignore matcher when .gitignore itself changes
                        if path.file_name().is_some_and(|n| n == ".gitignore") {
                            for wt in find_worktrees_for_path(path, &watch_to_worktrees) {
                                let gi = build_gitignore(&wt);
                                gitignores.insert(wt, gi);
                            }
                        }
                        for wt in find_worktrees_for_path(path, &watch_to_worktrees) {
                            if is_event_ignored(path, &wt, &gitignores) {
                                continue;
                            }
                            pending_worktrees.entry(wt).or_insert_with(Instant::now);
                        }
                    }
                };

                match fs_rx.recv_timeout(timeout) {
                    Ok(Ok(event)) => process_event(event),
                    Ok(Err(e)) => {
                        tracing::warn!("filesystem watch error: {}", e);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }

                // Drain any additional buffered events
                while let Ok(event_result) = fs_rx.try_recv() {
                    if let Ok(event) = event_result {
                        process_event(event);
                    }
                }

                // On channel overflow, mark all active worktrees as pending so
                // events for one noisy worktree can't starve updates to others.
                if fs_overflow.swap(false, Ordering::Relaxed) {
                    let now = Instant::now();
                    for path in worktree_watches.keys() {
                        pending_worktrees.entry(path.clone()).or_insert(now);
                    }
                }
            } else {
                // Poll-only fallback: sleep until next sweep check
                let sleep = full_sweep_interval
                    .saturating_sub(last_full_sweep.elapsed())
                    .min(Duration::from_secs(1));
                thread::sleep(sleep);
            }

            // Check for path updates (non-blocking)
            let mut paths_changed = false;
            while let Ok(entries) = rx.try_recv() {
                active_entries = entries;
                paths_changed = true;
            }

            if paths_changed {
                // Deduplicate paths. A path is stale only if ALL agents at that path are stale.
                path_stale.clear();
                for entry in &active_entries {
                    let e = path_stale.entry(entry.path.clone()).or_insert(true);
                    if !entry.is_stale {
                        *e = false;
                    }
                }
                unique_active = path_stale.keys().cloned().collect();
                unique_active.sort();
                let unique_set: HashSet<PathBuf> = unique_active.iter().cloned().collect();

                if let Some(ref mut w) = watcher {
                    // Remove watches for worktrees no longer active
                    let removed: Vec<PathBuf> = worktree_watches
                        .keys()
                        .filter(|p| !unique_set.contains(*p))
                        .cloned()
                        .collect();
                    for path in &removed {
                        if let Some(watched_paths) = worktree_watches.remove(path) {
                            for wp in &watched_paths {
                                remove_worktree_watch(w, wp, path, &mut watch_to_worktrees);
                            }
                        }
                        gitignores.remove(path);
                        pending_worktrees.remove(path);
                    }

                    // Add watches for new worktrees
                    for path in &unique_active {
                        if worktree_watches.contains_key(path) {
                            continue;
                        }
                        let watched = setup_worktree_watches(w, path, &mut watch_to_worktrees);
                        worktree_watches.insert(path.clone(), watched);
                        gitignores.insert(path.clone(), build_gitignore(path));
                        // Trigger immediate status fetch for new worktrees
                        pending_worktrees.insert(path.clone(), Instant::now() - debounce_duration);
                    }

                    // Prune cache for removed worktrees
                    if !removed.is_empty() {
                        if let Ok(mut c) = cache_clone.lock() {
                            c.retain(|p, _| unique_set.contains(p));
                        }
                        dirty_flag.store(true, Ordering::Relaxed);
                        let _ = wake_tx.try_send(());
                    }
                } else {
                    // Poll-only mode: just prune cache, no watches to manage
                    if let Ok(mut c) = cache_clone.lock() {
                        let before = c.len();
                        c.retain(|p, _| unique_set.contains(p));
                        if c.len() != before {
                            dirty_flag.store(true, Ordering::Relaxed);
                            let _ = wake_tx.try_send(());
                        }
                    }
                    // Trigger immediate fetch for new paths
                    for path in &unique_active {
                        if !cache_clone
                            .lock()
                            .ok()
                            .is_some_and(|c| c.contains_key(path))
                        {
                            pending_worktrees
                                .insert(path.clone(), Instant::now() - debounce_duration);
                        }
                    }
                }
            }

            // Process debounce-ready worktrees (skip stale ones, they only refresh on sweep)
            let now = Instant::now();
            let ready: Vec<PathBuf> = pending_worktrees
                .iter()
                .filter(|(_, last_event)| now.duration_since(**last_event) >= debounce_duration)
                .map(|(path, _)| path.clone())
                .collect();

            let mut any_changed = false;
            for path in &ready {
                pending_worktrees.remove(path);
                let is_stale = path_stale.get(path).copied().unwrap_or(false);
                if is_stale {
                    continue;
                }
                if refresh_git_status(path, &cache_clone) {
                    any_changed = true;
                }
            }

            // Fallback full sweep (30s with watcher, 2s without; includes stale worktrees)
            if last_full_sweep.elapsed() >= full_sweep_interval {
                last_full_sweep = Instant::now();
                let sweep_paths: Vec<PathBuf> = if watcher.is_some() {
                    worktree_watches.keys().cloned().collect()
                } else {
                    unique_active.clone()
                };
                for path in &sweep_paths {
                    pending_worktrees.remove(path);
                    if refresh_git_status(path, &cache_clone) {
                        any_changed = true;
                    }
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
                if let Err(mpsc::TrySendError::Full(_)) = fs_tx.try_send(event) {
                    overflow_clone.store(true, Ordering::Relaxed);
                }
            },
            notify::Config::default(),
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

        let interesting_basenames = ["config.yaml", "config.yml", ".workmux.yaml", ".workmux.yml"];

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
                    let interesting = event.paths.iter().any(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| interesting_basenames.contains(&n))
                    });
                    if interesting {
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
            entries: HashMap::new(),
            confirmed: HashMap::new(),
            timeout,
        }
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

/// Run the sidebar daemon (headless, no TUI).
pub fn run() -> Result<()> {
    let mux = create_backend(detect_backend());
    let instance_id = mux.instance_id();
    let config = Arc::new(Mutex::new(Config::load(None)?));
    // Captured at startup and intentionally not live-reloaded. tmux's
    // @workmux_pane_status holds the icon string itself; build_snapshot
    // compares pane statuses to these exact strings to suppress stale
    // done/waiting markers, so swapping the icons mid-run would mis-suppress.
    let status_icons = config.lock().unwrap().status_icons.clone();
    let config_version = Arc::new(AtomicU64::new(0));

    tracing::info!(instance_id = %instance_id, "sidebar daemon starting");

    // Signal handlers for clean shutdown and dirty notification
    let term = Arc::new(AtomicBool::new(false));
    let dirty_flag = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, term.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGUSR1, dirty_flag.clone())?;

    // Wake channel: replaces the 10ms spin loop. Producers send () to wake the
    // main loop immediately (SIGUSR1 uses the AtomicBool since signal handlers
    // can't send on channels; the wake channel handles git worker notifications).
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel::<()>(1);
    // Keep a sender alive so recv_timeout won't return Disconnected if the
    // git worker thread panics (which would spin the main loop at 100% CPU).
    let _wake_tx_keepalive = wake_tx.clone();

    let sock_path = socket_path(&instance_id);
    let _ = std::fs::remove_file(&sock_path); // Clean stale
    let server = SocketServer::bind(&sock_path)?;

    // Config watcher: bumps config_version on global / project .workmux.yaml changes.
    let config_paths_tx = spawn_config_watcher(
        term.clone(),
        config.clone(),
        config_version.clone(),
        dirty_flag.clone(),
        wake_tx.clone(),
    );

    // Background git status worker (shares dirty_flag for immediate broadcast on changes)
    let (git_cache, git_path_tx) =
        spawn_git_worker(term.clone(), dirty_flag.clone(), wake_tx.clone());
    let (pr_cache, pr_path_tx) = spawn_pr_worker(term.clone(), dirty_flag.clone(), wake_tx);

    // Store PID so toggle-off can kill us and hooks can signal us
    Cmd::new("tmux")
        .args(&[
            "set-option",
            "-g",
            "@workmux_sidebar_daemon_pid",
            &std::process::id().to_string(),
        ])
        .run()?;

    let mut inactivity_tracker = InactivityTracker::new(Duration::from_secs(10));
    let mut last_interrupted: HashSet<String> = HashSet::new();
    let mut last_runtime_write = Instant::now();
    let backend_name = mux.name().to_string();

    let mut last_refresh = Instant::now();
    let mut last_client_seen = Instant::now();
    let mut dirty_pending = false;
    let mut last_agent_list = String::new();
    let mut last_health_log = Instant::now();
    let refresh_interval = Duration::from_secs(2);
    let debounce_interval = Duration::from_millis(50);

    // Cache of agent_path -> project_config_dir so we don't run the walk-up
    // filesystem search on every tick. Misses (no config found) are NOT
    // cached, so a newly-created `.workmux.yaml` in or above an agent's path
    // is picked up on the next tick.
    let mut project_config_cache: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut last_config_dirs: HashSet<PathBuf> = HashSet::new();

    while !term.load(Ordering::Relaxed) {
        // Coalesce dirty signals: SIGUSR1 sets the flag, we service it once
        // per debounce interval to prevent signal floods from causing CPU storms
        if dirty_flag.swap(false, Ordering::Relaxed) {
            dirty_pending = true;
        }

        let time_since_refresh = last_refresh.elapsed();
        let debounce_cleared = dirty_pending && time_since_refresh >= debounce_interval;
        let timer_expired = time_since_refresh >= refresh_interval;

        if debounce_cleared || timer_expired {
            dirty_pending = false;
            last_refresh = Instant::now();

            // ── Gather inputs ──
            let tmux_state = query_tmux_state();
            let agents = StateStore::new()
                .and_then(|store| store.load_reconciled_agents(mux.as_ref()))
                .ok();
            let Some(agents) = agents else { continue };

            let (position, layout_mode) = {
                let cfg = config.lock().unwrap();
                (
                    super::read_sidebar_position(&cfg),
                    read_sidebar_layout_mode(&cfg).unwrap_or_default(),
                )
            };
            let filter_mode = read_sidebar_filter_mode();
            let sleeping_pane_ids = read_sleeping_panes();
            let git_statuses = git_cache.lock().ok().map(|c| c.clone()).unwrap_or_default();
            let pr_statuses = pr_cache.lock().ok().map(|c| c.clone()).unwrap_or_default();
            let captured_panes = gather_captures(&agents, mux.as_ref(), &inactivity_tracker);
            let now = Instant::now();
            let now_ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let heartbeat_due = last_runtime_write.elapsed() >= Duration::from_secs(10);

            // ── Compute tick (no I/O) ──
            let mut output = compute_tick(
                TickInput {
                    agents,
                    tmux_state,
                    captured_panes,
                    now,
                    now_ts,
                    position,
                    layout_mode,
                    filter_mode,
                    git_statuses,
                    pr_statuses,
                    sleeping_pane_ids,
                },
                &mut inactivity_tracker,
                &last_interrupted,
                &status_icons,
                heartbeat_due,
            );

            // ── Apply side effects, then commit state ──
            if let Ok(store) = StateStore::new()
                && apply_tick_effects(&output, &store, &backend_name, &instance_id)
            {
                last_runtime_write = Instant::now();
            }
            last_interrupted = output.next_interrupted;

            // ── Stamp config version + broadcast ──
            output.snapshot.config_version = config_version.load(Ordering::Relaxed);
            server.broadcast(&output.snapshot);

            // Update git worker with current agent paths and stale status
            let stale_threshold = 60 * 60; // 1 hour, matches sidebar UI
            let entries: Vec<GitWorkerPath> = output
                .snapshot
                .agents
                .iter()
                .map(|a| GitWorkerPath {
                    path: a.path.clone(),
                    is_stale: a
                        .status_ts
                        .map(|ts| now_ts.saturating_sub(ts) > stale_threshold)
                        .unwrap_or(false),
                })
                .collect();
            let _ = git_path_tx.send(entries);

            let pr_entries: Vec<PrWorkerPath> = output
                .snapshot
                .agents
                .iter()
                .filter_map(|a| {
                    let branch = output.snapshot.git_statuses.get(&a.path)?.branch.as_ref()?;
                    if branch == "main" || branch == "master" {
                        return None;
                    }
                    Some(PrWorkerPath {
                        path: a.path.clone(),
                        branch: branch.clone(),
                    })
                })
                .collect();
            let _ = pr_path_tx.send(pr_entries);

            // Update config watcher with current project-config dirs.
            // find_project_config does fs walks, so cache by agent path.
            let live_paths: HashSet<PathBuf> = output
                .snapshot
                .agents
                .iter()
                .map(|a| a.path.clone())
                .collect();
            project_config_cache.retain(|p, _| live_paths.contains(p));
            let mut config_dirs: HashSet<PathBuf> = HashSet::new();
            for a in &output.snapshot.agents {
                let dir = if let Some(d) = project_config_cache.get(&a.path) {
                    Some(d.clone())
                } else {
                    let found = crate::config::find_project_config(&a.path)
                        .ok()
                        .flatten()
                        .map(|loc| loc.config_dir);
                    if let Some(ref d) = found {
                        project_config_cache.insert(a.path.clone(), d.clone());
                    }
                    found
                };
                if let Some(d) = dir {
                    config_dirs.insert(d);
                }
            }
            if config_dirs != last_config_dirs {
                let _ = config_paths_tx.send(config_dirs.clone());
                last_config_dirs = config_dirs;
            }

            let agent_list: String = output
                .snapshot
                .agents
                .iter()
                .map(|a| a.pane_id.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            if agent_list != last_agent_list {
                if !agent_list.is_empty() {
                    let _ = Cmd::new("tmux")
                        .args(&["set-option", "-g", "@workmux_sidebar_agents", &agent_list])
                        .run();
                } else {
                    let _ = Cmd::new("tmux")
                        .args(&["set-option", "-gu", "@workmux_sidebar_agents"])
                        .run();
                }
                last_agent_list = agent_list;
            }
        }

        // Track client activity for auto-exit
        let cc = server.client_count();
        if cc > 0 {
            last_client_seen = Instant::now();
        } else if last_client_seen.elapsed() > Duration::from_secs(10) {
            tracing::info!("sidebar daemon exiting: no clients for 10s");
            break;
        }

        // Periodic health log (every 60s)
        if last_health_log.elapsed() >= Duration::from_secs(60) {
            tracing::info!(clients = cc, "sidebar daemon alive");
            last_health_log = Instant::now();
        }

        // Block until woken by a producer or next refresh is due.
        // SIGUSR1 sets dirty_flag (can't use channels from signal handlers),
        // so we cap the wait at 100ms to check it, but otherwise block fully.
        let wait = if dirty_pending {
            debounce_interval.saturating_sub(last_refresh.elapsed())
        } else {
            refresh_interval
                .saturating_sub(last_refresh.elapsed())
                .min(Duration::from_millis(100))
        };
        let _ = wake_rx.recv_timeout(wait);
    }

    if term.load(Ordering::Relaxed) {
        tracing::info!("sidebar daemon exiting: SIGTERM received");
    }

    // Cleanup
    let _ = std::fs::remove_file(&sock_path);
    if let Ok(store) = StateStore::new() {
        store.delete_runtime(&backend_name, &instance_id);
    }
    let _ = Cmd::new("tmux")
        .args(&["set-option", "-gu", "@workmux_sidebar_daemon_pid"])
        .run();
    let _ = Cmd::new("tmux")
        .args(&["set-option", "-gu", "@workmux_sidebar_agents"])
        .run();
    let _ = Cmd::new("tmux")
        .args(&["set-option", "-gu", "@workmux_sleeping_panes"])
        .run();
    let _ = Cmd::new("tmux")
        .args(&["set-option", "-gu", "@workmux_sidebar_scope"])
        .run();
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
    git_statuses: HashMap<PathBuf, GitStatus>,
    pr_statuses: HashMap<PathBuf, PrPathEntry>,
    sleeping_pane_ids: HashSet<String>,
}

/// A state-file write to apply after computing the tick.
struct AgentWrite {
    pane_id: String,
    status_ts: u64,
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
/// 2. Mutates agents in memory (status_ts reset for resumed agents)
/// 3. Builds the snapshot from the already-mutated agents
/// 4. Returns side effects (state file writes, runtime file write)
#[allow(clippy::too_many_arguments)]
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
        git_statuses,
        pr_statuses,
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
                agent_writes.push(AgentWrite {
                    pane_id: agent.pane_id.clone(),
                    status_ts: now_ts,
                });
            }
        }
    }

    // Phase 3: Build snapshot from already-mutated agents
    let mut snapshot = build_snapshot(
        agents,
        &tmux_state.window_statuses,
        &tmux_state.pane_window_ids,
        tmux_state.active_windows,
        tmux_state.active_pane_ids,
        tmux_state.window_pane_counts,
        position,
        layout_mode,
        filter_mode,
        status_icons,
        git_statuses,
        pr_statuses,
        &sleeping_pane_ids,
    );
    snapshot.interrupted_pane_ids = interrupted.clone();

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
        if let Ok(Some(mut state)) = store.get_agent(&pane_key) {
            state.status_ts = Some(write.status_ts);
            let _ = store.upsert_agent(&state);
        }
    }

    if let Some(ref runtime) = output.runtime_write {
        let _ = store.write_runtime(backend, instance, runtime);
        true
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
    use crate::multiplexer::{AgentPane, AgentStatus};
    use std::cell::RefCell;
    use std::path::PathBuf;

    fn working_agent(pane_id: &str, updated_ts: u64) -> AgentPane {
        AgentPane {
            session: String::new(),
            window_name: String::new(),
            pane_id: pane_id.to_string(),
            window_id: String::new(),
            path: PathBuf::new(),
            pane_title: None,
            status: Some(AgentStatus::Working),
            status_ts: Some(100),
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
        let entries = vec![PrWorkerPath {
            path: path.clone(),
            branch: "feature".to_string(),
        }];
        let repo_roots = HashMap::from([(path.clone(), repo_root.clone())]);
        let repo_cache =
            HashMap::from([(repo_root, HashMap::from([("feature".to_string(), summary)]))]);
        let path_cache: PrPathCache = Arc::new(Mutex::new(HashMap::new()));
        let dirty_flag = Arc::new(AtomicBool::new(false));
        let (wake_tx, _wake_rx) = std::sync::mpsc::sync_channel(1);

        publish_pr_path_cache(
            &entries,
            &repo_roots,
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
                pane_title: None,
                pane_pid: 1,
                command: "node".to_string(),
                updated_ts,
                window_name: None,
                session_name: None,
                boot_id: None,
                agent_kind: None,
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
                        window_statuses: HashMap::new(),
                        active_windows: HashSet::new(),
                        pane_window_ids: HashMap::new(),
                        active_pane_ids: HashSet::new(),
                        window_pane_counts: HashMap::new(),
                    },
                    captured_panes: captures,
                    now,
                    now_ts,
                    position: SidebarPosition::Left,
                    layout_mode: SidebarLayoutMode::default(),
                    filter_mode: SidebarFilterMode::default(),
                    git_statuses: HashMap::new(),
                    pr_statuses: HashMap::new(),
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

            // Side effect says to write it to disk
            assert_eq!(output.agent_writes.len(), 1);
            assert_eq!(output.agent_writes[0].status_ts, 1012);

            // Apply effects and verify store
            apply_tick_effects(&output, &store, BACKEND, INSTANCE);
            assert_eq!(
                store.get_agent(&pane_key("%1")).unwrap().unwrap().status_ts,
                Some(1012)
            );
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
            assert_eq!(
                store.get_agent(&pane_key("%1")).unwrap().unwrap().status_ts,
                Some(1012)
            );
            assert_eq!(
                store.get_agent(&pane_key("%2")).unwrap().unwrap().status_ts,
                Some(200)
            );
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
            assert!(!output.snapshot.interrupted_pane_ids.contains("%1"));
        }
    }
}
