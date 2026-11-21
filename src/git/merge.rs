use crate::cmd::Cmd;
use anyhow::{Context, Result, anyhow};
use std::path::Path;

/// Commit staged changes in a worktree using the user's editor
pub fn commit_with_editor(worktree_path: &Path) -> Result<()> {
    let status = super::interactive_git(worktree_path)?
        .arg("commit")
        .status()
        .context("Failed to run git commit")?;

    if !status.success() {
        return Err(anyhow!("Commit was aborted or failed"));
    }

    Ok(())
}

/// Merge a branch into the current branch in a specific worktree
pub fn merge_in_worktree(worktree_path: &Path, branch_name: &str) -> Result<()> {
    Cmd::new("git")
        .workdir(worktree_path)
        .args(&["merge", branch_name])
        .run()
        .context("Failed to merge")?;
    Ok(())
}

/// Rebase the current branch in a worktree onto a base branch
pub fn rebase_branch_onto_base(worktree_path: &Path, base_branch: &str) -> Result<()> {
    Cmd::new("git")
        .workdir(worktree_path)
        .args(&["rebase", base_branch])
        .run()
        .with_context(|| format!("Failed to rebase onto '{}'", base_branch))?;
    Ok(())
}

/// Perform a squash merge in a specific worktree (does not commit)
pub fn merge_squash_in_worktree(worktree_path: &Path, branch_name: &str) -> Result<()> {
    Cmd::new("git")
        .workdir(worktree_path)
        .args(&["merge", "--squash", branch_name])
        .run()
        .context("Failed to perform squash merge")?;
    Ok(())
}

/// Switch to a different branch in a specific worktree
pub fn switch_branch_in_worktree(worktree_path: &Path, branch_name: &str) -> Result<()> {
    Cmd::new("git")
        .workdir(worktree_path)
        .args(&["switch", branch_name])
        .run()
        .with_context(|| {
            format!(
                "Failed to switch to branch '{}' in worktree '{}'",
                branch_name,
                worktree_path.display()
            )
        })?;
    Ok(())
}

/// Stash uncommitted changes in `workdir`, optionally including untracked files or using
/// patch mode, and return the commit id of the new entry.
///
/// `message` must be unique to this invocation: the entry is found by it in the stash list,
/// so a concurrent `git stash` in another worktree of the repository is never picked up.
/// Returns `None` when there was nothing to stash.
pub fn stash_push(
    workdir: &Path,
    message: &str,
    include_untracked: bool,
    patch: bool,
) -> Result<Option<String>> {
    if patch {
        if super::has_staged_changes(workdir)? {
            return Err(anyhow!(
                "--patch cannot move staged changes; omit --patch to move staged and unstaged changes together"
            ));
        }
        let status = super::interactive_git(workdir)?
            .args(["stash", "push", "-m", message, "--patch"])
            .status()
            .context("Failed to run interactive git stash")?;

        if !status.success() {
            return Err(anyhow!(
                "Git stash --patch failed. Make sure you select at least one hunk."
            ));
        }
    } else {
        let mut cmd = Cmd::new("git")
            .workdir(workdir)
            .args(&["stash", "push", "-m", message]);

        if include_untracked {
            cmd = cmd.arg("--include-untracked");
        }

        cmd.run().context("Failed to stash changes")?;
    }
    stash_commit_by_message(workdir, message)
}

/// Apply the stash entry with commit id `sha` in `workdir`, keeping the entry.
pub fn stash_apply(workdir: &Path, sha: &str) -> Result<()> {
    Cmd::new("git")
        .workdir(workdir)
        .args(&["stash", "apply", "--index", sha])
        .run()
        .context("Failed to apply stashed changes. Conflicts may have occurred.")?;
    Ok(())
}

/// Find the commit id of the stash entry whose message is `message`.
fn stash_commit_by_message(workdir: &Path, message: &str) -> Result<Option<String>> {
    // Entry subjects read "On <branch>: <message>" (or "WIP on ..." without -m).
    let suffix = format!(": {message}");
    Ok(stash_entries(workdir)?
        .into_iter()
        .find(|(_, subject)| subject.ends_with(&suffix))
        .map(|(sha, _)| sha))
}

/// List stash entries, newest first, as (commit id, subject) pairs.
fn stash_entries(workdir: &Path) -> Result<Vec<(String, String)>> {
    let output = Cmd::new("git")
        .workdir(workdir)
        .args(&["stash", "list", "--format=%H%x00%gs"])
        .run()
        .context("Failed to list stash entries")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let (sha, subject) = line.split_once('\0')?;
            Some((sha.to_string(), subject.to_string()))
        })
        .collect())
}

/// Reset the worktree to HEAD, discarding all local changes.
pub fn reset_hard(worktree_path: &Path) -> Result<()> {
    Cmd::new("git")
        .workdir(worktree_path)
        .args(&["reset", "--hard", "HEAD"])
        .run()
        .context("Failed to reset worktree")?;
    Ok(())
}

