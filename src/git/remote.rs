use anyhow::{Context, Result, anyhow};
use std::path::Path;
use tracing::info;

use crate::cmd::Cmd;

/// Return a list of configured git remotes
pub fn list_remotes() -> Result<Vec<String>> {
    list_remotes_in(None)
}

/// Return a list of configured git remotes in a specific workdir
pub fn list_remotes_in(workdir: Option<&Path>) -> Result<Vec<String>> {
    let cmd = Cmd::new("git").arg("remote");
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    let output = cmd
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
#[allow(dead_code)]
pub fn remote_exists(remote: &str) -> Result<bool> {
    remote_exists_in(remote, None)
}

/// Check if a remote exists in a specific workdir
pub fn remote_exists_in(remote: &str, workdir: Option<&Path>) -> Result<bool> {
    Ok(list_remotes_in(workdir)?
        .into_iter()
        .any(|name| name == remote))
}

/// Fetch updates from the given remote
pub fn fetch_remote(remote: &str) -> Result<()> {
    fetch_remote_in(remote, None)
}

/// Fetch updates from the given remote in a specific workdir
pub fn fetch_remote_in(remote: &str, workdir: Option<&Path>) -> Result<()> {
    let cmd = Cmd::new("git").args(&["fetch", remote]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    cmd.run()
        .with_context(|| format!("Failed to fetch from remote '{}'", remote))?;
    Ok(())
}

/// Fetch from remote with prune to update remote-tracking refs
pub fn fetch_prune() -> Result<()> {
    Cmd::new("git")
        .args(&["fetch", "--prune"])
        .run()
        .context("Failed to fetch with prune")?;
    Ok(())
}

/// Fetch a specific refspec from a remote.
/// Used for PR checkout to fetch refs/pull/N/head into a remote-tracking ref.
#[allow(dead_code)]
pub fn fetch_refspec(remote: &str, refspec: &str) -> Result<()> {
    fetch_refspec_in(remote, refspec, None)
}

/// Fetch a specific refspec from a remote in a specific workdir.
pub fn fetch_refspec_in(remote: &str, refspec: &str, workdir: Option<&Path>) -> Result<()> {
    let cmd = Cmd::new("git").args(&["fetch", remote, refspec]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    cmd.run().with_context(|| {
        format!(
            "Failed to fetch refspec '{}' from remote '{}'",
            refspec, remote
        )
    })?;
    Ok(())
}

/// Add a git remote if it doesn't exist
#[allow(dead_code)]
pub fn add_remote(name: &str, url: &str) -> Result<()> {
    add_remote_in(name, url, None)
}

/// Add a git remote if it doesn't exist in a specific workdir
pub fn add_remote_in(name: &str, url: &str, workdir: Option<&Path>) -> Result<()> {
    let cmd = Cmd::new("git").args(&["remote", "add", name, url]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    cmd.run()
        .with_context(|| format!("Failed to add remote '{}' with URL '{}'", name, url))?;
    Ok(())
}

/// Set the URL for an existing git remote
#[allow(dead_code)]
pub fn set_remote_url(name: &str, url: &str) -> Result<()> {
    set_remote_url_in(name, url, None)
}

/// Set the URL for an existing git remote in a specific workdir
pub fn set_remote_url_in(name: &str, url: &str, workdir: Option<&Path>) -> Result<()> {
    let cmd = Cmd::new("git").args(&["remote", "set-url", name, url]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    cmd.run()
        .with_context(|| format!("Failed to set URL for remote '{}' to '{}'", name, url))?;
    Ok(())
}

/// Get the remote URL for a given remote name
/// Note: Returns the configured URL, not the resolved URL after insteadOf substitution
#[allow(dead_code)]
pub fn get_remote_url(remote: &str) -> Result<String> {
    get_remote_url_in(remote, None)
}

/// Get the remote URL for a given remote name in a specific workdir
pub fn get_remote_url_in(remote: &str, workdir: Option<&Path>) -> Result<String> {
    let config_key = format!("remote.{}.url", remote);
    let cmd = Cmd::new("git").args(&["config", "--get", &config_key]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    cmd.run_and_capture_stdout()
        .with_context(|| format!("Failed to get URL for remote '{}'", remote))
}

/// Find an existing remote that matches the given repository identity.
/// Returns the remote name if found, None otherwise.
#[allow(dead_code)]
fn find_remote_for_repo(target: &RepoIdentity) -> Result<Option<String>> {
    find_remote_for_repo_in(target, None)
}

fn find_remote_for_repo_in(
    target: &RepoIdentity,
    workdir: Option<&Path>,
) -> Result<Option<String>> {
    for remote_name in list_remotes_in(workdir)? {
        let url = match get_remote_url_in(&remote_name, workdir) {
            Ok(u) => u,
            Err(_) => continue,
        };
        if let Some(id) = parse_repo_identity_from_git_url(&url)
            && id.host.eq_ignore_ascii_case(&target.host)
            && id.owner.eq_ignore_ascii_case(&target.owner)
            && id.repo.eq_ignore_ascii_case(&target.repo)
        {
            return Ok(Some(remote_name));
        }
    }
    Ok(None)
}

/// Ensure a remote exists for a specific fork owner.
/// Returns the name of the remote (e.g., "origin" or "fork-username").
/// If the remote needs to be created, it constructs the URL based on the origin URL's scheme.
pub fn ensure_fork_remote(fork_owner: &str) -> Result<String> {
    ensure_fork_remote_in(fork_owner, None)
}

/// Ensure a remote exists for a specific fork owner in a specific workdir.
pub fn ensure_fork_remote_in(fork_owner: &str, workdir: Option<&Path>) -> Result<String> {
    // Parse origin URL to get base repo identity
    let origin_url = get_remote_url_in("origin", workdir)?;
    let origin_parsed = parse_git_remote_url(&origin_url).with_context(|| {
        format!(
            "Failed to parse origin URL for fork remote construction: {}",
            origin_url
        )
    })?;

    let origin_host = origin_parsed.host.to_string();
    let scheme = origin_parsed.scheme;
    let repo_name = origin_parsed.repo;

    // If the fork owner is the same as the origin owner, just use origin
    let current_owner = origin_parsed.owner;
    if fork_owner.eq_ignore_ascii_case(current_owner) {
        return Ok("origin".to_string());
    }

    let target_identity = RepoIdentity {
        host: origin_host.clone(),
        owner: fork_owner.to_string(),
        repo: repo_name.to_string(),
    };

    // Strict matching: look for any remote pointing to the exact same repo
    if let Some(existing) = find_remote_for_repo_in(&target_identity, workdir)? {
        info!(remote = %existing, "git:reusing existing remote for fork");
        return Ok(existing);
    }

    let remote_name = format!("fork-{}", fork_owner);

    let fork_url = match scheme {
        "https" => format!("https://{}/{}/{}.git", origin_host, fork_owner, repo_name),
        "http" => format!("http://{}/{}/{}.git", origin_host, fork_owner, repo_name),
        _ => {
            // SSH or other schemes
            format!("git@{}:{}/{}.git", origin_host, fork_owner, repo_name)
        }
    };

    // Check if remote exists and update URL if needed
    if remote_exists_in(&remote_name, workdir)? {
        let current_url = get_remote_url_in(&remote_name, workdir)?;
        if current_url != fork_url {
            info!(remote = %remote_name, url = %fork_url, "git:updating fork remote URL");
            set_remote_url_in(&remote_name, &fork_url, workdir)
                .with_context(|| format!("Failed to update remote for fork '{}'", fork_owner))?;
        }
    } else {
        info!(remote = %remote_name, url = %fork_url, "git:adding fork remote");
        add_remote_in(&remote_name, &fork_url, workdir)
            .with_context(|| format!("Failed to add remote for fork '{}'", fork_owner))?;
    }

    Ok(remote_name)
}

/// Parsed repository identity from a git remote URL.
#[derive(Debug, PartialEq, Eq)]
pub struct RepoIdentity {
    pub host: String,
    pub owner: String,
    pub repo: String,
}

struct ParsedGitRemote<'a> {
    scheme: &'a str,
    host: &'a str,
    owner: &'a str,
    repo: &'a str,
}

fn parse_git_remote_url(url: &str) -> Option<ParsedGitRemote<'_>> {
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        let scheme = if url.starts_with("https://") {
            "https"
        } else {
            "http"
        };
        return parse_remote_path(rest, scheme, '/');
    }

    if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        return parse_owner_repo_path(path).map(|(owner, repo)| ParsedGitRemote {
            scheme: "ssh",
            host,
            owner,
            repo,
        });
    }

    if let Some(rest) = url.strip_prefix("ssh://") {
        let host_and_path = rest.split_once('@').map(|(_, value)| value).unwrap_or(rest);
        return parse_remote_path(host_and_path, "ssh", '/');
    }

    None
}

fn parse_remote_path<'a>(
    value: &'a str,
    scheme: &'a str,
    separator: char,
) -> Option<ParsedGitRemote<'a>> {
    let (host, path) = value.split_once(separator)?;
    parse_owner_repo_path(path).map(|(owner, repo)| ParsedGitRemote {
        scheme,
        host,
        owner,
        repo,
    })
}

