//! `GitBackend`: a [`crate::vcs::VcsBackend`] implementation that delegates
//! to the existing `crate::git::*` free functions.

use anyhow::Result;
use std::path::Path;

use crate::cmd::Cmd;
use crate::git;
use crate::vcs::{
    CreateWorkspaceOptions, CreationLock, VcsBackend, VcsStatus, WorkmuxMetaStore, WorkspaceEntry,
};

/// [`VcsBackend`] implementation backed by the system `git` binary, via the
/// existing `crate::git::*` free functions.
pub struct GitBackend;

impl VcsBackend for GitBackend {
    fn name(&self) -> &'static str {
        "git"
    }

    fn is_repo_in(&self, workdir: Option<&Path>) -> Result<bool> {
        git::is_git_repo_in(workdir)
    }

    fn get_main_worktree_root_in(&self, workdir: Option<&Path>) -> Result<std::path::PathBuf> {
        git::get_main_worktree_root_in(workdir)
    }

    fn get_repo_root_in(&self, workdir: Option<&Path>) -> Result<std::path::PathBuf> {
        git::get_repo_root_in(workdir)
    }

    fn get_common_dir_in(&self, workdir: Option<&Path>) -> Result<std::path::PathBuf> {
        git::get_git_common_dir_in(workdir)
    }

    fn has_commits_in(&self, workdir: Option<&Path>) -> Result<bool> {
        git::has_commits_in(workdir)
    }

    fn create_workspace_in(
        &self,
        opts: &CreateWorkspaceOptions,
        workdir: Option<&Path>,
    ) -> Result<()> {
        git::create_worktree_in(
            &opts.path,
            &opts.name_or_branch,
            opts.create_branch,
            opts.base.as_deref(),
            opts.track_upstream,
            workdir,
        )
    }

    fn list_workspaces_in(&self, workdir: Option<&Path>) -> Result<Vec<WorkspaceEntry>> {
        Ok(git::list_worktrees_in(workdir)?
            .into_iter()
            .map(|(path, branch)| {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                let branch_or_bookmark = if branch == "(detached)" {
                    None
                } else {
                    Some(branch)
                };
                WorkspaceEntry {
                    path,
                    name,
                    branch_or_bookmark,
                }
            })
            .collect())
    }

    fn move_workspace(&self, old_path: &Path, new_path: &Path) -> Result<()> {
        git::move_worktree(old_path, new_path)
    }

    // NOT a delegation, and NOT a substitute for `workflow::cleanup`'s removal flow.
    //
    // There is no dedicated `git::worktree` free function for `git worktree
    // remove` today, so unlike every other `GitBackend` method this one is
    // not delegating to an existing `git::*` free function — it is new
    // git-invocation logic written directly against `Cmd`.
    //
    // Its semantics are also intentionally minimal and DO NOT match the real
    // worktree-removal path in `src/workflow/cleanup.rs`. That flow:
    //   - removes any linked-worktree `locked` file before touching the
    //     worktree, so a lock doesn't block `git worktree prune`;
    //   - quarantines (renames) the worktree directory before deleting it,
    //     so a process that still has the old path as its CWD doesn't get
    //     yanked out from under it mid-command;
    //   - carefully orders branch/metadata cleanup around the prune step.
    //
    // This method does none of that: it shells out to `git worktree remove
    // --force` synchronously, in place, with no lock handling and no
    // quarantine-before-delete safety. Do not wire a real call site to
    // `VcsBackend::remove_workspace_at` as a drop-in replacement for
    // `workflow::cleanup`'s removal flow. A future task must either extend
    // this method to match those semantics, or keep routing real worktree
    // removal through `workflow::cleanup` instead of this trait method,
    // until that work is done.
    fn remove_workspace_at(&self, handle_or_name: &str, common_dir: &Path) -> Result<()> {
        let (path, _branch) = git::find_worktree_in(handle_or_name, Some(common_dir))?;
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid worktree path"))?;
        // See the doc-comment above: this is a direct `git worktree remove
        // --force` invocation, not a delegation, and it skips the
        // lock-file/quarantine handling that `workflow::cleanup` performs.
        Cmd::new("git")
            .workdir(common_dir)
            .args(&["worktree", "remove", "--force", path_str])
            .run()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("Failed to remove worktree '{}': {}", handle_or_name, e))
    }

    fn prune_workspaces_in(&self, common_dir: &Path) -> Result<()> {
        git::prune_worktrees_in(common_dir)
    }

    fn get_default_branch_in(&self, workdir: Option<&Path>) -> Result<String> {
        git::get_default_branch_in(workdir)
    }

    fn branch_exists_in(&self, name: &str, workdir: Option<&Path>) -> Result<bool> {
        git::branch_exists_in(name, workdir)
    }

    fn get_current_branch_in(&self, workdir: &Path) -> Result<Option<String>> {
        // git always has a branch or is detached; `git branch --show-current`
        // returns an empty string when HEAD is detached.
        let branch = git::get_current_branch_in(workdir)?;
        Ok(if branch.is_empty() {
            None
        } else {
            Some(branch)
        })
    }

    fn delete_branch_in(&self, name: &str, force: bool, common_dir: &Path) -> Result<()> {
        git::delete_branch_in(name, force, common_dir)
    }

    fn get_merge_base_in(&self, workdir: Option<&Path>, base: &str) -> Result<String> {
        git::get_merge_base_in(workdir, base)
    }

    fn get_status(&self, workspace_path: &Path, main_branch: Option<&str>) -> Result<VcsStatus> {
        Ok(git::get_git_status(workspace_path, main_branch))
    }

    fn meta(&self) -> &dyn WorkmuxMetaStore {
        const INSTANCE: GitConfigMetaStore = GitConfigMetaStore;
        &INSTANCE
    }

    fn lock_creation_sequence(&self, common_dir: &Path) -> Result<CreationLock> {
        // `git worktree add` and every `git config` write both take
        // `.git/config.lock`, so the whole creation sequence has to be
        // serialized across processes or parallel `workmux add` runs fail
        // with "could not lock config file".
        // The caller adds the user-facing context, so don't duplicate it here.
        Ok(Box::new(git::GitConfigLock::acquire(common_dir)?))
    }

    fn get_gone_branches_in(&self, common_dir: &Path) -> Result<Vec<String>> {
        // `git::get_gone_branches` has no `_in` variant (it always inspects
        // the process's current directory), so `common_dir` can't be
        // threaded through; this mirrors the existing free function's
        // limitation rather than introducing a new one.
        let _ = common_dir;
        Ok(git::get_gone_branches()?.into_iter().collect())
    }

    fn fetch_prune_in(&self, workdir: Option<&Path>) -> Result<()> {
        // `git::remote::fetch_prune` has no `_in` variant either.
        let _ = workdir;
        git::fetch_prune()
    }

    fn get_unmerged_branches_in(
        &self,
        workdir: Option<&Path>,
        base_commit: &str,
    ) -> Result<Option<std::collections::HashSet<String>>> {
        Ok(Some(git::get_unmerged_branches_in(workdir, base_commit)?))
    }
}

