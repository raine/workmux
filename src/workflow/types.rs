use std::path::PathBuf;

use crate::config::MuxMode;
use crate::git::RepositoryIdentity;
use crate::github::PrSummary;
use crate::multiplexer::conversation::{ConversationForker, SessionInfo};
use crate::multiplexer::types::ResumeMode;
use crate::multiplexer::{AgentStatus, WindowTarget};
use crate::prompt::Prompt;

/// Arguments for creating a worktree
pub struct CreateArgs<'a> {
    pub branch_name: &'a str,
    pub handle: &'a str,
    pub base_branch: Option<&'a str>,
    pub remote_branch: Option<&'a str>,
    /// Forge-specific review ref fetched from origin for checkout.
    pub checkout_ref: Option<super::pr::CheckoutRef>,
    pub prompt: Option<&'a Prompt>,
    pub options: SetupOptions,
    pub mode_override: Option<MuxMode>,
    pub agent: Option<&'a str>,
    /// True if the handle was explicitly set via --name (skip auto-suffix on collision)
    pub is_explicit_name: bool,
    /// Write prompt file without injecting into agent commands.
    /// When true, the prompt file is written to .workmux/ but not passed to
    /// setup_environment, so validation and agent injection are skipped.
    pub prompt_file_only: bool,
    /// Fork a conversation from another worktree into this one
    pub fork_source: Option<ForkSource>,
}

/// Result of provisioning a worktree without a multiplexer target.
pub struct ProvisionResult {
    pub worktree_path: PathBuf,
    pub working_directory: PathBuf,
    pub branch_name: String,
    pub post_create_hooks_run: usize,
    pub base_branch: Option<String>,
    pub resolved_handle: String,
}

/// Result of creating a worktree
pub struct CreateResult {
    pub worktree_path: PathBuf,
    pub branch_name: String,
    pub post_create_hooks_run: usize,
    pub base_branch: Option<String>,
    /// True if we switched to an existing window instead of creating a new one
    pub did_switch: bool,
    /// The actual handle used (may differ from requested if auto-suffixed for cross-repo collision)
    pub resolved_handle: String,
    /// The full mux target name that was created or selected.
    pub mux_target_full_name: String,
    /// The mux mode that was actually used (window or session)
    pub mode: MuxMode,
}

/// Result of merging a worktree
pub struct MergeResult {
    pub branch_merged: String,
    pub main_branch: String,
    pub had_staged_changes: bool,
    pub cleanup_scheduled: bool,
    pub cleanup_error: Option<anyhow::Error>,
}

/// Result of removing a worktree
pub struct RemoveResult {
    pub branch_removed: Option<String>,
    pub cleanup_scheduled: bool,
}

/// Result of renaming a worktree
pub struct RenameResult {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub old_handle: String,
    pub new_handle: String,
    pub old_branch: String,
    /// Set when `--branch` was used and the branch was actually renamed.
    pub new_branch: Option<String>,
    /// Number of tmux windows/sessions renamed.
    pub tmux_renamed: usize,
    /// Number of agent state files updated.
    pub agents_migrated: usize,
}

#[derive(Clone, Debug)]
pub struct WorktreeCleanupIdentity {
    pub device: u64,
    pub inode: u64,
    pub repository: RepositoryIdentity,
}

/// Deferred cleanup operations to run after target close.
/// Destructive cleanup waits so processes in the target retain a valid CWD.
pub struct DeferredCleanup {
    pub worktree_path: PathBuf,
    pub branch_name: String,
    pub handle: String,
    pub keep_branch: bool,
    pub force: bool,
    pub expected_identity: WorktreeCleanupIdentity,
}

/// A source target that must close before destructive cleanup can run.
/// Captured multiplexer IDs bind closure and waiting to stable target identity.
#[derive(Clone, Debug)]
pub enum SourceTarget {
    Window(WindowTarget),
    Session { name: String, id: Option<String> },
}

/// Result of cleanup operations
pub struct CleanupResult {
    pub tmux_window_killed: bool,
    /// True when the source target is focused by the active client.
    pub source_target_is_active: bool,
    pub source_target_to_close: Option<SourceTarget>,
    /// Full cleanup deferred until after target close (rename + prune + branch delete).
    /// Scheduled results retain this payload until the detached worker starts.
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
    /// Source directory for configured file operations. If None, uses the project root.
    pub config_root: Option<PathBuf>,
    /// If true, open existing worktree instead of failing when it already exists.
    pub open_if_exists: bool,
    /// Mode for tmux operations: window (default) or session
    pub mode: MuxMode,
    /// Custom window target name for window mode.
    pub target_window_name: Option<String>,
    /// Custom session target name for session mode.
    pub target_session_name: Option<String>,
    /// Raw tmux parent session for window mode.
    pub window_session_name: Option<String>,
    /// Stable ownership token attached to windows created for this worktree.
    pub window_token: Option<String>,
    /// Whether the created window is the primary worktree window.
    pub primary_window: bool,
    /// How to resume a conversation (continue last, fork specific session, or none).
    pub resume_mode: ResumeMode,
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
            target_window_name: None,
            target_session_name: None,
            window_session_name: None,
            window_token: None,
            primary_window: true,
            resume_mode: ResumeMode::default(),
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
            target_window_name: None,
            target_session_name: None,
            window_session_name: None,
            window_token: None,
            primary_window: true,
            resume_mode: ResumeMode::default(),
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
            target_window_name: None,
            target_session_name: None,
            window_session_name: None,
            window_token: None,
            primary_window: true,
            resume_mode: ResumeMode::default(),
        }
    }

    pub fn primary_mux_target_name<'a>(&'a self, handle: &'a str) -> &'a str {
        match self.mode {
            MuxMode::Window => self.target_window_name.as_deref().unwrap_or(handle),
            MuxMode::Session => self.target_session_name.as_deref().unwrap_or(handle),
        }
    }

    pub fn has_explicit_primary_mux_target(&self) -> bool {
        match self.mode {
            MuxMode::Window => self.target_window_name.is_some(),
            MuxMode::Session => self.target_session_name.is_some(),
        }
    }
}

/// Source data for forking a conversation into a new worktree
pub struct ForkSource {
    pub forker: Box<dyn ConversationForker>,
    pub session: SessionInfo,
}

/// Summary of agent statuses for a worktree (may have multiple agents)
#[derive(Clone)]
pub struct AgentStatusSummary {
    pub statuses: Vec<AgentStatus>,
}

/// List all worktrees with their status
#[derive(Clone)]
pub struct WorktreeInfo {
    /// Canonical common Git directory shared by every checkout of this repository.
    pub repo_key: PathBuf,
    pub handle: String,
    pub branch: String,
    pub path: PathBuf,
    pub is_main: bool,
    pub mode: MuxMode,
    pub has_mux_window: bool,
    pub has_unmerged: bool,
    pub pr_info: Option<PrSummary>,
    pub agent_status: Option<AgentStatusSummary>,
    /// Worktree directory creation time (Unix timestamp in seconds)
    pub created_at: Option<u64>,
    /// The base branch this worktree was created from (from git config)
    pub base_branch: Option<String>,
}
