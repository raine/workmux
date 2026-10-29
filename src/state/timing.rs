//! Timing events for state persistence. Labels never contain agent-provided data.

use std::time::{Duration, Instant};

const SLOW_OPERATION: Duration = Duration::from_millis(250);

pub(super) struct Timer<'a> {
    start: Instant,
    phase: &'static str,
    operation: &'static str,
    backend: Option<&'a str>,
}

impl<'a> Timer<'a> {
    pub(super) fn start(
        phase: &'static str,
        operation: &'static str,
        backend: Option<&'a str>,
    ) -> Self {
        Self {
            start: Instant::now(),
            phase,
            operation,
            backend,
        }
    }
}

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        record(
            self.phase,
            self.operation,
            self.backend,
            self.start.elapsed(),
        );
    }
}

pub(super) fn record(
    phase: &'static str,
    operation: &'static str,
    backend: Option<&str>,
    elapsed: Duration,
) {
    let duration_ms = elapsed.as_secs_f64() * 1000.0;
    let process_id = std::process::id();
    if elapsed >= SLOW_OPERATION {
        tracing::warn!(
            phase,
            operation,
            backend,
            duration_ms,
            process_id,
            "agent state timing"
        );
    } else {
        tracing::debug!(
            phase,
            operation,
            backend,
            duration_ms,
            process_id,
            "agent state timing"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs(action: impl FnOnce()) -> String {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, action);
        String::from_utf8(capture.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn timing_threshold_and_fields() {
        let logs = capture_logs(|| {
            record(
                "backend_query",
                "server_boot_id",
                Some("tmux"),
                Duration::from_micros(125),
            );
            record("lock_wait", "update", None, Duration::from_millis(249));
            record("lock_hold", "update", None, Duration::from_millis(250));
        });
        let lines: Vec<_> = logs.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("DEBUG"));
        assert!(lines[0].contains("phase=\"backend_query\""));
        assert!(lines[0].contains("operation=\"server_boot_id\""));
        assert!(lines[0].contains("backend=\"tmux\""));
        assert!(lines[0].contains("duration_ms=0.125"));
        assert!(lines[0].contains(&format!("process_id={}", std::process::id())));
        assert!(lines[1].contains("DEBUG"));
        assert!(lines[2].contains("WARN"));
    }

    #[test]
    fn timer_records_early_return() {
        fn run() -> Option<()> {
            let _timer = Timer::start("persistence", "registration", Some("tmux"));
            None?;
            Some(())
        }
        let logs = capture_logs(|| assert_eq!(run(), None));
        assert!(logs.contains("phase=\"persistence\""));
        assert!(logs.contains("operation=\"registration\""));
    }

    #[test]
    fn lock_timings_preserve_results_and_release_on_error() {
        let (store, _dir) = crate::state::test_support::temp_store();
        let logs = capture_logs(|| {
            let result: anyhow::Result<()> =
                store.with_agent_lock("test_failure", |_| anyhow::bail!("expected failure"));
            assert_eq!(result.unwrap_err().to_string(), "expected failure");
            assert_eq!(
                store.with_agent_lock("test_success", |_| Ok(42)).unwrap(),
                42
            );
        });
        assert_eq!(logs.matches("phase=\"lock_wait\"").count(), 2);
        assert_eq!(logs.matches("phase=\"lock_hold\"").count(), 2);
        assert_eq!(logs.matches("operation=\"test_failure\"").count(), 2);
    }
}
