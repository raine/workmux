//! The `workmux sandbox run` supervisor process.
//!
//! Runs inside a tmux pane. Starts a TCP RPC server and executes the agent
//! command inside a sandbox (Lima VM or Docker/Podman container).

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tracing::{debug, info, warn};

use std::collections::HashSet;

use crate::config::{Config, SandboxBackend};
use crate::multiplexer;
use crate::sandbox::build_docker_run_args;
use crate::sandbox::ensure_sandbox_config_dirs;
use crate::sandbox::lima;
use crate::sandbox::network_proxy::NetworkProxy;
use crate::sandbox::rpc::{RpcContext, RpcServer, generate_token};
use crate::sandbox::shims;
use crate::sandbox::toolchain;
use crate::state::StateStore;

/// Guard that stops a container when dropped.
/// Ensures cleanup even if the supervisor is killed or panics.
struct ContainerGuard {
    runtime: &'static str,
    name: String,
    handle: String,
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        debug!(container = %self.name, "stopping container");
        let result = Command::new(self.runtime)
            .args(["stop", "-t", "2", &self.name])
            .output();
        match result {
            Ok(output) if output.status.success() => {
                debug!(container = %self.name, "container stopped");
            }
            Ok(output) => {
                // Container may have already exited, which is fine
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.contains("No such container") {
                    warn!(container = %self.name, stderr = %stderr.trim(), "failed to stop container");
                }
            }
            Err(e) => {
                warn!(container = %self.name, error = %e, "failed to run docker stop");
            }
        }

        // Unregister container from state store
        if let Ok(store) = StateStore::new() {
            store.unregister_container(&self.handle, &self.name);
        }
    }
}

/// Run the sandbox supervisor.
///
/// Detects the sandbox backend from config and dispatches to the
/// appropriate handler (Lima VM or Docker/Podman container).
pub fn run(worktree: PathBuf, worktree_root: Option<PathBuf>, command: Vec<String>) -> Result<i32> {
    if command.is_empty() {
        bail!("No command specified. Usage: workmux sandbox run <worktree> -- <command...>");
    }

    let config = Config::load(None)?;
    let worktree = worktree.canonicalize().unwrap_or_else(|_| worktree.clone());

    match config.sandbox.backend() {
        SandboxBackend::Lima => run_lima(&config, &worktree, &command),
        SandboxBackend::Container => {
            let wt_root = worktree_root
                .map(|p| p.canonicalize().unwrap_or(p))
                .unwrap_or_else(|| worktree.clone());
            run_container(&config, &worktree, &wt_root, &command)
        }
    }
}

/// Start RPC server and return (server, port, token, context).
/// Shared setup between Lima and Container backends.
fn start_rpc(
    worktree: &Path,
    allowed_commands: HashSet<String>,
    detected_toolchain: toolchain::DetectedToolchain,
    allow_unsandboxed_host_exec: bool,
) -> Result<(RpcServer, u16, String, Arc<RpcContext>)> {
    let rpc_server = RpcServer::bind()?;
    let rpc_port = rpc_server.port();
    let rpc_token = generate_token();
    info!(port = rpc_port, "RPC server listening");

    let mux = multiplexer::create_backend(multiplexer::detect_backend());
    let pane_id = mux.current_pane_id().unwrap_or_default();

    let ctx = Arc::new(RpcContext {
        pane_id,
        worktree_path: worktree.to_path_buf(),
        mux,
        token: rpc_token.clone(),
        allowed_commands,
        detected_toolchain,
        allow_unsandboxed_host_exec,
    });

    Ok((rpc_server, rpc_port, rpc_token, ctx))
}

/// Extract git `user.name` and `user.email` from the host's git config and
/// return `GIT_CONFIG_*` environment variable pairs to inject into the sandbox.
///
/// Runs `git config user.name` and `git config user.email` from `worktree_dir`
/// to respect all config scopes (system, global, conditional includes).
/// Returns an empty vec if neither value is configured (graceful no-op).
fn git_user_config_envs(worktree_dir: &Path) -> Vec<(String, String)> {
    let mut entries = Vec::new();

    for key in &["user.name", "user.email"] {
        if let Ok(output) = Command::new("git")
            .args(["config", key])
            .current_dir(worktree_dir)
            .output()
            && output.status.success()
        {
            let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !val.is_empty() {
                entries.push((key.to_string(), val));
            }
        }
    }

    if entries.is_empty() {
        return Vec::new();
    }

    let mut envs = Vec::with_capacity(1 + entries.len() * 2);
    envs.push(("GIT_CONFIG_COUNT".into(), entries.len().to_string()));
    for (i, (key, val)) in entries.iter().enumerate() {
        envs.push((format!("GIT_CONFIG_KEY_{}", i), key.clone()));
        envs.push((format!("GIT_CONFIG_VALUE_{}", i), val.clone()));
    }
    envs
}

