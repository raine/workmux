//! Mount path resolution for Lima backend.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

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
    let output = crate::git::pinned_git(worktree)?
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

/// Guest mount points for host directories that belong in the guest home.
///
/// Mount points are baked into the instance config before the VM exists, but
/// the guest home is not knowable then: Lima renames the guest user (moving
/// `$HOME` with it) when the local username is not a valid Linux username, and
/// the home suffix varies by Lima version. Mounting to fixed paths and linking
/// them into the home from a provision script, which runs inside the VM where
/// `$HOME` is known, avoids predicting any of it. See [`home_links`].
const GUEST_AGENT_CONFIG: &str = "/mnt/workmux/agent-config";
const GUEST_OPENCODE_CONFIG: &str = "/mnt/workmux/opencode-config";
const GUEST_STATE: &str = "/mnt/workmux/state";

/// Path, relative to the guest home, where an agent looks for its config.
///
/// Must stay in sync with `SandboxConfig::resolved_agent_config_dir`, which
/// picks the host directory these mirror.
fn guest_agent_config_subpath(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => ".claude",
        "copilot" => ".copilot",
        "gemini" => ".gemini",
        "agy" => ".gemini/antigravity-cli",
        "codex" => ".codex",
        "opencode" => ".local/share/opencode",
        "pi" => ".pi/agent",
        "omp" => ".omp/agent",
        _ => return None,
    })
}

/// Symlinks a provision script must create so the guest home points at the
/// mounts, as (mount point, path relative to `$HOME`).
///
/// Derived from the generated mounts so a mount that was skipped (e.g. no
/// opencode config on the host) does not get a dangling link.
pub fn home_links(agent: &str, mounts: &[Mount]) -> Vec<(&'static str, &'static str)> {
    let mounted = |target: &str| mounts.iter().any(|m| m.guest_path == Path::new(target));

    let mut links = Vec::new();
    if mounted(GUEST_AGENT_CONFIG)
        && let Some(subpath) = guest_agent_config_subpath(agent)
    {
        links.push((GUEST_AGENT_CONFIG, subpath));
    }
    if mounted(GUEST_OPENCODE_CONFIG) {
        links.push((GUEST_OPENCODE_CONFIG, ".config/opencode"));
    }
    if mounted(GUEST_STATE) {
        links.push((GUEST_STATE, ".workmux-state"));
    }
    links
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
    lima_state_dir_with_root(vm_name, None)
}

fn lima_state_dir_with_root(vm_name: &str, state_root: Option<&Path>) -> Result<PathBuf> {
    let state_dir = match state_root {
        Some(root) => root.join("lima").join(vm_name),
        None => crate::xdg::state_dir()?.join("lima").join(vm_name),
    };
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
    generate_mounts_with_state_root(worktree, isolation, config, vm_name, agent, None)
}

