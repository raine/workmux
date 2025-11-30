use anyhow::{Context, Result, anyhow};
use git_url_parse::GitUrl;
use git_url_parse::types::provider::GenericProvider;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, info};

use crate::cmd::Cmd;

#[derive(Debug, Clone)]
pub struct RemoteBranchSpec {
    pub remote: String,
    pub branch: String,
}

#[derive(Debug, Clone)]
pub struct ForkBranchSpec {
    pub owner: String,
    pub branch: String,
}

/// Custom error type for worktree not found
#[derive(Debug, thiserror::Error)]
#[error("Worktree not found for branch: {0}")]
pub struct WorktreeNotFound(String);

/// Check if we're in a git repository
pub fn is_git_repo() -> Result<bool> {
    Cmd::new("git")
        .args(&["rev-parse", "--git-dir"])
        .run_as_check()
}

/// Get the root directory of the git repository
pub fn get_repo_root() -> Result<PathBuf> {
    let path = Cmd::new("git")
        .args(&["rev-parse", "--show-toplevel"])
        .run_and_capture_stdout()?;
    Ok(PathBuf::from(path))
}

/// Get the main worktree root directory (not a linked worktree)
pub fn get_main_worktree_root() -> Result<PathBuf> {
    // Get all worktrees
    let list_str = Cmd::new("git")
        .args(&["worktree", "list", "--porcelain"])
        .run_and_capture_stdout()
        .context("Failed to list worktrees while locating main worktree")?;

    let worktrees = parse_worktree_list_porcelain(&list_str)?;

    // The first worktree in the list is always the main worktree
    if let Some((path, _)) = worktrees.first() {
        Ok(path.clone())
    } else {
        Err(anyhow!("No main worktree found"))
    }
}

/// Get the default branch (main or master)
pub fn get_default_branch() -> Result<String> {
    // Try to get the default branch from the remote
    if let Ok(ref_name) = Cmd::new("git")
        .args(&["symbolic-ref", "refs/remotes/origin/HEAD"])
        .run_and_capture_stdout()
        && let Some(branch) = ref_name.strip_prefix("refs/remotes/origin/")
    {
        debug!(branch = branch, "git:default branch from remote HEAD");
        return Ok(branch.to_string());
    }

    // Fallback: check if main or master exists locally
    if branch_exists("main")? {
        debug!("git:default branch 'main' (local fallback)");
        return Ok("main".to_string());
    }

    if branch_exists("master")? {
        debug!("git:default branch 'master' (local fallback)");
        return Ok("master".to_string());
    }

    // No default branch could be determined - require explicit configuration
    Err(anyhow!(
        "Could not determine the default branch (e.g., 'main' or 'master'). \
        Please specify it in .workmux.yaml using the 'main_branch' key."
    ))
}

