//! `workmux remote attach <name> <ex-id> --pane <i>`: relay one remote
//! session into the local pane this runs in.
//!
//! Each connection is a dedicated ssh on a fresh PTY running `node attach`.
//! Keystrokes are forwarded only while a connection is live and has shown
//! output; anything typed in between is dropped, never replayed late. When
//! the connection is lost, the relay draws a status line on the bottom row
//! and reconnects with backoff. A session that ended closes the pane.

use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::RemoteSpec;
use super::client::Node;
use super::install::Pin;
use super::node::attach::{DETACHED, SESSION_GONE};
use super::transport::{SshFailure, SshFailureKind, Transport};
use crate::config::Config;

const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);
/// An attachment that lasted this long resets the backoff.
const STABLE: Duration = Duration::from_secs(30);

/// Undo terminal input modes the remote tmux enabled: mouse reporting,
/// bracketed paste and focus events, and show the cursor.
const RESET_MODES: &str =
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?1004l\x1b[?25h";

/// How a connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// The remote session is gone: its program exited.
    SessionGone,
    /// The remote client was detached; the session still runs.
    Detached,
    /// ssh could not authenticate or verify the host.
    AuthFailed(String),
    /// The connection failed or dropped.
    TransportLost(String),
    /// `node attach` itself failed.
    RemoteError(String),
}

/// Map a finished connection to an [`Ending`].
pub fn classify(status: Option<i32>, stderr: &[u8], ssh_failure: Option<SshFailure>) -> Ending {
    if let Some(SshFailure { kind, message }) = ssh_failure {
        return match kind {
            SshFailureKind::Auth | SshFailureKind::HostKey => Ending::AuthFailed(message),
            SshFailureKind::Other => Ending::TransportLost(message),
        };
    }
    match status {
        Some(SESSION_GONE) => Ending::SessionGone,
        Some(DETACHED) => Ending::Detached,
        _ => {
            let text = String::from_utf8_lossy(stderr);
            let detail = super::protocol::first_line(&text)
                .map(|line| line.strip_prefix("Error: ").unwrap_or(line).to_string())
                .unwrap_or_else(|| match status {
                    Some(code) => format!("exit status {code}"),
                    None => "killed by a signal".to_string(),
                });
            Ending::RemoteError(detail)
        }
    }
}

/// Seconds to wait before retry number `attempt` (0-based).
pub fn backoff(attempt: u32) -> Duration {
    FIRST_RETRY
        .checked_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX))
        .unwrap_or(MAX_RETRY)
        .min(MAX_RETRY)
}

/// The bottom status line while not attached: state and keys first, so a
/// narrow pane still shows them, then the detail.
pub fn status_text(name: &str, ending: &Ending, retry_in: Option<Duration>) -> String {
    let (state, detail) = match ending {
        Ending::SessionGone => ("session ended", None),
        Ending::Detached => ("detached", None),
        Ending::AuthFailed(message) => ("authentication failed", Some(message)),
        Ending::TransportLost(message) => ("disconnected", Some(message)),
        Ending::RemoteError(message) => ("remote error", Some(message)),
    };
    let keys = match retry_in {
        Some(wait) => format!("retry in {}s · r retry · q close", wait.as_secs().max(1)),
        None => "r reconnect · q close".to_string(),
    };
    match detail {
        Some(detail) => format!(" {name}: {state} · {keys} · {detail} "),
        None => format!(" {name}: {state} · {keys} "),
    }
}

pub fn run(config: &Config, name: &str, ex_id: &str, pane: usize) -> Result<()> {
    let spec = RemoteSpec::resolve(config, name)?;
    let pin = match super::cache::by_execution(name, ex_id).and_then(|record| record.execution) {
        Some(execution) => Pin::installed(execution.node_bin),
        None => Pin::for_spec(&spec)?,
    };
    let node = Node {
        transport: Transport::open(&spec)?,
        spec,
        pin,
    };
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        bail!("workmux remote attach needs a terminal");
    }
    let _raw = RawMode::enable()?;
    Relay::new(node, ex_id.to_string(), pane).run()
}

