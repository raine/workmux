use anyhow::{Context, Result, anyhow};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tracing::debug;

use crate::util::write_atomic;

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

/// Aggregated status of PR checks
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CheckState {
    /// All checks passed
    Success,
    /// Some checks failed (passed/total)
    Failure { passed: u32, total: u32 },
    /// Checks still running (passed/total)
    Pending { passed: u32, total: u32 },
}

/// Summary of a PR found by head ref search
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrSummary {
    pub number: u32,
    pub title: String,
    pub state: String,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    /// Aggregated check status (None if no checks configured)
    #[serde(default)]
    pub checks: Option<CheckState>,
    /// Check timing and name metadata
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_meta: Option<CheckMeta>,
    /// PR URL for opening in browser
    #[serde(default)]
    pub url: Option<String>,
}

/// Metadata about PR checks (timing, names) separate from aggregated state
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct CheckMeta {
    /// Earliest start time among pending/running checks (Unix timestamp).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    /// Pre-computed total duration in seconds for completed check runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    /// Name of the first failing check, if any
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_name: Option<String>,
}

/// Handles both CheckRun (status/conclusion) and StatusContext (state) from GitHub API
#[derive(Debug, Deserialize)]
struct CheckRollupItem {
    #[serde(alias = "state")]
    status: Option<String>,
    conclusion: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
}

/// Parse a GitHub ISO 8601 UTC timestamp (e.g., "2026-03-24T14:02:00Z") to Unix seconds.
fn parse_github_timestamp(s: &str) -> Option<u64> {
    // GitHub always returns UTC timestamps in format: YYYY-MM-DDTHH:MM:SSZ
    let s = s.trim();
    if s.len() < 20 || !s.ends_with('Z') {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year: u64 = s[0..4].parse().ok()?;
    let month: u64 = s[5..7].parse().ok()?;
    let day: u64 = s[8..10].parse().ok()?;
    let hour: u64 = s[11..13].parse().ok()?;
    let min: u64 = s[14..16].parse().ok()?;
    let sec: u64 = s[17..19].parse().ok()?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 59 {
        return None;
    }

    // Days from year 0 to Unix epoch (1970-01-01)
    // Using a simplified days-since-epoch calculation
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }
    let month_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[(m - 1) as usize] as u64;
        if m == 2 && is_leap_year(year) {
            days += 1;
        }
    }
    days += day - 1;

    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap_year(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

/// Aggregate check results into a single CheckState with optional metadata
fn aggregate_checks(checks: &[CheckRollupItem]) -> (Option<CheckState>, Option<CheckMeta>) {
    if checks.is_empty() {
        return (None, None);
    }

    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut pending = 0u32;
    let mut skipped = 0u32;
    let mut earliest_pending_start: Option<u64> = None;
    let mut earliest_any_start: Option<u64> = None;
    let mut latest_any_start: Option<u64> = None;
    let mut failing_name: Option<String> = None;

    for check in checks {
        let status = check.status.as_deref().unwrap_or("");
        let conclusion = check.conclusion.as_deref().unwrap_or("");
        let ts = check.started_at.as_deref().and_then(parse_github_timestamp);

        // Track global start time range
        if let Some(t) = ts {
            earliest_any_start = Some(earliest_any_start.map_or(t, |prev: u64| prev.min(t)));
            latest_any_start = Some(latest_any_start.map_or(t, |prev: u64| prev.max(t)));
        }

        match (status, conclusion) {
            // Success states
            (_, "SUCCESS") | ("SUCCESS", _) => passed += 1,
            // Failure states (expanded to catch all failure-like conclusions)
            (_, "FAILURE" | "CANCELLED" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED")
            | ("FAILURE" | "ERROR", _) => {
                failed += 1;
                if failing_name.is_none() {
                    failing_name = check.name.clone();
                }
            }
            // Neutral/skipped - track but don't count toward active total
            (_, "NEUTRAL" | "SKIPPED") => skipped += 1,
            // Pending states (expanded)
            ("IN_PROGRESS" | "QUEUED" | "PENDING" | "REQUESTED" | "WAITING", _) => {
                pending += 1;
                if let Some(t) = ts {
                    earliest_pending_start =
                        Some(earliest_pending_start.map_or(t, |prev: u64| prev.min(t)));
                }
            }
            _ => {}
        }
    }

    let total = passed + failed + pending;

    // If no active checks but some were skipped, treat as success (GitHub behavior)
    if total == 0 {
        return if skipped > 0 {
            (Some(CheckState::Success), None)
        } else {
            (None, None)
        };
    }

    let state = if failed > 0 {
        CheckState::Failure { passed, total }
    } else if pending > 0 {
        CheckState::Pending { passed, total }
    } else {
        CheckState::Success
    };

    // Build metadata
    let meta = if pending > 0 {
        // Use earliest pending start, fall back to earliest any start
        let started = earliest_pending_start.or(earliest_any_start);
        if started.is_some() || failing_name.is_some() {
            Some(CheckMeta {
                started_at: started,
                duration_secs: None,
                failing_name,
            })
        } else {
            None
        }
    } else if failed > 0 {
        // For failures, compute duration if we know when checks started
        let duration_secs = match (earliest_any_start, current_unix_timestamp()) {
            (Some(start), Some(now)) => Some(now.saturating_sub(start)),
            _ => None,
        };
        if failing_name.is_some() || duration_secs.is_some() {
            Some(CheckMeta {
                started_at: earliest_any_start,
                duration_secs,
                failing_name,
            })
        } else {
            None
        }
    } else {
        None
    };

    (Some(state), meta)
}

/// Get current Unix timestamp in seconds
fn current_unix_timestamp() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
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
    #[serde(default)]
    pub url: Option<String>,
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
            "number,title,state,isDraft,headRepositoryOwner,url",
            "--limit",
            "50", // Get enough results to handle common branch names
        ])
        .output();

    let output = match output {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!("github:gh CLI not found, skipping PR lookup");
            return Ok(None);
        }
        Err(e) => {
            return Err(e).context("Failed to execute gh command");
        }
    };

    if !output.status.success() {
        debug!(
            owner = owner,
            branch = branch,
            "github:pr list failed, treating as no PR found"
        );
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
        checks: None,
        check_meta: None,
        url: pr.url,
    }))
}

