use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::{SidebarHeight, SidebarPosition, SidebarWidth};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct SidebarSessionState {
    pub enabled: bool,
    pub position: Option<SidebarPosition>,
    pub width: Option<SidebarWidth>,
    pub height: Option<SidebarHeight>,
    pub layout: Option<String>,
    pub filter: Option<String>,
    pub sleeping_panes: HashSet<String>,
    pub group_by: Option<String>,
    pub expanded_groups: Vec<String>,
    pub ordered_agents: Vec<String>,
    pub daemon_pid: Option<u32>,
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

fn lock_path(instance_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "workmux-sidebar-state-{:016x}.lock",
        instance_key(instance_id)
    ))
}

fn layout_lock_file(instance_id: &str) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path(instance_id).with_extension("layout.lock"))
        .context("failed to open Zellij sidebar layout lock")
}

pub(super) fn lock_layout(instance_id: &str) -> Result<File> {
    let file = layout_lock_file(instance_id)?;
    file.lock()
        .context("failed to lock Zellij sidebar layout")?;
    Ok(file)
}

pub(super) fn try_lock_layout(instance_id: &str) -> Result<Option<File>> {
    let file = layout_lock_file(instance_id)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => {
            Err(error).context("failed to lock Zellij sidebar layout")
        }
    }
}

struct StateLock {
    path: PathBuf,
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_lock(instance_id: &str) -> Result<StateLock> {
    let path = lock_path(instance_id);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(StateLock { path }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(5));
                if stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err(anyhow!("timed out locking sidebar session state"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("failed to lock sidebar session state"),
        }
    }
}

fn read_path(path: &Path) -> Result<SidebarSessionState> {
    match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).context("failed to parse sidebar session state")
        }
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
    let _lock = acquire_lock(instance_id)?;
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

    #[test]
    fn paths_are_stable_and_separate_by_instance() {
        assert_eq!(path("alpha"), path("alpha"));
        assert_ne!(path("alpha"), path("beta"));
        assert_ne!(path("alpha"), socket_path("alpha"));
    }

    #[test]
    fn layout_updates_exclude_concurrent_synchronization_and_release_on_drop() {
        let instance = format!("sidebar-layout-test-{:016x}", getrandom::u64().unwrap());
        let guard = lock_layout(&instance).unwrap();
        assert!(try_lock_layout(&instance).unwrap().is_none());
        drop(guard);
        assert!(try_lock_layout(&instance).unwrap().is_some());
        fs::remove_file(path(&instance).with_extension("layout.lock")).unwrap();
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
            state.enabled = true;
            state.position = Some(SidebarPosition::Top);
            state.ordered_agents = vec!["terminal_7".to_string()];
        })
        .unwrap();

        let state = read(&instance).unwrap();
        assert!(state.enabled);
        assert_eq!(state.position, Some(SidebarPosition::Top));
        assert_eq!(state.ordered_agents, vec!["terminal_7"]);

        let _ = fs::remove_file(path(&instance));
        let _ = fs::remove_file(lock_path(&instance));
    }
}
