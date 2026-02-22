use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::MuxMode;

use super::{Vcs, VcsStatus};

/// Jujutsu (jj) implementation of the Vcs trait.
///
/// Stub implementation - methods will be filled in during Phases 3-5.
pub struct JjVcs;

impl JjVcs {
    pub fn new() -> Self {
        JjVcs
    }
}

/// Helper to return a "not yet implemented" error for jj operations.
fn jj_todo(operation: &str) -> anyhow::Error {
    anyhow!("jj support not yet implemented: {}", operation)
}

impl Vcs for JjVcs {
    fn name(&self) -> &str {
        "jj"
    }

    // ── Repo detection ───────────────────────────────────────────────

    fn is_repo(&self) -> Result<bool> {
        // Walk up from CWD looking for .jj directory
        let cwd = std::env::current_dir()?;
        for dir in cwd.ancestors() {
            if dir.join(".jj").is_dir() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_commits(&self) -> Result<bool> {
        Err(jj_todo("has_commits"))
    }

    fn get_repo_root(&self) -> Result<PathBuf> {
        Err(jj_todo("get_repo_root"))
    }

    fn get_repo_root_for(&self, _dir: &Path) -> Result<PathBuf> {
        Err(jj_todo("get_repo_root_for"))
    }

    fn get_main_workspace_root(&self) -> Result<PathBuf> {
        Err(jj_todo("get_main_workspace_root"))
    }

    fn get_shared_dir(&self) -> Result<PathBuf> {
        Err(jj_todo("get_shared_dir"))
    }

    fn is_path_ignored(&self, _repo_path: &Path, _file_path: &str) -> bool {
        false // TODO: implement jj ignore checking
    }

    // ── Workspace lifecycle ──────────────────────────────────────────

    fn workspace_exists(&self, _branch_name: &str) -> Result<bool> {
        Err(jj_todo("workspace_exists"))
    }

    fn create_workspace(
        &self,
        _path: &Path,
        _branch: &str,
        _create_branch: bool,
        _base: Option<&str>,
        _track_upstream: bool,
    ) -> Result<()> {
        Err(jj_todo("create_workspace"))
    }

    fn list_workspaces(&self) -> Result<Vec<(PathBuf, String)>> {
        Err(jj_todo("list_workspaces"))
    }

    fn find_workspace(&self, _name: &str) -> Result<(PathBuf, String)> {
        Err(jj_todo("find_workspace"))
    }

    fn get_workspace_path(&self, _branch: &str) -> Result<PathBuf> {
        Err(jj_todo("get_workspace_path"))
    }

    fn prune_workspaces(&self, _shared_dir: &Path) -> Result<()> {
        Err(jj_todo("prune_workspaces"))
    }

    // ── Workspace metadata ───────────────────────────────────────────

    fn set_workspace_meta(&self, _handle: &str, _key: &str, _value: &str) -> Result<()> {
        Err(jj_todo("set_workspace_meta"))
    }

    fn get_workspace_meta(&self, _handle: &str, _key: &str) -> Option<String> {
        None // TODO: implement jj config reading
    }

    fn get_workspace_mode(&self, _handle: &str) -> MuxMode {
        MuxMode::Window // Default until jj metadata is implemented
    }

    fn get_all_workspace_modes(&self) -> HashMap<String, MuxMode> {
        HashMap::new() // TODO: implement jj config batch reading
    }

    fn remove_workspace_meta(&self, _handle: &str) -> Result<()> {
        Err(jj_todo("remove_workspace_meta"))
    }

    // ── Branch/bookmark operations ───────────────────────────────────

    fn get_default_branch(&self) -> Result<String> {
        Err(jj_todo("get_default_branch"))
    }

    fn get_default_branch_in(&self, _workdir: Option<&Path>) -> Result<String> {
        Err(jj_todo("get_default_branch_in"))
    }

    fn branch_exists(&self, _name: &str) -> Result<bool> {
        Err(jj_todo("branch_exists"))
    }

    fn branch_exists_in(&self, _name: &str, _workdir: Option<&Path>) -> Result<bool> {
        Err(jj_todo("branch_exists_in"))
    }

    fn get_current_branch(&self) -> Result<String> {
        Err(jj_todo("get_current_branch"))
    }

    fn list_checkout_branches(&self) -> Result<Vec<String>> {
        Err(jj_todo("list_checkout_branches"))
    }

    fn delete_branch(&self, _name: &str, _force: bool, _shared_dir: &Path) -> Result<()> {
        Err(jj_todo("delete_branch"))
    }

    fn get_merge_base(&self, _main_branch: &str) -> Result<String> {
        Err(jj_todo("get_merge_base"))
    }

    fn get_unmerged_branches(&self, _base: &str) -> Result<HashSet<String>> {
        Err(jj_todo("get_unmerged_branches"))
    }

    fn get_gone_branches(&self) -> Result<HashSet<String>> {
        Err(jj_todo("get_gone_branches"))
    }

    // ── Base branch tracking ─────────────────────────────────────────

    fn set_branch_base(&self, _branch: &str, _base: &str) -> Result<()> {
        Err(jj_todo("set_branch_base"))
    }

    fn get_branch_base(&self, _branch: &str) -> Result<String> {
        Err(jj_todo("get_branch_base"))
    }

    fn get_branch_base_in(&self, _branch: &str, _workdir: Option<&Path>) -> Result<String> {
        Err(jj_todo("get_branch_base_in"))
    }

    // ── Status ───────────────────────────────────────────────────────

    fn get_status(&self, _worktree: &Path) -> VcsStatus {
        VcsStatus::default() // TODO: implement jj status
    }

    fn has_uncommitted_changes(&self, _worktree: &Path) -> Result<bool> {
        Err(jj_todo("has_uncommitted_changes"))
    }

    fn has_tracked_changes(&self, _worktree: &Path) -> Result<bool> {
        Err(jj_todo("has_tracked_changes"))
    }

    fn has_untracked_files(&self, _worktree: &Path) -> Result<bool> {
        Err(jj_todo("has_untracked_files"))
    }

    fn has_staged_changes(&self, _worktree: &Path) -> Result<bool> {
        // jj has no staging area - "staged" is equivalent to "has changes"
        Err(jj_todo("has_staged_changes"))
    }

    fn has_unstaged_changes(&self, _worktree: &Path) -> Result<bool> {
        // jj has no staging area - "unstaged" is equivalent to "has changes"
        Err(jj_todo("has_unstaged_changes"))
    }

    // ── Merge operations ─────────────────────────────────────────────

    fn commit_with_editor(&self, _worktree: &Path) -> Result<()> {
        Err(jj_todo("commit_with_editor"))
    }

    fn merge_in_workspace(&self, _worktree: &Path, _branch: &str) -> Result<()> {
        Err(jj_todo("merge_in_workspace"))
    }

    fn rebase_onto_base(&self, _worktree: &Path, _base: &str) -> Result<()> {
        Err(jj_todo("rebase_onto_base"))
    }

    fn merge_squash(&self, _worktree: &Path, _branch: &str) -> Result<()> {
        Err(jj_todo("merge_squash"))
    }

    fn switch_branch(&self, _worktree: &Path, _branch: &str) -> Result<()> {
        Err(jj_todo("switch_branch"))
    }

    fn stash_push(&self, _msg: &str, _untracked: bool, _patch: bool) -> Result<()> {
        // jj doesn't need stash - working copy is always committed
        Ok(())
    }

    fn stash_pop(&self, _worktree: &Path) -> Result<()> {
        // jj doesn't need stash - working copy is always committed
        Ok(())
    }

    fn reset_hard(&self, _worktree: &Path) -> Result<()> {
        Err(jj_todo("reset_hard"))
    }

    fn abort_merge(&self, _worktree: &Path) -> Result<()> {
        Err(jj_todo("abort_merge"))
    }

    // ── Remotes ──────────────────────────────────────────────────────

    fn list_remotes(&self) -> Result<Vec<String>> {
        Err(jj_todo("list_remotes"))
    }

    fn remote_exists(&self, _name: &str) -> Result<bool> {
        Err(jj_todo("remote_exists"))
    }

    fn fetch_remote(&self, _remote: &str) -> Result<()> {
        Err(jj_todo("fetch_remote"))
    }

    fn fetch_prune(&self) -> Result<()> {
        Err(jj_todo("fetch_prune"))
    }

    fn add_remote(&self, _name: &str, _url: &str) -> Result<()> {
        Err(jj_todo("add_remote"))
    }

    fn set_remote_url(&self, _name: &str, _url: &str) -> Result<()> {
        Err(jj_todo("set_remote_url"))
    }

    fn get_remote_url(&self, _remote: &str) -> Result<String> {
        Err(jj_todo("get_remote_url"))
    }

    fn ensure_fork_remote(&self, _owner: &str) -> Result<String> {
        Err(jj_todo("ensure_fork_remote"))
    }

    fn get_repo_owner(&self) -> Result<String> {
        Err(jj_todo("get_repo_owner"))
    }

    // ── Deferred cleanup ─────────────────────────────────────────────

    fn build_cleanup_commands(
        &self,
        _shared_dir: &Path,
        _branch: &str,
        _handle: &str,
        _keep_branch: bool,
        _force: bool,
    ) -> Vec<String> {
        Vec::new() // TODO: implement jj cleanup commands
    }

    // ── Status cache ─────────────────────────────────────────────────

    fn load_status_cache(&self) -> HashMap<PathBuf, VcsStatus> {
        // Reuse the same cache infrastructure as git
        crate::git::load_status_cache()
    }

    fn save_status_cache(&self, statuses: &HashMap<PathBuf, VcsStatus>) {
        crate::git::save_status_cache(statuses)
    }
}
