//! `workmux remote attach <name> <ex-id> --pane <i>`: relay one remote
//! session into the local pane this runs in.
//!
//! Each connection is a dedicated ssh on a fresh PTY running `node attach`.
//! Keystrokes are forwarded only while a connection is live and has shown
//! output; anything typed in between is dropped, never replayed late. When
//! the connection is lost, the relay draws a status line on the bottom row
//! and reconnects with backoff. A session that ended closes the pane.
//! After the machine sleeps, the relay reconnects at once instead of waiting
//! for ssh to notice that the old connection died.

use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};

use super::RemoteSpec;
use super::cache::WorkspaceRecord;
use super::client::Node;
use super::install::Pin;
use super::node::attach::{DETACHED, SESSION_GONE};
use super::node::observe::{ObservedAgent, ObservedPane};
use super::observe::Observation;
use super::transport::{SshFailure, SshFailureKind, Transport};
use crate::command::set_window_status;
use crate::config::Config;
use crate::multiplexer::{Multiplexer, RemoteRef};
use crate::state::{PromptUpdate, RemoteUpdate};

const FIRST_RETRY: Duration = Duration::from_secs(1);
/// How often the relay looks at the remote's observation.
const MIRROR_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);
/// An attachment or stream that lasted this long resets the backoff.
pub(super) const STABLE: Duration = Duration::from_secs(30);
/// A wall-clock gap this long between two turns of the relay loop, which
/// turns at least every [`MIRROR_INTERVAL`], means the machine slept or the
/// process was suspended. The monotonic clock stops during sleep, so the
/// wall clock is what shows it.
const WAKE_GAP: Duration = Duration::from_secs(10);

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

/// Whether the wall clock moved far enough between two loop turns to mean
/// the relay was asleep. A clock set backwards is not a wake.
fn woke(last: SystemTime, now: SystemTime) -> bool {
    now.duration_since(last).is_ok_and(|gap| gap >= WAKE_GAP)
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
    let record = super::cache::by_execution(name, ex_id);
    let pin = match record.as_ref().and_then(|record| record.execution.clone()) {
        Some(execution) => Pin::installed(execution.node_bin),
        None => Pin::for_spec(&spec)?,
    };
    let status = record.and_then(|record| StatusMirror::start(config, &record, ex_id, pane));
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
    let mut relay = Relay::new(node, ex_id.to_string(), pane.to_string());
    relay.status = status;
    relay.run().map(|_| ())
}

/// Relay session `slot` of `ex_id` on `node` into this terminal, which must
/// be one, until the session ends ([`Left::SessionEnded`]) or the user
/// leaves the relay.
pub fn relay_session(node: Node, ex_id: &str, slot: &str) -> Result<Left> {
    let _raw = RawMode::enable()?;
    Relay::new(node, ex_id.to_string(), slot.to_string()).run()
}

/// Why a relay stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Left {
    /// The remote session ended.
    SessionEnded,
    /// The user closed the relay, or its terminal went away; the session
    /// may still run.
    Relay,
}

/// What a relay does about the observation of its remote pane.
#[derive(Debug, PartialEq)]
enum Mirror {
    /// Disconnected, not observed yet, or unchanged.
    Nothing,
    /// The node's status changed: show it.
    Status(ObservedAgent),
    /// Only the title, kind or prompt changed; the icon stays as it is, so
    /// a focus acknowledgement survives.
    Metadata(ObservedAgent),
    /// No agent runs in the pane any more.
    Clear,
}

/// Compare the observed pane with what was last mirrored (`None` before
/// the first observation).
fn mirror_step(shown: Option<&Option<ObservedAgent>>, observed: Option<&ObservedPane>) -> Mirror {
    let Some(observed) = observed else {
        return Mirror::Nothing;
    };
    match (&observed.agent, shown) {
        (Some(agent), Some(Some(previous)))
            if (previous.status, previous.status_ts) == (agent.status, agent.status_ts) =>
        {
            if previous == agent {
                Mirror::Nothing
            } else {
                Mirror::Metadata(agent.clone())
            }
        }
        (Some(agent), _) => Mirror::Status(agent.clone()),
        (None, Some(None)) => Mirror::Nothing,
        (None, _) => Mirror::Clear,
    }
}

/// Shows the remote agent's state on the local pane the relay runs in,
/// through the same path as local status hooks. It converges on the
/// latest observation instead of replaying transitions.
struct StatusMirror {
    config: Config,
    mux: Arc<dyn Multiplexer>,
    pane_id: String,
    at: RemoteRef,
    workdir: std::path::PathBuf,
    shown: Option<Option<ObservedAgent>>,
    checked: Option<Instant>,
}

