//! Sandbox management commands.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use tracing::debug;

use crate::config::Config;
use crate::sandbox;
use crate::sandbox::lima;
use crate::sandbox::lima::{LimaInstance, parse_lima_instances};

#[derive(Debug, Args)]
#[command(help_template = "\
{about-with-newline}
{usage-heading} {usage}

Container commands:
  build            Build the sandbox container image locally
  pull             Pull the latest sandbox image from the container registry
  init-dockerfile  Export customizable Dockerfile templates

Lima commands:
  stop             Stop Lima VMs to free resources
  prune            Delete unused Lima VMs to reclaim disk space

General commands:
  agent            Run an agent inside a sandbox with RPC support
  shell            Start an interactive shell in a sandbox
  install-dev      Cross-compile and install workmux into sandboxes
  help             Print this message or the help of the given subcommand(s)

{options}")]
pub struct SandboxArgs {
    #[command(subcommand)]
    pub command: SandboxCommand,
}

#[derive(Debug, Subcommand)]
pub enum SandboxCommand {
    /// Build the sandbox container image locally.
    /// Note: a pre-built image is available via `workmux sandbox pull`.
    Build,
    /// Pull the latest sandbox image from the container registry.
    Pull,
    /// Export customizable Dockerfile templates for building your own sandbox image.
    InitDockerfile {
        /// Overwrite existing Dockerfiles
        #[arg(long)]
        force: bool,
    },
    /// Delete unused Lima VMs to reclaim disk space.
    Prune {
        /// Skip confirmation and delete all workmux VMs
        #[arg(short, long)]
        force: bool,
    },
    /// Run a command inside a sandbox (internal, used by pane setup).
    #[command(hide = true)]
    Run {
        /// Path to the working directory
        worktree: PathBuf,
        /// Root of the worktree for mounting (defaults to worktree path)
        #[arg(long)]
        worktree_root: Option<PathBuf>,
        /// Command and arguments to run inside the sandbox
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Cross-compile and install workmux into containers and running Lima VMs for development.
    InstallDev {
        /// Skip cross-compilation and use existing binary
        #[arg(long)]
        skip_build: bool,
        /// Use release profile (slower build, faster binary)
        #[arg(long)]
        release: bool,
    },
    /// Stop Lima VMs to free resources.
    Stop {
        /// VM name to stop (if not provided, show interactive list)
        #[arg(conflicts_with = "all")]
        name: Option<String>,
        /// Stop all workmux VMs (wm-* prefix)
        #[arg(long)]
        all: bool,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Run the configured agent inside a sandbox with full RPC support.
    /// Unlike `shell`, this starts an RPC server so the agent can call
    /// workmux commands (e.g., `workmux add` to spawn sub-agents).
    Agent {
        /// Command to run instead of the configured agent
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Start an interactive shell in a sandbox.
    /// Uses the same mounts and environment as a normal worktree sandbox.
    Shell {
        /// Exec into an existing container for this worktree instead of starting a new one
        /// (container backend only)
        #[arg(long, short)]
        exec: bool,
        /// Command to run instead of bash
        #[arg(last = true)]
        command: Vec<String>,
    },
}

/// Resolve the canonical agent name from config.
fn resolve_agent(config: &Config) -> &'static str {
    crate::multiplexer::agent::resolve_profile_with_type(
        config.agent.as_deref(),
        config.agent_type.as_deref(),
    )
    .name()
}

fn run_agent(command: Vec<String>) -> Result<()> {
    let config = Config::load(None)?;

    let cwd = std::env::current_dir().context("Failed to get current directory")?;

    // Validate git repo early -- sandbox needs git dirs for mounts
    let worktree_root = crate::git::get_repo_root()
        .context(
            "Not inside a git repository. workmux sandbox agent requires a git repo for mounting.",
        )?
        .canonicalize()
        .unwrap_or_else(|_| cwd.clone());

    // Build agent command: explicit args or configured agent
    let agent_command = if command.is_empty() {
        let agent = config.agent.as_deref().unwrap_or("claude");
        vec![agent.to_string()]
    } else {
        command
    };

    let exit_code = super::sandbox_run::run(cwd, Some(worktree_root), agent_command)?;
    std::process::exit(exit_code);
}

pub fn run(args: SandboxArgs) -> Result<()> {
    match args.command {
        SandboxCommand::Build => run_build(),
        SandboxCommand::Pull => run_pull(),
        SandboxCommand::InitDockerfile { force } => run_init_dockerfile(force),
        SandboxCommand::Run {
            worktree,
            worktree_root,
            command,
        } => {
            debug!(worktree = %worktree.display(), ?worktree_root, ?command, "sandbox run");
            let exit_code = super::sandbox_run::run(worktree, worktree_root, command)?;
            std::process::exit(exit_code);
        }
        SandboxCommand::InstallDev {
            skip_build,
            release,
        } => run_install_dev(skip_build, release),
        SandboxCommand::Agent { command } => run_agent(command),
        SandboxCommand::Prune { force } => run_prune(force),
        SandboxCommand::Stop { name, all, yes } => run_stop(name, all, yes),
        SandboxCommand::Shell { exec, command } => run_shell(exec, command),
    }
}

fn run_build() -> Result<()> {
    let config = Config::load(None)?;
    let agent = resolve_agent(&config);

    println!(
        "Building sandbox image '{}' for agent '{}'...",
        config.sandbox.resolved_image(agent),
        agent,
    );
    sandbox::build_image(&config.sandbox, agent)?;
    println!("\nSandbox image built successfully!");
    println!();
    println!(
        "Tip: a pre-built image is available at {}:{}",
        sandbox::DEFAULT_IMAGE_REGISTRY,
        agent
    );
    println!("     Use `workmux sandbox pull` to pull it instead of building locally.");

    Ok(())
}

fn run_pull() -> Result<()> {
    let config = Config::load(None)?;
    let agent = resolve_agent(&config);
    let image = config.sandbox.resolved_image(agent);

    sandbox::pull_image(&config.sandbox, &image)?;
    sandbox::freshness::mark_fresh(&image, config.sandbox.runtime());

    println!("Image '{}' is up to date.", image);
    Ok(())
}

fn run_init_dockerfile(force: bool) -> Result<()> {
    use console::style;

    let config = Config::load(None)?;
    let agent_name = resolve_agent(&config);

    let dockerfile_path = PathBuf::from("Dockerfile.sandbox");

    if !force && dockerfile_path.exists() {
        bail!("Dockerfile.sandbox already exists. Use --force to overwrite.");
    }

    let agent_dockerfile = sandbox::dockerfile_for_agent(agent_name).ok_or_else(|| {
        anyhow::anyhow!(
            "No Dockerfile for agent '{}'. Known agents: {}",
            agent_name,
            sandbox::KNOWN_AGENTS.join(", ")
        )
    })?;

    // Merge base + agent into a single Dockerfile by stripping the ARG/FROM lines
    // from the agent Dockerfile and appending the rest to the base
    let agent_body: String = agent_dockerfile
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with("ARG BASE=") && !trimmed.starts_with("FROM ${BASE}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let combined = format!(
        "{}\n\n# --- {} agent ---{}",
        sandbox::DOCKERFILE_BASE.trim_end(),
        agent_name,
        agent_body.trim_end(),
    );

    std::fs::write(&dockerfile_path, combined.as_bytes())?;

    println!("✓ Created {}", style("Dockerfile.sandbox").bold());
    println!();
    println!("{}:", style("Next steps").bold());
    println!("  1. Edit Dockerfile.sandbox to add your packages");
    println!(
        "  2. Build: {}",
        style("docker build -t my-sandbox -f Dockerfile.sandbox .").dim()
    );
    println!("  3. Configure {}:", style(".workmux.yaml").bold());
    println!("       {}", style("sandbox:").dim());
    println!("         {}", style("enabled: true").dim());
    println!("         {}", style("image: my-sandbox").dim());
    Ok(())
}

fn linux_target_triple() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "aarch64" => Ok("aarch64-unknown-linux-gnu"),
        "x86_64" => Ok("x86_64-unknown-linux-gnu"),
        arch => bail!(
            "unsupported host architecture for cross-compilation: {}",
            arch
        ),
    }
}

