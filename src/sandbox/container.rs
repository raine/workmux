//! Docker/Podman container sandbox implementation.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::{SandboxConfig, SandboxRuntime};
use crate::state::StateStore;

/// Default image registry prefix.
pub const DEFAULT_IMAGE_REGISTRY: &str = "ghcr.io/raine/workmux-sandbox";

/// Embedded Dockerfiles for each agent.
pub const DOCKERFILE_BASE: &str = include_str!("../../docker/Dockerfile.base");
pub const DOCKERFILE_CLAUDE: &str = include_str!("../../docker/Dockerfile.claude");
pub const DOCKERFILE_CODEX: &str = include_str!("../../docker/Dockerfile.codex");
pub const DOCKERFILE_GEMINI: &str = include_str!("../../docker/Dockerfile.gemini");
pub const DOCKERFILE_OPENCODE: &str = include_str!("../../docker/Dockerfile.opencode");
pub const DOCKERFILE_PI: &str = include_str!("../../docker/Dockerfile.pi");

/// Known agents that have pre-built images.
pub const KNOWN_AGENTS: &[&str] = &["claude", "codex", "gemini", "opencode", "pi"];

/// Get the agent-specific Dockerfile content, or None for unknown agents.
pub fn dockerfile_for_agent(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some(DOCKERFILE_CLAUDE),
        "codex" => Some(DOCKERFILE_CODEX),
        "gemini" => Some(DOCKERFILE_GEMINI),
        "opencode" => Some(DOCKERFILE_OPENCODE),
        "pi" => Some(DOCKERFILE_PI),
        _ => None,
    }
}

/// Sandbox-specific config paths on host.
///
/// Two layouts exist:
/// - `config_file` (~/.claude-sandbox.json): direct file mount for Docker/Podman
/// - `config_dir` (~/.claude-sandbox-config/): directory mount for Apple Container,
///   which only supports directory mounts via virtiofs
pub struct SandboxPaths {
    /// ~/.claude-sandbox.json - used by Docker/Podman (file mount)
    pub config_file: PathBuf,
    /// ~/.claude-sandbox-config/ - used by Apple Container (directory mount)
    pub config_dir: PathBuf,
}

const CLAUDE_ONBOARDING_JSON: &str =
    r#"{"hasCompletedOnboarding":true,"bypassPermissionsModeAccepted":true}"#;

impl SandboxPaths {
    pub fn new() -> Option<Self> {
        let home = home::home_dir()?;
        Some(Self {
            config_file: home.join(".claude-sandbox.json"),
            config_dir: home.join(".claude-sandbox-config"),
        })
    }
}

/// Ensure sandbox config files exist on host.
pub fn ensure_sandbox_config_dirs() -> Result<SandboxPaths> {
    let paths = SandboxPaths::new().context("Could not determine home directory")?;

    // Docker/Podman: seed single file
    if !paths.config_file.exists() {
        std::fs::write(&paths.config_file, CLAUDE_ONBOARDING_JSON)
            .with_context(|| format!("Failed to create {}", paths.config_file.display()))?;
    }

    // Apple Container: seed directory with claude.json
    std::fs::create_dir_all(&paths.config_dir)
        .with_context(|| format!("Failed to create {}", paths.config_dir.display()))?;
    let dir_file = paths.config_dir.join("claude.json");
    if !dir_file.exists() {
        std::fs::write(&dir_file, CLAUDE_ONBOARDING_JSON)
            .with_context(|| format!("Failed to create {}", dir_file.display()))?;
    }

    Ok(paths)
}

/// Build the sandbox Docker image locally (two-stage: base + agent).
pub fn build_image(config: &SandboxConfig, agent: &str) -> Result<()> {
    let runtime = config.runtime().binary_name();

    let agent_dockerfile = dockerfile_for_agent(agent).ok_or_else(|| {
        anyhow::anyhow!(
            "No Dockerfile for agent '{}'. Known agents: {}",
            agent,
            KNOWN_AGENTS.join(", ")
        )
    })?;

    // Stage 1: Build base image (use localhost/ prefix for Podman compatibility)
    let base_tag = "localhost/workmux-sandbox-base";
    println!("Building base image...");

    let tmp_dir = tempfile::tempdir().context("Failed to create temp dir")?;
    std::fs::write(tmp_dir.path().join("Dockerfile"), DOCKERFILE_BASE)?;

    let status = Command::new(runtime)
        .env("DOCKER_BUILDKIT", "1")
        .env("DOCKER_CLI_HINTS", "false")
        .args(["build", "-t", base_tag, "-f", "Dockerfile", "."])
        .current_dir(tmp_dir.path())
        .status()
        .context("Failed to build base image")?;

    if !status.success() {
        anyhow::bail!("Failed to build base image");
    }

    // Stage 2: Build agent image on top of local base
    let image = config.resolved_image(agent);
    println!("Building {} image...", agent);

    let agent_tmp = tempfile::tempdir().context("Failed to create temp dir")?;
    std::fs::write(agent_tmp.path().join("Dockerfile"), agent_dockerfile)?;

    let status = Command::new(runtime)
        .env("DOCKER_BUILDKIT", "1")
        .env("DOCKER_CLI_HINTS", "false")
        .args([
            "build",
            "--build-arg",
            &format!("BASE={}", base_tag),
            "-t",
            &image,
            "-f",
            "Dockerfile",
            ".",
        ])
        .current_dir(agent_tmp.path())
        .status()
        .context("Failed to build agent image")?;

    if !status.success() {
        anyhow::bail!("Failed to build image '{}'", image);
    }

    Ok(())
}

/// Pull the sandbox image from the registry.
pub fn pull_image(config: &SandboxConfig, image: &str) -> Result<()> {
    let runtime = config.runtime();

    let status = Command::new(runtime.binary_name())
        .args(runtime.pull_args(image))
        .status()
        .context("Failed to run container runtime")?;

    if !status.success() {
        anyhow::bail!("Failed to pull image '{}'", image);
    }

    Ok(())
}