enum Event {
    Input(Vec<u8>),
    InputClosed,
    Output(u64),
    Exited(u64, Option<i32>, Vec<u8>),
    Resize,
    Terminate,
}

enum State {
    Connecting,
    Attached {
        since: Instant,
    },
    Waiting {
        ending: Ending,
        retry_at: Option<Instant>,
    },
}

struct Connection {
    pid: libc::pid_t,
    master: Arc<File>,
    output: Option<thread::JoinHandle<()>>,
}

struct Relay {
    node: Node,
    ex_id: String,
    pane: usize,
    events: Sender<Event>,
    receiver: Receiver<Event>,
    /// Where keystrokes go; `Some` only while attached.
    input_target: Arc<Mutex<Option<Arc<File>>>>,
    generation: u64,
    attempt: u32,
}

impl Relay {
    fn new(node: Node, ex_id: String, pane: usize) -> Self {
        let (events, receiver) = mpsc::channel();
        Self {
            node,
            ex_id,
            pane,
            events,
            receiver,
            input_target: Arc::new(Mutex::new(None)),
            generation: 0,
            attempt: 0,
        }
    }

    fn run(mut self) -> Result<()> {
        self.watch_signals()?;
        self.read_input();
        let mut connection = Some(self.connect()?);
        let mut state = State::Connecting;
        loop {
            let event = match self.receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            };
            match event {
                Some(Event::Terminate) | Some(Event::InputClosed) => {
                    if let Some(connection) = connection.take() {
                        stop(connection);
                    }
                    return Ok(());
                }
                Some(Event::Output(generation)) if generation == self.generation => {
                    if matches!(state, State::Connecting) {
                        flush_input();
                        *self.input_target.lock().unwrap() =
                            connection.as_ref().map(|c| c.master.clone());
                        state = State::Attached {
                            since: Instant::now(),
                        };
                    }
                }
                Some(Event::Exited(generation, status, stderr))
                    if generation == self.generation =>
                {
                    *self.input_target.lock().unwrap() = None;
                    if let Some(mut finished) = connection.take()
                        && let Some(output) = finished.output.take()
                    {
                        let _ = output.join();
                    }
                    flush_input();
                    if let State::Attached { since } = state
                        && since.elapsed() >= STABLE
                    {
                        self.attempt = 0;
                    }
                    let failure = self.node.transport.terminal_failure(status, &stderr);
                    let ending = classify(status, &stderr, failure);
                    tracing::info!(ex_id = %self.ex_id, pane = self.pane, ?status, ?ending, "remote attach ended");
                    if ending == Ending::SessionGone {
                        return Ok(());
                    }
                    let retry_at =
                        matches!(ending, Ending::TransportLost(_) | Ending::RemoteError(_)).then(
                            || {
                                let wait = backoff(self.attempt);
                                self.attempt += 1;
                                Instant::now() + wait
                            },
                        );
                    write_out(RESET_MODES);
                    state = State::Waiting { ending, retry_at };
                    self.draw(&state);
                }
                Some(Event::Input(bytes)) => {
                    if let State::Waiting { .. } = state {
                        if bytes.contains(&b'q') {
                            return Ok(());
                        }
                        if bytes.contains(&b'r') {
                            connection = Some(self.connect()?);
                            state = State::Connecting;
                            self.draw(&state);
                        }
                    }
                }
                Some(Event::Resize) => {
                    let size = window_size(libc::STDOUT_FILENO);
                    if let Some(connection) = &connection {
                        set_window_size(connection.master.as_raw_fd(), &size);
                    }
                    self.draw(&state);
                }
                Some(_) => {}
                None => {
                    if let State::Waiting {
                        retry_at: Some(at), ..
                    } = &state
                    {
                        if Instant::now() >= *at {
                            connection = Some(self.connect()?);
                            state = State::Connecting;
                            self.draw(&state);
                        } else {
                            self.draw(&state);
                        }
                    }
                }
            }
        }
    }

    fn draw(&self, state: &State) {
        let name = &self.node.spec.name;
        let text = match state {
            State::Attached { .. } => return,
            State::Connecting => format!(" {name}: connecting… "),
            State::Waiting { ending, retry_at } => {
                let retry_in = retry_at.map(|at| at.saturating_duration_since(Instant::now()));
                status_text(name, ending, retry_in)
            }
        };
        let size = window_size(libc::STDOUT_FILENO);
        let width = usize::from(size.ws_col.max(1));
        let text: String = text.chars().take(width).collect();
        write_out(&format!(
            "\x1b7\x1b[{};1H\x1b[2K\x1b[7m{text}\x1b[0m\x1b8",
            size.ws_row.max(1)
        ));
    }

    /// Start a connection on a fresh PTY sized like this terminal.
    fn connect(&mut self) -> Result<Connection> {
        self.generation += 1;
        tracing::info!(ex_id = %self.ex_id, pane = self.pane, attempt = self.attempt, "remote attach connecting");
        let generation = self.generation;
        let size = window_size(libc::STDOUT_FILENO);
        let (master, slave) = open_pty(&size)?;
        let mut cmd = self.node.attach_command(&self.ex_id, self.pane);
        cmd.stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave))
            .stderr(Stdio::piped());
        // SAFETY: only async-signal-safe calls between fork and exec. The PTY
        // becomes the child's controlling terminal so it receives SIGWINCH.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().context("Failed to start ssh")?;
        let pid = child.id() as libc::pid_t;
        let master = Arc::new(File::from(master));

        let events = self.events.clone();
        let reader = master.clone();
        let output = thread::spawn(move || {
            let mut reader = &*reader;
            let mut stdout = std::io::stdout();
            let mut buf = [0u8; 16 * 1024];
            let mut announced = false;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout
                            .write_all(&buf[..n])
                            .and_then(|_| stdout.flush())
                            .is_err()
                        {
                            break;
                        }
                        if !announced {
                            announced = true;
                            let _ = events.send(Event::Output(generation));
                        }
                    }
                }
            }
        });

        let events = self.events.clone();
        let mut stderr = child.stderr.take();
        thread::spawn(move || {
            let mut captured = Vec::new();
            if let Some(stderr) = stderr.as_mut() {
                let _ = stderr.read_to_end(&mut captured);
            }
            let status = child.wait().ok().and_then(|status| status.code());
            let _ = events.send(Event::Exited(generation, status, captured));
        });

        Ok(Connection {
            pid,
            master,
            output: Some(output),
        })
    }

    /// Forward keystrokes while attached; otherwise hand them to the event
    /// loop, which only looks for `r` and `q`.
    fn read_input(&self) {
        let events = self.events.clone();
        let target = self.input_target.clone();
        thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = events.send(Event::InputClosed);
                        return;
                    }
                    Ok(n) => {
                        let guard = target.lock().unwrap();
                        match guard.as_ref() {
                            Some(master) => {
                                let _ = (&**master).write_all(&buf[..n]);
                            }
                            None => {
                                drop(guard);
                                let _ = events.send(Event::Input(buf[..n].to_vec()));
                            }
                        }
                    }
                }
            }
        });
    }

    fn watch_signals(&self) -> Result<()> {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
        let mut signals = signal_hook::iterator::Signals::new([SIGWINCH, SIGHUP, SIGTERM, SIGINT])?;
        let events = self.events.clone();
        thread::spawn(move || {
            for signal in signals.forever() {
                let event = if signal == SIGWINCH {
                    Event::Resize
                } else {
                    Event::Terminate
                };
                if events.send(event).is_err() {
                    return;
                }
            }
        });
        Ok(())
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        write_out(RESET_MODES);
    }
}

