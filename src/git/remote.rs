use anyhow::{Context, Result};
use git_url_parse::GitUrl;
use git_url_parse::types::provider::GenericProvider;
use tracing::info;

use crate::cmd::Cmd;

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

/// Fetch a specific refspec from a remote name or URL.
pub fn fetch_refspec(source: &str, refspec: &str) -> Result<()> {
    Cmd::new("git")
        .args(&["fetch", source, refspec])
        .run()
        .with_context(|| format!("Failed to fetch '{}' from '{}'", refspec, source))?;
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

/// Find an existing remote whose URL points at `owner` (and `repo_name` if given).
///
/// This lets us reuse remotes the user already configured (e.g. `upstream`
/// pointing at the parent repo) instead of synthesizing a `fork-<owner>` remote
/// with a guessed URL.
pub fn find_remote_for(owner: &str, repo_name: Option<&str>) -> Result<Option<String>> {
    for remote in list_remotes()? {
        let Ok(url) = get_remote_url(&remote) else {
            continue;
        };
        let Ok(parsed) = GitUrl::parse(&url) else {
            continue;
        };
        let Ok(provider): std::result::Result<GenericProvider, _> = parsed.provider_info() else {
            continue;
        };
        if !provider.owner().eq_ignore_ascii_case(owner) {
            continue;
        }
        if let Some(repo) = repo_name
            && !provider.repo().eq_ignore_ascii_case(repo)
        {
            continue;
        }
        return Ok(Some(remote));
    }
    Ok(None)
}

/// Ensure a remote exists for a specific fork owner.
/// Returns the name of the remote (e.g., "origin", "upstream", or "fork-username").
///
/// `repo_name` overrides the repo name guessed from `origin`, which may differ
/// from the fork's actual name.
pub fn ensure_fork_remote(fork_owner: &str, repo_name: Option<&str>) -> Result<String> {
    if let Some(existing) = find_remote_for(fork_owner, repo_name)? {
        return Ok(existing);
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
    let repo_name = repo_name.unwrap_or_else(|| provider.repo());

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
