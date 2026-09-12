//! Shared types for the VCS abstraction layer.

use std::path::PathBuf;

/// Which version-control system(s) are present at (or above) a given directory.
///
/// `.jj` always wins over `.git` when both are present (colocated jj repo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoKind {
    /// A plain git repository (`.git` present, no `.jj`).
    Git,
    /// A jj repository colocated with a git repository (`.jj` and `.git` both present).
    JjColocated,
    /// A jj-only repository (`.jj` present, no `.git`).
    JjOnly,
    /// Neither `.git` nor `.jj` found walking up to the filesystem root.
    None,
}

/// One entry returned by [`crate::vcs::VcsBackend::list_workspaces_in`].
///
/// For git this corresponds to a worktree; for jj, a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// Absolute path to the workspace/worktree.
    pub path: PathBuf,
    /// Directory name (handle) of the workspace, if determinable.
    pub name: Option<String>,
    /// The branch (git) or bookmark (jj) checked out in this workspace.
    /// `None` for a detached HEAD / anonymous state.
    pub branch_or_bookmark: Option<String>,
}

/// Options for creating a new workspace/worktree via [`crate::vcs::VcsBackend::create_workspace_in`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateWorkspaceOptions {
    /// Path at which to create the new workspace/worktree.
    pub path: PathBuf,
    /// The branch (git) or bookmark (jj) name to check out or create.
    pub name_or_branch: String,
    /// Whether a new branch/bookmark should be created (vs. checking out an existing one).
    pub create_branch: bool,
    /// The base branch/commit to create the new branch/bookmark from, if any.
    pub base: Option<String>,
    /// Whether the new branch should track its upstream remote-tracking branch.
    pub track_upstream: bool,
}
