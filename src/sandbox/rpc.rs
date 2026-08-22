//! TCP RPC protocol for guest-host communication in sandboxed environments.
//!
//! The host-side supervisor runs an RPC server on a random port. The guest
//! workmux binary connects via a host-internal address and sends JSON-lines
//! requests.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::multiplexer::{AgentStatus, Multiplexer};
use crate::sandbox::constant_time::constant_time_eq;

// ── Protocol types ──────────────────────────────────────────────────────

/// RPC request sent from guest to host.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RpcRequest {
    SetStatus {
        status: String,
    },
    SetTitle {
        title: String,
    },
    Heartbeat,
    SpawnAgent {
        prompt: String,
        branch_name: Option<String>,
        background: Option<bool>,
    },
    Exec {
        command: String,
        args: Vec<String>,
    },
    Merge {
        name: String,
        into: Option<String>,
        rebase: bool,
        squash: bool,
        ignore_uncommitted: bool,
        keep: bool,
        #[serde(default)]
        cleanup: bool,
        no_verify: bool,
        no_hooks: bool,
        notification: bool,
    },
    Close {
        name: String,
    },
    ClipboardRead {
        mime: String,
    },
}

/// RPC response sent from host to guest.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RpcResponse {
    Ok,
    Error { message: String },
    Output { message: String },
    ExecOutput { data: String },
    ExecError { data: String },
    ExecExit { code: i32 },
    ClipboardData { path: String },
}

// ── Server ──────────────────────────────────────────────────────────────

/// Context available to RPC request handlers.
pub struct RpcContext {
    /// The tmux/wezterm pane ID of the supervisor pane.
    pub pane_id: String,
    /// Path to the worktree being supervised.
    pub worktree_path: PathBuf,
    /// Multiplexer backend (resolved once at startup).
    pub mux: Arc<dyn Multiplexer>,
    /// Effective configuration captured before the guest starts.
    pub config: Arc<Config>,
    /// Host-only snapshot used by Workmux child commands.
    pub frozen_config_path: PathBuf,
    /// Shared secret for authenticating RPC requests.
    pub token: String,
    /// Commands allowed for host-exec.
    pub allowed_commands: std::collections::HashSet<String>,
    /// Resolved toolchain for host-exec command wrapping.
    pub detected_toolchain: crate::sandbox::toolchain::DetectedToolchain,
    /// Whether to allow host-exec without bwrap on Linux.
    pub allow_unsandboxed_host_exec: bool,
}

/// TCP RPC server that accepts guest connections.
pub struct RpcServer {
    listener: TcpListener,
    port: u16,
}

impl RpcServer {
    /// Bind to a random port on all interfaces.
    ///
    /// Must bind to `0.0.0.0` (not `127.0.0.1`) because the Lima VM connects
    /// via `host.lima.internal`, which resolves to the host's gateway IP on
    /// the shared network interface, not the loopback address.
    pub fn bind() -> Result<Self> {
        let listener = TcpListener::bind("0.0.0.0:0").context("Failed to bind RPC listener")?;
        let port = listener.local_addr()?.port();
        info!(port, "RPC server bound");
        Ok(Self { listener, port })
    }

    /// Get the port the server is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Spawn a background thread that accepts connections and dispatches handlers.
    pub fn spawn(self, ctx: Arc<RpcContext>) -> thread::JoinHandle<()> {
        /// Max concurrent RPC connections. One sandbox session typically uses a
        /// single connection, so 16 is generous while still preventing thread
        /// exhaustion from malicious connection floods.
        const MAX_CONNECTIONS: usize = 16;

        let active = Arc::new(AtomicUsize::new(0));
        thread::spawn(move || {
            for stream in self.listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let current = active.load(Ordering::Relaxed);
                        if current >= MAX_CONNECTIONS {
                            warn!(current, "RPC connection limit reached, dropping");
                            drop(stream);
                            continue;
                        }
                        active.fetch_add(1, Ordering::Relaxed);
                        let ctx = Arc::clone(&ctx);
                        let active = Arc::clone(&active);
                        thread::spawn(move || {
                            if let Err(e) = handle_connection(stream, &ctx) {
                                debug!(error = %e, "RPC connection ended");
                            }
                            active.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Err(e) => {
                        debug!(error = %e, "RPC accept error, shutting down");
                        break;
                    }
                }
            }
        })
    }
}

/// Generate a random token for RPC authentication.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("failed to get random bytes");
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// ── Connection handler ──────────────────────────────────────────────────

/// Maximum size of a single RPC request line (1 MB).
/// Prevents memory exhaustion from a malicious guest sending unbounded data.
const MAX_REQUEST_LINE: usize = 1024 * 1024;

/// Header line sent by client before requests. Contains the auth token.
#[derive(Debug, Serialize, Deserialize)]
struct AuthHeader {
    token: String,
}

/// Read a single line from a buffered reader, enforcing a size limit.
/// Returns `Ok(None)` on EOF, `Err` if the line exceeds the limit.
///
/// Accumulates raw bytes first, then validates UTF-8 once the line is
/// complete. This avoids false rejections when multi-byte UTF-8 characters
/// are split across buffer boundaries.
fn read_bounded_line(reader: &mut impl BufRead, buf: &mut String) -> Result<Option<()>> {
    buf.clear();
    let mut bytes = Vec::new();
    let mut total = 0usize;

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if total == 0 {
                return Ok(None);
            }
            break;
        }

        let (take, done) = match available.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (available.len(), false),
        };

        total += take;
        if total > MAX_REQUEST_LINE {
            anyhow::bail!("RPC request line exceeds {} byte limit", MAX_REQUEST_LINE);
        }

        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);

        if done {
            break;
        }
    }

    let s = std::str::from_utf8(&bytes).context("Invalid UTF-8 in RPC request")?;
    buf.push_str(s);
    Ok(Some(()))
}

