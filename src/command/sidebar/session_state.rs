use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::config::{SidebarHeight, SidebarWidth};

use super::preferences::Preferences;
use super::view::WindowView;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct SidebarSessionState {
    #[serde(flatten)]
    pub scope: FileScope,
    pub width: Option<SidebarWidth>,
    pub height: Option<SidebarHeight>,
    #[serde(flatten)]
    pub prefs: Preferences,
    pub ordered_agents: Vec<String>,
    /// The view each window's sidebar shows, by window ID. Rex and Zellij only.
    pub window_views: BTreeMap<String, WindowView>,
    pub daemon_pid: Option<u32>,
    /// Sidebars the daemon placed, by window ID. Rex only.
    pub placements: BTreeMap<String, Placement>,
    /// Windows whose sidebar the user closed. They get no new sidebar until
    /// the sidebar is turned on again. Rex only.
    pub dismissed: BTreeSet<String>,
    /// The server boot that `placements` and `dismissed` belong to.
    pub boot_id: Option<String>,
}

/// A sidebar the daemon placed and the size it last gave it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "PlacementRecord", into = "PlacementRecord")]
pub(super) struct Placement {
    pub block_id: String,
    /// The root split ratio the daemon last set or accepted.
    pub applied_ratio: f64,
    /// The window extent in cells when the ratio was applied, once a client
    /// displays the window.
    pub applied_window_cells: Option<u16>,
    /// Fingerprint of the window's block membership when the ratio was
    /// applied.
    pub membership: u64,
    /// What the daemon makes of ratio changes it did not ask for.
    pub tracking: Tracking,
    /// Each block's size stamp when the daemon last changed the window's
    /// layout. A size still carrying its stamp predates that change.
    pub stale_sizes: BTreeMap<String, String>,
}

/// A placement as the state file stores it. A daemon from another workmux
/// version can keep running on the same server and reads every placement,
/// so this shape must stay readable by the daemons already released.
#[derive(Serialize, Deserialize)]
struct PlacementRecord {
    block_id: String,
    applied_ratio: f64,
    applied_window_cells: Option<u16>,
    topology: u64,
    #[serde(default)]
    settling: bool,
    #[serde(default)]
    drag_candidate: Option<f64>,
    #[serde(default)]
    stale_sizes: BTreeMap<String, String>,
}

impl From<PlacementRecord> for Placement {
    fn from(record: PlacementRecord) -> Self {
        let tracking = match (record.settling, record.drag_candidate) {
            (true, _) => Tracking::Settling,
            (false, Some(ratio)) => Tracking::Candidate(ratio),
            (false, None) => Tracking::Stable,
        };
        Self {
            block_id: record.block_id,
            applied_ratio: record.applied_ratio,
            applied_window_cells: record.applied_window_cells,
            membership: record.topology,
            tracking,
            stale_sizes: record.stale_sizes,
        }
    }
}

impl From<Placement> for PlacementRecord {
    fn from(placement: Placement) -> Self {
        Self {
            block_id: placement.block_id,
            applied_ratio: placement.applied_ratio,
            applied_window_cells: placement.applied_window_cells,
            topology: placement.membership,
            settling: placement.tracking == Tracking::Settling,
            drag_candidate: match placement.tracking {
                Tracking::Candidate(ratio) => Some(ratio),
                _ => None,
            },
            stale_sizes: placement.stale_sizes,
        }
    }
}

/// What the daemon makes of a root ratio it did not set.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) enum Tracking {
    /// The applied ratio holds: a new ratio may be a drag.
    #[default]
    Stable,
    /// The blocks just changed, so the next ratio change may be the app
    /// rebalancing the row rather than the user dragging the border.
    Settling,
    /// A ratio seen once that differs from the applied one. Seen twice, it
    /// is a drag.
    Candidate(f64),
}

/// Which sessions the sidebar covers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct FileScope {
    pub enabled: bool,
    /// Session IDs the sidebar covers while `enabled` is off. Rex only.
    pub sessions: BTreeSet<String>,
    /// Session IDs left out while `enabled` is on. Rex only.
    pub optout_sessions: BTreeSet<String>,
}

impl FileScope {
    /// Cover every session, or one more.
    pub(super) fn include(&mut self, session: Option<&str>) {
        match session {
            None => {
                self.enabled = true;
                self.sessions.clear();
                self.optout_sessions.clear();
            }
            Some(session) if self.enabled => {
                self.optout_sessions.remove(session);
            }
            Some(session) => {
                self.sessions.insert(session.to_string());
            }
        }
    }

    /// Cover no session, or one fewer.
    pub(super) fn exclude(&mut self, session: Option<&str>) {
        match session {
            None => {
                self.enabled = false;
                self.sessions.clear();
                self.optout_sessions.clear();
            }
            Some(session) if self.enabled => {
                self.optout_sessions.insert(session.to_string());
            }
            Some(session) => {
                self.sessions.remove(session);
            }
        }
    }