/// An open PR entry for display in the add-worktree modal.
pub struct PrListEntry {
    pub number: u32,
    pub title: String,
    pub head_ref_name: String,
    pub author: String,
    pub is_draft: bool,
}

/// List open PRs for a repository using the GitHub CLI.
pub fn list_open_prs(repo_root: &Path) -> Result<Vec<PrListEntry>> {
    #[derive(Deserialize)]
    struct RawPr {
        number: u32,
        title: String,
        #[serde(rename = "headRefName")]
        head_ref_name: String,
        #[serde(rename = "isDraft")]
        is_draft: bool,
        author: Author,
    }

    let output = Command::new("gh")
        .current_dir(repo_root)
        .args([
            "pr",
            "list",
            "--state",
            "open",
            "--json",
            "number,title,headRefName,isDraft,author",
            "--limit",
            "100",
        ])
        .output();

    let output = match output {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!("GitHub CLI (gh) not found"));
        }
        Err(e) => return Err(e).context("Failed to execute gh command"),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("gh pr list failed: {}", stderr.trim()));
    }

    let raw: Vec<RawPr> =
        serde_json::from_slice(&output.stdout).context("Failed to parse gh pr list output")?;

    Ok(raw
        .into_iter()
        .map(|pr| PrListEntry {
            number: pr.number,
            title: pr.title,
            head_ref_name: pr.head_ref_name,
            author: pr.author.login,
            is_draft: pr.is_draft,
        })
        .collect())
}

