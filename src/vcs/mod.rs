mod git;
mod jj;

use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::MuxMode;

// Re-export VCS implementations
pub use self::git::GitVcs;
pub use self::jj::JjVcs;

/// VCS-agnostic status information for a workspace
pub type VcsStatus = crate::git::GitStatus;

/// Custom error type for workspace not found
#[derive(Debug, thiserror::Error)]
#[error("Workspace not found: {0}")]
pub struct WorkspaceNotFound(pub String);

/// Trait encapsulating all VCS operations needed by workmux.
///
/// Implementations exist for git (GitVcs) and jj/Jujutsu (JjVcs).
pub trait Vcs: Send + Sync {
    /// Return the name of this VCS backend ("git" or "jj")
    fn name(&self) -> &str;

    // ── Repo detection ───────────────────────────────────────────────

    /// Check if CWD is inside a repository managed by this VCS
    fn is_repo(&self) -> Result<bool>;

    /// Check if the repository has any commits
    fn has_commits(&self) -> Result<bool>;

    /// Get the root directory of the repository
    fn get_repo_root(&self) -> Result<PathBuf>;

    /// Get the root directory of the repository containing the given path
    fn get_repo_root_for(&self, dir: &Path) -> Result<PathBuf>;

    /// Get the main workspace root (primary worktree or bare repo path)
    fn get_main_workspace_root(&self) -> Result<PathBuf>;

    /// Get the shared directory (git-common-dir or jj repo dir)
    fn get_shared_dir(&self) -> Result<PathBuf>;

    /// Check if a path is ignored by the VCS
    fn is_path_ignored(&self, repo_path: &Path, file_path: &str) -> bool;

    // ── Workspace lifecycle ──────────────────────────────────────────

    /// Check if a workspace already exists for a branch
    fn workspace_exists(&self, branch_name: &str) -> Result<bool>;

    /// Create a new workspace
    fn create_workspace(
        &self,
        path: &Path,
        branch: &str,
        create_branch: bool,
        base: Option<&str>,
        track_upstream: bool,
    ) -> Result<()>;

    /// List all workspaces as (path, branch/bookmark) pairs
    fn list_workspaces(&self) -> Result<Vec<(PathBuf, String)>>;

    /// Find a workspace by handle (directory name) or branch name.
    /// Returns (path, branch_name).
    fn find_workspace(&self, name: &str) -> Result<(PathBuf, String)>;

    /// Get the path to a workspace for a given branch
    fn get_workspace_path(&self, branch: &str) -> Result<PathBuf>;

    /// Prune stale workspace metadata
    fn prune_workspaces(&self, shared_dir: &Path) -> Result<()>;

    // ── Workspace metadata ───────────────────────────────────────────

    /// Store per-workspace metadata
    fn set_workspace_meta(&self, handle: &str, key: &str, value: &str) -> Result<()>;

    /// Retrieve per-workspace metadata. Returns None if key doesn't exist.
    fn get_workspace_meta(&self, handle: &str, key: &str) -> Option<String>;

    /// Determine the mux mode for a workspace from metadata
    fn get_workspace_mode(&self, handle: &str) -> MuxMode;

    /// Batch-load all workspace modes in a single call
    fn get_all_workspace_modes(&self) -> HashMap<String, MuxMode>;

    /// Remove all metadata for a workspace handle
    fn remove_workspace_meta(&self, handle: &str) -> Result<()>;

    // ── Branch/bookmark operations ───────────────────────────────────

    /// Get the default branch (main/master)
    fn get_default_branch(&self) -> Result<String>;

    /// Get the default branch for a repository at a specific path
    fn get_default_branch_in(&self, workdir: Option<&Path>) -> Result<String>;

    /// Check if a branch exists
    fn branch_exists(&self, name: &str) -> Result<bool>;

    /// Check if a branch exists in a specific workdir
    fn branch_exists_in(&self, name: &str, workdir: Option<&Path>) -> Result<bool>;

    /// Get the current branch name
    fn get_current_branch(&self) -> Result<String>;

    /// List branches available for checkout (excluding those already checked out)
    fn list_checkout_branches(&self) -> Result<Vec<String>>;

    /// Delete a branch
    fn delete_branch(&self, name: &str, force: bool, shared_dir: &Path) -> Result<()>;

    /// Get the base ref for merge checks, preferring local over remote
    fn get_merge_base(&self, main_branch: &str) -> Result<String>;

    /// Get branches not merged into the base branch
    fn get_unmerged_branches(&self, base: &str) -> Result<HashSet<String>>;

    /// Get branches whose upstream tracking branch has been deleted
    fn get_gone_branches(&self) -> Result<HashSet<String>>;

    // ── Base branch tracking (metadata) ──────────────────────────────

    /// Store the base branch that a branch was created from
    fn set_branch_base(&self, branch: &str, base: &str) -> Result<()>;