fn linux_musl_target_triple() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "aarch64" => Ok("aarch64-unknown-linux-musl"),
        "x86_64" => Ok("x86_64-unknown-linux-musl"),
        arch => bail!("unsupported host architecture: {}", arch),
    }
}

fn find_cargo_workspace() -> Result<PathBuf> {
    let output = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format=plain"])
        .output()
        .context("Failed to run cargo locate-project")?;
    if !output.status.success() {
        bail!("Failed to locate cargo workspace");
    }
    let path = String::from_utf8_lossy(&output.stdout);
    let cargo_toml = PathBuf::from(path.trim());
    cargo_toml
        .parent()
        .map(|p| p.to_path_buf())
        .context("Failed to determine workspace root from Cargo.toml path")
}

/// Returns true if the host is already Linux (no cross-compilation needed).
fn is_native_linux() -> bool {
    cfg!(target_os = "linux")
}

fn native_build(release: bool) -> Result<PathBuf> {
    let workspace = find_cargo_workspace()?;
    let target = linux_musl_target_triple()?;
    let profile = if release { "release" } else { "debug" };
    let profile_dir = if release { "release" } else { "debug" };

    println!("Building workmux ({}, {})...\n", profile, target);

    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--target", target]);
    if release {
        cmd.arg("--release");
    }
    cmd.current_dir(&workspace);

    let status = cmd.status().context("Failed to run cargo build")?;
    if !status.success() {
        bail!("Build failed");
    }

    let binary = workspace.join(format!("target/{}/{}/workmux", target, profile_dir));
    if !binary.exists() {
        bail!("Expected binary not found at {}", binary.display());
    }

    println!();
    Ok(binary)
}

