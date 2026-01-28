use anyhow::{Context, Result, anyhow};
use git_url_parse::GitUrl;
use git_url_parse::types::provider::GenericProvider;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
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
#[error("Worktree not found: {0}")]
pub struct WorktreeNotFound(pub String);

/// Git status information for a worktree
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitStatus {
    /// Commits ahead of upstream
    pub ahead: usize,
    /// Commits behind upstream
    pub behind: usize,
    /// Branch has conflicts when merging with base
    pub has_conflict: bool,
    /// Has uncommitted changes (staged or unstaged)
    pub is_dirty: bool,
    /// Lines added in committed changes only (base...HEAD)
    pub lines_added: usize,
    /// Lines removed in committed changes only (base...HEAD)
    pub lines_removed: usize,
    /// Lines added in uncommitted changes only (working tree + untracked)
    #[serde(default)]
    pub uncommitted_added: usize,
    /// Lines removed in uncommitted changes only (working tree)
    #[serde(default)]
    pub uncommitted_removed: usize,
    /// Timestamp when this status was cached (UNIX seconds)
    #[serde(default)]
    pub cached_at: Option<u64>,
    /// The base branch used for comparison (e.g., "main")
    #[serde(default)]
    pub base_branch: String,
}

/// Get the path to the git status cache file
pub fn get_cache_path() -> Result<PathBuf> {
    let home = home::home_dir().ok_or_else(|| anyhow!("Could not find home directory"))?;
    let cache_dir = home.join(".cache").join("workmux");
    std::fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir.join("git_status_cache.json"))
}

/// Load the git status cache from disk
pub fn load_status_cache() -> HashMap<PathBuf, GitStatus> {
    if let Ok(path) = get_cache_path()
        && path.exists()
        && let Ok(content) = std::fs::read_to_string(&path)
    {
        return serde_json::from_str(&content).unwrap_or_default();
    }
    HashMap::new()
}

/// Save the git status cache to disk
pub fn save_status_cache(statuses: &HashMap<PathBuf, GitStatus>) {
    if let Ok(path) = get_cache_path()
        && let Ok(content) = serde_json::to_string(statuses)
    {
        let _ = std::fs::write(path, content);
    }
}

/// Check if we're in a git repository
pub fn is_git_repo() -> Result<bool> {
    Cmd::new("git")
        .args(&["rev-parse", "--git-dir"])
        .run_as_check()
}

/// Check if the repository has any commits (HEAD is valid)
pub fn has_commits() -> Result<bool> {
    Cmd::new("git")
        .args(&["rev-parse", "--verify", "--quiet", "HEAD"])
        .run_as_check()
}

/// Get the root directory of the git repository
pub fn get_repo_root() -> Result<PathBuf> {
    let path = Cmd::new("git")
        .args(&["rev-parse", "--show-toplevel"])
        .run_and_capture_stdout()?;
    Ok(PathBuf::from(path))
}

/// Get the root directory of the git repository containing the given path.
/// Uses `git -C <dir>` to run git from the target directory.
pub fn get_repo_root_for(dir: &Path) -> Result<PathBuf> {
    let output = std::process::Command::new("git")
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
pub fn get_git_common_dir() -> Result<PathBuf> {
    let raw = Cmd::new("git")
        .args(&["rev-parse", "--git-common-dir"])
        .run_and_capture_stdout()
        .context("Failed to get git common directory")?;

    if raw.is_empty() {
        return Err(anyhow!(
            "git rev-parse --git-common-dir returned empty output"
        ));
    }

    let path = PathBuf::from(raw);

    // Normalize to absolute path since git may return relative paths like ".git"
    let abs_path = if path.is_relative() {
        std::env::current_dir()
            .context("Failed to get current directory")?
            .join(path)
    } else {
        path
    };

    Ok(abs_path)
}

/// Get the main worktree root directory (not a linked worktree)
///
/// For bare repositories with linked worktrees, this returns the bare repo path.
/// For regular repositories, this returns the first worktree that exists on disk.
pub fn get_main_worktree_root() -> Result<PathBuf> {
    let list_str = Cmd::new("git")
        .args(&["worktree", "list", "--porcelain"])
        .run_and_capture_stdout()
        .context("Failed to list worktrees while locating main worktree")?;

    // Check if this is a bare repo setup.
    // The first entry in `git worktree list` is always the main worktree or bare repo.
    // For bare repos, it looks like:
    //   worktree /path/to/.bare
    //   bare
    if let Some(first_block) = list_str.trim().split("\n\n").next() {
        let mut path: Option<PathBuf> = None;
        let mut is_bare = false;

        for line in first_block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if line.trim() == "bare" {
                is_bare = true;
            }
        }

        // If this is a bare repo, return its path immediately.
        // Git commands like `git worktree prune` work correctly from bare repo directories.
        if is_bare && let Some(p) = path {
            return Ok(p);
        }
    }

    // Not a bare repo - find the first worktree that exists on disk.
    // This handles edge cases where a worktree was deleted but not yet pruned.
    let worktrees = parse_worktree_list_porcelain(&list_str)?;

    for (path, _) in &worktrees {
        if path.exists() {
            return Ok(path.clone());
        }
    }

    // Fallback: return the first worktree even if it doesn't exist
    if let Some((path, _)) = worktrees.first() {
        Ok(path.clone())
    } else {
        Err(anyhow!("No main worktree found"))
    }
}

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

