//! Lima VM instance management.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;
use tracing::{debug, info};

use crate::config::Config;

/// Lima instance information from `limactl list --json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LimaInstanceInfo {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub dir: Option<String>,
}

impl LimaInstanceInfo {
    /// Check if the instance is running.
    pub fn is_running(&self) -> bool {
        self.status == "Running"
    }
}

/// Parse NDJSON output from `limactl list --json` (one JSON object per line).
pub fn parse_lima_instances(stdout: &[u8]) -> Result<Vec<LimaInstanceInfo>> {
    std::str::from_utf8(stdout)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str::<LimaInstanceInfo>(l)
                .with_context(|| format!("Failed to parse limactl row: {}", l))
        })
        .collect()
}

/// VM state detected from `limactl list`.
pub(crate) enum VmState {
    /// VM is already running, no boot needed
    Running,
    /// VM exists but is stopped, needs `limactl start <name>`
    Stopped,
    /// VM doesn't exist, needs `limactl start --name <name> <config>`
    NotFound,
}

/// Check the current state of a Lima VM by name.
pub(crate) fn check_vm_state(vm_name: &str) -> Result<VmState> {
    let instances = LimaInstance::list()?;

    match instances.iter().find(|i| i.name == vm_name) {
        Some(info) if info.is_running() => Ok(VmState::Running),
        Some(_) => Ok(VmState::Stopped),
        None => Ok(VmState::NotFound),
    }
}

/// Lima VM operations.
pub struct LimaInstance;

impl LimaInstance {
    /// Check if limactl is available on the system.
    pub fn is_lima_available() -> bool {
        Command::new("limactl")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// List all Lima instances.
    pub fn list() -> Result<Vec<LimaInstanceInfo>> {
        let output = Command::new("limactl")
            .arg("list")
            .arg("--json")
            .output()
            .context("Failed to list Lima instances")?;

        if !output.status.success() {
            bail!("Failed to list Lima instances");
        }

        parse_lima_instances(&output.stdout)
    }

    /// Stop a Lima VM by name. This is idempotent -- succeeds if the VM is already stopped.
    pub fn stop_by_name(name: &str) -> Result<()> {
        let output = Command::new("limactl")
            .arg("stop")
            .arg(name)
            .output()
            .with_context(|| format!("Failed to execute limactl stop for '{}'", name))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Treat "not running" as success for idempotency
            if stderr.contains("not running") {
                return Ok(());
            }
            bail!("Failed to stop Lima VM '{}': {}", name, stderr);
        }

        Ok(())
    }
}

/// Ensure a Lima VM is running for the given worktree.
///
/// Checks the VM state and boots it if necessary, showing a spinner with
/// streaming limactl output in the user's terminal. Should be called from
/// the main process BEFORE creating tmux panes.
///
/// Returns the VM name for use by `wrap_for_lima()`.
fn acknowledge_reduced_git_isolation(config: &Config, vm_name: &str) -> Result<()> {
    use std::io::{IsTerminal, Write};

    let state_dir = crate::xdg::state_dir()?
        .join("lima-acknowledgements")
        .join("git-metadata-isolation-v1");
    let acknowledgement = state_dir.join(vm_name);
    if let Ok(metadata) = std::fs::symlink_metadata(&acknowledgement) {
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || !matches!(
                std::fs::read_to_string(&acknowledgement).as_deref(),
                Ok("accepted-v1\n")
            )
        {
            bail!(
                "Invalid Lima reduced-isolation acknowledgement at {}",
                acknowledgement.display()
            );
        }
        return Ok(());
    }

    eprintln!(
        "warning: Lima exposes broad writable host mounts to a privileged guest. Git metadata control files cannot be isolated from ordinary writable Git data on this backend. A Lima guest can modify Git policy later consumed by host Git. Docker, Podman, and Apple Container provide the enforced Git metadata boundary."
    );

    let accepted = if config.sandbox.lima.accepts_reduced_git_metadata_isolation() {
        true
    } else if std::io::stdin().is_terminal() {
        eprint!("Continue with reduced Git metadata isolation? [y/N] ");
        std::io::stderr().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        input.trim().eq_ignore_ascii_case("y")
    } else {
        false
    };

    if !accepted {
        bail!(
            "Lima requires explicit acceptance of reduced Git metadata isolation. Set sandbox.lima.accept_reduced_git_metadata_isolation: true for unattended use, or use Docker, Podman, or Apple Container."
        );
    }

    std::fs::create_dir_all(&state_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&acknowledgement)?;
        file.write_all(b"accepted-v1\n")?;
    }
    #[cfg(not(unix))]
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&acknowledgement)?
        .write_all(b"accepted-v1\n")?;
    Ok(())
}

