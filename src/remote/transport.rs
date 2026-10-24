//! Run POSIX `sh` scripts on a remote node.
//!
//! SSH runs share one ControlMaster per remote. The remote login shell
//! re-parses the command string, so scripts travel base64-encoded inside a
//! fixed `sh -c` one-liner whose only variable part is `[A-Za-z0-9+/=]`;
//! this keeps quoting identical under bash, zsh, fish, tcsh and others.
//! Loopback runs the same encoded command with the local `sh`, for tests.

use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use sha2::{Digest, Sha256};

use super::RemoteSpec;
use super::protocol::first_line;

/// Environment variable that selects the loopback transport.
pub const LOOPBACK_ENV: &str = "WORKMUX_REMOTE_LOOPBACK";

const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// OpenSSH binds `<ControlPath>.<16 hex>` before renaming it into place, and
/// `sun_path` holds 104 bytes on macOS including the terminating NUL.
const MAX_CONTROL_PATH_LEN: usize = 104 - 1 - 17;

/// Exit status ssh uses for its own failures.
const SSH_ERROR_STATUS: i32 = 255;

/// Captured result of a finished command.
#[derive(Debug)]
pub struct Output {
    /// Exit code, or `None` when killed by a signal.
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// First non-empty stderr line, for error messages.
    pub fn stderr_line(&self) -> String {
        first_line(&String::from_utf8_lossy(&self.stderr))
            .unwrap_or_default()
            .to_string()
    }
}

/// Stdin for a remote script.
pub enum Input {
    Null,
    File(PathBuf),
}

pub struct Transport {
    kind: Kind,
}

enum Kind {
    Ssh {
        name: String,
        host: String,
        socket: PathBuf,
    },
    Loopback,
}

impl Transport {
    /// Transport for a validated remote. Does not connect.
    pub fn open(spec: &RemoteSpec) -> Result<Self> {
        if std::env::var_os(LOOPBACK_ENV).is_some_and(|value| value == "1") {
            return Ok(Self {
                kind: Kind::Loopback,
            });
        }
        let socket = control_socket(&spec.name, &spec.host)?;
        Ok(Self {
            kind: Kind::Ssh {
                name: spec.name.clone(),
                host: spec.host.clone(),
                socket,
            },
        })
    }