/// Ensure the container image is ready to run.
///
/// - If the image is missing and it's an official image, pull it automatically.
/// - If the image exists but is stale (per freshness cache), pull the update.
///   If the update pull fails, warn and continue with the local image.
/// - For custom (non-official) images, only check existence.
/// - Kicks off a background freshness cache update for the next run.
pub fn ensure_image_ready(config: &SandboxConfig, image: &str) -> Result<()> {
    let runtime = config.runtime();
    let runtime_bin = runtime.binary_name();
    let runtime_display = runtime.display_name();
    let is_official = crate::sandbox::freshness::is_official_image(image);

    // Check if image exists locally
    let exists = Command::new(runtime_bin)
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !exists {
        if is_official {
            eprintln!("Image '{}' not found locally, pulling...", image);
            pull_image(config, image)?;
            crate::sandbox::freshness::mark_fresh(image, runtime);
            return Ok(());
        } else {
            anyhow::bail!(
                "Image '{}' not found in {} image store. \
                 If you built this image with a different runtime \
                 (e.g. docker vs apple-container), it won't be visible here.",
                image,
                runtime_display,
            );
        }
    }

    // Image exists. For official images, check if it's stale.
    if is_official {
        let stale = crate::sandbox::freshness::cached_is_stale(image, runtime);
        if stale == Some(true) {
            eprintln!("Updating sandbox image '{}'...", image);
            match pull_image(config, image) {
                Ok(()) => {
                    crate::sandbox::freshness::mark_fresh(image, runtime);
                }
                Err(e) => {
                    eprintln!(
                        "warning: failed to update sandbox image: {}; continuing with local image",
                        e
                    );
                    // Still refresh cache in background so next run retries
                    crate::sandbox::freshness::check_in_background(image.to_string(), runtime);
                }
            }
        } else {
            // Not known stale: refresh cache in background for next run
            crate::sandbox::freshness::check_in_background(image.to_string(), runtime);
        }
    }

    Ok(())
}