fn handle_connection(stream: TcpStream, ctx: &RpcContext) -> Result<()> {
    let peer = stream.peer_addr().ok();
    debug!(?peer, "RPC connection accepted");

    // Require auth header within 5 seconds to prevent slowloris-style DoS.
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;

    let mut reader = BufReader::new(&stream);
    let mut writer = stream.try_clone().context("Failed to clone TCP stream")?;

    // First line must be auth header (bounded read)
    let mut auth_line = String::new();
    match read_bounded_line(&mut reader, &mut auth_line)? {
        Some(()) => {}
        None => return Ok(()),
    }
    let auth: AuthHeader =
        serde_json::from_str(auth_line.trim()).context("Failed to parse auth header")?;

    if !constant_time_eq(auth.token.as_bytes(), ctx.token.as_bytes()) {
        let resp = RpcResponse::Error {
            message: "Invalid token".to_string(),
        };
        write_response(&mut writer, &resp)?;
        return Ok(());
    }

    // Clear timeout for authenticated connections so long-running requests
    // (e.g., Exec streaming) are not interrupted.
    stream.set_read_timeout(None)?;

    // Process request lines (bounded reads)
    let mut line = String::new();
    loop {
        match read_bounded_line(&mut reader, &mut line)? {
            Some(()) => {}
            None => break,
        }

        if line.trim().is_empty() {
            continue;
        }

        let request: RpcRequest = serde_json::from_str(line.trim())
            .with_context(|| format!("Failed to parse RPC request: {}", line.trim()))?;

        info!(?request, "RPC request received");

        // Exec and Merge require streaming multiple responses, handle separately
        if let RpcRequest::Exec {
            ref command,
            ref args,
        } = request
        {
            handle_exec(command, args, ctx, &mut writer)?;
            continue;
        }

        if let RpcRequest::Merge {
            ref name,
            ref into,
            rebase,
            squash,
            ignore_uncommitted,
            keep,
            cleanup,
            no_verify: _,
            no_hooks: _,
            notification,
        } = request
        {
            // SECURITY: Force --no-verify --no-hooks regardless of guest request.
            // Workmux hooks are user-configured shell commands that run unsandboxed
            // on the host. Git native hooks are also disabled in handle_merge via
            // core.hooksPath=/dev/null to prevent a compromised guest from planting
            // hooks in the bind-mounted .git/hooks/ directory.
            handle_merge(
                name,
                into.as_deref(),
                rebase,
                squash,
                ignore_uncommitted,
                keep,
                cleanup,
                notification,
                ctx,
                &mut writer,
            )?;
            continue;
        }

        if let RpcRequest::Close { ref name } = request {
            handle_close(name, ctx, &mut writer)?;
            continue;
        }

        let response = dispatch_request(&request, ctx);
        debug!(?response, "RPC response");

        write_response(&mut writer, &response)?;
    }

    Ok(())
}

fn write_response(writer: &mut impl Write, response: &RpcResponse) -> Result<()> {
    let mut json = serde_json::to_string(response)?;
    json.push('\n');
    writer.write_all(json.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn stream_child_output<Out, ErrFn>(
    child: &mut std::process::Child,
    map_stdout: Out,
    map_stderr: ErrFn,
    writer: &mut impl Write,
) -> Result<std::process::ExitStatus>
where
    Out: Fn(String) -> RpcResponse + Send + 'static,
    ErrFn: Fn(String) -> RpcResponse + Send + 'static,
{
    use std::io::Read;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let (tx, rx) = std::sync::mpsc::channel::<RpcResponse>();

    let tx_out = tx.clone();
    let stdout_thread = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut reader = std::io::BufReader::new(stdout);
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let _ = tx_out.send(map_stdout(data));
                }
                Err(_) => break,
            }
        }
    });

    let tx_err = tx.clone();
    let stderr_thread = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut reader = std::io::BufReader::new(stderr);
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let _ = tx_err.send(map_stderr(data));
                }
                Err(_) => break,
            }
        }
    });

    drop(tx);

    let stream_result = (|| -> Result<()> {
        for response in rx {
            write_response(writer, &response)?;
        }
        Ok(())
    })();

    if let Err(e) = stream_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }

    stdout_thread.join().ok();
    stderr_thread.join().ok();

    child.wait().context("failed to wait on child")
}

// ── Request dispatch ────────────────────────────────────────────────────

fn dispatch_request(request: &RpcRequest, ctx: &RpcContext) -> RpcResponse {
    match request {
        RpcRequest::Heartbeat => RpcResponse::Ok,
        RpcRequest::SetStatus { status } => handle_set_status(status, ctx),
        RpcRequest::SetTitle { title } => handle_set_title(title, ctx),
        RpcRequest::SpawnAgent {
            prompt,
            branch_name,
            background,
        } => handle_spawn_agent(prompt, branch_name.as_deref(), *background, ctx),
        RpcRequest::ClipboardRead { mime } => handle_clipboard_read(mime, &ctx.worktree_path),
        RpcRequest::Exec { .. } => {
            // Handled in handle_connection before dispatch
            unreachable!("Exec is handled directly in handle_connection")
        }
        RpcRequest::Merge { .. } => {
            // Handled in handle_connection before dispatch (needs streaming)
            unreachable!("Merge is handled directly in handle_connection")
        }
        RpcRequest::Close { .. } => {
            // Handled in handle_connection before dispatch
            unreachable!("Close is handled directly in handle_connection")
        }
    }
}

// ── Handlers ────────────────────────────────────────────────────────────

fn handle_set_status(status: &str, ctx: &RpcContext) -> RpcResponse {
    let config = &ctx.config;

    let (agent_status, icon, auto_clear) = match status.to_lowercase().as_str() {
        "working" => (
            Some(AgentStatus::Working),
            config.status_icons.working().to_string(),
            false,
        ),
        "waiting" => (
            Some(AgentStatus::Waiting),
            config.status_icons.waiting().to_string(),
            true,
        ),
        "done" => (
            Some(AgentStatus::Done),
            config.status_icons.done().to_string(),
            true,
        ),
        "clear" => {
            if let Err(e) = ctx.mux.clear_status(&ctx.pane_id) {
                return RpcResponse::Error {
                    message: format!("Failed to clear status: {}", e),
                };
            }
            return RpcResponse::Ok;
        }
        _ => {
            return RpcResponse::Error {
                message: format!("Unknown status: {}", status),
            };
        }
    };

    if config.status_format.unwrap_or(true) {
        let _ = ctx.mux.ensure_status_format(&ctx.pane_id);
    }

    match ctx.mux.set_status(&ctx.pane_id, &icon, auto_clear) {
        Ok(()) => {
            // Persist agent state to StateStore so the dashboard sees this agent
            if let Some(agent_status) = agent_status {
                crate::state::persist_agent_update(
                    &*ctx.mux,
                    &ctx.pane_id,
                    Some(agent_status),
                    None,
                    None,
                );
            }
            RpcResponse::Ok
        }
        Err(e) => RpcResponse::Error {
            message: format!("Failed to set status: {}", e),
        },
    }
}

