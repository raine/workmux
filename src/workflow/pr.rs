//! PR and fork branch resolution logic.
//!
//! This module extracts domain logic for resolving pull requests and fork branches
//! from the command layer, making it reusable and testable.

use crate::{git, github, spinner};
use anyhow::{Context, Result, anyhow};
use std::str::FromStr;

const PR_URL_PREFIX: &str = "https://";
const PR_REFERENCE_ERROR: &str = "expected a pull request number or a full GitHub pull request URL, or a GitLab merge request URL, including self-hosted instances, like https://github.example.com/owner/repo/pull/123";

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum Forge {
    Github,
    Gitlab,
}

#[derive(Clone, Copy, Debug)]
pub struct CheckoutRef {
    pub number: u32,
    pub forge: Forge,
}

impl CheckoutRef {
    pub fn head_ref(self) -> String {
        match self.forge {
            Forge::Github => format!("refs/pull/{}/head", self.number),
            Forge::Gitlab => format!("refs/merge-requests/{}/head", self.number),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrReference {
    number: u32,
    forge: Option<Forge>,
    repository: Option<crate::gitlab::Repository>,
}

impl PrReference {
    pub fn number(&self) -> u32 {
        self.number
    }

    pub fn resolve(
        &self,
        explicit: Option<Forge>,
        branch: Option<&str>,
        dry_run: bool,
    ) -> Result<PrCheckoutResult> {
        if let (Some(url_forge), Some(explicit)) = (self.forge, explicit)
            && url_forge != explicit
        {
            return Err(anyhow!(
                "--forge conflicts with the provider in the --pr URL"
            ));
        }
        let forge = explicit.or(self.forge).unwrap_or_else(|| {
            git::get_remote_url("origin")
                .ok()
                .and_then(|url| crate::gitlab::Repository::parse(&url).ok())
                .filter(|repo| repo.host.eq_ignore_ascii_case("gitlab.com"))
                .map(|_| Forge::Gitlab)
                .unwrap_or(Forge::Github)
        });
        match forge {
            Forge::Gitlab => {
                crate::gitlab::resolve(self.number(), self.repository.as_ref(), branch, dry_run)
            }
            Forge::Github if dry_run => resolve_pr_ref_dry_run(self.number(), branch),
            Forge::Github => resolve_pr_ref(self.number(), branch),
        }
    }
}

impl FromStr for PrReference {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if let Some((repository, number)) = value
            .trim_end_matches('/')
            .rsplit_once("/-/merge_requests/")
        {
            if !repository.starts_with("https://") && !repository.starts_with("http://") {
                return Err(PR_REFERENCE_ERROR.to_string());
            }
            let repository = crate::gitlab::Repository::parse(repository)
                .map_err(|_| PR_REFERENCE_ERROR.to_string())?;
            let number = number
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| PR_REFERENCE_ERROR.to_string())?;
            return Ok(Self {
                number,
                forge: Some(Forge::Gitlab),
                repository: Some(repository),
            });
        }
        let number = if let Ok(number) = value.parse() {
            number
        } else {
            let url = value
                .strip_prefix(PR_URL_PREFIX)
                .ok_or_else(|| PR_REFERENCE_ERROR.to_string())?;
            let url = url.strip_suffix('/').unwrap_or(url);
            let mut segments = url.split('/');
            let (Some(host), Some(owner), Some(repository), Some("pull"), Some(number), None) = (
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
            ) else {
                return Err(PR_REFERENCE_ERROR.to_string());
            };
            if host.is_empty() || owner.is_empty() || repository.is_empty() {
                return Err(PR_REFERENCE_ERROR.to_string());
            }
            number.parse().map_err(|_| PR_REFERENCE_ERROR.to_string())?
        };

        if number == 0 {
            return Err(PR_REFERENCE_ERROR.to_string());
        }

        Ok(Self {
            number,
            forge: value.starts_with(PR_URL_PREFIX).then_some(Forge::Github),
            repository: None,
        })
    }
}

/// Abstraction for git operations used in remote detection
trait RemoteDetectionContext {
    fn list_remotes(&self) -> Result<Vec<String>>;
    fn branch_exists(&self, ref_name: &str) -> Result<bool>;
    fn resolve_fork(&self, spec: &git::ForkBranchSpec) -> Result<ForkBranchResult>;
    fn fetch_remote(&self, remote: &str) -> Result<()>;
}

/// Real implementation using the git module
struct RealRemoteDetectionContext;

impl RemoteDetectionContext for RealRemoteDetectionContext {
    fn list_remotes(&self) -> Result<Vec<String>> {
        git::list_remotes()
    }