/// Fetches pull request details using the GitHub CLI
pub fn get_pr_details(pr_number: u32) -> Result<PrDetails> {
    get_pr_details_in(None, pr_number)
}

/// Fetches pull request details using the GitHub CLI in a specific repository path
pub fn get_pr_details_in(repo_root: Option<&Path>, pr_number: u32) -> Result<PrDetails> {
    // Fetch PR details using gh CLI
    // Note: We don't pre-check with 'which' because it doesn't respect test PATH modifications
    let mut command = Command::new("gh");
    if let Some(path) = repo_root {
        command.current_dir(path);
    }
    let output = command
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
            debug!("github:gh CLI not found");
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
        debug!(pr = pr_number, stderr = %stderr, "github:pr view failed");
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

const PR_LIST_FIELDS_WITH_CHECKS: &str =
    "number,title,state,isDraft,headRefName,url,statusCheckRollup";
const PR_LIST_FIELDS: &str = "number,title,state,isDraft,headRefName,url";

fn run_pr_list(
    repo_root: Option<&Path>,
    args: &[&str],
    json_fields: &str,
) -> std::io::Result<std::process::Output> {
    let mut command = Command::new("gh");
    if let Some(path) = repo_root {
        command.current_dir(path);
    }
    command.args(args).args(["--json", json_fields]).output()
}

/// Internal struct for parsing batch PR list results
#[derive(Debug, Deserialize)]
struct PrBatchItem {
    number: u32,
    title: String,
    state: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    url: String,
    #[serde(rename = "statusCheckRollup", default)]
    status_check_rollup: Vec<CheckRollupItem>,
}

/// Fetch all PRs for the current repository.
pub fn list_prs() -> Result<HashMap<String, PrSummary>> {
    let args = ["pr", "list", "--state", "all", "--limit", "200"];
    let output = run_pr_list(None, &args, PR_LIST_FIELDS_WITH_CHECKS);

    let output = match output {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            debug!(stderr = %stderr, "github:pr list batch with checks failed, retrying without checks");
            match run_pr_list(None, &args, PR_LIST_FIELDS) {
                Ok(retry) => retry,
                Err(e) => return Err(e).context("Failed to execute gh command"),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!("github:gh CLI not found, skipping PR lookup");
            return Ok(HashMap::new());
        }
        Err(e) => return Err(e).context("Failed to execute gh command"),
    };

    if !output.status.success() {
        debug!("github:pr list batch failed, treating as no PRs found");
        return Ok(HashMap::new());
    }

    let json_str = String::from_utf8(output.stdout).context("gh output is not valid UTF-8")?;

    let prs: Vec<PrBatchItem> =
        serde_json::from_str(&json_str).context("Failed to parse gh JSON output")?;

    let pr_map = prs
        .into_iter()
        .map(|pr| {
            (pr.head_ref_name, {
                let (checks, check_meta) = aggregate_checks(&pr.status_check_rollup);
                PrSummary {
                    number: pr.number,
                    title: pr.title,
                    state: pr.state,
                    is_draft: pr.is_draft,
                    checks,
                    check_meta,
                    url: Some(pr.url),
                }
            })
        })
        .collect();

    Ok(pr_map)
}

/// Fetch PR status for specific branches using a single GraphQL query.
/// Falls back to per-branch REST calls if GraphQL fails.
pub fn list_prs_for_branches(
    repo_root: &Path,
    branches: &[String],
) -> Result<HashMap<String, PrSummary>> {
    if branches.is_empty() {
        return Ok(HashMap::new());
    }

    match list_prs_for_branches_graphql(repo_root, branches) {
        Ok(map) => Ok(map),
        Err(e) => {
            debug!("github:graphql batch failed, falling back to per-branch REST: {e}");
            list_prs_for_branches_rest(repo_root, branches)
        }
    }
}

/// Sanitize a branch name into a valid GraphQL alias (alphanumeric + underscore).
fn branch_to_alias(index: usize, branch: &str) -> String {
    // Use a prefix + index to guarantee uniqueness, since sanitizing could create collisions
    let sanitized: String = branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("br{}_{}", index, sanitized)
}

/// Build a GraphQL query fragment for a single branch alias.
fn build_branch_fragment(alias: &str, branch: &str) -> String {
    // Escape any quotes in branch name for safety
    let escaped = branch.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"    {alias}: pullRequests(headRefName: "{escaped}", first: 1, states: [OPEN, MERGED, CLOSED], orderBy: {{field: CREATED_AT, direction: DESC}}) {{
      nodes {{
        number title state isDraft headRefName url
        commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ contexts(first: 100) {{
          nodes {{ __typename ... on CheckRun {{ name status conclusion startedAt }} ... on StatusContext {{ context state createdAt }} }}
        }} }} }} }} }}
      }}
    }}"#
    )
}