fn handle_set_title(title: &str, ctx: &RpcContext) -> RpcResponse {
    // Use tmux rename-window via the Cmd helper (consistent with codebase patterns)
    use crate::cmd::Cmd;

    match Cmd::new("tmux")
        .args(&["rename-window", "-t", &ctx.pane_id, title])
        .run()
    {
        Ok(_) => {
            // Persist title to StateStore so the dashboard sees it
            crate::state::persist_agent_update(
                &*ctx.mux,
                &ctx.pane_id,
                None,
                Some(title.to_string()),
                None,
            );
            RpcResponse::Ok
        }
        Err(e) => RpcResponse::Error {
            message: format!("Failed to set title: {}", e),
        },
    }
}

/// Disable git native hooks for a spawned command and all its children.
///
/// Sets `core.hooksPath=/dev/null` via git's `GIT_CONFIG_*` environment
/// variables. This prevents git from executing hooks (post-merge, post-rewrite,
/// post-checkout, etc.) that a sandboxed guest could have planted in the
/// bind-mounted `.git/hooks/` directory.
///
/// Intentionally hard-sets `GIT_CONFIG_COUNT=1` rather than appending to any
/// existing value. RPC-spawned commands should run in a minimal, known-safe
/// configuration -- inheriting arbitrary env-injected git config from the host
/// is not needed and could weaken the security boundary.
fn disable_git_hooks(cmd: &mut std::process::Command) {
    cmd.env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null");
}

fn handle_clipboard_read(mime: &str, worktree_path: &std::path::Path) -> RpcResponse {
    if mime != "image/png" {
        return RpcResponse::Error {
            message: format!("unsupported clipboard MIME type: {}", mime),
        };
    }

    match crate::sandbox::clipboard::materialize_clipboard_png(worktree_path) {
        Ok(Some(path)) => RpcResponse::ClipboardData {
            path: path.to_string_lossy().into_owned(),
        },
        Ok(None) => RpcResponse::Error {
            message: "no image in clipboard".to_string(),
        },
        Err(e) => RpcResponse::Error {
            message: format!("clipboard read failed: {}", e),
        },
    }
}

fn host_workmux_command(ctx: &RpcContext) -> std::process::Command {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("workmux"));
    let mut cmd = std::process::Command::new(exe);
    cmd.env_remove("WM_SANDBOX_GUEST")
        .env_remove("WM_RPC_HOST")
        .env_remove("WM_RPC_PORT")
        .env_remove("WM_RPC_TOKEN")
        .env(
            crate::frozen_config::FROZEN_CONFIG_ENV,
            &ctx.frozen_config_path,
        );

    cmd
}

fn spawn_agent_command(
    prompt: &str,
    branch_name: Option<&str>,
    background: Option<bool>,
    ctx: &RpcContext,
) -> std::process::Command {
    let mut cmd = host_workmux_command(ctx);
    cmd.arg("add");

    if branch_name.is_none() {
        cmd.arg("--auto-name");
    }
    if !prompt.is_empty() {
        cmd.args(["--prompt", prompt]);
    }
    if background.unwrap_or(false) {
        cmd.arg("--background");
    }

    // Workmux hooks can execute host commands. Git hook suppression keeps RPC
    // children independent of repository Git policy across sandbox backends.
    cmd.arg("--no-hooks");
    disable_git_hooks(&mut cmd);

    if let Some(name) = branch_name {
        cmd.arg("--").arg(name);
    }
    cmd.current_dir(&ctx.worktree_path);
    cmd
}

fn handle_spawn_agent(
    prompt: &str,
    branch_name: Option<&str>,
    background: Option<bool>,
    ctx: &RpcContext,
) -> RpcResponse {
    let mut cmd = spawn_agent_command(prompt, branch_name, background, ctx);

    match cmd.output() {
        Ok(output) if output.status.success() => RpcResponse::Ok,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            RpcResponse::Error {
                message: format!("workmux add failed: {}", stderr.trim()),
            }
        }
        Err(e) => RpcResponse::Error {
            message: format!("Failed to run workmux add: {}", e),
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn merge_command(
    name: &str,
    into: Option<&str>,
    rebase: bool,
    squash: bool,
    ignore_uncommitted: bool,
    keep: bool,
    cleanup: bool,
    notification: bool,
    ctx: &RpcContext,
) -> std::process::Command {
    let mut cmd = host_workmux_command(ctx);
    cmd.arg("merge");

    if let Some(target) = into {
        cmd.args(["--into", target]);
    }
    if rebase {
        cmd.arg("--rebase");
    }
    if squash {
        cmd.arg("--squash");
    }
    if ignore_uncommitted {
        cmd.arg("--ignore-uncommitted");
    }
    if keep {
        cmd.arg("--keep");
    }
    if cleanup {
        cmd.arg("--cleanup");
    }
    if notification {
        cmd.arg("--notification");
    }

    // Workmux hooks can execute host commands. Git hook suppression keeps RPC
    // children independent of repository Git policy across sandbox backends.
    cmd.args(["--no-verify", "--no-hooks"]);
    disable_git_hooks(&mut cmd);
    cmd.arg("--").arg(name);
    cmd.current_dir(&ctx.worktree_path);
    cmd
}

#[allow(clippy::too_many_arguments)]
fn handle_merge(
    name: &str,
    into: Option<&str>,
    rebase: bool,
    squash: bool,
    ignore_uncommitted: bool,
    keep: bool,
    cleanup: bool,
    notification: bool,
    ctx: &RpcContext,
    writer: &mut impl Write,
) -> Result<()> {
    use std::process::Stdio;

    let mut cmd = merge_command(
        name,
        into,
        rebase,
        squash,
        ignore_uncommitted,
        keep,
        cleanup,
        notification,
        ctx,
    );
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            write_response(
                writer,
                &RpcResponse::Error {
                    message: format!("Failed to run workmux merge: {}", e),
                },
            )?;
            return Ok(());
        }
    };

    let status = stream_child_output(
        &mut child,
        |message| RpcResponse::Output { message },
        |message| RpcResponse::Output { message },
        writer,
    )?;
    if status.success() {
        write_response(writer, &RpcResponse::Ok)?;
    } else {
        write_response(
            writer,
            &RpcResponse::Error {
                message: format!(
                    "workmux merge exited with code {}",
                    status.code().unwrap_or(1)
                ),
            },
        )?;
    }

    Ok(())
}

/// Environment variables allowed to pass through to host-exec child processes.
/// Everything else is cleared to prevent leaking host secrets.
const EXEC_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "TERM",
    "COLORTERM",
    "LANG",
    "LC_ALL",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
];