pub fn ensure_vm_running(config: &Config, worktree_path: &Path) -> Result<String> {
    if !LimaInstance::is_lima_available() {
        bail!(
            "Lima backend is enabled but limactl is not installed.\n\
             Install Lima: https://lima-vm.io/docs/installation/\n\
             Or disable sandbox: set 'sandbox.enabled: false' in config."
        );
    }

    let isolation = config.sandbox.lima.isolation();
    let vm_name = super::instance_name(worktree_path, isolation.clone(), config)?;
    acknowledge_reduced_git_isolation(config, &vm_name)?;

    debug!(vm_name = %vm_name, "checking Lima VM state");
    let vm_state = check_vm_state(&vm_name)?;

    match vm_state {
        VmState::Running => {
            debug!(vm_name = %vm_name, "Lima VM already running");
            if config.sandbox.lima.provision_script().is_some() {
                info!(vm_name = %vm_name, "custom provision script only runs on first VM creation; recreate VM to apply changes");
            }
        }
        VmState::Stopped => {
            info!(vm_name = %vm_name, "starting stopped Lima VM");
            if config.sandbox.lima.provision_script().is_some() {
                info!(vm_name = %vm_name, "custom provision script only runs on first VM creation; recreate VM to apply changes");
            }
            let msg = format!("Starting Lima VM {}", vm_name);
            let mut cmd = Command::new("limactl");
            cmd.args(["start", "--tty=false", "--progress", &vm_name]);

            let start = std::time::Instant::now();
            match crate::spinner::with_streaming_command_formatted(&msg, cmd, move |line| {
                super::log_format::format_lima_log_line(line, &start)
            }) {
                Ok(()) => {}
                Err(_) => {
                    // Race condition: another process may have started the VM.
                    // Re-check state before failing.
                    if matches!(check_vm_state(&vm_name)?, VmState::Running) {
                        return Ok(vm_name);
                    }
                    bail!("Failed to start Lima VM '{}'", vm_name);
                }
            }
        }
        VmState::NotFound => {
            info!(vm_name = %vm_name, "creating new Lima VM");

            let agent = crate::multiplexer::agent::resolve_profile_with_type(
                config.agent.as_deref(),
                config.agent_type.as_deref(),
            )
            .name();

            // Only generate config and mounts when we need to create a new VM
            let mounts = super::generate_mounts(worktree_path, isolation, config, &vm_name, agent)?;

            eprintln!("  Mounts:");
            for m in &mounts {
                if m.host_path == m.guest_path {
                    eprintln!("    {} (rw)", m.host_path.display());
                } else {
                    eprintln!(
                        "    {} -> {} ({})",
                        m.host_path.display(),
                        m.guest_path.display(),
                        if m.read_only { "ro" } else { "rw" }
                    );
                }
            }

            // Resolve toolchain: only install Nix/Devbox if the project has
            // devbox.json or flake.nix (or the user explicitly set devbox/flake)
            let needs_nix = {
                use crate::sandbox::toolchain::{DetectedToolchain, resolve_toolchain};
                resolve_toolchain(&config.sandbox.toolchain(), worktree_path)
                    != DetectedToolchain::None
            };

            let lima_config =
                super::generate_lima_config(&vm_name, &mounts, &config.sandbox, agent, needs_nix)?;

            let config_path = std::env::temp_dir().join(format!("workmux-lima-{}.yaml", vm_name));
            std::fs::write(&config_path, &lima_config).with_context(|| {
                format!("Failed to write Lima config to {}", config_path.display())
            })?;

            let msg = format!("Creating Lima VM {}", vm_name);
            let mut cmd = Command::new("limactl");
            cmd.args([
                "start",
                "--name",
                &vm_name,
                "--tty=false",
                "--progress",
                &config_path.to_string_lossy(),
            ]);

            let start = std::time::Instant::now();
            match crate::spinner::with_streaming_command_formatted(&msg, cmd, move |line| {
                super::log_format::format_lima_log_line(line, &start)
            }) {
                Ok(()) => {}
                Err(_) => {
                    // Race condition: another process may have created the VM.
                    // Re-check state before failing.
                    if matches!(check_vm_state(&vm_name)?, VmState::Running) {
                        return Ok(vm_name);
                    }
                    bail!("Failed to create Lima VM '{}'", vm_name);
                }
            }
        }
    }

    info!(vm_name = %vm_name, "Lima VM ready");
    Ok(vm_name)
}