/// Build the argument list for a `docker run` command.
///
/// Returns the full arg vector (excluding the runtime binary name itself).
/// Used by the sandbox supervisor to run containers with RPC connection details.
///
/// Callers must:
/// - Prepend the runtime binary name (docker/podman)
/// - Call `ensure_sandbox_config_dirs()` before this function if config mounts are needed
/// - Use `Command::args()` (not string joining) since args are not shell-quoted
#[allow(clippy::too_many_arguments)]
pub fn build_docker_run_args(
    command: &str,
    config: &SandboxConfig,
    agent: &str,
    worktree_root: &Path,
    pane_cwd: &Path,
    extra_envs: &[(&str, &str)],
    shim_host_dir: Option<&Path>,
    network_deny: bool,
) -> Result<Vec<String>> {
    let image = config.resolved_image(agent);
    let worktree_root_str = worktree_root.to_string_lossy();
    let pane_cwd_str = pane_cwd.to_string_lossy();

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    let mut args = Vec::new();

    // Base command (no runtime name -- caller prepends that)
    args.push("run".to_string());
    args.push("--rm".to_string());
    args.push("-it".to_string());

    let runtime = config.runtime();

    // Resource limits: user config overrides runtime default.
    // Apple Container VMs default to 1 GB RAM which is too low for most workloads.
    // Docker/Podman use host resources directly, so these are only passed when
    // explicitly configured (or when the runtime provides a default).
    if let Some(mem) = config
        .container
        .memory
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| runtime.default_memory())
    {
        args.push("--memory".to_string());
        args.push(mem.to_string());
    }
    if let Some(cpus) = config.container.cpus {
        args.push("--cpus".to_string());
        args.push(cpus.to_string());
    }

    // On Linux Docker Engine (not Desktop), host.docker.internal doesn't resolve
    // unless we explicitly add it. The special "host-gateway" value maps to the
    // host's gateway IP. This is a harmless no-op on Docker Desktop.

    if runtime.needs_add_host() {
        args.push("--add-host".to_string());
        args.push("host.docker.internal:host-gateway".to_string());
    }

    // Host hardware access: global-only in config, not supported on Apple Container.
    let devices = config.container.devices();
    let group_add = config.container.group_add();
    if (!devices.is_empty() || !group_add.is_empty()) && runtime == SandboxRuntime::AppleContainer {
        anyhow::bail!(
            "sandbox.container.devices and sandbox.container.group_add are not supported \
             on Apple Container. Set sandbox.container.runtime to docker or podman."
        );
    }
    for dev in devices {
        args.push("--device".to_string());
        args.push(dev.to_arg());
    }

    if network_deny {
        // Deny mode: start as root for iptables setup, drop privileges via setpriv.
        // Do NOT use --userns=keep-id (Podman) in deny mode since the container
        // starts as root and drops privileges after iptables setup.
        if runtime.needs_deny_mode_caps() {
            args.extend(deny_mode_run_flags());
        }
        args.push("--env".to_string());
        args.push(format!("WM_TARGET_UID={}", uid));
        args.push("--env".to_string());
        args.push(format!("WM_TARGET_GID={}", gid));
        // Supplementary groups are applied inside the container by setpriv
        // (see docker/Dockerfile.base). We do NOT pass --group-add here because
        // in deny mode the root process drops privileges after iptables setup,
        // and the --group-add groups would be stripped during that drop.
        if !group_add.is_empty() {
            args.push("--env".to_string());
            args.push(format!("WM_EXTRA_GIDS={}", group_add.join(",")));
        }
    } else {
        // Normal mode: run as user directly.
        // Rootless Podman uses a user namespace that remaps UIDs. Without --userns=keep-id,
        // the host UID appears as root inside the container, making bind-mounted files
        // (credentials, config) inaccessible to the --user process.
        if runtime.needs_userns_keep_id() {
            args.push("--userns=keep-id".to_string());
        }
        args.push("--user".to_string());
        args.push(format!("{}:{}", uid, gid));
        for g in group_add {
            args.push("--group-add".to_string());
            args.push(g.clone());
        }
    }

    // Mirror mount worktree
    args.push("--mount".to_string());
    args.push(format!(
        "type=bind,source={},target={}",
        worktree_root_str, worktree_root_str
    ));

    // Git worktree mounts: .git directory + main worktree (for symlink resolution)
    //
    // `.git` in a linked worktree is a file like `gitdir: <path>`. `<path>` is
    // absolute by default but can be relative when the worktree was created
    // with `git worktree add --relative-paths` (git 2.48+), in which case it
    // is resolved against the worktree root. Emitting a relative path into
    // `--mount` would produce bogus mount specs.
    let mut main_worktree_path: Option<PathBuf> = None;
    let git_path = worktree_root.join(".git");
    if git_path.is_file()
        && let Ok(content) = std::fs::read_to_string(&git_path)
        && let Some(gitdir) = content.strip_prefix("gitdir: ")
    {
        let gitdir_path = {
            let p = Path::new(gitdir.trim());
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                worktree_root.join(p)
            }
        };
        if let Some(main_git) = gitdir_path.ancestors().nth(2) {
            // Mount the .git directory for git operations
            args.push("--mount".to_string());
            args.push(format!(
                "type=bind,source={},target={}",
                main_git.display(),
                main_git.display()
            ));

            // Mount the main worktree to resolve symlinks pointing there
            // (e.g., CLAUDE.local.md -> ../../main-worktree/CLAUDE.local.md)
            if let Some(main_worktree) = main_git.parent() {
                args.push("--mount".to_string());
                args.push(format!(
                    "type=bind,source={},target={}",
                    main_worktree.display(),
                    main_worktree.display()
                ));
                main_worktree_path = Some(main_worktree.to_path_buf());
            }
        }
    }

    // Mask configured files out of the worktree mounts by bind-mounting
    // /dev/null over them. Must come AFTER the worktree AND main-worktree
    // mounts so the /dev/null mounts win even for aliased paths (a file in
    // the current worktree that is a symlink into the main worktree).
    //
    // Missing files are skipped -- bind-mounting over a nonexistent target
    // would fail and kill the container. Paths that escape the worktree are
    // rejected to prevent a malicious project config from masking host files.
    let excluded = config.container.excluded_files();
    if !excluded.is_empty() {
        if !runtime.supports_file_mounts() {
            anyhow::bail!(
                "sandbox.container.excluded_files is set but runtime {:?} does \
                 not support file-level bind mounts. Secrets would remain \
                 readable inside the sandbox. Use docker or podman, or remove \
                 sandbox.container.excluded_files.",
                runtime
            );
        }
        for rel in excluded {
            let rel_path = Path::new(rel);
            if rel_path.is_absolute()
                || rel_path
                    .components()
                    .any(|c| matches!(c, Component::ParentDir))
            {
                tracing::warn!(
                    path = %rel,
                    "sandbox.container.excluded_files entry must be a relative path inside the worktree; skipping"
                );
                continue;
            }
            // Mask the path under the current worktree AND, if applicable,
            // under the main worktree (which workmux also bind-mounts for
            // symlink resolution). Without the second mount, a symlinked
            // secret would still be readable via the main-worktree alias.
            let mut candidates = vec![worktree_root.join(rel_path)];
            if let Some(ref main) = main_worktree_path {
                let main_candidate = main.join(rel_path);
                if main_candidate != candidates[0] {
                    candidates.push(main_candidate);
                }
            }
            let mut masked_any = false;
            let mut saw_dir = false;
            for host_path in &candidates {
                if host_path.is_file() {
                    args.push("--mount".to_string());
                    args.push(format!(
                        "type=bind,source=/dev/null,target={},readonly",
                        host_path.display()
                    ));
                    masked_any = true;
                } else if host_path.is_dir() {
                    saw_dir = true;
                }
            }
            if !masked_any {
                if saw_dir {
                    tracing::warn!(
                        path = %rel,
                        "sandbox.container.excluded_files entry is a directory; only regular files can be masked. Skipping."
                    );
                } else {
                    tracing::warn!(
                        path = %rel,
                        "sandbox.container.excluded_files entry does not exist on disk; skipping"
                    );
                }
            }
        }
    }

    // Bind-mount shim directory if host-exec is configured
    if let Some(shim_dir) = shim_host_dir {
        args.push("--mount".to_string());
        args.push(format!(
            "type=bind,source={},target=/tmp/.workmux-shims/bin,readonly",
            shim_dir.display()
        ));
    }

    // Extra mounts from config
    for mount in config.extra_mounts() {
        let (host, guest, read_only) = mount.resolve()?;
        let mut mount_arg = format!(
            "type=bind,source={},target={}",
            host.display(),
            guest.display()
        );
        if read_only {
            mount_arg.push_str(",readonly");
        }
        args.push("--mount".to_string());
        args.push(mount_arg);
    }

    args.push("--workdir".to_string());
    args.push(pane_cwd_str.to_string());

    args.push("--env".to_string());
    args.push("HOME=/tmp".to_string());

    // Codex refuses to create helper binaries when CODEX_HOME is under a
    // temporary directory (i.e. /tmp). Setting CODEX_HOME to a non-temp path
    // avoids this while keeping HOME=/tmp like the other agents.
    if agent == "codex" {
        args.push("--env".to_string());
        args.push("CODEX_HOME=/home/user/.codex".to_string());
    }

    // Agent-specific credential mounts
    // Claude uses ~/.claude-sandbox-config/claude.json for container-specific config.
    // Apple Container only supports directory mounts, so we mount the directory
    // and symlink the file inside the container (see command wrapping below).
    // Docker/Podman can mount the file directly.
    let needs_claude_config_symlink = if agent == "claude"
        && let Some(paths) = SandboxPaths::new()
    {
        if runtime.supports_file_mounts() && paths.config_file.exists() {
            args.push("--mount".to_string());
            args.push(format!(
                "type=bind,source={},target=/tmp/.claude.json",
                paths.config_file.display()
            ));
            false
        } else if !runtime.supports_file_mounts() && paths.config_dir.exists() {
            args.push("--mount".to_string());
            args.push(format!(
                "type=bind,source={},target=/tmp/.claude-sandbox-config",
                paths.config_dir.display()
            ));
            true
        } else {
            false
        }
    } else {
        false
    };

    // Mount agent config directory
    if let Some(config_dir) = config.resolved_agent_config_dir(agent) {
        let target = match agent {
            "claude" => "/tmp/.claude",
            "gemini" => "/tmp/.gemini",
            "codex" => "/home/user/.codex",
            "opencode" => "/tmp/.local/share/opencode",
            "pi" => "/tmp/.pi/agent",
            _ => unreachable!(), // resolved_agent_config_dir returns None for unknown agents
        };
        let _ = std::fs::create_dir_all(&config_dir);
        args.push("--mount".to_string());
        args.push(format!(
            "type=bind,source={},target={}",
            config_dir.display(),
            target
        ));

        // Pi stores managed fd/rg binaries under bin/. Overlay a per-worktree,
        // arch-keyed directory there so the guest's Linux downloads never
        // clobber the host's Mach-O binaries via the parent bind mount. The
        // cache key includes a hash of the canonical worktree path so two
        // different projects with the same basename don't share a cache.
        if agent == "pi" {
            let canonical = worktree_root
                .canonicalize()
                .unwrap_or_else(|_| worktree_root.to_path_buf());
            let basename = worktree_root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            let cache_key = format!(
                "{}-{}",
                slug::slugify(basename),
                crate::sandbox::pi::path_hash(&canonical)
            );
            let state_dir = crate::xdg::state_dir()?.join("container").join(cache_key);
            std::fs::create_dir_all(&state_dir)?;
            let overlay = crate::sandbox::pi::pi_bin_overlay_dir(&state_dir)?;
            args.push("--mount".to_string());
            args.push(format!(
                "type=bind,source={},target=/tmp/.pi/agent/bin",
                overlay.display()
            ));
        }
    }

    // Mount opencode global config directory (~/.config/opencode/) read-only.
    // This is separate from the data directory (~/.local/share/opencode/) and
    // contains opencode.json, plugins, and global MCP definitions.
    if agent == "opencode"
        && let Some(cfg_dir) = crate::agent_setup::opencode::opencode_config_dir()
        && cfg_dir.is_dir()
    {
        let target = "/tmp/.config/opencode";
        args.push("--mount".to_string());
        args.push(format!(
            "type=bind,source={},target={},readonly",
            cfg_dir.display(),
            target
        ));
    }

    // Terminal vars
    for term_var in ["TERM", "COLORTERM"] {
        if std::env::var(term_var).is_ok() {
            args.push("--env".to_string());
            args.push(term_var.to_string());
        }
    }

    // Env passthrough
    for var in config.env_passthrough() {
        if std::env::var(var).is_ok() {
            args.push("--env".to_string());
            args.push(var.to_string());
        }
    }

    // Explicit env vars from config
    for (key, value) in config.env_vars() {
        args.push("--env".to_string());
        args.push(format!("{}={}", key, value));
    }

    // Extra env vars (RPC connection details)
    for (key, value) in extra_envs {
        args.push("--env".to_string());
        args.push(format!("{}={}", key, value));
    }

    // Include $HOME/.local/bin so runtime-installed tools are found (HOME=/tmp).
    // Prepend shim directory when host-exec is configured.
    let sbin = if network_deny { ":/usr/sbin:/sbin" } else { "" };
    let path = if shim_host_dir.is_some() {
        format!("/tmp/.workmux-shims/bin:/tmp/.local/bin:/usr/local/bin:/usr/bin:/bin{sbin}")
    } else {
        format!("/tmp/.local/bin:/usr/local/bin:/usr/bin:/bin{sbin}")
    };
    args.push("--env".to_string());
    args.push(format!("PATH={}", path));

    // Image
    args.push(image.to_string());

    // Command
    // No shell quoting needed -- callers use Command::args() which handles escaping
    //
    // For Apple Container with Claude, we symlink the config file from the
    // mounted directory since Apple Container doesn't support file mounts.
    let wrapped_command = if needs_claude_config_symlink {
        format!(
            "ln -sf /tmp/.claude-sandbox-config/claude.json /tmp/.claude.json; {}",
            command
        )
    } else {
        command.to_string()
    };

    if network_deny {
        // In deny mode, wrap command with network-init.sh which sets up
        // iptables firewall rules and then drops privileges via gosu.
        args.push("network-init.sh".to_string());
        args.push("sh".to_string());
        args.push("-c".to_string());
        args.push(wrapped_command);
    } else {
        args.push("sh".to_string());
        args.push("-c".to_string());
        args.push(wrapped_command);
    }

    Ok(args)
}