fn parse_owner_repo_path(path: &str) -> Option<(&str, &str)> {
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo_part = parts.next()?;
    let repo = repo_part.strip_suffix(".git").unwrap_or(repo_part);
    (!owner.is_empty() && !repo.is_empty()).then_some((owner, repo))
}

/// Parse full repository identity (host, owner, repo) from a git remote URL.
/// Supports both HTTPS and SSH formats.
fn parse_repo_identity_from_git_url(url: &str) -> Option<RepoIdentity> {
    let parsed = parse_git_remote_url(url)?;
    Some(RepoIdentity {
        host: parsed.host.to_string(),
        owner: parsed.owner.to_string(),
        repo: parsed.repo.to_string(),
    })
}

/// Parse the repository owner from a git remote URL
/// Supports both HTTPS and SSH formats for github.com and GitHub Enterprise domains
fn parse_owner_from_git_url(url: &str) -> Option<&str> {
    parse_git_remote_url(url).map(|parsed| parsed.owner)
}

/// Get the repository owner from the origin remote URL
pub fn get_repo_owner() -> Result<String> {
    get_repo_owner_in(None)
}

/// Get the repository owner from the origin remote URL in a specific workdir
pub fn get_repo_owner_in(workdir: Option<&Path>) -> Result<String> {
    let url = get_remote_url_in("origin", workdir)?;

    parse_owner_from_git_url(&url)
        .ok_or_else(|| anyhow!("Could not parse repository owner from origin URL: {}", url))
        .map(|s| s.to_string())
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
}
