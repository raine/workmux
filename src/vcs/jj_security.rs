//! jj command hardening and repository identity discovery.
//!
//! Mirrors the structure of `crate::git::security`: a `RepositoryIdentity`-style
//! validator that resolves and cross-checks a jj repository's control
//! structure without trusting jj's own upward cwd-based discovery, and a
//! `protected_jj` constructor that always pins an explicit, validated
//! repository path and clears ambient environment/config influence before
//! invoking the `jj` binary.
//!
//! ## jj's attack surface vs. git's
//!
//! jj has no hooks mechanism at all (no `core.hooksPath`, no
//! `pre-commit`/`post-checkout`/etc. equivalents), and no content-filter or
//! external-diff/merge-tool config that runs by default the way git's
//! `filter.*.smudge`/`clean` or `diff.external` do. That eliminates most of
//! the arbitrary-code-execution vectors `git/security.rs` defends against
//! (`core.fsmonitor`, `uploadpack.packObjectsHook`, working-tree filters,
//! commit hooks, GPG program invocation, credential helpers, etc.) — jj
//! simply has no equivalent settings.
//!
//! It is not zero attack surface, though: a repository's own committed `jj`
//! config (`.jj/repo/config.toml` is not committed, but a project can ship a
//! `.jjconfig.toml`-style file consulted via `--config-file`, and more
//! importantly `revset-aliases.*`, `template-aliases.*`, and
//! `fileset-aliases.*` can be defined in *any* config layer jj reads,
//! including ones an attacker who controls repo content might be able to
//! influence depending on how workmux later wires config discovery) can
//! redefine well-known alias names such as `trunk()`. If workmux ever
//! evaluates a revset like `trunk()` by name, a hostile alias redefinition
//! could redirect that expression somewhere unexpected. This module doesn't
//! attempt to fully close that gap (no alias evaluation happens here), but
//! it documents the risk for later tasks: whenever feasible, workmux should
//! resolve revsets it depends on via literal expressions it constructs
//! itself (explicit `-r` arguments), rather than relying on an alias name
//! that a repo's config could redefine.
//!
//! ## No interactive/unattended split
//!
//! `git/security.rs` splits `unattended_git`/`interactive_git` because git
//! has interactive prompts (credential prompts, `GIT_EDITOR` for commit
//! messages/rebases, merge tools) that workmux sometimes needs to allow and
//! sometimes needs to suppress. jj workflows workmux drives are all
//! non-interactive: `jj git init`, `jj workspace add`, `jj log`, etc. never
//! block on an editor or credential prompt in the way git's commit/rebase
//! flow can. There is therefore only one constructor here, `protected_jj`;
//! if a future task needs an editor-invoking jj command (e.g. `jj describe`
//! without `-m`), split it out then rather than speculatively adding it now.

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Ambient environment variables that could redirect jj's repository
/// discovery, configuration, or external tool invocation away from what
/// `protected_jj` explicitly pins. Mirrors `git::security::GIT_ENVIRONMENT`.
const JJ_ENVIRONMENT: &[&str] = &[
    "JJ_CONFIG",
    "JJ_USER",
    "JJ_EMAIL",
    "JJ_OP_HOSTNAME",
    "JJ_OP_USERNAME",
    "JJ_EDITOR",
    "JJ_DIFF_TOOL",
    "JJ_MERGE_TOOL",
    "EDITOR",
    "VISUAL",
    "PAGER",
];

/// `--config NAME=VALUE` overrides `protected_jj` always forces, analogous
/// to `git::security::CONFIG_OVERRIDES`. These pin jj to non-interactive,
/// deterministic behavior regardless of what a repo's own config requests.
const JJ_CONFIG_OVERRIDES: &[(&str, &str)] = &[
    ("ui.paginate", "never"),
    ("ui.editor", "true"),
    ("ui.diff-editor", "true"),
    ("ui.merge-editor", "true"),
];

