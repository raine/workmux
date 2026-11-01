use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;

use crate::cmd::Cmd;

/// Check if a path is ignored by git (via .gitignore, global gitignore, etc.)
pub fn is_path_ignored(repo_path: &Path, file_path: &str) -> bool {
    super::unattended_git(Some(repo_path))
        .and_then(|mut command| {
            command
                .args(["check-ignore", "-q", file_path])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            command.status().map_err(anyhow::Error::from)
        })
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Classify paths with the same index-aware rules and false-on-error behavior
/// as `is_path_ignored`, without starting Git separately for every path.
pub fn ignored_paths(repo_path: &Path, paths: &[String]) -> std::collections::HashSet<String> {
    let individually = || {
        paths
            .iter()
            .filter(|path| is_path_ignored(repo_path, path))
            .cloned()
            .collect()
    };
    // These inputs are errors/options in the single-path invocation, not
    // literal stdin records. Preserve its classification for them.
    if paths.len() <= 1
        || paths
            .iter()
            .any(|path| path.starts_with('-') || path.contains('\0'))
    {
        return individually();
    }
    let batch = || -> Result<std::collections::HashSet<String>> {
        use std::io::{Seek, Write};
        // File-backed stdin avoids pipe backpressure for large selections.
        let mut input = tempfile::tempfile()?;
        {
            let mut writer = std::io::BufWriter::new(&mut input);
            for path in paths {
                writer.write_all(path.as_bytes())?;
                writer.write_all(&[0])?;
            }
            writer.flush()?;
        }
        input.rewind()?;
        let output = super::unattended_git(Some(repo_path))?
            .args(["check-ignore", "-z", "--stdin"])
            .stdin(input)
            .stderr(std::process::Stdio::null())
            .output()?;
        anyhow::ensure!(
            matches!(output.status.code(), Some(0 | 1)),
            "git check-ignore failed"
        );
        Ok(output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect())
    };
    // One invalid path (e.g. beyond a symlink) must not discard valid results
    // for the other paths when Git aborts the batch.
    batch().unwrap_or_else(|_| individually())
}

/// Check if we're in a git repository
pub fn is_git_repo() -> Result<bool> {
    is_git_repo_in(None)
}

/// Check if a specific path is in a git repository
pub fn is_git_repo_in(workdir: Option<&Path>) -> Result<bool> {
    let cmd = Cmd::new("git").args(&["rev-parse", "--git-dir"]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    cmd.run_as_check()
}

pub fn get_repo_root_if_present() -> Result<Option<PathBuf>> {
    get_repo_root_if_present_in(None)
}

pub fn get_repo_root_if_present_in(workdir: Option<&Path>) -> Result<Option<PathBuf>> {
    repository_path_if_present(workdir, "--git-dir")
}

/// Resolve a worktree root, distinguishing absent repositories from failed probes.
pub(crate) fn worktree_root_if_present(path: &Path) -> Result<Option<PathBuf>> {
    repository_path_if_present(Some(path), "--show-toplevel")
}

fn repository_path_if_present(workdir: Option<&Path>, argument: &str) -> Result<Option<PathBuf>> {
    let cwd = match workdir {
        Some(path) => path
            .canonicalize()
            .context("Failed to resolve repository path")?,
        None => std::env::current_dir().context("Failed to resolve current directory")?,
    };
    let output = super::unattended_git(Some(&cwd))?
        .args(["rev-parse", argument])
        .output()
        .context("Failed to execute git rev-parse")?;

    if output.status.success() {
        let git_dir = PathBuf::from(String::from_utf8(output.stdout)?.trim());
        return Ok(Some(if git_dir.is_absolute() {
            git_dir
        } else {
            cwd.join(git_dir)
        }));
    }

    let mut current = Some(cwd.as_path());
    while let Some(path) = current {
        match std::fs::symlink_metadata(path.join(".git")) {
            Ok(_) => {
                return Err(anyhow!(
                    "Git metadata exists but repository resolution failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Failed to inspect Git metadata"),
        }
        current = path.parent();
    }

    Ok(None)
}

/// Check if the repository has any commits (HEAD is valid)
#[allow(dead_code)]
pub fn has_commits() -> Result<bool> {
    has_commits_in(None)
}

/// Check if the repository at a specific path has any commits
pub fn has_commits_in(workdir: Option<&Path>) -> Result<bool> {
    let cmd = Cmd::new("git").args(&["rev-parse", "--verify", "--quiet", "HEAD"]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    cmd.run_as_check()
}

/// Get the root directory of the git repository
pub fn get_repo_root() -> Result<PathBuf> {
    get_repo_root_in(None)
}

/// Get the root directory of a git repository in a specific workdir
pub fn get_repo_root_in(workdir: Option<&Path>) -> Result<PathBuf> {
    let cmd = Cmd::new("git").args(&["rev-parse", "--show-toplevel"]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    let path = cmd.run_and_capture_stdout()?;
    Ok(PathBuf::from(path))
}

/// Get the root directory of the git repository containing the given path.
/// Uses `git -C <dir>` to run git from the target directory.
pub fn get_repo_root_for(dir: &Path) -> Result<PathBuf> {
    let mut command = super::unattended_git(Some(dir))?;
    let output = command
        .args(["-C", &dir.to_string_lossy(), "rev-parse", "--show-toplevel"])
        .output()
        .context("Failed to run git rev-parse")?;

    if !output.status.success() {
        anyhow::bail!("Not a git repository: {}", dir.display());
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(path))
}

/// Get the common git directory (shared across all worktrees).
///
/// This returns the absolute path where git stores shared data like refs, objects, and config.
/// - For regular repos: Returns the `.git` directory
/// - For bare repos: Returns the bare repo path (e.g., `.bare`)
///
/// Git commands like `git worktree prune` and `git branch -D` work correctly
/// when run from this directory, even for bare repo setups.
#[allow(dead_code)]
pub fn get_git_common_dir() -> Result<PathBuf> {
    get_git_common_dir_in(None)
}

/// Get the common git directory for a repository at a specific path.
pub fn get_git_common_dir_in(workdir: Option<&Path>) -> Result<PathBuf> {
    let cmd = Cmd::new("git").args(&["rev-parse", "--git-common-dir"]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    let raw = cmd
        .run_and_capture_stdout()
        .context("Failed to get git common directory")?;

    if raw.is_empty() {
        return Err(anyhow!(
            "git rev-parse --git-common-dir returned empty output"
        ));
    }

    let path = PathBuf::from(raw);

    let abs_path = if path.is_relative() {
        let base = match workdir {
            Some(path) => path.to_path_buf(),
            None => std::env::current_dir().context("Failed to get current directory")?,
        };
        base.join(path)
    } else {
        path
    };

    Ok(abs_path)
}

/// Stable repository identity shared by all linked worktrees and status caches.
pub fn get_repository_key_in(workdir: Option<&Path>) -> Result<PathBuf> {
    get_git_common_dir_in(workdir).map(|path| crate::util::canon_or_self(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_ignore_matches_individual_rules_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        crate::test_support::init_repo(repo);
        std::fs::write(repo.join(".gitignore"), "*.secret\n!keep.secret\nodd*\n").unwrap();
        std::fs::write(repo.join(".git/info/exclude"), "local-*\n").unwrap();
        let global = repo.join("global-ignore");
        std::fs::write(&global, "global-*\n").unwrap();
        crate::test_support::run_git(
            repo,
            &["config", "core.excludesFile", global.to_str().unwrap()],
        );
        let paths: Vec<String> = [
            "ignored.secret",
            "keep.secret",
            "tracked.secret",
            "local-only",
            "global-only",
            "odd space",
            "odd\nline",
            "odd\ttab",
            "odd\"quote",
            "plain",
            "missing.secret",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        for prefix in ["", "app"] {
            let source = repo.join(prefix);
            std::fs::create_dir_all(&source).unwrap();
            for name in &paths {
                if name != "missing.secret" {
                    std::fs::write(source.join(name), "data").unwrap();
                }
            }
            crate::test_support::run_git(
                repo,
                &[
                    "add",
                    "-f",
                    format!("{prefix}/tracked.secret").trim_start_matches('/'),
                ],
            );
            let expected: std::collections::HashSet<_> = paths
                .iter()
                .filter(|path| is_path_ignored(&source, path))
                .cloned()
                .collect();
            assert_eq!(ignored_paths(&source, &paths), expected);
            assert_eq!(expected.len(), 8);
            for not_ignored in ["keep.secret", "tracked.secret", "plain"] {
                assert!(!expected.contains(not_ignored));
            }
        }
        assert!(ignored_paths(repo, &[]).is_empty());
        assert!(ignored_paths(repo, &["plain".into()]).is_empty());
    }

    #[test]
    fn batch_ignore_preserves_results_when_a_path_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        crate::test_support::init_repo(repo);
        std::fs::write(repo.join(".gitignore"), "*.secret\n").unwrap();
        std::os::unix::fs::symlink("missing", repo.join("link")).unwrap();
        for invalid in [
            "link/child.secret",
            "../outside.secret",
            "-option.secret",
            "bad\0path",
        ] {
            let paths: Vec<String> =
                vec!["first.secret".into(), invalid.into(), "last.secret".into()];
            let expected: std::collections::HashSet<_> = paths
                .iter()
                .filter(|path| is_path_ignored(repo, path))
                .cloned()
                .collect();
            assert_eq!(ignored_paths(repo, &paths), expected, "{invalid:?}");
            assert!(expected.contains("first.secret"));
            assert!(expected.contains("last.secret"));
        }
        std::fs::write(repo.join(".git/config"), "[invalid").unwrap();
        assert!(ignored_paths(repo, &["first.secret".into(), "last.secret".into()]).is_empty());
    }

    #[test]
    fn repository_key_is_shared_by_primary_linked_and_nested_paths() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("project");
        std::fs::create_dir(&repo).unwrap();
        crate::test_support::init_repo(&repo);
        let linked = temp.path().join("feature");
        crate::test_support::run_git(
            &repo,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );
        let nested = linked.join("src");
        std::fs::create_dir(&nested).unwrap();
        let expected = repo.join(".git").canonicalize().unwrap();
        for path in [&repo, &linked, &nested] {
            assert_eq!(get_repository_key_in(Some(path)).unwrap(), expected);
        }
    }

    #[test]
    fn repository_probe_confirms_non_repository() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(get_repo_root_if_present_in(Some(dir.path())).unwrap(), None);
    }

    #[test]
    fn repository_probe_accepts_bare_repository() {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "--bare", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        assert!(
            get_repo_root_if_present_in(Some(dir.path()))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn repository_probe_rejects_broken_metadata() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: /missing").unwrap();

        assert!(get_repo_root_if_present_in(Some(dir.path())).is_err());
    }
}
