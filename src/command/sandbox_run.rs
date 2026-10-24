//! The `workmux sandbox run` supervisor process.
//!
//! Runs inside a tmux pane. Starts a TCP RPC server and executes the agent
//! command inside a sandbox (Lima VM or Docker/Podman container).

use anyhow::{Context, Result, anyhow, bail};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tracing::{debug, info, warn};

use std::collections::HashSet;

use crate::config::{Config, SandboxBackend, SandboxRuntime};
use crate::frozen_config::FrozenConfigGuard;
use crate::multiplexer;
use crate::remote::node::{NodeRoot, secrets};
use crate::sandbox::build_docker_run_args;
use crate::sandbox::ensure_sandbox_config_dirs;
use crate::sandbox::lima;
use crate::sandbox::network_proxy::NetworkProxy;
use crate::sandbox::rpc::{NodeWorkspace, RpcContext, RpcServer, generate_token};
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
///
/// With `node` (root and workspace id), the supervisor runs on a remote node
/// for an agent the laptop started: the config is the workspace's frozen
/// snapshot, state lives under the node root, listeners bind only to
/// docker's bridge, and the container gets the workspace's secrets.
pub fn run(
    worktree: PathBuf,
    worktree_root: Option<PathBuf>,
    command: Vec<String>,
    node: Option<(PathBuf, String)>,
) -> Result<i32> {
    if command.is_empty() {
        bail!("No command specified. Usage: workmux sandbox run <worktree> -- <command...>");
    }
    let node_root = node.as_ref().map(|(root, _)| root);
    if let Some(root) = node_root {
        if std::env::var_os(crate::frozen_config::FROZEN_CONFIG_ENV).is_none() {
            bail!(
                "--node-root needs the workspace config in {}",
                crate::frozen_config::FROZEN_CONFIG_ENV
            );
        }
        // SAFETY: no other threads exist yet.
        unsafe { std::env::set_var("XDG_STATE_HOME", root.join("state")) };
    }

    let worktree = worktree.canonicalize().unwrap_or_else(|_| worktree.clone());
    let (config, location) = Config::load_with_location_from(&worktree, None)?;
    let frozen_config = FrozenConfigGuard::capture(&config, location.as_ref(), &worktree)?;

    match config.sandbox.backend() {
        SandboxBackend::Lima if node_root.is_some() => {
            bail!("remote agents run in docker containers, not Lima")
        }
        SandboxBackend::Lima => run_lima(&config, frozen_config.path(), &worktree, &command),
        SandboxBackend::Container => {
            let wt_root = worktree_root
                .map(|p| p.canonicalize().unwrap_or(p))
                .unwrap_or_else(|| worktree.clone());
            let node = node
                .map(|(root, ws_id)| -> Result<_> {
                    crate::remote::node::validate_id(&ws_id, 'w', "workspace id")?;
                    Ok((NodeRoot::at(root)?, ws_id))
                })
                .transpose()?;
            run_container(
                &config,
                frozen_config.path(),
                &worktree,
                &wt_root,
                &command,
                node.as_ref().map(|(root, ws_id)| (root, ws_id.as_str())),
            )
        }
    }
}