    /// Ensure a working connection, replacing a master whose connection died.
    pub fn connect(&self) -> Result<()> {
        let Kind::Ssh { name, host, socket } = &self.kind else {
            return Ok(());
        };
        if socket.symlink_metadata().is_ok() {
            let wedged = match run_bounded(
                self.control(socket, host, "check"),
                Stdio::null(),
                Some(CHECK_TIMEOUT),
            )? {
                // The master answers; prove its connection still carries sessions.
                Some(check) if check.success() => {
                    let probe = run_bounded(
                        self.ssh(socket, host, &remote_command("exit 0")),
                        Stdio::null(),
                        Some(PROBE_TIMEOUT),
                    )?;
                    if probe.is_some_and(|probe| probe.success()) {
                        return Ok(());
                    }
                    true
                }
                // No master behind the socket: OpenSSH replaces it on connect.
                Some(_) => false,
                // The master accepted the check but never answered.
                None => true,
            };
            if wedged {
                tracing::info!(host, "replacing unresponsive ssh control master");
                // `stop` (unlike `exit`) lets sessions other processes still
                // run through the old master finish.
                let _ = run_bounded(
                    self.control(socket, host, "stop"),
                    Stdio::null(),
                    Some(CHECK_TIMEOUT),
                );
                match fs::remove_file(socket) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(err).with_context(|| {
                            format!("Failed to remove stale ssh socket {}", socket.display())
                        });
                    }
                }
            }
        }
        let output = run_bounded(
            self.ssh(socket, host, &remote_command("exit 0")),
            Stdio::null(),
            Some(CONNECT_TIMEOUT),
        )?
        .ok_or_else(|| {
            anyhow!(
                "ssh {host}: no response within {}s",
                CONNECT_TIMEOUT.as_secs()
            )
        })?;
        if !output.success() {
            bail!(classify_ssh_failure(name, host, &output));
        }
        Ok(())
    }

    /// Run `script` with POSIX `sh` on the node. Returns the script's own
    /// result; ssh failures and timeouts are errors.
    pub fn run(&self, script: &str, input: Input, timeout: Option<Duration>) -> Result<Output> {
        let stdin = match input {
            Input::Null => Stdio::null(),
            Input::File(path) => Stdio::from(
                fs::File::open(&path)
                    .with_context(|| format!("Failed to open {}", path.display()))?,
            ),
        };
        let command = remote_command(script);
        let (cmd, remote) = match &self.kind {
            Kind::Ssh { name, host, socket } => {
                (self.ssh(socket, host, &command), Some((name, host)))
            }
            Kind::Loopback => {
                let mut cmd = Command::new("sh");
                cmd.arg("-c").arg(&command);
                (cmd, None)
            }
        };
        let output = run_bounded(cmd, stdin, timeout)?.ok_or_else(|| {
            anyhow!(
                "remote command timed out after {}s",
                timeout.unwrap_or_default().as_secs()
            )
        })?;
        if let Some((name, host)) = remote
            && output.status == Some(SSH_ERROR_STATUS)
        {
            bail!(classify_ssh_failure(name, host, &output));
        }
        Ok(output)
    }

    /// Command that closes the shared connection, so the next run starts a
    /// fresh login (for example to pick up new group membership).
    pub fn disconnect_hint(&self) -> Option<String> {
        match &self.kind {
            Kind::Ssh { host, socket, .. } => Some(format!(
                "ssh -O exit -o ControlPath={} {}",
                crate::shell::shell_quote(&socket.to_string_lossy()),
                crate::shell::shell_quote(host)
            )),
            Kind::Loopback => None,
        }
    }

    fn ssh(&self, socket: &Path, host: &str, remote_command: &str) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.args(ssh_args(socket, host, remote_command));
        cmd
    }

    fn control(&self, socket: &Path, host: &str, operation: &str) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!("ControlPath={}", socket.display()))
            .arg("-O")
            .arg(operation)
            .arg("--")
            .arg(host);
        cmd
    }
}

fn ssh_args(socket: &Path, host: &str, remote_command: &str) -> Vec<String> {
    let mut args = vec!["-T".to_string()];
    for option in [
        "BatchMode=yes",
        "ConnectTimeout=10",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
        "ControlMaster=auto",
        "ControlPersist=10m",
        &format!("ControlPath={}", socket.display()),
        "RemoteCommand=none",
        "ForwardAgent=no",
        "ForwardX11=no",
        "ClearAllForwardings=yes",
    ] {
        args.push("-o".to_string());
        args.push(option.to_string());
    }
    args.push("--".to_string());
    args.push(host.to_string());
    args.push(remote_command.to_string());
    args
}

/// Command string that decodes and runs `script` with `sh`, immune to the
/// quoting rules of whatever login shell receives it.
pub fn remote_command(script: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(script);
    format!("sh -c 'eval \"$(printf %s \"$1\" | base64 -d)\"' workmux {encoded}")
}

fn classify_ssh_failure(name: &str, host: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = if stderr.contains("Permission denied") {
        format!(
            "authentication failed. workmux runs ssh in batch mode and never prompts; \
             make sure `ssh {host}` logs in without a password prompt (ssh-agent or an \
             unencrypted key)"
        )
    } else if stderr.contains("Host key verification failed") {
        format!("host key is not trusted. Run `ssh {host}` once to verify and accept it")
    } else if stderr.contains("Could not resolve hostname") {
        format!("cannot resolve the host name. Check remotes.{name}.host and ~/.ssh/config")
    } else if stderr.contains("Connection refused") {
        "connection refused. Is sshd running and the port correct?".to_string()
    } else if stderr.contains("timed out") {
        "connection timed out".to_string()
    } else {
        match output.stderr_line() {
            line if line.is_empty() => "ssh failed without an error message".to_string(),
            line => line,
        }
    };
    format!("ssh {host}: {reason}")
}