/// Build a sanitized environment map from the current process environment.
/// Only variables in the allowlist are included. PATH is normalized to
/// contain only absolute entries (prevents relative-path hijacking from
/// the worktree's current directory).
fn sanitized_env() -> std::collections::HashMap<String, String> {
    let mut envs = std::collections::HashMap::new();
    for key in EXEC_ENV_ALLOWLIST {
        if let Ok(val) = std::env::var(key) {
            if *key == "PATH" {
                // Strip relative/empty PATH entries to prevent hijacking
                let normalized: String = val
                    .split(':')
                    .filter(|p| p.starts_with('/'))
                    .collect::<Vec<_>>()
                    .join(":");
                envs.insert(key.to_string(), normalized);
            } else {
                envs.insert(key.to_string(), val);
            }
        }
    }
    envs
}

fn close_command(name: &str, ctx: &RpcContext) -> std::process::Command {
    let mut cmd = host_workmux_command(ctx);
    cmd.arg("close").arg("--").arg(name);
    cmd.current_dir(&ctx.worktree_path);
    cmd
}

fn handle_close(name: &str, ctx: &RpcContext, writer: &mut impl Write) -> Result<()> {
    use std::io::Read;
    use std::process::Stdio;

    let mut cmd = close_command(name, ctx);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            write_response(
                writer,
                &RpcResponse::Error {
                    message: format!("Failed to run workmux close: {}", e),
                },
            )?;
            return Ok(());
        }
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let stdout_thread = thread::spawn(move || {
        let mut output = String::new();
        let mut reader = std::io::BufReader::new(stdout);
        let _ = reader.read_to_string(&mut output);
        output
    });

    let stderr_thread = thread::spawn(move || {
        let mut output = String::new();
        let mut reader = std::io::BufReader::new(stderr);
        let _ = reader.read_to_string(&mut output);
        output
    });

    let status = child.wait()?;
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    if status.success() {
        if !stdout.is_empty() {
            write_response(writer, &RpcResponse::Output { message: stdout })?;
        }
        write_response(writer, &RpcResponse::Ok)?;
    } else {
        let message = if stderr.trim().is_empty() {
            format!(
                "workmux close exited with code {}",
                status.code().unwrap_or(1)
            )
        } else {
            stderr.trim().to_string()
        };
        write_response(writer, &RpcResponse::Error { message })?;
    }

    Ok(())
}

fn handle_exec(
    command: &str,
    args: &[String],
    ctx: &RpcContext,
    writer: &mut impl Write,
) -> Result<()> {
    info!(command, ?args, "host-exec request");

    // Validate command name format (strict alphanumeric + dash/underscore/dot)
    if !crate::sandbox::shims::validate_command_name(command) {
        let resp = RpcResponse::ExecExit { code: 127 };
        write_response(writer, &resp)?;
        return Ok(());
    }

    // Validate command is in allowlist
    if !ctx.allowed_commands.contains(command) {
        let resp = RpcResponse::ExecExit { code: 127 };
        write_response(writer, &resp)?;
        return Ok(());
    }

    // Skip toolchain wrapping for built-in host commands (e.g., afplay) since they
    // exist outside the project's devbox/nix environment
    let is_builtin = crate::sandbox::shims::BUILTIN_HOST_COMMANDS.contains(&command);
    let wrapper_script = if !is_builtin {
        crate::sandbox::toolchain::toolchain_wrapper_script(&ctx.detected_toolchain)
    } else {
        None
    };

    // Build the logical command (program + args), then delegate to sandbox
    let (program, final_args) = if let Some(script) = wrapper_script {
        // Safe toolchain wrapping: command and args are passed as positional
        // parameters to bash, never interpolated into the shell string.
        // bash -c '<script>' -- <command> <arg1> <arg2> ...
        let mut script_args = vec![
            "-c".to_string(),
            script,
            "--".to_string(),
            command.to_string(),
        ];
        script_args.extend_from_slice(args);
        ("bash".to_string(), script_args)
    } else {
        // Direct execution: no shell involved, args passed as argv
        (command.to_string(), args.to_vec())
    };

    let envs = sanitized_env();
    let spawn_result = crate::sandbox::host_exec_sandbox::spawn_sandboxed(
        &program,
        &final_args,
        &ctx.worktree_path,
        &envs,
        ctx.allow_unsandboxed_host_exec,
    );

    let mut child = match spawn_result {
        Ok(child) => child,
        Err(e) => {
            warn!(command, error = %e, "failed to spawn command");
            write_response(
                writer,
                &RpcResponse::ExecError {
                    data: format!("host-exec spawn failed: {e}\n"),
                },
            )?;
            write_response(writer, &RpcResponse::ExecExit { code: 126 })?;
            return Ok(());
        }
    };

    let status = stream_child_output(
        &mut child,
        |data| RpcResponse::ExecOutput { data },
        |data| RpcResponse::ExecError { data },
        writer,
    )?;
    let code = status.code().unwrap_or(1);
    info!(command, code, "host-exec finished");

    write_response(writer, &RpcResponse::ExecExit { code })?;
    Ok(())
}

// ── Client ──────────────────────────────────────────────────────────────

/// RPC client for guest-side use. Connects to the host supervisor.
///
/// Used by the guest workmux binary to send requests to the host supervisor
/// when `WM_SANDBOX_GUEST=1` is set. Commands like `set-window-status` route
/// through RPC instead of calling tmux directly.
pub struct RpcClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl RpcClient {
    /// Connect using WM_RPC_HOST, WM_RPC_PORT, and WM_RPC_TOKEN env vars.
    pub fn from_env() -> Result<Self> {
        let host = std::env::var("WM_RPC_HOST").context("WM_RPC_HOST not set")?;
        let port: u16 = std::env::var("WM_RPC_PORT")
            .context("WM_RPC_PORT not set")?
            .parse()
            .context("WM_RPC_PORT is not a valid port")?;
        let token = std::env::var("WM_RPC_TOKEN").context("WM_RPC_TOKEN not set")?;

        Self::connect(&host, port, &token)
    }

    /// Connect to a specific host, port, and authenticate with token.
    pub fn connect(host: &str, port: u16, token: &str) -> Result<Self> {
        let stream = TcpStream::connect(format!("{}:{}", host, port))
            .with_context(|| format!("Failed to connect to RPC server at {}:{}", host, port))?;

        let writer = stream.try_clone().context("Failed to clone TCP stream")?;
        let reader = BufReader::new(stream);

        // Send auth header
        let auth = AuthHeader {
            token: token.to_string(),
        };
        let mut auth_json = serde_json::to_string(&auth)?;
        auth_json.push('\n');
        let writer_ref = &writer;
        (&*writer_ref).write_all(auth_json.as_bytes())?;
        (&*writer_ref).flush()?;

        Ok(Self { reader, writer })
    }

