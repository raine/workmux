//! Pane handshake mechanisms for shell startup synchronization.
//!
//! Different backends use different mechanisms to ensure a shell is ready
//! before sending commands to a pane.

use anyhow::{Context, Result, anyhow};
#[cfg(unix)]
use nix::sys::stat::Mode;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, trace, warn};

use crate::cmd::Cmd;

/// Trait for pane handshake mechanisms.
///
/// A handshake ensures the shell has started in a pane before sending commands.
/// Different backends may use different mechanisms (tmux wait-for, named pipes, etc.)
pub trait PaneHandshake: Send {
    /// Returns a full shell command string that wraps the shell to signal readiness.
    /// Formatted for direct shell evaluation (e.g., `sh -c "..."`).
    /// Used by backends that need a single command string (tmux).
    fn wrapper_command(&self, shell: &str) -> String;

    /// Returns the raw POSIX script body that signals readiness and exec's the shell.
    /// Does NOT include `sh -c` wrapping -- the backend decides how to invoke it.
    /// Used by the shared `setup_panes` implementation, where each backend wraps
    /// the script appropriately for its CLI.
    fn script_content(&self, shell: &str) -> String {
        // Default: delegate to wrapper_command (backwards compat)
        self.wrapper_command(shell)
    }

    /// Waits for the handshake signal, consuming the handshake object.
    fn wait(self: Box<Self>) -> Result<()>;

    /// Waits for the handshake and any backend-specific shell readiness check.
    ///
    /// Backends without a post-exec readiness mechanism retain the original
    /// handshake behavior.
    fn wait_for_shell(self: Box<Self>, _pane_id: &str) -> Result<()> {
        self.wait()
    }
}

/// Timeout for waiting for pane readiness (seconds)
const HANDSHAKE_TIMEOUT_SECS: u64 = 5;
const PANE_READY_POLL_INTERVAL_MS: u64 = 50;

#[derive(Default)]
struct PaneContentStability {
    previous: Option<String>,
}

impl PaneContentStability {
    fn reset(&mut self) {
        self.previous = None;
    }

    fn observe(&mut self, content: String) -> bool {
        if content.is_empty() {
            self.reset();
            return false;
        }

        if self.previous.as_ref() == Some(&content) {
            return true;
        }

        self.previous = Some(content);
        false
    }
}

#[derive(Debug, PartialEq)]
enum PaneReadiness {
    Ready(Duration),
    TimedOut { last_error: Option<String> },
}

fn poll_until_stable_pane(
    timeout: Duration,
    poll_interval: Duration,
    mut capture: impl FnMut() -> Result<String>,
) -> PaneReadiness {
    let start = Instant::now();
    let mut stability = PaneContentStability::default();

    loop {
        let last_error = match capture() {
            Ok(content) => {
                if stability.observe(content) {
                    return PaneReadiness::Ready(start.elapsed());
                }
                None
            }
            Err(error) => {
                stability.reset();
                Some(error.to_string())
            }
        };

        if start.elapsed() >= timeout {
            return PaneReadiness::TimedOut { last_error };
        }

        thread::sleep(poll_interval);
    }
}

/// Manages the tmux wait-for handshake protocol for pane synchronization.
///
/// This struct encapsulates the channel-based handshake mechanism that ensures
/// the shell is ready before sending commands. The handshake uses tmux's `wait-for`
/// feature with channel locking to synchronize between the process spawning the
/// pane and the shell that starts inside it.
///
/// # Protocol
/// 1. Lock a unique channel (on construction)
/// 2. Start the shell with a wrapper that unlocks the channel when ready
/// 3. Wait for the shell to signal readiness (wait blocks until unlock)
/// 4. Clean up the channel
pub struct TmuxHandshake {
    channel: String,
}

impl TmuxHandshake {
    /// Create a new handshake and lock the channel.
    ///
    /// The channel must be locked before spawning the pane to ensure we don't
    /// miss the signal even if the shell starts instantly.
    pub fn new() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let channel = format!("wm_ready_{}_{}", pid, nanos);

        // Lock the channel (ensures we don't miss the signal)
        Cmd::new("tmux")
            .args(&["wait-for", "-L", &channel])
            .run()
            .context("Failed to initialize wait channel")?;

