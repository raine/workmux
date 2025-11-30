use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::process::Command;

#[derive(Debug, Deserialize)]
pub struct PrDetails {
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    #[serde(rename = "headRepositoryOwner")]
    pub head_repository_owner: RepositoryOwner,
    pub state: String,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    pub title: String,
    pub author: Author,
}

#[derive(Debug, Deserialize)]
pub struct RepositoryOwner {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct Author {
    pub login: String,
}

impl PrDetails {
    pub fn is_fork(&self, current_repo_owner: &str) -> bool {
        self.head_repository_owner.login != current_repo_owner
    }
}

/// Summary of a PR found by head ref search
#[derive(Debug, Deserialize)]
pub struct PrSummary {
    pub number: u32,
    pub title: String,
    pub state: String,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
}

/// Internal struct for parsing PR list results with owner info
#[derive(Debug, Deserialize)]
struct PrListResult {
    pub number: u32,
    pub title: String,
    pub state: String,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    #[serde(rename = "headRepositoryOwner")]
    pub head_repository_owner: RepositoryOwner,
}

/// Find a PR by its head ref (e.g., "owner:branch" format).
/// Returns None if no PR is found, or the first matching PR if found.
pub fn find_pr_by_head_ref(owner: &str, branch: &str) -> Result<Option<PrSummary>> {
    // gh pr list --head only matches branch name, not owner:branch format
    // So we query by branch and filter by owner in the results
    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all", // Include closed/merged PRs
            "--json",
            "number,title,state,isDraft,headRepositoryOwner",
            "--limit",
            "50", // Get enough results to handle common branch names
        ])
        .output();

    let output = match output {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // gh not installed, silently return None (PR lookup is optional)
            return Ok(None);
        }
        Err(e) => {
            return Err(e).context("Failed to execute gh command");
        }
    };

    if !output.status.success() {
        // Non-zero exit could mean no PRs found or other error; treat as no PR
        return Ok(None);
    }

    let json_str = String::from_utf8(output.stdout).context("gh output is not valid UTF-8")?;

    // gh pr list returns an array
    let prs: Vec<PrListResult> =
        serde_json::from_str(&json_str).context("Failed to parse gh JSON output")?;

    // Find the PR from the specified owner (case-insensitive, as GitHub usernames are case-insensitive)
    let matching_pr = prs
        .into_iter()
        .find(|pr| pr.head_repository_owner.login.eq_ignore_ascii_case(owner));

    Ok(matching_pr.map(|pr| PrSummary {
        number: pr.number,
        title: pr.title,
        state: pr.state,
        is_draft: pr.is_draft,
    }))
}

/// Fetches pull request details using the GitHub CLI
pub fn get_pr_details(pr_number: u32) -> Result<PrDetails> {
    // Fetch PR details using gh CLI
    // Note: We don't pre-check with 'which' because it doesn't respect test PATH modifications
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "headRefName,headRepositoryOwner,state,isDraft,title,author",
        ])
        .output();

    let output = match output {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!(
                "GitHub CLI (gh) is required for --pr. Install from https://cli.github.com"
            ));
        }
        Err(e) => {
            return Err(e).context("Failed to execute gh command");
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to fetch PR #{}: {}",
            pr_number,
            stderr.trim()
        ));
    }

    let json_str = String::from_utf8(output.stdout).context("gh output is not valid UTF-8")?;

    let pr_details: PrDetails =
        serde_json::from_str(&json_str).context("Failed to parse gh JSON output")?;

    Ok(pr_details)
}