/// Spawn `cmd` and collect its output. Returns `None` if `timeout` elapses
/// first, after killing the process.
fn run_bounded(
    mut cmd: Command,
    stdin: Stdio,
    timeout: Option<Duration>,
) -> Result<Option<Output>> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to run {program}"))?;
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());
    let Some(status) = wait_with_deadline(&mut child, timeout)? else {
        return Ok(None);
    };
    Ok(Some(Output {
        status: status.code(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    }))
}

fn read_in_background(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

fn wait_with_deadline(
    child: &mut Child,
    timeout: Option<Duration>,
) -> Result<Option<std::process::ExitStatus>> {
    let Some(timeout) = timeout else {
        return Ok(Some(child.wait()?));
    };
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// ControlPath for a remote. Keyed by name and host so a changed `host`
/// never reuses a master connected elsewhere.
fn control_socket(name: &str, host: &str) -> Result<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let path = socket_path(&crate::xdg::state_dir()?.join("ssh"), name, host, uid);
    let dir = path.parent().expect("socket path has a parent");
    ensure_private_dir(dir, uid)?;
    Ok(path)
}

fn socket_path(preferred_dir: &Path, name: &str, host: &str, uid: u32) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(host.as_bytes());
    let key = format!("{:x}", hasher.finalize());
    let key = &key[..16];
    let preferred = preferred_dir.join(key);
    if is_literal_control_path(&preferred) {
        preferred
    } else {
        PathBuf::from(format!("/tmp/workmux-ssh-{uid}")).join(key)
    }
}

/// Whether ssh binds exactly `path` for `-o ControlPath=<path>`: OpenSSH
/// splits option values on whitespace and quotes, expands `%` tokens and
/// `${VAR}`, and limits socket paths to `sun_path`.
fn is_literal_control_path(path: &Path) -> bool {
    path.as_os_str().len() <= MAX_CONTROL_PATH_LEN
        && path.to_str().is_some_and(|text| {
            text.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
        })
}