    /// Whether the sidebar covers this session.
    pub(super) fn covers(&self, session_id: &str) -> bool {
        if self.enabled {
            !self.optout_sessions.contains(session_id)
        } else {
            self.sessions.contains(session_id)
        }
    }

    /// Whether the sidebar covers any session.
    pub(super) fn is_active(&self) -> bool {
        self.enabled || !self.sessions.is_empty()
    }
}

impl SidebarSessionState {
    /// Whether the sidebar covers this session.
    pub(super) fn covers_session(&self, session_id: &str) -> bool {
        self.scope.covers(session_id)
    }

    /// Whether the sidebar covers any session.
    pub(super) fn is_active(&self) -> bool {
        self.scope.is_active()
    }
}

fn instance_key(instance_id: &str) -> u64 {
    let mut key = 0xcbf29ce484222325u64;
    for byte in instance_id.as_bytes() {
        key ^= u64::from(*byte);
        key = key.wrapping_mul(0x100000001b3);
    }
    key
}

pub(super) fn path(instance_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "workmux-sidebar-state-{:016x}.json",
        instance_key(instance_id)
    ))
}

pub(super) fn socket_path(instance_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "workmux-sidebar-{:016x}.sock",
        instance_key(instance_id)
    ))
}

fn lock_file(path: PathBuf) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .context("failed to open sidebar lock file")
}

fn layout_lock_file(instance_id: &str) -> Result<File> {
    lock_file(path(instance_id).with_extension("layout.lock"))
}

/// Serializes daemon startup, so concurrent callers never remove a socket
/// that a starting daemon has just bound.
pub(super) fn lock_spawn(instance_id: &str) -> Result<File> {
    let file = lock_file(path(instance_id).with_extension("spawn.lock"))?;
    file.lock()
        .context("failed to lock sidebar daemon startup")?;
    Ok(file)
}

/// Held by a running daemon for its whole lifetime. `None` means another
/// daemon for this instance holds it.
pub(super) fn try_lock_daemon(instance_id: &str) -> Result<Option<File>> {
    let file = lock_file(path(instance_id).with_extension("daemon.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error).context("failed to lock sidebar daemon"),
    }
}

pub(super) fn lock_layout(instance_id: &str) -> Result<File> {
    let file = layout_lock_file(instance_id)?;
    file.lock().context("failed to lock sidebar layout")?;
    Ok(file)
}

pub(super) fn try_lock_layout(instance_id: &str) -> Result<Option<File>> {
    let file = layout_lock_file(instance_id)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error).context("failed to lock sidebar layout"),
    }
}

/// Hold an exclusive lock on the state file's companion lock for writes.
fn lock_state(instance_id: &str) -> Result<File> {
    let file = lock_file(path(instance_id).with_extension("lock"))?;
    file.lock()
        .context("failed to lock sidebar session state")?;
    Ok(file)
}

/// Read the state file. A file this version cannot parse reads as the
/// default state, so a format change resets the sidebar instead of stopping
/// every command and daemon pass that reads it.
fn read_path(path: &Path) -> Result<SidebarSessionState> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %path.display(), "ignoring unreadable sidebar state");
            SidebarSessionState::default()
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(SidebarSessionState::default())
        }
        Err(error) => Err(error).context("failed to read sidebar session state"),
    }
}

pub(super) fn read(instance_id: &str) -> Result<SidebarSessionState> {
    read_path(&path(instance_id))
}

