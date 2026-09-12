use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config;
use crate::multiplexer::Multiplexer;
use crate::vcs::VcsBackend;
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
    pub vcs: Arc<dyn VcsBackend>,
}

fn resolve_main_branch(
    config: &config::Config,
    repo_path: &Path,
    vcs: &dyn VcsBackend,
) -> Result<String> {
    if let Some(ref branch) = config.main_branch {
        Ok(branch.clone())
    } else {
        vcs.get_default_branch_in(Some(repo_path))
            .context("Failed to determine the main branch")
    }
}

pub fn resolve_configured_base_branch(
    config: &config::Config,
    repo_path: &Path,
    vcs: &dyn VcsBackend,
) -> Result<Option<String>> {
    let Some(base) = config
        .base_branch
        .as_deref()
        .filter(|base| !base.trim().is_empty())
    else {
        return Ok(None);
    };

    if base == AUTO_BASE_BRANCH {
        resolve_main_branch(config, repo_path, vcs).map(Some)
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

        let vcs = crate::vcs::detect::detect_backend_in(&execution_dir)?;

        let main_worktree_root = vcs
            .get_main_worktree_root_in(Some(&execution_dir))
            .context("Could not find the main worktree")?;

        let git_common_dir = vcs
            .get_common_dir_in(Some(&execution_dir))
            .context("Could not find the repository's common directory")?;

        let main_branch = resolve_main_branch(&config, &execution_dir, vcs.as_ref())?;

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
            vcs,
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
    use super::WorkflowContext;
    use crate::config;
    use crate::git;
    use crate::multiplexer::types::{
        CreateSessionParams, CreateWindowInSessionParams, CreateWindowParams, LivePaneInfo,
        PaneSetupOptions, PaneSetupResult,
    };
    use crate::multiplexer::{Multiplexer, PaneHandshake};
    use anyhow::Result;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::tempdir;

    struct TestMux;

    impl Multiplexer for TestMux {
        fn name(&self) -> &'static str {
            "tmux"
        }

        fn is_running(&self) -> Result<bool> {
            Ok(true)
        }

        fn current_pane_id(&self) -> Option<String> {
            None
        }

        fn active_pane_id(&self) -> Option<String> {
            None
        }

        fn get_client_active_pane_path(&self) -> Result<PathBuf> {
            Ok(PathBuf::new())
        }

        fn create_window(&self, _params: CreateWindowParams) -> Result<String> {
            Ok("pane-1".to_string())
        }

        fn create_session(&self, _params: CreateSessionParams) -> Result<String> {
            Ok("pane-1".to_string())
        }

        fn create_window_in_session(&self, _params: CreateWindowInSessionParams) -> Result<String> {
            Ok("pane-1".to_string())
        }

        fn switch_to_session(&self, _prefix: &str, _name: &str) -> Result<()> {
            Ok(())
        }

        fn session_exists(&self, _full_name: &str) -> Result<bool> {
            Ok(false)
        }

        fn kill_session(&self, _full_name: &str) -> Result<()> {
            Ok(())
        }

        fn kill_window(&self, _full_name: &str) -> Result<()> {
            Ok(())
        }

        fn schedule_window_close(&self, _full_name: &str, _delay: Duration) -> Result<()> {
            Ok(())
        }

        fn schedule_session_close(&self, _full_name: &str, _delay: Duration) -> Result<()> {
            Ok(())
        }

        fn run_deferred_script(&self, _script: &str) -> Result<()> {
            Ok(())
        }

        fn shell_select_window_cmd(&self, _full_name: &str) -> Result<String> {
            Ok(String::new())
        }

        fn shell_kill_window_cmd(&self, _full_name: &str) -> Result<String> {
            Ok(String::new())
        }

        fn shell_switch_session_cmd(&self, _full_name: &str) -> Result<String> {
            Ok(String::new())
        }

        fn shell_kill_session_cmd(&self, _full_name: &str) -> Result<String> {
            Ok(String::new())
        }

        fn select_window(&self, _prefix: &str, _name: &str) -> Result<()> {
            Ok(())
        }

        fn window_exists(&self, _prefix: &str, _name: &str) -> Result<bool> {
            Ok(false)
        }

        fn window_exists_by_full_name(&self, _full_name: &str) -> Result<bool> {
            Ok(false)
        }

        fn current_window_name(&self) -> Result<Option<String>> {
            Ok(None)
        }

        fn get_all_window_names(&self) -> Result<HashSet<String>> {
            Ok(HashSet::new())
        }

        fn get_all_session_names(&self) -> Result<HashSet<String>> {
            Ok(HashSet::new())
        }

        fn filter_active_windows(&self, _windows: &[String]) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        fn wait_until_windows_closed(&self, _full_window_names: &[String]) -> Result<()> {
            Ok(())
        }

        fn wait_until_session_closed(&self, _full_session_name: &str) -> Result<()> {
            Ok(())
        }

        fn select_pane(&self, _pane_id: &str) -> Result<()> {
            Ok(())
        }

        fn switch_to_pane(&self, _pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
            Ok(())
        }

        fn kill_pane(&self, _pane_id: &str) -> Result<()> {
            Ok(())
        }

        fn respawn_pane(&self, pane_id: &str, _cwd: &Path, _cmd: Option<&str>) -> Result<String> {
            Ok(pane_id.to_string())
        }

        fn capture_pane(&self, _pane_id: &str, _lines: u16) -> Option<String> {
            None
        }

        fn send_text_fragment(&self, _pane_id: &str, _text: &str) -> Result<()> {
            Ok(())
        }

        fn send_enter(&self, _pane_id: &str) -> Result<()> {
            Ok(())
        }

        fn send_key(&self, _pane_id: &str, _key: &str) -> Result<()> {
            Ok(())
        }

        fn paste_text(&self, _pane_id: &str, _content: &str) -> Result<()> {
            Ok(())
        }

        fn get_default_shell(&self) -> Result<String> {
            Ok("/bin/sh".to_string())
        }

        fn create_handshake(&self) -> Result<Box<dyn PaneHandshake>> {
            Err(anyhow::anyhow!("not used"))
        }

        fn set_status(
            &self,
            _pane_id: &str,
            _icon: &str,
            _auto_clear_on_focus: bool,
        ) -> Result<()> {
            Ok(())
        }

        fn clear_status(&self, _pane_id: &str) -> Result<()> {
            Ok(())
        }

        fn ensure_status_format(&self, _pane_id: &str) -> Result<()> {
            Ok(())
        }

        fn split_pane(
            &self,
            _target_pane_id: &str,
            _direction: &crate::config::SplitDirection,
            _cwd: &Path,
            _size: Option<u16>,
            _percentage: Option<u8>,
            _command: Option<&str>,
        ) -> Result<String> {
            Ok("pane-2".to_string())
        }

        fn setup_panes(
            &self,
            initial_pane_id: &str,
            _panes: &[crate::config::PaneConfig],
            _working_dir: &Path,
            _options: PaneSetupOptions<'_>,
            _config: &config::Config,
            _task_agent: Option<&str>,
        ) -> Result<PaneSetupResult> {
            Ok(PaneSetupResult {
                focus_pane_id: initial_pane_id.to_string(),
                zoom_pane_id: None,
            })
        }

        fn instance_id(&self) -> String {
            "test".to_string()
        }

        fn get_live_pane_info(&self, _pane_id: &str) -> Result<Option<LivePaneInfo>> {
            Ok(None)
        }

        fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
            Ok(HashMap::new())
        }
    }

    /// Regression test: for an existing git repo, wiring `WorkflowContext`
    /// through `crate::vcs::detect::detect_backend_in` /
    /// `GitBackend` must be behaviorally invisible. `GitBackend`'s methods
    /// are pure delegations to the same `git::*` free functions this file
    /// called directly before this change, so the values `WorkflowContext`
    /// exposes must be identical to calling those free functions straight
    /// against the fixture.
    #[test]
    fn new_in_matches_git_free_functions_for_git_repo() {
        let temp = tempdir().unwrap();
        crate::test_support::init_repo(temp.path());

        let context = WorkflowContext::new_in(
            temp.path(),
            config::Config::default(),
            Arc::new(TestMux),
            None,
        )
        .unwrap();

        let execution_dir = temp.path().canonicalize().unwrap();

        assert_eq!(
            context.main_worktree_root,
            git::get_main_worktree_root_in(Some(&execution_dir)).unwrap()
        );
        assert_eq!(
            context.git_common_dir,
            git::get_git_common_dir_in(Some(&execution_dir)).unwrap()
        );
        assert_eq!(
            context.main_branch,
            git::get_default_branch_in(Some(&execution_dir)).unwrap()
        );
        assert_eq!(context.vcs.name(), "git");
    }

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