/// Abort a merge in progress in a specific worktree
pub fn abort_merge_in_worktree(worktree_path: &Path) -> Result<()> {
    Cmd::new("git")
        .workdir(worktree_path)
        .args(&["merge", "--abort"])
        .run()
        .context("Failed to abort merge. The worktree may not be in a merging state.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();
        git(path, &["init", "-qb", "main"]);
        git(path, &["config", "user.name", "Test"]);
        git(path, &["config", "user.email", "test@example.com"]);
        git(path, &["config", "commit.gpgSign", "false"]);
        std::fs::write(path.join("tracked.txt"), "base\n").unwrap();
        git(path, &["add", "."]);
        git(path, &["commit", "-qm", "base"]);
        temp
    }

    #[test]
    fn stash_push_returns_commit_of_tracked_changes() {
        let temp = repo();
        let path = temp.path();
        std::fs::write(path.join("tracked.txt"), "changed\n").unwrap();

        let sha = stash_push(path, "workmux: test [1]", false, false)
            .unwrap()
            .expect("tracked change should be stashed");

        assert_eq!(sha, git(path, &["rev-parse", "stash@{0}"]));
        assert_eq!(
            std::fs::read_to_string(path.join("tracked.txt")).unwrap(),
            "base\n"
        );
    }

    #[test]
    fn stash_push_with_untracked_removes_new_file() {
        let temp = repo();
        let path = temp.path();
        std::fs::write(path.join("new.txt"), "new\n").unwrap();

        let sha = stash_push(path, "workmux: test [2]", true, false).unwrap();

        assert!(sha.is_some());
        assert!(!path.join("new.txt").exists());
        assert_eq!(git(path, &["status", "--porcelain"]), "");
    }

    #[test]
    fn stash_push_on_clean_tree_returns_none() {
        let temp = repo();
        let path = temp.path();

        assert_eq!(
            stash_push(path, "workmux: test [3]", true, false).unwrap(),
            None
        );
        assert_eq!(git(path, &["stash", "list"]), "");
    }

    #[test]
    fn stash_lookup_ignores_foreign_entry_pushed_on_top() {
        let temp = repo();
        let path = temp.path();
        std::fs::write(path.join("tracked.txt"), "ours\n").unwrap();
        let ours = stash_push(path, "workmux: test [4]", false, false)
            .unwrap()
            .unwrap();

        std::fs::write(path.join("tracked.txt"), "foreign\n").unwrap();
        git(path, &["stash", "push", "-m", "someone else"]);
        assert_ne!(git(path, &["rev-parse", "stash@{0}"]), ours);

        assert_eq!(
            stash_commit_by_message(path, "workmux: test [4]").unwrap(),
            Some(ours)
        );
    }
    #[test]
    fn stash_apply_fails_on_existing_untracked_path_and_keeps_entry() {
        let temp = repo();
        let path = temp.path();
        std::fs::write(path.join("new.txt"), "stashed\n").unwrap();
        let sha = stash_push(path, "workmux: test [6]", true, false)
            .unwrap()
            .unwrap();
        std::fs::write(path.join("new.txt"), "in the way\n").unwrap();

        assert!(stash_apply(path, &sha).is_err());

        assert_eq!(git(path, &["rev-parse", "refs/stash"]), sha);
        assert_eq!(
            std::fs::read_to_string(path.join("new.txt")).unwrap(),
            "in the way\n"
        );
    }

    #[test]
    fn patch_rejects_a_staged_version_before_modifying_anything() {
        let temp = repo();
        let path = temp.path();
        let file = path.join("tracked.txt");
        std::fs::write(&file, "staged\n").unwrap();
        git(path, &["add", "tracked.txt"]);
        std::fs::write(&file, "unstaged\n").unwrap();
        assert!(stash_push(path, "test", false, true).is_err());
        assert_eq!(git(path, &["show", ":tracked.txt"]), "staged");
        assert_eq!(std::fs::read_to_string(file).unwrap(), "unstaged\n");
        assert_eq!(git(path, &["stash", "list"]), "");
    }

    #[test]
    fn conflicted_restore_can_recover_both_versions_in_a_clean_worktree() {
        let temp = repo();
        let path = temp.path();
        let file = path.join("tracked.txt");
        std::fs::write(&file, "staged\n").unwrap();
        git(path, &["add", "tracked.txt"]);
        std::fs::write(&file, "unstaged\n").unwrap();
        let sha = stash_push(path, "test", false, false).unwrap().unwrap();
        std::fs::write(&file, "concurrent edit\n").unwrap();
        assert!(stash_apply(path, &sha).is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "concurrent edit\n");
        let recovery = tempfile::tempdir().unwrap();
        git(
            path,
            &[
                "worktree",
                "add",
                "--detach",
                recovery.path().to_str().unwrap(),
                &format!("{sha}^1"),
            ],
        );
        stash_apply(recovery.path(), &sha).unwrap();
        assert_eq!(git(recovery.path(), &["show", ":tracked.txt"]), "staged");
        assert_eq!(
            std::fs::read_to_string(recovery.path().join("tracked.txt")).unwrap(),
            "unstaged\n"
        );
    }

    #[test]
    fn stash_apply_by_commit_restores_content() {
        let temp = repo();
        let path = temp.path();
        std::fs::write(path.join("tracked.txt"), "ours\n").unwrap();
        std::fs::write(path.join("new.txt"), "new\n").unwrap();
        let ours = stash_push(path, "workmux: test [7]", true, false)
            .unwrap()
            .unwrap();
        std::fs::write(path.join("tracked.txt"), "foreign\n").unwrap();
        git(path, &["stash", "push", "-m", "someone else"]);

        stash_apply(path, &ours).unwrap();

        assert_eq!(
            std::fs::read_to_string(path.join("tracked.txt")).unwrap(),
            "ours\n"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("new.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(git(path, &["rev-parse", "stash@{1}"]), ours);
    }
}