pub(super) fn update(
    instance_id: &str,
    update: impl FnOnce(&mut SidebarSessionState),
) -> Result<SidebarSessionState> {
    let _lock = lock_state(instance_id)?;
    let path = path(instance_id);
    let mut state = read_path(&path)?;
    update(&mut state);
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(&state)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .context("failed to create sidebar session state")?;
    file.write_all(&bytes)
        .context("failed to write sidebar session state")?;
    file.sync_all()
        .context("failed to sync sidebar session state")?;
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
        .context("failed to secure sidebar session state")?;
    fs::rename(&temp, &path).context("failed to publish sidebar session state")?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SidebarPosition;

    #[test]
    fn paths_are_stable_and_separate_by_instance() {
        assert_eq!(path("alpha"), path("alpha"));
        assert_ne!(path("alpha"), path("beta"));
        assert_ne!(path("alpha"), socket_path("alpha"));
    }

    /// A process another test is spawning shares every open file until it
    /// execs, so a released lock can stay held for a moment.
    fn eventually<T>(attempt: impl Fn() -> Option<T>) -> Option<T> {
        for _ in 0..100 {
            if let Some(value) = attempt() {
                return Some(value);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    #[test]
    fn layout_updates_exclude_concurrent_synchronization_and_release_on_drop() {
        let instance = format!("sidebar-layout-test-{:016x}", getrandom::u64().unwrap());
        let guard = lock_layout(&instance).unwrap();
        assert!(try_lock_layout(&instance).unwrap().is_none());
        drop(guard);
        assert!(eventually(|| try_lock_layout(&instance).unwrap()).is_some());
        fs::remove_file(path(&instance).with_extension("layout.lock")).unwrap();
    }

    #[test]
    fn daemon_lock_admits_one_holder() {
        let instance = format!("sidebar-daemon-test-{:016x}", getrandom::u64().unwrap());
        let first = try_lock_daemon(&instance).unwrap();
        assert!(first.is_some());
        assert!(try_lock_daemon(&instance).unwrap().is_none());
        drop(first);
        assert!(eventually(|| try_lock_daemon(&instance).unwrap()).is_some());
        fs::remove_file(path(&instance).with_extension("daemon.lock")).unwrap();
    }

    #[test]
    fn scope_includes_and_excludes_sessions() {
        let mut scope = FileScope::default();
        scope.include(Some("s1"));
        scope.include(Some("s2"));
        assert!(scope.covers("s1") && scope.covers("s2") && !scope.covers("s3"));
        scope.exclude(Some("s1"));
        assert!(!scope.covers("s1") && scope.is_active());
        scope.exclude(Some("s2"));
        assert!(!scope.is_active());
    }

    #[test]
    fn global_scope_opts_sessions_out_and_back_in() {
        let mut scope = FileScope::default();
        scope.include(Some("s1"));
        scope.include(None);
        assert!(scope.enabled && scope.sessions.is_empty());
        assert!(scope.covers("s1") && scope.covers("s2"));
        scope.exclude(Some("s2"));
        assert!(!scope.covers("s2") && scope.covers("s3"));
        scope.include(Some("s2"));
        assert!(scope.covers("s2"));
        scope.exclude(Some("s2"));
        scope.exclude(None);
        assert_eq!(scope, FileScope::default());
    }

    #[test]
    fn scope_keeps_its_file_shape() {
        let state = SidebarSessionState {
            scope: FileScope {
                enabled: true,
                sessions: BTreeSet::new(),
                optout_sessions: BTreeSet::from(["s1".to_string()]),
            },
            ..Default::default()
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["enabled"], true);
        assert_eq!(json["optout_sessions"], serde_json::json!(["s1"]));
        let state: SidebarSessionState = serde_json::from_value(json).unwrap();
        assert!(!state.covers_session("s1"));
    }

    #[test]
    fn placements_keep_their_file_shape() {
        for (tracking, settling, drag_candidate) in [
            (Tracking::Stable, false, serde_json::Value::Null),
            (Tracking::Settling, true, serde_json::Value::Null),
            (Tracking::Candidate(0.25), false, serde_json::json!(0.25)),
        ] {
            let placement = Placement {
                block_id: "b".to_string(),
                applied_ratio: 0.2,
                applied_window_cells: Some(200),
                membership: 7,
                tracking,
                stale_sizes: BTreeMap::from([("b".to_string(), "t1".to_string())]),
            };
            let json = serde_json::to_value(&placement).unwrap();
            assert_eq!(
                json,
                serde_json::json!({
                    "block_id": "b",
                    "applied_ratio": 0.2,
                    "applied_window_cells": 200,
                    "topology": 7,
                    "settling": settling,
                    "drag_candidate": drag_candidate,
                    "stale_sizes": {"b": "t1"},
                })
            );
            assert_eq!(
                serde_json::from_value::<Placement>(json).unwrap(),
                placement
            );
        }
    }

    #[test]
    fn placements_without_optional_fields_read_as_stable() {
        let placement: Placement = serde_json::from_value(serde_json::json!({
            "block_id": "b",
            "applied_ratio": 0.2,
            "applied_window_cells": null,
            "topology": 7,
        }))
        .unwrap();
        assert_eq!(placement.tracking, Tracking::Stable);
        assert!(placement.stale_sizes.is_empty());
    }

    #[test]
    fn unparseable_state_reads_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, br#"{"enabled": "yes", "position": 3}"#).unwrap();
        let state = read_path(&path).unwrap();
        assert!(!state.scope.enabled);
        assert_eq!(state.prefs.position, None);
    }

    #[test]
    fn updates_preserve_session_state() {
        let instance = format!(
            "sidebar-state-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        update(&instance, |state| {
            state.scope.enabled = true;
            state.prefs.position = Some(SidebarPosition::Top);
            state.ordered_agents = vec!["terminal_7".to_string()];
        })
        .unwrap();

        let state = read(&instance).unwrap();
        assert!(state.scope.enabled);
        assert_eq!(state.prefs.position, Some(SidebarPosition::Top));
        assert_eq!(state.ordered_agents, vec!["terminal_7"]);

        let _ = fs::remove_file(path(&instance));
        let _ = fs::remove_file(path(&instance).with_extension("lock"));
    }
}