fn cross_compile(target: &str, release: bool) -> Result<PathBuf> {
    // Check if target is installed
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .context("Failed to run rustup")?;
    let installed = String::from_utf8_lossy(&output.stdout);
    if !installed.lines().any(|l| l.trim() == target) {
        bail!(
            "Rust target {} is not installed.\n\
             Install it with: rustup target add {}",
            target,
            target
        );
    }

    // Check if cross-linker is available (unless CARGO_TARGET_*_LINKER is set)
    let linker_env = format!(
        "CARGO_TARGET_{}_LINKER",
        target.to_uppercase().replace('-', "_")
    );
    let linker_set = std::env::var(&linker_env).is_ok();
    if !linker_set && which::which(format!("{}-gcc", target)).is_err() {
        bail!(
            "Cross-linker {}-gcc not found and {} is not set.\n\
             Install with: brew install messense/macos-cross-toolchains/{}",
            target,
            linker_env,
            target,
        );
    }

    let workspace = find_cargo_workspace()?;
    let profile = if release { "release" } else { "debug" };
    let profile_dir = if release { "release" } else { "debug" };

    println!("Cross-compiling workmux for {} ({})...\n", target, profile);

    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--target", target]);
    if release {
        cmd.arg("--release");
    }
    cmd.current_dir(&workspace);

    // Set linker env var if not already set
    if !linker_set {
        cmd.env(&linker_env, format!("{}-gcc", target));
    }

    let status = cmd.status().context("Failed to run cargo build")?;
    if !status.success() {
        bail!("Cross-compilation failed");
    }

    let binary = workspace.join(format!("target/{}/{}/workmux", target, profile_dir));
    if !binary.exists() {
        bail!("Expected binary not found at {}", binary.display());
    }

    println!();
    Ok(binary)
}