/// Check if a branch exists (can be local or remote tracking branch)
pub fn branch_exists(branch_name: &str) -> Result<bool> {
    Cmd::new("git")
        .args(&["rev-parse", "--verify", "--quiet", branch_name])
        .run_as_check()
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

/// Return a list of configured git remotes
pub fn list_remotes() -> Result<Vec<String>> {
    let output = Cmd::new("git")
        .arg("remote")
        .run_and_capture_stdout()
        .context("Failed to list git remotes")?;

    Ok(output
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect())
}

/// Check if a remote exists
pub fn remote_exists(remote: &str) -> Result<bool> {
    Ok(list_remotes()?.into_iter().any(|name| name == remote))
}

/// Fetch updates from the given remote
pub fn fetch_remote(remote: &str) -> Result<()> {
    Cmd::new("git")
        .args(&["fetch", remote])
        .run()
        .with_context(|| format!("Failed to fetch from remote '{}'", remote))?;
    Ok(())
}

/// Add a git remote if it doesn't exist
pub fn add_remote(name: &str, url: &str) -> Result<()> {
    Cmd::new("git")
        .args(&["remote", "add", name, url])
        .run()
        .with_context(|| format!("Failed to add remote '{}' with URL '{}'", name, url))?;
    Ok(())
}

/// Set the URL for an existing git remote
pub fn set_remote_url(name: &str, url: &str) -> Result<()> {
    Cmd::new("git")
        .args(&["remote", "set-url", name, url])
        .run()
        .with_context(|| format!("Failed to set URL for remote '{}' to '{}'", name, url))?;
    Ok(())
}

/// Get the remote URL for a given remote name
/// Note: Returns the configured URL, not the resolved URL after insteadOf substitution
pub fn get_remote_url(remote: &str) -> Result<String> {
    // Use git config to get the raw URL, not the insteadOf-resolved one
    // git remote get-url resolves insteadOf, which breaks our owner parsing in tests
    Cmd::new("git")
        .args(&["config", "--get", &format!("remote.{}.url", remote)])
        .run_and_capture_stdout()
        .with_context(|| format!("Failed to get URL for remote '{}'", remote))
}

/// Ensure a remote exists for a specific fork owner.
/// Returns the name of the remote (e.g., "origin" or "fork-username").
/// If the remote needs to be created, it constructs the URL based on the origin URL's scheme.
pub fn ensure_fork_remote(fork_owner: &str) -> Result<String> {
    // If the fork owner is the same as the origin owner, just use origin
    let current_owner = get_repo_owner().unwrap_or_default();
    if !current_owner.is_empty() && fork_owner == current_owner {
        return Ok("origin".to_string());
    }

    let remote_name = format!("fork-{}", fork_owner);

    // Construct fork URL based on origin URL format, preserving host and protocol
    let origin_url = get_remote_url("origin")?;
    let parsed_url = GitUrl::parse(&origin_url).with_context(|| {
        format!(
            "Failed to parse origin URL for fork remote construction: {}",
            origin_url
        )
    })?;

    let host = parsed_url.host().unwrap_or("github.com");
    let scheme = parsed_url.scheme().unwrap_or("ssh");

    let provider: GenericProvider = parsed_url
        .provider_info()
        .with_context(|| "Failed to extract provider info from origin URL")?;
    let repo_name = provider.repo();

    let fork_url = match scheme {
        "https" => format!("https://{}/{}/{}.git", host, fork_owner, repo_name),
        "http" => format!("http://{}/{}/{}.git", host, fork_owner, repo_name),
        _ => {
            // SSH or other schemes
            format!("git@{}:{}/{}.git", host, fork_owner, repo_name)
        }
    };

    // Check if remote exists and update URL if needed
    if remote_exists(&remote_name)? {
        let current_url = get_remote_url(&remote_name)?;
        if current_url != fork_url {
            info!(remote = %remote_name, url = %fork_url, "git:updating fork remote URL");
            set_remote_url(&remote_name, &fork_url)
                .with_context(|| format!("Failed to update remote for fork '{}'", fork_owner))?;
        }
    } else {
        info!(remote = %remote_name, url = %fork_url, "git:adding fork remote");
        add_remote(&remote_name, &fork_url)
            .with_context(|| format!("Failed to add remote for fork '{}'", fork_owner))?;
    }

    Ok(remote_name)
}

/// Parse the repository owner from a git remote URL
/// Supports both HTTPS and SSH formats for github.com and GitHub Enterprise domains
fn parse_owner_from_git_url(url: &str) -> Option<&str> {
    if let Some(https_part) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        // HTTPS format: https://github.com/owner/repo.git or https://github.enterprise.com/owner/repo.git
        https_part.split('/').nth(1)
    } else if url.starts_with("git@") {
        // SSH format: git@github.com:owner/repo.git or git@github.enterprise.com:owner/repo.git
        url.split(':')
            .nth(1)
            .and_then(|path| path.split('/').next())
    } else {
        None
    }
}

/// Get the repository owner from the origin remote URL
pub fn get_repo_owner() -> Result<String> {
    let url = get_remote_url("origin")?;

    parse_owner_from_git_url(&url)
        .ok_or_else(|| anyhow!("Could not parse repository owner from origin URL: {}", url))
        .map(|s| s.to_string())
}