    fn branch_exists(&self, ref_name: &str) -> Result<bool> {
        git::branch_exists(ref_name)
    }

    fn resolve_fork(&self, spec: &git::ForkBranchSpec) -> Result<ForkBranchResult> {
        resolve_fork_branch(spec)
    }

    fn fetch_remote(&self, remote: &str) -> Result<()> {
        git::fetch_remote(remote)
    }
}

/// Generate a local branch name for a fork branch by prefixing with the owner.
/// Used by both `resolve_pr_ref` (--pr) and `resolve_fork_branch` (owner:branch)
/// to avoid conflicts with common branch names like "main".
fn fork_local_branch_name(owner: &str, branch: &str) -> String {
    format!("{}-{}", owner, branch)
}

/// Build the remote-tracking base ref for a PR target branch. GitHub PRs are
/// opened against the repository reachable through `origin`, so the target
/// branch lives there even when the head comes from a fork remote.
pub(crate) fn pr_base_branch(base_ref_name: &str) -> Option<String> {
    let base = base_ref_name.trim();
    (!base.is_empty()).then(|| format!("origin/{base}"))
}

/// Result of resolving a PR checkout.
#[derive(Debug)]
pub struct PrCheckoutResult {
    pub checkout_ref: CheckoutRef,
    pub local_branch: String,
    pub remote_branch: String,
    /// Remote-tracking ref to compare the checkout against for review, for
    /// example `origin/main`. This is the PR's target branch in the target
    /// repository and can differ from the ref used to populate the worktree.
    pub base_branch: Option<String>,
}

/// Resolve a PR reference and prepare for checkout.
///
/// Fetches PR details, sets up the remote if it's a fork, and returns
/// the branch information needed to create a worktree.
pub fn resolve_pr_ref(
    pr_number: u32,
    custom_branch_name: Option<&str>,
) -> Result<PrCheckoutResult> {
    let pr_details = spinner::with_spinner(&format!("Fetching PR #{}", pr_number), || {
        github::get_pr_details(pr_number)
    })
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

    // Determine if this is a fork PR and ensure remote exists
    let current_repo_owner =
        git::get_repo_owner().context("Failed to determine repository owner from origin remote")?;

    let is_fork = pr_details.is_fork(&current_repo_owner);
    let fork_owner = &pr_details.head_repository_owner.login;

    let remote_name = if is_fork {
        git::ensure_fork_remote(fork_owner)?
    } else {
        "origin".to_string()
    };

    // Determine local branch name.
    // For fork PRs, prefix with the fork owner to avoid conflicts with common
    // branch names like "main", matching resolve_fork_branch behavior.
    let local_branch = custom_branch_name.map(String::from).unwrap_or_else(|| {
        if is_fork {
            fork_local_branch_name(fork_owner, &pr_details.head_ref_name)
        } else {
            pr_details.head_ref_name.clone()
        }
    });

    // Note: We do not fetch here. The `create` workflow handles fetching
    // the remote branch to ensure the worktree base is up to date.
    let remote_branch = format!("{}/{}", remote_name, pr_details.head_ref_name);
    let base_branch = pr_base_branch(&pr_details.base_ref_name);

    Ok(PrCheckoutResult {
        checkout_ref: CheckoutRef {
            number: pr_number,
            forge: Forge::Github,
        },
        local_branch,
        remote_branch,
        base_branch,
    })
}

pub fn resolve_pr_ref_dry_run(
    pr_number: u32,
    custom_branch_name: Option<&str>,
) -> Result<PrCheckoutResult> {
    let pr_details = spinner::with_spinner(&format!("Fetching PR #{}", pr_number), || {
        github::get_pr_details(pr_number)
    })
    .with_context(|| format!("Failed to fetch details for PR #{}", pr_number))?;
    println!("PR #{}: {}", pr_number, pr_details.title);
    println!("Author: {}", pr_details.author.login);
    println!("Branch: {}", pr_details.head_ref_name);

    let current_repo_owner =
        git::get_repo_owner().context("Failed to determine repository owner from origin remote")?;
    let is_fork = pr_details.is_fork(&current_repo_owner);
    let fork_owner = &pr_details.head_repository_owner.login;
    let remote_name = if is_fork {
        git::resolve_fork_remote_name(fork_owner)?
    } else {
        "origin".to_string()
    };
    let local_branch = custom_branch_name.map(String::from).unwrap_or_else(|| {
        if is_fork {
            fork_local_branch_name(fork_owner, &pr_details.head_ref_name)
        } else {
            pr_details.head_ref_name.clone()
        }
    });
    Ok(PrCheckoutResult {
        checkout_ref: CheckoutRef {
            number: pr_number,
            forge: Forge::Github,
        },
        local_branch,
        remote_branch: format!("{}/{}", remote_name, pr_details.head_ref_name),
        base_branch: pr_base_branch(&pr_details.base_ref_name),
    })
}

/// Result of resolving a fork branch.
pub struct ForkBranchResult {
    pub remote_ref: String,
    pub template_base_name: String,
}

/// Resolve a fork branch specified as "owner:branch".
///
/// Sets up the fork remote and optionally displays associated PR info.
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