    /// Retrieve the base branch that a branch was created from
    fn get_branch_base(&self, branch: &str) -> Result<String>;

    /// Get the base branch for a given branch in a specific workdir
    fn get_branch_base_in(&self, branch: &str, workdir: Option<&Path>) -> Result<String>;

    // ── Status ───────────────────────────────────────────────────────

    /// Get full VCS status for a workspace (for dashboard display)
    fn get_status(&self, worktree: &Path) -> VcsStatus;

    /// Check if the workspace has any uncommitted changes
    fn has_uncommitted_changes(&self, worktree: &Path) -> Result<bool>;

    /// Check if the workspace has tracked changes (staged or modified, excluding untracked)
    fn has_tracked_changes(&self, worktree: &Path) -> Result<bool>;

    /// Check if the workspace has untracked files
    fn has_untracked_files(&self, worktree: &Path) -> Result<bool>;

    /// Check if the workspace has staged changes
    fn has_staged_changes(&self, worktree: &Path) -> Result<bool>;

    /// Check if the workspace has unstaged changes
    fn has_unstaged_changes(&self, worktree: &Path) -> Result<bool>;

    // ── Merge operations ─────────────────────────────────────────────

    /// Commit staged changes using the user's editor
    fn commit_with_editor(&self, worktree: &Path) -> Result<()>;

    /// Merge a branch into the current branch in a workspace
    fn merge_in_workspace(&self, worktree: &Path, branch: &str) -> Result<()>;

    /// Rebase the current branch onto a base branch
    fn rebase_onto_base(&self, worktree: &Path, base: &str) -> Result<()>;

    /// Squash merge a branch (stages changes but does not commit)
    fn merge_squash(&self, worktree: &Path, branch: &str) -> Result<()>;

    /// Switch to a different branch in a workspace
    fn switch_branch(&self, worktree: &Path, branch: &str) -> Result<()>;

    /// Stash uncommitted changes
    fn stash_push(&self, msg: &str, untracked: bool, patch: bool) -> Result<()>;

    /// Pop the latest stash in a workspace
    fn stash_pop(&self, worktree: &Path) -> Result<()>;

    /// Reset the workspace to HEAD, discarding all changes
    fn reset_hard(&self, worktree: &Path) -> Result<()>;

    /// Abort a merge in progress
    fn abort_merge(&self, worktree: &Path) -> Result<()>;

    // ── Remotes ──────────────────────────────────────────────────────

    /// List configured remotes
    fn list_remotes(&self) -> Result<Vec<String>>;

    /// Check if a remote exists
    fn remote_exists(&self, name: &str) -> Result<bool>;

    /// Fetch updates from a remote
    fn fetch_remote(&self, remote: &str) -> Result<()>;

    /// Fetch from remote with prune
    fn fetch_prune(&self) -> Result<()>;

    /// Add a remote
    fn add_remote(&self, name: &str, url: &str) -> Result<()>;

    /// Set the URL for an existing remote
    fn set_remote_url(&self, name: &str, url: &str) -> Result<()>;

    /// Get the URL for a remote
    fn get_remote_url(&self, remote: &str) -> Result<String>;

    /// Ensure a remote exists for a specific fork owner.
    /// Returns the remote name.
    fn ensure_fork_remote(&self, owner: &str) -> Result<String>;

    /// Get the repository owner from the origin remote URL
    fn get_repo_owner(&self) -> Result<String>;

    // ── Deferred cleanup ─────────────────────────────────────────────

    /// Build shell commands for deferred cleanup.
    /// Returns individual commands (caller handles joining/formatting).
    fn build_cleanup_commands(
        &self,
        shared_dir: &Path,
        branch: &str,
        handle: &str,
        keep_branch: bool,
        force: bool,
    ) -> Vec<String>;

    // ── Status cache ─────────────────────────────────────────────────

    /// Load the status cache from disk
    fn load_status_cache(&self) -> HashMap<PathBuf, VcsStatus>;

    /// Save the status cache to disk
    fn save_status_cache(&self, statuses: &HashMap<PathBuf, VcsStatus>);
}

/// Detect the VCS backend for the current directory.
///
/// Walks up from CWD looking for `.jj/` or `.git/` directories.
/// Prefers jj if both are found (colocated repo).
pub fn detect_vcs() -> Result<Arc<dyn Vcs>> {
    let cwd = std::env::current_dir()?;
    for dir in cwd.ancestors() {
        if dir.join(".jj").is_dir() {
            return Ok(Arc::new(JjVcs::new()));
        }
        if dir.join(".git").exists() {
            return Ok(Arc::new(GitVcs::new()));
        }
    }
    Err(anyhow!("Not in a git or jj repository"))
}

/// Try to detect VCS, returning None if not in a repository.
/// Useful for contexts where being outside a repo is not an error (e.g., shell completions).
pub fn try_detect_vcs() -> Option<Arc<dyn Vcs>> {
    detect_vcs().ok()
}