fn run_lima(config: &Config, worktree: &Path, command: &[String]) -> Result<i32> {
    info!(worktree = %worktree.display(), "sandbox supervisor starting (lima)");

    // Ensure Lima VM is running
    let vm_name = lima::ensure_vm_running(config, worktree)?;
    info!(vm_name = %vm_name, "Lima VM ready");

    let agent = crate::multiplexer::agent::resolve_profile_with_type(
        config.agent.as_deref(),
        config.agent_type.as_deref(),
    )
    .name();

    if agent == "claude"
        && let Err(e) = lima::mounts::seed_claude_json(&vm_name)
    {
        tracing::warn!(vm_name = %vm_name, error = %e, "failed to seed ~/.claude.json; continuing");
    }

    // Detect toolchain for both agent wrapping and host-exec
    let detected = toolchain::resolve_toolchain(&config.sandbox.toolchain(), worktree);
    if detected != toolchain::DetectedToolchain::None {
        info!(toolchain = ?detected, "wrapping command with toolchain environment");
    }

    // Create shims (built-in commands like afplay, clipboard shims, + user-configured ones)
    let host_commands = shims::effective_host_commands(config.sandbox.host_commands());
    // Clipboard shims use ClipboardRead RPC, not Exec -- exclude from exec allowlist
    let allowed_commands: HashSet<String> = host_commands
        .iter()
        .filter(|cmd| !shims::is_clipboard_shim(cmd))
        .cloned()
        .collect();

    let state_dir = lima::mounts::lima_state_dir_path(&vm_name)?;
    shims::create_shim_directory(&state_dir, &host_commands)?;
    info!(commands = ?host_commands, "created host-exec shims");

    let (rpc_server, rpc_port, rpc_token, ctx) = start_rpc(
        worktree,
        allowed_commands,
        detected.clone(),
        config.sandbox.allow_unsandboxed_host_exec(),
    )?;
    let _rpc_handle = rpc_server.spawn(ctx);

    // Build limactl shell command
    let mut lima_cmd = Command::new("limactl");
    lima_cmd
        .arg("shell")
        .args(["--workdir", &worktree.to_string_lossy()])
        .arg(&vm_name);

    let mut env_exports = vec![
        r#"PATH="$HOME/.workmux-state/shims/bin:$HOME/.local/bin:/nix/var/nix/profiles/default/bin:$PATH""#.to_string(),
        "WM_SANDBOX_GUEST=1".to_string(),
        "WM_RPC_HOST=host.lima.internal".to_string(),
        format!("WM_RPC_PORT={}", rpc_port),
        format!("WM_RPC_TOKEN={}", rpc_token),
    ];

    for term_var in ["TERM", "COLORTERM"] {
        if let Ok(val) = std::env::var(term_var) {
            env_exports.push(format!("{}={}", term_var, val));
        }
    }

    for env_var in config.sandbox.env_passthrough() {
        if let Ok(val) = std::env::var(env_var) {
            env_exports.push(format!("{}={}", env_var, val));
        }
    }

    // Explicit env vars from config
    for (key, val) in config.sandbox.env_vars() {
        env_exports.push(format!("{}='{}'", key, crate::shell::shell_escape(val)));
    }

    // Inject host git user config (user.name, user.email) for commits
    for (key, val) in git_user_config_envs(worktree) {
        env_exports.push(format!("{}='{}'", key, crate::shell::shell_escape(&val)));
    }

    let exports: String = env_exports
        .iter()
        .map(|e| format!("export {e}"))
        .collect::<Vec<_>>()
        .join("; ");
    let user_command = command.join(" ");

    let final_command = toolchain::wrap_command(&user_command, &detected);
    let full_command = format!("{exports}; {final_command}");

    lima_cmd.arg("--");
    lima_cmd.arg("eval");
    lima_cmd.arg(&full_command);

    debug!(vm = %vm_name, command = %user_command, "spawning limactl shell");

    let status = lima_cmd
        .status()
        .context("Failed to execute limactl shell")?;

    let exit_code = status.code().unwrap_or(1);
    info!(exit_code, "agent command exited");
    Ok(exit_code)
}