/// GraphQL response structures
#[derive(Debug, Deserialize)]
struct GraphqlResponse {
    data: Option<GraphqlData>,
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
}

/// The `data.repository` value is a map of alias -> PullRequestConnection
#[derive(Debug, Deserialize)]
struct GraphqlData {
    repository: HashMap<String, GraphqlPrConnection>,
}

#[derive(Debug, Deserialize)]
struct GraphqlPrConnection {
    nodes: Vec<GraphqlPrNode>,
}

#[derive(Debug, Deserialize)]
struct GraphqlPrNode {
    number: u32,
    title: String,
    state: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    url: String,
    commits: GraphqlCommits,
}

#[derive(Debug, Deserialize)]
struct GraphqlCommits {
    nodes: Vec<GraphqlCommitNode>,
}

#[derive(Debug, Deserialize)]
struct GraphqlCommitNode {
    commit: GraphqlCommit,
}

#[derive(Debug, Deserialize)]
struct GraphqlCommit {
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<GraphqlCheckRollup>,
}

#[derive(Debug, Deserialize)]
struct GraphqlCheckRollup {
    contexts: GraphqlCheckContexts,
}

#[derive(Debug, Deserialize)]
struct GraphqlCheckContexts {
    nodes: Vec<GraphqlCheckNode>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum GraphqlCheckNode {
    CheckRun {
        name: Option<String>,
        status: Option<String>,
        conclusion: Option<String>,
        #[serde(rename = "startedAt")]
        started_at: Option<String>,
    },
    StatusContext {
        context: Option<String>,
        state: Option<String>,
        #[serde(rename = "createdAt")]
        created_at: Option<String>,
    },
}

impl GraphqlCheckNode {
    fn to_rollup_item(&self) -> CheckRollupItem {
        match self {
            GraphqlCheckNode::CheckRun {
                name,
                status,
                conclusion,
                started_at,
            } => CheckRollupItem {
                status: status.clone(),
                conclusion: conclusion.clone(),
                name: name.clone(),
                started_at: started_at.clone(),
            },
            GraphqlCheckNode::StatusContext {
                context,
                state,
                created_at,
            } => CheckRollupItem {
                status: state.clone(),
                conclusion: None,
                name: context.clone(),
                started_at: created_at.clone(),
            },
        }
    }
}

/// Repository context resolved by `gh`, matching its own repo detection logic
/// (respects `gh repo set-default`, fork conventions, GHES hosts).
#[derive(Debug, Deserialize)]
struct RepoContext {
    name: String,
    owner: RepositoryOwner,
    url: String,
}

/// Get the repo owner, name, and API hostname using `gh repo view`.
/// This delegates repo resolution to `gh` so it works correctly with forks,
/// `gh repo set-default`, and GitHub Enterprise.
fn get_repo_context(repo_root: &Path) -> Result<(String, String, String)> {
    let output = Command::new("gh")
        .current_dir(repo_root)
        .args(["repo", "view", "--json", "owner,name,url"])
        .output()
        .context("Failed to run gh repo view")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("gh repo view failed: {stderr}"));
    }

    let ctx: RepoContext =
        serde_json::from_slice(&output.stdout).context("Failed to parse gh repo view output")?;

    // Extract hostname from the repo URL for GHES support
    let hostname = ctx
        .url
        .strip_prefix("https://")
        .or_else(|| ctx.url.strip_prefix("http://"))
        .and_then(|s| s.split('/').next())
        .unwrap_or("github.com")
        .to_string();

    Ok((ctx.owner.login, ctx.name, hostname))
}

