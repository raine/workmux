//! Error handling for the sandbox TCP accept loops.

use std::io;
use std::thread;
use std::time::{Duration, Instant};
use tracing::warn;

/// Delay before calling `accept()` again after a transient error.
const RETRY_DELAY: Duration = Duration::from_millis(100);

/// Minimum interval between repeated accept error warnings.
const WARN_INTERVAL: Duration = Duration::from_secs(10);

/// Tracks `accept()` failures for one listener.
pub(crate) struct AcceptErrors {
    listener: &'static str,
    last_warned: Option<Instant>,
    suppressed: u64,
}

impl AcceptErrors {
    pub(crate) fn new(listener: &'static str) -> Self {
        Self {
            listener,
            last_warned: None,
            suppressed: 0,
        }
    }

    /// Log an accept error and wait before the next attempt.
    ///
    /// Returns `false` when the listener is unusable and the accept loop
    /// should stop. Transient errors such as `EMFILE` return `true`.
    pub(crate) fn backoff(&mut self, error: &io::Error) -> bool {
        if is_fatal(error) {
            warn!(listener = self.listener, %error, "accept failed, shutting down");
            return false;
        }

        let now = Instant::now();
        if self
            .last_warned
            .is_none_or(|last| now.duration_since(last) >= WARN_INTERVAL)
        {
            warn!(
                listener = self.listener,
                %error,
                suppressed = self.suppressed,
                "accept failed, retrying"
            );
            self.last_warned = Some(now);
            self.suppressed = 0;
        } else {
            self.suppressed += 1;
        }

        thread::sleep(RETRY_DELAY);
        true
    }
}

/// Whether an `accept()` error means the listening socket itself is broken.
fn is_fatal(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EBADF | libc::EFAULT | libc::EINVAL | libc::ENOTSOCK)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_exhaustion_is_not_fatal() {
        for errno in [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOBUFS,
            libc::ENOMEM,
            libc::ECONNABORTED,
            libc::EINTR,
            libc::EAGAIN,
            libc::EPROTO,
            libc::EOPNOTSUPP,
            libc::EPERM,
        ] {
            assert!(!is_fatal(&io::Error::from_raw_os_error(errno)), "{errno}");
        }
    }

    #[test]
    fn broken_listener_is_fatal() {
        for errno in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            assert!(is_fatal(&io::Error::from_raw_os_error(errno)), "{errno}");
        }
    }

    #[test]
    fn backoff_retries_transient_errors_and_suppresses_repeat_warnings() {
        let mut errors = AcceptErrors::new("test");
        let emfile = io::Error::from_raw_os_error(libc::EMFILE);

        assert!(errors.backoff(&emfile));
        assert!(errors.last_warned.is_some());
        assert_eq!(errors.suppressed, 0);

        assert!(errors.backoff(&emfile));
        assert_eq!(errors.suppressed, 1);

        errors.last_warned = Some(Instant::now() - WARN_INTERVAL);
        assert!(errors.backoff(&emfile));
        assert_eq!(errors.suppressed, 0);
    }

    #[test]
    fn backoff_stops_on_fatal_error() {
        let mut errors = AcceptErrors::new("test");
        assert!(!errors.backoff(&io::Error::from_raw_os_error(libc::EBADF)));
    }
}
