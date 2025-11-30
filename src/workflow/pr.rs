//! PR and fork branch resolution logic.
//!
//! This module extracts domain logic for resolving pull requests and fork branches
//! from the command layer, making it reusable and testable.

use crate::{git, github};
use anyhow::{Context, Result, anyhow};

/// Result of resolving a PR checkout.
pub struct PrCheckoutResult {
    pub local_branch: String,
    pub remote_branch: String,
}

/// Resolve a PR reference and prepare for checkout.
///
/// Fetches PR details, sets up the remote if it's a fork, and returns
/// the branch information needed to create a worktree.
pub fn resolve_pr_ref(
    pr_number: u32,
    custom_branch_name: Option<&str>,
) -> Result<PrCheckoutResult> {
    println!("Fetching PR #{}...", pr_number);
    let pr_details = github::get_pr_details(pr_number)
        .with_context(|| format!("Failed to fetch details for PR #{}", pr_number))?;

    // Display PR information
    println!("PR #{}: {}", pr_number, pr_details.title);
    println!("Author: {}", pr_details.author.login);
    println!("Branch: {}", pr_details.head_ref_name);

    // Warn about PR state
    if pr_details.state != "OPEN" {
        eprintln!(
            "⚠️  Warning: PR #{} is {}. Proceeding with checkout...",
            pr_number, pr_details.state
        );
    }
    if pr_details.is_draft {
        eprintln!("⚠️  Warning: PR #{} is a DRAFT.", pr_number);
    }

    // Determine local branch name (match gh pr checkout behavior)
    let local_branch = custom_branch_name
        .map(String::from)
        .unwrap_or_else(|| pr_details.head_ref_name.clone());

    // Determine if this is a fork PR and ensure remote exists
    let current_repo_owner =
        git::get_repo_owner().context("Failed to determine repository owner from origin remote")?;

    let remote_name = if pr_details.is_fork(&current_repo_owner) {
        let fork_owner = &pr_details.head_repository_owner.login;
        git::ensure_fork_remote(fork_owner)?
    } else {
        "origin".to_string()
    };

    // Fetch the PR branch
    println!(
        "Fetching branch '{}' from '{}'...",
        pr_details.head_ref_name, remote_name
    );
    git::fetch_remote(&remote_name)
        .with_context(|| format!("Failed to fetch from remote '{}'", remote_name))?;

    let remote_branch = format!("{}/{}", remote_name, pr_details.head_ref_name);

    Ok(PrCheckoutResult {
        local_branch,
        remote_branch,
    })
}

/// Result of resolving a fork branch.
pub struct ForkBranchResult {
    pub remote_ref: String,
    pub template_base_name: String,
}

/// Resolve a fork branch specified as "owner:branch".
///
/// Sets up the fork remote, fetches, and optionally displays associated PR info.
pub fn resolve_fork_branch(fork_spec: &git::ForkBranchSpec) -> Result<ForkBranchResult> {
    // Try to find an associated PR and display info (optional, non-blocking)
    if let Ok(Some(pr)) = github::find_pr_by_head_ref(&fork_spec.owner, &fork_spec.branch) {
        let state_suffix = match pr.state.as_str() {
            "OPEN" if pr.is_draft => " (draft)",
            "OPEN" => "",
            "MERGED" => " (merged)",
            "CLOSED" => " (closed)",
            _ => "",
        };
        println!("PR #{}: {}{}", pr.number, pr.title, state_suffix);
    }

    // Ensure the fork remote exists
    let remote_name = git::ensure_fork_remote(&fork_spec.owner)?;

    // Fetch to get the latest refs
    println!(
        "Fetching branch '{}' from '{}'...",
        fork_spec.branch, remote_name
    );
    git::fetch_remote(&remote_name)
        .with_context(|| format!("Failed to fetch from remote '{}'", remote_name))?;

    // Verify the branch exists on the remote
    let remote_ref = format!("{}/{}", remote_name, fork_spec.branch);
    if !git::branch_exists(&remote_ref)? {
        return Err(anyhow!(
            "Branch '{}' not found on remote '{}' (fork of {})",
            fork_spec.branch,
            remote_name,
            fork_spec.owner
        ));
    }

    Ok(ForkBranchResult {
        remote_ref,
        template_base_name: fork_spec.branch.clone(),
    })
}

/// Detect if a branch name refers to a remote branch and extract the base name.
///
/// Handles both "remote/branch" format and "owner:branch" (GitHub fork) format.
/// Returns (remote_branch, template_base_name).
pub fn detect_remote_branch(
    branch_name: &str,
    base: Option<&str>,
) -> Result<(Option<String>, String)> {
    // 1. Check for owner:branch syntax (GitHub fork format, e.g., "someuser:feature-a")
    if let Some(fork_spec) = git::parse_fork_branch_spec(branch_name) {
        if base.is_some() {
            return Err(anyhow!(
                "Cannot use --base with 'owner:branch' syntax. \
                The branch '{}' from '{}' will be used as the base.",
                fork_spec.branch,
                fork_spec.owner
            ));
        }

        let result = resolve_fork_branch(&fork_spec)?;
        return Ok((Some(result.remote_ref), result.template_base_name));
    }

    // 2. Existing remote/branch detection (e.g., "origin/feature")
    let remotes = git::list_remotes().context("Failed to list git remotes")?;
    let detected_remote = remotes
        .iter()
        .find(|r| branch_name.starts_with(&format!("{}/", r)));

    if let Some(remote_name) = detected_remote {
        if base.is_some() {
            return Err(anyhow!(
                "Cannot use --base with a remote branch reference. \
                The remote branch '{}' will be used as the base.",
                branch_name
            ));
        }

        let spec = git::parse_remote_branch_spec(branch_name)
            .context("Invalid remote branch format. Use <remote>/<branch>")?;

        if spec.remote != *remote_name {
            return Err(anyhow!("Mismatched remote detection"));
        }

        Ok((Some(branch_name.to_string()), spec.branch))
    } else {
        Ok((None, branch_name.to_string()))
    }
}
