use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::MuxMode;
use crate::git;
use crate::shell::shell_quote;

use super::{Vcs, VcsStatus};

/// Git implementation of the Vcs trait.
///
/// Delegates to the existing `git::*` module functions.
pub struct GitVcs;

impl GitVcs {
    pub fn new() -> Self {
        GitVcs
    }
}

impl Vcs for GitVcs {
    fn name(&self) -> &str {
        "git"
    }

    // ── Repo detection ───────────────────────────────────────────────

    fn is_repo(&self) -> Result<bool> {
        git::is_git_repo()
    }

    fn has_commits(&self) -> Result<bool> {
        git::has_commits()
    }

    fn get_repo_root(&self) -> Result<PathBuf> {
        git::get_repo_root()
    }

    fn get_repo_root_for(&self, dir: &Path) -> Result<PathBuf> {
        git::get_repo_root_for(dir)
    }

    fn get_main_workspace_root(&self) -> Result<PathBuf> {
        git::get_main_worktree_root()
    }

    fn get_shared_dir(&self) -> Result<PathBuf> {
        git::get_git_common_dir()
    }

    fn is_path_ignored(&self, repo_path: &Path, file_path: &str) -> bool {
        git::is_path_ignored(repo_path, file_path)
    }

    // ── Workspace lifecycle ──────────────────────────────────────────

    fn workspace_exists(&self, branch_name: &str) -> Result<bool> {
        git::worktree_exists(branch_name)
    }

    fn create_workspace(
        &self,
        path: &Path,
        branch: &str,
        create_branch: bool,
        base: Option<&str>,
        track_upstream: bool,
    ) -> Result<()> {
        git::create_worktree(path, branch, create_branch, base, track_upstream)
    }

    fn list_workspaces(&self) -> Result<Vec<(PathBuf, String)>> {
        git::list_worktrees()
    }

    fn find_workspace(&self, name: &str) -> Result<(PathBuf, String)> {
        git::find_worktree(name)
    }

    fn get_workspace_path(&self, branch: &str) -> Result<PathBuf> {
        git::get_worktree_path(branch)
    }

    fn prune_workspaces(&self, shared_dir: &Path) -> Result<()> {
        git::prune_worktrees_in(shared_dir)
    }

    // ── Workspace metadata ───────────────────────────────────────────

    fn set_workspace_meta(&self, handle: &str, key: &str, value: &str) -> Result<()> {
        git::set_worktree_meta(handle, key, value)
    }

    fn get_workspace_meta(&self, handle: &str, key: &str) -> Option<String> {
        git::get_worktree_meta(handle, key)
    }

    fn get_workspace_mode(&self, handle: &str) -> MuxMode {
        git::get_worktree_mode(handle)
    }

    fn get_all_workspace_modes(&self) -> HashMap<String, MuxMode> {
        git::get_all_worktree_modes()
    }

    fn remove_workspace_meta(&self, handle: &str) -> Result<()> {
        git::remove_worktree_meta(handle)
    }

    // ── Branch/bookmark operations ───────────────────────────────────

    fn get_default_branch(&self) -> Result<String> {
        git::get_default_branch()
    }

    fn get_default_branch_in(&self, workdir: Option<&Path>) -> Result<String> {
        git::get_default_branch_in(workdir)
    }

    fn branch_exists(&self, name: &str) -> Result<bool> {
        git::branch_exists(name)
    }

    fn branch_exists_in(&self, name: &str, workdir: Option<&Path>) -> Result<bool> {
        git::branch_exists_in(name, workdir)
    }

    fn get_current_branch(&self) -> Result<String> {
        git::get_current_branch()
    }

    fn list_checkout_branches(&self) -> Result<Vec<String>> {
        git::list_checkout_branches()
    }

    fn delete_branch(&self, name: &str, force: bool, shared_dir: &Path) -> Result<()> {
        git::delete_branch_in(name, force, shared_dir)
    }

    fn get_merge_base(&self, main_branch: &str) -> Result<String> {
        git::get_merge_base(main_branch)
    }

    fn get_unmerged_branches(&self, base: &str) -> Result<HashSet<String>> {
        git::get_unmerged_branches(base)
    }

    fn get_gone_branches(&self) -> Result<HashSet<String>> {
        git::get_gone_branches()
    }

    // ── Base branch tracking ─────────────────────────────────────────

    fn set_branch_base(&self, branch: &str, base: &str) -> Result<()> {
        git::set_branch_base(branch, base)
    }

    fn get_branch_base(&self, branch: &str) -> Result<String> {
        git::get_branch_base(branch)
    }

