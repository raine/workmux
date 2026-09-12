//! Generic, path-parameterized file lock.
//!
//! This is the OS-level locking primitive originally embedded in
//! `crate::git::config_lock::GitConfigLock`, extracted so both the git and
//! jj backends (and their metadata stores) can serialize concurrent workmux
//! processes against an arbitrary lock file, not just `.git/.workmux.lock`.

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};
use std::fs::{File, OpenOptions};
use std::path::Path;
use tracing::debug;

/// RAII guard that holds an exclusive advisory lock (via `flock(2)`) on the
/// file it was acquired for. Dropping the guard releases the lock.
pub struct FileLockGuard {
    _lock: Flock<File>,
}

/// Path-parameterized file lock. Acquiring the lock blocks until any other
/// holder (in this or another process) releases it.
pub struct FileLock;

impl FileLock {
    /// Acquire an exclusive lock on `lock_path`, blocking until available.
    /// The lock file is created (but not truncated) if it does not exist.
    pub fn acquire(lock_path: &Path) -> Result<FileLockGuard> {
        debug!(path = %lock_path.display(), "meta_lock:acquiring");

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .with_context(|| format!("Failed to open lock file: {}", lock_path.display()))?;

        let lock = Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_file, errno)| errno)
            .with_context(|| format!("Failed to acquire lock: {}", lock_path.display()))?;

        debug!(path = %lock_path.display(), "meta_lock:acquired");
        Ok(FileLockGuard { _lock: lock })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn second_acquire_blocks_until_first_guard_drops() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("meta.lock");

        let first = FileLock::acquire(&lock_path).unwrap();

        let (acquired_tx, acquired_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let thread_lock_path = lock_path.clone();
        let handle = std::thread::spawn(move || {
            let _second = FileLock::acquire(&thread_lock_path).unwrap();
            acquired_tx.send(()).unwrap();
            // Hold the lock until the main thread says we can drop it, so
            // the assertion below observes it while still held.
            let _ = release_rx.recv();
        });

        // The second acquire should not complete while `first` is held.
        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "second acquire completed before first guard was dropped"
        );

        drop(first);

        // Now that the first guard is dropped, the second acquire should
        // complete promptly.
        acquired_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("second acquire did not complete after first guard dropped");

        release_tx.send(()).unwrap();
        handle.join().unwrap();
    }
}