fn run_container(
    config: &Config,
    pane_cwd: &Path,
    worktree_root: &Path,
    command: &[String],
) -> Result<i32> {
    info!(
        pane_cwd = %pane_cwd.display(),
        worktree_root = %worktree_root.display(),
        "sandbox supervisor starting (container)"
    );

    // Validate that pane_cwd is under worktree_root
    if !pane_cwd.starts_with(worktree_root) {
        bail!(
            "Working directory {} is not under worktree root {}",
            pane_cwd.display(),
            worktree_root.display()
        );
    }

    // Ensure sandbox config dirs exist before building container args
    ensure_sandbox_config_dirs()?;

    // Merge built-in commands (e.g. afplay, clipboard shims) with user-configured ones
    let host_commands = shims::effective_host_commands(config.sandbox.host_commands());
    // Clipboard shims use ClipboardRead RPC, not Exec -- exclude from exec allowlist
    let allowed_commands: HashSet<String> = host_commands
        .iter()
        .filter(|cmd| !shims::is_clipboard_shim(cmd))
        .cloned()
        .collect();

    // Resolve toolchain for host-exec command wrapping (runs on host, not in container)
    let detected = toolchain::resolve_toolchain(&config.sandbox.toolchain(), worktree_root);
    if detected != toolchain::DetectedToolchain::None {
        info!(toolchain = ?detected, "wrapping host-exec commands with toolchain environment");
    }

    // Create shims directory for host-exec (on host, will be bind-mounted into container).
    // Use ~/.cache/workmux/shims/ instead of system temp (/var/folders/... on macOS)
    // so the path is inside ~ and accessible to VM-based runtimes like Colima.
    let _shim_dir = {
        let home = home::home_dir().context("Could not determine home directory")?;
        let shims_base = home.join(".cache/workmux/shims");
        std::fs::create_dir_all(&shims_base)
            .with_context(|| format!("Failed to create {}", shims_base.display()))?;
        let dir = tempfile::Builder::new()
            .prefix("shims-")
            .tempdir_in(&shims_base)
            .context("Failed to create shim temp dir")?;
        shims::create_shim_directory(dir.path(), &host_commands)?;
        info!(commands = ?host_commands, "created host-exec shims");
        Some(dir)
    };

    let (rpc_server, rpc_port, rpc_token, ctx) = start_rpc(
        pane_cwd,
        allowed_commands,
        detected.clone(),
        config.sandbox.allow_unsandboxed_host_exec(),
    )?;
    let _rpc_handle = rpc_server.spawn(ctx);

    // Start network proxy when policy is deny
    let network_deny = config.sandbox.network_policy_is_deny();
    let proxy = if network_deny {
        let allowed = config.sandbox.network.allowed_domains();
        let proxy = NetworkProxy::bind(allowed)?;
        let proxy_port = proxy.port();
        let proxy_token = proxy.token().to_string();
        let handle = proxy.spawn();
        info!(port = proxy_port, "network proxy started");
        Some((proxy_port, proxy_token, handle))
    } else {
        None
    };

    // Compute RPC host BEFORE matching on runtime (SandboxRuntime is not Copy)
    let rpc_host = config.sandbox.resolved_rpc_host();
    let runtime = config.sandbox.runtime();
    let runtime_bin = runtime.binary_name();

    // Generate container name from worktree directory name so cleanup can find it.
    // Include PID to allow multiple agents in the same worktree (e.g., open -n).
    let handle = worktree_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let container_name = format!("wm-{}-{}", handle, std::process::id());

    // Register container in state store so cleanup can find it without docker ps
    if let Ok(store) = StateStore::new()
        && let Err(e) = store.register_container(&handle, &container_name, &runtime)
    {
        warn!(error = %e, "failed to register container state");
    }

    // Build owned env pairs first, then borrow at call site.
    // Proxy URL is a local String so we can't use &str slices directly.
    let rpc_port_str = rpc_port.to_string();
    let mut owned_envs: Vec<(String, String)> = vec![
        ("WM_SANDBOX_GUEST".into(), "1".into()),
        ("WM_RPC_HOST".into(), rpc_host.clone()),
        ("WM_RPC_PORT".into(), rpc_port_str.clone()),
        ("WM_RPC_TOKEN".into(), rpc_token.clone()),
    ];

    if let Some((proxy_port, ref proxy_token, _)) = proxy {
        let proxy_url = format!("http://workmux:{}@{}:{}", proxy_token, rpc_host, proxy_port);
        let no_proxy = format!("localhost,127.0.0.1,{}", rpc_host);

        owned_envs.push(("HTTPS_PROXY".into(), proxy_url.clone()));
        owned_envs.push(("HTTP_PROXY".into(), proxy_url.clone()));
        owned_envs.push(("https_proxy".into(), proxy_url.clone()));
        owned_envs.push(("http_proxy".into(), proxy_url));
        owned_envs.push(("NO_PROXY".into(), no_proxy.clone()));
        owned_envs.push(("no_proxy".into(), no_proxy));
        // Pass hostname (not IP literal) so the init script can resolve ALL
        // IPs and whitelist them all in iptables.
        owned_envs.push(("WM_PROXY_HOST".into(), rpc_host.clone()));
        owned_envs.push(("WM_PROXY_PORT".into(), proxy_port.to_string()));
    }

    // Inject host git user config (user.name, user.email) for commits
    owned_envs.extend(git_user_config_envs(worktree_root));

    // Borrow owned envs for call site
    let env_refs: Vec<(&str, &str)> = owned_envs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let agent = crate::multiplexer::agent::resolve_profile_with_type(
        config.agent.as_deref(),
        config.agent_type.as_deref(),
    )
    .name();

    let user_command = command.join(" ");
    let shim_host_dir = _shim_dir.as_ref().map(|d| d.path().join("shims/bin"));
    let mut docker_args = build_docker_run_args(
        &user_command,
        &config.sandbox,
        agent,
        worktree_root,
        pane_cwd,
        &env_refs,
        shim_host_dir.as_deref(),
        network_deny,
    )?;

    // Insert --name after "run" (index 0 is "run")
    docker_args.insert(1, "--name".to_string());
    docker_args.insert(2, container_name.clone());

    let env_keys: Vec<&str> = config.sandbox.env_vars().iter().map(|(k, _)| *k).collect();
    let redacted_args: Vec<_> = docker_args
        .iter()
        .map(|a| redact_env_arg(a, &env_keys))
        .collect();
    debug!(runtime = runtime_bin, container = %container_name, args = ?redacted_args, "spawning container");

    // Background freshness check (non-blocking)
    let freshness_image = config.sandbox.resolved_image(agent);
    crate::sandbox::freshness::check_in_background(freshness_image, config.sandbox.runtime());

    // Create guard to stop container on exit (panic, SIGTERM, etc.)
    let _guard = ContainerGuard {
        runtime: runtime_bin,
        name: container_name,
        handle,
    };

    let status = Command::new(runtime_bin)
        .args(&docker_args)
        .status()
        .with_context(|| format!("Failed to execute {} run", runtime_bin))?;

    let exit_code = status.code().unwrap_or(1);
    info!(exit_code, "container command exited");
    Ok(exit_code)
}