/// Docker's bridge gateway: the host's address on the network containers
/// join by default, reachable from them and not routed from outside.
fn bridge_gateway() -> Result<Ipv4Addr> {
    let output = Command::new("docker")
        .args([
            "network",
            "inspect",
            "bridge",
            "--format",
            "{{range .IPAM.Config}}{{.Gateway}} {{end}}",
        ])
        .output()
        .context("Failed to run docker network inspect")?;
    if !output.status.success() {
        bail!(
            "docker network inspect bridge failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_bridge_gateway(&String::from_utf8_lossy(&output.stdout))
}

fn parse_bridge_gateway(text: &str) -> Result<Ipv4Addr> {
    text.split_whitespace()
        .filter_map(|word| word.parse::<Ipv4Addr>().ok())
        .find(|ip| ip.is_private())
        .ok_or_else(|| {
            anyhow!(
                "docker's bridge network has no private IPv4 gateway ({:?}); refusing to \
                 start listeners for the agent",
                text.trim()
            )
        })
}

/// Start RPC server and return (server, port, token, context).
/// Shared setup between Lima and Container backends.
#[allow(clippy::too_many_arguments)]
fn start_rpc(
    config: &Config,
    frozen_config_path: &Path,
    worktree: &Path,
    allowed_commands: HashSet<String>,
    detected_toolchain: toolchain::DetectedToolchain,
    allow_unsandboxed_host_exec: bool,
    bind_ip: Option<IpAddr>,
    node: Option<(&NodeRoot, &str)>,
) -> Result<(RpcServer, u16, String, Arc<RpcContext>)> {
    let rpc_server = match bind_ip {
        Some(ip) => RpcServer::bind_to(ip)?,
        None => RpcServer::bind()?,
    };
    let rpc_port = rpc_server.port();
    let rpc_token = generate_token();
    info!(port = rpc_port, "RPC server listening");

    let mux = multiplexer::create_backend(multiplexer::detect_backend());
    let pane_id = mux.current_pane_id().unwrap_or_default();

    let ctx = Arc::new(RpcContext {
        pane_id,
        worktree_path: worktree.to_path_buf(),
        mux,
        config: Arc::new(config.clone()),
        frozen_config_path: frozen_config_path.to_path_buf(),
        token: rpc_token.clone(),
        allowed_commands,
        detected_toolchain,
        allow_unsandboxed_host_exec,
        node: node.map(|(root, ws_id)| NodeWorkspace {
            root: root.path.clone(),
            ws_id: ws_id.to_string(),
        }),
    });

    Ok((rpc_server, rpc_port, rpc_token, ctx))
}

/// Build command-scope Git configuration for a sandbox guest.
///
/// The worktree is marked safe for runtimes whose mount ownership differs from
/// the guest UID. Host user identity is copied so guest commits retain the
/// configured author name and email.
pub(super) fn git_user_config_envs(worktree_dir: &Path) -> Vec<(String, String)> {
    let mut entries = vec![(
        "safe.directory".to_string(),
        worktree_dir.to_string_lossy().into_owned(),
    )];

    for key in &["user.name", "user.email"] {
        let Ok(mut command) = crate::git::unattended_git(Some(worktree_dir)) else {
            continue;
        };
        if let Ok(output) = command
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

    let mut envs = Vec::with_capacity(1 + entries.len() * 2);
    envs.push(("GIT_CONFIG_COUNT".into(), entries.len().to_string()));
    for (i, (key, val)) in entries.iter().enumerate() {
        envs.push((format!("GIT_CONFIG_KEY_{}", i), key.clone()));
        envs.push((format!("GIT_CONFIG_VALUE_{}", i), val.clone()));
    }
    envs
}

#[cfg(test)]
fn clear_ambient_git_env(command: &mut Command) {
    crate::git::clear_ambient_git_env(command);
}

fn run_lima(
    config: &Config,
    frozen_config_path: &Path,
    worktree: &Path,
    command: &[String],
) -> Result<i32> {
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
        config,
        frozen_config_path,
        worktree,
        allowed_commands,
        detected.clone(),
        config.sandbox.allow_unsandboxed_host_exec(),
        None,
        None,
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
    frozen_config_path: &Path,
    pane_cwd: &Path,
    worktree_root: &Path,
    command: &[String],
    node: Option<(&NodeRoot, &str)>,
) -> Result<i32> {
    let on_node = node.is_some();
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

    // On a node, listeners bind to docker's bridge only, never to every
    // interface of a public host.
    let bind_ip = if on_node {
        if config.sandbox.runtime() != SandboxRuntime::Docker {
            bail!("remote agents need docker; podman is not supported on remotes");
        }
        Some(IpAddr::V4(bridge_gateway()?))
    } else {
        None
    };

    // Ensure sandbox config dirs exist before building container args
    ensure_sandbox_config_dirs()?;

    // Ensure image is present and up-to-date before setting up RPC/state.
    // Done early so an interrupted pull doesn't leave stale state entries.
    let agent = crate::multiplexer::agent::resolve_profile_with_type(
        config.agent.as_deref(),
        config.agent_type.as_deref(),
    )
    .name();
    let freshness_image = config.sandbox.resolved_image(agent);
    crate::sandbox::ensure_image_ready(&config.sandbox, &freshness_image)?;

    // Merge built-in commands (e.g. afplay, clipboard shims) with user-configured ones.
    // A node's host is not the user's machine, so it runs none.
    let host_commands = if on_node {
        Vec::new()
    } else {
        shims::effective_host_commands(config.sandbox.host_commands())
    };
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
    // Use $XDG_CACHE_HOME/workmux/shims/ instead of system temp (/var/folders/... on macOS)
    // so the path is inside ~ and accessible to VM-based runtimes like Colima.
    let _shim_dir = if on_node {
        None
    } else {
        let shims_base = crate::xdg::cache_dir()?.join("shims");
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
        config,
        frozen_config_path,
        pane_cwd,
        allowed_commands,
        detected.clone(),
        config.sandbox.allow_unsandboxed_host_exec() && !on_node,
        bind_ip,
        node,
    )?;
    let _rpc_handle = rpc_server.spawn(ctx);

    // Start network proxy when policy is deny
    let network_deny = config.sandbox.network_policy_is_deny();
    let proxy = if network_deny {
        let allowed = config.sandbox.network.allowed_domain_rules();
        let max_connections = config.sandbox.network.max_connections();
        let proxy = match bind_ip {
            Some(ip) => NetworkProxy::bind_to(ip, &allowed, max_connections)?,
            None => NetworkProxy::bind(&allowed, max_connections)?,
        };
        let proxy_port = proxy.port();
        let proxy_token = proxy.token().to_string();
        let handle = proxy.spawn();
        info!(port = proxy_port, "network proxy started");
        Some((proxy_port, proxy_token, handle))
    } else {
        None
    };

    // Compute RPC host BEFORE matching on runtime (SandboxRuntime is not Copy)
    let rpc_host = match bind_ip {
        Some(ip) => ip.to_string(),
        None => config.sandbox.resolved_rpc_host(),
    };
    let runtime = config.sandbox.runtime();
    crate::sandbox::validate_git_metadata_boundary(runtime)?;
    let runtime_bin = runtime.binary_name();

    // Generate container name from worktree directory name so cleanup can find it.
    // Include PID to allow multiple agents in the same worktree (e.g., open -n).
    let handle = worktree_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    // Slugify only the container name; the raw handle is used as the state store key
    // so cleanup (which derives the handle from the directory name without slugifying)
    // can find it.
    let container_name = format!("wm-{}-{}", slug::slugify(&handle), std::process::id());

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
        owned_envs.extend(crate::sandbox::proxy_env_vars(
            &rpc_host,
            proxy_port,
            proxy_token,
        ));
    }

    // Inject host git user config (user.name, user.email) for commits
    owned_envs.extend(git_user_config_envs(worktree_root));

    // Borrow owned envs for call site
    let env_refs: Vec<(&str, &str)> = owned_envs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

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

    // A node's workspace environment goes through a private pipe, so values
    // never appear in argv or change this process's own environment.
    let _env_file = match node {
        Some((root, ws_id)) => {
            let secrets = secrets::for_execution(root, ws_id, Some(agent))?;
            let file = secrets::env_file(root, &container_name, &secrets)?;
            docker_args.insert(3, "--env-file".to_string());
            docker_args.insert(4, file.to_string_lossy().into_owned());
            Some(file)
        }
        None => None,
    };

    let env_keys: Vec<&str> = config.sandbox.env_vars().iter().map(|(k, _)| *k).collect();
    let redacted_args: Vec<_> = docker_args
        .iter()
        .map(|a| redact_env_arg(a, &env_keys))
        .collect();
    debug!(runtime = runtime_bin, container = %container_name, args = ?redacted_args, "spawning container");

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
    fn bridge_gateway_must_be_private_ipv4() {
        assert_eq!(
            parse_bridge_gateway("172.17.0.1 \n").unwrap(),
            Ipv4Addr::new(172, 17, 0, 1)
        );
        assert_eq!(
            parse_bridge_gateway("fd00::1 10.1.0.1 ").unwrap(),
            Ipv4Addr::new(10, 1, 0, 1)
        );
        for bad in ["", "\n", "0.0.0.0", "203.0.113.5", "fd00::1"] {
            assert!(parse_bridge_gateway(bad).is_err(), "{bad:?}");
        }
    }

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
        let mut init = Command::new("git");
        clear_ambient_git_env(&mut init);
        init.args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let mut set_name = Command::new("git");
        clear_ambient_git_env(&mut set_name);
        set_name
            .args(["config", "user.name", name])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let mut set_email = Command::new("git");
        clear_ambient_git_env(&mut set_email);
        set_email
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

        assert_eq!(envs.len(), 7);
        assert_eq!(envs[0], ("GIT_CONFIG_COUNT".into(), "3".into()));
        assert_eq!(
            envs[1],
            ("GIT_CONFIG_KEY_0".into(), "safe.directory".into())
        );
        assert_eq!(
            envs[2],
            (
                "GIT_CONFIG_VALUE_0".into(),
                tmp.path().to_string_lossy().into_owned()
            )
        );
        assert_eq!(envs[3], ("GIT_CONFIG_KEY_1".into(), "user.name".into()));
        assert_eq!(envs[4], ("GIT_CONFIG_VALUE_1".into(), "Test User".into()));
        assert_eq!(envs[5], ("GIT_CONFIG_KEY_2".into(), "user.email".into()));
        assert_eq!(
            envs[6],
            ("GIT_CONFIG_VALUE_2".into(), "test@example.com".into())
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
            .find(|(k, _)| k == "GIT_CONFIG_VALUE_1")
            .map(|(_, v)| v.as_str());
        assert_eq!(name_val, Some("John O'Brien"));
    }
}