    /// Send a request and receive a response.
    pub fn call(&mut self, request: &RpcRequest) -> Result<RpcResponse> {
        self.send(request)?;
        self.recv()
    }

    /// Send a request without waiting for a response.
    pub fn send(&mut self, request: &RpcRequest) -> Result<()> {
        let mut req_json = serde_json::to_string(request)?;
        req_json.push('\n');
        (&self.writer).write_all(req_json.as_bytes())?;
        (&self.writer).flush()?;
        Ok(())
    }

    /// Receive a single response line.
    pub fn recv(&mut self) -> Result<RpcResponse> {
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        serde_json::from_str(&line)
            .with_context(|| format!("Failed to parse RPC response: {}", line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer;

    fn test_context(worktree_path: &std::path::Path) -> RpcContext {
        RpcContext {
            pane_id: "%0".to_string(),
            worktree_path: worktree_path.to_path_buf(),
            mux: multiplexer::create_backend(multiplexer::BackendType::Tmux),
            config: Arc::new(Config::default()),
            frozen_config_path: worktree_path.join("frozen-config.json"),
            token: "test-token".to_string(),
            allowed_commands: std::collections::HashSet::new(),
            detected_toolchain: crate::sandbox::toolchain::DetectedToolchain::None,
            allow_unsandboxed_host_exec: false,
        }
    }

    fn command_args(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn test_request_serialization_heartbeat() {
        let req = RpcRequest::Heartbeat;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"Heartbeat\""));
    }

    #[test]
    fn test_request_serialization_set_status() {
        let req = RpcRequest::SetStatus {
            status: "working".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"SetStatus\""));
        assert!(json.contains("\"status\":\"working\""));
    }

    #[test]
    fn test_request_serialization_spawn_agent() {
        let req = RpcRequest::SpawnAgent {
            prompt: "fix the bug".to_string(),
            branch_name: Some("fix-bug".to_string()),
            background: Some(true),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            RpcRequest::SpawnAgent {
                prompt,
                branch_name,
                background,
            } => {
                assert_eq!(prompt, "fix the bug");
                assert_eq!(branch_name.as_deref(), Some("fix-bug"));
                assert_eq!(background, Some(true));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_response_serialization() {
        let resp = RpcResponse::Ok;
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"Ok\""));

        let resp = RpcResponse::Error {
            message: "oops".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"Error\""));
        assert!(json.contains("\"message\":\"oops\""));
    }

    #[test]
    fn test_request_roundtrip_deserialization() {
        let cases = vec![
            r#"{"type":"Heartbeat"}"#,
            r#"{"type":"SetStatus","status":"working"}"#,
            r#"{"type":"SetTitle","title":"my agent"}"#,
            r#"{"type":"SpawnAgent","prompt":"do stuff","branch_name":null,"background":null}"#,
            r#"{"type":"Exec","command":"cargo","args":["build","--release"]}"#,
            r#"{"type":"Merge","name":"feat","into":null,"rebase":true,"squash":false,"ignore_uncommitted":false,"keep":false,"cleanup":false,"no_verify":false,"no_hooks":false,"notification":false}"#,
            r#"{"type":"Close","name":"feature-x"}"#,
            r#"{"type":"ClipboardRead","mime":"image/png"}"#,
        ];
        for json in cases {
            let req: RpcRequest = serde_json::from_str(json).unwrap();
            // Verify it round-trips
            let re_json = serde_json::to_string(&req).unwrap();
            let _: RpcRequest = serde_json::from_str(&re_json).unwrap();
        }
    }

    #[test]
    fn test_generate_token_is_nonempty() {
        let token = generate_token();
        assert!(!token.is_empty());
        assert_eq!(token.len(), 64, "token should be 64 hex chars (32 bytes)");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_server_bind_assigns_port() {
        let server = RpcServer::bind().unwrap();
        assert!(server.port() > 0);
    }

    #[test]
    fn test_client_server_heartbeat_roundtrip() {
        let (mut client, _port, _token) = setup_test_server();
        let resp = client.call(&RpcRequest::Heartbeat).unwrap();
        match resp {
            RpcResponse::Ok => {}
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_request_serialization_exec() {
        let req = RpcRequest::Exec {
            command: "just".to_string(),
            args: vec!["check".to_string()],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"Exec\""));
        assert!(json.contains("\"command\":\"just\""));

        let parsed: RpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            RpcRequest::Exec { command, args } => {
                assert_eq!(command, "just");
                assert_eq!(args, vec!["check"]);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_response_serialization_exec_output() {
        let resp = RpcResponse::ExecOutput {
            data: "hello\n".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"ExecOutput\""));
        assert!(json.contains("\"data\":\"hello\\n\""));
    }

    #[test]
    fn test_response_serialization_exec_exit() {
        let resp = RpcResponse::ExecExit { code: 42 };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"ExecExit\""));
        assert!(json.contains("\"code\":42"));

        let parsed: RpcResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            RpcResponse::ExecExit { code } => assert_eq!(code, 42),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_read_bounded_line_normal() {
        let data = b"hello world\nsecond line\n";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut buf = String::new();

        let result = read_bounded_line(&mut reader, &mut buf).unwrap();
        assert!(result.is_some());
        assert_eq!(buf, "hello world\n");

        let result = read_bounded_line(&mut reader, &mut buf).unwrap();
        assert!(result.is_some());
        assert_eq!(buf, "second line\n");

        let result = read_bounded_line(&mut reader, &mut buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_sanitized_env_normalizes_path() {
        // Test the normalization logic directly without modifying env
        // (set_var is unsafe in Rust 2024 edition due to thread safety)
        let envs = sanitized_env();
        if let Some(path) = envs.get("PATH") {
            for entry in path.split(':') {
                assert!(
                    entry.starts_with('/'),
                    "PATH should only have absolute entries, found: '{}'",
                    entry
                );
            }
        }
    }

    #[test]
    fn test_sanitized_env_excludes_secrets() {
        let envs = sanitized_env();
        // Common secret env vars should never appear
        assert!(!envs.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!envs.contains_key("GITHUB_TOKEN"));
        assert!(!envs.contains_key("WM_RPC_TOKEN"));
        // Only allowlisted keys should be present
        for key in envs.keys() {
            assert!(
                EXEC_ENV_ALLOWLIST.contains(&key.as_str()),
                "unexpected env key in sanitized env: {}",
                key
            );
        }
    }

    #[test]
    fn test_read_bounded_line_rejects_oversized() {
        // Create a line that exceeds MAX_REQUEST_LINE
        let huge = "x".repeat(MAX_REQUEST_LINE + 1);
        let data = format!("{}\n", huge);
        let mut reader = std::io::BufReader::new(data.as_bytes());
        let mut buf = String::new();

        let result = read_bounded_line(&mut reader, &mut buf);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds"));
    }

    #[test]
    fn test_client_server_invalid_token() {
        let (_client, port, _token) = setup_test_server();
        let mut client = RpcClient::connect("127.0.0.1", port, "wrong-token").unwrap();
        let resp = client.call(&RpcRequest::Heartbeat).unwrap();
        match resp {
            RpcResponse::Error { message } => assert!(message.contains("Invalid token")),
            other => panic!("Expected Error, got {:?}", other),
        }
    }

    // ── Host-exec integration tests ─────────────────────────────────────

    /// Start an RPC server with the given allowed commands and return a
    /// connected client. Uses a temp dir as the worktree path.
    ///
    /// When `allow_unsandboxed` is true, commands run without OS sandbox
    /// (no bwrap/sandbox-exec). Use this for tests that verify RPC pipeline
    /// behavior (streaming, exit codes, env sanitization) rather than
    /// sandbox enforcement.
    fn start_exec_server(
        allowed: &[&str],
        allow_unsandboxed: bool,
    ) -> (RpcClient, tempfile::TempDir, thread::JoinHandle<()>) {
        let server = RpcServer::bind().unwrap();
        let port = server.port();
        let token = generate_token();
        let tmp = tempfile::tempdir().unwrap();

        let mux = multiplexer::create_backend(multiplexer::BackendType::Tmux);
        let ctx = Arc::new(RpcContext {
            pane_id: "%0".to_string(),
            worktree_path: tmp.path().to_path_buf(),
            mux,
            config: Arc::new(Config::default()),
            frozen_config_path: tmp.path().join("frozen-config.json"),
            token: token.clone(),
            allowed_commands: allowed.iter().map(|s| s.to_string()).collect(),
            detected_toolchain: crate::sandbox::toolchain::DetectedToolchain::None,
            allow_unsandboxed_host_exec: allow_unsandboxed,
        });

        let handle = server.spawn(ctx);
        std::thread::sleep(std::time::Duration::from_millis(50));

        let client = RpcClient::connect("127.0.0.1", port, &token).unwrap();
        (client, tmp, handle)
    }

    /// Send an exec request and collect all streaming responses into
    /// (stdout, stderr, exit_code).
    fn exec_collect(client: &mut RpcClient, command: &str, args: &[&str]) -> (String, String, i32) {
        client
            .send(&RpcRequest::Exec {
                command: command.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
            })
            .unwrap();

        let mut stdout = String::new();
        let mut stderr = String::new();
        loop {
            match client.recv().unwrap() {
                RpcResponse::ExecOutput { data } => stdout.push_str(&data),
                RpcResponse::ExecError { data } => stderr.push_str(&data),
                RpcResponse::ExecExit { code } => return (stdout, stderr, code),
                other => panic!("Unexpected response: {:?}", other),
            }
        }
    }

    #[test]
    fn test_exec_allowed_command() {
        let (mut client, _tmp, _handle) = start_exec_server(&["echo"], true);
        let (stdout, _stderr, code) = exec_collect(&mut client, "echo", &["hello", "world"]);
        assert_eq!(code, 0);
        assert_eq!(stdout.trim(), "hello world");
    }

    #[test]
    fn test_exec_disallowed_command() {
        let (mut client, _tmp, _handle) = start_exec_server(&["echo"], true);
        let (_stdout, _stderr, code) = exec_collect(&mut client, "ls", &[]);
        assert_eq!(code, 127, "disallowed command should return 127");
    }

    #[test]
    fn test_exec_invalid_command_name() {
        let (mut client, _tmp, _handle) = start_exec_server(&["echo"], true);

        // Shell metacharacters in command name
        let (_stdout, _stderr, code) = exec_collect(&mut client, "echo;whoami", &[]);
        assert_eq!(code, 127);

        // Path traversal
        let (_stdout, _stderr, code) = exec_collect(&mut client, "/bin/echo", &[]);
        assert_eq!(code, 127);
    }

    #[test]
    fn test_exec_shell_metacharacters_in_args_not_interpreted() {
        let (mut client, _tmp, _handle) = start_exec_server(&["echo"], true);

        // $(whoami) should be printed literally, not expanded
        let (stdout, _stderr, code) = exec_collect(&mut client, "echo", &["$(whoami)"]);
        assert_eq!(code, 0);
        assert_eq!(stdout.trim(), "$(whoami)");

        // Backtick substitution should be literal
        let (stdout, _stderr, code) = exec_collect(&mut client, "echo", &["`whoami`"]);
        assert_eq!(code, 0);
        assert_eq!(stdout.trim(), "`whoami`");

        // Semicolons should be literal
        let (stdout, _stderr, code) = exec_collect(&mut client, "echo", &["hello; whoami"]);
        assert_eq!(code, 0);
        assert_eq!(stdout.trim(), "hello; whoami");
    }

    #[test]
    fn test_exec_env_sanitized() {
        let (mut client, _tmp, _handle) = start_exec_server(&["env"], true);
        let (stdout, _stderr, code) = exec_collect(&mut client, "env", &[]);
        assert_eq!(code, 0);

        // The RPC token is in our process env but should NOT leak to child
        let env_lines: Vec<&str> = stdout.lines().collect();
        assert!(
            !env_lines.iter().any(|l| l.starts_with("WM_RPC_TOKEN=")),
            "WM_RPC_TOKEN should not be in child environment"
        );

        // PATH should still be present (it's in the allowlist)
        assert!(
            env_lines.iter().any(|l| l.starts_with("PATH=")),
            "PATH should be in child environment"
        );

        // PATH should contain only absolute entries (normalized)
        let path_line = env_lines.iter().find(|l| l.starts_with("PATH=")).unwrap();
        let path_val = &path_line["PATH=".len()..];
        for entry in path_val.split(':') {
            assert!(
                entry.starts_with('/'),
                "PATH entry should be absolute: {}",
                entry
            );
        }
    }

    #[test]
    fn test_exec_sandbox_blocks_ssh_read() {
        #[cfg(target_os = "linux")]
        {
            // Probe bwrap usability, not just existence. bwrap requires user
            // namespaces which are unavailable inside nested sandboxes (Nix
            // build sandbox, Docker without --privileged, etc.).
            let probe = std::process::Command::new("bwrap")
                .args(["--ro-bind", "/", "/", "--", "true"])
                .status();
            match probe {
                Ok(s) if s.success() => {}
                _ => return, // bwrap not usable in this environment
            }
        }

        let (mut client, _tmp, _handle) = start_exec_server(&["ls"], false);
        let home = std::env::var("HOME").unwrap();
        let ssh_dir = format!("{}/.ssh", home);

        // If ~/.ssh doesn't exist, skip (CI environments)
        if !std::path::Path::new(&ssh_dir).exists() {
            return;
        }

        let (_stdout, _stderr, code) = exec_collect(&mut client, "ls", &[&ssh_dir]);

        #[cfg(target_os = "macos")]
        {
            // macOS sandbox-exec denies the read entirely
            assert_ne!(
                code,
                0,
                "ls ~/.ssh should fail under sandbox-exec (stderr: {})",
                _stderr.trim()
            );
        }

        #[cfg(target_os = "linux")]
        {
            // bwrap masks ~/.ssh with tmpfs so ls succeeds but sees nothing
            assert_eq!(code, 0);
            assert!(
                _stdout.trim().is_empty(),
                "~/.ssh should appear empty under bwrap, got: {}",
                _stdout.trim()
            );
        }
    }

    #[test]
    fn test_exec_nonexistent_command() {
        let (mut client, _tmp, _handle) =
            start_exec_server(&["this-command-definitely-does-not-exist-xyz"], true);
        let (_stdout, _stderr, code) = exec_collect(
            &mut client,
            "this-command-definitely-does-not-exist-xyz",
            &[],
        );
        // Should fail to spawn, not hang
        assert_ne!(code, 0);
    }

    #[test]
    fn test_exec_exit_code_propagated() {
        let (mut client, _tmp, _handle) = start_exec_server(&["sh"], true);
        let (_stdout, _stderr, code) = exec_collect(&mut client, "sh", &["-c", "exit 42"]);
        assert_eq!(code, 42, "exit code should be propagated from child");
    }

    #[test]
    fn test_exec_stderr_captured() {
        let (mut client, _tmp, _handle) = start_exec_server(&["sh"], true);
        let (_stdout, stderr, code) = exec_collect(&mut client, "sh", &["-c", "echo oops >&2"]);
        assert_eq!(code, 0);
        assert_eq!(stderr.trim(), "oops");
    }

    #[test]
    fn test_exec_multiple_commands_on_same_connection() {
        let (mut client, _tmp, _handle) = start_exec_server(&["echo"], true);

        let (stdout1, _, code1) = exec_collect(&mut client, "echo", &["first"]);
        assert_eq!(code1, 0);
        assert_eq!(stdout1.trim(), "first");

        let (stdout2, _, code2) = exec_collect(&mut client, "echo", &["second"]);
        assert_eq!(code2, 0);
        assert_eq!(stdout2.trim(), "second");
    }

    #[test]
    fn test_request_serialization_merge() {
        let req = RpcRequest::Merge {
            name: "feature-x".to_string(),
            into: Some("main".to_string()),
            rebase: true,
            squash: false,
            ignore_uncommitted: false,
            keep: true,
            cleanup: false,
            no_verify: false,
            no_hooks: true,
            notification: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"Merge\""));
        assert!(json.contains("\"name\":\"feature-x\""));
        assert!(json.contains("\"rebase\":true"));
        assert!(json.contains("\"keep\":true"));
        assert!(json.contains("\"cleanup\":false"));
        assert!(json.contains("\"no_hooks\":true"));
        assert!(json.contains("\"notification\":true"));

        // Roundtrip
        let parsed: RpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            RpcRequest::Merge {
                name,
                into,
                rebase,
                squash,
                ignore_uncommitted,
                keep,
                cleanup,
                no_verify,
                no_hooks,
                notification,
            } => {
                assert_eq!(name, "feature-x");
                assert_eq!(into.as_deref(), Some("main"));
                assert!(rebase);
                assert!(!squash);
                assert!(!ignore_uncommitted);
                assert!(keep);
                assert!(!cleanup);
                assert!(!no_verify);
                assert!(no_hooks);
                assert!(notification);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_request_serialization_merge_defaults_cleanup() {
        let json = r#"{"type":"Merge","name":"feature-x","into":null,"rebase":false,"squash":false,"ignore_uncommitted":false,"keep":false,"no_verify":false,"no_hooks":false,"notification":false}"#;
        let parsed: RpcRequest = serde_json::from_str(json).unwrap();
        match parsed {
            RpcRequest::Merge { cleanup, .. } => assert!(!cleanup),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_request_serialization_close() {
        let req = RpcRequest::Close {
            name: "feature-x".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"Close\""));
        assert!(json.contains("\"name\":\"feature-x\""));

        let parsed: RpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            RpcRequest::Close { name } => assert_eq!(name, "feature-x"),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_response_serialization_output() {
        let resp = RpcResponse::Output {
            message: "Merged 'feature' into 'main'".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"Output\""));
        let parsed: RpcResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            RpcResponse::Output { message } => {
                assert_eq!(message, "Merged 'feature' into 'main'");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_spawn_agent_command_omits_empty_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path());
        let cmd = spawn_agent_command("", Some("test-branch"), None, &ctx);
        let args = command_args(&cmd);

        assert!(!args.iter().any(|arg| arg == "--prompt"));
        assert_eq!(
            args,
            ["add", "--no-hooks", "--", "test-branch"]
                .map(str::to_string)
                .to_vec()
        );
    }

    #[test]
    fn test_spawn_agent_command_includes_background_and_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path());
        let cmd = spawn_agent_command("do stuff", Some("bg-branch"), Some(true), &ctx);

        assert_eq!(
            command_args(&cmd),
            [
                "add",
                "--prompt",
                "do stuff",
                "--background",
                "--no-hooks",
                "--",
                "bg-branch",
            ]
            .map(str::to_string)
            .to_vec()
        );
    }

    #[test]
    fn test_spawn_agent_command_uses_auto_name_without_positional() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path());
        let cmd = spawn_agent_command("fix bug", None, None, &ctx);

        assert_eq!(
            command_args(&cmd),
            ["add", "--auto-name", "--prompt", "fix bug", "--no-hooks"]
                .map(str::to_string)
                .to_vec()
        );
    }

    #[test]
    fn test_spawn_agent_command_terminates_options_before_name() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path());
        let cmd = spawn_agent_command("", Some("--config=/tmp/guest.yaml"), None, &ctx);
        let args = command_args(&cmd);

        assert_eq!(args[args.len() - 2..], ["--", "--config=/tmp/guest.yaml"]);
    }

    #[test]
    fn test_spawn_agent_request_serialization_with_background() {
        // Verify the SpawnAgent request serializes background correctly
        let req = RpcRequest::SpawnAgent {
            prompt: "test".to_string(),
            branch_name: None,
            background: Some(true),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            RpcRequest::SpawnAgent { background, .. } => {
                assert_eq!(background, Some(true));
            }
            _ => panic!("Wrong variant"),
        }

        // Test with None background
        let req = RpcRequest::SpawnAgent {
            prompt: "test".to_string(),
            branch_name: Some("branch".to_string()),
            background: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            RpcRequest::SpawnAgent { background, .. } => {
                assert_eq!(background, None);
            }
            _ => panic!("Wrong variant"),
        }
    }

    // ── Git hook suppression tests ──────────────────────────────────────

    #[test]
    fn test_host_workmux_command_sets_host_only_environment() {
        use std::ffi::OsStr;

        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path());
        let cmd = host_workmux_command(&ctx);
        let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();

        assert_eq!(
            envs.get(OsStr::new("WM_SANDBOX_GUEST")),
            Some(&None),
            "host workmux child must not inherit guest mode"
        );
        assert_eq!(
            envs.get(OsStr::new("WM_RPC_HOST")),
            Some(&None),
            "host workmux child must not inherit guest RPC host"
        );
        assert_eq!(
            envs.get(OsStr::new("WM_RPC_PORT")),
            Some(&None),
            "host workmux child must not inherit guest RPC port"
        );
        assert_eq!(
            envs.get(OsStr::new("WM_RPC_TOKEN")),
            Some(&None),
            "host workmux child must not inherit guest RPC token"
        );
        assert_eq!(
            envs.get(OsStr::new(crate::frozen_config::FROZEN_CONFIG_ENV)),
            Some(&Some(ctx.frozen_config_path.as_os_str())),
            "host workmux child must use the supervisor snapshot"
        );
    }

