//! GitLab merge request metadata and checkout preparation through glab.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::process::Command;

use crate::git;
use crate::workflow::pr::{CheckoutRef, Forge, PrCheckoutResult};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repository {
    pub host: String,
    path: String,
    scheme: String,
}

impl Repository {
    /// Parse web, scp-style SSH, and ssh:// Git remotes, preserving nested namespaces.
    pub fn parse(url: &str) -> Result<Self> {
        let (authority, path, scheme) = if let Some(rest) = url.strip_prefix("https://") {
            let (host, path) = rest.split_once('/').context("Missing project path")?;
            (host.rsplit('@').next().unwrap_or(host), path, "https")
        } else if let Some(rest) = url.strip_prefix("http://") {
            let (host, path) = rest.split_once('/').context("Missing project path")?;
            (host.rsplit('@').next().unwrap_or(host), path, "http")
        } else if let Some(rest) = url.strip_prefix("ssh://") {
            let (host, path) = rest.split_once('/').context("Missing project path")?;
            let host = host.rsplit('@').next().unwrap_or(host);
            // The SSH port is independent of the GitLab web/API port.
            (host.split(':').next().unwrap_or(host), path, "https")
        } else {
            let (user_host, path) = url
                .split_once(':')
                .context("Unsupported GitLab remote URL")?;
            let host = user_host.rsplit('@').next().unwrap_or(user_host);
            (host, path, "https")
        };
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        if authority.is_empty()
            || authority.starts_with('-')
            || !authority
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-:".contains(c))
            || !path.contains('/')
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || !path
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._/".contains(c))
        {
            bail!("Invalid GitLab repository URL");
        }
        Ok(Self {
            host: authority.to_ascii_lowercase(),
            path: path.to_string(),
            scheme: scheme.to_string(),
        })
    }

    #[cfg(test)]
    fn web_url(&self) -> String {
        format!("{}://{}/{}", self.scheme, self.host, self.path)
    }

    fn same_project(&self, other: &Self) -> bool {
        self.host == other.host && self.path == other.path
    }
}

#[derive(Deserialize)]
struct Author {
    username: String,
}

#[derive(Deserialize)]
struct MergeRequest {
    iid: u32,
    title: String,
    web_url: String,
    source_branch: String,
    #[serde(default)]
    target_branch: Option<String>,
    source_project_id: Option<u64>,
    target_project_id: u64,
    state: String,
    #[serde(default)]
    draft: bool,
    author: Author,
}

#[derive(Deserialize)]
struct Project {
    ssh_url_to_repo: String,
    http_url_to_repo: String,
}

/// A private origin-only repository lets glab resolve SSH aliases and configured
/// API hosts without selecting another remote or changing the user's repository.
struct GlabContext {
    directory: tempfile::TempDir,
}

impl GlabContext {
    fn new(origin_url: &str) -> Result<Self> {
        let directory = tempfile::tempdir().context("Cannot create GitLab lookup context")?;
        for args in [
            vec!["init", "-q"],
            vec!["config", "remote.origin.url", origin_url],
        ] {
            let output = git::unattended_git(None)?
                .current_dir(directory.path())
                .args(args)
                .output()?;
            if !output.status.success() {
                bail!("Cannot initialize GitLab lookup context");
            }
        }
        Ok(Self { directory })
    }

    fn json<T: serde::de::DeserializeOwned>(&self, args: &[&str]) -> Result<T> {
        let mut command = Command::new("glab");
        git::clear_ambient_git_env(&mut command);
        let output = command
            .current_dir(self.directory.path())
            .args(args)
            .env_remove("GITLAB_REPO")
            .env_remove("GITLAB_HOST")
            .env_remove("GITLAB_URI")
            .env_remove("GL_HOST")
            .env("GLAB_ENABLE_CI_AUTOLOGIN", "false")
            .env("NO_PROMPT", "1")
            .env("GLAB_NO_PROMPT", "1")
            .output()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    anyhow!("GitLab CLI (glab 1.37.0 or newer) is required for GitLab --pr checkout. Install and authenticate glab for your GitLab host")
                } else {
                    anyhow!(error).context("Failed to execute glab")
                }
            })?;
        if !output.status.success() {
            bail!(
                "glab {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        serde_json::from_slice(&output.stdout).context("Failed to parse glab JSON output")
    }
}

