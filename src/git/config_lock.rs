use crate::vcs::meta_lock::{FileLock, FileLockGuard};
use anyhow::Result;
use std::path::Path;

/// RAII guard that holds an exclusive advisory lock on a `.workmux.lock` file
/// in the git common directory. Serializes concurrent workmux processes that
/// write to `.git/config`.
///
/// This is a thin wrapper over the generic `crate::vcs::meta_lock::FileLock`
/// primitive, kept for backward compatibility with existing call sites
/// (`src/workflow/create.rs`, etc.) that expect a git-specific type and a
/// `git_common_dir`-relative acquisition API.
pub struct GitConfigLock {
    _lock: FileLockGuard,
}

impl GitConfigLock {
    /// Acquire an exclusive lock, blocking until available.
    pub fn acquire(git_common_dir: &Path) -> Result<Self> {
        let lock_path = git_common_dir.join(".workmux.lock");
        let lock = FileLock::acquire(&lock_path)?;
        Ok(Self { _lock: lock })
    }
}