impl StatusMirror {
    /// Register the local pane as relaying the remote pane, with no status,
    /// and start observing the remote. `None` outside a multiplexer pane.
    fn start(config: &Config, record: &WorkspaceRecord, ex_id: &str, pane: usize) -> Option<Self> {
        let mux = crate::multiplexer::create_backend(crate::multiplexer::detect_backend());
        let pane_id = mux.current_pane_id()?;
        let mirror = Self {
            config: config.clone(),
            mux,
            pane_id,
            at: RemoteRef {
                remote: record.remote.clone(),
                ws_id: record.ws_id.clone(),
                ex_id: ex_id.to_string(),
                pane,
            },
            workdir: record.local_path.clone(),
            shown: None,
            checked: None,
        };
        crate::state::persist_remote_registration(
            mirror.mux.as_ref(),
            &mirror.pane_id,
            mirror.update(None, None),
        );
        super::observe::spawn(config, std::slice::from_ref(&record.remote));
        Some(mirror)
    }

    fn update(
        &self,
        agent: Option<&ObservedAgent>,
        observation: Option<&Observation>,
    ) -> RemoteUpdate {
        RemoteUpdate {
            at: self.at.clone(),
            workdir: self.workdir.clone(),
            status_ts: agent
                .and_then(|agent| agent.status_ts)
                .zip(observation)
                .map(|(ts, observation)| observation.local_ts(ts)),
            title: agent.and_then(|agent| agent.title.clone()),
            agent_kind: agent.and_then(|agent| agent.agent_kind.clone()),
        }
    }

    /// Mirror the current observation, at most once a second.
    fn tick(&mut self) {
        if self
            .checked
            .is_some_and(|at| at.elapsed() < MIRROR_INTERVAL)
        {
            return;
        }
        self.checked = Some(Instant::now());
        let Some(observation) = super::observe::load_connected(&self.at.remote) else {
            return;
        };
        let observed = observation.pane(&self.at.ws_id, &self.at.ex_id, self.at.pane);
        let step = mirror_step(self.shown.as_ref(), observed);
        let mux = self.mux.as_ref();
        let result = match &step {
            Mirror::Nothing => return,
            Mirror::Status(agent) => {
                let update = self.update(Some(agent), Some(&observation));
                let applied = set_window_status::apply_status(
                    &self.config,
                    mux,
                    &self.pane_id,
                    agent.status,
                    None,
                    agent.prompt.as_deref(),
                    Some(update.clone()),
                );
                if agent.status.is_none() {
                    crate::state::persist_remote_update(
                        mux,
                        &self.pane_id,
                        None,
                        update,
                        PromptUpdate::Keep,
                    );
                }
                applied
            }
            Mirror::Metadata(agent) => {
                let update = self.update(Some(agent), Some(&observation));
                crate::state::persist_remote_update(
                    mux,
                    &self.pane_id,
                    None,
                    update,
                    PromptUpdate::Keep,
                );
                crate::command::sidebar::request_refresh_for(mux);
                Ok(())
            }
            Mirror::Clear => set_window_status::apply_status(
                &self.config,
                mux,
                &self.pane_id,
                None,
                None,
                None,
                None,
            ),
        };
        match result {
            Ok(()) => {
                self.shown = Some(match step {
                    Mirror::Status(agent) | Mirror::Metadata(agent) => Some(agent),
                    _ => None,
                });
            }
            Err(err) => {
                tracing::warn!(pane = %self.pane_id, "failed to mirror remote status: {err:#}")
            }
        }
    }
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
    /// The session's slot: a pane index or a shell.
    slot: String,
    events: Sender<Event>,
    receiver: Receiver<Event>,
    /// Where keystrokes go; `Some` only while attached.
    input_target: Arc<Mutex<Option<Arc<File>>>>,
    generation: u64,
    attempt: u32,
    status: Option<StatusMirror>,
    /// The row of the last painted status line, cleared when a resize
    /// moves the bottom row away from it.
    painted_row: Option<u16>,
}

impl Relay {
    fn new(node: Node, ex_id: String, slot: String) -> Self {
        let (events, receiver) = mpsc::channel();
        Self {
            node,
            ex_id,
            slot,
            events,
            receiver,
            input_target: Arc::new(Mutex::new(None)),
            generation: 0,
            attempt: 0,
            status: None,
            painted_row: None,
        }
    }