/// Check if a worktree already exists for a branch
pub fn worktree_exists(branch_name: &str) -> Result<bool> {
    match get_worktree_path(branch_name) {
        Ok(_) => Ok(true),
        Err(e) => {
            // Check if this is a WorktreeNotFound error
            if e.is::<WorktreeNotFound>() {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// Create a new git worktree
pub fn create_worktree(
    worktree_path: &Path,
    branch_name: &str,
    create_branch: bool,
    base_branch: Option<&str>,
    track_upstream: bool,
) -> Result<()> {
    let path_str = worktree_path
        .to_str()
        .ok_or_else(|| anyhow!("Invalid worktree path"))?;

    let mut cmd = Cmd::new("git").arg("worktree").arg("add");

    if create_branch {
        cmd = cmd.arg("-b").arg(branch_name).arg(path_str);
        if let Some(base) = base_branch {
            cmd = cmd.arg(base);
        }
    } else {
        cmd = cmd.arg(path_str).arg(branch_name);
    }

    cmd.run().context("Failed to create worktree")?;

    // When creating a new branch from a remote tracking branch (e.g., origin/main),
    // git automatically sets up tracking for the new branch. This is desirable when
    // opening a remote branch locally, but we unset the upstream when the new branch
    // should be independent.
    if create_branch && !track_upstream {
        unset_branch_upstream(branch_name)?;
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

fn branch_has_upstream(branch_name: &str) -> Result<bool> {
    // Ask git to resolve <branch>@{upstream}; success means an upstream exists.
    let upstream_ref = format!("{branch_name}@{{upstream}}");
    Cmd::new("git")
        .args(&[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &upstream_ref,
        ])
        .run_as_check()
}

/// Prune stale worktree metadata
pub fn prune_worktrees() -> Result<()> {
    // Ensure this command always runs from a valid git directory.
    let main_worktree_root = get_main_worktree_root()?;
    Cmd::new("git")
        .workdir(&main_worktree_root)
        .args(&["worktree", "prune"])
        .run()
        .context("Failed to prune worktrees")?;
    Ok(())
}

/// Parse the output of `git worktree list --porcelain`
fn parse_worktree_list_porcelain(output: &str) -> Result<Vec<(PathBuf, String)>> {
    let mut worktrees = Vec::new();
    for block in output.trim().split("\n\n") {
        let mut path: Option<PathBuf> = None;
        let mut branch: Option<String> = None;

        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            } else if line.trim() == "detached" {
                branch = Some("(detached)".to_string());
            }
        }

        if let (Some(p), Some(b)) = (path, branch) {
            worktrees.push((p, b));
        }
    }
    Ok(worktrees)
}

/// Get the path to a worktree for a given branch
pub fn get_worktree_path(branch_name: &str) -> Result<PathBuf> {
    let list_str = Cmd::new("git")
        .args(&["worktree", "list", "--porcelain"])
        .run_and_capture_stdout()
        .context("Failed to list worktrees while locating worktree path")?;

    let worktrees = parse_worktree_list_porcelain(&list_str)?;

    for (path, branch) in worktrees {
        if branch == branch_name {
            return Ok(path);
        }
    }

    Err(WorktreeNotFound(branch_name.to_string()).into())
}

/// List all worktrees with their branches
pub fn list_worktrees() -> Result<Vec<(PathBuf, String)>> {
    let list = Cmd::new("git")
        .args(&["worktree", "list", "--porcelain"])
        .run_and_capture_stdout()
        .context("Failed to list worktrees")?;
    parse_worktree_list_porcelain(&list)
}

/// Check if the worktree has uncommitted changes
pub fn has_uncommitted_changes(worktree_path: &Path) -> Result<bool> {
    let output = Cmd::new("git")
        .workdir(worktree_path)
        .args(&["status", "--porcelain"])
        .run_and_capture_stdout()?;

    Ok(!output.is_empty())
}

/// Check if the worktree has tracked changes (staged or modified)
/// This excludes untracked files
pub fn has_tracked_changes(worktree_path: &Path) -> Result<bool> {
    let output = Cmd::new("git")
        .workdir(worktree_path)
        .args(&["status", "--porcelain"])
        .run_and_capture_stdout()?;

    // Filter out untracked files (lines starting with "??")
    for line in output.lines() {
        if !line.starts_with("??") && !line.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Check if the worktree has untracked files
pub fn has_untracked_files(worktree_path: &Path) -> Result<bool> {
    let output = Cmd::new("git")
        .workdir(worktree_path)
        .args(&["status", "--porcelain"])
        .run_and_capture_stdout()?;

    // Look for untracked files (lines starting with "??")
    for line in output.lines() {
        if line.starts_with("??") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Check if the worktree has staged changes
pub fn has_staged_changes(worktree_path: &Path) -> Result<bool> {
    // Exit code 0 = no changes, 1 = has changes
    // So we invert the result of run_as_check
    let no_changes = Cmd::new("git")
        .workdir(worktree_path)
        .args(&["diff", "--cached", "--quiet"])
        .run_as_check()?;
    Ok(!no_changes)
}

/// Check if the worktree has unstaged changes
pub fn has_unstaged_changes(worktree_path: &Path) -> Result<bool> {
    // Exit code 0 = no changes, 1 = has changes
    // So we invert the result of run_as_check
    let no_changes = Cmd::new("git")
        .workdir(worktree_path)
        .args(&["diff", "--quiet"])
        .run_as_check()?;
    Ok(!no_changes)
}

/// Commit staged changes in a worktree using the user's editor
pub fn commit_with_editor(worktree_path: &Path) -> Result<()> {
    let status = Command::new("git")
        .current_dir(worktree_path)
        .arg("commit")
        .status()
        .context("Failed to run git commit")?;

    if !status.success() {
        return Err(anyhow!("Commit was aborted or failed"));
    }

    Ok(())
}

/// Get the base branch for merge checks, preferring remote tracking branch
pub fn get_merge_base(main_branch: &str) -> Result<String> {
    // Try to get the configured upstream tracking branch
    let upstream_arg = format!("{}@{{upstream}}", main_branch);
    if let Ok(upstream) = Cmd::new("git")
        .args(&["rev-parse", "--abbrev-ref", &upstream_arg])
        .run_and_capture_stdout()
        && !upstream.is_empty()
    {
        return Ok(upstream);
    }

    // Fallback: check if origin/<main_branch> exists
    let remote_main = format!("origin/{}", main_branch);
    if branch_exists(&remote_main)? {
        Ok(remote_main)
    } else {
        Ok(main_branch.to_string())
    }
}

/// Get a set of all branches not merged into the base branch
pub fn get_unmerged_branches(base_branch: &str) -> Result<HashSet<String>> {
    // Special handling for potential errors since base branch might not exist
    let no_merged_arg = format!("--no-merged={}", base_branch);
    let result = Cmd::new("git")
        .args(&[
            "for-each-ref",
            "--format=%(refname:short)",
            &no_merged_arg,
            "refs/heads/",
        ])
        .run_and_capture_stdout();

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

/// Get the current branch name
pub fn get_current_branch() -> Result<String> {
    Cmd::new("git")
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
    let worktree_branches: HashSet<String> = list_worktrees()
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

/// Delete a local branch
pub fn delete_branch(branch_name: &str, force: bool) -> Result<()> {
    // Run from main worktree root to avoid issues when deleting from within a worktree
    // or after a worktree has been removed
    let main_worktree_root = get_main_worktree_root()?;

    let mut cmd = Cmd::new("git").workdir(&main_worktree_root).arg("branch");

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

/// Delete a remote branch
pub fn delete_remote_branch(branch_name: &str) -> Result<()> {
    Cmd::new("git")
        .args(&["push", "origin", "--delete", branch_name])
        .run()
        .with_context(|| format!("Failed to delete remote branch '{}'", branch_name))?;
    Ok(())
}

/// Stash uncommitted changes, optionally including untracked files or using patch mode.
pub fn stash_push(message: &str, include_untracked: bool, patch: bool) -> Result<()> {
    use std::process::Command;

    if patch {
        // For --patch mode, we need an interactive terminal
        let status = Command::new("git")
            .args(["stash", "push", "-m", message, "--patch"])
            .status()
            .context("Failed to run interactive git stash")?;

        if !status.success() {
            return Err(anyhow!(
                "Git stash --patch failed. Make sure you select at least one hunk."
            ));
        }
    } else {
        let mut cmd = Cmd::new("git").args(&["stash", "push", "-m", message]);

        if include_untracked {
            cmd = cmd.arg("--include-untracked");
        }

        cmd.run().context("Failed to stash changes")?;
    }
    Ok(())
}

/// Pop the latest stash in a specific worktree.
pub fn stash_pop(worktree_path: &Path) -> Result<()> {
    Cmd::new("git")
        .workdir(worktree_path)
        .args(&["stash", "pop"])
        .run()
        .context("Failed to apply stashed changes. Conflicts may have occurred.")?;
    Ok(())
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

/// Store the base branch/commit that a branch was created from
pub fn set_branch_base(branch: &str, base: &str) -> Result<()> {
    Cmd::new("git")
        .args(&[
            "config",
            "--local",
            &format!("branch.{}.workmux-base", branch),
            base,
        ])
        .run()
        .context("Failed to set workmux-base config")?;
    Ok(())
}

/// Retrieve the base branch/commit that a branch was created from
pub fn get_branch_base(branch: &str) -> Result<String> {
    let output = Cmd::new("git")
        .args(&[
            "config",
            "--local",
            &format!("branch.{}.workmux-base", branch),
        ])
        .run_and_capture_stdout()
        .context("Failed to get workmux-base config")?;

    if output.is_empty() {
        return Err(anyhow!("No workmux-base found for branch '{}'", branch));
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::parse_owner_from_git_url;

    #[test]
    fn test_parse_repo_owner_https_github_com() {
        assert_eq!(
            parse_owner_from_git_url("https://github.com/owner/repo.git"),
            Some("owner")
        );
    }

    #[test]
    fn test_parse_repo_owner_https_github_com_no_git_suffix() {
        assert_eq!(
            parse_owner_from_git_url("https://github.com/owner/repo"),
            Some("owner")
        );
    }

    #[test]
    fn test_parse_repo_owner_http_github_com() {
        assert_eq!(
            parse_owner_from_git_url("http://github.com/owner/repo.git"),
            Some("owner")
        );
    }

    #[test]
    fn test_parse_repo_owner_ssh_github_com() {
        assert_eq!(
            parse_owner_from_git_url("git@github.com:owner/repo.git"),
            Some("owner")
        );
    }

    #[test]
    fn test_parse_repo_owner_ssh_github_com_no_git_suffix() {
        assert_eq!(
            parse_owner_from_git_url("git@github.com:owner/repo"),
            Some("owner")
        );
    }

    #[test]
    fn test_parse_repo_owner_https_github_enterprise() {
        assert_eq!(
            parse_owner_from_git_url("https://github.enterprise.com/owner/repo.git"),
            Some("owner")
        );
    }

    #[test]
    fn test_parse_repo_owner_ssh_github_enterprise() {
        assert_eq!(
            parse_owner_from_git_url("git@github.enterprise.net:org/project.git"),
            Some("org")
        );
    }

    #[test]
    fn test_parse_repo_owner_https_github_enterprise_subdomain() {
        assert_eq!(
            parse_owner_from_git_url("https://github.company.internal/team/project.git"),
            Some("team")
        );
    }

    #[test]
    fn test_parse_repo_owner_with_nested_path() {
        assert_eq!(
            parse_owner_from_git_url("https://github.com/owner/repo/subpath"),
            Some("owner")
        );
    }

    #[test]
    fn test_parse_repo_owner_ssh_with_nested_path() {
        assert_eq!(
            parse_owner_from_git_url("git@github.com:owner/repo/subpath"),
            Some("owner")
        );
    }

    #[test]
    fn test_parse_repo_owner_invalid_format() {
        assert_eq!(parse_owner_from_git_url("not-a-valid-url"), None);
    }

    #[test]
    fn test_parse_repo_owner_local_path() {
        assert_eq!(parse_owner_from_git_url("/local/path/to/repo"), None);
    }

    #[test]
    fn test_parse_repo_owner_file_protocol() {
        assert_eq!(parse_owner_from_git_url("file:///local/path/to/repo"), None);
    }

    use super::parse_fork_branch_spec;

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
        // SSH URLs start with "git@" and should be rejected
        assert!(parse_fork_branch_spec("git@github.com:owner/repo").is_none());
    }

    #[test]
    fn test_parse_fork_branch_spec_remote_branch_format() {
        // origin/feature should NOT match (no colon)
        assert!(parse_fork_branch_spec("origin/feature").is_none());
    }
}
