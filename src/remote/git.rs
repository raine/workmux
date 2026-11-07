//! Moving a workspace's git state between a node and the laptop.
//!
//! A [`Snapshot`] names the state of a worktree with three objects: the HEAD
//! commit, the tree of its index and the tree of its files (tracked and
//! untracked, ignored ones excepted). The node stores the two trees as
//! commits whose parent is HEAD under `refs/workmux/export/<ws-id>/`; the
//! laptop fetches them and rebuilds the same state in a local worktree, so
//! staged changes stay staged and unstaged or untracked ones stay that way.
//!
//! The agent can write the node repository, so git runs hardened there:
//! hooks, fsmonitor and ambient `GIT_*` overrides are off.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// Identity the throwaway snapshot commits are written with.
const SNAPSHOT_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "workmux"),
    ("GIT_AUTHOR_EMAIL", "workmux@localhost"),
    ("GIT_COMMITTER_NAME", "workmux"),
    ("GIT_COMMITTER_EMAIL", "workmux@localhost"),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// HEAD commit.
    pub head: String,
    /// Tree of the index.
    pub index: String,
    /// Tree of the files in the worktree, ignored ones excepted.
    pub worktree: String,
}

/// The paths a snapshot changes, by where the change lives.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    /// Paths whose index entry differs from HEAD.
    pub staged: Vec<String>,
    /// Tracked paths whose file differs from the index.
    pub unstaged: Vec<String>,
    /// Files the index does not track.
    pub untracked: Vec<String>,
}

fn git(dir: &Path) -> Result<Command> {
    crate::git::pinned_git(dir)
}

fn output(mut command: Command, args: &[&str]) -> Result<String> {
    let output = command
        .args(args)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `git <args>` in `dir`, returning its trimmed stdout.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    output(git(dir)?, args)
}

