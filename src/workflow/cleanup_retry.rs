//! Retry transient filesystem cleanup failures without losing orphan identities.

use anyhow::{Context, Result};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::cleanup::DirectoryIdentity;

const RETRY_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_BACKOFF: Duration = Duration::from_millis(50);
const MAX_BACKOFF: Duration = Duration::from_millis(500);

pub(super) struct PendingCleanup {
    path: PathBuf,
    record_path: PathBuf,
    identity: DirectoryIdentity,
}

impl PendingCleanup {
    /// Persist evidence before rename, so Git cleanup cannot make an orphan untracked.
    pub fn prepare(
        original_path: &Path,
        path: PathBuf,
        identity: DirectoryIdentity,
    ) -> Result<Self> {
        Self::prepare_in(
            &crate::xdg::state_dir()?.join("pending-cleanup"),
            original_path,
            path,
            identity,
        )
    }

    fn prepare_in(
        state_dir: &Path,
        original_path: &Path,
        path: PathBuf,
        identity: DirectoryIdentity,
    ) -> Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let name = path
            .file_name()
            .context("Quarantine path has no filename")?;
        let mut record_name = name.to_os_string();
        record_name.push(".json");
        let record_path = state_dir.join(record_name);
        let record = serde_json::json!({
            "version": 1,
            "original_path": original_path.to_string_lossy(),
            "original_path_bytes": original_path.as_os_str().as_encoded_bytes(),
            "quarantine_path": path.to_string_lossy(),
            "quarantine_path_bytes": path.as_os_str().as_encoded_bytes(),
            "device": identity.device,
            "inode": identity.inode,
            "scope": "filesystem only; Git cleanup may be incomplete; do not replay branch deletion",
        });
        crate::util::write_atomic_durable(&record_path, &serde_json::to_vec_pretty(&record)?)
            .context("Failed to persist pending cleanup before quarantine")?;
        tracing::info!(record = %record_path.display(), path = %path.display(), "cleanup:pending filesystem cleanup recorded");
        Ok(Self {
            path,
            record_path,
            identity,
        })
    }

    pub fn remove(self) -> Result<()> {
        let started = Instant::now();
        self.remove_with_clock(|| started.elapsed(), std::thread::sleep)
    }

    fn remove_with_clock(
        self,
        elapsed: impl Fn() -> Duration,
        sleep: impl FnMut(Duration),
    ) -> Result<()> {
        retry_with_clock(
            RETRY_TIMEOUT,
            || super::cleanup_tree::remove(&self.path, self.identity),
            elapsed,
            sleep,
        ).map_err(|error| {
            let snapshot = super::cleanup_diagnostics::remaining_entries(&self.path);
            tracing::warn!(
                path = %self.path.display(),
                record = %self.record_path.display(),
                error = %error,
                "cleanup:quarantine deletion failed; pending record retained"
            );
            let context = format!(
                "Failed to remove quarantined worktree {} (kind={:?}, errno={:?}); pending cleanup record retained at {}; {}",
                self.path.display(), error.kind(), error.raw_os_error(),
                self.record_path.display(), snapshot,
            );
            anyhow::Error::new(error).context(context)
        })?;
        std::fs::remove_file(&self.record_path)
            .context("Worktree deleted, but failed to clear pending cleanup record")?;
        Ok(())
    }
}

