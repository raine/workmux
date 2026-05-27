//! Test-only helpers shared across modules.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static PROCESS_STATE_LOCK: Mutex<()> = Mutex::new(());

pub struct ProcessStateGuard {
    _lock: MutexGuard<'static, ()>,
    original_cwd: PathBuf,
    saved_env: HashMap<OsString, Option<OsString>>,
}

pub fn process_state() -> std::io::Result<ProcessStateGuard> {
    let lock = PROCESS_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let original_cwd = std::env::current_dir()?;

    Ok(ProcessStateGuard {
        _lock: lock,
        original_cwd,
        saved_env: HashMap::new(),
    })
}

impl ProcessStateGuard {
    pub fn set_current_dir(&mut self, path: impl AsRef<Path>) -> std::io::Result<()> {
        std::env::set_current_dir(path)
    }

    pub fn set_env(&mut self, key: &'static str, value: impl AsRef<OsStr>) {
        self.save_env(key);
        // SAFETY: tests that mutate process env must do it through this guard,
        // which serializes mutation behind PROCESS_STATE_LOCK.
        unsafe {
            std::env::set_var(key, value);
        }
    }

    pub fn remove_env(&mut self, key: &'static str) {
        self.save_env(key);
        // SAFETY: tests that mutate process env must do it through this guard,
        // which serializes mutation behind PROCESS_STATE_LOCK.
        unsafe {
            std::env::remove_var(key);
        }
    }

    fn save_env(&mut self, key: &'static str) {
        let key = OsString::from(key);
        self.saved_env
            .entry(key.clone())
            .or_insert_with(|| std::env::var_os(&key));
    }
}

impl Drop for ProcessStateGuard {
    fn drop(&mut self) {
        if let Err(err) = std::env::set_current_dir(&self.original_cwd) {
            eprintln!("failed to restore cwd after test: {err}");
        }

        for (key, value) in &self.saved_env {
            // SAFETY: tests that mutate process env must do it through this guard,
            // which serializes mutation behind PROCESS_STATE_LOCK.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