        Ok(Self { channel })
    }

    /// Wait until the pane has rendered non-empty, stable content.
    ///
    /// The initial wait-for signal is emitted before the shell is exec'd. Polling
    /// the pane afterwards keeps input out of the PTY while interactive shell
    /// initialization (including async prompt setup) is still in progress.
    fn wait_for_stable_pane(pane_id: &str) {
        let timeout = Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);
        let poll_interval = Duration::from_millis(PANE_READY_POLL_INTERVAL_MS);
        let readiness = poll_until_stable_pane(timeout, poll_interval, || {
            Cmd::new("tmux")
                .args(&["capture-pane", "-p", "-t", pane_id])
                .run_and_capture_stdout()
                .inspect_err(|error| {
                    trace!(pane_id, error = %error, "tmux:handshake pane capture failed");
                })
        });

        match readiness {
            PaneReadiness::Ready(elapsed) => {
                debug!(
                    pane_id,
                    elapsed_ms = elapsed.as_millis(),
                    "tmux:handshake pane ready"
                );
            }
            PaneReadiness::TimedOut { last_error } => {
                warn!(
                    pane_id,
                    timeout_secs = HANDSHAKE_TIMEOUT_SECS,
                    last_error = ?last_error,
                    "tmux:handshake pane readiness timeout; sending command anyway"
                );
            }
        }
    }
}

impl PaneHandshake for TmuxHandshake {
    /// Build a shell wrapper command that signals readiness.
    ///
    /// The wrapper briefly disables echo while signaling the channel, restores it,
    /// then exec's into the shell so the TTY starts in a normal state.
    ///
    /// We wrap in `sh -c "..."` with double quotes to ensure the command works when
    /// tmux's default-shell is a non-POSIX shell like nushell. Single-quote escaping
    /// (`'\''`) doesn't work reliably when nushell parses the command before passing
    /// it to sh.
    fn wrapper_command(&self, shell: &str) -> String {
        let escaped_shell = super::util::escape_for_sh_c_inner_single_quote(shell);
        format!(
            "sh -c \"stty -echo 2>/dev/null; tmux wait-for -U {}; stty echo 2>/dev/null; exec '{}' -l\"",
            self.channel, escaped_shell
        )
    }

    fn script_content(&self, shell: &str) -> String {
        format!(
            "stty -echo 2>/dev/null; tmux wait-for -U {}; stty echo 2>/dev/null; exec '{}' -l",
            self.channel, shell
        )
    }

    /// Wait for the shell to signal it is ready, then clean up.
    ///
    /// This method consumes the handshake to ensure cleanup happens exactly once.
    /// Uses a polling loop with timeout to prevent indefinite hangs if the pane
    /// fails to start.
    fn wait(self: Box<Self>) -> Result<()> {
        debug!(channel = %self.channel, "tmux:handshake start");

        let mut child = std::process::Command::new("tmux")
            .args(["wait-for", "-L", &self.channel])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn tmux wait-for command")?;

        let start = Instant::now();
        let timeout = Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        // Cleanup: unlock the channel we just re-locked
                        Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run()
                            .context("Failed to cleanup wait channel")?;
                        debug!(channel = %self.channel, "tmux:handshake success");
                        return Ok(());
                    } else {
                        // Attempt cleanup even on failure
                        let _ = Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run();
                        warn!(channel = %self.channel, status = ?status.code(), "tmux:handshake failed (wait-for error)");
                        return Err(anyhow!(
                            "Pane handshake failed - tmux wait-for returned error"
                        ));
                    }
                }
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait(); // Ensure process is reaped

                        // Attempt cleanup
                        let _ = Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run();

                        warn!(
                            channel = %self.channel,
                            timeout_secs = HANDSHAKE_TIMEOUT_SECS,
                            "tmux:handshake timeout"
                        );
                        return Err(anyhow!(
                            "Pane handshake timed out after {}s - shell may have failed to start",
                            HANDSHAKE_TIMEOUT_SECS
                        ));
                    }
                    trace!(
                        channel = %self.channel,
                        elapsed_ms = start.elapsed().as_millis(),
                        "tmux:handshake waiting"
                    );
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = Cmd::new("tmux")
                        .args(&["wait-for", "-U", &self.channel])
                        .run();
                    warn!(channel = %self.channel, error = %e, "tmux:handshake error");
                    return Err(anyhow!("Error waiting for pane handshake: {}", e));
                }
            }
        }
    }

    fn wait_for_shell(self: Box<Self>, pane_id: &str) -> Result<()> {
        self.wait()?;
        Self::wait_for_stable_pane(pane_id);
        Ok(())
    }
}

/// Unix named pipe (FIFO) based handshake for backends without wait-for.
///
/// Used by WezTerm and other backends that don't have a built-in synchronization
/// mechanism like tmux's wait-for.
#[cfg(unix)]
pub struct UnixPipeHandshake {
    pipe_path: PathBuf,
}