/// Why `worktree` cannot be snapshotted on `branch`, if it cannot: an
/// operation in progress or unresolved conflicts would not survive the
/// trip, and HEAD must be the workspace's branch.
pub fn unexportable(worktree: &Path, branch: &str) -> Result<Option<String>> {
    let git_dir = PathBuf::from(run(worktree, &["rev-parse", "--absolute-git-dir"])?);
    for (marker, what) in [
        ("rebase-merge", "a rebase"),
        ("rebase-apply", "a rebase"),
        ("MERGE_HEAD", "a merge"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("REVERT_HEAD", "a revert"),
    ] {
        if git_dir.join(marker).exists() {
            return Ok(Some(format!("{what} is in progress")));
        }
    }
    let head = git(worktree)?
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .context("Failed to run git symbolic-ref")?;
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    if head != format!("refs/heads/{branch}") {
        return Ok(Some(format!("HEAD is not on branch '{branch}'")));
    }
    if !run(worktree, &["ls-files", "-u"])?.is_empty() {
        return Ok(Some("the index has unresolved conflicts".to_string()));
    }
    Ok(None)
}

/// An untracked git repository inside `worktree`, if any: a snapshot holds
/// it as a bare gitlink, without its files or history.
pub fn nested_repository(worktree: &Path) -> Result<Option<String>> {
    let others = run(
        worktree,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?;
    Ok(others
        .split('\0')
        .find(|path| path.ends_with('/'))
        .map(str::to_string))
}

/// The state of `worktree`. Writes tree objects into its repository but
/// leaves the index entries and files alone.
pub fn snapshot(worktree: &Path) -> Result<Snapshot> {
    let head = run(worktree, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let index = run(worktree, &["write-tree"])?;

    // The worktree tree comes from a scratch copy of the index, so `add`
    // reuses its stat data and the real index stays untouched.
    let index_path = PathBuf::from(run(worktree, &["rev-parse", "--git-path", "index"])?);
    let index_path = if index_path.is_absolute() {
        index_path
    } else {
        worktree.join(index_path)
    };
    let scratch = tempfile::Builder::new()
        .prefix("workmux-snapshot-index")
        .tempfile_in(index_path.parent().unwrap_or(worktree))
        .context("Failed to create a scratch index")?;
    std::fs::copy(&index_path, scratch.path())
        .with_context(|| format!("Failed to copy {}", index_path.display()))?;
    let with_scratch = || -> Result<Command> {
        let mut command = git(worktree)?;
        command.env("GIT_INDEX_FILE", scratch.path());
        Ok(command)
    };
    output(with_scratch()?, &["add", "-A", "--", "."])?;
    let files = output(with_scratch()?, &["write-tree"])?;
    Ok(Snapshot {
        head,
        index,
        worktree: files,
    })
}

/// A commit of `tree` whose parent is `parent`, for moving a tree between
/// repositories.
pub fn commit_tree(dir: &Path, tree: &str, parent: &str) -> Result<String> {
    let mut command = git(dir)?;
    command.envs(SNAPSHOT_IDENTITY);
    output(
        command,
        &["commit-tree", tree, "-p", parent, "-m", "workmux snapshot"],
    )
}

/// What `snapshot` changes relative to its HEAD.
pub fn changes(dir: &Path, snapshot: &Snapshot) -> Result<Changes> {
    let mut changes = Changes {
        staged: diff_names(dir, &snapshot.head, &snapshot.index)?
            .into_iter()
            .map(|(_, path)| path)
            .collect(),
        ..Default::default()
    };
    for (status, path) in diff_names(dir, &snapshot.index, &snapshot.worktree)? {
        if status == 'A' {
            changes.untracked.push(path);
        } else {
            changes.unstaged.push(path);
        }
    }
    Ok(changes)
}

/// `(status letter, path)` of every path that differs between two trees.
fn diff_names(dir: &Path, from: &str, to: &str) -> Result<Vec<(char, String)>> {
    let output = git(dir)?
        .args([
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--name-status",
            from,
            to,
        ])
        .output()
        .context("Failed to run git diff-tree")?;
    if !output.status.success() {
        bail!(
            "git diff-tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut fields = output.stdout.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut names = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        let status = status.first().copied().map(char::from).unwrap_or('M');
        names.push((status, String::from_utf8_lossy(path).into_owned()));
    }
    Ok(names)
}

/// The tree of commit `rev`.
pub fn tree_of(dir: &Path, rev: &str) -> Result<String> {
    run(dir, &["rev-parse", "--verify", &format!("{rev}^{{tree}}")])
}

/// The first parent of commit `rev`.
pub fn parent_of(dir: &Path, rev: &str) -> Result<String> {
    run(
        dir,
        &["rev-parse", "--verify", &format!("{rev}^1^{{commit}}")],
    )
    .map_err(|err| anyhow!("{rev} has no parent: {err:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let status = Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(status.success(), "{script}");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        sh(
            dir.path(),
            "git init -q -b main . && echo a > a && echo b > b && echo ignored > .gitignore \
             && git add . && git commit -qm init",
        );
        dir
    }

    #[test]
    fn snapshot_separates_staged_unstaged_and_untracked() {
        let dir = repo();
        let path = dir.path();
        sh(
            path,
            "echo staged > a && git add a && echo unstaged > b && echo new > c \
             && echo skip > ignored",
        );
        let status = || run(path, &["status", "--porcelain"]).unwrap();
        let before = status();
        let snapshot = snapshot(path).unwrap();
        assert_eq!(status(), before);
        assert_eq!(snapshot.head, run(path, &["rev-parse", "HEAD"]).unwrap());

        let changes = changes(path, &snapshot).unwrap();
        assert_eq!(changes.staged, vec!["a"]);
        assert_eq!(changes.unstaged, vec!["b"]);
        assert_eq!(changes.untracked, vec!["c"]);

        assert_eq!(snapshot, super::snapshot(path).unwrap());
        sh(path, "echo more > c");
        assert_ne!(snapshot.worktree, super::snapshot(path).unwrap().worktree);
    }

    #[test]
    fn unexportable_states() {
        let dir = repo();
        let path = dir.path();
        assert_eq!(unexportable(path, "main").unwrap(), None);
        assert!(
            unexportable(path, "other")
                .unwrap()
                .unwrap()
                .contains("not on branch")
        );
        sh(
            path,
            "git checkout -qb side && echo x > a && git commit -qam x && git checkout -q main \
             && echo y > a && git commit -qam y && (git merge -q side || true)",
        );
        assert!(
            unexportable(path, "main")
                .unwrap()
                .unwrap()
                .contains("merge")
        );
    }

    #[test]
    fn finds_nested_repositories() {
        let dir = repo();
        let path = dir.path();
        sh(
            path,
            "echo new > c && mkdir -p vendor/x && echo y > vendor/y",
        );
        assert_eq!(nested_repository(path).unwrap(), None);
        sh(path, "git init -q vendor/x");
        assert_eq!(
            nested_repository(path).unwrap().as_deref(),
            Some("vendor/x/")
        );
    }

    #[test]
    fn commit_tree_wraps_a_snapshot_tree() {
        let dir = repo();
        let path = dir.path();
        sh(path, "echo new > c");
        let snapshot = snapshot(path).unwrap();
        let commit = commit_tree(path, &snapshot.worktree, &snapshot.head).unwrap();
        assert_eq!(tree_of(path, &commit).unwrap(), snapshot.worktree);
        assert_eq!(parent_of(path, &commit).unwrap(), snapshot.head);
    }
}