fn install_to_vm(binary_path: &Path, vm_name: &str) -> Result<()> {
    // Use bash -c with $HOME for absolute paths because limactl shell sets
    // the working directory to the host cwd (if mounted), not $HOME.
    let mkdir = Command::new("limactl")
        .args([
            "shell",
            vm_name,
            "--",
            "bash",
            "-c",
            "mkdir -p \"$HOME/.local/bin\"",
        ])
        .output()
        .context("Failed to run limactl shell for mkdir")?;
    if !mkdir.status.success() {
        let stderr = String::from_utf8_lossy(&mkdir.stderr);
        bail!("Failed to create ~/.local/bin: {}", stderr.trim());
    }

    // Copy to temp location to avoid "text file busy"
    let tmp_dest = format!("{}:/tmp/workmux.new", vm_name);
    let cp = Command::new("limactl")
        .args(["cp", &binary_path.to_string_lossy(), &tmp_dest])
        .output()
        .context("Failed to run limactl cp")?;
    if !cp.status.success() {
        let stderr = String::from_utf8_lossy(&cp.stderr);
        bail!("Failed to copy binary: {}", stderr.trim());
    }

    // Move into place and make executable
    let install = Command::new("limactl")
        .args([
            "shell",
            vm_name,
            "--",
            "bash",
            "-c",
            "install -m 755 /tmp/workmux.new \"$HOME/.local/bin/workmux\"",
        ])
        .output()
        .context("Failed to run limactl shell for install")?;
    if !install.status.success() {
        let stderr = String::from_utf8_lossy(&install.stderr);
        bail!("Failed to install binary: {}", stderr.trim());
    }

    Ok(())
}

fn run_install_dev(skip_build: bool, release: bool) -> Result<()> {
    use crate::sandbox::lima::VM_PREFIX;

    let config = Config::load(None)?;
    let native = is_native_linux();

    // Build (or locate existing binary)
    let binary_path = if !skip_build {
        if native {
            native_build(release)?
        } else {
            let target = linux_target_triple()?;
            cross_compile(target, release)?
        }
    } else {
        let workspace = find_cargo_workspace()?;
        let profile_dir = if release { "release" } else { "debug" };
        let path = if native {
            let target = linux_musl_target_triple()?;
            workspace.join(format!("target/{}/{}/workmux", target, profile_dir))
        } else {
            let target = linux_target_triple()?;
            workspace.join(format!("target/{}/{}/workmux", target, profile_dir))
        };
        if !path.exists() {
            bail!(
                "No binary found at {}\nRun without --skip-build first.",
                path.display()
            );
        }
        path
    };

    let mut did_something = false;

    // --- Container image patching ---
    let agent = resolve_agent(&config);
    let image_name = config.sandbox.resolved_image(agent);
    match install_dev_container(&binary_path, &image_name, &config) {
        Ok(true) => {
            did_something = true;
        }
        Ok(false) => {
            // Image doesn't exist, skip silently
        }
        Err(e) => {
            eprintln!("Warning: failed to patch container image: {}", e);
        }
    }

    // --- Lima VM installation ---
    if LimaInstance::is_lima_available() {
        let instances = LimaInstance::list()?;
        let running: Vec<_> = instances
            .iter()
            .filter(|i| i.name.starts_with(VM_PREFIX) && i.is_running())
            .collect();

        if !running.is_empty() {
            println!(
                "Installing workmux into {} running VM(s)...\n",
                running.len()
            );
            let mut failed: Vec<(String, String)> = Vec::new();

            for vm in &running {
                print!("  {} ... ", vm.name);
                io::stdout().flush().ok();

                match install_to_vm(&binary_path, &vm.name) {
                    Ok(()) => println!("ok"),
                    Err(e) => {
                        println!("failed");
                        failed.push((vm.name.clone(), e.to_string()));
                    }
                }
            }

            if !failed.is_empty() {
                eprintln!("\nFailed to install to {} VM(s):", failed.len());
                for (name, error) in &failed {
                    eprintln!("  - {}: {}", name, error);
                }
                bail!("Some installations failed");
            }

            did_something = true;
        }
    }

    if !did_something {
        bail!(
            "Nothing to do: no container image '{}' found and no running Lima VMs.",
            image_name
        );
    }

    println!("\nDone.");
    Ok(())
}