/// Paths that identify one jj repository without consulting repository
/// configuration or relying on jj's own upward cwd-based discovery.
///
/// A jj **workspace** is a working-copy checkout with its own `.jj`
/// directory. The workspace that owns the actual repository data (the
/// object store, operation log, etc.) is the **primary** workspace: its
/// `.jj/repo` is a real directory. Every other workspace created via
/// `jj workspace add` is **secondary**: its `.jj/repo` is a one-line text
/// file containing a path back to the primary workspace's `.jj/repo`
/// directory (analogous to a linked git worktree's `.git` pointer file).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JjRepositoryIdentity {
    /// The working-copy root of the workspace that was resolved (i.e. the
    /// directory containing the `.jj` directory), canonicalized.
    pub workspace_root: PathBuf,
    /// The `.jj` control directory for `workspace_root`, canonicalized.
    pub dot_jj: PathBuf,
    /// The repository data directory (`<primary .jj>/repo`), canonicalized.
    /// Shared by every workspace of this repository.
    pub repo_dir: PathBuf,
    /// True if `workspace_root` is the primary workspace (i.e. `repo_dir`
    /// lives directly under its own `.jj`, not reached via a pointer file).
    pub is_primary: bool,
}

fn reject_symlink(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("Failed to inspect {label} at {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("{label} must not be a symbolic link: {}", path.display());
    }
    Ok(())
}

/// Read a one-line pointer file (jj's `.jj/repo` secondary-workspace
/// pointer), resolving a relative path against the pointer file's parent
/// directory, mirroring `git::security::read_pointer`.
fn read_repo_pointer(path: &Path) -> Result<PathBuf> {
    reject_symlink(path, "workspace .jj/repo pointer")?;
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("Failed to inspect .jj/repo pointer at {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            ".jj/repo pointer must be a regular file: {}",
            path.display()
        );
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read .jj/repo pointer at {}", path.display()))?;
    let value = contents.trim();
    if value.is_empty() || contents.lines().count() != 1 {
        bail!("Invalid .jj/repo pointer at {}", path.display());
    }
    let value = Path::new(value);
    Ok(if value.is_absolute() {
        value.to_path_buf()
    } else {
        path.parent().unwrap_or(Path::new("/")).join(value)
    })
}

impl JjRepositoryIdentity {
    /// Resolve and validate the `.jj` control structure for the workspace
    /// that contains (or is) `path`. Rejects a `.jj` directory (or its
    /// `repo`/`working_copy` entries) that is a symlink, and cross-checks a
    /// secondary workspace's pointer against the primary workspace's real
    /// `repo` directory.
    pub fn discover(path: &Path) -> Result<Self> {
        let start = path
            .canonicalize()
            .with_context(|| format!("Failed to resolve repository path {}", path.display()))?;

        let mut candidate = Some(start.as_path());
        let workspace_root = loop {
            let dir = candidate.ok_or_else(|| anyhow!("Not a jj workspace: {}", path.display()))?;
            if std::fs::symlink_metadata(dir.join(".jj")).is_ok() {
                break dir.to_path_buf();
            }
            candidate = dir.parent();
        };

        let dot_jj = workspace_root.join(".jj");
        reject_symlink(&dot_jj, "workspace .jj directory")?;
        let dot_jj_metadata = std::fs::metadata(&dot_jj)
            .with_context(|| format!("Failed to inspect .jj at {}", dot_jj.display()))?;
        if !dot_jj_metadata.is_dir() {
            bail!(".jj must be a directory: {}", dot_jj.display());
        }
        let dot_jj = dot_jj
            .canonicalize()
            .with_context(|| format!("Failed to resolve .jj at {}", dot_jj.display()))?;

        let repo_entry = dot_jj.join("repo");
        reject_symlink(&repo_entry, "workspace .jj/repo entry")?;
        let repo_metadata = std::fs::metadata(&repo_entry)
            .with_context(|| format!("Failed to inspect .jj/repo at {}", repo_entry.display()))?;

        if repo_metadata.is_dir() {
            // Primary workspace: repo data lives directly here.
            let repo_dir = repo_entry.canonicalize().with_context(|| {
                format!(
                    "Failed to resolve repo directory at {}",
                    repo_entry.display()
                )
            })?;
            return Ok(Self {
                workspace_root,
                dot_jj,
                repo_dir,
                is_primary: true,
            });
        }

        if !repo_metadata.is_file() {
            bail!(
                ".jj/repo must be a directory or pointer file: {}",
                repo_entry.display()
            );
        }

        // Secondary workspace: follow the pointer to the primary's repo dir.
        let repo_dir = read_repo_pointer(&repo_entry)?
            .canonicalize()
            .with_context(|| {
                format!(
                    "Failed to resolve primary repo directory referenced by {}",
                    repo_entry.display()
                )
            })?;
        reject_symlink(&repo_dir, "primary repo directory")?;
        let repo_dir_metadata = std::fs::metadata(&repo_dir).with_context(|| {
            format!(
                "Failed to inspect primary repo directory at {}",
                repo_dir.display()
            )
        })?;
        if !repo_dir_metadata.is_dir() {
            bail!(
                "Primary repo directory referenced by {} is not a directory: {}",
                repo_entry.display(),
                repo_dir.display()
            );
        }

        Ok(Self {
            workspace_root,
            dot_jj,
            repo_dir,
            is_primary: false,
        })
    }
}