/// Prune stale worktree metadata.
pub fn prune_worktrees_in(git_common_dir: &Path) -> Result<()> {
    Cmd::new("git")
        .workdir(git_common_dir)
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

/// Find a worktree by handle (directory name) or branch name.
/// Tries handle first, then falls back to branch lookup.
/// Returns both the path and the branch name checked out in that worktree.
pub fn find_worktree(name: &str) -> Result<(PathBuf, String)> {
    let list_str = Cmd::new("git")
        .args(&["worktree", "list", "--porcelain"])
        .run_and_capture_stdout()
        .context("Failed to list worktrees")?;

    let worktrees = parse_worktree_list_porcelain(&list_str)?;

    // First: try to match by handle (directory name)
    for (path, branch) in &worktrees {
        if let Some(dir_name) = path.file_name()
            && dir_name.to_string_lossy() == name
        {
            return Ok((path.clone(), branch.clone()));
        }
    }

    // Fallback: try to match by branch name
    for (path, branch) in worktrees {
        if branch == name {
            return Ok((path, branch));
        }
    }

    Err(WorktreeNotFound(name.to_string()).into())
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

/// Get the base branch for merge checks, preferring local branch over remote
pub fn get_merge_base(main_branch: &str) -> Result<String> {
    // Check if the local branch exists first.
    // This ensures we compare against the local state (which might be ahead of remote)
    // avoiding false positives when local main has merged changes but hasn't been pushed.
    if branch_exists(main_branch)? {
        return Ok(main_branch.to_string());
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

/// Fetch from remote with prune to update remote-tracking refs
pub fn fetch_prune() -> Result<()> {
    Cmd::new("git")
        .args(&["fetch", "--prune"])
        .run()
        .context("Failed to fetch with prune")?;
    Ok(())
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
    get_branch_base_in(branch, None)
}

/// Store per-worktree metadata using git config.
/// Uses the worktree handle (directory name) as the key prefix.
pub fn set_worktree_meta(handle: &str, key: &str, value: &str) -> Result<()> {
    Cmd::new("git")
        .args(&[
            "config",
            "--local",
            &format!("workmux.worktree.{}.{}", handle, key),
            value,
        ])
        .run()
        .with_context(|| format!("Failed to set worktree metadata {}.{}", handle, key))?;
    Ok(())
}

/// Retrieve per-worktree metadata from git config.
/// Returns None if the key doesn't exist.
pub fn get_worktree_meta(handle: &str, key: &str) -> Option<String> {
    Cmd::new("git")
        .args(&[
            "config",
            "--local",
            "--get",
            &format!("workmux.worktree.{}.{}", handle, key),
        ])
        .run_and_capture_stdout()
        .ok()
        .filter(|s| !s.is_empty())
}

/// Determine the tmux target mode for a worktree from git metadata.
/// Falls back to Window mode if no metadata is found (backward compatibility).
pub fn get_worktree_target(handle: &str) -> crate::config::TmuxTarget {
    match get_worktree_meta(handle, "target") {
        Some(target) if target == "session" => crate::config::TmuxTarget::Session,
        _ => crate::config::TmuxTarget::Window,
    }
}

/// Remove per-worktree metadata from git config.
/// Silently succeeds if the key doesn't exist.
#[allow(dead_code)] // Will be used when cleanup/commands are updated
pub fn unset_worktree_meta(handle: &str, key: &str) -> Result<()> {
    // --unset returns exit code 5 if key doesn't exist, which we ignore
    let _ = Cmd::new("git")
        .args(&[
            "config",
            "--local",
            "--unset",
            &format!("workmux.worktree.{}.{}", handle, key),
        ])
        .run();
    Ok(())
}

/// Remove all metadata for a worktree handle.
pub fn remove_worktree_meta(handle: &str) -> Result<()> {
    // Use --remove-section to remove all keys under the handle's section
    let _ = Cmd::new("git")
        .args(&[
            "config",
            "--local",
            "--remove-section",
            &format!("workmux.worktree.{}", handle),
        ])
        .run();
    Ok(())
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

/// Parse git status porcelain v2 output to extract branch info and dirty state.
/// Returns (branch_name, ahead, behind, is_dirty).
fn parse_porcelain_v2_status(output: &str) -> (Option<String>, usize, usize, bool) {
    let mut branch_name: Option<String> = None;
    let mut ahead: usize = 0;
    let mut behind: usize = 0;
    let mut is_dirty = false;

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            // "(detached)" indicates detached HEAD state
            if rest != "(detached)" {
                branch_name = Some(rest.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            // Format: "+<ahead> -<behind>"
            // Use iterator directly to avoid Vec allocation
            let mut parts = rest.split_whitespace();
            if let (Some(part_a), Some(part_b)) = (parts.next(), parts.next()) {
                if let Some(a) = part_a.strip_prefix('+') {
                    ahead = a.parse().unwrap_or(0);
                }
                if let Some(b) = part_b.strip_prefix('-') {
                    behind = b.parse().unwrap_or(0);
                }
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            // Any non-header, non-empty line indicates dirty state
            // This includes: '1' (ordinary), '2' (rename/copy), 'u' (unmerged), '?' (untracked)
            is_dirty = true;
            // Headers are always printed first in porcelain v2.
            // Once we find a file entry, we know the repo is dirty and can stop.
            break;
        }
    }

    (branch_name, ahead, behind, is_dirty)
}

/// Count lines in a file, treating it like git (text files only).
/// Returns 0 for binary files or errors.
fn count_lines(path: &Path) -> std::io::Result<usize> {
    use std::fs::File;

    let mut file = File::open(path)?;

    // Check for binary content (heuristic: null byte in first 8KB)
    let mut buffer = [0; 8192];
    let n = file.read(&mut buffer)?;
    if buffer[..n].contains(&0) {
        return Ok(0);
    }

    // Reset file position to start
    file.seek(SeekFrom::Start(0))?;

    let mut count = 0;
    let mut buf = [0; 32 * 1024];
    let mut last_byte = None;

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count += buf[..n].iter().filter(|&&b| b == b'\n').count();
        last_byte = Some(buf[n - 1]);
    }

    // If file ends with non-newline character, count that as a line (like git)
    if let Some(b) = last_byte
        && b != b'\n'
    {
        count += 1;
    }

    Ok(count)
}

/// Diff statistics returned by get_diff_stats.
///
/// Separates committed and uncommitted changes:
/// - Committed: changes in base...HEAD (what's been committed on the branch)
/// - Uncommitted: working tree changes + untracked files
struct DiffStats {
    /// Lines added in committed changes only (base...HEAD)
    committed_added: usize,
    /// Lines removed in committed changes only (base...HEAD)
    committed_removed: usize,
    /// Lines added in uncommitted changes (working tree + untracked)
    uncommitted_added: usize,
    /// Lines removed in uncommitted changes (working tree)
    uncommitted_removed: usize,
}

fn get_diff_stats(worktree_path: &Path, base_ref: &str) -> DiffStats {
    let mut committed_added = 0;
    let mut committed_removed = 0;
    let mut uncommitted_added = 0;
    let mut uncommitted_removed = 0;

    // Helper to parse numstat output
    let parse_numstat = |output: &str| -> (usize, usize) {
        let mut a = 0;
        let mut r = 0;
        for line in output.lines() {
            let mut parts = line.split_whitespace();
            // Format: <added> <removed> <filename>
            // Binary files use "-" instead of numbers (parse will fail, which is fine)
            if let (Some(added), Some(removed)) = (parts.next(), parts.next()) {
                a += added.parse::<usize>().unwrap_or(0);
                r += removed.parse::<usize>().unwrap_or(0);
            }
        }
        (a, r)
    };

    // 1. Committed changes (base...HEAD)
    if let Ok(output) = Cmd::new("git")
        .workdir(worktree_path)
        .args(&["diff", "--numstat", &format!("{}...HEAD", base_ref)])
        .run_and_capture_stdout()
    {
        let (a, r) = parse_numstat(&output);
        committed_added += a;
        committed_removed += r;
    }

    // 2. Uncommitted changes (HEAD vs working tree)
    // This covers both staged and unstaged changes to tracked files
    if let Ok(output) = Cmd::new("git")
        .workdir(worktree_path)
        .args(&["diff", "--numstat", "HEAD"])
        .run_and_capture_stdout()
    {
        let (a, r) = parse_numstat(&output);
        uncommitted_added += a;
        uncommitted_removed += r;
    }

    // 3. Untracked files (all lines count as added to uncommitted)
    // Use -z to separate paths with null bytes, handling spaces/special chars correctly
    if let Ok(output) = Cmd::new("git")
        .workdir(worktree_path)
        .args(&["ls-files", "--others", "--exclude-standard", "-z"])
        .run_and_capture_stdout()
    {
        for file_path in output.split('\0') {
            if file_path.is_empty() {
                continue;
            }

            let full_path = worktree_path.join(file_path);

            // Check for symlinks - treat as 1 line (the path) like git does
            if let Ok(metadata) = std::fs::symlink_metadata(&full_path)
                && metadata.file_type().is_symlink()
            {
                uncommitted_added += 1;
                continue;
            }

            if let Ok(lines) = count_lines(&full_path) {
                uncommitted_added += lines;
            }
        }
    }

    DiffStats {
        committed_added,
        committed_removed,
        uncommitted_added,
        uncommitted_removed,
    }
}

/// Get git status for a worktree (ahead/behind, conflicts, dirty state, diff stats).
/// This is designed for dashboard display and prioritizes speed over completeness.
/// Uses `git status --porcelain=v2 --branch` to get most info in a single command.
pub fn get_git_status(worktree_path: &Path) -> GitStatus {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .ok();

    // Get branch info, ahead/behind, and dirty state in one command
    let (branch, ahead, behind, is_dirty) = match Cmd::new("git")
        .workdir(worktree_path)
        .args(&["status", "--porcelain=v2", "--branch"])
        .run_and_capture_stdout()
    {
        Ok(output) => parse_porcelain_v2_status(&output),
        Err(_) => {
            return GitStatus {
                cached_at: now,
                ..Default::default()
            };
        }
    };

    // If no branch (detached HEAD or error), return early with dirty state
    let branch = match branch {
        Some(b) => b,
        None => {
            return GitStatus {
                is_dirty,
                cached_at: now,
                ..Default::default()
            };
        }
    };

    // Determine base branch for conflict check and diff stats
    // First try workmux-base config, then fall back to default branch
    let base_branch = get_branch_base_in(&branch, Some(worktree_path))
        .ok()
        .or_else(|| get_default_branch_in(Some(worktree_path)).ok())
        .unwrap_or_else(|| "main".to_string());

    // On the base branch: no branch-level diff, but still show uncommitted changes
    if branch == base_branch {
        let stats = get_diff_stats(worktree_path, &branch);

        return GitStatus {
            ahead,
            behind,
            is_dirty,
            uncommitted_added: stats.uncommitted_added,
            uncommitted_removed: stats.uncommitted_removed,
            cached_at: now,
            base_branch,
            ..Default::default()
        };
    }

    // Use local base branch for comparisons (clone since we need it in the return)
    let base_ref = base_branch.clone();

    // Check for merge conflicts with base branch
    // git merge-tree --write-tree returns exit code 1 on conflict (Git 2.38+)
    // Exit code 129 means unknown option (older Git) - treat as no conflict
    let has_conflict = {
        let status = Command::new("git")
            .current_dir(worktree_path)
            .args(["merge-tree", "--write-tree", &base_ref, "HEAD"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        matches!(status, Ok(s) if s.code() == Some(1))
    };

    // Get diff stats (lines added/removed vs base)
    let diff_stats = get_diff_stats(worktree_path, &base_ref);

    GitStatus {
        ahead,
        behind,
        has_conflict,
        is_dirty,
        lines_added: diff_stats.committed_added,
        lines_removed: diff_stats.committed_removed,
        uncommitted_added: diff_stats.uncommitted_added,
        uncommitted_removed: diff_stats.uncommitted_removed,
        cached_at: now,
        base_branch,
    }
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

    use super::parse_porcelain_v2_status;

    #[test]
    fn test_parse_porcelain_v2_clean_repo() {
        let output = "# branch.oid abc123def456\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -0\n";
        let (branch, ahead, behind, is_dirty) = parse_porcelain_v2_status(output);
        assert_eq!(branch, Some("main".to_string()));
        assert_eq!(ahead, 0);
        assert_eq!(behind, 0);
        assert!(!is_dirty);
    }

    #[test]
    fn test_parse_porcelain_v2_dirty_repo() {
        let output = "# branch.oid abc123\n# branch.head feature\n# branch.upstream origin/feature\n# branch.ab +1 -2\n1 .M N... 100644 100644 100644 abc123 def456 src/file.rs\n? untracked.txt\n";
        let (branch, ahead, behind, is_dirty) = parse_porcelain_v2_status(output);
        assert_eq!(branch, Some("feature".to_string()));
        assert_eq!(ahead, 1);
        assert_eq!(behind, 2);
        assert!(is_dirty);
    }

    #[test]
    fn test_parse_porcelain_v2_no_upstream() {
        // When there's no upstream, branch.ab line is missing
        let output = "# branch.oid abc123\n# branch.head new-branch\n";
        let (branch, ahead, behind, is_dirty) = parse_porcelain_v2_status(output);
        assert_eq!(branch, Some("new-branch".to_string()));
        assert_eq!(ahead, 0);
        assert_eq!(behind, 0);
        assert!(!is_dirty);
    }

    #[test]
    fn test_parse_porcelain_v2_detached_head() {
        let output = "# branch.oid abc123\n# branch.head (detached)\n";
        let (branch, ahead, behind, is_dirty) = parse_porcelain_v2_status(output);
        assert_eq!(branch, None);
        assert_eq!(ahead, 0);
        assert_eq!(behind, 0);
        assert!(!is_dirty);
    }

    #[test]
    fn test_parse_porcelain_v2_untracked_only() {
        let output = "# branch.oid abc123\n# branch.head main\n? untracked.txt\n";
        let (branch, _ahead, _behind, is_dirty) = parse_porcelain_v2_status(output);
        assert_eq!(branch, Some("main".to_string()));
        assert!(is_dirty);
    }

    #[test]
    fn test_parse_porcelain_v2_renamed_file() {
        let output = "# branch.oid abc123\n# branch.head main\n2 R. N... 100644 100644 100644 abc123 def456 R100 old.rs\tnew.rs\n";
        let (branch, _ahead, _behind, is_dirty) = parse_porcelain_v2_status(output);
        assert_eq!(branch, Some("main".to_string()));
        assert!(is_dirty);
    }

    #[test]
    fn test_parse_porcelain_v2_initial_commit() {
        // Repo created but no commits made yet
        let output = "# branch.oid (initial)\n# branch.head master\n";
        let (branch, ahead, behind, is_dirty) = parse_porcelain_v2_status(output);
        assert_eq!(branch, Some("master".to_string()));
        assert_eq!(ahead, 0);
        assert_eq!(behind, 0);
        assert!(!is_dirty);
    }

    #[test]
    fn test_parse_porcelain_v2_unmerged_conflict() {
        // Merge conflict (unmerged entry starting with 'u')
        let output = "# branch.oid abc123\n# branch.head feature\n# branch.upstream origin/feature\n# branch.ab +0 -0\nu UU N... 100644 100644 100644 100644 abc def ghi jkl src/conflict.rs\n";
        let (branch, _ahead, _behind, is_dirty) = parse_porcelain_v2_status(output);
        assert_eq!(branch, Some("feature".to_string()));
        assert!(is_dirty);
    }
}