/// Build a thin overlay image to replace workmux in the container image.
/// Returns Ok(true) if the image was patched, Ok(false) if the base image
/// doesn't exist.
fn install_dev_container(binary_path: &Path, image_name: &str, config: &Config) -> Result<bool> {
    let runtime = config.sandbox.runtime().binary_name();

    // Check if the base image exists
    let inspect = Command::new(runtime)
        .args(["image", "inspect", image_name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("Failed to run container runtime")?;

    if !inspect.success() {
        return Ok(false);
    }

    println!("Patching container image '{}'...", image_name);

    let temp_dir = tempfile::Builder::new()
        .prefix("workmux-install-dev-")
        .tempdir()
        .context("Failed to create temp directory")?;
    let context_path = temp_dir.path();

    // Copy binary to build context
    let dest = context_path.join("workmux");
    std::fs::copy(binary_path, &dest).context("Failed to copy binary")?;

    // Write overlay Dockerfile
    let dockerfile = format!("FROM {}\nCOPY workmux /usr/local/bin/workmux\n", image_name);
    std::fs::write(context_path.join("Dockerfile"), &dockerfile)
        .context("Failed to write Dockerfile")?;

    // Build, tagging as the same image name (replaces it in-place).
    // Pass the absolute context path as the argument instead of using
    // current_dir + "." because Apple Container doesn't resolve "." correctly
    // when current_dir differs from the process working directory.
    let status = Command::new(runtime)
        .env("DOCKER_CLI_HINTS", "false")
        .args(["build", "-t", image_name])
        .arg(context_path)
        .status()
        .with_context(|| format!("Failed to run {} build", runtime))?;

    if !status.success() {
        bail!("Container image patch build failed");
    }

    println!("  {} ... ok", image_name);
    Ok(true)
}

#[derive(Debug)]
struct VmInfo {
    name: String,
    status: String,
    created: Option<SystemTime>,
    last_accessed: Option<SystemTime>,
}

fn run_prune(force: bool) -> Result<()> {
    if !LimaInstance::is_lima_available() {
        bail!("limactl is not installed or not in PATH");
    }

    let output = Command::new("limactl")
        .arg("list")
        .arg("--json")
        .output()
        .context("Failed to execute limactl list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to list Lima instances: {}", stderr.trim());
    }

    let instances =
        parse_lima_instances(&output.stdout).context("Failed to parse limactl output")?;

    // Default Lima directory as fallback
    let default_lima_dir = home::home_dir()
        .context("Could not determine home directory")?
        .join(".lima");

    let mut vm_infos: Vec<VmInfo> = Vec::new();

    for instance in instances {
        if !instance.name.starts_with("wm-") {
            continue;
        }

        // Use the dir field from limactl output, fall back to default
        let vm_dir = instance
            .dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| default_lima_dir.join(&instance.name));

        let (created, last_accessed) = if vm_dir.exists() {
            let metadata = std::fs::metadata(&vm_dir)?;
            (metadata.created().ok(), metadata.accessed().ok())
        } else {
            (None, None)
        };

        vm_infos.push(VmInfo {
            name: instance.name,
            status: instance.status,
            created,
            last_accessed,
        });
    }

    if vm_infos.is_empty() {
        println!("No workmux Lima VMs found.");
        return Ok(());
    }

    // Display VM information
    println!("Found {} workmux Lima VM(s):\n", vm_infos.len());

    for (i, vm) in vm_infos.iter().enumerate() {
        println!("{}. {} ({})", i + 1, vm.name, vm.status);
        if let Some(created) = vm.created {
            println!("   Age: {}", format_duration_since(created));
        }
        if let Some(accessed) = vm.last_accessed {
            println!("   Last accessed: {}", format_duration_since(accessed));
        }
        println!();
    }

    // Confirm deletion unless --force
    if !force {
        print!("Delete all these VMs? [y/N] ");
        io::stdout().flush().context("Failed to flush stdout")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read input")?;

        if input.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Delete VMs
    println!("\nDeleting VMs...");
    let mut deleted_count: u64 = 0;
    let mut failed: Vec<(String, String)> = Vec::new();

    for vm in vm_infos {
        print!("  Deleting {}... ", vm.name);
        io::stdout().flush().ok();

        let result = Command::new("limactl")
            .arg("delete")
            .arg(&vm.name)
            .arg("--force")
            .output();

        match result {
            Ok(output) if output.status.success() => {
                println!("done");
                deleted_count += 1;

                // Clean up per-VM state directory
                if let Ok(state_dir) = lima::mounts::lima_state_dir_path(&vm.name)
                    && state_dir.exists()
                    && let Err(e) = std::fs::remove_dir_all(&state_dir)
                {
                    tracing::warn!(vm = %vm.name, error = %e, "failed to clean up state dir");
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                println!("failed");
                failed.push((vm.name, stderr.trim().to_string()));
            }
            Err(e) => {
                println!("failed");
                failed.push((vm.name, e.to_string()));
            }
        }
    }

    // Report results
    println!();
    if deleted_count > 0 {
        println!("Deleted {} VM(s).", deleted_count);
    }

    if !failed.is_empty() {
        eprintln!("\nFailed to delete {} VM(s):", failed.len());
        for (name, error) in &failed {
            eprintln!("  - {}: {}", name, error);
        }
        bail!("Some VMs could not be deleted");
    }

    Ok(())
}

/// Format duration since a timestamp as human-readable string.
fn format_duration_since(time: SystemTime) -> String {
    let now = SystemTime::now();

    let duration = match now.duration_since(time) {
        Ok(d) => d,
        Err(_) => return "in the future".to_string(),
    };

    let seconds = duration.as_secs();

    if seconds < 60 {
        return "just now".to_string();
    }

    let minutes = seconds / 60;
    if minutes < 60 {
        return format!(
            "{} minute{} ago",
            minutes,
            if minutes == 1 { "" } else { "s" }
        );
    }

    let hours = minutes / 60;
    if hours < 24 {
        return format!("{} hour{} ago", hours, if hours == 1 { "" } else { "s" });
    }

    let days = hours / 24;
    if days < 30 {
        return format!("{} day{} ago", days, if days == 1 { "" } else { "s" });
    }

    let months = days / 30;
    if months < 12 {
        return format!("{} month{} ago", months, if months == 1 { "" } else { "s" });
    }

    let years = months / 12;
    format!("{} year{} ago", years, if years == 1 { "" } else { "s" })
}

fn run_stop(name: Option<String>, all: bool, skip_confirm: bool) -> Result<()> {
    use crate::sandbox::lima::{LimaInstance, LimaInstanceInfo, VM_PREFIX};
    use std::io::{self, IsTerminal, Write};

    // Check if limactl is available
    if !LimaInstance::is_lima_available() {
        anyhow::bail!("limactl not found. Please install Lima first.");
    }

    // Get list of all workmux VMs
    let all_vms = LimaInstance::list()?;
    let workmux_vms: Vec<LimaInstanceInfo> = all_vms
        .into_iter()
        .filter(|vm| vm.name.starts_with(VM_PREFIX))
        .collect();

    // Filter to running VMs for display/selection
    let running_vms: Vec<&LimaInstanceInfo> =
        workmux_vms.iter().filter(|vm| vm.is_running()).collect();

    let vms_to_stop: Vec<&LimaInstanceInfo> = if all {
        // Stop all running VMs
        if running_vms.is_empty() {
            println!("No running workmux VMs found.");
            return Ok(());
        }
        running_vms
    } else if let Some(ref vm_name) = name {
        // Stop specific VM - check all VMs (not just running) for better error messages
        let vm = workmux_vms.iter().find(|v| v.name == *vm_name);
        match vm {
            Some(v) if v.is_running() => vec![v],
            Some(v) => {
                println!(
                    "VM '{}' is already stopped (status: {}).",
                    vm_name, v.status
                );
                return Ok(());
            }
            None => {
                anyhow::bail!(
                    "VM '{}' not found. Use 'workmux sandbox stop' to see available VMs.",
                    vm_name
                );
            }
        }
    } else {
        // Interactive mode: require TTY
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("Non-interactive stdin detected. Use --all or specify a VM name.");
        }

        if running_vms.is_empty() {
            println!("No running workmux VMs found.");
            return Ok(());
        }

        select_vms_interactive(&running_vms)?
    };

    if vms_to_stop.is_empty() {
        println!("No VMs selected.");
        return Ok(());
    }

    // Show what will be stopped
    println!("The following VMs will be stopped:");
    for vm in &vms_to_stop {
        println!("  - {} ({})", vm.name, vm.status);
    }

    // Confirm unless --yes flag is provided
    if !skip_confirm {
        print!(
            "\nAre you sure you want to stop {} VM(s)? [y/N] ",
            vms_to_stop.len()
        );
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        let answer = input.trim().to_ascii_lowercase();
        if !matches!(answer.as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Stop VMs
    let mut success_count = 0;
    let mut failed: Vec<(String, String)> = Vec::new();

    for vm in vms_to_stop {
        print!("Stopping {}... ", vm.name);
        io::stdout().flush()?;

        match LimaInstance::stop_by_name(&vm.name) {
            Ok(()) => {
                println!("✓");
                success_count += 1;
            }
            Err(e) => {
                println!("✗");
                failed.push((vm.name.clone(), e.to_string()));
            }
        }
    }

    // Report results
    if success_count > 0 {
        println!("\n✓ Successfully stopped {} VM(s)", success_count);
    }

    if !failed.is_empty() {
        eprintln!("\nFailed to stop {} VM(s):", failed.len());
        for (name, error) in &failed {
            eprintln!("  - {}: {}", name, error);
        }
        anyhow::bail!("Some VMs could not be stopped");
    }

    Ok(())
}

fn run_shell(exec: bool, command: Vec<String>) -> Result<()> {
    use crate::config::SandboxBackend;

    let config = Config::load(None)?;

    match config.sandbox.backend() {
        SandboxBackend::Container => run_shell_container(exec, command, &config),
        SandboxBackend::Lima => run_shell_lima(exec, command, &config),
    }
}

fn run_shell_container(exec: bool, command: Vec<String>, config: &Config) -> Result<()> {
    use crate::sandbox::network_proxy::NetworkProxy;
    use crate::state::StateStore;

    // Get current directory as worktree
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let worktree_root = cwd.clone();

    // Get handle from directory name
    let handle = worktree_root
        .file_name()
        .and_then(|n| n.to_str())
        .context("Could not determine worktree handle from directory name")?;

    // Build shell command
    let shell_cmd = if command.is_empty() {
        "bash".to_string()
    } else {
        command.join(" ")
    };

    if exec {
        // --exec attaches to an existing container which already has iptables
        // rules and proxy env vars in place -- no additional proxy needed.
        let store = StateStore::new().context("Failed to access state store")?;
        let containers = store.list_containers(handle);

        if containers.is_empty() {
            bail!(
                "No running container found for worktree '{}'. \n\
                 Start a sandbox first with 'workmux add --sandbox' or use 'workmux sandbox shell' without --exec.",
                handle
            );
        }

        // Use the first (usually only) container, with its stored runtime
        let (container_name, container_runtime) = &containers[0];
        let runtime_bin = container_runtime.binary_name();
        if containers.len() > 1 {
            let others: Vec<&str> = containers[1..].iter().map(|(n, _)| n.as_str()).collect();
            println!(
                "Multiple containers found, using: {} (others: {})",
                container_name,
                others.join(", ")
            );
        }

        debug!(
            runtime = runtime_bin,
            container = container_name.as_str(),
            cmd = shell_cmd,
            "exec into container"
        );

        let status = Command::new(runtime_bin)
            .args(["exec", "-it", container_name, "bash", "-c", &shell_cmd])
            .status()
            .with_context(|| format!("Failed to exec into container {}", container_name))?;

        std::process::exit(status.code().unwrap_or(1));
    } else {
        // Start new container
        sandbox::ensure_sandbox_config_dirs()?;
        let agent = resolve_agent(config);

        let network_deny = config.sandbox.network_policy_is_deny();

        // Start proxy if network deny mode is active
        let proxy = if network_deny {
            let allowed = config.sandbox.network.allowed_domains();
            let proxy = NetworkProxy::bind(allowed)?;
            let proxy_port = proxy.port();
            let proxy_token = proxy.token().to_string();
            let handle = proxy.spawn();
            Some((proxy_port, proxy_token, handle))
        } else {
            None
        };

        // Build env vars (owned, then borrowed -- same pattern as sandbox_run)
        let rpc_host = config.sandbox.resolved_rpc_host();
        let mut owned_envs: Vec<(String, String)> = Vec::new();

        if let Some((proxy_port, ref proxy_token, _)) = proxy {
            let proxy_url = format!("http://workmux:{}@{}:{}", proxy_token, rpc_host, proxy_port);
            let no_proxy = format!("localhost,127.0.0.1,{}", rpc_host);

            owned_envs.push(("HTTPS_PROXY".into(), proxy_url.clone()));
            owned_envs.push(("HTTP_PROXY".into(), proxy_url.clone()));
            owned_envs.push(("https_proxy".into(), proxy_url.clone()));
            owned_envs.push(("http_proxy".into(), proxy_url));
            owned_envs.push(("NO_PROXY".into(), no_proxy.clone()));
            owned_envs.push(("no_proxy".into(), no_proxy));
            owned_envs.push(("WM_PROXY_HOST".into(), rpc_host.clone()));
            owned_envs.push(("WM_PROXY_PORT".into(), proxy_port.to_string()));
        }

        let env_refs: Vec<(&str, &str)> = owned_envs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let mut docker_args = sandbox::build_docker_run_args(
            &shell_cmd,
            &config.sandbox,
            agent,
            &worktree_root,
            &cwd,
            &env_refs,
            None,
            network_deny,
        )?;

        // Add container name for easier identification
        docker_args.insert(1, "--name".to_string());
        docker_args.insert(2, format!("wm-shell-{}", std::process::id()));

        let runtime_bin = config.sandbox.runtime().binary_name();
        let redacted_args: Vec<_> = docker_args
            .iter()
            .map(|a| super::sandbox_run::redact_env_arg(a, &[]))
            .collect();
        debug!(runtime = runtime_bin, args = ?redacted_args, "starting shell container");

        let status = Command::new(runtime_bin)
            .args(&docker_args)
            .status()
            .with_context(|| format!("Failed to execute {} run", runtime_bin))?;

        std::process::exit(status.code().unwrap_or(1));
    }
}

fn run_shell_lima(exec: bool, command: Vec<String>, config: &Config) -> Result<()> {
    if exec {
        bail!(
            "The --exec flag is only supported with the container backend.\n\
             Lima VMs are persistent, so 'workmux sandbox shell' always connects to the existing VM."
        );
    }

    let cwd = std::env::current_dir().context("Failed to get current directory")?;

    // Ensure VM is running (creates it if needed)
    let vm_name = lima::ensure_vm_running(config, &cwd)?;

    // Build shell command
    let shell_cmd = if command.is_empty() {
        "bash".to_string()
    } else {
        command.join(" ")
    };

    debug!(vm = %vm_name, cmd = %shell_cmd, "starting Lima shell");

    let status = Command::new("limactl")
        .arg("shell")
        .args(["--workdir", &cwd.to_string_lossy()])
        .arg(&vm_name)
        .arg("--")
        .args(["bash", "-c", &shell_cmd])
        .status()
        .context("Failed to execute limactl shell")?;

    std::process::exit(status.code().unwrap_or(1));
}

fn select_vms_interactive<'a>(
    vms: &'a [&'a crate::sandbox::lima::LimaInstanceInfo],
) -> Result<Vec<&'a crate::sandbox::lima::LimaInstanceInfo>> {
    use std::io::{self, Write};

    println!("Running workmux VMs:");
    println!();
    for (idx, vm) in vms.iter().enumerate() {
        println!("  {}. {} ({})", idx + 1, vm.name, vm.status);
    }
    println!();
    println!("Enter VM number to stop (or 'all' for all VMs):");
    print!("> ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.eq_ignore_ascii_case("all") {
        return Ok(vms.to_vec());
    }

    // Parse as number
    let idx: usize = input
        .parse()
        .context("Invalid input. Please enter a number or 'all'.")?;

    if idx < 1 || idx > vms.len() {
        anyhow::bail!(
            "Invalid selection. Please choose a number between 1 and {}",
            vms.len()
        );
    }

    Ok(vec![vms[idx - 1]])
}