fn stop(connection: Connection) {
    // SAFETY: plain kill(2) of a child we spawned.
    unsafe {
        libc::kill(connection.pid, libc::SIGTERM);
    }
}

fn write_out(text: &str) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// Drop keystrokes the terminal has queued but nobody read yet.
fn flush_input() {
    // SAFETY: tcflush on our own stdin.
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}

fn window_size(fd: RawFd) -> libc::winsize {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes a winsize into `size`.
    let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0;
    if !ok || size.ws_row == 0 || size.ws_col == 0 {
        size.ws_row = 24;
        size.ws_col = 80;
    }
    size
}

fn set_window_size(fd: RawFd, size: &libc::winsize) {
    // SAFETY: TIOCSWINSZ reads a winsize.
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, size);
    }
}

/// A new PTY pair; the master is close-on-exec so ssh only gets the slave.
fn open_pty(size: &libc::winsize) -> Result<(OwnedFd, OwnedFd)> {
    // SAFETY: standard posix_openpt sequence; every descriptor is owned.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(std::io::Error::last_os_error()).context("Failed to open a PTY");
        }
        let master = OwnedFd::from_raw_fd(master);
        let fd = master.as_raw_fd();
        if libc::grantpt(fd) != 0 || libc::unlockpt(fd) != 0 {
            return Err(std::io::Error::last_os_error()).context("Failed to unlock the PTY");
        }
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        let name = libc::ptsname(fd);
        if name.is_null() {
            return Err(std::io::Error::last_os_error()).context("Failed to name the PTY");
        }
        let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if slave < 0 {
            return Err(std::io::Error::last_os_error()).context("Failed to open the PTY");
        }
        let slave = OwnedFd::from_raw_fd(slave);
        set_window_size(slave.as_raw_fd(), size);
        Ok((master, slave))
    }
}