/// Create `dir` if needed and require it to be a real directory private to
/// `uid`: anyone who can write it could plant a socket that impersonates the
/// master.
fn ensure_private_dir(dir: &Path, uid: u32) -> Result<()> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(err).with_context(|| format!("Failed to create {}", dir.display()));
        }
    }
    let meta = dir
        .symlink_metadata()
        .with_context(|| format!("Failed to inspect {}", dir.display()))?;
    if !meta.file_type().is_dir() {
        bail!(
            "ssh socket directory {} is not a directory (or is a symlink); remove it",
            dir.display()
        );
    }
    if meta.uid() != uid {
        bail!(
            "ssh socket directory {} is owned by uid {}, not you; remove it",
            dir.display(),
            meta.uid()
        );
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("Failed to restrict permissions of {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_args_end_with_separator_host_command() {
        let args = ssh_args(Path::new("/s/abc"), "-oProxyCommand=x", "cmd");
        let n = args.len();
        assert_eq!(&args[n - 3..], ["--", "-oProxyCommand=x", "cmd"]);
        assert_eq!(args[0], "-T");
        for option in [
            "BatchMode=yes",
            "ControlMaster=auto",
            "ControlPath=/s/abc",
            "ServerAliveInterval=15",
            "RemoteCommand=none",
            "ForwardAgent=no",
        ] {
            assert!(args.iter().any(|arg| arg == option), "missing {option}");
        }
    }

    #[test]
    fn socket_path_depends_on_name_and_host() {
        let dir = Path::new("/state/ssh");
        let a = socket_path(dir, "hetzner", "host-a", 501);
        assert!(a.starts_with(dir));
        assert_eq!(a.file_name().unwrap().len(), 16);
        assert_ne!(a, socket_path(dir, "hetzner", "host-b", 501));
        assert_ne!(a, socket_path(dir, "other", "host-a", 501));
        assert_eq!(a, socket_path(dir, "hetzner", "host-a", 501));
    }

    #[test]
    fn long_state_dir_falls_back_to_tmp() {
        let dir = PathBuf::from(format!("/{}/ssh", "x".repeat(80)));
        let path = socket_path(&dir, "hetzner", "host", 501);
        assert!(path.starts_with("/tmp/workmux-ssh-501"), "{path:?}");
        assert!(path.as_os_str().len() <= MAX_CONTROL_PATH_LEN);
    }

    #[test]
    fn state_dir_ssh_would_reinterpret_falls_back_to_tmp() {
        for dir in [
            "/tmp/a b/ssh",
            "/tmp/a%h/ssh",
            "/tmp/${HOME}/ssh",
            "/tmp/a\"b/ssh",
        ] {
            let path = socket_path(Path::new(dir), "hetzner", "host", 501);
            assert!(path.starts_with("/tmp/workmux-ssh-501"), "{dir}: {path:?}");
        }
        let path = socket_path(Path::new("/Users/me/.local/state/workmux/ssh"), "h", "x", 1);
        assert!(
            path.starts_with("/Users/me/.local/state/workmux/ssh"),
            "{path:?}"
        );
    }

    #[test]
    fn private_dir_is_created_and_tightened() {
        let tmp = tempfile::tempdir().unwrap();
        let uid = unsafe { libc::getuid() };
        let dir = tmp.path().join("state/ssh");
        ensure_private_dir(&dir, uid).unwrap();
        assert_eq!(dir.metadata().unwrap().mode() & 0o777, 0o700);

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        ensure_private_dir(&dir, uid).unwrap();
        assert_eq!(dir.metadata().unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn private_dir_rejects_symlink_and_foreign_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let uid = unsafe { libc::getuid() };
        let target = tmp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = ensure_private_dir(&link, uid).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");

        let err = ensure_private_dir(&target, uid + 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("owned by uid"), "{err}");
    }

    const TRICKY_SCRIPT: &str =
        "printf '%s\\n' 'back\\\\slash' \"dq \\\"x\\\"\" 'it'\\''s'\nprintf 'line2 $HOME?\\n'\n";

    fn decoded_by(shell: &str) -> Option<String> {
        which::which(shell).ok()?;
        let output = Command::new(shell)
            .arg("-c")
            .arg(remote_command(TRICKY_SCRIPT))
            .env("HOME", "/h")
            .output()
            .unwrap();
        assert!(output.status.success(), "{shell}: {output:?}");
        Some(String::from_utf8(output.stdout).unwrap())
    }

    #[test]
    fn encoded_command_runs_identically_under_login_shells() {
        let expected = "back\\\\slash\ndq \"x\"\nit's\nline2 $HOME?\n";
        assert_eq!(decoded_by("sh").as_deref(), Some(expected));
        for shell in ["bash", "zsh", "fish", "tcsh", "nu"] {
            if let Some(out) = decoded_by(shell) {
                assert_eq!(out, expected, "{shell}");
            }
        }
    }

    #[test]
    fn ssh_failures_are_classified() {
        let output = |stderr: &str| Output {
            status: Some(255),
            stdout: vec![],
            stderr: stderr.as_bytes().to_vec(),
        };
        let cases = [
            (
                "dev@h: Permission denied (publickey).",
                "authentication failed",
            ),
            ("Host key verification failed.", "host key is not trusted"),
            (
                "ssh: Could not resolve hostname nope: nodename nor servname provided",
                "cannot resolve",
            ),
            (
                "ssh: connect to host h port 22: Connection refused",
                "refused",
            ),
            (
                "ssh: connect to host h port 22: Operation timed out",
                "timed out",
            ),
            (
                "kex_exchange_identification: read: Connection reset",
                "kex_exchange",
            ),
        ];
        for (stderr, expected) in cases {
            let message = classify_ssh_failure("r", "h", &output(stderr));
            assert!(message.contains(expected), "{stderr} -> {message}");
            assert!(message.starts_with("ssh h: "), "{message}");
        }
    }

    #[test]
    fn run_bounded_times_out_and_kills() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 5");
        let start = Instant::now();
        let result = run_bounded(cmd, Stdio::null(), Some(Duration::from_millis(200))).unwrap();
        assert!(result.is_none());
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn run_bounded_captures_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo out; echo err >&2; exit 3");
        let output = run_bounded(cmd, Stdio::null(), Some(Duration::from_secs(5)))
            .unwrap()
            .unwrap();
        assert_eq!(output.status, Some(3));
        assert_eq!(output.stdout_text(), "out\n");
        assert_eq!(output.stderr_line(), "err");
    }
}