    #[test]
    fn test_disable_git_hooks_sets_env_vars() {
        use std::ffi::OsStr;
        use std::process::Command;

        let mut cmd = Command::new("true");
        disable_git_hooks(&mut cmd);

        let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();

        assert_eq!(
            envs.get(OsStr::new("GIT_CONFIG_COUNT")),
            Some(&Some(OsStr::new("1"))),
            "GIT_CONFIG_COUNT should be set to 1"
        );
        assert_eq!(
            envs.get(OsStr::new("GIT_CONFIG_KEY_0")),
            Some(&Some(OsStr::new("core.hooksPath"))),
            "GIT_CONFIG_KEY_0 should be core.hooksPath"
        );
        assert_eq!(
            envs.get(OsStr::new("GIT_CONFIG_VALUE_0")),
            Some(&Some(OsStr::new("/dev/null"))),
            "GIT_CONFIG_VALUE_0 should be /dev/null"
        );
        // GIT_CONFIG_PARAMETERS must be removed to prevent it from overriding
        // the GIT_CONFIG_COUNT-based settings
        assert_eq!(
            envs.get(OsStr::new("GIT_CONFIG_PARAMETERS")),
            Some(&None),
            "GIT_CONFIG_PARAMETERS should be explicitly removed"
        );
    }

