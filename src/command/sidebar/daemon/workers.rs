//! Background workers that keep git, GitHub and project config state fresh
//! between observations.

#[cfg(target_os = "macos")]
use super::super::fsevent::FsEventWatcher as SidebarWatcher;
use anyhow::Result;
use ignore::gitignore::Gitignore;
#[cfg(not(target_os = "macos"))]
use notify::RecommendedWatcher as SidebarWatcher;
use notify::{EventKindMask, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::git::GitStatus;
use crate::github::{CheckSummary, PrSummary};

use super::super::snapshot::{CheckPathEntry, PrPathEntry};

/// Shared git status cache, updated by a background worker thread.
type GitCache = Arc<Mutex<HashMap<PathBuf, GitStatus>>>;

pub(super) fn snapshot_status_cache<T: Clone>(
    enabled: bool,
    cache: &Arc<Mutex<HashMap<PathBuf, T>>>,
) -> HashMap<PathBuf, T> {
    if !enabled {
        return HashMap::new();
    }
    cache
        .lock()
        .ok()
        .map(|cache| cache.clone())
        .unwrap_or_default()
}

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
pub(super) fn git_status_semantically_equal(a: &GitStatus, b: &GitStatus) -> bool {
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
    watcher: &mut SidebarWatcher,
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
    watcher: &mut SidebarWatcher,
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
    watcher: &mut SidebarWatcher,
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
    watcher: &mut SidebarWatcher,
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
    watcher: &mut SidebarWatcher,
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
    watcher: &mut SidebarWatcher,
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
fn refresh_git_status(
    worktree: &Path,
    agent_paths: &[PathBuf],
    cache: &GitCache,
    computations: &mut crate::git::GitStatusCache,
) -> bool {
    let new_status = crate::git::get_git_status_cached(worktree, None, computations);
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
pub(super) struct GitWorkerPath {
    pub(super) path: PathBuf,
    pub(super) is_stale: bool,
    pub(super) is_focused: bool,
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
pub(super) struct GithubWorkerPath {
    pub(super) path: PathBuf,
    pub(super) branch: String,
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
    crate::git::get_repository_key_in(Some(path)).ok()
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

pub(super) fn spawn_github_worker(
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
pub(super) fn spawn_git_worker(
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
        let mut watcher: Option<SidebarWatcher> = match SidebarWatcher::new(
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

        let mut computations = crate::git::GitStatusCache::default();
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
                    computations.retain(&unique_set);
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
                    if refresh_git_status(
                        &path,
                        &worktree.agent_paths,
                        &cache_clone,
                        &mut computations,
                    ) {
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

const PROJECT_CONFIG_REVALIDATE: Duration = Duration::from_secs(30);

struct ProjectConfigEntry {
    observation: Option<crate::config::ProjectConfigDiscovery>,
    retry_at: Instant,
    failures: u32,
}

#[derive(Default)]
pub(super) struct ProjectConfigCache {
    entries: HashMap<PathBuf, ProjectConfigEntry>,
    next_sweep: Option<Instant>,
}

pub(super) struct ProjectConfigResolution {
    pub(super) directories: HashSet<PathBuf>,
    pub(super) changed: bool,
    pub(super) reconciled: bool,
}

impl ProjectConfigCache {
    pub(super) fn resolve(
        &mut self,
        live_paths: &HashSet<PathBuf>,
        now: Instant,
        invalidated: bool,
        mut discover: impl FnMut(&Path) -> Result<crate::config::ProjectConfigDiscovery>,
    ) -> ProjectConfigResolution {
        self.entries.retain(|path, _| live_paths.contains(path));
        let sweep = self.next_sweep.is_none_or(|deadline| now >= deadline);
        if sweep {
            self.next_sweep = Some(now + PROJECT_CONFIG_REVALIDATE);
        }
        let mut changed = false;
        for path in live_paths {
            let entry = self
                .entries
                .entry(path.clone())
                .or_insert(ProjectConfigEntry {
                    observation: None,
                    retry_at: now,
                    failures: 0,
                });
            if !sweep && !invalidated && now < entry.retry_at {
                continue;
            }
            match discover(path) {
                Ok(observation) => {
                    changed |= entry.failures > 0
                        || entry
                            .observation
                            .as_ref()
                            .is_some_and(|old| old != &observation);
                    entry.observation = Some(observation);
                    entry.failures = 0;
                    entry.retry_at = now + PROJECT_CONFIG_REVALIDATE;
                }
                Err(error) => {
                    entry.failures = entry.failures.saturating_add(1);
                    let backoff = Duration::from_secs(1 << entry.failures.min(5))
                        .min(PROJECT_CONFIG_REVALIDATE);
                    entry.retry_at = now + backoff;
                    tracing::debug!(path = %path.display(), %error, "project config discovery failed");
                }
            }
        }
        ProjectConfigResolution {
            directories: self
                .entries
                .values()
                .filter_map(|entry| {
                    entry
                        .observation
                        .as_ref()?
                        .location
                        .as_ref()
                        .map(|location| location.config_dir.clone())
                })
                .collect(),
            // Watcher invalidation already requests a client reload.
            changed: changed && !invalidated,
            reconciled: sweep || invalidated,
        }
    }
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

pub(super) fn reload_daemon_config(config: &Mutex<Config>) {
    match Config::load(None) {
        Ok(new_cfg) => {
            if let Ok(mut slot) = config.lock() {
                *slot = new_cfg;
            }
            tracing::debug!("daemon config reloaded");
        }
        Err(error) => {
            tracing::warn!(%error, "daemon-side config load failed, keeping previous");
        }
    }
}

pub(super) struct ConfigWatchPaths {
    directories: HashSet<PathBuf>,
    #[cfg(test)]
    reconciled: Option<mpsc::Sender<()>>,
}

impl From<HashSet<PathBuf>> for ConfigWatchPaths {
    fn from(directories: HashSet<PathBuf>) -> Self {
        Self {
            directories,
            #[cfg(test)]
            reconciled: None,
        }
    }
}

/// Spawn a thread that watches the global config file and per-project
/// `.workmux.yaml` files and bumps `config_version` whenever a reload succeeds.
///
/// Returns a channel for the daemon main loop to send the current set of
/// project config directories (parents of `.workmux.yaml`) to watch.
pub(super) fn spawn_config_watcher(
    term: Arc<AtomicBool>,
    config: Arc<Mutex<Config>>,
    config_version: Arc<AtomicU64>,
    dirty_flag: Arc<AtomicBool>,
    wake_tx: mpsc::SyncSender<()>,
) -> mpsc::Sender<ConfigWatchPaths> {
    let (paths_tx, paths_rx) = mpsc::channel::<ConfigWatchPaths>();
    thread::spawn(move || {
        // Bounded fs event channel; on overflow force a reload.
        let (fs_tx, fs_rx) = mpsc::sync_channel::<notify::Result<notify::Event>>(64);
        let overflow = Arc::new(AtomicBool::new(false));
        let overflow_clone = overflow.clone();
        let mut watcher: SidebarWatcher = match SidebarWatcher::new(
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
        let mut watched_project_dirs: HashMap<PathBuf, Option<(u64, u64)>> = HashMap::new();
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
            while let Ok(request) = paths_rx.try_recv() {
                let new_dirs = request.directories;
                // A watch belongs to a directory inode, not merely its pathname.
                use std::os::unix::fs::MetadataExt;
                let identities: HashMap<_, _> = new_dirs
                    .iter()
                    .filter_map(|dir| {
                        let metadata = std::fs::metadata(dir).ok()?;
                        Some((dir.clone(), (metadata.dev(), metadata.ino())))
                    })
                    .collect();
                let to_remove: Vec<PathBuf> = watched_project_dirs
                    .iter()
                    .filter(|(dir, identity)| {
                        !new_dirs.contains(*dir) || **identity != identities.get(*dir).copied()
                    })
                    .map(|(dir, _)| dir.clone())
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
                    .iter()
                    .filter(|dir| !watched_project_dirs.contains_key(*dir))
                    .cloned()
                    .collect();
                for dir in to_add {
                    // Skip if it's the same as the global watched dir to avoid
                    // double-watching the same path.
                    if Some(&dir) == watched_global.as_ref() {
                        watched_project_dirs.insert(dir.clone(), identities.get(&dir).copied());
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
                            watched_project_dirs.insert(dir.clone(), identities.get(&dir).copied());
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
                #[cfg(test)]
                if let Some(reconciled) = request.reconciled {
                    let _ = reconciled.send(());
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
                reload_daemon_config(&config);
                let v = config_version.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::info!(version = v, "sidebar config_version bumped");
                dirty_flag.store(true, Ordering::Relaxed);
                let _ = wake_tx.try_send(());
            }
        }
    });

    paths_tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::CheckState;

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

    #[cfg(target_os = "macos")]
    #[test]
    fn git_watch_churn_delivers_events_and_stops() {
        const TEST: &str =
            "command::sidebar::daemon::workers::tests::git_watch_churn_delivers_events_and_stops";
        run_git_watch_churn(TEST, 100, Duration::from_secs(180));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "manual FSEvents watch churn stress test"]
    fn git_watch_churn_stress_delivers_events_and_stops() {
        const TEST: &str = "command::sidebar::daemon::workers::tests::git_watch_churn_stress_delivers_events_and_stops";
        run_git_watch_churn(TEST, 10_000, Duration::from_secs(900));
    }

    #[cfg(target_os = "macos")]
    fn run_git_watch_churn(test_name: &str, iterations: usize, timeout: Duration) {
        let temp = tempfile::tempdir().unwrap();
        if !crate::test_support::is_isolated_child(test_name) {
            crate::test_support::run_isolated_test_with_timeout(
                test_name,
                temp.path(),
                &[],
                timeout,
            );
            return;
        }
        let root = temp.path().canonicalize().unwrap();
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut watcher = SidebarWatcher::new(
            move |event| {
                let _ = tx.send(event);
            },
            mutation_watcher_config(),
        )
        .unwrap();
        let mut reverse = HashMap::new();
        assert!(add_watch(
            &mut watcher,
            &a,
            RecursiveMode::Recursive,
            &a,
            &mut reverse
        ));
        // Sibling roots require stream restarts even on backends that coalesce
        // nested watches. Each registration immediately follows a fresh start.
        let churn_started = Instant::now();
        for iteration in 0..iterations {
            if iteration % 100 == 0 {
                eprintln!(
                    "watch churn {iteration}/{iterations}: {:?}",
                    churn_started.elapsed()
                );
            }
            assert!(add_watch(
                &mut watcher,
                &b,
                RecursiveMode::NonRecursive,
                &b,
                &mut reverse
            ));
            remove_worktree_watch(&mut watcher, &b, &b, &mut reverse);
        }
        eprintln!("watch churn complete: {:?}", churn_started.elapsed());
        let changed = a.join("changed");
        std::fs::write(&changed, "mutation").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("watcher did not deliver a mutation after churn")
                .expect("watcher reported an error");
            if event.paths.contains(&changed) {
                break;
            }
        }
        // Destruction must finish before the child can report success.
        drop(watcher);
        println!("{}", crate::test_support::ISOLATED_TEST_CANARY);
    }

    #[test]
    fn project_config_watcher_recovers_registration_and_directory_replacement() {
        const TEST: &str = "command::sidebar::daemon::workers::tests::project_config_watcher_recovers_registration_and_directory_replacement";
        if !crate::test_support::is_isolated_child(TEST) {
            let temp = tempfile::tempdir().unwrap();
            crate::test_support::run_isolated_test(
                TEST,
                temp.path(),
                &[("HOME", temp.path()), ("XDG_CONFIG_HOME", temp.path())],
            );
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("project");
        let global = crate::config::global_config_path().unwrap();
        let global_dir = global.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&global_dir).unwrap();
        let term = Arc::new(AtomicBool::new(false));
        let version = Arc::new(AtomicU64::new(0));
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let paths_tx = spawn_config_watcher(
            term.clone(),
            Arc::new(Mutex::new(Config::default())),
            version.clone(),
            Arc::new(AtomicBool::new(false)),
            wake_tx,
        );
        let reconcile = |directories| {
            let (tx, rx) = mpsc::channel();
            paths_tx
                .send(ConfigWatchPaths {
                    directories,
                    reconciled: Some(tx),
                })
                .unwrap();
            rx.recv_timeout(Duration::from_secs(10))
                .expect("config watch reconciliation did not finish");
        };
        let desired = HashSet::from([dir.clone(), global_dir]);
        // Complete the missing-directory registration attempt before creating it.
        reconcile(desired.clone());
        std::fs::create_dir(&dir).unwrap();
        reconcile(desired.clone());
        let config_path = dir.join(".workmux.yaml");
        std::fs::write(&config_path, "agent: first").unwrap();
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(version.load(Ordering::Relaxed) > 0);
        std::fs::write(dir.join("temporary"), "agent: atomic").unwrap();
        std::fs::rename(dir.join("temporary"), &config_path).unwrap();
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        std::fs::write(&config_path, "agent: after-atomic").unwrap();
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        std::fs::rename(&dir, temp.path().join("old-project")).unwrap();
        std::fs::create_dir(&dir).unwrap();
        reconcile(desired);
        while wake_rx.try_recv().is_ok() {}
        let before = version.load(Ordering::Relaxed);
        std::fs::write(&config_path, "agent: after-directory-replacement").unwrap();
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(version.load(Ordering::Relaxed) > before);
        // Removing project membership must not remove the shared global watch.
        reconcile(HashSet::from([dir]));
        while wake_rx.try_recv().is_ok() {}
        let before = version.load(Ordering::Relaxed);
        std::fs::write(global, "agent: global-reload").unwrap();
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(version.load(Ordering::Relaxed) > before);
        term.store(true, Ordering::Relaxed);
        println!("{}", crate::test_support::ISOLATED_TEST_CANARY);
    }

    #[test]
    fn project_config_cache_caches_absence_and_deduplicates_paths() {
        let temp = tempfile::tempdir().unwrap();
        let absent = crate::config::discover_project_config(temp.path()).unwrap();
        let paths: HashSet<_> = [temp.path().to_path_buf(), temp.path().to_path_buf()].into();
        let mut cache = ProjectConfigCache::default();
        let now = Instant::now();
        let mut calls = 0;
        for tick in 0..15 {
            let result = cache.resolve(&paths, now + Duration::from_secs(tick * 2), false, |_| {
                calls += 1;
                Ok(absent.clone())
            });
            assert!(result.directories.is_empty());
            assert!(!result.changed);
        }
        assert_eq!(
            calls, 1,
            "30 agent discoveries across 15 publications become one"
        );
        cache.resolve(&paths, now + PROJECT_CONFIG_REVALIDATE, false, |_| {
            calls += 1;
            Ok(absent.clone())
        });
        assert_eq!(calls, 2);
        cache.resolve(&HashSet::new(), now, false, |_| unreachable!());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn project_config_cache_revalidates_files_and_repository_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let nested = root.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let paths = HashSet::from([nested.clone()]);
        let mut cache = ProjectConfigCache::default();
        let mut now = Instant::now();
        let first = cache.resolve(&paths, now, false, crate::config::discover_project_config);
        assert!(first.directories.is_empty());
        crate::test_support::init_repo(&root);
        now += PROJECT_CONFIG_REVALIDATE;
        assert!(
            cache
                .resolve(&paths, now, false, crate::config::discover_project_config)
                .changed
        );
        std::fs::write(root.join(".workmux.yml"), "agent: root").unwrap();
        now += PROJECT_CONFIG_REVALIDATE;
        let created = cache.resolve(&paths, now, false, crate::config::discover_project_config);
        assert!(created.changed);
        assert_eq!(created.directories, HashSet::from([root.clone()]));
        std::fs::write(nested.join(".workmux.yaml"), "agent: nearer").unwrap();
        now += PROJECT_CONFIG_REVALIDATE;
        let nearer = cache.resolve(&paths, now, false, crate::config::discover_project_config);
        assert!(nearer.changed);
        assert_eq!(nearer.directories, HashSet::from([nested.clone()]));
        std::fs::write(nested.join("replacement"), "agent: replaced").unwrap();
        std::fs::rename(nested.join("replacement"), nested.join(".workmux.yaml")).unwrap();
        now += PROJECT_CONFIG_REVALIDATE;
        assert!(
            cache
                .resolve(&paths, now, false, crate::config::discover_project_config)
                .changed
        );
        // An external watcher reload already accounts for a deletion/move.
        std::fs::remove_file(nested.join(".workmux.yaml")).unwrap();
        let moved = cache.resolve(&paths, now, true, crate::config::discover_project_config);
        assert!(!moved.changed);
        assert_eq!(moved.directories, HashSet::from([root.clone()]));
        std::fs::remove_file(root.join(".workmux.yml")).unwrap();
        now += PROJECT_CONFIG_REVALIDATE;
        let deleted = cache.resolve(&paths, now, false, crate::config::discover_project_config);
        assert!(deleted.changed);
        assert!(deleted.directories.is_empty());
        // Replace a repository at identical paths with no config on either side.
        std::fs::rename(root.join(".git"), root.join("old-git")).unwrap();
        crate::test_support::init_repo(&root);
        now += PROJECT_CONFIG_REVALIDATE;
        assert!(
            cache
                .resolve(&paths, now, false, crate::config::discover_project_config)
                .changed
        );
        // A nested repository takes ownership of an unchanged agent path.
        crate::test_support::init_repo(&nested);
        now += PROJECT_CONFIG_REVALIDATE;
        assert!(
            cache
                .resolve(&paths, now, false, crate::config::discover_project_config)
                .changed
        );
    }

    #[test]
    fn project_config_cache_keeps_watches_on_error_and_backs_off() {
        let repo = tempfile::tempdir().unwrap();
        crate::test_support::init_repo(repo.path());
        std::fs::write(repo.path().join(".workmux.yaml"), "agent: test").unwrap();
        let path = repo.path().canonicalize().unwrap();
        let paths = HashSet::from([path.clone()]);
        let observation = crate::config::discover_project_config(&path).unwrap();
        let now = Instant::now();
        let mut cache = ProjectConfigCache::default();
        cache.resolve(&paths, now, false, |_| Ok(observation.clone()));
        let mut calls = 0;
        for second in 0..60 {
            let result = cache.resolve(
                &paths,
                now + Duration::from_secs(second),
                second == 0,
                |_| {
                    calls += 1;
                    anyhow::bail!("recoverable failure")
                },
            );
            assert_eq!(result.directories, paths);
            assert!(!result.changed);
        }
        assert!(calls <= 6, "failure retry calls: {calls}");
        let recovered = cache.resolve(&paths, now + Duration::from_secs(60), false, |_| {
            Ok(observation.clone())
        });
        assert!(recovered.changed);
        assert_eq!(cache.entries[&path].failures, 0);
        assert_eq!(recovered.directories, paths);
    }

    #[test]
    fn project_config_cache_revalidates_linked_worktree_reassignment_and_symlinks() {
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("a");
        let b = fixture.path().join("b");
        for repo in [&a, &b] {
            std::fs::create_dir(repo).unwrap();
            crate::test_support::init_repo(repo);
            std::fs::write(repo.join(".workmux.yaml"), "agent: main").unwrap();
        }
        let linked = fixture.path().join("linked");
        crate::test_support::run_git(
            &a,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let paths = HashSet::from([linked.clone()]);
        let mut cache = ProjectConfigCache::default();
        let now = Instant::now();
        let first = cache.resolve(&paths, now, false, crate::config::discover_project_config);
        assert_eq!(
            first.directories,
            HashSet::from([a.canonicalize().unwrap()])
        );
        crate::test_support::run_git(
            &a,
            &["worktree", "remove", "--force", linked.to_str().unwrap()],
        );
        let failed = cache.resolve(&paths, now, true, crate::config::discover_project_config);
        assert_eq!(failed.directories, first.directories);
        crate::test_support::run_git(
            &b,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let reassigned = cache.resolve(
            &paths,
            now + Duration::from_secs(2),
            false,
            crate::config::discover_project_config,
        );
        assert!(reassigned.changed);
        assert_eq!(
            reassigned.directories,
            HashSet::from([b.canonicalize().unwrap()])
        );
        let symlink = fixture.path().join("symlink");
        std::os::unix::fs::symlink(&a, &symlink).unwrap();
        let paths = HashSet::from([symlink.clone()]);
        cache.resolve(&paths, now, false, crate::config::discover_project_config);
        std::fs::remove_file(&symlink).unwrap();
        std::os::unix::fs::symlink(&b, &symlink).unwrap();
        let retargeted = cache.resolve(
            &paths,
            now + PROJECT_CONFIG_REVALIDATE,
            false,
            crate::config::discover_project_config,
        );
        assert!(retargeted.changed);
        assert_eq!(
            retargeted.directories,
            HashSet::from([b.canonicalize().unwrap()])
        );
    }

    #[test]
    fn project_config_cache_independent_repositories_and_shared_watches() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for repo in [&a, &b] {
            crate::test_support::init_repo(repo.path());
            std::fs::write(repo.path().join(".workmux.yaml"), "agent: test").unwrap();
            std::fs::create_dir(repo.path().join("nested")).unwrap();
        }
        let a = a.path().canonicalize().unwrap();
        let b = b.path().canonicalize().unwrap();
        let now = Instant::now();
        let mut cache = ProjectConfigCache::default();
        let paths = HashSet::from([a.clone(), a.join("nested"), b.clone()]);
        let result = cache.resolve(&paths, now, false, crate::config::discover_project_config);
        assert_eq!(result.directories, HashSet::from([a.clone(), b]));
        let result = cache.resolve(
            &HashSet::from([a.join("nested")]),
            now,
            false,
            |_| unreachable!(),
        );
        assert_eq!(result.directories, HashSet::from([a]));
        assert!(!result.changed);
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
            closing_issues: Vec::new(),
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

    #[test]
    fn mutation_watchers_exclude_access_events() {
        assert_eq!(mutation_watcher_config().event_kinds(), EventKindMask::CORE);
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
                &cache,
                &mut crate::git::GitStatusCache::default()
            ));
            assert_eq!(
                cache.lock().unwrap()[&child].branch.as_deref(),
                Some("parent")
            );

            let mut watcher = SidebarWatcher::new(
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
                &cache,
                &mut crate::git::GitStatusCache::default()
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
                    &cache,
                    &mut crate::git::GitStatusCache::default()
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
    fn disabled_status_collection_hides_cached_values() {
        let cache = Arc::new(Mutex::new(HashMap::from([(
            PathBuf::from("/repo"),
            GitStatus {
                is_dirty: true,
                ..Default::default()
            },
        )])));

        assert_eq!(snapshot_status_cache(true, &cache).len(), 1);
        assert!(snapshot_status_cache(false, &cache).is_empty());
        assert_eq!(cache.lock().unwrap().len(), 1);
    }

    #[test]
    fn git_worker_clears_cache_when_collection_is_disabled() {
        struct StopWorker(Arc<AtomicBool>);
        impl Drop for StopWorker {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo);
        let term = Arc::new(AtomicBool::new(false));
        let _stop = StopWorker(term.clone());
        let dirty = Arc::new(AtomicBool::new(false));
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let (cache, paths_tx) = spawn_git_worker(term, dirty, wake_tx);
        paths_tx
            .send(vec![GitWorkerPath {
                path: repo.clone(),
                is_stale: false,
                is_focused: true,
            }])
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while cache.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "worker did not populate cache");
            let _ = wake_rx.recv_timeout(Duration::from_millis(50));
        }

        paths_tx.send(Vec::new()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cache.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "worker did not clear cache");
            let _ = wake_rx.recv_timeout(Duration::from_millis(50));
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
                    closing_issues: Vec::new(),
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
            closing_issues: Vec::new(),
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
}