/// [`WorkmuxMetaStore`] implementation backed by git config, via the
/// existing `crate::git::*` free functions. Stateless.
pub struct GitConfigMetaStore;

impl WorkmuxMetaStore for GitConfigMetaStore {
    fn get(&self, handle: &str, key: &str, workdir: Option<&Path>) -> Option<String> {
        git::get_worktree_meta_in(handle, key, workdir)
    }

    fn set(&self, handle: &str, key: &str, value: &str, workdir: Option<&Path>) -> Result<()> {
        git::set_worktree_meta_in(handle, key, value, workdir)
    }

    fn remove_all_at(&self, handle: &str, common_dir: &Path) -> Result<()> {
        git::remove_worktree_meta_at(handle, common_dir)
    }

    fn migrate(&self, old_handle: &str, new_handle: &str, workdir: Option<&Path>) -> Result<()> {
        // `git::migrate_worktree_meta` has no `_in` variant (it always
        // operates on the process's current directory via `Cmd::new("git")`
        // with no explicit workdir), so `workdir` can't be threaded through.
        let _ = workdir;
        git::migrate_worktree_meta(old_handle, new_handle)
    }

    fn get_branch_base(&self, branch: &str, workdir: Option<&Path>) -> Option<String> {
        git::get_branch_base_in(branch, workdir).ok()
    }

    fn set_branch_base(&self, branch: &str, base: &str, workdir: Option<&Path>) -> Result<()> {
        git::set_branch_base_in(branch, base, workdir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn is_repo_in_matches_git_free_function() {
        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());

        let backend = GitBackend;
        assert_eq!(
            backend.is_repo_in(Some(temp.path())).unwrap(),
            git::is_git_repo_in(Some(temp.path())).unwrap()
        );
        assert!(backend.is_repo_in(Some(temp.path())).unwrap());
    }

    #[test]
    fn get_main_worktree_root_in_matches_git_free_function() {
        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());

        let backend = GitBackend;
        let expected = git::get_main_worktree_root_in(Some(temp.path())).unwrap();
        let actual = backend
            .get_main_worktree_root_in(Some(temp.path()))
            .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn get_common_dir_in_matches_git_free_function() {
        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());

        let backend = GitBackend;
        assert_eq!(
            backend.get_common_dir_in(Some(temp.path())).unwrap(),
            git::get_git_common_dir_in(Some(temp.path())).unwrap()
        );
    }

    #[test]
    fn has_commits_in_matches_git_free_function() {
        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());

