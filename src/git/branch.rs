use anyhow::{Context, Result, anyhow};
use std::collections::HashSet;
use std::path::Path;
use tracing::debug;

use crate::cmd::Cmd;

use super::repo::has_commits;
use super::{ForkBranchSpec, RemoteBranchSpec};

/// Get the default branch (main or master)
pub fn get_default_branch() -> Result<String> {
    get_default_branch_in(None)
}

/// Get the default branch for a repository at a specific path
pub fn get_default_branch_in(workdir: Option<&Path>) -> Result<String> {
    // Try to get the default branch from the remote
    let cmd = Cmd::new("git").args(&["symbolic-ref", "refs/remotes/origin/HEAD"]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    if let Ok(ref_name) = cmd.run_and_capture_stdout()
        && let Some(branch) = ref_name.strip_prefix("refs/remotes/origin/")
    {
        debug!(branch = branch, "git:default branch from remote HEAD");
        return Ok(branch.to_string());
    }

    // Fallback: check if main or master exists locally
    if branch_exists_in("main", workdir)? {
        debug!("git:default branch 'main' (local fallback)");
        return Ok("main".to_string());
    }

    if branch_exists_in("master", workdir)? {
        debug!("git:default branch 'master' (local fallback)");
        return Ok("master".to_string());
    }

    // Check if repo has any commits at all
    if !has_commits()? {
        return Err(anyhow!(
            "The repository has no commits yet. Please make an initial commit before using workmux, \
            or specify the main branch in .workmux.yaml using the 'main_branch' key."
        ));
    }

    // No default branch could be determined - require explicit configuration
    Err(anyhow!(
        "Could not determine the default branch (e.g., 'main' or 'master'). \
        Please specify it in .workmux.yaml using the 'main_branch' key."
    ))
}

/// Check if a branch exists (can be local or remote tracking branch)
pub fn branch_exists(branch_name: &str) -> Result<bool> {
    branch_exists_in(branch_name, None)
}

/// Check if a branch exists in a specific workdir
pub fn branch_exists_in(branch_name: &str, workdir: Option<&Path>) -> Result<bool> {
    let cmd = Cmd::new("git").args(&["rev-parse", "--verify", "--quiet", branch_name]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    cmd.run_as_check()
}

/// Parse a remote branch specification in the form "<remote>/<branch>"
pub fn parse_remote_branch_spec(spec: &str) -> Result<RemoteBranchSpec> {
    let mut parts = spec.splitn(2, '/');
    let remote = parts.next().unwrap_or("");
    let branch = parts.next().unwrap_or("");

    if remote.is_empty() || branch.is_empty() {
        return Err(anyhow!(
            "Invalid remote branch '{}'. Use the format <remote>/<branch> (e.g., origin/feature/foo).",
            spec
        ));
    }

    Ok(RemoteBranchSpec {
        remote: remote.to_string(),
        branch: branch.to_string(),
    })
}

/// Parse a fork branch specification in the form "owner:branch" (GitHub fork format).
/// Returns None if the input doesn't match this format.
pub fn parse_fork_branch_spec(input: &str) -> Option<ForkBranchSpec> {
    // Skip URLs (contain "://" or start with "git@")
    if input.contains("://") || input.starts_with("git@") {
        return None;
    }

    // Split on first colon only
    let (owner, branch) = input.split_once(':')?;

    // Validate both parts are non-empty
    if owner.is_empty() || branch.is_empty() {
        return None;
    }

    Some(ForkBranchSpec {
        owner: owner.to_string(),
        branch: branch.to_string(),
    })
}

/// Get the current branch name
pub fn get_current_branch() -> Result<String> {
    Cmd::new("git")
        .args(&["branch", "--show-current"])
        .run_and_capture_stdout()
}

/// Get the current branch in a specific workdir
pub fn get_current_branch_in(workdir: &Path) -> Result<String> {
    Cmd::new("git")
        .workdir(workdir)
        .args(&["branch", "--show-current"])
        .run_and_capture_stdout()
}

/// List all checkout-able branches (local and remote) for shell completion.
/// Excludes branches that are already checked out in existing worktrees.
pub fn list_checkout_branches() -> Result<Vec<String>> {
    let output = Cmd::new("git")
        .args(&[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/",
            "refs/remotes/",
        ])
        .run_and_capture_stdout()
        .context("Failed to list git branches")?;

    // Get branches currently checked out in worktrees to exclude them
    let worktree_branches: HashSet<String> = super::list_worktrees()
        .unwrap_or_default()
        .into_iter()
        .map(|(_, branch)| branch)
        .collect();

    Ok(output
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "HEAD" && !s.ends_with("/HEAD"))
        .filter(|s| !worktree_branches.contains(*s))
        .map(String::from)
        .collect())
}

/// List all local branches in a specific workdir, without excluding checked-out ones.
/// Suitable for base branch selection where any local branch is a valid target.
pub fn list_local_branches_in(workdir: Option<&Path>) -> Result<Vec<String>> {
    let cmd = Cmd::new("git").args(&[
        "for-each-ref",
        "--format=%(refname:short)",
        "--sort=refname",
        "refs/heads/",
    ]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    let output = cmd
        .run_and_capture_stdout()
        .context("Failed to list local branches")?;

    Ok(output
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

/// Rename a local branch using `git branch -m`.
///
/// Git automatically migrates `branch.<old>.*` config (including
/// `branch.<old>.workmux-base`) to `branch.<new>.*`, so we don't need to
/// touch branch-base metadata manually.
pub fn rename_branch(old: &str, new: &str) -> Result<()> {
    Cmd::new("git")
        .args(&["branch", "-m", old, new])
        .run()
        .with_context(|| format!("Failed to rename branch {} -> {}", old, new))?;
    Ok(())
}

/// Delete a local branch.
pub fn delete_branch_in(branch_name: &str, force: bool, git_common_dir: &Path) -> Result<()> {
    let mut cmd = Cmd::new("git").workdir(git_common_dir).arg("branch");

    if force {
        cmd = cmd.arg("-D");
    } else {
        cmd = cmd.arg("-d");
    }

    cmd.arg(branch_name)
        .run()
        .context("Failed to delete branch")?;
    Ok(())
}

/// Get the base branch for merge checks, preferring local branch over remote
pub fn get_merge_base(main_branch: &str) -> Result<String> {
    get_merge_base_in(None, main_branch)
}

/// Get the base branch for merge checks in a specific workdir
pub fn get_merge_base_in(workdir: Option<&Path>, main_branch: &str) -> Result<String> {
    // Check if the local branch exists first.
    // This ensures we compare against the local state (which might be ahead of remote)
    // avoiding false positives when local main has merged changes but hasn't been pushed.
    if branch_exists_in(main_branch, workdir)? {
        return Ok(main_branch.to_string());
    }

    // Fallback: check if origin/<main_branch> exists
    let remote_main = format!("origin/{}", main_branch);
    if branch_exists_in(&remote_main, workdir)? {
        Ok(remote_main)
    } else {
        Ok(main_branch.to_string())
    }
}

/// Get a set of all branches not merged into the base branch
pub fn get_unmerged_branches(base_branch: &str) -> Result<HashSet<String>> {
    get_unmerged_branches_in(None, base_branch)
}

/// Get a set of all branches not merged into the base branch in a specific workdir
pub fn get_unmerged_branches_in(
    workdir: Option<&Path>,
    base_branch: &str,
) -> Result<HashSet<String>> {
    // Special handling for potential errors since base branch might not exist
    let no_merged_arg = format!("--no-merged={}", base_branch);
    let cmd = Cmd::new("git").args(&[
        "for-each-ref",
        "--format=%(refname:short)",
        &no_merged_arg,
        "refs/heads/",
    ]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    let result = cmd.run_and_capture_stdout();

    match result {
        Ok(stdout) => {
            let branches: HashSet<String> = stdout.lines().map(String::from).collect();
            Ok(branches)
        }
        Err(e) => {
            // Non-fatal error if base branch doesn't exist; return empty set.
            let err_msg = e.to_string();
            if err_msg.contains("malformed object name") || err_msg.contains("unknown commit") {
                Ok(HashSet::new())
            } else {
                Err(e)
            }
        }
    }
}

/// Get the branch name for a worktree at a specific path.
///
/// Runs `git branch --show-current` in the worktree's directory.
pub fn get_branch_for_worktree(worktree_path: &Path) -> Result<String> {
    Cmd::new("git")
        .workdir(worktree_path)
        .args(&["branch", "--show-current"])
        .run_and_capture_stdout()
}

/// Get a set of branches whose upstream remote-tracking branch has been deleted.
pub fn get_gone_branches() -> Result<HashSet<String>> {
    let output = Cmd::new("git")
        .args(&[
            "for-each-ref",
            "--format=%(refname:short)|%(upstream:track)",
            "refs/heads",
        ])
        .run_and_capture_stdout()?;

    let mut gone = HashSet::new();
    for line in output.lines() {
        if let Some((branch, track)) = line.split_once('|')
            && track.trim() == "[gone]"
        {
            gone.insert(branch.to_string());
        }
    }
    Ok(gone)
}

/// Resolve a ref to its commit SHA.
pub fn rev_parse(rev: &str) -> Result<String> {
    Cmd::new("git")
        .args(&["rev-parse", "--verify", rev])
        .run_and_capture_stdout()
        .with_context(|| format!("Failed to resolve ref '{}'", rev))
}

/// Configure a branch to track a ref on a remote given by URL (no named remote).
///
/// Mirrors what `gh pr checkout` does so that `git pull`/`git push` on the
/// branch talk directly to the PR's head repository.
pub fn set_branch_tracking_url(branch: &str, remote_url: &str, merge_ref: &str) -> Result<()> {
    for (key, val) in [
        (format!("branch.{}.remote", branch), remote_url),
        (format!("branch.{}.pushRemote", branch), remote_url),
        (format!("branch.{}.merge", branch), merge_ref),
    ] {
        Cmd::new("git")
            .args(&["config", "--local", &key, val])
            .run()
            .with_context(|| format!("Failed to set {}", key))?;
    }
    Ok(())
}

/// Unset the upstream tracking for a branch
pub fn unset_branch_upstream(branch_name: &str) -> Result<()> {
    if !branch_has_upstream(branch_name)? {
        return Ok(());
    }

    Cmd::new("git")
        .args(&["branch", "--unset-upstream", branch_name])
        .run()
        .context("Failed to unset branch upstream")?;
    Ok(())
}

pub(super) fn branch_has_upstream(branch_name: &str) -> Result<bool> {
    // Check for the existence of tracking config for this branch.
    // We check both 'merge' and 'remote' to catch edge cases where one might be set without the other.
    // This confirms if tracking configuration exists (which is what we want to unset),
    // rather than checking if it resolves to a valid commit (which rev-parse does).
    let has_merge = Cmd::new("git")
        .args(&["config", "--get", &format!("branch.{}.merge", branch_name)])
        .run_as_check()?;

    if has_merge {
        return Ok(true);
    }

    Cmd::new("git")
        .args(&["config", "--get", &format!("branch.{}.remote", branch_name)])
        .run_as_check()
}

/// Store the base branch/commit that a branch was created from
pub fn set_branch_base(branch: &str, base: &str) -> Result<()> {
    set_branch_base_in(branch, base, None)
}

/// Store the base branch/commit in a specific workdir
pub fn set_branch_base_in(branch: &str, base: &str, workdir: Option<&Path>) -> Result<()> {
    let config_key = format!("branch.{}.workmux-base", branch);
    let cmd = Cmd::new("git").args(&["config", "--local", &config_key, base]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    cmd.run().context("Failed to set workmux-base config")?;
    Ok(())
}

/// Retrieve the base branch/commit that a branch was created from
pub fn get_branch_base(branch: &str) -> Result<String> {
    get_branch_base_in(branch, None)
}

/// Get the base branch for a given branch in a specific workdir
pub fn get_branch_base_in(branch: &str, workdir: Option<&Path>) -> Result<String> {
    let config_key = format!("branch.{}.workmux-base", branch);
    let cmd = Cmd::new("git").args(&["config", "--local", &config_key]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    let output = cmd
        .run_and_capture_stdout()
        .context("Failed to get workmux-base config")?;

    if output.is_empty() {
        return Err(anyhow!("No workmux-base found for branch '{}'", branch));
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_fork_branch_spec_valid() {
        let spec = parse_fork_branch_spec("someuser:feature-branch").unwrap();
        assert_eq!(spec.owner, "someuser");
        assert_eq!(spec.branch, "feature-branch");
    }

    #[test]
    fn test_parse_fork_branch_spec_with_slashes() {
        let spec = parse_fork_branch_spec("user:feature/some-feature").unwrap();
        assert_eq!(spec.owner, "user");
        assert_eq!(spec.branch, "feature/some-feature");
    }

    #[test]
    fn test_parse_fork_branch_spec_empty_owner() {
        assert!(parse_fork_branch_spec(":branch").is_none());
    }

    #[test]
    fn test_parse_fork_branch_spec_empty_branch() {
        assert!(parse_fork_branch_spec("owner:").is_none());
    }

    #[test]
    fn test_parse_fork_branch_spec_no_colon() {
        assert!(parse_fork_branch_spec("just-a-branch").is_none());
    }

    #[test]
    fn test_parse_fork_branch_spec_url_https() {
        assert!(parse_fork_branch_spec("https://github.com/owner/repo").is_none());
    }

    #[test]
    fn test_parse_fork_branch_spec_url_ssh() {
        assert!(parse_fork_branch_spec("git@github.com:owner/repo").is_none());
    }

    #[test]
    fn test_parse_fork_branch_spec_remote_branch_format() {
        assert!(parse_fork_branch_spec("origin/feature").is_none());
    }
}
