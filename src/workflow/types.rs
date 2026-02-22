use std::path::PathBuf;

use crate::config::MuxMode;
use crate::github::PrSummary;
use crate::multiplexer::AgentStatus;
use crate::prompt::Prompt;

/// Arguments for creating a worktree
pub struct CreateArgs<'a> {
    pub branch_name: &'a str,
    pub handle: &'a str,
    pub base_branch: Option<&'a str>,
    pub remote_branch: Option<&'a str>,
    pub prompt: Option<&'a Prompt>,
    pub options: SetupOptions,
    pub agent: Option<&'a str>,
}

/// Result of creating a worktree
pub struct CreateResult {
    pub worktree_path: PathBuf,
    pub branch_name: String,
    pub post_create_hooks_run: usize,
    pub base_branch: Option<String>,
    /// True if we switched to an existing window instead of creating a new one
    pub did_switch: bool,
}

/// Result of merging a worktree
pub struct MergeResult {
    pub branch_merged: String,
    pub main_branch: String,
    pub had_staged_changes: bool,
}

/// Result of removing a worktree
pub struct RemoveResult {
    pub branch_removed: String,
}

/// Deferred cleanup operations to run after window close.
/// Used when running inside the target window to avoid invalidating the agent's CWD.
pub struct DeferredCleanup {
    pub worktree_path: PathBuf,
    pub trash_path: PathBuf,
    pub branch_name: String,
    pub handle: String,
    pub keep_branch: bool,
    pub force: bool,
    /// Pre-computed VCS-specific cleanup commands (prune, branch delete, config remove).
    pub vcs_cleanup_commands: Vec<String>,
}

/// Result of cleanup operations
pub struct CleanupResult {
    pub tmux_window_killed: bool,
    pub worktree_removed: bool,
    pub local_branch_deleted: bool,
    /// The actual window name to close later (when running inside a duplicate window)
    pub window_to_close_later: Option<String>,
    /// Trash directory path to delete after window close (deferred to avoid race condition)
    pub trash_path_to_delete: Option<PathBuf>,
    /// Full cleanup deferred until after window close (rename + prune + branch delete).
    /// Used when running inside the target window to keep CWD valid for agent hooks.
    pub deferred_cleanup: Option<DeferredCleanup>,
}

/// Options for setting up a worktree environment
#[derive(Debug, Clone)]
pub struct SetupOptions {
    pub run_hooks: bool,
    pub run_file_ops: bool,
    pub run_pane_commands: bool,
    pub prompt_file_path: Option<PathBuf>,
    /// If true, switch to the new tmux window when done; if false, leave it in the background.
    pub focus_window: bool,
    /// Working directory for the tmux window. If None, uses worktree_path.
    pub working_dir: Option<PathBuf>,
    /// Config root directory in source repo (for file ops). If None, uses main worktree root.
    pub config_root: Option<PathBuf>,
    /// If true, open existing worktree instead of failing when it already exists.
    pub open_if_exists: bool,
    /// Mode for tmux operations: window (default) or session
    pub mode: MuxMode,
}

impl SetupOptions {
    /// Create SetupOptions with all options enabled
    #[allow(dead_code)]
    pub fn all() -> Self {
        Self {
            run_hooks: true,
            run_file_ops: true,
            run_pane_commands: true,
            prompt_file_path: None,
            focus_window: true,
            working_dir: None,
            config_root: None,
            open_if_exists: false,
            mode: MuxMode::default(),
        }
    }

    /// Create SetupOptions with custom values
    pub fn new(run_hooks: bool, run_file_ops: bool, run_pane_commands: bool) -> Self {
        Self {
            run_hooks,
            run_file_ops,
            run_pane_commands,
            prompt_file_path: None,
            focus_window: true,
            working_dir: None,
            config_root: None,
            open_if_exists: false,
            mode: MuxMode::default(),
        }
    }

    /// Create SetupOptions with a prompt file
    #[allow(dead_code)]
    pub fn with_prompt(
        run_hooks: bool,
        run_file_ops: bool,
        run_pane_commands: bool,
        prompt_file_path: Option<PathBuf>,
    ) -> Self {
        Self {
            run_hooks,
            run_file_ops,
            run_pane_commands,
            prompt_file_path,
            focus_window: true,
            working_dir: None,
            config_root: None,
            open_if_exists: false,
            mode: MuxMode::default(),
        }
    }
}

/// Summary of agent statuses for a worktree (may have multiple agents)
pub struct AgentStatusSummary {
    pub statuses: Vec<AgentStatus>,
}

/// List all worktrees with their status
pub struct WorktreeInfo {
    pub branch: String,
    pub path: PathBuf,
    pub has_mux_window: bool,
    pub has_unmerged: bool,
    pub pr_info: Option<PrSummary>,
    pub agent_status: Option<AgentStatusSummary>,
}