        let backend = GitBackend;
        assert_eq!(
            backend.has_commits_in(Some(temp.path())).unwrap(),
            git::has_commits_in(Some(temp.path())).unwrap()
        );
        assert!(backend.has_commits_in(Some(temp.path())).unwrap());
    }

    #[test]
    fn get_default_branch_in_matches_git_free_function() {
        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());

        let backend = GitBackend;
        assert_eq!(
            backend.get_default_branch_in(Some(temp.path())).unwrap(),
            git::get_default_branch_in(Some(temp.path())).unwrap()
        );
        assert_eq!(
            backend.get_default_branch_in(Some(temp.path())).unwrap(),
            "main"
        );
    }

    #[test]
    fn branch_exists_in_matches_git_free_function() {
        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());

        let backend = GitBackend;
        assert_eq!(
            backend.branch_exists_in("main", Some(temp.path())).unwrap(),
            git::branch_exists_in("main", Some(temp.path())).unwrap()
        );
        assert!(
            !backend
                .branch_exists_in("does-not-exist", Some(temp.path()))
                .unwrap()
        );
    }

    #[test]
    fn get_current_branch_in_matches_git_free_function_mapped_to_option() {
        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());

        let backend = GitBackend;
        let direct = git::get_current_branch_in(temp.path()).unwrap();
        assert_eq!(
            backend.get_current_branch_in(temp.path()).unwrap(),
            Some(direct)
        );
    }

    #[test]
    fn get_status_matches_git_free_function() {
        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());

        let backend = GitBackend;
        let expected = git::get_git_status(temp.path(), None);
        let actual = backend.get_status(temp.path(), None).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn create_list_and_remove_workspace_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());
        let backend = GitBackend;

        let worktree_path = temp.path().join("feature-wt");
        let opts = CreateWorkspaceOptions {
            path: worktree_path.clone(),
            name_or_branch: "feature".to_string(),
            create_branch: true,
            base: Some("main".to_string()),
            track_upstream: false,
        };
        backend
            .create_workspace_in(&opts, Some(temp.path()))
            .unwrap();

        let entries = backend.list_workspaces_in(Some(temp.path())).unwrap();
        let entry = entries
            .iter()
            .find(|e| e.path == worktree_path)
            .expect("created worktree should be listed");
        assert_eq!(entry.branch_or_bookmark.as_deref(), Some("feature"));

        let common_dir = backend.get_common_dir_in(Some(temp.path())).unwrap();
        backend
            .remove_workspace_at("feature-wt", &common_dir)
            .unwrap();

        let entries_after = backend.list_workspaces_in(Some(temp.path())).unwrap();
        assert!(!entries_after.iter().any(|e| e.path == worktree_path));
    }

    #[test]
    fn meta_get_set_round_trip_matches_git_free_functions() {
        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());
        let backend = GitBackend;

        backend
            .meta()
            .set("some-handle", "mode", "window", Some(temp.path()))
            .unwrap();
        assert_eq!(
            backend.meta().get("some-handle", "mode", Some(temp.path())),
            git::get_worktree_meta_in("some-handle", "mode", Some(temp.path()))
        );
        assert_eq!(
            backend.meta().get("some-handle", "mode", Some(temp.path())),
            Some("window".to_string())
        );
    }

    #[test]
    fn lock_creation_sequence_still_serializes_git_config_writes_across_processes() {
        use std::sync::mpsc;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());
        let common_dir = git::get_git_common_dir_in(Some(temp.path())).unwrap();

        let backend = GitBackend;
        let guard = backend.lock_creation_sequence(&common_dir).unwrap();

        // A competing acquisition (what a parallel `workmux add` process does)
        // must block while the creation-sequence guard is held - this is the
        // behavior that prevents git's "could not lock config file" race.
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let thread_dir = common_dir.clone();
        let handle = std::thread::spawn(move || {
            let _second = git::GitConfigLock::acquire(&thread_dir).unwrap();
            acquired_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });

        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "GitBackend::lock_creation_sequence did not hold an exclusive lock"
        );

        drop(guard);

        acquired_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("lock was not released when the creation guard dropped");
        release_tx.send(()).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn jj_backend_creation_sequence_lock_is_a_no_op() {
        // jj's metadata store locks internally per write and its workspace
        // creation shares no `.git/config`-style file, so the default
        // (no-op) guard applies and must never block.
        let temp = tempfile::tempdir().unwrap();
        let backend = crate::vcs::JjBackend;
        let first = backend.lock_creation_sequence(temp.path()).unwrap();
        let second = backend.lock_creation_sequence(temp.path()).unwrap();
        drop(first);
        drop(second);
    }

    #[test]
    fn meta_branch_base_round_trip_matches_git_free_functions() {
        let temp = tempfile::tempdir().unwrap();
        test_support::init_repo(temp.path());
        let backend = GitBackend;

        backend
            .meta()
            .set_branch_base("main", "origin/main", Some(temp.path()))
            .unwrap();
        assert_eq!(
            backend.meta().get_branch_base("main", Some(temp.path())),
            git::get_branch_base_in("main", Some(temp.path())).ok()
        );
        assert_eq!(
            backend.meta().get_branch_base("main", Some(temp.path())),
            Some("origin/main".to_string())
        );
    }
}
