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
pub const DOCKERFILE_OMP: &str = include_str!("../../docker/Dockerfile.omp");

/// Known agents that have pre-built images.
pub const KNOWN_AGENTS: &[&str] = &["claude", "codex", "gemini", "opencode", "pi", "omp"];

/// Get the agent-specific Dockerfile content, or None for unknown agents.
pub fn dockerfile_for_agent(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some(DOCKERFILE_CLAUDE),
        "codex" => Some(DOCKERFILE_CODEX),
        "gemini" => Some(DOCKERFILE_GEMINI),
        "opencode" => Some(DOCKERFILE_OPENCODE),
        "pi" => Some(DOCKERFILE_PI),
        "omp" => Some(DOCKERFILE_OMP),
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

fn apple_container_supports_read_only_path(help: &str) -> bool {
    help.contains("--read-only-path")
}

/// Validate that the selected runtime can enforce the Git metadata mount boundary.
pub fn validate_git_metadata_boundary(runtime: SandboxRuntime) -> Result<()> {
    if runtime != SandboxRuntime::AppleContainer {
        return Ok(());
    }
    let output = Command::new(runtime.binary_name())
        .args(["run", "--help"])
        .output()
        .context("Failed to inspect Apple Container capabilities")?;
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() || !apple_container_supports_read_only_path(&help) {
        anyhow::bail!(
            "This Apple Container build lacks --read-only-path, which Workmux requires to protect host Git control files. Upgrade Apple Container or use Docker or Podman."
        );
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
    build_docker_run_args_inner(
        command,
        config,
        agent,
        worktree_root,
        pane_cwd,
        extra_envs,
        shim_host_dir,
        network_deny,
        None,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn build_docker_run_args_with_state_dir(
    command: &str,
    config: &SandboxConfig,
    agent: &str,
    worktree_root: &Path,
    pane_cwd: &Path,
    extra_envs: &[(&str, &str)],
    shim_host_dir: Option<&Path>,
    network_deny: bool,
    state_root: &Path,
) -> Result<Vec<String>> {
    build_docker_run_args_inner(
        command,
        config,
        agent,
        worktree_root,
        pane_cwd,
        extra_envs,
        shim_host_dir,
        network_deny,
        Some(state_root),
    )
}

fn push_bind_path(args: &mut Vec<String>, path: &Path, read_only: bool) {
    args.push("--mount".to_string());
    let mut mount = format!(
        "type=bind,source={},target={}",
        path.display(),
        path.display()
    );
    if read_only {
        mount.push_str(",readonly");
    }
    args.push(mount);
}

fn push_read_only_path(args: &mut Vec<String>, runtime: SandboxRuntime, path: &Path) {
    if runtime == SandboxRuntime::AppleContainer {
        args.push("--read-only-path".to_string());
        args.push(path.to_string_lossy().into_owned());
    } else {
        push_bind_path(args, path, true);
    }
}

fn git_private_state_dir(worktree: &Path, state_root: Option<&Path>) -> Result<PathBuf> {
    let canonical = worktree.canonicalize()?;
    let key = crate::sandbox::pi::path_hash(&canonical);
    let root = match state_root {
        Some(root) => root.to_path_buf(),
        None => crate::xdg::state_dir()?.join("container"),
    };
    let path = root.join(format!("git-{key}"));
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

fn collect_worktree_git_pointers(worktree: &Path) -> Result<Vec<PathBuf>> {
    fn visit(dir: &Path, pointers: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if path.file_name() == Some(std::ffi::OsStr::new(".git")) {
                if metadata.is_file() {
                    pointers.push(path);
                }
                continue;
            }
            if metadata.is_dir() {
                visit(&path, pointers)?;
            }
        }
        Ok(())
    }

    let mut pointers = Vec::new();
    visit(worktree, &mut pointers)?;
    Ok(pointers)
}

fn config_include_paths(config: &Path) -> Result<Vec<PathBuf>> {
    fn visit(
        config: &Path,
        visited: &mut std::collections::HashSet<PathBuf>,
        paths: &mut Vec<PathBuf>,
    ) -> Result<()> {
        let config = config.canonicalize()?;
        if !visited.insert(config.clone()) {
            return Ok(());
        }
        let output = crate::git::unattended_git(None)?
            .args([
                "config",
                "--file",
                config.to_string_lossy().as_ref(),
                "--get-regexp",
                "^include.*\\.path$",
            ])
            .output()?;
        if !output.status.success() && output.status.code() != Some(1) {
            anyhow::bail!(
                "Failed to inspect Git config includes in {}",
                config.display()
            );
        }
        for line in String::from_utf8(output.stdout)?.lines() {
            let Some((_, value)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            if value.contains(['*', '?', '[', '%']) {
                anyhow::bail!("Git config include path cannot be protected safely: {value}");
            }
            let include = if let Some(rest) = value.strip_prefix("~/") {
                home::home_dir()
                    .context("Could not resolve home directory for Git config include")?
                    .join(rest)
            } else {
                let path = Path::new(value);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    config.parent().unwrap_or(Path::new("/")).join(path)
                }
            };
            if !include.is_file() {
                anyhow::bail!(
                    "Git config include target must exist before sandbox startup: {}",
                    include.display()
                );
            }
            paths.push(include.clone());
            visit(&include, visited, paths)?;
        }

        let executable_output = crate::git::unattended_git(None)?
            .args([
                "config",
                "--file",
                config.to_string_lossy().as_ref(),
                "--get-regexp",
                "^(core\\.(hooksPath|fsmonitor)|diff\\..*\\.(command|textconv)|filter\\..*\\.(clean|smudge|process)|merge\\..*\\.driver)$",
            ])
            .output()?;
        if !executable_output.status.success() && executable_output.status.code() != Some(1) {
            anyhow::bail!(
                "Failed to inspect executable Git config in {}",
                config.display()
            );
        }
        for line in String::from_utf8(executable_output.stdout)?.lines() {
            let Some((key, value)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            if key.eq_ignore_ascii_case("core.fsmonitor") && matches!(value, "true" | "false") {
                continue;
            }
            let token = value
                .trim_start_matches('!')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(['\'', '"']);
            if token.is_empty() || token.contains(['$', '`', '%']) {
                continue;
            }
            let candidate = Path::new(token);
            let candidate = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                config.parent().unwrap_or(Path::new("/")).join(candidate)
            };
            if candidate.exists() {
                paths.push(candidate);
            }
        }
        Ok(())
    }

    let mut paths = Vec::new();
    visit(config, &mut std::collections::HashSet::new(), &mut paths)?;
    Ok(paths)
}

fn ensure_policy_file(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!("Git policy path must be a regular file: {}", path.display());
        }
        #[cfg(unix)]
        if metadata.nlink() > 1 {
            anyhow::bail!(
                "Git policy files must not have hard links: {}",
                path.display()
            );
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, b"")?;
    Ok(())
}

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Reject hook layouts whose executable policy can be changed outside the protected directory.
fn validate_git_hooks(common_dir: &Path) -> Result<()> {
    let hooks = common_dir.join("hooks");
    if hooks.exists() {
        let metadata = std::fs::symlink_metadata(&hooks)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!("Git hooks path must be a directory: {}", hooks.display());
        }
        for entry in std::fs::read_dir(&hooks)? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                anyhow::bail!("Git hooks must not be symbolic links: {}", path.display());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.is_file() && metadata.nlink() > 1 {
                    anyhow::bail!("Git hooks must not have hard links: {}", path.display());
                }
            }
        }
    }
    Ok(())
}