#[cfg(unix)]
impl UnixPipeHandshake {
    /// Create a new pipe handshake.
    ///
    /// Creates a named pipe (FIFO) that the shell will write to when ready.
    pub fn new() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();

        let pipe_path = std::env::temp_dir().join(format!("workmux_pipe_{}_{}", pid, nanos));

        // Create FIFO with 0o600 permissions (owner read/write only)
        let mode = Mode::S_IRUSR | Mode::S_IWUSR;
        nix::unistd::mkfifo(&pipe_path, mode).context("Failed to create named pipe")?;

        Ok(Self { pipe_path })
    }
}

#[cfg(unix)]
impl PaneHandshake for UnixPipeHandshake {
    fn wrapper_command(&self, shell: &str) -> String {
        let escaped_shell = super::util::escape_for_sh_c_inner_single_quote(shell);
        format!(
            "sh -c 'echo ready > {}; exec '\\''{}'\\'' -l'",
            self.pipe_path.display(),
            escaped_shell
        )
    }

    fn script_content(&self, shell: &str) -> String {
        format!(
            "echo ready > {}; exec '{}' -l",
            self.pipe_path.display(),
            shell
        )
    }

    fn wait(self: Box<Self>) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        const POLL_INTERVAL_MS: u64 = 50;

        // Open pipe for reading (non-blocking)
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.pipe_path)
            .context("Failed to open pipe for reading")?;

        let fd = file.as_raw_fd();
        let start = Instant::now();
        let timeout = Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);

        loop {
            // Check if data available via poll()
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            let poll_timeout_ms = POLL_INTERVAL_MS as i32;
            let ret = unsafe { libc::poll(&mut pollfd, 1, poll_timeout_ms) };

            if ret > 0 && (pollfd.revents & libc::POLLIN) != 0 {
                // Data available - read and verify we got data
                let mut buf = [0u8; 64];
                let bytes_read =
                    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                let _ = std::fs::remove_file(&self.pipe_path);

                if bytes_read > 0 {
                    return Ok(());
                } else {
                    // EOF (0) or error (-1) - writer closed without sending data
                    return Err(anyhow!("Pipe closed without receiving handshake signal"));
                }
            }

            if start.elapsed() >= timeout {
                let _ = std::fs::remove_file(&self.pipe_path);
                return Err(anyhow!(
                    "Pane handshake timed out after {}s - shell may have failed to start",
                    HANDSHAKE_TIMEOUT_SECS
                ));
            }

            // Continue polling
        }
    }
}

#[cfg(unix)]
impl Drop for UnixPipeHandshake {
    fn drop(&mut self) {
        // Clean up the pipe file if it still exists
        let _ = std::fs::remove_file(&self.pipe_path);
    }
}

#[cfg(test)]
mod tests {
    use super::{PaneContentStability, PaneReadiness, poll_until_stable_pane};
    use anyhow::anyhow;
    use std::collections::VecDeque;
    use std::time::Duration;

    #[test]
    fn pane_content_must_be_non_empty_and_stable() {
        let mut stability = PaneContentStability::default();

        assert!(!stability.observe(String::new()));
        assert!(!stability.observe("loading".to_string()));
        assert!(!stability.observe("prompt".to_string()));
        assert!(stability.observe("prompt".to_string()));
    }

    #[test]
    fn empty_pane_content_resets_stability() {
        let mut stability = PaneContentStability::default();

        assert!(!stability.observe("prompt".to_string()));
        assert!(!stability.observe(String::new()));
        assert!(!stability.observe("prompt".to_string()));
        assert!(stability.observe("prompt".to_string()));
    }

    #[test]
    fn stable_pane_poll_requires_consecutive_matching_captures() {
        let mut captures = VecDeque::from([
            Ok(String::new()),
            Ok("loading".to_string()),
            Err(anyhow!("capture failed")),
            Ok("loading".to_string()),
            Ok("prompt".to_string()),
            Ok("prompt".to_string()),
        ]);

        let readiness = poll_until_stable_pane(Duration::from_secs(1), Duration::ZERO, || {
            captures.pop_front().expect("capture sequence exhausted")
        });

        assert!(matches!(readiness, PaneReadiness::Ready(_)));
        assert!(captures.is_empty());
    }

    #[test]
    fn stable_pane_poll_times_out_after_capture_error() {
        let readiness = poll_until_stable_pane(Duration::ZERO, Duration::ZERO, || {
            Err(anyhow!("pane exited"))
        });

        assert_eq!(
            readiness,
            PaneReadiness::TimedOut {
                last_error: Some("pane exited".to_string()),
            }
        );
    }
}