/// Docker/Podman run flags specific to network deny mode.
///
/// Returns flags needed to run a container with iptables support: CAP_NET_ADMIN
/// for firewall setup and no-new-privileges to prevent privilege escalation
/// after the init script drops to the target user.
///
/// Used by BOTH the preflight probe and the actual container launch to ensure
/// they always match.
pub fn deny_mode_run_flags() -> Vec<String> {
    vec![
        "--cap-add=NET_ADMIN".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
    ]
}

use crate::shell::shell_escape;

/// Wrap a command to run inside a Docker/Podman container via the sandbox supervisor.
///
/// Generates a `workmux sandbox run` command that starts an RPC server, then
/// runs the command inside a container with RPC connection details as env vars.
pub fn wrap_for_container(
    command: &str,
    _config: &SandboxConfig,
    worktree_root: &Path,
    pane_cwd: &Path,
) -> Result<String> {
    // Strip the single leading space that rewrite_agent_command adds for
    // shell history prevention -- not needed for the supervisor.
    let command = command.strip_prefix(' ').unwrap_or(command);

    let mut parts = format!(
        "workmux sandbox run '{}'",
        shell_escape(&pane_cwd.to_string_lossy()),
    );

    // Only add --worktree-root when it differs from pane_cwd
    if worktree_root != pane_cwd {
        parts.push_str(&format!(
            " --worktree-root '{}'",
            shell_escape(&worktree_root.to_string_lossy()),
        ));
    }

    parts.push_str(&format!(" -- '{}'", shell_escape(command)));

    // Prefix with space to prevent shell history entry (same as rewrite_agent_command)
    Ok(format!(" {}", parts))
}

