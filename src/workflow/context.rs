use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::multiplexer::Multiplexer;
use crate::{config, git};
use tracing::debug;

const AUTO_BASE_BRANCH: &str = "auto";

/// Shared context for workflow operations
///
/// This struct centralizes pre-flight checks and holds essential data
/// needed by workflow modules, reducing code duplication.
pub struct WorkflowContext {
    pub execution_dir: PathBuf,
    pub main_worktree_root: PathBuf,
    pub git_common_dir: PathBuf,
    pub main_branch: String,
    pub prefix: String,
    pub config: config::Config,
    pub mux: Arc<dyn Multiplexer>,
    /// Relative path from repo root to config directory.
    /// Empty if config is at repo root or using defaults.
    pub config_rel_dir: PathBuf,
    /// Absolute path to the directory where config was found.
    /// Used as source for file operations (copy/symlink).
    pub config_source_dir: PathBuf,
}

fn resolve_main_branch(config: &config::Config, repo_path: &Path) -> Result<String> {
    if let Some(ref branch) = config.main_branch {
        Ok(branch.clone())
    } else {
        git::get_default_branch_in(Some(repo_path)).context("Failed to determine the main branch")
    }
}

pub fn resolve_configured_base_branch(
    config: &config::Config,
    repo_path: &Path,
) -> Result<Option<String>> {
    let Some(base) = config
        .base_branch
        .as_deref()
        .filter(|base| !base.trim().is_empty())
    else {
        return Ok(None);
    };

    if base == AUTO_BASE_BRANCH {
        resolve_main_branch(config, repo_path).map(Some)
    } else {
        Ok(Some(base.to_string()))
    }
}

fn paths_identify_same_worktree(path: &Path, main_worktree_root: &Path) -> bool {
    match (path.canonicalize(), main_worktree_root.canonicalize()) {
        (Ok(canonical_path), Ok(canonical_main)) => canonical_path == canonical_main,
        _ => path == main_worktree_root,
    }
}

impl WorkflowContext {
    /// Directory new worktrees of this repository go in: `worktree_dir`, or
    /// `<project>__worktrees` beside the main worktree.
    pub fn worktree_base_dir(&self) -> Result<PathBuf> {
        if let Some(ref worktree_dir) = self.config.worktree_dir {
            return crate::util::expand_worktree_dir(worktree_dir, &self.main_worktree_root);
        }
        let project_name = self
            .main_worktree_root
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("Could not determine project name"))?;
        Ok(self
            .main_worktree_root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Could not determine parent directory"))?
            .join(format!("{}__worktrees", project_name)))
    }

    /// Create a new workflow context
    ///
    /// Performs the git repository check and gathers all commonly needed data.
    /// Does NOT check if multiplexer is running or change the current directory - those
    /// are optional operations that can be performed via helper methods.
    pub fn new(
        config: config::Config,
        mux: Arc<dyn Multiplexer>,
        config_location: Option<config::ConfigLocation>,
    ) -> Result<Self> {
        let execution_dir = std::env::current_dir().context("Failed to get current directory")?;
        Self::new_in(&execution_dir, config, mux, config_location)
    }

    /// Create a new workflow context for an explicit repository path
    pub fn new_in(
        repo_path: &Path,
        config: config::Config,
        mux: Arc<dyn Multiplexer>,
        config_location: Option<config::ConfigLocation>,
    ) -> Result<Self> {
        let execution_dir = repo_path.canonicalize().with_context(|| {
            format!(
                "Could not resolve repository path '{}'",
                repo_path.display()
            )
        })?;

        if !git::is_git_repo_in(Some(&execution_dir))? {
            return Err(anyhow!("Not in a git repository"));
        }

        let main_worktree_root = git::get_main_worktree_root_in(Some(&execution_dir))
            .context("Could not find the main git worktree")?;

        let git_common_dir = git::get_git_common_dir_in(Some(&execution_dir))
            .context("Could not find the git common directory")?;

        let main_branch = resolve_main_branch(&config, &execution_dir)?;

        let prefix = config.window_prefix().to_string();

        let (config_rel_dir, config_source_dir) = match config_location {
            Some(loc) => (loc.rel_dir, loc.config_dir),
            None => (PathBuf::new(), main_worktree_root.clone()),
        };

        debug!(
            execution_dir = %execution_dir.display(),
            main_worktree_root = %main_worktree_root.display(),
            git_common_dir = %git_common_dir.display(),
            main_branch = %main_branch,
            prefix = %prefix,
            backend = mux.name(),
            config_rel_dir = %config_rel_dir.display(),
            config_source_dir = %config_source_dir.display(),
            "workflow_context:created"
        );

        Ok(Self {
            execution_dir,
            main_worktree_root,
            git_common_dir,
            main_branch,
            prefix,
            config,
            mux,
            config_rel_dir,
            config_source_dir,
        })
    }

    /// Return whether a path identifies the main worktree.
    pub fn is_main_worktree(&self, path: &Path) -> bool {
        paths_identify_same_worktree(path, &self.main_worktree_root)
    }

    /// Ensure the terminal multiplexer is running, returning an error if not
    ///
    /// Call this at the start of workflows that require a multiplexer.
    pub fn ensure_mux_running(&self) -> Result<()> {
        if !self.mux.is_running()? {
            return Err(anyhow!(
                "{} is not running. Please start a {} session first.",
                self.mux.name(),
                self.mux.name()
            ));
        }
        Ok(())
    }

    /// Ensure tmux is running (backward-compat alias for ensure_mux_running)
    #[deprecated(note = "Use ensure_mux_running() instead")]
    #[allow(dead_code)]
    pub fn ensure_tmux_running(&self) -> Result<()> {
        self.ensure_mux_running()
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

#[cfg(test)]
mod tests {
    use super::paths_identify_same_worktree;
    use tempfile::tempdir;

    #[test]
    fn main_worktree_path_comparison_handles_existing_and_missing_paths() {
        let temp = tempdir().unwrap();
        let main = temp.path().join("main");
        let sibling = temp.path().join("sibling");
        std::fs::create_dir(&main).unwrap();
        std::fs::create_dir(&sibling).unwrap();

        assert!(paths_identify_same_worktree(&main, &main));
        assert!(!paths_identify_same_worktree(&sibling, &main));

        let missing = temp.path().join("missing");
        assert!(paths_identify_same_worktree(&missing, &missing));
        assert!(!paths_identify_same_worktree(
            &temp.path().join("other-missing"),
            &missing
        ));
    }

    #[cfg(unix)]
    #[test]
    fn main_worktree_path_comparison_resolves_symlinks() {
        let temp = tempdir().unwrap();
        let main = temp.path().join("main");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&main).unwrap();
        std::os::unix::fs::symlink(&main, &alias).unwrap();

        assert!(paths_identify_same_worktree(&alias, &main));
    }
}