fn generate_mounts_with_state_root(
    worktree: &Path,
    isolation: IsolationLevel,
    config: &Config,
    vm_name: &str,
    agent: &str,
    state_root: Option<&Path>,
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
        let guest_path = PathBuf::from(GUEST_AGENT_CONFIG);
        mounts.push(Mount {
            host_path: auth_dir.clone(),
            guest_path: guest_path.clone(),
            read_only: false,
        });

        // Pi stores managed fd/rg binaries under bin/. Overlay a per-VM,
        // arch-keyed directory there so the guest's Linux downloads never
        // clobber the host's Mach-O binaries via the parent bind mount.
        if agent == "pi" {
            let state_dir = lima_state_dir_with_root(vm_name, state_root)?;
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
        mounts.push(Mount {
            host_path: cfg_dir,
            guest_path: PathBuf::from(GUEST_OPENCODE_CONFIG),
            read_only: true,
        });
    }

    // Mount per-VM state directory for workmux state
    if let Ok(state_dir) = lima_state_dir(vm_name) {
        mounts.push(Mount {
            host_path: state_dir,
            guest_path: PathBuf::from(GUEST_STATE),
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
    fn test_home_links_follow_generated_mounts() {
        let mounts = vec![
            Mount::rw(PathBuf::from("/Users/test/code")),
            Mount {
                host_path: PathBuf::from("/Users/test/.claude"),
                guest_path: PathBuf::from(GUEST_AGENT_CONFIG),
                read_only: false,
            },
            Mount {
                host_path: PathBuf::from("/Users/test/.local/state/workmux/lima/wm-test"),
                guest_path: PathBuf::from(GUEST_STATE),
                read_only: false,
            },
        ];

        assert_eq!(
            home_links("claude", &mounts),
            vec![
                (GUEST_AGENT_CONFIG, ".claude"),
                (GUEST_STATE, ".workmux-state")
            ]
        );
    }

    #[test]
    fn test_home_links_skips_absent_mounts() {
        // No opencode config on the host means no mount, so no dangling link.
        let mounts = vec![Mount {
            host_path: PathBuf::from("/Users/test/.local/share/opencode"),
            guest_path: PathBuf::from(GUEST_AGENT_CONFIG),
            read_only: false,
        }];

        assert_eq!(
            home_links("opencode", &mounts),
            vec![(GUEST_AGENT_CONFIG, ".local/share/opencode")]
        );
    }

    #[test]
    fn test_home_links_empty_for_unknown_agent() {
        let mounts = vec![Mount {
            host_path: PathBuf::from("/Users/test/.config/custom"),
            guest_path: PathBuf::from(GUEST_AGENT_CONFIG),
            read_only: false,
        }];

        assert!(home_links("kiro", &mounts).is_empty());
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

    fn custom_agent_config_dir(tmp_path: &Path) -> String {
        format!("{}/agent-cfg/{{agent}}", tmp_path.display())
    }

    fn project_mounts_for_test(tmp_path: &Path, vm_name: &str, agent_name: &str) -> Vec<Mount> {
        let project_root = init_git_project(tmp_path);
        let mut config = Config::default();
        config.sandbox.agent_config_dir = Some(custom_agent_config_dir(tmp_path));

        generate_mounts_with_state_root(
            &project_root,
            IsolationLevel::Project,
            &config,
            vm_name,
            agent_name,
            Some(tmp_path),
        )
        .unwrap()
    }

    fn agent_config_host_path(tmp_path: &Path, agent_name: &str) -> PathBuf {
        tmp_path.join("agent-cfg").join(agent_name)
    }

    #[test]
    fn test_pi_agent_appends_bin_overlay_after_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let mounts = project_mounts_for_test(tmp.path(), "test-vm", "pi");
        let parent_host_path = agent_config_host_path(tmp.path(), "pi");

        let parent_idx = mounts
            .iter()
            .position(|m| m.host_path == parent_host_path)
            .expect("parent pi agent mount missing");
        let parent_guest_path = mounts[parent_idx].guest_path.clone();
        let bin_idx = mounts
            .iter()
            .position(|m| m.guest_path == parent_guest_path.join("bin"))
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
    fn test_omp_agent_mounts_agent_dir_without_bin_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let mounts = project_mounts_for_test(tmp.path(), "test-vm", "omp");

        let omp_host_path = agent_config_host_path(tmp.path(), "omp");
        let parent_mount = mounts
            .iter()
            .find(|m| m.host_path == omp_host_path)
            .expect("parent omp agent mount missing");
        assert!(
            !mounts
                .iter()
                .any(|m| m.guest_path == parent_mount.guest_path.join("bin")),
            "omp agent should not get a bin overlay"
        );
        assert!(
            !mounts
                .iter()
                .any(|m| m.host_path.to_string_lossy().contains("pi-agent-bin")),
            "omp agent should not get pi bin overlay"
        );
    }

    #[test]
    fn test_non_pi_agent_has_no_bin_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let mounts = project_mounts_for_test(tmp.path(), "test-vm", "claude");

        assert!(
            !mounts
                .iter()
                .any(|m| m.host_path.to_string_lossy().contains("pi-agent-bin")),
            "claude agent should not get pi bin overlay"
        );
    }
}