    #[test]
    fn test_merge_command_disables_hooks_and_terminates_options() {
        use std::ffi::OsStr;

        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path());
        let cmd = merge_command(
            "--config=/tmp/guest.yaml",
            Some("main"),
            true,
            false,
            true,
            false,
            true,
            true,
            &ctx,
        );
        let args = command_args(&cmd);
        let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();

        assert!(args.iter().any(|arg| arg == "--no-verify"));
        assert!(args.iter().any(|arg| arg == "--no-hooks"));
        assert_eq!(args[args.len() - 2..], ["--", "--config=/tmp/guest.yaml"]);
        assert_eq!(
            envs.get(OsStr::new("GIT_CONFIG_VALUE_0")),
            Some(&Some(OsStr::new("/dev/null")))
        );
    }

    #[test]
    fn test_spawn_agent_command_disables_git_hooks() {
        use std::ffi::OsStr;

        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path());
        let cmd = spawn_agent_command("", None, None, &ctx);
        let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();

        assert_eq!(
            envs.get(OsStr::new("GIT_CONFIG_KEY_0")),
            Some(&Some(OsStr::new("core.hooksPath")))
        );
    }

    #[test]
    fn test_close_command_terminates_options_before_name() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path());
        let cmd = close_command("--config=/tmp/guest.yaml", &ctx);

        assert_eq!(
            command_args(&cmd),
            ["close", "--", "--config=/tmp/guest.yaml"]
                .map(str::to_string)
                .to_vec()
        );
    }

    #[test]
    fn test_request_serialization_clipboard_read() {
        let req = RpcRequest::ClipboardRead {
            mime: "image/png".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"ClipboardRead\""));
        assert!(json.contains("\"mime\":\"image/png\""));

        let parsed: RpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            RpcRequest::ClipboardRead { mime } => assert_eq!(mime, "image/png"),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_response_serialization_clipboard_data() {
        let resp = RpcResponse::ClipboardData {
            path: "/tmp/test/.workmux/tmp/clipboard-123-456.png".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"ClipboardData\""));
        assert!(json.contains("clipboard-123-456.png"));

        let parsed: RpcResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            RpcResponse::ClipboardData { path } => {
                assert!(path.contains("clipboard-123-456.png"));
            }
            _ => panic!("Wrong variant"),
        }
    }

    /// Bind an RPC server, spawn it with a default RpcContext, and return
    /// a connected client, the port, and the generated token.
    fn setup_test_server() -> (RpcClient, u16, String) {
        let server = RpcServer::bind().unwrap();
        let port = server.port();
        let token = generate_token();

        let mux = multiplexer::create_backend(multiplexer::BackendType::Tmux);
        let ctx = Arc::new(RpcContext {
            pane_id: "%0".to_string(),
            worktree_path: PathBuf::from("/tmp/test"),
            mux,
            config: Arc::new(Config::default()),
            frozen_config_path: PathBuf::from("/tmp/frozen-config.json"),
            token: token.clone(),
            allowed_commands: std::collections::HashSet::new(),
            detected_toolchain: crate::sandbox::toolchain::DetectedToolchain::None,
            allow_unsandboxed_host_exec: false,
        });

        let _handle = server.spawn(ctx);
        // Give server thread a moment to start
        std::thread::sleep(std::time::Duration::from_millis(50));

        let client = RpcClient::connect("127.0.0.1", port, &token).unwrap();
        (client, port, token)
    }
}