/// Redact sensitive values in docker run args for debug logging.
/// Covers RPC token, proxy URLs (which embed the proxy auth token),
/// and explicit sandbox.env values (which may contain secrets).
pub(super) fn redact_env_arg(arg: &str, extra_redact_keys: &[&str]) -> String {
    if let Some((key, _)) = arg.split_once('=')
        && (key == "WM_RPC_TOKEN"
            || key.to_uppercase().contains("PROXY")
            || extra_redact_keys.contains(&key))
    {
        return format!("{}=<redacted>", key);
    }
    arg.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_rpc_token() {
        assert_eq!(
            redact_env_arg("WM_RPC_TOKEN=abc123", &[]),
            "WM_RPC_TOKEN=<redacted>"
        );
    }

    #[test]
    fn redact_proxy_env_vars() {
        let cases = [
            "HTTPS_PROXY=http://workmux:secret@host:1234",
            "HTTP_PROXY=http://workmux:secret@host:1234",
            "https_proxy=http://workmux:secret@host:1234",
            "http_proxy=http://workmux:secret@host:1234",
        ];
        for arg in &cases {
            let redacted = redact_env_arg(arg, &[]);
            assert!(
                redacted.ends_with("=<redacted>"),
                "expected redacted for {}, got {}",
                arg,
                redacted
            );
            assert!(!redacted.contains("secret"), "token leaked in {}", redacted);
        }
    }

    #[test]
    fn redact_no_proxy_env_var() {
        // NO_PROXY contains "PROXY" so it gets redacted too (safe default)
        let redacted = redact_env_arg("NO_PROXY=localhost,127.0.0.1", &[]);
        assert_eq!(redacted, "NO_PROXY=<redacted>");
    }

    #[test]
    fn no_redact_normal_args() {
        assert_eq!(redact_env_arg("HOME=/tmp", &[]), "HOME=/tmp");
        assert_eq!(redact_env_arg("--rm", &[]), "--rm");
        assert_eq!(
            redact_env_arg("WM_SANDBOX_GUEST=1", &[]),
            "WM_SANDBOX_GUEST=1"
        );
    }

    #[test]
    fn redact_sandbox_env_keys() {
        let extra = ["GH_TOKEN", "OPENAI_API_KEY"];
        assert_eq!(
            redact_env_arg("GH_TOKEN=ghp_secret123", &extra),
            "GH_TOKEN=<redacted>"
        );
        assert_eq!(
            redact_env_arg("OPENAI_API_KEY=sk-abc", &extra),
            "OPENAI_API_KEY=<redacted>"
        );
        // Non-extra keys still pass through
        assert_eq!(redact_env_arg("HOME=/tmp", &extra), "HOME=/tmp");
    }

    // ── git_user_config_envs tests ──────────────────────────────────────

    /// Create a temp directory with a git repo and local user config.
    fn git_repo_with_user(name: &str, email: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", name])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", email])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        tmp
    }

    #[test]
    fn test_git_user_config_envs_with_both_values() {
        let tmp = git_repo_with_user("Test User", "test@example.com");
        let envs = git_user_config_envs(tmp.path());

        assert_eq!(envs.len(), 5); // COUNT + 2*(KEY + VALUE)
        assert_eq!(envs[0], ("GIT_CONFIG_COUNT".into(), "2".into()));
        assert_eq!(envs[1], ("GIT_CONFIG_KEY_0".into(), "user.name".into()));
        assert_eq!(envs[2], ("GIT_CONFIG_VALUE_0".into(), "Test User".into()));
        assert_eq!(envs[3], ("GIT_CONFIG_KEY_1".into(), "user.email".into()));
        assert_eq!(
            envs[4],
            ("GIT_CONFIG_VALUE_1".into(), "test@example.com".into())
        );
    }

    #[test]
    fn test_git_user_config_envs_with_no_config() {
        let tmp = tempfile::tempdir().unwrap();
        // Init repo but don't set user config
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let envs = git_user_config_envs(tmp.path());
        // May return values from global/system config or empty vec
        // We can't assert empty because the test runner's global git config may have user.*
        // Instead, verify the structure is correct if any values are returned
        if !envs.is_empty() {
            assert!(envs[0].0 == "GIT_CONFIG_COUNT");
        }
    }

    #[test]
    fn test_git_user_config_envs_not_a_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        // No git init -- not a git repo
        let envs = git_user_config_envs(tmp.path());
        // Should not crash, returns empty or global config values
        for (key, _) in &envs {
            assert!(key.starts_with("GIT_CONFIG_"), "unexpected key: {}", key);
        }
    }

    #[test]
    fn test_git_user_config_envs_special_characters() {
        let tmp = git_repo_with_user("John O'Brien", "john@example.com");
        let envs = git_user_config_envs(tmp.path());

        let name_val = envs
            .iter()
            .find(|(k, _)| k == "GIT_CONFIG_VALUE_0")
            .map(|(_, v)| v.as_str());
        assert_eq!(name_val, Some("John O'Brien"));
    }
}
