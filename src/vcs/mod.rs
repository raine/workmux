//! VCS abstraction layer.
//!
//! This module provides a trait-based abstraction (mirroring
//! `crate::multiplexer::Multiplexer`) that allows workmux to work with
//! different version-control backends (git, jj) interchangeably.
//!
//! As of this task only `GitBackend` exists; `JjBackend` lands in a later
//! task. Nothing in the rest of the codebase is wired to use this module yet.

pub mod detect;
pub mod git_backend;
pub mod types;

use anyhow::Result;
use std::path::{Path, PathBuf};

#[allow(unused_imports)]
pub use git_backend::{GitBackend, GitConfigMetaStore};
pub use types::{CreateWorkspaceOptions, RepoKind, WorkspaceEntry};

/// Re-export of the existing git status struct, unchanged, under a
/// VCS-neutral alias. The underlying struct (and its fields) lives in
/// `crate::git::types::GitStatus` and is not modified by this module.
pub use crate::git::GitStatus as VcsStatus;

/// Main trait for version-control backends (git, jj).
///
/// Implementations must be Send + Sync to allow sharing via `Arc<dyn VcsBackend>`.
pub trait VcsBackend: Send + Sync {
    /// Returns the name of this backend (e.g., "git", "jj").
    fn name(&self) -> &'static str;

    /// Check if `workdir` (or the current directory) is inside a repository
    /// managed by this backend.
    fn is_repo_in(&self, workdir: Option<&Path>) -> Result<bool>;

    /// Get the main worktree/workspace root directory (not a linked one).
    fn get_main_worktree_root_in(&self, workdir: Option<&Path>) -> Result<PathBuf>;

    /// Get the common repository directory (shared across all worktrees/workspaces).
    fn get_common_dir_in(&self, workdir: Option<&Path>) -> Result<PathBuf>;

    /// Check if the repository has any commits.
    fn has_commits_in(&self, workdir: Option<&Path>) -> Result<bool>;

    /// Create a new workspace/worktree.
    fn create_workspace_in(
        &self,
        opts: &CreateWorkspaceOptions,
        workdir: Option<&Path>,
    ) -> Result<()>;

    /// List all workspaces/worktrees.
    fn list_workspaces_in(&self, workdir: Option<&Path>) -> Result<Vec<WorkspaceEntry>>;

    /// Move a registered workspace/worktree to a new path.
    fn move_workspace(&self, old_path: &Path, new_path: &Path) -> Result<()>;

    /// Remove a workspace/worktree, identified by handle (directory name) or branch/bookmark name.
    fn remove_workspace_at(&self, handle_or_name: &str, common_dir: &Path) -> Result<()>;

    /// Prune stale workspace/worktree metadata.
    fn prune_workspaces_in(&self, common_dir: &Path) -> Result<()>;

    /// Get the default branch/bookmark (e.g., "main" or "master").
    fn get_default_branch_in(&self, workdir: Option<&Path>) -> Result<String>;

    /// Check if a branch/bookmark exists.
    fn branch_exists_in(&self, name: &str, workdir: Option<&Path>) -> Result<bool>;

    /// Get the current branch/bookmark checked out in `workdir`.
    /// Returns `None` for a detached/anonymous state.
    fn get_current_branch_in(&self, workdir: &Path) -> Result<Option<String>>;

    /// Delete a branch/bookmark.
    fn delete_branch_in(&self, name: &str, force: bool, common_dir: &Path) -> Result<()>;

    /// Get the merge base branch/commit for comparisons.
    fn get_merge_base_in(&self, workdir: Option<&Path>, base: &str) -> Result<String>;

    /// Get status information (ahead/behind, dirty state, diff stats) for a workspace/worktree.
    fn get_status(&self, workspace_path: &Path, main_branch: Option<&str>) -> Result<VcsStatus>;

    /// Access the per-workspace metadata store for this backend.
    fn meta(&self) -> &dyn WorkmuxMetaStore;

    /// Get branches whose upstream remote-tracking branch has been deleted.
    ///
    /// No jj analog in v1 — default to empty, overridden by `GitBackend`.
    fn get_gone_branches_in(&self, _common_dir: &Path) -> Result<Vec<String>> {
        Ok(vec![])
    }

    /// Fetch from the remote with prune, updating remote-tracking refs.
    ///
    /// No jj analog in v1 — default to no-op, overridden by `GitBackend`.
    fn fetch_prune_in(&self, _workdir: Option<&Path>) -> Result<()> {
        Ok(())
    }
}

/// Per-workspace metadata storage, abstracted over the backend's native
/// mechanism (git config for `GitBackend`).
pub trait WorkmuxMetaStore: Send + Sync {
    /// Retrieve a metadata value for `handle`, or `None` if unset.
    fn get(&self, handle: &str, key: &str, workdir: Option<&Path>) -> Option<String>;

    /// Store a metadata value for `handle`.
    fn set(&self, handle: &str, key: &str, value: &str, workdir: Option<&Path>) -> Result<()>;

    /// Remove all metadata for `handle`, using an explicitly identified repository.
    fn remove_all_at(&self, handle: &str, common_dir: &Path) -> Result<()>;

    /// Migrate all metadata from `old_handle` to `new_handle`.
    fn migrate(&self, old_handle: &str, new_handle: &str, workdir: Option<&Path>) -> Result<()>;

    /// Retrieve the stored base branch/commit for `branch`, or `None` if unset.
    fn get_branch_base(&self, branch: &str, workdir: Option<&Path>) -> Option<String>;

    /// Store the base branch/commit that `branch` was created from.
    fn set_branch_base(&self, branch: &str, base: &str, workdir: Option<&Path>) -> Result<()>;
}