    fn run(mut self) -> Result<Left> {
        self.watch_signals()?;
        self.read_input();
        let mut connection = Some(self.connect()?);
        let mut state = State::Connecting;
        let mut last_turn = SystemTime::now();
        loop {
            if let Some(status) = &mut self.status {
                status.tick();
            }
            let event = match self.receiver.recv_timeout(MIRROR_INTERVAL) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return Ok(Left::Relay),
            };
            let now = SystemTime::now();
            // A connection that slept with the machine usually died with its
            // network, but ssh only notices after its keepalives time out.
            // Attaching again repaints the current screen right away. States
            // waiting on the user, such as detached, stay as they are.
            let retrying = match &state {
                State::Waiting { retry_at, .. } => retry_at.is_some(),
                State::Connecting | State::Attached { .. } => true,
            };
            if woke(last_turn, now) && retrying {
                tracing::info!(ex_id = %self.ex_id, slot = %self.slot, "remote attach woke from sleep; reconnecting");
                *self.input_target.lock().unwrap() = None;
                if let Some(old) = connection.take() {
                    stop(old);
                }
                flush_input();
                write_out(RESET_MODES);
                self.attempt = 0;
                connection = Some(self.connect()?);
                state = State::Connecting;
                self.draw(&state);
            }
            last_turn = now;
            match event {
                Some(Event::Terminate) | Some(Event::InputClosed) => {
                    if let Some(connection) = connection.take() {
                        stop(connection);
                    }
                    return Ok(Left::Relay);
                }
                Some(Event::Output(generation)) if generation == self.generation => {
                    if matches!(state, State::Connecting) {
                        flush_input();
                        *self.input_target.lock().unwrap() =
                            connection.as_ref().map(|c| c.master.clone());
                        // The remote client repaints the whole screen.
                        self.painted_row = None;
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
                    tracing::info!(ex_id = %self.ex_id, slot = %self.slot, ?status, ?ending, "remote attach ended");
                    if ending == Ending::SessionGone {
                        return Ok(Left::SessionEnded);
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
                            return Ok(Left::Relay);
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

    fn draw(&mut self, state: &State) {
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
        let row = size.ws_row.max(1);
        let stale = match self.painted_row.replace(row) {
            Some(old) if old < row => format!("\x1b[{old};1H\x1b[2K"),
            _ => String::new(),
        };
        write_out(&format!(
            "\x1b7{stale}\x1b[{row};1H\x1b[2K\x1b[7m{text}\x1b[0m\x1b8"
        ));
    }

    /// Start a connection on a fresh PTY sized like this terminal.
    fn connect(&mut self) -> Result<Connection> {
        self.generation += 1;
        tracing::info!(ex_id = %self.ex_id, slot = %self.slot, attempt = self.attempt, "remote attach connecting");
        let generation = self.generation;
        // The remote tmux client restores this cursor when it leaves and
        // prints its exit message there. Left wherever the last one ended,
        // the messages creep down until they scroll the screen, carrying
        // painted status lines up with them.
        write_out("\x1b[r\x1b[H");
        let size = window_size(libc::STDOUT_FILENO);
        let (master, slave) = open_pty(&size)?;
        let mut cmd = self.node.attach_command(&self.ex_id, &self.slot);
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

    fn observed(agent: Option<ObservedAgent>) -> ObservedPane {
        ObservedPane {
            index: 0,
            alive: true,
            agent,
        }
    }

    fn agent(status: crate::multiplexer::AgentStatus, ts: u64, title: &str) -> ObservedAgent {
        ObservedAgent {
            status: Some(status),
            status_ts: Some(ts),
            title: Some(title.into()),
            agent_kind: Some("claude".into()),
            prompt: None,
        }
    }

    #[test]
    fn mirror_applies_changes_once() {
        use crate::multiplexer::AgentStatus::{Done, Working};
        let working = agent(Working, 10, "a");
        // Not observed: nothing to mirror.
        assert_eq!(mirror_step(None, None), Mirror::Nothing);
        // First observation shows the agent.
        let pane = observed(Some(working.clone()));
        assert_eq!(
            mirror_step(None, Some(&pane)),
            Mirror::Status(working.clone())
        );
        // Unchanged: an acknowledged icon is not re-armed.
        let shown = Some(working.clone());
        assert_eq!(mirror_step(Some(&shown), Some(&pane)), Mirror::Nothing);
        // Title only.
        let retitled = agent(Working, 10, "b");
        assert_eq!(
            mirror_step(Some(&shown), Some(&observed(Some(retitled.clone())))),
            Mirror::Metadata(retitled)
        );
        // New transition, including the same status again later.
        let done = agent(Done, 20, "a");
        assert_eq!(
            mirror_step(Some(&shown), Some(&observed(Some(done.clone())))),
            Mirror::Status(done)
        );
        let again = agent(Working, 30, "a");
        assert_eq!(
            mirror_step(Some(&shown), Some(&observed(Some(again.clone())))),
            Mirror::Status(again)
        );
        // The agent exited, once.
        assert_eq!(
            mirror_step(Some(&shown), Some(&observed(None))),
            Mirror::Clear
        );
        assert_eq!(mirror_step(None, Some(&observed(None))), Mirror::Clear);
        assert_eq!(
            mirror_step(Some(&None), Some(&observed(None))),
            Mirror::Nothing
        );
    }

    #[test]
    fn a_long_wall_clock_gap_is_a_wake() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(!woke(t, t + MIRROR_INTERVAL));
        assert!(!woke(t, t + WAKE_GAP - Duration::from_millis(1)));
        assert!(woke(t, t + WAKE_GAP));
        assert!(woke(t, t + Duration::from_secs(3600)));
        assert!(!woke(t + Duration::from_secs(3600), t));
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