fn retry_with_clock(
    timeout: Duration,
    mut remove: impl FnMut() -> io::Result<()>,
    elapsed: impl Fn() -> Duration,
    mut sleep: impl FnMut(Duration),
) -> io::Result<()> {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let error = match remove() {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        if error.kind() != io::ErrorKind::DirectoryNotEmpty || elapsed() >= timeout {
            return Err(error);
        }
        let delay = backoff.min(timeout.saturating_sub(elapsed()));
        tracing::debug!(?delay, "cleanup:retrying nonempty quarantine directory");
        sleep(delay);
        if elapsed() >= timeout {
            return Err(error);
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn identity(path: &Path) -> DirectoryIdentity {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        DirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    #[derive(Default)]
    struct Clock {
        elapsed: std::cell::Cell<Duration>,
        sleeps: std::cell::RefCell<Vec<Duration>>,
    }

    impl Clock {
        fn advance(&self, delay: Duration) {
            self.sleeps.borrow_mut().push(delay);
            self.elapsed.set(self.elapsed.get() + delay);
        }

        fn retry(
            &self,
            timeout: Duration,
            remove: impl FnMut() -> io::Result<()>,
        ) -> io::Result<()> {
            retry_with_clock(
                timeout,
                remove,
                || self.elapsed.get(),
                |delay| {
                    self.advance(delay);
                },
            )
        }
    }

    #[test]
    fn retries_transient_nonempty_errors() {
        let clock = Clock::default();
        let mut attempts = 0;
        clock
            .retry(Duration::from_secs(1), || {
                attempts += 1;
                if attempts < 3 {
                    Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty))
                } else {
                    Ok(())
                }
            })
            .unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(
            *clock.sleeps.borrow(),
            vec![Duration::from_millis(50), Duration::from_millis(100)]
        );
    }

    #[test]
    fn stops_retrying_at_deadline_and_does_not_retry_other_errors() {
        let clock = Clock::default();
        let mut attempts = 0;
        let error = clock
            .retry(Duration::from_millis(120), || {
                attempts += 1;
                Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::DirectoryNotEmpty);
        assert_eq!(attempts, 2);
        assert_eq!(
            *clock.sleeps.borrow(),
            vec![Duration::from_millis(50), Duration::from_millis(70)]
        );
        assert_eq!(clock.elapsed.get(), Duration::from_millis(120));
        let clock = Clock::default();
        let mut attempts = 0;
        clock
            .retry(Duration::from_secs(5), || {
                attempts += 1;
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            })
            .unwrap_err();
        assert_eq!(attempts, 1);
        assert!(clock.sleeps.borrow().is_empty());
    }

    fn exercise_late_writer(persistent: bool) {
        use super::super::cleanup_tree;
        use std::cell::Cell;
        use std::ffi::OsStr;
        use std::rc::Rc;

        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("worktree");
        let trash = root.path().join(".workmux_trash_test");
        std::fs::create_dir_all(original.join("churn")).unwrap();
        std::fs::write(original.join("churn/seed"), "seed").unwrap();
        std::fs::create_dir(original.join("crate-0")).unwrap();
        std::fs::write(original.join("crate-0/artifact"), "remove").unwrap();
        let pending = PendingCleanup::prepare_in(
            &root.path().join("state"),
            &original,
            trash.clone(),
            identity(&original),
        )
        .unwrap();
        let record_path = pending.record_path.clone();
        let record_bytes = std::fs::read(&record_path).unwrap();
        std::fs::rename(&original, &trash).unwrap();
        let churn = trash.join("churn");
        let calls = Rc::new(Cell::new(0));
        let observed = calls.clone();
        let _guard = cleanup_tree::before_rmdir::install(move |name| {
            if name == OsStr::new("churn") {
                observed.set(observed.get() + 1);
                if persistent || observed.get() == 1 {
                    std::fs::write(churn.join("late"), "late write").unwrap();
                }
            }
        });
        let clock = Clock::default();
        let result =
            pending.remove_with_clock(|| clock.elapsed.get(), |delay| clock.advance(delay));
        if persistent {
            assert_eq!(calls.get(), 13);
            assert_eq!(clock.elapsed.get(), RETRY_TIMEOUT);
            let error = format!("{:#}", result.unwrap_err());
            assert!(error.contains("kind=DirectoryNotEmpty"));
            assert!(error.contains("Directory not empty"));
            assert!(error.contains("Recursive deletion encountered"));
            assert!(error.contains("pending cleanup record retained at"));
            assert!(error.contains("post-failure snapshot"));
            assert!(error.contains(&record_path.display().to_string()));
            assert_eq!(std::fs::read(&record_path).unwrap(), record_bytes);
            let record: serde_json::Value = serde_json::from_slice(&record_bytes).unwrap();
            let metadata = std::fs::metadata(&trash).unwrap();
            assert_eq!(record["inode"], metadata.ino());
            assert_eq!(record["device"], metadata.dev());
            assert!(trash.join("churn/late").is_file());
            assert!(
                !trash.join("crate-0").exists(),
                "A busy directory must not block sibling cleanup"
            );
        } else {
            result.unwrap();
            assert_eq!(calls.get(), 2);
            assert!(!trash.exists());
            assert!(!record_path.exists());
        }
    }

    #[test]
    fn late_writer_quiescence_clears_quarantine_and_record() {
        exercise_late_writer(false);
    }

    #[test]
    fn persistent_late_writes_exhaust_retries_and_retain_identity() {
        exercise_late_writer(true);
    }

    #[test]
    fn persists_before_rename_and_clears_only_after_success() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("worktree");
        let trash = root.path().join(".workmux_trash_test");
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("file"), "contents").unwrap();
        let pending = PendingCleanup::prepare_in(
            &root.path().join("state"),
            &original,
            trash.clone(),
            identity(&original),
        )
        .unwrap();
        let record_path = pending.record_path.clone();
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
        assert_eq!(record["quarantine_path"], trash.to_str().unwrap());
        assert!(original.exists());
        assert!(!trash.exists());
        std::fs::rename(original, &trash).unwrap();
        pending.remove().unwrap();
        assert!(!record_path.exists());
        assert!(!trash.exists());
    }

    #[test]
    fn preserves_non_utf8_paths_and_refuses_unwritable_record_location() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("worktree");
        std::fs::create_dir(&original).unwrap();
        let trash = root.path().join(".workmux_trash_test");
        let state = root.path().join("state");
        std::fs::write(&state, "not a directory").unwrap();
        assert!(
            PendingCleanup::prepare_in(&state, &original, trash.clone(), identity(&original))
                .is_err()
        );
        assert!(original.is_dir());
        assert!(!trash.exists());
        std::fs::remove_file(&state).unwrap();
        // Record serialization must preserve OS paths even on filesystems that
        // cannot themselves create non-UTF-8 names.
        let non_utf8 = root
            .path()
            .join(std::ffi::OsString::from_vec(b"worktree-\xff".to_vec()));
        let pending =
            PendingCleanup::prepare_in(&state, &non_utf8, trash, identity(&original)).unwrap();
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&pending.record_path).unwrap()).unwrap();
        let bytes: Vec<u8> = serde_json::from_value(record["original_path_bytes"].clone()).unwrap();
        assert_eq!(bytes, non_utf8.as_os_str().as_encoded_bytes());
    }

    #[test]
    fn preserves_record_and_rejects_replaced_directory() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        let trash = root.path().join(".workmux_trash_test");
        std::fs::create_dir(&original).unwrap();
        let pending = PendingCleanup::prepare_in(
            &root.path().join("state"),
            &original,
            trash.clone(),
            identity(&original),
        )
        .unwrap();
        let record_path = pending.record_path.clone();
        std::fs::create_dir(&trash).unwrap();
        std::fs::write(trash.join("sentinel"), "keep").unwrap();
        assert!(format!("{:#}", pending.remove().unwrap_err()).contains("identity changed"));
        assert!(record_path.exists());
        assert!(trash.join("sentinel").exists());
    }
}