/// Fetch PR status for multiple branches in a single GraphQL API call.
fn list_prs_for_branches_graphql(
    repo_root: &Path,
    branches: &[String],
) -> Result<HashMap<String, PrSummary>> {
    let (owner, repo_name, hostname) = get_repo_context(repo_root)?;

    // Build query fragments with one alias per branch
    let fragments: Vec<String> = branches
        .iter()
        .enumerate()
        .map(|(i, branch)| {
            let alias = branch_to_alias(i, branch);
            build_branch_fragment(&alias, branch)
        })
        .collect();

    // Use GraphQL variables for owner/name to avoid injection from crafted repo names
    let query = format!(
        "query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{\n{}\n  }} }}",
        fragments.join("\n")
    );

    let body = serde_json::to_vec(&serde_json::json!({
        "query": query,
        "variables": {
            "owner": owner,
            "name": repo_name,
        }
    }))
    .context("JSON serialize")?;

    let mut child = Command::new("gh")
        .current_dir(repo_root)
        .args(["api", "graphql", "--hostname", &hostname, "--input", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn gh api graphql")?;

    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(&body)
        .context("Failed to write to gh stdin")?;

    let output = child
        .wait_with_output()
        .context("Failed to wait for gh api graphql")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("gh api graphql failed: {stderr}"));
    }

    let response: GraphqlResponse =
        serde_json::from_slice(&output.stdout).context("Failed to parse GraphQL response")?;

    if let Some(errors) = &response.errors
        && !errors.is_empty()
    {
        let msgs: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
        return Err(anyhow!("GraphQL errors: {}", msgs.join("; ")));
    }

    let data = response
        .data
        .ok_or_else(|| anyhow!("No data in GraphQL response"))?;
    let repo = data.repository;

    let mut map = HashMap::new();
    for (_alias, connection) in repo {
        for node in connection.nodes {
            let check_items: Vec<CheckRollupItem> = node
                .commits
                .nodes
                .first()
                .and_then(|c| c.commit.status_check_rollup.as_ref())
                .map(|rollup| {
                    rollup
                        .contexts
                        .nodes
                        .iter()
                        .map(|n| n.to_rollup_item())
                        .collect()
                })
                .unwrap_or_default();

            map.insert(node.head_ref_name, {
                let (checks, check_meta) = aggregate_checks(&check_items);
                PrSummary {
                    number: node.number,
                    title: node.title,
                    state: node.state,
                    is_draft: node.is_draft,
                    checks,
                    check_meta,
                    url: Some(node.url),
                }
            });
        }
    }

    Ok(map)
}