pub fn resolve(
    number: u32,
    url_repository: Option<&Repository>,
    custom_branch: Option<&str>,
    dry_run: bool,
) -> Result<PrCheckoutResult> {
    let origin_url = git::get_remote_url("origin")?;
    Repository::parse(&origin_url).context("Cannot resolve GitLab project from origin")?;
    let glab = GlabContext::new(&origin_url)?;
    let mr: MergeRequest = crate::spinner::with_spinner(&format!("Fetching MR !{number}"), || {
        glab.json(&["mr", "view", &number.to_string(), "--output", "json"])
    })?;
    let (web_repository, web_number) = mr
        .web_url
        .trim_end_matches('/')
        .rsplit_once("/-/merge_requests/")
        .context("glab returned an invalid merge request URL")?;
    let repository =
        Repository::parse(web_repository).context("glab returned an invalid project URL")?;
    if web_number.parse::<u32>().ok() != Some(number) {
        bail!("glab returned an unexpected merge request URL");
    }
    if let Some(url_repository) = url_repository
        && !repository.same_project(url_repository)
    {
        bail!("The merge request URL must match the origin repository resolved by glab");
    }
    if mr.iid != number {
        bail!("glab returned MR !{} instead of !{number}", mr.iid);
    }
    let branch_ref = format!("refs/heads/{}", mr.source_branch);
    if mr.source_branch.starts_with('-')
        || !git::unattended_git(None)?
            .args(["check-ref-format", &branch_ref])
            .status()?
            .success()
    {
        bail!("GitLab returned an invalid source branch");
    }
    let source_id = mr.source_project_id.filter(|id| *id > 0).context(
        "The merge request source project was deleted; source-project checkout is unavailable",
    )?;
    let is_fork = source_id != mr.target_project_id;
    let remote = if is_fork {
        let project: Project = glab
            .json(&["api", &format!("projects/{source_id}")])
            .context("Cannot read the merge request source project")?;
        let clone_url = if origin_url.starts_with("http://") || origin_url.starts_with("https://") {
            &project.http_url_to_repo
        } else {
            &project.ssh_url_to_repo
        };
        let source = Repository::parse(clone_url).context("Invalid source project clone URL")?;
        let mut existing = None;
        for name in git::list_remotes()? {
            if let Ok(url) = git::get_remote_url(&name)
                && let Ok(candidate) = Repository::parse(&url)
                && source.same_project(&candidate)
            {
                existing = Some(name);
                break;
            }
        }
        if let Some(name) = existing {
            name
        } else {
            let name = format!("gitlab-{source_id}");
            if git::remote_exists(&name)? {
                bail!(
                    "Remote '{name}' already points to a different repository; rename it before checking out this MR"
                );
            }
            if !dry_run {
                git::add_remote(&name, clone_url)?;
            }
            name
        }
    } else {
        "origin".to_string()
    };

    println!("MR !{number}: {}", mr.title);
    println!("Author: {}", mr.author.username);
    println!("Branch: {}", mr.source_branch);
    if mr.state != "opened" {
        eprintln!(
            "Warning: MR !{number} is {}. Proceeding with checkout...",
            mr.state
        );
    }
    if mr.draft {
        eprintln!("Warning: MR !{number} is a DRAFT.");
    }
    let local_branch = custom_branch.map(str::to_owned).unwrap_or_else(|| {
        if is_fork {
            format!("gitlab-{source_id}-{}", mr.source_branch)
        } else {
            mr.source_branch.clone()
        }
    });
    // The merge request target branch lives in the target project, which is
    // the one behind `origin`; the head may come from a different fork remote.
    let base_branch = mr
        .target_branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(|branch| format!("origin/{branch}"));
    Ok(PrCheckoutResult {
        checkout_ref: CheckoutRef {
            number,
            forge: Forge::Gitlab,
        },
        local_branch,
        remote_branch: format!("{remote}/{}", mr.source_branch),
        base_branch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_urls_preserve_nested_groups() {
        for url in [
            "https://gitlab.example.com/team/subgroup/project.git",
            "https://oauth2:example-token@gitlab.example.com/team/subgroup/project.git",
            "gitlab.example.com:team/subgroup/project.git",
            "git@gitlab.example.com:team/subgroup/project.git",
            "ssh://git@gitlab.example.com:2222/team/subgroup/project.git",
        ] {
            let repo = Repository::parse(url).unwrap();
            assert_eq!(
                repo.web_url(),
                "https://gitlab.example.com/team/subgroup/project"
            );
        }
        assert_eq!(
            Repository::parse("http://gitlab.example.com:8080/team/project.git")
                .unwrap()
                .web_url(),
            "http://gitlab.example.com:8080/team/project"
        );
    }

    #[test]
    fn invalid_repository_urls_are_rejected() {
        for url in [
            "/local/repo",
            "https://host/project",
            "https://host/a/../b",
            "https://host/a/b?query",
            "https://host/a//b",
            "file:///a/b",
        ] {
            assert!(Repository::parse(url).is_err(), "{url}");
        }
    }
}