fn protect_writable_git_root(
    args: &mut Vec<String>,
    runtime: SandboxRuntime,
    root: &Path,
    full_git_dir: bool,
    private: &Path,
    config_index: &mut usize,
) -> Result<()> {
    if full_git_dir {
        for directory in ["hooks", "info", "objects/info", "modules", "worktrees"] {
            let path = root.join(directory);
            std::fs::create_dir_all(&path)?;
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                anyhow::bail!("Git policy path must be a directory: {}", path.display());
            }
            push_read_only_path(args, runtime, &path);
        }
    }

    let names: &[&str] = if full_git_dir {
        &["config", "config.worktree", "gitdir", "commondir"]
    } else {
        &["config.worktree", "gitdir", "commondir"]
    };
    for name in names {
        let path = root.join(name);
        if *name == "config" || *name == "config.worktree" {
            ensure_policy_file(&path)?;
            if runtime != SandboxRuntime::AppleContainer {
                let snapshot = private.join(format!("config-{}", *config_index));
                *config_index += 1;
                crate::git::snapshot_local_config(&path, &snapshot)?;
                push_bind_path(args, &snapshot, true);
                let last = args.last_mut().expect("mount argument");
                *last = last.replace(
                    &format!("target={}", snapshot.display()),
                    &format!("target={}", path.display()),
                );
            } else {
                push_read_only_path(args, runtime, &path);
            }
        } else if path.exists() {
            ensure_policy_file(&path)?;
            push_read_only_path(args, runtime, &path);
        }
    }
    Ok(())
}

fn module_git_roots(common_dir: &Path) -> Result<Vec<PathBuf>> {
    fn visit(path: &Path, roots: &mut Vec<PathBuf>) -> Result<()> {
        if !path.is_dir() {
            return Ok(());
        }
        let is_root = path.join("config").is_file()
            || (path.join("HEAD").is_file() && path.join("objects").is_dir());
        if is_root {
            roots.push(path.to_path_buf());
        }
        let children = if is_root {
            path.join("modules")
        } else {
            path.to_path_buf()
        };
        if children.is_dir() {
            for entry in std::fs::read_dir(children)? {
                let child = entry?.path();
                if child.is_dir() {
                    visit(&child, roots)?;
                }
            }
        }
        Ok(())
    }
    let mut roots = Vec::new();
    let modules = common_dir.join("modules");
    if modules.is_dir() {
        for entry in std::fs::read_dir(modules)? {
            let child = entry?.path();
            if child.is_dir() {
                visit(&child, &mut roots)?;
            }
        }
    }
    Ok(roots)
}

fn add_git_metadata_boundary(
    args: &mut Vec<String>,
    runtime: SandboxRuntime,
    worktree: &Path,
    state_root: Option<&Path>,
) -> Result<()> {
    let identity = match crate::git::RepositoryIdentity::discover(worktree) {
        Ok(identity) => identity,
        Err(error) => {
            #[cfg(test)]
            {
                let _ = error;
                return Ok(());
            }
            #[cfg(not(test))]
            return Err(error.context("Refusing to sandbox a worktree with invalid Git metadata"));
        }
    };
    if identity.is_bare || identity.admin_dir == identity.common_dir {
        anyhow::bail!("Container sandboxes require a linked Git worktree");
    }
    validate_git_hooks(&identity.common_dir)?;
    let private = git_private_state_dir(worktree, state_root)?;

    for pointer in collect_worktree_git_pointers(&identity.worktree)? {
        push_read_only_path(args, runtime, &pointer);
    }

    if runtime == SandboxRuntime::AppleContainer {
        push_read_only_path(args, runtime, &identity.common_dir);
    }
    for path in [
        identity.common_dir.join("objects"),
        identity.common_dir.join("refs"),
        identity.common_dir.join("logs"),
        identity.common_dir.join("rr-cache"),
        identity.admin_dir.clone(),
    ] {
        if path.exists() {
            push_bind_path(args, &path, false);
        }
    }

    let common_object_info = identity.common_dir.join("objects/info");
    if common_object_info.exists() {
        push_read_only_path(args, runtime, &common_object_info);
    }

    let module_roots = module_git_roots(&identity.common_dir)?;
    for root in &module_roots {
        push_bind_path(args, root, false);
    }

    let mut config_index = 0;
    protect_writable_git_root(
        args,
        runtime,
        &identity.admin_dir,
        false,
        &private,
        &mut config_index,
    )?;
    for root in &module_roots {
        protect_writable_git_root(args, runtime, root, true, &private, &mut config_index)?;
    }

    let common_config = identity.common_dir.join("config");
    ensure_policy_file(&common_config)?;
    for include in config_include_paths(&common_config)? {
        if include.starts_with(&identity.worktree) || include.starts_with(&identity.common_dir) {
            push_read_only_path(args, runtime, &include);
        }
    }
    if runtime != SandboxRuntime::AppleContainer {
        let snapshot = private.join(format!("config-{config_index}"));
        crate::git::snapshot_local_config(&common_config, &snapshot)?;
        args.push("--mount".to_string());
        args.push(format!(
            "type=bind,source={},target={},readonly",
            snapshot.display(),
            common_config.display()
        ));
    }

    Ok(())
}

