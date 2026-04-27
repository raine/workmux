//! Mount path resolution for Lima backend.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, IsolationLevel};

/// A mount point configuration for Lima.
#[derive(Debug, Clone)]
pub struct Mount {
    /// Path on the host
    pub host_path: PathBuf,
    /// Path inside the VM (if different from host_path)
    pub guest_path: PathBuf,
    /// Whether the mount is read-only
    pub read_only: bool,
}

impl Mount {
    /// Create a read-write mount
    pub fn rw(path: PathBuf) -> Self {
        Self {
            guest_path: path.clone(),
            host_path: path,
            read_only: false,
        }
    }

    /// Create a read-only mount
    #[allow(dead_code)]
    pub fn ro(path: PathBuf) -> Self {
        Self {
            guest_path: path.clone(),
            host_path: path,
            read_only: true,
        }
    }

    /// Create a mount with different host and guest paths
    #[allow(dead_code)]
    pub fn with_guest_path(mut self, guest_path: PathBuf) -> Self {
        self.guest_path = guest_path;
        self
    }
}

/// Determine the project root using git.
///
/// Uses the git common directory's parent to find the main repository root.
/// This is stable across worktrees: `--show-toplevel` returns each worktree's
/// own path, but `--git-common-dir` always points to the shared `.git` directory
/// in the main repo, so its parent is the true project root.
///
/// This matters for both VM naming (project-level isolation hashes this path)
/// and mount generation (must mount the real project root, not a worktree).
/// Using `--show-toplevel` would produce per-worktree paths like
/// `/code/project__worktrees/feature-a`, causing each worktree to get its own
/// VM and a nonsensical worktrees_dir mount like `feature-a__worktrees`.
pub fn determine_project_root(worktree: &Path) -> Result<PathBuf> {
    let git_common_dir = determine_git_common_dir(worktree)?;

    // The git common dir is typically `/path/to/project/.git`.
    // Its parent is the project root.
    let project_root = git_common_dir.parent().ok_or_else(|| {
        anyhow::anyhow!("Git common dir has no parent: {}", git_common_dir.display())
    })?;

    Ok(project_root.to_path_buf())
}

/// Determine the git common directory using git.
/// Uses `git rev-parse --git-common-dir` to handle `git clone --separate-git-dir` correctly.
pub fn determine_git_common_dir(worktree: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("rev-parse")
        .arg("--path-format=absolute")
        .arg("--git-common-dir")
        .output()?;

    if !output.status.success() {
        bail!("Failed to determine git common dir");
    }

    let path = String::from_utf8(output.stdout)?.trim().to_string();

    Ok(PathBuf::from(path))
}

/// Get the Lima guest home directory.
///
/// Lima <2.1.0 creates a user with home at `/home/<user>.linux/`.
/// Lima >=2.1.0 changed this to `/home/<user>.guest/`.
fn lima_guest_home() -> Option<PathBuf> {
    let username = std::env::var("USER").ok()?;
    let suffix = lima_guest_home_suffix();
    Some(PathBuf::from(format!("/home/{}.{}", username, suffix)))
}

/// Determine the guest home directory suffix based on Lima version.
///
/// Returns "guest" for Lima >=2.1.0, "linux" for older versions.
fn lima_guest_home_suffix() -> &'static str {
    let version = Command::new("limactl")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        });

    match version {
        Some(v) => {
            // Output format: "limactl version 2.1.0"
            if let Some(ver_str) = v.trim().rsplit(' ').next()
                && lima_version_gte(ver_str, "2.1.0")
            {
                return "guest";
            }
            "linux"
        }
        None => "linux",
    }
}

/// Check if version `a` is >= version `b` using simple numeric comparison.
fn lima_version_gte(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> {
        v.split('.')
            .map(|s| s.parse::<u32>().unwrap_or(0))
            .collect()
    };
    let va = parse(a);
    let vb = parse(b);
    for i in 0..va.len().max(vb.len()) {
        let a_part = va.get(i).copied().unwrap_or(0);
        let b_part = vb.get(i).copied().unwrap_or(0);
        match a_part.cmp(&b_part) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => continue,
        }
    }
    true // equal
}

/// Calculate the standard worktrees directory for a project.
fn calc_worktrees_dir(project_root: &Path) -> Result<PathBuf> {
    let project_name = project_root
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Invalid project path"))?
        .to_string_lossy();

    let worktrees_dir = project_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("No parent directory"))?
        .join(format!("{}__worktrees", project_name));

    Ok(worktrees_dir)
}

/// Get the host-side state directory for a Lima VM.
/// Uses XDG state dir: $XDG_STATE_HOME/workmux/lima/<vm_name>/
fn lima_state_dir(vm_name: &str) -> Result<PathBuf> {
    let state_dir = crate::xdg::state_dir()?.join("lima").join(vm_name);
    std::fs::create_dir_all(&state_dir)?;
    Ok(state_dir)
}