/// Fallback: fetch PR status one branch at a time using REST-style gh pr list.
fn list_prs_for_branches_rest(
    repo_root: &Path,
    branches: &[String],
) -> Result<HashMap<String, PrSummary>> {
    let mut map = HashMap::new();

    for branch in branches {
        let args = [
            "pr", "list", "--head", branch, "--state", "all", "--limit", "1",
        ];
        let output = match run_pr_list(Some(repo_root), &args, PR_LIST_FIELDS_WITH_CHECKS) {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                debug!(branch = branch, stderr = %stderr, "github:branch pr list with checks failed, retrying without checks");
                match run_pr_list(Some(repo_root), &args, PR_LIST_FIELDS) {
                    Ok(output) => output,
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        };

        if !output.status.success() {
            continue;
        }

        let prs: Vec<PrBatchItem> = match serde_json::from_slice(&output.stdout) {
            Ok(prs) => prs,
            Err(_) => continue,
        };

        if let Some(pr) = prs.into_iter().next() {
            let (checks, check_meta) = aggregate_checks(&pr.status_check_rollup);
            map.insert(
                pr.head_ref_name,
                PrSummary {
                    number: pr.number,
                    title: pr.title,
                    state: pr.state,
                    is_draft: pr.is_draft,
                    checks,
                    check_meta,
                    url: Some(pr.url),
                },
            );
        }
    }

    Ok(map)
}

/// Get the path to the PR status cache file
fn get_pr_cache_path() -> Result<PathBuf> {
    let cache_dir = crate::xdg::cache_dir()?;
    std::fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir.join("pr_status_cache.json"))
}

/// Load the PR status cache from disk
pub fn load_pr_cache() -> HashMap<PathBuf, HashMap<String, PrSummary>> {
    if let Ok(path) = get_pr_cache_path()
        && path.exists()
        && let Ok(content) = std::fs::read_to_string(&path)
    {
        return serde_json::from_str(&content).unwrap_or_default();
    }
    HashMap::new()
}