    // Note: We do not fetch or verify the branch exists here.
    // The `create` workflow will perform the fetch and fail if the branch is missing.
    let remote_ref = format!("{}/{}", remote_name, fork_spec.branch);

    // Always prefix the local branch name with the fork owner to avoid conflicts
    // with existing branches (e.g., "main"). The owner:branch syntax already signals
    // this is someone else's branch, so including the owner is informative.
    let local_branch_name = fork_local_branch_name(&fork_spec.owner, &fork_spec.branch);

    Ok(ForkBranchResult {
        remote_ref,
        template_base_name: local_branch_name,
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
    detect_remote_branch_internal(branch_name, base, &RealRemoteDetectionContext)
}

pub fn detect_remote_branch_dry_run(
    branch_name: &str,
    base: Option<&str>,
) -> Result<(Option<String>, String)> {
    if let Some(fork_spec) = git::parse_fork_branch_spec(branch_name) {
        if base.is_some() {
            return Err(anyhow!(
                "Cannot use --base with 'owner:branch' syntax. The branch '{}' from '{}' will be used as the base.",
                fork_spec.branch,
                fork_spec.owner
            ));
        }
        let remote = git::resolve_fork_remote_name(&fork_spec.owner)?;
        return Ok((
            Some(format!("{}/{}", remote, fork_spec.branch)),
            fork_local_branch_name(&fork_spec.owner, &fork_spec.branch),
        ));
    }

    let remotes = git::list_remotes().context("Failed to list git remotes")?;
    if remotes
        .iter()
        .any(|remote| branch_name.starts_with(&format!("{}/", remote)))
    {
        if base.is_some() {
            return Err(anyhow!(
                "Cannot use --base with a remote branch reference. The remote branch '{}' will be used as the base.",
                branch_name
            ));
        }
        let spec = git::parse_remote_branch_spec(branch_name)
            .context("Invalid remote branch format. Use <remote>/<branch>")?;
        return Ok((Some(branch_name.to_string()), spec.branch));
    }
    Ok((None, branch_name.to_string()))
}

/// Internal logic using the context trait for testability.
fn detect_remote_branch_internal(
    branch_name: &str,
    base: Option<&str>,
    ctx: &dyn RemoteDetectionContext,
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

        let result = ctx.resolve_fork(&fork_spec)?;
        return Ok((Some(result.remote_ref), result.template_base_name));
    }

    // 2. Existing remote/branch detection (e.g., "origin/feature")
    let remotes = ctx.list_remotes().context("Failed to list git remotes")?;
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

        // Only treat as remote branch if it actually exists (is locally known).
        // Explicitly check refs/remotes/ to avoid matching a local branch named "remote/branch".
        // If the remote branch is not found locally, try fetching it first.
        let remote_ref = format!("refs/remotes/{}", branch_name);
        if !ctx.branch_exists(&remote_ref)? {
            // Remote branch not found locally - try fetching to see if it exists on the server
            spinner::with_spinner(
                &format!(
                    "Branch prefix matches remote '{}', verifying if it exists there...",
                    remote_name
                ),
                || {
                    ctx.fetch_remote(remote_name).with_context(|| {
                        format!(
                            "Failed to fetch from remote '{}'. Please check your network connection and try again.",
                            remote_name
                        )
                    })
                },
            )?;

            // Check again after fetch
            if !ctx.branch_exists(&remote_ref)? {
                // Branch doesn't exist on the server either - user wants a local branch with this name
                eprintln!(
                    "Not found on '{}', creating local branch '{}'",
                    remote_name, branch_name
                );
                return Ok((None, branch_name.to_string()));
            }

            // Found it after fetching! Treat as remote branch
        }

        Ok((Some(branch_name.to_string()), spec.branch))
    } else {
        Ok((None, branch_name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn parses_numeric_pr_reference() {
        assert_eq!(PrReference::from_str("190").unwrap().number(), 190);
    }

    #[test]
    fn parses_github_pr_url() {
        for value in [
            "https://github.com/raine/workmux/pull/190",
            "https://github.com/raine/workmux/pull/190/",
            "https://github.example.com/owner/repo/pull/190",
        ] {
            assert_eq!(PrReference::from_str(value).unwrap().number(), 190);
        }
    }

    #[test]
    fn parses_gitlab_references() {
        for url in [
            "https://gitlab.com/group/project/-/merge_requests/123",
            "https://gitlab.example.com/group/subgroup/project/-/merge_requests/123/",
            "http://gitlab.example.com:8080/group/project/-/merge_requests/123",
        ] {
            let reference: PrReference = url.parse().unwrap();
            assert_eq!(reference.number(), 123);
            assert_eq!(reference.forge, Some(Forge::Gitlab));
            assert!(reference.repository.is_some());
            assert!(
                reference
                    .resolve(Some(Forge::Github), None, true)
                    .unwrap_err()
                    .to_string()
                    .contains("conflicts")
            );
        }
    }

    #[test]
    fn rejects_invalid_gitlab_references() {
        for url in [
            "https://gitlab.com/group/project/-/merge_requests/0",
            "https://gitlab.com/group/project/-/merge_requests/123/diffs",
            "https://gitlab.com/group/project/-/merge_requests/123?query",
            "https://gitlab.com/group/project/-/merge_requests/nope",
            "https://gitlab.com/group/project/-/issues/123",
        ] {
            assert!(url.parse::<PrReference>().is_err(), "{url}");
        }
    }

    #[test]
    fn pr_base_branch_builds_remote_tracking_ref() {
        assert_eq!(pr_base_branch("main").as_deref(), Some("origin/main"));
        assert_eq!(
            pr_base_branch("release/1.0").as_deref(),
            Some("origin/release/1.0")
        );
        assert_eq!(pr_base_branch("  ").as_deref(), None);
    }

    #[test]
    fn review_ref_uses_forge_namespace() {
        assert_eq!(
            CheckoutRef {
                number: 123,
                forge: Forge::Github
            }
            .head_ref(),
            "refs/pull/123/head"
        );
        assert_eq!(
            CheckoutRef {
                number: 123,
                forge: Forge::Gitlab
            }
            .head_ref(),
            "refs/merge-requests/123/head"
        );
    }

    #[test]
    fn rejects_invalid_pr_references() {
        for value in [
            "0",
            "not-a-pr",
            "http://github.com/raine/workmux/pull/190",
            "https://github.com/raine/workmux/issues/190",
            "https://github.com/raine/workmux/pull/0",
            "https://github.com/raine/workmux/pull/not-a-number",
            "https://github.com/raine/workmux/pull/190/files",
        ] {
            assert_eq!(
                PrReference::from_str(value).unwrap_err().to_string(),
                PR_REFERENCE_ERROR
            );
        }
    }

    /// Mock context for testing without actual git operations
    struct MockContext {
        remotes: Vec<String>,
        existing_refs: HashSet<String>,
        /// Refs that will become available after fetch is called
        refs_available_after_fetch: HashSet<String>,
        /// Whether fetch should fail
        fetch_should_fail: bool,
    }

    impl MockContext {
        fn new(remotes: &[&str], existing_refs: &[&str]) -> Self {
            Self {
                remotes: remotes.iter().map(|s| s.to_string()).collect(),
                existing_refs: existing_refs.iter().map(|s| s.to_string()).collect(),
                refs_available_after_fetch: HashSet::new(),
                fetch_should_fail: false,
            }
        }

        /// Create mock with refs that will appear after fetch
        fn with_fetchable_refs(
            remotes: &[&str],
            existing_refs: &[&str],
            fetchable: &[&str],
        ) -> Self {
            Self {
                remotes: remotes.iter().map(|s| s.to_string()).collect(),
                existing_refs: existing_refs.iter().map(|s| s.to_string()).collect(),
                refs_available_after_fetch: fetchable.iter().map(|s| s.to_string()).collect(),
                fetch_should_fail: false,
            }
        }

        /// Create mock where fetch will fail
        fn with_failing_fetch(remotes: &[&str], existing_refs: &[&str]) -> Self {
            Self {
                remotes: remotes.iter().map(|s| s.to_string()).collect(),
                existing_refs: existing_refs.iter().map(|s| s.to_string()).collect(),
                refs_available_after_fetch: HashSet::new(),
                fetch_should_fail: true,
            }
        }
    }

    impl RemoteDetectionContext for MockContext {
        fn list_remotes(&self) -> Result<Vec<String>> {
            Ok(self.remotes.clone())
        }

        fn branch_exists(&self, ref_name: &str) -> Result<bool> {
            Ok(self.existing_refs.contains(ref_name)
                || self.refs_available_after_fetch.contains(ref_name))
        }

        fn resolve_fork(&self, spec: &git::ForkBranchSpec) -> Result<ForkBranchResult> {
            // Mirror real behavior: always prefix local branch name with owner
            Ok(ForkBranchResult {
                remote_ref: format!("fork-{}/{}", spec.owner, spec.branch),
                template_base_name: format!("{}-{}", spec.owner, spec.branch),
            })
        }

        fn fetch_remote(&self, remote: &str) -> Result<()> {
            if self.fetch_should_fail {
                return Err(anyhow!("Network error: failed to fetch from '{}'", remote));
            }
            // Mock fetch is a no-op - refs in refs_available_after_fetch are already "available"
            Ok(())
        }
    }

    #[test]
    fn test_simple_local_branch_no_slash() {
        // Case: "feature" - simple branch name with no slash
        let ctx = MockContext::new(&["origin"], &[]);
        let (remote, local) = detect_remote_branch_internal("feature", None, &ctx).unwrap();
        assert_eq!(remote, None);
        assert_eq!(local, "feature");
    }

    #[test]
    fn test_local_branch_with_slash_no_remote_match() {
        // Case: "feature/foo" where "feature" is not a remote name
        // Should treat the entire string as a local branch name
        let ctx = MockContext::new(&["origin"], &[]);
        let (remote, local) = detect_remote_branch_internal("feature/foo", None, &ctx).unwrap();
        assert_eq!(remote, None);
        assert_eq!(local, "feature/foo");
    }

    #[test]
    fn test_remote_branch_exists() {
        // Case: "origin/feature" where origin is a remote AND the remote branch exists
        // Should treat as remote branch reference
        let ctx = MockContext::new(&["origin"], &["refs/remotes/origin/feature"]);
        let (remote, local) = detect_remote_branch_internal("origin/feature", None, &ctx).unwrap();
        assert_eq!(remote, Some("origin/feature".to_string()));
        assert_eq!(local, "feature");
    }

    #[test]
    fn test_remote_prefix_but_branch_missing_issue_28() {
        // Case: "ezh/some-feature" where "ezh" IS a remote name but remote branch doesn't exist
        // This is the main issue #28 case - should create local branch, not error
        let ctx = MockContext::new(&["origin", "ezh"], &[]);
        let (remote, local) =
            detect_remote_branch_internal("ezh/some-feature", None, &ctx).unwrap();

        // Should fallback to local branch creation
        assert_eq!(remote, None);
        assert_eq!(local, "ezh/some-feature");
    }

    #[test]
    fn test_origin_branch_missing_forgot_to_fetch() {
        // Case: "origin/new-feature" where user likely forgot to fetch
        // Should warn and create local branch (not error)
        let ctx = MockContext::new(&["origin"], &[]);
        let (remote, local) =
            detect_remote_branch_internal("origin/new-feature", None, &ctx).unwrap();

        // Should fallback to local branch creation with warning
        assert_eq!(remote, None);
        assert_eq!(local, "origin/new-feature");
    }

    #[test]
    fn test_fork_syntax_owner_colon_branch() {
        // Case: "owner:branch" - GitHub fork format
        // Local branch name is always prefixed with owner to avoid conflicts
        let ctx = MockContext::new(&["origin"], &[]);
        let (remote, local) = detect_remote_branch_internal("owner:branch", None, &ctx).unwrap();

        assert_eq!(remote, Some("fork-owner/branch".to_string()));
        assert_eq!(local, "owner-branch");
    }

    #[test]
    fn test_fork_syntax_with_slash_in_branch() {
        // Case: "owner:feature/foo" - fork with slash in branch name
        // Local branch is prefixed: "owner-feature/foo" (slugified later to "owner-feature-foo")
        let ctx = MockContext::new(&["origin"], &[]);
        let (remote, local) =
            detect_remote_branch_internal("owner:feature/foo", None, &ctx).unwrap();

        assert_eq!(remote, Some("fork-owner/feature/foo".to_string()));
        assert_eq!(local, "owner-feature/foo");
    }

    #[test]
    fn test_fork_syntax_avoids_main_conflict() {
        // Case: "sundbp:main" - would conflict with existing "main" branch
        // Always prefixed with owner regardless of whether conflict exists
        let ctx = MockContext::new(&["origin"], &[]);
        let (remote, local) = detect_remote_branch_internal("sundbp:main", None, &ctx).unwrap();

        assert_eq!(remote, Some("fork-sundbp/main".to_string()));
        assert_eq!(local, "sundbp-main");
    }

    #[test]
    fn test_base_flag_with_remote_syntax_errors() {
        // Case: Using --base with remote syntax should error
        let ctx = MockContext::new(&["origin"], &["refs/remotes/origin/feature"]);

        let err = detect_remote_branch_internal("origin/feature", Some("main"), &ctx).unwrap_err();
        assert!(err.to_string().contains("Cannot use --base"));
        assert!(err.to_string().contains("remote branch"));
    }

    #[test]
    fn test_base_flag_with_fork_syntax_errors() {
        // Case: Using --base with fork syntax should error
        let ctx = MockContext::new(&["origin"], &[]);

        let err = detect_remote_branch_internal("owner:branch", Some("main"), &ctx).unwrap_err();
        assert!(err.to_string().contains("Cannot use --base"));
        assert!(err.to_string().contains("owner:branch"));
    }

    #[test]
    fn test_multiple_remotes_correct_match() {
        // Case: Multiple remotes exist, ensure we match the right one
        let ctx = MockContext::new(
            &["origin", "upstream", "fork"],
            &["refs/remotes/upstream/develop"],
        );

        let (remote, local) =
            detect_remote_branch_internal("upstream/develop", None, &ctx).unwrap();
        assert_eq!(remote, Some("upstream/develop".to_string()));
        assert_eq!(local, "develop");
    }

    #[test]
    fn test_nested_slashes_in_branch_name() {
        // Case: "feature/sub/task" where "feature" is not a remote
        let ctx = MockContext::new(&["origin"], &[]);
        let (remote, local) =
            detect_remote_branch_internal("feature/sub/task", None, &ctx).unwrap();

        assert_eq!(remote, None);
        assert_eq!(local, "feature/sub/task");
    }

    #[test]
    fn test_remote_with_nested_branch() {
        // Case: "origin/feature/sub/task" where origin is a remote and remote branch exists
        let ctx = MockContext::new(&["origin"], &["refs/remotes/origin/feature/sub/task"]);

        let (remote, local) =
            detect_remote_branch_internal("origin/feature/sub/task", None, &ctx).unwrap();
        assert_eq!(remote, Some("origin/feature/sub/task".to_string()));
        assert_eq!(local, "feature/sub/task");
    }

    #[test]
    fn test_fetch_makes_remote_branch_available() {
        // Case: "origin/new-feature" doesn't exist locally, but becomes available after fetch
        // This simulates the "forgot to fetch" scenario where the branch exists on the server
        let ctx = MockContext::with_fetchable_refs(
            &["origin"],
            &[],
            &["refs/remotes/origin/new-feature"],
        );

        let (remote, local) =
            detect_remote_branch_internal("origin/new-feature", None, &ctx).unwrap();

        // Should successfully treat as remote branch (found after fetch)
        assert_eq!(remote, Some("origin/new-feature".to_string()));
        assert_eq!(local, "new-feature");
    }

    #[test]
    fn test_fetch_succeeds_but_branch_not_found_creates_local() {
        // Case: Fetch succeeds but remote branch doesn't exist on server
        // This is for branch naming conventions like "ezh/feature" where ezh is a remote
        // but the branch doesn't exist on the server either
        let ctx = MockContext::new(&["ezh"], &[]);

        let (remote, local) = detect_remote_branch_internal("ezh/my-feature", None, &ctx).unwrap();

        // Should fallback to local branch creation (fetch succeeded, branch just doesn't exist)
        assert_eq!(remote, None);
        assert_eq!(local, "ezh/my-feature");
    }

    #[test]
    fn test_fetch_fails_returns_error() {
        // Case: Network error or auth failure during fetch
        // Should NOT create a confusingly-named local branch "origin/feature"
        let ctx = MockContext::with_failing_fetch(&["origin"], &[]);

        let err = detect_remote_branch_internal("origin/new-feature", None, &ctx).unwrap_err();

        // Should error out, not fallback to local branch creation
        assert!(err.to_string().contains("Failed to fetch"));
        assert!(err.to_string().contains("origin"));
    }
}