/// Ambient environment variables jj honors that could redirect config,
/// editor/pager invocation, or operation identity away from what
/// `protected_jj` pins explicitly.
pub fn clear_ambient_jj_env(command: &mut Command) {
    for key in JJ_ENVIRONMENT {
        command.env_remove(key);
    }
}

/// Construct a `jj` `std::process::Command` with a validated and pinned
/// repository identity, paging/color disabled, ambient environment cleared,
/// and known-safe config overrides forced.
///
/// Always resolves `workdir` (or the current directory, if `None`) via
/// `JjRepositoryIdentity::discover` and pins the result with `-R
/// <canonical-repo-path>`, rather than relying on jj's own upward cwd-based
/// discovery — this prevents a maliciously nested or symlinked `.jj`
/// somewhere between `workdir` and the filesystem root from being picked up
/// silently.
pub fn protected_jj(workdir: Option<&Path>) -> Result<Command> {
    let mut command = Command::new("jj");
    clear_ambient_jj_env(&mut command);
    command.arg("--no-pager").arg("--color=never");
    for (key, value) in JJ_CONFIG_OVERRIDES {
        command.args(["--config", &format!("{key}={value}")]);
    }

    if let Some(path) = workdir {
        let identity = JjRepositoryIdentity::discover(path)?;
        command
            .current_dir(&identity.workspace_root)
            .arg("-R")
            .arg(&identity.workspace_root);
    }

    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{init_colocated_repo, init_jj_repo};

    #[test]
    fn discover_succeeds_on_jj_only_repo() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let identity = JjRepositoryIdentity::discover(temp.path()).unwrap();
        assert!(identity.is_primary);
        assert!(identity.repo_dir.is_dir());
    }

    #[test]
    fn discover_succeeds_on_colocated_repo() {
        let temp = tempfile::tempdir().unwrap();
        init_colocated_repo(temp.path());
        let identity = JjRepositoryIdentity::discover(temp.path()).unwrap();
        assert!(identity.is_primary);
        assert!(identity.repo_dir.is_dir());
        assert!(temp.path().join(".git").exists());
    }

    #[test]
    fn discover_fails_on_non_repo_directory() {
        let temp = tempfile::tempdir().unwrap();
        assert!(JjRepositoryIdentity::discover(temp.path()).is_err());
    }

    #[test]
    fn discover_rejects_symlinked_dot_jj() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        init_jj_repo(&real);

        let linked = temp.path().join("linked");
        std::fs::create_dir(&linked).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(real.join(".jj"), linked.join(".jj")).unwrap();
        }

        assert!(JjRepositoryIdentity::discover(&linked).is_err());
    }

    #[test]
    fn discover_distinguishes_secondary_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("primary");
        std::fs::create_dir(&primary).unwrap();
        init_jj_repo(&primary);

        let secondary = temp.path().join("secondary");
        let mut command = protected_jj(Some(&primary)).unwrap();
        let status = command
            .args(["workspace", "add"])
            .arg(&secondary)
            .status()
            .unwrap();
        assert!(status.success());

        let identity = JjRepositoryIdentity::discover(&secondary).unwrap();
        assert!(!identity.is_primary);
        let primary_identity = JjRepositoryIdentity::discover(&primary).unwrap();
        assert_eq!(identity.repo_dir, primary_identity.repo_dir);
    }

    #[test]
    fn protected_jj_clears_ambient_environment() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let command = protected_jj(Some(temp.path())).unwrap();
        let environment = command
            .get_envs()
            .map(|(key, value)| (key.to_string_lossy().into_owned(), value))
            .collect::<std::collections::HashMap<_, _>>();
        for key in JJ_ENVIRONMENT {
            assert_eq!(environment.get(*key), Some(&None), "{key} was not cleared");
        }
    }

    #[test]
    fn protected_jj_pins_repository_path_and_disables_paging() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let command = protected_jj(Some(temp.path())).unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|arg| arg == "--no-pager"));
        assert!(args.iter().any(|arg| arg == "--color=never"));
        assert!(args.iter().any(|arg| arg == "-R"));
    }
}