    fn get_branch_base_in(&self, branch: &str, workdir: Option<&Path>) -> Result<String> {
        git::get_branch_base_in(branch, workdir)
    }

    // ── Status ───────────────────────────────────────────────────────

    fn get_status(&self, worktree: &Path) -> VcsStatus {
        git::get_git_status(worktree)
    }

    fn has_uncommitted_changes(&self, worktree: &Path) -> Result<bool> {
        git::has_uncommitted_changes(worktree)
    }

    fn has_tracked_changes(&self, worktree: &Path) -> Result<bool> {
        git::has_tracked_changes(worktree)
    }

    fn has_untracked_files(&self, worktree: &Path) -> Result<bool> {
        git::has_untracked_files(worktree)
    }

    fn has_staged_changes(&self, worktree: &Path) -> Result<bool> {
        git::has_staged_changes(worktree)
    }

    fn has_unstaged_changes(&self, worktree: &Path) -> Result<bool> {
        git::has_unstaged_changes(worktree)
    }

    // ── Merge operations ─────────────────────────────────────────────

    fn commit_with_editor(&self, worktree: &Path) -> Result<()> {
        git::commit_with_editor(worktree)
    }

    fn merge_in_workspace(&self, worktree: &Path, branch: &str) -> Result<()> {
        git::merge_in_worktree(worktree, branch)
    }

    fn rebase_onto_base(&self, worktree: &Path, base: &str) -> Result<()> {
        git::rebase_branch_onto_base(worktree, base)
    }

    fn merge_squash(&self, worktree: &Path, branch: &str) -> Result<()> {
        git::merge_squash_in_worktree(worktree, branch)
    }

    fn switch_branch(&self, worktree: &Path, branch: &str) -> Result<()> {
        git::switch_branch_in_worktree(worktree, branch)
    }

    fn stash_push(&self, msg: &str, untracked: bool, patch: bool) -> Result<()> {
        git::stash_push(msg, untracked, patch)
    }

    fn stash_pop(&self, worktree: &Path) -> Result<()> {
        git::stash_pop(worktree)
    }

    fn reset_hard(&self, worktree: &Path) -> Result<()> {
        git::reset_hard(worktree)
    }

    fn abort_merge(&self, worktree: &Path) -> Result<()> {
        git::abort_merge_in_worktree(worktree)
    }

    // ── Remotes ──────────────────────────────────────────────────────

    fn list_remotes(&self) -> Result<Vec<String>> {
        git::list_remotes()
    }

    fn remote_exists(&self, name: &str) -> Result<bool> {
        git::remote_exists(name)
    }

    fn fetch_remote(&self, remote: &str) -> Result<()> {
        git::fetch_remote(remote)
    }

    fn fetch_prune(&self) -> Result<()> {
        git::fetch_prune()
    }

    fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        git::add_remote(name, url)
    }

    fn set_remote_url(&self, name: &str, url: &str) -> Result<()> {
        git::set_remote_url(name, url)
    }

    fn get_remote_url(&self, remote: &str) -> Result<String> {
        git::get_remote_url(remote)
    }

    fn ensure_fork_remote(&self, owner: &str) -> Result<String> {
        git::ensure_fork_remote(owner)
    }

    fn get_repo_owner(&self) -> Result<String> {
        git::get_repo_owner()
    }

    // ── Deferred cleanup ─────────────────────────────────────────────

    fn build_cleanup_commands(
        &self,
        shared_dir: &Path,
        branch: &str,
        handle: &str,
        keep_branch: bool,
        force: bool,
    ) -> Vec<String> {
        let git_dir = shell_quote(&shared_dir.to_string_lossy());

        let mut cmds = Vec::new();

        // Prune git worktrees
        cmds.push(format!(
            "git -C {} worktree prune >/dev/null 2>&1",
            git_dir
        ));

        // Delete branch (if not keeping)
        if !keep_branch {
            let branch_q = shell_quote(branch);
            let force_flag = if force { "-D" } else { "-d" };
            cmds.push(format!(
                "git -C {} branch {} {} >/dev/null 2>&1",
                git_dir, force_flag, branch_q
            ));
        }

        // Remove worktree metadata from git config
        let handle_q = shell_quote(handle);
        cmds.push(format!(
            "git -C {} config --local --remove-section workmux.worktree.{} >/dev/null 2>&1",
            git_dir, handle_q
        ));

        cmds
    }

    // ── Status cache ─────────────────────────────────────────────────

    fn load_status_cache(&self) -> HashMap<PathBuf, VcsStatus> {
        git::load_status_cache()
    }

    fn save_status_cache(&self, statuses: &HashMap<PathBuf, VcsStatus>) {
        git::save_status_cache(statuses)
    }
}
