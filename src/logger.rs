use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_appender::rolling;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use crate::sandbox::guest::is_sandbox_guest;

// Bound the eagerly allocated queue while retaining room for log bursts.
// A full queue drops new logs rather than blocking the caller.
const LOG_BUFFERED_LINES_LIMIT: usize = 4096;

static INIT: OnceLock<()> = OnceLock::new();
static GUARD: OnceLock<WorkerGuard> = OnceLock::new();

pub fn init() -> Result<()> {
    if INIT.get().is_some() {
        return Ok(());
    }

    // Skip file logging in sandbox guests - they're thin RPC clients and the
    // host supervisor handles all real logging. Also avoids needing to create
    // ~/.local/state/ in containers.
    if is_sandbox_guest() {
        let _ = INIT.set(());
        return Ok(());
    }

    init_inner()?;
    let _ = INIT.set(());
    Ok(())
}

fn init_inner() -> Result<()> {
    let log_path = determine_log_path()?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create log directory at {}", parent.display()))?;
    }

    let (directory, file_name) = split_path(&log_path)?;
    let file_appender = rolling::never(directory, file_name);
    let (non_blocking, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(LOG_BUFFERED_LINES_LIMIT)
        .lossy(true)
        .finish(file_appender);
    let _ = GUARD.set(guard);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(false),
        )
        .try_init()
        .context("Failed to initialize tracing subscriber")?;

    Ok(())
}

pub fn record_deferred_cleanup_failure(
    handle: &str,
    worktree_path: &Path,
    error: &anyhow::Error,
) -> Result<()> {
    let log_path = determine_log_path()?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create log directory at {}", parent.display()))?;
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let cause = format!("{error:#}").replace(['\r', '\n'], " ");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open log file at {}", log_path.display()))?;
    write_deferred_cleanup_record(&mut file, timestamp, handle, worktree_path, &cause)?;
    file.sync_data()?;
    Ok(())
}

fn write_deferred_cleanup_record(
    writer: &mut impl Write,
    timestamp: u64,
    handle: &str,
    worktree_path: &Path,
    cause: &str,
) -> std::io::Result<()> {
    // Format before appending so concurrent log writers cannot split the fields
    // across separate writes performed by write_fmt.
    let record = format!(
        "{timestamp} ERROR deferred cleanup worker failed handle={handle:?} worktree={worktree_path:?} cause={cause}\n"
    );
    writer.write_all(record.as_bytes())
}

fn determine_log_path() -> Result<PathBuf> {
    if let Ok(state_dir) = crate::xdg::state_dir() {
        return Ok(state_dir.join("workmux.log"));
    }

    // Fallback to current directory if home cannot be determined
    Ok(std::env::current_dir()?.join("workmux.log"))
}

fn split_path(path: &Path) -> Result<(PathBuf, &str)> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("Invalid log file name"))?;

    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    Ok((dir, file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingWriter {
        writes: Vec<Vec<u8>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes.push(bytes.to_vec());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn deferred_cleanup_record_is_appended_in_one_write() {
        let mut writer = RecordingWriter::default();
        write_deferred_cleanup_record(
            &mut writer,
            123,
            "deferred-a",
            Path::new("/tmp/deferred-a"),
            "Worktree identity changed before quarantine",
        )
        .unwrap();
        assert_eq!(writer.writes.len(), 1);
        assert_eq!(
            String::from_utf8(writer.writes.pop().unwrap()).unwrap(),
            "123 ERROR deferred cleanup worker failed handle=\"deferred-a\" worktree=\"/tmp/deferred-a\" cause=Worktree identity changed before quarantine\n"
        );
    }
}