/// Raw mode on the terminal for the guard's lifetime.
struct RawMode;

impl RawMode {
    fn enable() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("Failed to set raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exits_map_to_endings() {
        assert_eq!(classify(Some(SESSION_GONE), b"", None), Ending::SessionGone);
        assert_eq!(classify(Some(DETACHED), b"", None), Ending::Detached);
        assert_eq!(
            classify(
                Some(255),
                b"",
                Some(SshFailure {
                    kind: SshFailureKind::Auth,
                    message: "denied".into()
                })
            ),
            Ending::AuthFailed("denied".into())
        );
        assert_eq!(
            classify(
                Some(255),
                b"",
                Some(SshFailure {
                    kind: SshFailureKind::HostKey,
                    message: "changed".into()
                })
            ),
            Ending::AuthFailed("changed".into())
        );
        assert_eq!(
            classify(
                Some(255),
                b"",
                Some(SshFailure {
                    kind: SshFailureKind::Other,
                    message: "timed out".into()
                })
            ),
            Ending::TransportLost("timed out".into())
        );
        assert_eq!(
            classify(Some(127), b"sh: 1: workmux: not found\n", None),
            Ending::RemoteError("sh: 1: workmux: not found".into())
        );
        assert_eq!(
            classify(Some(1), b"Error: tmux attach failed\n", None),
            Ending::RemoteError("tmux attach failed".into())
        );
        assert_eq!(
            classify(None, b"", None),
            Ending::RemoteError("killed by a signal".into())
        );
    }

    #[test]
    fn backoff_doubles_to_the_cap() {
        let secs: Vec<u64> = (0..8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(backoff(200), MAX_RETRY);
    }

    #[test]
    fn status_line_names_reason_and_keys() {
        let text = status_text(
            "box",
            &Ending::TransportLost("timed out".into()),
            Some(Duration::from_secs(4)),
        );
        assert_eq!(
            text,
            " box: disconnected · retry in 4s · r retry · q close · timed out "
        );
        let text = status_text("box", &Ending::Detached, None);
        assert_eq!(text, " box: detached · r reconnect · q close ");
    }
}