fn podman_uses_remote_connection() -> bool {
    !cfg!(target_os = "linux")
        || std::env::var_os("CONTAINER_HOST").is_some()
        || std::env::var_os("CONTAINER_CONNECTION").is_some()
}

fn validate_oci_runtime_support(runtime: SandboxRuntime, podman_remote: bool) -> Result<()> {
    if runtime == SandboxRuntime::Podman && podman_remote {
        anyhow::bail!(
            "sandbox.container.oci_runtime is not supported by remote Podman; \
             podman run --runtime is available only with local Podman on Linux. \
             Use sandbox.container.runtime: docker, connect to local Podman, or \
             unset sandbox.container.oci_runtime."
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_docker_run_args_inner(
    command: &str,
    config: &SandboxConfig,
    agent: &str,
    worktree_root: &Path,
    pane_cwd: &Path,
    extra_envs: &[(&str, &str)],
    shim_host_dir: Option<&Path>,
    network_deny: bool,
    state_root: Option<&Path>,
) -> Result<Vec<String>> {
    let image = config.resolved_image(agent);
    let worktree_root_str = worktree_root.to_string_lossy();
    let pane_cwd_str = pane_cwd.to_string_lossy();

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    let runtime = config.runtime();

    let mut args = Vec::new();

    // Base command (no runtime name -- caller prepends that)
    args.push("run".to_string());

    // Optional VM/sandboxed OCI runtime. Apple Container manages its own VM
    // and has no `--runtime` concept. Remote Podman does not expose its local
    // engine's `--runtime` flag, so reject the setting instead of silently
    // running under a different isolation boundary.
    if let Some(oci_runtime) = config.container.oci_runtime()
        && runtime != SandboxRuntime::AppleContainer
    {
        validate_oci_runtime_support(runtime, podman_uses_remote_connection())?;
        args.push("--runtime".to_string());
        args.push(oci_runtime.to_string());
    }

    args.push("--rm".to_string());
    args.push("-it".to_string());

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

    // Extra capabilities / security options (global-only). Applied in both
    // network modes. Primary use: docker-in-docker under `oci_runtime: kata`,
    // where `--privileged` cannot be used and the guest VM remains the isolation
    // boundary. Permissive values weaken host-kernel container isolation when
    // used without a VM-based OCI runtime. Apple Container doesn't accept
    // Docker/Podman `--cap-add`/`--security-opt`, so they're omitted there.
    if runtime != SandboxRuntime::AppleContainer {
        if config.container.cap_add().iter().any(|cap| {
            matches!(
                cap.to_ascii_uppercase().as_str(),
                "ALL" | "SYS_ADMIN" | "CAP_SYS_ADMIN"
            )
        }) {
            anyhow::bail!(
                "sandbox.container.cap_add cannot grant SYS_ADMIN or ALL while host Git metadata is mounted"
            );
        }
        for cap in config.container.cap_add() {
            args.push("--cap-add".to_string());
            args.push(cap.clone());
        }
        for opt in config.container.security_opt() {
            args.push("--security-opt".to_string());
            args.push(opt.clone());
        }
    }

    // Rootless Podman remaps UIDs. keep-id maps the host user to the same UID
    // inside the container so bind-mounted files remain accessible to that UID.
    if runtime.needs_userns_keep_id() {
        args.push("--userns=keep-id".to_string());
    }

    if network_deny {
        // Docker-compatible runtimes start as root for firewall setup before
        // network-init.sh drops privileges to the bind-mount owner.
        if runtime.needs_deny_mode_caps() {
            args.push("--user".to_string());
            args.push("0:0".to_string());
            args.extend(deny_mode_run_flags());
        }
        args.push("--env".to_string());
        args.push(format!("WM_TARGET_UID={}", uid));
        args.push("--env".to_string());
        args.push(format!("WM_TARGET_GID={}", gid));
        // Supplementary groups are applied inside the container by setpriv
        // (see docker/Dockerfile.base). The root process drops privileges after
        // firewall setup, so setpriv receives the complete supplementary list.
        if !group_add.is_empty() {
            args.push("--env".to_string());
            args.push(format!("WM_EXTRA_GIDS={}", group_add.join(",")));
        }
    } else {
        // Normal mode runs as the host user directly.
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
            // The main worktree is present only as a symlink target. The nested
            // common directory mount provides the Git data that guest commands update.
            if let Some(main_worktree) = main_git.parent() {
                args.push("--mount".to_string());
                args.push(format!(
                    "type=bind,source={},target={},readonly",
                    main_worktree.display(),
                    main_worktree.display()
                ));
                main_worktree_path = Some(main_worktree.to_path_buf());
            }

            args.push("--mount".to_string());
            let read_only = if runtime == SandboxRuntime::AppleContainer {
                ""
            } else {
                ",readonly"
            };
            args.push(format!(
                "type=bind,source={},target={}{}",
                main_git.display(),
                main_git.display(),
                read_only
            ));
        }
    }

    add_git_metadata_boundary(&mut args, runtime, worktree_root, state_root)?;

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
    let extra_mounts = config.extra_mounts();
    let git_identity = crate::git::RepositoryIdentity::discover(worktree_root).ok();
    for mount in extra_mounts {
        let (host, guest, read_only) = mount.resolve()?;
        if !read_only {
            if let Some(git_identity) = &git_identity {
                let protected = [
                    git_identity.dot_git.as_path(),
                    git_identity.admin_dir.as_path(),
                    git_identity.common_dir.as_path(),
                ];
                if protected
                    .iter()
                    .any(|path| guest.starts_with(path) || path.starts_with(&guest))
                {
                    anyhow::bail!(
                        "Writable extra mount at {} overlaps protected Git metadata",
                        guest.display()
                    );
                }
            }
            let socket_name = host
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if matches!(
                socket_name,
                "docker.sock" | "podman.sock" | "containerd.sock"
            ) {
                anyhow::bail!(
                    "Writable container-engine sockets are incompatible with the Git metadata boundary"
                );
            }
        }
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
            "omp" => "/tmp/.omp/agent",
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
            let state_dir = match state_root {
                Some(root) => root.join("container").join(&cache_key),
                None => crate::xdg::state_dir()?.join("container").join(cache_key),
            };
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
        // iptables firewall rules and then drops privileges via setpriv.
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
    // Strip the leading history-prevention space before passing the command to
    // the sandbox supervisor.
    let command = command.strip_prefix(' ').unwrap_or(command);

    let frozen_config = crate::frozen_config::shell_assignment()
        .map(|assignment| format!("{assignment} "))
        .unwrap_or_default();
    let mut parts = format!(
        "{frozen_config}workmux sandbox run '{}'",
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

    // Prefix with space to prevent shell history entry.
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

    fn linked_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let worktree = temp.path().join("worktree");
        std::fs::create_dir(&main).unwrap();
        let run = |args: &[&str], cwd: &Path| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(cwd)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q"], &main);
        run(&["config", "user.name", "Test"], &main);
        run(&["config", "user.email", "test@example.com"], &main);
        std::fs::write(main.join("tracked"), "base").unwrap();
        run(&["add", "."], &main);
        run(&["commit", "-qm", "base"], &main);
        assert!(
            Command::new("git")
                .args(["worktree", "add", "-qb", "topic"])
                .arg(&worktree)
                .current_dir(&main)
                .status()
                .unwrap()
                .success()
        );
        (temp, main, worktree)
    }

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

    fn test_sandbox_config(excluded_files: Vec<String>) -> SandboxConfig {
        SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(SandboxRuntime::Docker),
                excluded_files: Some(excluded_files),
                ..Default::default()
            },
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        }
    }

    fn test_build_run_args_result_for_worktree(
        worktree: &Path,
        config: &SandboxConfig,
        network_deny: bool,
    ) -> Result<Vec<String>> {
        build_docker_run_args(
            "claude",
            config,
            "claude",
            worktree,
            worktree,
            &[],
            None,
            network_deny,
        )
    }

    fn test_build_run_args_result(
        config: &SandboxConfig,
        network_deny: bool,
    ) -> Result<Vec<String>> {
        test_build_run_args_result_for_worktree(Path::new("/tmp/project"), config, network_deny)
    }

    fn test_build_args(worktree: &Path, config: &SandboxConfig) -> Vec<String> {
        test_build_run_args_result_for_worktree(worktree, config, false).unwrap()
    }

    fn sandbox_config(
        runtime: SandboxRuntime,
        configure: impl FnOnce(&mut ContainerConfig),
    ) -> SandboxConfig {
        let mut container = ContainerConfig {
            runtime: Some(runtime),
            ..Default::default()
        };
        configure(&mut container);
        SandboxConfig {
            enabled: Some(true),
            container,
            image: Some("test-image:latest".to_string()),
            ..Default::default()
        }
    }

    fn test_build_run_args(config: &SandboxConfig, network_deny: bool) -> Vec<String> {
        test_build_run_args_result(config, network_deny).unwrap()
    }

    fn test_build_run_args_for_agent(agent: &str, config: &SandboxConfig) -> Vec<String> {
        build_docker_run_args(
            agent,
            config,
            agent,
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
        )
        .unwrap()
    }

    fn test_build_run_args_for_agent_with_state(
        agent: &str,
        config: &SandboxConfig,
        state_root: &Path,
    ) -> Vec<String> {
        build_docker_run_args_with_state_dir(
            agent,
            config,
            agent,
            Path::new("/tmp/project"),
            Path::new("/tmp/project"),
            &[],
            None,
            false,
            state_root,
        )
        .unwrap()
    }

    fn agent_sandbox_config(runtime: SandboxRuntime) -> SandboxConfig {
        SandboxConfig {
            enabled: Some(true),
            container: ContainerConfig {
                runtime: Some(runtime),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn agent_sandbox_config_with_dir(runtime: SandboxRuntime, dir: &Path) -> SandboxConfig {
        SandboxConfig {
            agent_config_dir: Some(dir.join("{agent}").to_string_lossy().to_string()),
            ..agent_sandbox_config(runtime)
        }
    }

    fn test_build_deny_args() -> Vec<String> {
        test_build_run_args(&make_config(), true)
    }

    fn find_flag_value<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].as_str())
            .collect()
    }

    fn assert_flag_eq(args: &[String], flag: &str, expected: &str) {
        let values = find_flag_value(args, flag);
        assert_eq!(
            values.as_slice(),
            [expected],
            "expected single {flag}={expected}, got: {values:?} in {args:?}"
        );
    }

    fn assert_agent_credential_mount(
        args: &[String],
        expected_target: Option<&str>,
        excluded: &[&str],
    ) {
        let args_str = args.join(" ");
        if let Some(target) = expected_target {
            assert!(
                args_str.contains(&format!("target={target}")),
                "expected credential mount target={target}, got: {args_str}"
            );
        }
        for item in excluded {
            assert!(
                !args_str.contains(item),
                "unexpected credential mount {item}, got: {args_str}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn container_boundary_rejects_linked_hooks() {
        use std::os::unix::fs::symlink;

        for runtime in [
            SandboxRuntime::Docker,
            SandboxRuntime::Podman,
            SandboxRuntime::AppleContainer,
        ] {
            for layout in ["symlink", "dangling", "hardlink", "directory"] {
                let (temp, _main, worktree) = linked_worktree();
                let identity = crate::git::RepositoryIdentity::discover(&worktree).unwrap();
                let hooks = identity.common_dir.join("hooks");
                let target = temp.path().join("target");
                std::fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
                let hook = hooks.join("pre-commit");
                match layout {
                    "symlink" => symlink(&target, &hook).unwrap(),
                    "dangling" => symlink(temp.path().join("missing"), &hook).unwrap(),
                    "hardlink" => std::fs::hard_link(&target, &hook).unwrap(),
                    "directory" => {
                        std::fs::remove_dir_all(&hooks).unwrap();
                        let directory = temp.path().join("external-hooks");
                        std::fs::create_dir(&directory).unwrap();
                        symlink(&directory, &hooks).unwrap();
                    }
                    _ => unreachable!(),
                }
                let error = add_git_metadata_boundary(
                    &mut Vec::new(),
                    runtime,
                    &worktree,
                    Some(temp.path()),
                )
                .unwrap_err();
                assert!(
                    error.to_string().contains("Git hooks"),
                    "{layout}: {error:#}"
                );
            }
        }
    }

    #[test]
    fn docker_mounts_git_policy_separately_from_writable_data() {
        let (temp, main, worktree) = linked_worktree();
        let identity = crate::git::RepositoryIdentity::discover(&worktree).unwrap();
        let submodule_worktree = worktree.join("vendor/submodule");
        std::fs::create_dir_all(&submodule_worktree).unwrap();
        let submodule_worktree = submodule_worktree.canonicalize().unwrap();
        let submodule_admin = identity.common_dir.join("modules/vendor/submodule");
        std::fs::create_dir_all(submodule_admin.join("hooks")).unwrap();
        std::fs::write(
            submodule_worktree.join(".git"),
            format!("gitdir: {}\n", submodule_admin.display()),
        )
        .unwrap();
        std::fs::write(submodule_admin.join("config"), "[core]\n\tbare = false\n").unwrap();
        let config = sandbox_config(SandboxRuntime::Docker, |_| {});
        let args = build_docker_run_args_with_state_dir(
            "true",
            &config,
            "claude",
            &worktree,
            &worktree,
            &[],
            None,
            false,
            temp.path(),
        )
        .unwrap();
        let joined = args.join("\n");
        assert!(joined.contains(&format!("target={},readonly", identity.dot_git.display())));
        assert!(joined.contains(&format!(
            "target={},readonly",
            identity.admin_dir.join("gitdir").display()
        )));
        assert!(joined.contains(&format!(
            "target={},readonly",
            identity.admin_dir.join("commondir").display()
        )));
        assert!(joined.contains(&format!(
            "target={},readonly",
            main.canonicalize().unwrap().display()
        )));
        assert!(joined.contains(&format!(
            "target={}",
            identity.common_dir.join("config").display()
        )));
        assert!(!joined.contains(&format!(
            "source={},target={}",
            identity.common_dir.join("config").display(),
            identity.common_dir.join("config").display()
        )));
        assert!(joined.contains(&format!(
            "target={},readonly",
            submodule_worktree.join(".git").display()
        )));
        assert!(joined.contains(&format!(
            "target={}",
            submodule_admin.join("config").display()
        )));
        assert!(joined.contains(&format!(
            "target={},readonly",
            submodule_admin.join("hooks").display()
        )));
    }

    #[test]
    fn apple_container_marks_git_policy_paths_read_only() {
        let (temp, _main, worktree) = linked_worktree();
        let config = sandbox_config(SandboxRuntime::AppleContainer, |_| {});
        let args = build_docker_run_args_with_state_dir(
            "true",
            &config,
            "claude",
            &worktree,
            &worktree,
            &[],
            None,
            false,
            temp.path(),
        )
        .unwrap();
        let identity = crate::git::RepositoryIdentity::discover(&worktree).unwrap();
        for protected in [
            identity.dot_git.clone(),
            identity.common_dir.clone(),
            identity.admin_dir.join("gitdir"),
            identity.admin_dir.join("commondir"),
            identity.admin_dir.join("config.worktree"),
            identity.common_dir.join("objects/info"),
        ] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == "--read-only-path"
                        && pair[1] == protected.to_string_lossy()),
                "missing {} in {args:?}",
                protected.display()
            );
        }
    }

    #[test]
    fn test_build_args_basic() {
        let config = make_config();
        let args = test_build_args(Path::new("/tmp/project"), &config);

        assert!(args.contains(&"run".to_string()));
        assert!(args.contains(&"--rm".to_string()));
        assert!(args.contains(&"-it".to_string()));
        assert!(args.contains(&"test-image:latest".to_string()));
        assert!(args.contains(&"sh".to_string()));
        assert!(args.contains(&"-c".to_string()));
        assert!(args.contains(&"claude".to_string()));
    }

    #[test]
    fn test_oci_runtime_inserted_after_run() {
        let config = sandbox_config(SandboxRuntime::Docker, |c| {
            c.oci_runtime = Some("kata".to_string());
        });
        let args = test_build_run_args(&config, false);

        assert_eq!(args[0], "run");
        assert_eq!(args[1], "--runtime");
        assert_eq!(args[2], "kata");
    }

    #[test]
    fn test_oci_runtime_absent_by_default() {
        let config = sandbox_config(SandboxRuntime::Docker, |_| {});
        let args = test_build_run_args(&config, false);

        assert!(
            find_flag_value(&args, "--runtime").is_empty(),
            "no --runtime should be added when oci_runtime is unset, got: {args:?}"
        );
    }

    #[test]
    fn test_remote_podman_rejects_oci_runtime() {
        let err = validate_oci_runtime_support(SandboxRuntime::Podman, true)
            .expect_err("remote Podman must reject oci_runtime");
        assert!(err.to_string().contains("remote Podman"));

        assert!(validate_oci_runtime_support(SandboxRuntime::Podman, false).is_ok());
        assert!(validate_oci_runtime_support(SandboxRuntime::Docker, true).is_ok());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_oci_runtime_rejected_for_podman_on_remote_platform() {
        let config = sandbox_config(SandboxRuntime::Podman, |c| {
            c.oci_runtime = Some("crun".to_string());
        });
        let err = test_build_run_args_result(&config, false)
            .expect_err("Podman is remote on non-Linux platforms");

        assert!(err.to_string().contains("remote Podman"));
    }

    #[test]
    fn test_cap_add_and_security_opt_emitted() {
        let config = sandbox_config(SandboxRuntime::Docker, |c| {
            c.cap_add = Some(vec!["NET_BIND_SERVICE".to_string()]);
            c.security_opt = Some(vec!["seccomp=unconfined".to_string()]);
        });
        let args = test_build_run_args(&config, false);

        assert_eq!(
            find_flag_value(&args, "--cap-add"),
            vec!["NET_BIND_SERVICE"]
        );
        assert_eq!(
            find_flag_value(&args, "--security-opt"),
            vec!["seccomp=unconfined"]
        );

        let unsafe_config = sandbox_config(SandboxRuntime::Docker, |c| {
            c.cap_add = Some(vec!["ALL".to_string()]);
        });
        assert!(test_build_run_args_result(&unsafe_config, false).is_err());
    }

    #[test]
    fn test_cap_add_and_security_opt_absent_by_default() {
        let config = sandbox_config(SandboxRuntime::Docker, |_| {});
        let args = test_build_run_args(&config, false);

        assert!(
            find_flag_value(&args, "--cap-add").is_empty(),
            "no --cap-add when cap_add is unset, got: {args:?}"
        );
        assert!(
            find_flag_value(&args, "--security-opt").is_empty(),
            "no --security-opt when security_opt is unset, got: {args:?}"
        );
    }

    #[test]
    fn test_oci_runtime_omitted_on_apple_container() {
        // Apple Container has no `--runtime` concept and would misread the
        // value (e.g. treat `kata` as an Apple-native plugin), so a globally
        // configured oci_runtime must be dropped rather than passed through.
        let config = sandbox_config(SandboxRuntime::AppleContainer, |c| {
            c.oci_runtime = Some("kata".to_string());
        });
        let args = test_build_run_args(&config, false);

        assert!(
            find_flag_value(&args, "--runtime").is_empty(),
            "--runtime must be omitted on Apple Container, got: {args:?}"
        );
    }

    #[test]
    fn test_cap_add_and_security_opt_omitted_on_apple_container() {
        // Apple Container does not accept Docker/Podman `--cap-add` /
        // `--security-opt`; they must be dropped rather than passed through.
        let config = sandbox_config(SandboxRuntime::AppleContainer, |c| {
            c.cap_add = Some(vec!["ALL".to_string()]);
            c.security_opt = Some(vec!["seccomp=unconfined".to_string()]);
        });
        let args = test_build_run_args(&config, false);

        assert!(
            find_flag_value(&args, "--cap-add").is_empty(),
            "--cap-add must be omitted on Apple Container, got: {args:?}"
        );
        assert!(
            find_flag_value(&args, "--security-opt").is_empty(),
            "--security-opt must be omitted on Apple Container, got: {args:?}"
        );
    }

    #[test]
    fn test_excluded_files_default_empty() {
        let config = make_config();
        let args = test_build_args(Path::new("/tmp/project"), &config);

        assert!(
            !args.iter().any(|a| a.contains("source=/dev/null")),
            "no /dev/null mounts should be added when excluded_files is unset"
        );
    }

    #[test]
    fn test_excluded_files_masks_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "SECRET=1").unwrap();

        let config = test_sandbox_config(vec![".env".to_string()]);

        let args = test_build_args(tmp.path(), &config);

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

        let config = test_sandbox_config(vec![".env".to_string()]);

        let args = test_build_args(tmp.path(), &config);

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

        let config = sandbox_config(SandboxRuntime::AppleContainer, |c| {
            c.excluded_files = Some(vec![".env".to_string()]);
        });

        let err = test_build_run_args_result_for_worktree(tmp.path(), &config, false)
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

        let config = test_sandbox_config(vec![".env".to_string()]);

        let args = test_build_args(&wt, &config);

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

        let config = test_sandbox_config(vec![".env".to_string()]);

        let args = test_build_args(&wt, &config);

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

        let config = test_sandbox_config(vec![".aws".to_string()]);

        let args = test_build_args(tmp.path(), &config);

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

        let config = test_sandbox_config(vec!["my..env".to_string(), "foo..bar".to_string()]);

        let args = test_build_args(tmp.path(), &config);

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

        let config = test_sandbox_config(vec![
            "../outside-secret".to_string(),
            "/etc/passwd".to_string(),
        ]);

        let args = test_build_args(tmp.path(), &config);

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
        let config = make_config();
        let args = test_build_args(Path::new("/tmp/project"), &config);

        assert!(args.contains(&"--add-host".to_string()));
        assert!(args.contains(&"host.docker.internal:host-gateway".to_string()));
    }

    #[test]
    fn test_build_args_podman_omits_add_host() {
        let config = sandbox_config(SandboxRuntime::Podman, |_| {});
        let args = test_build_run_args(&config, false);

        assert!(!args.contains(&"--add-host".to_string()));
    }

    #[test]
    fn test_build_args_runtime_not_in_args() {
        let config = sandbox_config(SandboxRuntime::Podman, |_| {});
        let args = test_build_run_args(&config, false);

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
        assert!(dockerfile_for_agent("omp").is_some());
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

        let mut config = sandbox_config(SandboxRuntime::Docker, |_| {});
        config.extra_mounts = Some(vec![ExtraMount::Path("/tmp/notes".to_string())]);
        let args = test_build_run_args(&config, false);

        let args_str = args.join(" ");
        assert!(args_str.contains("type=bind,source=/tmp/notes,target=/tmp/notes,readonly"));
    }

    #[test]
    fn test_build_args_extra_mounts_writable_with_guest_path() {
        use crate::config::ExtraMount;

        let mut config = sandbox_config(SandboxRuntime::Docker, |_| {});
        config.extra_mounts = Some(vec![ExtraMount::Spec {
            host_path: "/tmp/data".to_string(),
            guest_path: Some("/mnt/data".to_string()),
            writable: Some(true),
        }]);
        let args = test_build_run_args(&config, false);

        let args_str = args.join(" ");
        assert!(args_str.contains("type=bind,source=/tmp/data,target=/mnt/data"));
        // Readonly stays absent.
        assert!(!args_str.contains("/tmp/data,target=/mnt/data,readonly"));
    }

    #[test]
    fn writable_extra_mount_cannot_overlap_git_metadata() {
        use crate::config::ExtraMount;

        let (temp, _main, worktree) = linked_worktree();
        let identity = crate::git::RepositoryIdentity::discover(&worktree).unwrap();
        let mut config = sandbox_config(SandboxRuntime::Docker, |_| {});
        config.extra_mounts = Some(vec![ExtraMount::Spec {
            host_path: identity.common_dir.to_string_lossy().into_owned(),
            guest_path: Some(identity.common_dir.to_string_lossy().into_owned()),
            writable: Some(true),
        }]);
        let error = build_docker_run_args_with_state_dir(
            "claude",
            &config,
            "claude",
            &worktree,
            &worktree,
            &[],
            None,
            false,
            temp.path(),
        )
        .expect_err("writable mount must not shadow Git policy mounts");
        assert!(
            error
                .to_string()
                .contains("overlaps protected Git metadata"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn test_build_args_gemini_agent_credential_mount() {
        let args = test_build_run_args_for_agent("gemini", &make_config());
        assert_agent_credential_mount(
            &args,
            Some("/tmp/.gemini"),
            &[
                "target=/tmp/.claude.json",
                "target=/tmp/.claude,",
                "/home/user/.codex",
            ],
        );
    }

    #[test]
    fn test_build_args_codex_agent_credential_mount() {
        let args = test_build_run_args_for_agent("codex", &make_config());
        assert_agent_credential_mount(
            &args,
            Some("/home/user/.codex"),
            &["target=/tmp/.claude.json", "target=/tmp/.gemini"],
        );
        assert!(args.iter().any(|a| a == "CODEX_HOME=/home/user/.codex"));
    }

    #[test]
    fn test_build_args_opencode_agent_credential_mount() {
        let args = test_build_run_args_for_agent("opencode", &make_config());
        assert_agent_credential_mount(
            &args,
            Some("/tmp/.local/share/opencode"),
            &["target=/tmp/.claude.json", "target=/tmp/.gemini"],
        );
    }

    #[test]
    fn test_build_args_unknown_agent_no_credential_mount() {
        let args = test_build_run_args_for_agent("unknown-agent", &make_config());
        assert_agent_credential_mount(
            &args,
            None,
            &[
                "target=/tmp/.claude",
                "target=/tmp/.gemini",
                "/home/user/.codex",
                "target=/tmp/.local/share/opencode",
            ],
        );
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
        let args = test_build_run_args(&config, false);

        let args_str = args.join(" ");
        assert!(args_str.contains(&format!(
            "type=bind,source={},target=/tmp/.claude",
            claude_dir.display()
        )));
    }

    // --- Network deny mode tests ---

    #[test]
    fn test_build_args_network_deny_has_cap_net_admin() {
        let args = test_build_deny_args();

        assert!(args.contains(&"--cap-add=NET_ADMIN".to_string()));
        assert!(args.contains(&"--security-opt".to_string()));
        assert!(args.contains(&"no-new-privileges".to_string()));
    }

    #[test]
    fn test_build_args_network_deny_docker_starts_as_root() {
        let args = test_build_deny_args();

        assert_flag_eq(&args, "--user", "0:0");
        assert!(!args.contains(&"--userns=keep-id".to_string()));
    }

    #[test]
    fn test_build_args_network_deny_has_target_uid_gid() {
        let args = test_build_deny_args();

        let args_str = args.join(" ");
        assert!(args_str.contains("WM_TARGET_UID="));
        assert!(args_str.contains("WM_TARGET_GID="));
    }

    #[test]
    fn test_build_args_network_deny_wraps_with_network_init() {
        let args = test_build_deny_args();

        // Command should be: image network-init.sh sh -c <command>
        let image_idx = args.iter().position(|a| a == "test-image:latest").unwrap();
        assert_eq!(args[image_idx + 1], "network-init.sh");
        assert_eq!(args[image_idx + 2], "sh");
        assert_eq!(args[image_idx + 3], "-c");
        assert_eq!(args[image_idx + 4], "claude");
    }

    #[test]
    fn test_build_args_network_deny_path_includes_sbin() {
        let args = test_build_deny_args();

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
        let args = test_build_args(Path::new("/tmp/project"), &config);

        let path_arg = args.iter().find(|a| a.starts_with("PATH=")).unwrap();
        assert!(
            !path_arg.contains("/usr/sbin"),
            "allow mode PATH should not include /usr/sbin: {}",
            path_arg
        );
    }

    #[test]
    fn test_build_args_network_deny_podman_keeps_id_and_starts_as_root() {
        let config = sandbox_config(SandboxRuntime::Podman, |_| {});
        let args = test_build_run_args(&config, true);

        assert!(args.contains(&"--userns=keep-id".to_string()));
        assert_flag_eq(&args, "--user", "0:0");
    }

    #[test]
    fn test_build_args_allow_mode_no_cap_net_admin() {
        let config = make_config();
        let args = test_build_args(Path::new("/tmp/project"), &config);

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
        let config = sandbox_config(SandboxRuntime::AppleContainer, |_| {});
        let args = test_build_run_args(&config, false);

        // Should NOT have Docker's --add-host
        assert!(!args.contains(&"--add-host".to_string()));
        // Should NOT have Podman's --userns=keep-id
        assert!(!args.contains(&"--userns=keep-id".to_string()));
    }

    #[test]
    fn test_build_args_apple_container_deny_mode_skips_caps() {
        let config = sandbox_config(SandboxRuntime::AppleContainer, |_| {});
        let args = test_build_run_args(&config, true);

        // Apple Container handles isolation without Docker/Podman run flags.
        assert!(!args.contains(&"--cap-add=NET_ADMIN".to_string()));
        assert!(!args.contains(&"--security-opt".to_string()));
        assert!(!args.contains(&"--user".to_string()));
        // UID/GID env vars supply the network-init.sh privilege-drop target.
        assert!(args.iter().any(|a| a.starts_with("WM_TARGET_UID=")));
        assert!(args.iter().any(|a| a.starts_with("WM_TARGET_GID=")));
    }

    #[test]
    fn test_build_args_apple_container_default_memory() {
        let config = sandbox_config(SandboxRuntime::AppleContainer, |_| {});
        let args = test_build_run_args(&config, false);

        assert_flag_eq(&args, "--memory", "16G");
        // No --cpus unless explicitly configured
        assert!(!args.contains(&"--cpus".to_string()));
    }

    #[test]
    fn test_build_args_apple_container_custom_resources() {
        let config = sandbox_config(SandboxRuntime::AppleContainer, |c| {
            c.memory = Some("8G".to_string());
            c.cpus = Some(8);
        });
        let args = test_build_run_args(&config, false);

        assert_flag_eq(&args, "--memory", "8G");
        assert_flag_eq(&args, "--cpus", "8");
    }

    #[test]
    fn test_build_args_docker_no_default_resource_flags() {
        let config = make_config();
        let args = test_build_args(Path::new("/tmp/project"), &config);

        // Docker should NOT get --memory or --cpus by default
        assert!(!args.contains(&"--memory".to_string()));
        assert!(!args.contains(&"--cpus".to_string()));
    }

    #[test]
    fn test_build_args_docker_explicit_memory() {
        let config = sandbox_config(SandboxRuntime::Docker, |c| {
            c.memory = Some("4G".to_string());
        });
        let args = test_build_run_args(&config, false);

        assert_flag_eq(&args, "--memory", "4G");
    }

    #[test]
    fn docker_emits_device_flags() {
        let config = sandbox_config(SandboxRuntime::Docker, |c| {
            c.devices = Some(vec![
                ContainerDevice::String("/dev/kvm".to_string()),
                ContainerDevice::String("/dev/dri:/dev/dri:rwm".to_string()),
            ]);
        });
        let args = test_build_run_args(&config, false);

        let devs = find_flag_value(&args, "--device");
        assert!(devs.contains(&"/dev/kvm"));
        assert!(devs.contains(&"/dev/dri:/dev/dri:rwm"));
    }

    #[test]
    fn docker_allow_mode_emits_group_add() {
        let config = sandbox_config(SandboxRuntime::Docker, |c| {
            c.group_add = Some(vec!["dialout".to_string(), "video".to_string()]);
        });
        let args = test_build_run_args(&config, false);

        let groups = find_flag_value(&args, "--group-add");
        assert!(groups.contains(&"dialout"));
        assert!(groups.contains(&"video"));
        assert!(!args.iter().any(|a| a.starts_with("WM_EXTRA_GIDS=")));
    }

    #[test]
    fn docker_deny_mode_uses_wm_extra_gids_not_group_add() {
        let config = sandbox_config(SandboxRuntime::Docker, |c| {
            c.group_add = Some(vec!["dialout".to_string(), "20".to_string()]);
        });
        let args = test_build_run_args(&config, true);

        assert!(!args.iter().any(|a| a == "--group-add"));
        assert!(args.iter().any(|a| a == "WM_EXTRA_GIDS=dialout,20"));
    }

    #[test]
    fn docker_deny_mode_still_emits_device_flags() {
        let config = sandbox_config(SandboxRuntime::Docker, |c| {
            c.devices = Some(vec![ContainerDevice::String("/dev/kvm".to_string())]);
        });
        let args = test_build_run_args(&config, true);

        let devs = find_flag_value(&args, "--device");
        assert!(devs.contains(&"/dev/kvm"));
    }

    #[test]
    fn apple_container_git_boundary_capability_is_explicit() {
        assert!(apple_container_supports_read_only_path(
            "--read-only-path <path>"
        ));
        assert!(!apple_container_supports_read_only_path("--read-only"));
    }

    #[test]
    fn apple_container_rejects_devices() {
        let config = sandbox_config(SandboxRuntime::AppleContainer, |c| {
            c.devices = Some(vec![ContainerDevice::String("/dev/kvm".to_string())]);
        });
        let result = test_build_run_args_result(&config, false);
        assert!(result.is_err());
    }

    #[test]
    fn apple_container_rejects_group_add() {
        let config = sandbox_config(SandboxRuntime::AppleContainer, |c| {
            c.group_add = Some(vec!["dialout".to_string()]);
        });
        let result = test_build_run_args_result(&config, false);
        assert!(result.is_err());
    }

    #[test]
    fn podman_allow_mode_supports_devices_and_group_add() {
        let config = sandbox_config(SandboxRuntime::Podman, |c| {
            c.devices = Some(vec![ContainerDevice::String("/dev/kvm".to_string())]);
            c.group_add = Some(vec!["dialout".to_string()]);
        });
        let args = test_build_run_args(&config, false);

        let devs = find_flag_value(&args, "--device");
        assert!(devs.contains(&"/dev/kvm"));
        let groups = find_flag_value(&args, "--group-add");
        assert!(groups.contains(&"dialout"));
    }

    #[test]
    fn test_build_args_pi_agent_apple_container_mounts_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let config = agent_sandbox_config_with_dir(SandboxRuntime::AppleContainer, tmp.path());
        let args = test_build_run_args_for_agent_with_state("pi", &config, tmp.path());

        let args_str = args.join(" ");
        // Pi agent should mount ~/.pi/agent to /tmp/.pi/agent
        assert!(
            args_str.contains("/tmp/.pi/agent"),
            "pi agent config mount missing: {}",
            args_str
        );
        // Claude-specific mounts stay absent.
        assert!(
            !args_str.contains("/tmp/.claude.json"),
            "no claude mount expected for pi"
        );
        assert!(!args_str.contains("/tmp/.claude,"));
    }

    #[test]
    fn test_build_args_omp_agent_mounts_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let config = agent_sandbox_config_with_dir(SandboxRuntime::AppleContainer, tmp.path());
        let args = test_build_run_args_for_agent("omp", &config);

        let args_str = args.join(" ");
        assert!(
            args_str.contains("/tmp/.omp/agent"),
            "omp agent config mount missing: {}",
            args_str
        );
        assert!(
            !args_str.contains("/tmp/.claude.json"),
            "no claude mount expected for omp"
        );
        assert!(!args_str.contains("/tmp/.claude,"));
    }

    #[test]
    fn test_build_args_pi_agent_overlays_bin_after_parent() {
        use crate::config::SandboxConfig;
        let tmp = tempfile::tempdir().unwrap();
        let config = SandboxConfig {
            enabled: Some(true),
            agent_config_dir: Some(tmp.path().join("{agent}").to_string_lossy().to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args_with_state_dir(
            "pi",
            &config,
            "pi",
            Path::new("/tmp/myproject"),
            Path::new("/tmp/myproject"),
            &[],
            None,
            false,
            tmp.path(),
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
        // The per-worktree handle excludes the container name PID suffix.
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
        let tmp = tempfile::tempdir().unwrap();
        let config = SandboxConfig {
            enabled: Some(true),
            agent_config_dir: Some(tmp.path().join("{agent}").to_string_lossy().to_string()),
            ..Default::default()
        };
        let args = build_docker_run_args(
            "omp",
            &config,
            "omp",
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
            "omp agent should not have pi bin overlay"
        );
    }
}