/// Save the PR status cache to disk
pub fn save_pr_cache(statuses: &HashMap<PathBuf, HashMap<String, PrSummary>>) {
    let Ok(path) = get_pr_cache_path() else {
        return;
    };
    let lock_path = path.with_extension("json.lock");
    let lock_file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(file) => file,
        Err(_) => return,
    };
    let Ok(_lock) = Flock::lock(lock_file, FlockArg::LockExclusive).map_err(|(_file, errno)| errno)
    else {
        return;
    };

    let mut merged = load_pr_cache();
    for (repo, prs) in statuses {
        if prs.is_empty() {
            merged.remove(repo);
        } else {
            merged.insert(repo.clone(), prs.clone());
        }
    }
    let Ok(content) = serde_json::to_string(&merged) else {
        return;
    };
    let _ = write_atomic(&path, content.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_item(status: Option<&str>, conclusion: Option<&str>) -> CheckRollupItem {
        CheckRollupItem {
            status: status.map(String::from),
            conclusion: conclusion.map(String::from),
            name: None,
            started_at: None,
        }
    }

    #[test]
    fn aggregate_checks_empty() {
        assert_eq!(aggregate_checks(&[]).0, None);
    }

    #[test]
    fn aggregate_checks_all_success() {
        let checks = vec![
            check_item(Some("COMPLETED"), Some("SUCCESS")),
            check_item(Some("COMPLETED"), Some("SUCCESS")),
        ];
        assert_eq!(aggregate_checks(&checks).0, Some(CheckState::Success));
    }

    #[test]
    fn aggregate_checks_with_failure() {
        let checks = vec![
            check_item(Some("COMPLETED"), Some("SUCCESS")),
            check_item(Some("COMPLETED"), Some("FAILURE")),
        ];
        assert_eq!(
            aggregate_checks(&checks).0,
            Some(CheckState::Failure {
                passed: 1,
                total: 2
            })
        );
    }

    #[test]
    fn aggregate_checks_with_pending() {
        let checks = vec![
            check_item(Some("COMPLETED"), Some("SUCCESS")),
            check_item(Some("IN_PROGRESS"), None),
        ];
        assert_eq!(
            aggregate_checks(&checks).0,
            Some(CheckState::Pending {
                passed: 1,
                total: 2
            })
        );
    }

    #[test]
    fn aggregate_checks_failure_takes_priority_over_pending() {
        let checks = vec![
            check_item(Some("COMPLETED"), Some("SUCCESS")),
            check_item(Some("COMPLETED"), Some("FAILURE")),
            check_item(Some("IN_PROGRESS"), None),
        ];
        assert_eq!(
            aggregate_checks(&checks).0,
            Some(CheckState::Failure {
                passed: 1,
                total: 3
            })
        );
    }

    #[test]
    fn aggregate_checks_status_context_success() {
        // StatusContext uses "state" field (aliased to status) with values like SUCCESS
        let checks = vec![check_item(Some("SUCCESS"), None)];
        assert_eq!(aggregate_checks(&checks).0, Some(CheckState::Success));
    }

    #[test]
    fn aggregate_checks_status_context_pending() {
        let checks = vec![check_item(Some("PENDING"), None)];
        assert_eq!(
            aggregate_checks(&checks).0,
            Some(CheckState::Pending {
                passed: 0,
                total: 1
            })
        );
    }

    #[test]
    fn aggregate_checks_status_context_error() {
        let checks = vec![check_item(Some("ERROR"), None)];
        assert_eq!(
            aggregate_checks(&checks).0,
            Some(CheckState::Failure {
                passed: 0,
                total: 1
            })
        );
    }

    #[test]
    fn aggregate_checks_all_skipped_returns_success() {
        let checks = vec![
            check_item(Some("COMPLETED"), Some("SKIPPED")),
            check_item(Some("COMPLETED"), Some("NEUTRAL")),
        ];
        assert_eq!(aggregate_checks(&checks).0, Some(CheckState::Success));
    }

    #[test]
    fn aggregate_checks_skipped_not_counted_in_total() {
        let checks = vec![
            check_item(Some("COMPLETED"), Some("SUCCESS")),
            check_item(Some("COMPLETED"), Some("SKIPPED")),
            check_item(Some("IN_PROGRESS"), None),
        ];
        // Only SUCCESS and IN_PROGRESS count toward total (2), not SKIPPED
        assert_eq!(
            aggregate_checks(&checks).0,
            Some(CheckState::Pending {
                passed: 1,
                total: 2
            })
        );
    }

    #[test]
    fn aggregate_checks_cancelled_is_failure() {
        let checks = vec![check_item(Some("COMPLETED"), Some("CANCELLED"))];
        assert_eq!(
            aggregate_checks(&checks).0,
            Some(CheckState::Failure {
                passed: 0,
                total: 1
            })
        );
    }

    #[test]
    fn aggregate_checks_timed_out_is_failure() {
        let checks = vec![check_item(Some("COMPLETED"), Some("TIMED_OUT"))];
        assert_eq!(
            aggregate_checks(&checks).0,
            Some(CheckState::Failure {
                passed: 0,
                total: 1
            })
        );
    }

    #[test]
    fn aggregate_checks_mixed_check_types() {
        // Mix of CheckRun (status/conclusion) and StatusContext (state only)
        let checks = vec![
            check_item(Some("COMPLETED"), Some("SUCCESS")), // CheckRun success
            check_item(Some("IN_PROGRESS"), None),          // CheckRun pending
            check_item(Some("SUCCESS"), None),              // StatusContext success
        ];
        assert_eq!(
            aggregate_checks(&checks).0,
            Some(CheckState::Pending {
                passed: 2,
                total: 3
            })
        );
    }

    #[test]
    fn aggregate_checks_queued_is_pending() {
        let checks = vec![check_item(Some("QUEUED"), None)];
        assert_eq!(
            aggregate_checks(&checks).0,
            Some(CheckState::Pending {
                passed: 0,
                total: 1
            })
        );
    }

    #[test]
    fn aggregate_checks_waiting_is_pending() {
        let checks = vec![check_item(Some("WAITING"), None)];
        assert_eq!(
            aggregate_checks(&checks).0,
            Some(CheckState::Pending {
                passed: 0,
                total: 1
            })
        );
    }

    #[test]
    fn branch_to_alias_sanitizes_hyphens() {
        let alias = branch_to_alias(0, "my-feature-branch");
        assert_eq!(alias, "br0_my_feature_branch");
    }

    #[test]
    fn branch_to_alias_sanitizes_slashes() {
        let alias = branch_to_alias(3, "feat/add-thing");
        assert_eq!(alias, "br3_feat_add_thing");
    }

    #[test]
    fn branch_to_alias_index_prevents_collisions() {
        // "a-b" and "a_b" would collide without the index prefix
        let a1 = branch_to_alias(0, "a-b");
        let a2 = branch_to_alias(1, "a_b");
        assert_ne!(a1, a2);
    }

    #[test]
    fn graphql_check_node_to_rollup_item_check_run() {
        let node = GraphqlCheckNode::CheckRun {
            name: Some("build".to_string()),
            status: Some("COMPLETED".to_string()),
            conclusion: Some("SUCCESS".to_string()),
            started_at: Some("2026-03-24T14:00:00Z".to_string()),
        };
        let item = node.to_rollup_item();
        assert_eq!(item.status.as_deref(), Some("COMPLETED"));
        assert_eq!(item.conclusion.as_deref(), Some("SUCCESS"));
        assert_eq!(item.name.as_deref(), Some("build"));
        assert_eq!(item.started_at.as_deref(), Some("2026-03-24T14:00:00Z"));
    }

    #[test]
    fn graphql_check_node_to_rollup_item_status_context() {
        let node = GraphqlCheckNode::StatusContext {
            context: Some("ci/circleci".to_string()),
            state: Some("PENDING".to_string()),
            created_at: Some("2026-03-24T14:00:00Z".to_string()),
        };
        let item = node.to_rollup_item();
        assert_eq!(item.status.as_deref(), Some("PENDING"));
        assert_eq!(item.conclusion, None);
        assert_eq!(item.name.as_deref(), Some("ci/circleci"));
        assert_eq!(item.started_at.as_deref(), Some("2026-03-24T14:00:00Z"));
    }

    #[test]
    fn parse_github_timestamp_valid() {
        assert_eq!(
            parse_github_timestamp("2026-03-24T14:02:00Z"),
            Some(1774360920)
        );
        // Unix epoch
        assert_eq!(parse_github_timestamp("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parse_github_timestamp_invalid() {
        assert_eq!(parse_github_timestamp(""), None);
        assert_eq!(parse_github_timestamp("not a date"), None);
        assert_eq!(parse_github_timestamp("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn aggregate_checks_captures_failing_name() {
        let checks = vec![
            CheckRollupItem {
                status: Some("COMPLETED".into()),
                conclusion: Some("SUCCESS".into()),
                name: Some("build".into()),
                started_at: None,
            },
            CheckRollupItem {
                status: Some("COMPLETED".into()),
                conclusion: Some("FAILURE".into()),
                name: Some("lint-check".into()),
                started_at: None,
            },
        ];
        let (state, meta) = aggregate_checks(&checks);
        assert_eq!(
            state,
            Some(CheckState::Failure {
                passed: 1,
                total: 2
            })
        );
        assert_eq!(
            meta.as_ref().and_then(|m| m.failing_name.as_deref()),
            Some("lint-check")
        );
    }

    #[test]
    fn aggregate_checks_captures_pending_started_at() {
        let checks = vec![
            CheckRollupItem {
                status: Some("COMPLETED".into()),
                conclusion: Some("SUCCESS".into()),
                name: Some("build".into()),
                started_at: Some("2026-03-24T14:00:00Z".into()),
            },
            CheckRollupItem {
                status: Some("IN_PROGRESS".into()),
                conclusion: None,
                name: Some("test".into()),
                started_at: Some("2026-03-24T14:05:00Z".into()),
            },
        ];
        let (_state, meta) = aggregate_checks(&checks);
        let meta = meta.unwrap();
        // started_at should be the pending check's time (2026-03-24T14:05:00Z)
        assert_eq!(meta.started_at, Some(1774361100));
    }
}