/// Stop any running containers associated with a worktree handle.
///
/// Uses the state store to find registered containers instead of running
/// `docker ps`. This avoids spawning docker commands for users who don't
/// use containers.
pub fn stop_containers_for_handle(handle: &str) {
    // Check state store for registered containers
    let store = match StateStore::new() {
        Ok(s) => s,
        Err(_) => return,
    };

    let containers = store.list_containers(handle);
    if containers.is_empty() {
        return;
    }

    tracing::debug!(?containers, handle, "stopping containers for worktree");

    // Group containers by runtime so we issue separate stop commands per binary
    let mut by_runtime: std::collections::HashMap<SandboxRuntime, Vec<String>> =
        std::collections::HashMap::new();
    for (name, runtime) in &containers {
        by_runtime.entry(*runtime).or_default().push(name.clone());
    }

    for (runtime, names) in &by_runtime {
        let _ = Command::new(runtime.binary_name())
            .arg("stop")
            .arg("-t")
            .arg("0")
            .args(names)
            .output();
    }

    // Unregister containers from state store
    for (name, _) in containers {
        store.unregister_container(handle, &name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ContainerConfig, ContainerDevice, SandboxConfig, SandboxRuntime};

    fn make_config() -> SandboxConfig {
        SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            env_passthrough: Some(vec!["TEST_KEY".to_string()]),
            ..Default::default()
        }
    }

    #[test]
    fn test_build_args_basic() {
        let config = make_config();
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        assert!(args.contains(&"run".to_string()));
        assert!(args.contains(&"--rm".to_string()));
        assert!(args.contains(&"-it".to_string()));
        assert!(args.contains(&"test-image:latest".to_string()));
        assert!(args.contains(&"sh".to_string()));
        assert!(args.contains(&"-c".to_string()));
        assert!(args.contains(&"claude".to_string()));
    }

    #[test]
    fn test_excluded_files_default_empty() {
        let config = make_config();
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        assert!(
            !args.iter().any(|a| a.contains("source=/dev/null")),
            "no /dev/null mounts should be added when excluded_files is unset"
        );
    }

    #[test]
    fn test_excluded_files_masks_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "SECRET=1").unwrap();

        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                excluded_files: Some(vec![".env".to_string()]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };

        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            tmp.path(),
            tmp.path(),
            &[],
            None,
            false,
        )
        .unwrap();

        let env_abs = tmp.path().join(".env");
        let expected = format!(
            "type=bind,source=/dev/null,target={},readonly",
            env_abs.display()
        );
        assert!(
            args.contains(&expected),
            "expected /dev/null mount for .env, got: {:?}",
            args
        );
    }

    #[test]
    fn test_excluded_files_skips_missing() {
        let tmp = tempfile::tempdir().unwrap();

        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                excluded_files: Some(vec![".env".to_string()]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };

        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            tmp.path(),
            tmp.path(),
            &[],
            None,
            false,
        )
        .unwrap();

        assert!(
            !args.iter().any(|a| a.contains("source=/dev/null")),
            "nonexistent excluded files should be skipped, not mounted"
        );
    }

    #[test]
    fn test_excluded_files_errors_on_apple_container() {
        // Apple Container cannot honor excluded_files (no file-level mounts).
        // Silently skipping would leave secrets readable inside the sandbox
        // without the user noticing, so we hard-fail instead.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "SECRET=1").unwrap();

        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::AppleContainer),
                excluded_files: Some(vec![".env".to_string()]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };

        let err = build_docker_run_args(
            "claude",
            &config,
            "claude",
            tmp.path(),
            tmp.path(),
            &[],
            None,
            false,
        )
        .expect_err("expected hard error when excluded_files is set on apple-container");

        let msg = format!("{err}");
        assert!(
            msg.contains("excluded_files"),
            "error message should mention excluded_files, got: {msg}"
        );
    }

    #[test]
    fn test_excluded_files_masks_main_worktree_alias() {
        // When the current worktree has a `.git` gitlink pointing into a main
        // repo's worktrees/<name>/ directory, workmux bind-mounts both the
        // current worktree and the main worktree. A secret reachable via the
        // main-worktree mount (e.g. a symlink from current worktree -> main)
        // must be masked on both paths.
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let wt = tmp.path().join("wt1");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&wt).unwrap();

        // Build a plausible main/.git/worktrees/wt1 layout.
        let main_git = main.join(".git");
        let wt1_git_dir = main_git.join("worktrees").join("wt1");
        std::fs::create_dir_all(&wt1_git_dir).unwrap();

        // Current worktree's .git is a gitlink file pointing at the main
        // repo's worktree dir, matching real git behavior.
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt1_git_dir.display()),
        )
        .unwrap();

        // Secret lives only in the main worktree.
        std::fs::write(main.join(".env"), "SECRET=1").unwrap();

        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                excluded_files: Some(vec![".env".to_string()]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };

        let args =
            build_docker_run_args("claude", &config, "claude", &wt, &wt, &[], None, false).unwrap();

        let main_env = main.join(".env");
        let expected_main = format!(
            "type=bind,source=/dev/null,target={},readonly",
            main_env.display()
        );
        assert!(
            args.contains(&expected_main),
            "expected main-worktree alias {} to be masked, got: {:?}",
            main_env.display(),
            args
        );
    }

    #[test]
    fn test_excluded_files_masks_main_worktree_alias_with_relative_gitdir() {
        // `git worktree add --relative-paths` (git 2.48+) writes a `.git` file
        // with a RELATIVE `gitdir:` pointer. Workmux must resolve it against
        // the worktree root; otherwise the main-worktree mount and the alias
        // masking would be emitted with relative `--mount` paths.
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let wt = tmp.path().join("wt1");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        let wt1_git_dir = main.join(".git").join("worktrees").join("wt1");
        std::fs::create_dir_all(&wt1_git_dir).unwrap();

        // Mirror git's output under --relative-paths exactly.
        std::fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt1\n").unwrap();

        std::fs::write(main.join(".env"), "SECRET=1").unwrap();

        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                excluded_files: Some(vec![".env".to_string()]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };

        let args =
            build_docker_run_args("claude", &config, "claude", &wt, &wt, &[], None, false).unwrap();

        // The joined path preserves `..`, but critically it is absolute
        // (anchored at the worktree root) so Docker can resolve it.
        let resolved_main_env = wt.join("../main/.env");
        let expected = format!(
            "type=bind,source=/dev/null,target={},readonly",
            resolved_main_env.display()
        );
        assert!(
            args.contains(&expected),
            "expected main-worktree alias masked at absolute path, got: {:?}",
            args
        );

        // Regression: no `--mount` arg must start with a relative path.
        let mount_args: Vec<&String> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                if i > 0 && args[i - 1] == "--mount" {
                    Some(a)
                } else {
                    None
                }
            })
            .collect();
        for m in &mount_args {
            for kv in m.split(',') {
                if let Some(v) = kv
                    .strip_prefix("source=")
                    .or_else(|| kv.strip_prefix("target="))
                {
                    assert!(
                        v.starts_with('/'),
                        "mount spec has non-absolute path in {kv:?} (full: {m})"
                    );
                }
            }
        }
    }

    #[test]
    fn test_excluded_files_directory_warns_not_missing() {
        // An entry that is a directory on disk must not be reported as
        // "does not exist on disk" -- that would mislead users into thinking
        // they had a typo. Behavior: no mount emitted, and (verified by
        // inspection) the dedicated directory warning is chosen.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".aws")).unwrap();

        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                excluded_files: Some(vec![".aws".to_string()]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };

        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            tmp.path(),
            tmp.path(),
            &[],
            None,
            false,
        )
        .unwrap();

        assert!(
            !args.iter().any(|a| a.contains("source=/dev/null")),
            "directories must not produce /dev/null mounts, got: {:?}",
            args
        );
    }

    #[test]
    fn test_excluded_files_allows_safe_dotted_names() {
        // Paths like "foo..bar" and "my..env" contain ".." but are NOT parent
        // traversal components, so they must be accepted.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("my..env"), "SECRET=1").unwrap();
        std::fs::write(tmp.path().join("foo..bar"), "SECRET=2").unwrap();

        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                excluded_files: Some(vec!["my..env".to_string(), "foo..bar".to_string()]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };

        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            tmp.path(),
            tmp.path(),
            &[],
            None,
            false,
        )
        .unwrap();

        let my_env = tmp.path().join("my..env");
        let foo_bar = tmp.path().join("foo..bar");
        assert!(
            args.iter().any(|a| a.contains(&format!(
                "source=/dev/null,target={},readonly",
                my_env.display()
            ))),
            "my..env should be masked: {:?}",
            args
        );
        assert!(
            args.iter().any(|a| a.contains(&format!(
                "source=/dev/null,target={},readonly",
                foo_bar.display()
            ))),
            "foo..bar should be masked: {:?}",
            args
        );
    }

    #[test]
    fn test_excluded_files_rejects_escape_paths() {
        let tmp = tempfile::tempdir().unwrap();
        // Create the target of the attempted escape so is_file() would succeed
        // if the path-safety check weren't applied.
        let outside = tmp.path().parent().unwrap().join("outside-secret");
        let _ = std::fs::write(&outside, "SECRET=1");

        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                excluded_files: Some(vec![
                    "../outside-secret".to_string(),
                    "/etc/passwd".to_string(),
                ]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };

        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            tmp.path(),
            tmp.path(),
            &[],
            None,
            false,
        )
        .unwrap();

        let _ = std::fs::remove_file(&outside);

        assert!(
            !args.iter().any(|a| a.contains("source=/dev/null")),
            "paths escaping the worktree must not produce mounts: {:?}",
            args
        );
    }

    #[test]
    fn test_build_args_extra_envs() {
        let config = make_config();
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[("WM_SANDBOX_GUEST", "1"), ("WM_RPC_PORT", "12345")],
            None,
            false,
        )
        .unwrap();

        assert!(args.contains(&"WM_SANDBOX_GUEST=1".to_string()));
        assert!(args.contains(&"WM_RPC_PORT=12345".to_string()));
    }

    #[test]
    fn test_build_args_docker_includes_add_host() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        assert!(args.contains(&"--add-host".to_string()));
        assert!(args.contains(&"host.docker.internal:host-gateway".to_string()));
    }

    #[test]
    fn test_build_args_podman_omits_add_host() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Podman),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        assert!(!args.contains(&"--add-host".to_string()));
    }

    #[test]
    fn test_build_args_runtime_not_in_args() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Podman),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        assert!(!args.contains(&"podman".to_string()));
        assert!(!args.contains(&"docker".to_string()));
    }

    #[test]
    fn test_wrap_generates_supervisor_command() {
        let config = make_config();
        let result = wrap_for_container(
            "claude",
            &config,
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
        )
        .unwrap();

        assert!(result.starts_with(" workmux sandbox run"));
        assert!(result.contains("'/tmp/project'"));
        assert!(result.contains("-- 'claude'"));
        // Should NOT contain --worktree-root when paths are equal
        assert!(!result.contains("--worktree-root"));
    }

    #[test]
    fn test_wrap_escapes_quotes_in_command() {
        let config = make_config();
        let result = wrap_for_container(
            "echo 'hello'",
            &config,
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
        )
        .unwrap();

        assert!(result.contains("echo '\\''hello'\\''"));
    }

    #[test]
    fn test_wrap_strips_leading_space() {
        let config = make_config();
        let result = wrap_for_container(
            " claude -- \"$(cat PROMPT.md)\"",
            &config,
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
        )
        .unwrap();

        assert!(result.contains("-- 'claude -- \"$(cat PROMPT.md)\"'"));
    }

    #[test]
    fn test_wrap_with_different_worktree_root() {
        let config = make_config();
        let result = wrap_for_container(
            "claude",
            &config,
            Path::new("/tmp/project"),
            Path::new("/tmp/project/backend"),
        )
        .unwrap();

        assert!(result.contains("--worktree-root '/tmp/project'"));
        assert!(result.contains("'/tmp/project/backend'"));
    }

    #[test]
    fn test_build_args_with_shims() {
        let config = make_config();
        let tmp = tempfile::tempdir().unwrap();
        let shim_bin = tmp.path().join("shims/bin");
        std::fs::create_dir_all(&shim_bin).unwrap();

        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            Some(&shim_bin),
            false,
        )
        .unwrap();

        let args_str = args.join(" ");
        // Shim dir should be bind-mounted
        assert!(args_str.contains(".workmux-shims/bin"));
        // PATH should include shim dir first
        let path_arg = args.iter().find(|a| a.starts_with("PATH=")).unwrap();
        assert!(path_arg.starts_with("PATH=/tmp/.workmux-shims/bin:"));
    }

    #[test]
    fn test_dockerfile_for_known_agents() {
        assert!(dockerfile_for_agent("claude").is_some());
        assert!(dockerfile_for_agent("codex").is_some());
        assert!(dockerfile_for_agent("gemini").is_some());
        assert!(dockerfile_for_agent("opencode").is_some());
        assert!(dockerfile_for_agent("pi").is_some());
    }

    #[test]
    fn test_dockerfile_for_unknown_agent() {
        assert!(dockerfile_for_agent("unknown").is_none());
        assert!(dockerfile_for_agent("default").is_none());
    }

    #[test]
    fn test_default_image_resolution() {
        let config = SandboxConfig::default();
        assert_eq!(
            config.resolved_image("claude"),
            "ghcr.io/raine/workmux-sandbox:claude"
        );
        assert_eq!(
            config.resolved_image("codex"),
            "ghcr.io/raine/workmux-sandbox:codex"
        );
    }

    #[test]
    fn test_custom_image_resolution() {
        let config = SandboxConfig {
            image: Some("my-image:latest".to_string()),
            ..Default::default()
        };
        assert_eq!(config.resolved_image("claude"), "my-image:latest");
    }

    #[test]
    fn test_build_args_extra_mounts_readonly() {
        use crate::config::ExtraMount;

        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            extra_mounts: Some(vec![ExtraMount::Path("/tmp/notes".to_string())]),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let args_str = args.join(" ");
        assert!(args_str.contains("type=bind,source=/tmp/notes,target=/tmp/notes,readonly"));
    }

    #[test]
    fn test_build_args_extra_mounts_writable_with_guest_path() {
        use crate::config::ExtraMount;

        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            extra_mounts: Some(vec![ExtraMount::Spec {
                host_path: "/tmp/data".to_string(),
                guest_path: Some("/mnt/data".to_string()),
                writable: Some(true),
            }]),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let args_str = args.join(" ");
        assert!(args_str.contains("type=bind,source=/tmp/data,target=/mnt/data"));
        // Should NOT contain readonly
        assert!(!args_str.contains("/tmp/data,target=/mnt/data,readonly"));
    }

    #[test]
    fn test_build_args_gemini_agent_credential_mount() {
        let config = make_config();
        let args = build_docker_run_args(
            "gemini",
            &config,
            "gemini",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let args_str = args.join(" ");
        // Gemini agent should mount ~/.gemini to /tmp/.gemini
        assert!(args_str.contains("target=/tmp/.gemini"));
        // Gemini agent should NOT have Claude-specific mounts
        assert!(!args_str.contains("target=/tmp/.claude.json"));
        assert!(!args_str.contains("target=/tmp/.claude,"));
        assert!(!args_str.contains("/home/user/.codex"));
    }

    #[test]
    fn test_build_args_codex_agent_credential_mount() {
        let config = make_config();
        let args = build_docker_run_args(
            "codex",
            &config,
            "codex",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let args_str = args.join(" ");
        // Codex agent should mount ~/.codex to /home/user/.codex (matches CODEX_HOME)
        assert!(args_str.contains("target=/home/user/.codex"));
        // CODEX_HOME set to avoid "Refusing to create helper binaries under temporary dir" warning
        assert!(args_str.contains("CODEX_HOME=/home/user/.codex"));
        // Codex agent should NOT have Claude-specific mounts
        assert!(!args_str.contains("target=/tmp/.claude.json"));
        assert!(!args_str.contains("target=/tmp/.gemini"));
    }

    #[test]
    fn test_build_args_opencode_agent_credential_mount() {
        let config = make_config();
        let args = build_docker_run_args(
            "opencode",
            &config,
            "opencode",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let args_str = args.join(" ");
        // OpenCode agent should mount ~/.local/share/opencode to /tmp/.local/share/opencode
        assert!(args_str.contains("target=/tmp/.local/share/opencode"));
        // OpenCode agent should NOT have Claude-specific mounts
        assert!(!args_str.contains("target=/tmp/.claude.json"));
        assert!(!args_str.contains("target=/tmp/.gemini"));
    }

    #[test]
    fn test_build_args_unknown_agent_no_credential_mount() {
        let config = make_config();
        let args = build_docker_run_args(
            "unknown-agent",
            &config,
            "unknown-agent",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let args_str = args.join(" ");
        // Unknown agent should NOT have any agent credential mounts
        assert!(!args_str.contains("target=/tmp/.claude"));
        assert!(!args_str.contains("target=/tmp/.gemini"));
        assert!(!args_str.contains("/home/user/.codex"));
        assert!(!args_str.contains("target=/tmp/.local/share/opencode"));
    }

    #[test]
    fn test_build_args_custom_agent_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join("claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let config = SandboxConfig {
            agent_config_dir: Some(tmp.path().join("{agent}").to_string_lossy().to_string()),
            ..make_config()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let args_str = args.join(" ");
        assert!(args_str.contains(&format!(
            "type=bind,source={},target=/tmp/.claude",
            claude_dir.display()
        )));
    }

    // --- Network deny mode tests ---

    #[test]
    fn test_build_args_network_deny_has_cap_net_admin() {
        let config = make_config();
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            true, // network_deny
        )
        .unwrap();

        assert!(args.contains(&"--cap-add=NET_ADMIN".to_string()));
        assert!(args.contains(&"--security-opt".to_string()));
        assert!(args.contains(&"no-new-privileges".to_string()));
    }

    #[test]
    fn test_build_args_network_deny_no_user_flag() {
        let config = make_config();
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            true,
        )
        .unwrap();

        // Deny mode should NOT have --user (container starts as root)
        assert!(!args.contains(&"--user".to_string()));
    }

    #[test]
    fn test_build_args_network_deny_has_target_uid_gid() {
        let config = make_config();
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            true,
        )
        .unwrap();

        let args_str = args.join(" ");
        assert!(args_str.contains("WM_TARGET_UID="));
        assert!(args_str.contains("WM_TARGET_GID="));
    }

    #[test]
    fn test_build_args_network_deny_wraps_with_network_init() {
        let config = make_config();
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            true,
        )
        .unwrap();

        // Command should be: image network-init.sh sh -c <command>
        let image_idx = args.iter().position(|a| a == "test-image:latest").unwrap();
        assert_eq!(args[image_idx + 1], "network-init.sh");
        assert_eq!(args[image_idx + 2], "sh");
        assert_eq!(args[image_idx + 3], "-c");
        assert_eq!(args[image_idx + 4], "claude");
    }

    #[test]
    fn test_build_args_network_deny_path_includes_sbin() {
        let config = make_config();
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            true,
        )
        .unwrap();

        let path_arg = args.iter().find(|a| a.starts_with("PATH=")).unwrap();
        assert!(
            path_arg.contains("/usr/sbin"),
            "deny mode PATH must include /usr/sbin for iptables: {}",
            path_arg
        );
    }

    #[test]
    fn test_build_args_allow_mode_path_no_sbin() {
        let config = make_config();
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let path_arg = args.iter().find(|a| a.starts_with("PATH=")).unwrap();
        assert!(
            !path_arg.contains("/usr/sbin"),
            "allow mode PATH should not include /usr/sbin: {}",
            path_arg
        );
    }

    #[test]
    fn test_build_args_network_deny_podman_no_keep_id() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Podman),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            true,
        )
        .unwrap();

        // Deny mode should NOT use --userns=keep-id
        assert!(!args.contains(&"--userns=keep-id".to_string()));
    }

    #[test]
    fn test_build_args_allow_mode_no_cap_net_admin() {
        let config = make_config();
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        // Allow mode should have --user and no --cap-add
        assert!(args.contains(&"--user".to_string()));
        assert!(!args.contains(&"--cap-add=NET_ADMIN".to_string()));
        // Command should not include network-init.sh
        let image_idx = args.iter().position(|a| a == "test-image:latest").unwrap();
        assert_eq!(args[image_idx + 1], "sh");
    }

    #[test]
    fn test_deny_mode_run_flags() {
        let flags = deny_mode_run_flags();
        assert!(flags.contains(&"--cap-add=NET_ADMIN".to_string()));
        assert!(flags.contains(&"--security-opt".to_string()));
        assert!(flags.contains(&"no-new-privileges".to_string()));
    }

    #[test]
    fn test_build_args_apple_container_omits_docker_podman_flags() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::AppleContainer),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        // Should NOT have Docker's --add-host
        assert!(!args.contains(&"--add-host".to_string()));
        // Should NOT have Podman's --userns=keep-id
        assert!(!args.contains(&"--userns=keep-id".to_string()));
    }

    #[test]
    fn test_build_args_apple_container_deny_mode_skips_caps() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::AppleContainer),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            true, // network_deny
        )
        .unwrap();

        // Should NOT have --cap-add=NET_ADMIN or --security-opt
        assert!(!args.contains(&"--cap-add=NET_ADMIN".to_string()));
        assert!(!args.contains(&"--security-opt".to_string()));
        // Should still have UID/GID env vars for deny mode
        assert!(args.iter().any(|a| a.starts_with("WM_TARGET_UID=")));
        assert!(args.iter().any(|a| a.starts_with("WM_TARGET_GID=")));
    }

    #[test]
    fn test_build_args_apple_container_default_memory() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::AppleContainer),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        // Apple Container should get --memory 16G by default
        let mem_idx = args.iter().position(|a| a == "--memory").unwrap();
        assert_eq!(args[mem_idx + 1], "16G");
        // No --cpus unless explicitly configured
        assert!(!args.contains(&"--cpus".to_string()));
    }

    #[test]
    fn test_build_args_apple_container_custom_resources() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::AppleContainer),
                memory: Some("8G".to_string()),
                cpus: Some(8),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let mem_idx = args.iter().position(|a| a == "--memory").unwrap();
        assert_eq!(args[mem_idx + 1], "8G");
        let cpu_idx = args.iter().position(|a| a == "--cpus").unwrap();
        assert_eq!(args[cpu_idx + 1], "8");
    }

    #[test]
    fn test_build_args_docker_no_default_resource_flags() {
        let config = make_config();
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        // Docker should NOT get --memory or --cpus by default
        assert!(!args.contains(&"--memory".to_string()));
        assert!(!args.contains(&"--cpus".to_string()));
    }

    #[test]
    fn test_build_args_docker_explicit_memory() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                memory: Some("4G".to_string()),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        // Explicit memory should be passed even for Docker
        let mem_idx = args.iter().position(|a| a == "--memory").unwrap();
        assert_eq!(args[mem_idx + 1], "4G");
    }

    fn find_flag_value<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].as_str())
            .collect()
    }

    #[test]
    fn docker_emits_device_flags() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                devices: Some(vec![
                    ContainerDevice::String("/dev/kvm".to_string()),
                    ContainerDevice::String("/dev/dri:/dev/dri:rwm".to_string()),
                ]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let devs = find_flag_value(&args, "--device");
        assert!(devs.contains(&"/dev/kvm"));
        assert!(devs.contains(&"/dev/dri:/dev/dri:rwm"));
    }

    #[test]
    fn docker_allow_mode_emits_group_add() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                group_add: Some(vec!["dialout".to_string(), "video".to_string()]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let groups = find_flag_value(&args, "--group-add");
        assert!(groups.contains(&"dialout"));
        assert!(groups.contains(&"video"));
        assert!(!args.iter().any(|a| a.starts_with("WM_EXTRA_GIDS=")));
    }

    #[test]
    fn docker_deny_mode_uses_wm_extra_gids_not_group_add() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                group_add: Some(vec!["dialout".to_string(), "20".to_string()]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            true,
        )
        .unwrap();

        assert!(!args.iter().any(|a| a == "--group-add"));
        assert!(args.iter().any(|a| a == "WM_EXTRA_GIDS=dialout,20"));
    }

    #[test]
    fn docker_deny_mode_still_emits_device_flags() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                devices: Some(vec![ContainerDevice::String("/dev/kvm".to_string())]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            true,
        )
        .unwrap();

        let devs = find_flag_value(&args, "--device");
        assert!(devs.contains(&"/dev/kvm"));
    }

    #[test]
    fn apple_container_rejects_devices() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::AppleContainer),
                devices: Some(vec![ContainerDevice::String("/dev/kvm".to_string())]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let result = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn apple_container_rejects_group_add() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::AppleContainer),
                group_add: Some(vec!["dialout".to_string()]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let result = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn podman_allow_mode_supports_devices_and_group_add() {
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Podman),
                devices: Some(vec![ContainerDevice::String("/dev/kvm".to_string())]),
                group_add: Some(vec!["dialout".to_string()]),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let devs = find_flag_value(&args, "--device");
        assert!(devs.contains(&"/dev/kvm"));
        let groups = find_flag_value(&args, "--group-add");
        assert!(groups.contains(&"dialout"));
    }

    #[test]
    fn test_build_args_pi_agent_apple_container_mounts_config_dir() {
        use crate::config::{ContainerConfig, SandboxConfig, SandboxRuntime};
        let config = SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::AppleContainer),
                ..Default::default()
            },
            ..Default::default()
        };
        let args = build_docker_run_args(
            "pi",
            &config,
            "pi",
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap();

        let args_str = args.join(" ");
        // Pi agent should mount ~/.pi/agent to /tmp/.pi/agent
        assert!(
            args_str.contains("/tmp/.pi/agent"),
            "pi agent config mount missing: {}",
            args_str
        );
        // Should NOT have Claude-specific mounts
        assert!(
            !args_str.contains("/tmp/.claude.json"),
            "no claude mount expected for pi"
        );
        assert!(!args_str.contains("/tmp/.claude,"));
    }

    #[test]
    fn test_build_args_pi_agent_overlays_bin_after_parent() {
        use crate::config::SandboxConfig;
        let config = SandboxConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "pi",
            &config,
            "pi",
            Path::new("/tmp/myproject"),
            Path::new("/tmp/myproject"),
            &[],
            None,
            false,
        )
        .unwrap();

        // Find indices of the parent and bin mount entries.
        let parent_idx = args
            .iter()
            .position(|a| a.contains("target=/tmp/.pi/agent") && !a.contains("/tmp/.pi/agent/bin"))
            .expect("parent /tmp/.pi/agent mount missing");
        let bin_idx = args
            .iter()
            .position(|a| a.contains("target=/tmp/.pi/agent/bin"))
            .expect("bin overlay /tmp/.pi/agent/bin mount missing");
        assert!(bin_idx > parent_idx, "bin overlay must come after parent");

        let bin_arg = &args[bin_idx];
        assert!(
            bin_arg.contains("pi-agent-bin"),
            "bin overlay source should contain pi-agent-bin: {}",
            bin_arg
        );
        assert!(
            bin_arg.contains(crate::sandbox::pi::linux_arch_key()),
            "bin overlay source should contain arch key: {}",
            bin_arg
        );
        // Per-worktree-handle, NOT container_name (which contains the PID).
        assert!(
            !bin_arg.contains(&format!("-{}", std::process::id())),
            "bin overlay path must not contain PID: {}",
            bin_arg
        );
        assert!(
            bin_arg.contains("myproject"),
            "bin overlay path should contain worktree handle: {}",
            bin_arg
        );
    }

    #[test]
    fn test_build_args_non_pi_agent_has_no_bin_overlay() {
        use crate::config::SandboxConfig;
        let config = SandboxConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "claude",
            &config,
            "claude",
            Path::new("/tmp/myproject"),
            Path::new("/tmp/myproject"),
            &[],
            None,
            false,
        )
        .unwrap();

        let args_str = args.join(" ");
        assert!(
            !args_str.contains("pi-agent-bin"),
            "claude agent should not have pi bin overlay"
        );
    }
}