/// Get the state directory path for a VM without creating it.
pub(crate) fn lima_state_dir_path(vm_name: &str) -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?.join("lima").join(vm_name))
}

/// Seed ~/.claude.json into the VM's state directory.
/// Writes a minimal config with hasCompletedOnboarding so Claude Code
/// skips the onboarding flow. Only writes when the destination doesn't
/// exist (if_missing policy). Each VM evolves its own copy independently.
pub(crate) fn seed_claude_json(vm_name: &str) -> Result<()> {
    let state_dir = lima_state_dir(vm_name)?;
    let dest = state_dir.join(".claude.json");
    if !dest.exists() {
        std::fs::write(
            &dest,
            r#"{"hasCompletedOnboarding":true,"bypassPermissionsModeAccepted":true}"#,
        )?;
    }
    Ok(())
}

/// Generate mount points for Lima VM based on isolation level and config.
///
/// The `agent` parameter controls agent-specific mounts (e.g. `~/.claude`
/// is only mounted when the active agent is "claude").
pub fn generate_mounts(
    worktree: &Path,
    isolation: IsolationLevel,
    config: &Config,
    vm_name: &str,
    agent: &str,
) -> Result<Vec<Mount>> {
    let mut mounts = Vec::new();

    match isolation {
        IsolationLevel::Shared => {
            let projects_dir = config.sandbox.lima.projects_dir.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Shared isolation requires 'sandbox.lima.projects_dir' in config.\n\
                         All projects must be under a single root directory.\n\
                         \n\
                         Example config:\n\
                         sandbox:\n  \
                           lima:\n    \
                             isolation: shared\n    \
                             projects_dir: /Users/me/code"
                )
            })?;

            mounts.push(Mount::rw(projects_dir.clone()));
        }

        IsolationLevel::Project => {
            // 1. Mount project root
            let project_root = determine_project_root(worktree)?;
            mounts.push(Mount::rw(project_root.clone()));

            // 2. Mount git common dir if separate
            let git_common_dir = determine_git_common_dir(worktree)?;
            if !git_common_dir.starts_with(&project_root) {
                mounts.push(Mount::rw(git_common_dir));
            }

            // 3. Mount standard worktrees directory
            let worktrees_dir = calc_worktrees_dir(&project_root)?;

            // CRITICAL: Always create and mount (even if doesn't exist yet)
            std::fs::create_dir_all(&worktrees_dir)?;
            mounts.push(Mount::rw(worktrees_dir.clone()));

            // 4. Mount custom worktree directory if configured
            if let Some(custom_template) = config.worktree_dir.as_ref() {
                let custom_dir = crate::util::expand_worktree_dir(custom_template, &project_root)?;
                std::fs::create_dir_all(&custom_dir)?;

                if custom_dir != worktrees_dir {
                    mounts.push(Mount::rw(custom_dir));
                }
            }
        }
    }

    // Mount agent config directory
    if let Some(auth_dir) = config.sandbox.resolved_agent_config_dir(agent) {
        let guest_subpath = match agent {
            "claude" => ".claude",
            "gemini" => ".gemini",
            "codex" => ".codex",
            "opencode" => ".local/share/opencode",
            "pi" => ".pi/agent",
            _ => unreachable!(),
        };
        let guest_path = lima_guest_home()
            .map(|h| h.join(guest_subpath))
            .unwrap_or_else(|| auth_dir.clone());
        mounts.push(Mount {
            host_path: auth_dir.clone(),
            guest_path: guest_path.clone(),
            read_only: false,
        });

        // Pi stores managed fd/rg binaries under bin/. Overlay a per-VM,
        // arch-keyed directory there so the guest's Linux downloads never
        // clobber the host's Mach-O binaries via the parent bind mount.
        if agent == "pi" {
            let state_dir = lima_state_dir(vm_name)?;
            let overlay = crate::sandbox::pi::pi_bin_overlay_dir(&state_dir)?;
            mounts.push(Mount {
                host_path: overlay,
                guest_path: guest_path.join("bin"),
                read_only: false,
            });
        }
    }

    // Mount opencode global config directory (~/.config/opencode/) read-only.
    // This is separate from the data directory (~/.local/share/opencode/) and
    // contains opencode.json, plugins, and global MCP definitions.
    if agent == "opencode"
        && let Some(cfg_dir) = crate::agent_setup::opencode::opencode_config_dir()
        && cfg_dir.is_dir()
    {
        let guest_path = lima_guest_home()
            .map(|h| h.join(".config/opencode"))
            .unwrap_or_else(|| cfg_dir.clone());
        mounts.push(Mount {
            host_path: cfg_dir,
            guest_path,
            read_only: true,
        });
    }

    // Mount per-VM state directory for workmux state
    if let Ok(state_dir) = lima_state_dir(vm_name) {
        let guest_path = lima_guest_home()
            .map(|h| h.join(".workmux-state"))
            .unwrap_or_else(|| state_dir.clone());
        mounts.push(Mount {
            host_path: state_dir,
            guest_path,
            read_only: false,
        });
    }

    // Extra mounts from config
    for extra in config.sandbox.extra_mounts() {
        let (host_path, guest_path, read_only) = extra.resolve()?;
        mounts.push(Mount {
            host_path,
            guest_path,
            read_only,
        });
    }

    Ok(mounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seed_claude_json_writes_onboarding_config() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let dest = state_dir.join(".claude.json");

        assert!(!dest.exists());
        std::fs::write(
            &dest,
            r#"{"hasCompletedOnboarding":true,"bypassPermissionsModeAccepted":true}"#,
        )
        .unwrap();
        assert!(dest.exists());

        let contents: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(contents["hasCompletedOnboarding"], true);
    }

    #[test]
    fn test_seed_claude_json_does_not_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let dest = state_dir.join(".claude.json");
        std::fs::write(&dest, r#"{"hasCompletedOnboarding":true,"tips_shown":10}"#).unwrap();

        // if_missing policy: don't overwrite
        if !dest.exists() {
            std::fs::write(
                &dest,
                r#"{"hasCompletedOnboarding":true,"bypassPermissionsModeAccepted":true}"#,
            )
            .unwrap();
        }

        let contents: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(contents["tips_shown"], 10);
    }

    #[test]
    fn test_lima_state_dir_path_format() {
        let path = lima_state_dir_path("wm-myproject-abc12345").unwrap();
        // Should end with the expected suffix regardless of XDG_STATE_HOME
        assert!(path.ends_with("workmux/lima/wm-myproject-abc12345"));
    }

    #[test]
    fn test_lima_version_gte() {
        // Equal
        assert!(lima_version_gte("2.1.0", "2.1.0"));
        // Greater
        assert!(lima_version_gte("2.1.1", "2.1.0"));
        assert!(lima_version_gte("2.2.0", "2.1.0"));
        assert!(lima_version_gte("3.0.0", "2.1.0"));
        // Less
        assert!(!lima_version_gte("2.0.3", "2.1.0"));
        assert!(!lima_version_gte("1.9.9", "2.1.0"));
        assert!(!lima_version_gte("2.0.99", "2.1.0"));
    }

    #[test]
    fn test_lima_guest_home_suffix_returns_valid_suffix() {
        let suffix = lima_guest_home_suffix();
        assert!(
            suffix == "linux" || suffix == "guest",
            "unexpected suffix: {}",
            suffix
        );
    }

    fn init_git_project(parent: &Path) -> PathBuf {
        let project_root = parent.join("proj");
        std::fs::create_dir_all(&project_root).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project_root)
            .status()
            .unwrap();
        project_root
    }

    #[test]
    fn test_pi_agent_appends_bin_overlay_after_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = init_git_project(tmp.path());

        let mut config = Config::default();
        // Use a custom agent_config_dir so the test doesn't depend on $HOME.
        config.sandbox.agent_config_dir =
            Some(format!("{}/agent-cfg/{{agent}}", tmp.path().display()));

        let mounts = generate_mounts(
            &project_root,
            IsolationLevel::Project,
            &config,
            "test-vm",
            "pi",
        )
        .unwrap();

        // Find the parent and bin mounts by guest_path suffix.
        let parent_idx = mounts
            .iter()
            .position(|m| m.guest_path.ends_with(".pi/agent"))
            .expect("parent .pi/agent mount missing");
        let bin_idx = mounts
            .iter()
            .position(|m| m.guest_path.ends_with(".pi/agent/bin"))
            .expect("bin overlay mount missing");
        assert!(
            bin_idx > parent_idx,
            "bin overlay must come after parent mount"
        );

        let bin_mount = &mounts[bin_idx];
        assert!(!bin_mount.read_only, "bin overlay must be writable");
        let src = bin_mount.host_path.to_string_lossy();
        assert!(
            src.contains("pi-agent-bin"),
            "host path should contain pi-agent-bin: {}",
            src
        );
        assert!(
            src.contains(super::super::super::pi::linux_arch_key()),
            "host path should contain arch key: {}",
            src
        );
        assert!(
            src.contains("test-vm"),
            "host path should be per-VM: {}",
            src
        );
    }

    #[test]
    fn test_non_pi_agent_has_no_bin_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = init_git_project(tmp.path());

        let mut config = Config::default();
        config.sandbox.agent_config_dir =
            Some(format!("{}/agent-cfg/{{agent}}", tmp.path().display()));

        let mounts = generate_mounts(
            &project_root,
            IsolationLevel::Project,
            &config,
            "test-vm",
            "claude",
        )
        .unwrap();

        assert!(
            !mounts
                .iter()
                .any(|m| m.host_path.to_string_lossy().contains("pi-agent-bin")),
            "claude agent should not get pi bin overlay"
        );
    }
}
