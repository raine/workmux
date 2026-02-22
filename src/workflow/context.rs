use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use crate::multiplexer::Multiplexer;
use crate::vcs::{self, Vcs};
use crate::config;
use tracing::debug;

/// Shared context for workflow operations
///
/// This struct centralizes pre-flight checks and holds essential data
/// needed by workflow modules, reducing code duplication.
pub struct WorkflowContext {
    pub main_worktree_root: PathBuf,
    pub shared_dir: PathBuf,
    pub main_branch: String,
    pub prefix: String,
    pub config: config::Config,
    pub mux: Arc<dyn Multiplexer>,
    pub vcs: Arc<dyn Vcs>,
    /// Relative path from repo root to config directory.
    /// Empty if config is at repo root or using defaults.
    pub config_rel_dir: PathBuf,
    /// Absolute path to the directory where config was found.
    /// Used as source for file operations (copy/symlink).
    pub config_source_dir: PathBuf,
}

impl WorkflowContext {
    /// Create a new workflow context
    ///
    /// Performs the VCS repository check and gathers all commonly needed data.
    /// Does NOT check if multiplexer is running or change the current directory - those
    /// are optional operations that can be performed via helper methods.
    pub fn new(
        config: config::Config,
        mux: Arc<dyn Multiplexer>,
        config_location: Option<config::ConfigLocation>,
    ) -> Result<Self> {
        let vcs = vcs::detect_vcs()?;

        let main_worktree_root =
            vcs.get_main_workspace_root().context("Could not find the main worktree")?;

        let shared_dir =
            vcs.get_shared_dir().context("Could not find the shared VCS directory")?;

        let main_branch = if let Some(ref branch) = config.main_branch {
            branch.clone()
        } else {
            vcs.get_default_branch().context("Failed to determine the main branch")?
        };

        let prefix = config.window_prefix().to_string();

        let (config_rel_dir, config_source_dir) = match config_location {
            Some(loc) => (loc.rel_dir, loc.config_dir),
            None => (PathBuf::new(), main_worktree_root.clone()),
        };

        debug!(
            main_worktree_root = %main_worktree_root.display(),
            shared_dir = %shared_dir.display(),
            main_branch = %main_branch,
            prefix = %prefix,
            vcs_backend = vcs.name(),
            mux_backend = mux.name(),
            config_rel_dir = %config_rel_dir.display(),
            config_source_dir = %config_source_dir.display(),
            "workflow_context:created"
        );

        Ok(Self {
            main_worktree_root,
            shared_dir,
            main_branch,
            prefix,
            config,
            mux,
            vcs,
            config_rel_dir,
            config_source_dir,
        })
    }

    /// Ensure the terminal multiplexer is running, returning an error if not
    ///
    /// Call this at the start of workflows that require a multiplexer.
    pub fn ensure_mux_running(&self) -> Result<()> {
        if !self.mux.is_running()? {
            return Err(anyhow::anyhow!(
                "{} is not running. Please start a {} session first.",
                self.mux.name(),
                self.mux.name()
            ));
        }
        Ok(())
    }

    /// Change working directory to main worktree root
    ///
    /// This is necessary for destructive operations (merge, remove) to prevent
    /// "Unable to read current working directory" errors when the command is run
    /// from within a worktree that is about to be deleted.
    pub fn chdir_to_main_worktree(&self) -> Result<()> {
        debug!(
            safe_cwd = %self.main_worktree_root.display(),
            "workflow_context:changing to main worktree"
        );
        std::env::set_current_dir(&self.main_worktree_root).with_context(|| {
            format!(
                "Could not change directory to '{}'",
                self.main_worktree_root.display()
            )
        })
    }
}
