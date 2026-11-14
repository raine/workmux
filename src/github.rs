use anyhow::{Context, Result, anyhow};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use tracing::debug;

use crate::util::write_atomic;

#[derive(Debug, Deserialize)]
pub struct PrDetails {
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    #[serde(rename = "baseRefName")]
    pub base_ref_name: String,
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

/// An issue GitHub reports as closed by a pull request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClosingIssue {
    pub number: u32,
    /// Repository name with owner, for example `raine/workmux`.
    pub repository: String,
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
    /// Issues GitHub reports as closed by this pull request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closing_issues: Vec<ClosingIssue>,
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

/// Aggregated checks for a GitHub commit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckSummary {
    pub state: CheckState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<CheckMeta>,
}

impl CheckSummary {
    pub fn should_display_for_branch(&self, branch: &str) -> bool {
        !matches!(branch, "main" | "master") || matches!(self.state, CheckState::Failure { .. })
    }
}

/// GitHub state associated with a branch.
#[derive(Debug, Clone, PartialEq)]
pub struct BranchSummary {
    pub pr: Option<PrSummary>,
    pub checks: Option<CheckSummary>,
}

/// One local repository's branches requested by the sidebar GitHub worker.
pub struct BranchQueryRequest {
    pub repo_key: PathBuf,
    pub repo_root: PathBuf,
    pub branches: Vec<String>,
}

/// Fresh branch results, plus the complete active branch set for cache merging.
pub struct BranchQueryOutcome {
    pub requested: HashSet<String>,
    pub answered: HashMap<String, BranchSummary>,
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

fn summarize_checks(checks: &[CheckRollupItem]) -> Option<CheckSummary> {
    let (state, meta) = aggregate_checks(checks);
    state.map(|state| CheckSummary { state, meta })
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
        closing_issues: Vec::new(),
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
            "headRefName,baseRefName,headRepositoryOwner,state,isDraft,title,author",
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

const PR_LIST_FIELDS_WITH_ISSUES_AND_CHECKS: &str =
    "number,title,state,isDraft,headRefName,url,statusCheckRollup,closingIssuesReferences";
const PR_LIST_FIELDS_WITH_CHECKS: &str =
    "number,title,state,isDraft,headRefName,url,statusCheckRollup";
const PR_LIST_FIELDS: &str = "number,title,state,isDraft,headRefName,url";
const DASHBOARD_PR_LIST_FIELD_SETS: [&str; 3] = [
    PR_LIST_FIELDS_WITH_ISSUES_AND_CHECKS,
    PR_LIST_FIELDS_WITH_CHECKS,
    PR_LIST_FIELDS,
];

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

fn run_pr_list_with_fallback(
    field_sets: &[&str],
    mut run: impl FnMut(&str) -> std::io::Result<std::process::Output>,
) -> std::io::Result<std::process::Output> {
    let mut last_output = None;
    for fields in field_sets {
        let output = run(fields)?;
        if output.status.success() {
            return Ok(output);
        }
        debug!(fields, stderr = %String::from_utf8_lossy(&output.stderr), "github:pr list failed, retrying with fewer JSON fields");
        last_output = Some(output);
    }
    Ok(last_output.expect("PR field fallback requires at least one field set"))
}

#[derive(Debug, Deserialize)]
struct RawClosingIssue {
    number: u32,
    repository: RawIssueRepository,
}

#[derive(Debug, Deserialize)]
struct RawIssueRepository {
    name: String,
    owner: RepositoryOwner,
}

impl From<RawClosingIssue> for ClosingIssue {
    fn from(issue: RawClosingIssue) -> Self {
        Self {
            number: issue.number,
            repository: format!("{}/{}", issue.repository.owner.login, issue.repository.name),
        }
    }
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
    #[serde(rename = "closingIssuesReferences", default)]
    closing_issues: Vec<RawClosingIssue>,
}

/// Fetch all PRs for the repository containing `repo_root`.
pub fn list_prs_in(repo_root: Option<&Path>) -> Result<HashMap<String, PrSummary>> {
    let args = ["pr", "list", "--state", "all", "--limit", "200"];
    let output = run_pr_list(repo_root, &args, PR_LIST_FIELDS_WITH_CHECKS);

    let output = match output {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            debug!(stderr = %stderr, "github:pr list batch with checks failed, retrying without checks");
            match run_pr_list(repo_root, &args, PR_LIST_FIELDS) {
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
                    closing_issues: Vec::new(),
                }
            })
        })
        .collect();

    Ok(pr_map)
}

/// Fetch GitHub state for specific branches using a single GraphQL query.
/// Falls back to per-branch PR calls if GraphQL fails.
pub fn list_branch_summaries(
    repo_root: &Path,
    branches: &[String],
) -> Result<HashMap<String, BranchSummary>> {
    if branches.is_empty() {
        return Ok(HashMap::new());
    }

    match list_branch_summaries_graphql(repo_root, branches) {
        Ok(map) => Ok(map),
        Err(e) => {
            debug!("github:graphql batch failed, falling back to per-branch REST: {e}");
            list_prs_for_branches_rest(repo_root, branches).map(|prs| {
                prs.into_iter()
                    .map(|(branch, pr)| {
                        let checks = pr.checks.clone().map(|state| CheckSummary {
                            state,
                            meta: pr.check_meta.clone(),
                        });
                        (
                            branch,
                            BranchSummary {
                                pr: Some(pr),
                                checks,
                            },
                        )
                    })
                    .collect()
            })
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
    #[serde(default)]
    path: Vec<serde_json::Value>,
}

fn is_closing_issues_error(error: &GraphqlError) -> bool {
    error
        .path
        .iter()
        .any(|segment| segment.as_str() == Some("closingIssuesReferences"))
}

#[derive(Debug, Deserialize)]
struct GraphqlPrConnection {
    nodes: Vec<GraphqlPrNode>,
}

#[derive(Debug, Deserialize)]
struct GraphqlBranchRef {
    target: Option<GraphqlCommit>,
}

#[derive(Debug, Deserialize)]
struct GraphqlPrNode {
    number: u32,
    title: String,
    state: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    url: String,
    commits: GraphqlCommits,
    #[serde(rename = "closingIssuesReferences", default)]
    closing_issues: Option<GraphqlClosingIssues>,
}

#[derive(Debug, Deserialize)]
struct GraphqlClosingIssues {
    nodes: Vec<RawClosingIssue>,
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

fn check_items_for_commit(commit: &GraphqlCommit) -> Vec<CheckRollupItem> {
    commit
        .status_check_rollup
        .as_ref()
        .map(|rollup| {
            rollup
                .contexts
                .nodes
                .iter()
                .map(GraphqlCheckNode::to_rollup_item)
                .collect()
        })
        .unwrap_or_default()
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

type ResolvedRepoContext = (String, String, String);
static REPO_CONTEXT_CACHE: OnceLock<Mutex<HashMap<PathBuf, ResolvedRepoContext>>> = OnceLock::new();

fn repo_context_cache_key(repo_root: &Path) -> PathBuf {
    crate::git::get_git_common_dir_in(Some(repo_root))
        .ok()
        .and_then(|path| path.canonicalize().ok().or(Some(path)))
        .unwrap_or_else(|| repo_root.to_path_buf())
}

/// Get the repo owner, name, and API hostname using `gh repo view`.
/// This delegates repo resolution to `gh` so it works correctly with forks,
/// `gh repo set-default`, and GitHub Enterprise.
fn get_repo_context(repo_root: &Path) -> Result<ResolvedRepoContext> {
    let cache_key = repo_context_cache_key(repo_root);
    let cache = REPO_CONTEXT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some(context) = cache.get(&cache_key)
    {
        return Ok(context.clone());
    }

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

    let context = (ctx.owner.login, ctx.name, hostname);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(cache_key, context.clone());
    }
    Ok(context)
}

const MAX_BATCH_BRANCHES: usize = 32;
const MAX_BATCH_BODY_BYTES: usize = 64 * 1024;

struct LocalQuery {
    repo_key: PathBuf,
    branches: BTreeSet<String>,
}

struct RemoteQuery {
    hostname: String,
    owner: String,
    name: String,
    branches: Vec<String>,
    locals: Vec<LocalQuery>,
}

type RemoteQueryOutcome = HashMap<String, BranchSummary>;

#[derive(Debug, Deserialize)]
struct BatchGraphqlResponse {
    data: Option<HashMap<String, serde_json::Value>>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

struct GraphqlCommandResponse {
    success: bool,
    response: BatchGraphqlResponse,
}

/// Fetch sidebar GitHub state with one bounded GraphQL query per hostname chunk.
pub fn list_branch_summaries_batch(
    requests: Vec<BranchQueryRequest>,
) -> HashMap<PathBuf, BranchQueryOutcome> {
    let mut remotes: BTreeMap<(String, String, String), RemoteQuery> = BTreeMap::new();

    for request in requests {
        let requested: BTreeSet<String> = request.branches.into_iter().collect();
        let Ok((owner, name, hostname)) = get_repo_context(&request.repo_root) else {
            continue;
        };
        let key = (
            hostname.to_ascii_lowercase(),
            owner.to_ascii_lowercase(),
            name.to_ascii_lowercase(),
        );
        let remote = remotes.entry(key).or_insert_with(|| RemoteQuery {
            hostname,
            owner,
            name,
            branches: Vec::new(),
            locals: Vec::new(),
        });
        remote.locals.push(LocalQuery {
            repo_key: request.repo_key,
            branches: requested.clone(),
        });
        remote.branches.extend(requested);
    }

    let mut by_host: BTreeMap<String, Vec<RemoteQuery>> = BTreeMap::new();
    for mut remote in remotes.into_values() {
        remote.branches.sort();
        remote.branches.dedup();
        by_host
            .entry(remote.hostname.to_ascii_lowercase())
            .or_default()
            .push(remote);
    }

    let mut outcomes = HashMap::new();
    for remotes in by_host.into_values() {
        for chunk in pack_remote_queries(remotes) {
            let remote_outcomes = execute_remote_chunk(&chunk);
            for (remote, remote_outcome) in chunk.into_iter().zip(remote_outcomes) {
                let Some(remote_outcome) = remote_outcome else {
                    continue;
                };
                for local in remote.locals {
                    let answered: HashMap<String, BranchSummary> = remote_outcome
                        .iter()
                        .filter(|(branch, _)| local.branches.contains(*branch))
                        .map(|(branch, summary)| (branch.clone(), summary.clone()))
                        .collect();
                    if !answered.is_empty() {
                        outcomes.insert(
                            local.repo_key,
                            BranchQueryOutcome {
                                requested: local.branches.into_iter().collect(),
                                answered,
                            },
                        );
                    }
                }
            }
        }
    }
    outcomes
}

fn pack_remote_queries(remotes: Vec<RemoteQuery>) -> Vec<Vec<RemoteQuery>> {
    let mut chunks = Vec::new();
    let mut chunk = Vec::new();
    let mut branch_count = 0usize;

    for remote in remotes {
        let remote_branches = remote.branches.len();
        chunk.push(remote);
        let candidate_bytes = build_batch_body(&chunk)
            .map(|body| body.len())
            .unwrap_or(usize::MAX);
        if chunk.len() > 1
            && (branch_count + remote_branches > MAX_BATCH_BRANCHES
                || candidate_bytes > MAX_BATCH_BODY_BYTES)
        {
            let remote = chunk.pop().expect("candidate was just pushed");
            chunks.push(std::mem::take(&mut chunk));
            branch_count = 0;
            chunk.push(remote);
        }
        branch_count += remote_branches;
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

fn build_batch_body(remotes: &[RemoteQuery]) -> Result<Vec<u8>> {
    let mut declarations = Vec::new();
    let mut repositories = Vec::new();
    let mut variables = serde_json::Map::new();

    for (repo_index, remote) in remotes.iter().enumerate() {
        let owner_var = format!("owner_{repo_index}");
        let name_var = format!("name_{repo_index}");
        declarations.push(format!("${owner_var}: String!"));
        declarations.push(format!("${name_var}: String!"));
        variables.insert(owner_var.clone(), remote.owner.clone().into());
        variables.insert(name_var.clone(), remote.name.clone().into());

        let mut branches = Vec::new();
        for (branch_index, branch) in remote.branches.iter().enumerate() {
            let head_var = format!("head_{repo_index}_{branch_index}");
            let ref_var = format!("qualified_{repo_index}_{branch_index}");
            declarations.push(format!("${head_var}: String!"));
            declarations.push(format!("${ref_var}: String!"));
            variables.insert(head_var.clone(), branch.clone().into());
            variables.insert(ref_var.clone(), format!("refs/heads/{branch}").into());
            branches.push(format!(
                r#"    pr_r{repo_index}_b{branch_index}: pullRequests(headRefName: ${head_var}, first: 1, states: [OPEN, MERGED, CLOSED], orderBy: {{field: CREATED_AT, direction: DESC}}) {{
      nodes {{
        number title state isDraft url
        closingIssuesReferences(first: 100) {{ nodes {{ number repository {{ name owner {{ login }} }} }} }}
        commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ contexts(first: 100) {{
          nodes {{ __typename ... on CheckRun {{ name status conclusion startedAt }} ... on StatusContext {{ context state createdAt }} }}
        }} }} }} }} }}
      }}
    }}
    ref_r{repo_index}_b{branch_index}: ref(qualifiedName: ${ref_var}) {{
      target {{ ... on Commit {{ statusCheckRollup {{ contexts(first: 100) {{
        nodes {{ __typename ... on CheckRun {{ name status conclusion startedAt }} ... on StatusContext {{ context state createdAt }} }}
      }} }} }} }}
    }}"#
            ));
        }
        repositories.push(format!(
            "  repo_{repo_index}: repository(owner: ${owner_var}, name: ${name_var}) {{\n{}\n  }}",
            branches.join("\n")
        ));
    }

    let query = format!(
        "query({}) {{\n{}\n}}",
        declarations.join(", "),
        repositories.join("\n")
    );
    serde_json::to_vec(&serde_json::json!({
        "query": query,
        "variables": variables,
    }))
    .context("JSON serialize")
}

fn run_graphql(hostname: &str, body: &[u8]) -> Result<GraphqlCommandResponse> {
    let mut child = Command::new("gh")
        .args(["api", "graphql", "--hostname", hostname, "--input", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn gh api graphql")?;

    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(body)
        .context("Failed to write to gh stdin")?;
    let output = child
        .wait_with_output()
        .context("Failed to wait for gh api graphql")?;

    let response = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "gh api graphql returned invalid JSON with status {}",
            output.status
        )
    })?;
    Ok(GraphqlCommandResponse {
        success: output.status.success(),
        response,
    })
}

fn execute_remote_chunk(chunk: &[RemoteQuery]) -> Vec<Option<RemoteQueryOutcome>> {
    execute_remote_chunk_with(chunk, &mut run_graphql)
}

fn execute_remote_chunk_with(
    chunk: &[RemoteQuery],
    run: &mut impl FnMut(&str, &[u8]) -> Result<GraphqlCommandResponse>,
) -> Vec<Option<RemoteQueryOutcome>> {
    match execute_remote_chunk_once(chunk, run) {
        Ok(outcomes) => outcomes,
        Err(_) if chunk.len() > 1 => chunk
            .iter()
            .map(|remote| {
                execute_remote_chunk_once(std::slice::from_ref(remote), run)
                    .ok()
                    .and_then(|mut outcomes| outcomes.pop().flatten())
            })
            .collect(),
        Err(_) => vec![None],
    }
}

fn execute_remote_chunk_once(
    chunk: &[RemoteQuery],
    run: &mut impl FnMut(&str, &[u8]) -> Result<GraphqlCommandResponse>,
) -> Result<Vec<Option<RemoteQueryOutcome>>> {
    let body = build_batch_body(chunk)?;
    let response = run(&chunk[0].hostname, &body)?.response;
    let aliases: HashSet<String> = (0..chunk.len())
        .map(|index| format!("repo_{index}"))
        .collect();
    if response.errors.iter().any(|error| {
        !is_closing_issues_error(error)
            && error
                .path
                .first()
                .and_then(serde_json::Value::as_str)
                .is_none_or(|alias| !aliases.contains(alias))
    }) {
        return Err(anyhow!("GraphQL response contained an unscoped error"));
    }
    let data = response
        .data
        .ok_or_else(|| anyhow!("No data in GraphQL response"))?;
    Ok(chunk
        .iter()
        .enumerate()
        .map(|(repo_index, remote)| parse_remote_query(&data, &response.errors, repo_index, remote))
        .collect())
}

fn parse_remote_query(
    data: &HashMap<String, serde_json::Value>,
    errors: &[GraphqlError],
    repo_index: usize,
    remote: &RemoteQuery,
) -> Option<RemoteQueryOutcome> {
    let repo_alias = format!("repo_{repo_index}");
    let repo_value = data.get(&repo_alias)?;
    if repo_value.is_null()
        || errors.iter().any(|error| {
            error.path.len() == 1
                && error.path.first().and_then(serde_json::Value::as_str)
                    == Some(repo_alias.as_str())
        })
    {
        return None;
    }
    let repo = repo_value.as_object()?;
    let mut answered = HashMap::new();

    for (branch_index, branch) in remote.branches.iter().enumerate() {
        let pr_alias = format!("pr_r{repo_index}_b{branch_index}");
        let ref_alias = format!("ref_r{repo_index}_b{branch_index}");
        let branch_error = errors.iter().any(|error| {
            !is_closing_issues_error(error)
                && error.path.len() >= 2
                && error.path.first().and_then(serde_json::Value::as_str)
                    == Some(repo_alias.as_str())
                && error
                    .path
                    .get(1)
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|alias| alias == pr_alias || alias == ref_alias)
        });
        if branch_error {
            continue;
        }
        let (Some(pr_value), Some(ref_value)) = (repo.get(&pr_alias), repo.get(&ref_alias)) else {
            continue;
        };
        if let Ok(summary) = parse_branch_summary_values(pr_value.clone(), ref_value.clone()) {
            answered.insert(branch.clone(), summary);
        }
    }

    Some(answered)
}

fn parse_branch_summary_values(
    pr_value: serde_json::Value,
    ref_value: serde_json::Value,
) -> Result<BranchSummary> {
    let connection: GraphqlPrConnection =
        serde_json::from_value(pr_value).context("Failed to parse GraphQL PR data")?;
    let branch_ref: Option<GraphqlBranchRef> =
        serde_json::from_value(ref_value).context("Failed to parse GraphQL ref data")?;
    let pr = connection.nodes.into_iter().next().map(|node| {
        let check_items = node
            .commits
            .nodes
            .first()
            .map(|node| check_items_for_commit(&node.commit))
            .unwrap_or_default();
        let (checks, check_meta) = aggregate_checks(&check_items);
        PrSummary {
            number: node.number,
            title: node.title,
            state: node.state,
            is_draft: node.is_draft,
            checks,
            check_meta,
            url: Some(node.url),
            closing_issues: node
                .closing_issues
                .map(|issues| issues.nodes.into_iter().map(Into::into).collect())
                .unwrap_or_default(),
        }
    });
    let branch_checks = branch_ref
        .and_then(|branch_ref| branch_ref.target)
        .and_then(|commit| summarize_checks(&check_items_for_commit(&commit)));
    let checks = pr
        .as_ref()
        .and_then(|pr| {
            pr.checks.clone().map(|state| CheckSummary {
                state,
                meta: pr.check_meta.clone(),
            })
        })
        .or(branch_checks);
    Ok(BranchSummary { pr, checks })
}

/// Fetch branch and PR status for multiple branches in one GraphQL API call.
fn list_branch_summaries_graphql(
    repo_root: &Path,
    branches: &[String],
) -> Result<HashMap<String, BranchSummary>> {
    let (owner, name, hostname) = get_repo_context(repo_root)?;
    let remote = RemoteQuery {
        hostname,
        owner,
        name,
        branches: branches.to_vec(),
        locals: Vec::new(),
    };
    let body = build_batch_body(std::slice::from_ref(&remote))?;
    let command = run_graphql(&remote.hostname, &body)?;
    let material_errors: Vec<_> = command
        .response
        .errors
        .iter()
        .filter(|error| !is_closing_issues_error(error))
        .collect();
    if !command.success && material_errors.is_empty() && command.response.data.is_none() {
        return Err(anyhow!("gh api graphql failed"));
    }
    if !material_errors.is_empty() {
        let messages: Vec<&str> = material_errors
            .iter()
            .map(|error| error.message.as_str())
            .collect();
        return Err(anyhow!("GraphQL errors: {}", messages.join("; ")));
    }
    let data = command
        .response
        .data
        .ok_or_else(|| anyhow!("No data in GraphQL response"))?;
    let summaries = parse_remote_query(&data, &[], 0, &remote)
        .ok_or_else(|| anyhow!("Missing repository data in GraphQL response"))?;
    if branches
        .iter()
        .any(|branch| !summaries.contains_key(branch))
    {
        return Err(anyhow!("Missing branch data in GraphQL response"));
    }
    Ok(summaries)
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
        let output = match run_pr_list_with_fallback(&DASHBOARD_PR_LIST_FIELD_SETS, |fields| {
            run_pr_list(Some(repo_root), &args, fields)
        }) {
            Ok(output) => output,
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
                    closing_issues: pr.closing_issues.into_iter().map(Into::into).collect(),
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

fn get_check_cache_path() -> Result<PathBuf> {
    let cache_dir = crate::xdg::cache_dir()?;
    std::fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir.join("check_status_cache.json"))
}

pub fn load_check_cache() -> HashMap<PathBuf, HashMap<String, CheckSummary>> {
    if let Ok(path) = get_check_cache_path()
        && path.exists()
        && let Ok(content) = std::fs::read_to_string(&path)
    {
        return serde_json::from_str(&content).unwrap_or_default();
    }
    HashMap::new()
}

pub fn save_check_cache(statuses: &HashMap<PathBuf, HashMap<String, CheckSummary>>) {
    let Ok(path) = get_check_cache_path() else {
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

    let mut merged = load_check_cache();
    for (repo, checks) in statuses {
        if checks.is_empty() {
            merged.remove(repo);
        } else {
            merged.insert(repo.clone(), checks.clone());
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
    fn main_branch_checks_only_display_failures() {
        let success = CheckSummary {
            state: CheckState::Success,
            meta: None,
        };
        let pending = CheckSummary {
            state: CheckState::Pending {
                passed: 1,
                total: 2,
            },
            meta: None,
        };
        let failure = CheckSummary {
            state: CheckState::Failure {
                passed: 1,
                total: 2,
            },
            meta: None,
        };

        assert!(!success.should_display_for_branch("main"));
        assert!(!pending.should_display_for_branch("main"));
        assert!(failure.should_display_for_branch("main"));
        assert!(!success.should_display_for_branch("master"));
        assert!(success.should_display_for_branch("feature"));
        assert!(pending.should_display_for_branch("feature"));
    }

    #[test]
    fn parses_branch_checks_without_pr() {
        let repository = serde_json::from_value(serde_json::json!({
            "pr_br0_feature": { "nodes": [] },
            "ref_br0_feature": {
                "target": {
                    "statusCheckRollup": {
                        "contexts": {
                            "nodes": [
                                {
                                    "__typename": "CheckRun",
                                    "name": "build",
                                    "status": "COMPLETED",
                                    "conclusion": "SUCCESS",
                                    "startedAt": "2026-03-24T14:00:00Z"
                                }
                            ]
                        }
                    }
                }
            }
        }))
        .unwrap();

        let mut repository: HashMap<String, serde_json::Value> = repository;
        let summary = parse_branch_summary_values(
            repository.remove("pr_br0_feature").unwrap(),
            repository.remove("ref_br0_feature").unwrap(),
        )
        .unwrap();

        assert!(summary.pr.is_none());
        assert_eq!(
            summary.checks.as_ref().map(|checks| &checks.state),
            Some(&CheckState::Success)
        );
    }

    #[test]
    fn closing_issues_match_between_graphql_and_cli_shapes() {
        let issue = serde_json::json!({
            "number": 308,
            "repository": { "name": "workmux", "owner": { "login": "raine" } }
        });
        let summary = parse_branch_summary_values(
            serde_json::json!({
                "nodes": [{
                    "number": 42,
                    "title": "Feature",
                    "state": "OPEN",
                    "isDraft": false,
                    "url": "https://github.com/raine/workmux/pull/42",
                    "closingIssuesReferences": { "nodes": [issue.clone()] },
                    "commits": { "nodes": [] }
                }]
            }),
            serde_json::Value::Null,
        )
        .unwrap();
        let cli: Vec<PrBatchItem> = serde_json::from_value(serde_json::json!([{
            "number": 42,
            "title": "Feature",
            "state": "OPEN",
            "isDraft": false,
            "headRefName": "feature",
            "url": "https://github.com/raine/workmux/pull/42",
            "closingIssuesReferences": [issue]
        }]))
        .unwrap();
        let cli_issues: Vec<ClosingIssue> = cli
            .into_iter()
            .next()
            .unwrap()
            .closing_issues
            .into_iter()
            .map(Into::into)
            .collect();

        assert_eq!(summary.pr.unwrap().closing_issues, cli_issues);
        assert_eq!(cli_issues[0].repository, "raine/workmux");
    }

    #[test]
    fn old_pr_cache_entry_defaults_closing_issues() {
        let pr: PrSummary = serde_json::from_value(serde_json::json!({
            "number": 42,
            "title": "Feature",
            "state": "OPEN",
            "isDraft": false,
            "checks": null,
            "url": "https://github.com/raine/workmux/pull/42"
        }))
        .unwrap();

        assert!(pr.closing_issues.is_empty());
    }

    #[test]
    fn pr_list_field_fallback_preserves_supported_fields() {
        let mut attempts = Vec::new();
        let output = run_pr_list_with_fallback(&DASHBOARD_PR_LIST_FIELD_SETS, |fields| {
            attempts.push(fields.to_string());
            let command = if fields.contains("closingIssuesReferences") {
                "exit 1"
            } else {
                "printf '[]'"
            };
            std::process::Command::new("sh")
                .args(["-c", command])
                .output()
        })
        .unwrap();

        assert!(output.status.success());
        assert_eq!(attempts.len(), 2);
        assert!(attempts[1].contains("statusCheckRollup"));
    }

    #[test]
    fn closing_issue_field_error_keeps_branch_summary() {
        let remote = remote_query("repo", vec!["feature".to_string()]);
        let data = HashMap::from([(
            "repo_0".to_string(),
            serde_json::json!({
                "pr_r0_b0": { "nodes": [{
                    "number": 42,
                    "title": "Feature",
                    "state": "OPEN",
                    "isDraft": false,
                    "url": "https://github.com/raine/workmux/pull/42",
                    "closingIssuesReferences": null,
                    "commits": { "nodes": [] }
                }] },
                "ref_r0_b0": null
            }),
        )]);
        let errors = vec![GraphqlError {
            message: "issues unavailable".to_string(),
            path: vec![
                "repo_0".into(),
                "pr_r0_b0".into(),
                "nodes".into(),
                0.into(),
                "closingIssuesReferences".into(),
            ],
        }];

        let outcome = parse_remote_query(&data, &errors, 0, &remote).unwrap();
        assert_eq!(outcome["feature"].pr.as_ref().unwrap().number, 42);
        assert!(
            outcome["feature"]
                .pr
                .as_ref()
                .unwrap()
                .closing_issues
                .is_empty()
        );
    }

    #[test]
    fn pr_checks_take_precedence_over_branch_checks() {
        let repository = serde_json::from_value(serde_json::json!({
            "pr_br0_feature": {
                "nodes": [{
                    "number": 42,
                    "title": "Feature",
                    "state": "OPEN",
                    "isDraft": false,
                    "url": "https://github.com/owner/repo/pull/42",
                    "commits": {
                        "nodes": [{
                            "commit": {
                                "statusCheckRollup": {
                                    "contexts": {
                                        "nodes": [{
                                            "__typename": "CheckRun",
                                            "name": "test",
                                            "status": "COMPLETED",
                                            "conclusion": "FAILURE",
                                            "startedAt": null
                                        }]
                                    }
                                }
                            }
                        }]
                    }
                }]
            },
            "ref_br0_feature": {
                "target": {
                    "statusCheckRollup": {
                        "contexts": {
                            "nodes": [{
                                "__typename": "CheckRun",
                                "name": "build",
                                "status": "COMPLETED",
                                "conclusion": "SUCCESS",
                                "startedAt": null
                            }]
                        }
                    }
                }
            }
        }))
        .unwrap();

        let mut repository: HashMap<String, serde_json::Value> = repository;
        let summary = parse_branch_summary_values(
            repository.remove("pr_br0_feature").unwrap(),
            repository.remove("ref_br0_feature").unwrap(),
        )
        .unwrap();

        assert_eq!(summary.pr.as_ref().map(|pr| pr.number), Some(42));
        assert_eq!(
            summary.checks.as_ref().map(|checks| &checks.state),
            Some(&CheckState::Failure {
                passed: 0,
                total: 1,
            })
        );
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

    fn remote_query(name: &str, branches: Vec<String>) -> RemoteQuery {
        RemoteQuery {
            hostname: "github.com".to_string(),
            owner: "owner".to_string(),
            name: name.to_string(),
            branches,
            locals: Vec::new(),
        }
    }

    #[test]
    fn batch_query_uses_variables_for_repository_and_branch_values() {
        let branch = "feat/quote-\"-line\n-unicode-雪".to_string();
        let body = build_batch_body(&[remote_query("repo", vec![branch.clone()])]).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let query = body["query"].as_str().unwrap();

        assert!(!query.contains(&branch));
        assert!(query.contains("closingIssuesReferences"));
        assert_eq!(body["variables"]["head_0_0"], branch);
        assert_eq!(
            body["variables"]["qualified_0_0"],
            "refs/heads/feat/quote-\"-line\n-unicode-雪"
        );
    }

    #[test]
    fn batch_packing_limits_total_branches() {
        let remotes = (0..33)
            .map(|index| remote_query(&format!("repo-{index}"), vec!["main".to_string()]))
            .collect();
        let chunks = pack_remote_queries(remotes);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 32);
        assert_eq!(chunks[1].len(), 1);
    }

    #[test]
    fn batch_packing_keeps_oversized_repository_atomic() {
        let branches = (0..(MAX_BATCH_BRANCHES + 8))
            .map(|index| format!("branch-{index}"))
            .collect();
        let chunks = pack_remote_queries(vec![
            remote_query("large", branches),
            remote_query("next", vec!["main".to_string()]),
        ]);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 1);
        assert_eq!(chunks[0][0].branches.len(), MAX_BATCH_BRANCHES + 8);
        assert_eq!(chunks[1][0].name, "next");
    }

    #[test]
    fn batch_packing_limits_serialized_body_size() {
        let long_branch = "x".repeat(MAX_BATCH_BODY_BYTES);
        let chunks = pack_remote_queries(vec![
            remote_query("first", vec![long_branch.clone()]),
            remote_query("second", vec![long_branch]),
        ]);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 1);
        assert_eq!(chunks[1].len(), 1);
    }

    #[test]
    fn unscoped_batch_failure_retries_each_repository_once() {
        let remotes = vec![
            remote_query("one", vec!["main".to_string()]),
            remote_query("two", vec!["main".to_string()]),
            remote_query("three", vec!["main".to_string()]),
        ];
        let mut calls = 0;
        let outcomes = execute_remote_chunk_with(&remotes, &mut |_hostname, body| {
            calls += 1;
            if calls == 1 {
                return Ok(GraphqlCommandResponse {
                    success: false,
                    response: BatchGraphqlResponse {
                        data: None,
                        errors: vec![GraphqlError {
                            message: "unscoped".to_string(),
                            path: Vec::new(),
                        }],
                    },
                });
            }
            let request: serde_json::Value = serde_json::from_slice(body).unwrap();
            assert_eq!(
                request["variables"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .filter(|key| key.starts_with("owner_"))
                    .count(),
                1
            );
            Ok(GraphqlCommandResponse {
                success: true,
                response: BatchGraphqlResponse {
                    data: Some(HashMap::from([(
                        "repo_0".to_string(),
                        serde_json::json!({
                            "pr_r0_b0": { "nodes": [] },
                            "ref_r0_b0": null
                        }),
                    )])),
                    errors: Vec::new(),
                },
            })
        });

        assert_eq!(calls, 4);
        assert_eq!(outcomes.len(), 3);
        assert!(
            outcomes
                .into_iter()
                .all(|outcome| { outcome.is_some_and(|outcome| outcome.contains_key("main")) })
        );
    }

    #[test]
    fn batch_parser_accepts_sibling_data_on_unsuccessful_command() {
        let response: BatchGraphqlResponse = serde_json::from_value(serde_json::json!({
            "data": {
                "repo_0": null,
                "repo_1": {
                    "pr_r1_b0": { "nodes": [] },
                    "ref_r1_b0": null
                }
            },
            "errors": [
                { "message": "hidden", "path": ["repo_0"] }
            ]
        }))
        .unwrap();
        let remotes = [
            remote_query("hidden", vec!["main".to_string()]),
            remote_query("visible", vec!["feature".to_string()]),
        ];
        let mut response = Some(response);
        let outcomes = execute_remote_chunk_with(&remotes, &mut |_hostname, _body| {
            Ok(GraphqlCommandResponse {
                success: false,
                response: response.take().unwrap(),
            })
        });

        assert!(outcomes[0].is_none());
        assert_eq!(
            outcomes[1].as_ref().unwrap().get("feature"),
            Some(&BranchSummary {
                pr: None,
                checks: None,
            })
        );
    }

    #[test]
    fn batch_parser_leaves_nested_failed_branch_unanswered_and_keeps_siblings() {
        let successful_commit = serde_json::json!({
            "statusCheckRollup": {
                "contexts": {
                    "nodes": [{
                        "__typename": "CheckRun",
                        "name": "test",
                        "status": "COMPLETED",
                        "conclusion": "SUCCESS",
                        "startedAt": null
                    }]
                }
            }
        });
        for failed_alias in ["pr_r0_b0", "ref_r0_b0"] {
            let failed_commit = serde_json::json!({ "statusCheckRollup": null });
            let (pr_commit, ref_commit, path) = if failed_alias == "pr_r0_b0" {
                (
                    &failed_commit,
                    &successful_commit,
                    serde_json::json!([
                        "repo_0",
                        failed_alias,
                        "nodes",
                        0,
                        "commits",
                        "nodes",
                        0,
                        "commit",
                        "statusCheckRollup"
                    ]),
                )
            } else {
                (
                    &successful_commit,
                    &failed_commit,
                    serde_json::json!(["repo_0", failed_alias, "target", "statusCheckRollup"]),
                )
            };
            let response: BatchGraphqlResponse = serde_json::from_value(serde_json::json!({
                "data": {
                    "repo_0": {
                        "pr_r0_b0": { "nodes": [{
                            "number": 42,
                            "title": "Feature",
                            "state": "OPEN",
                            "isDraft": false,
                            "url": "https://github.com/owner/repo/pull/42",
                            "commits": { "nodes": [{ "commit": pr_commit }] }
                        }] },
                        "ref_r0_b0": { "target": ref_commit },
                        "pr_r0_b1": { "nodes": [] },
                        "ref_r0_b1": null
                    },
                    "repo_1": {
                        "pr_r1_b0": { "nodes": [] },
                        "ref_r1_b0": null
                    }
                },
                "errors": [{ "message": "rollup resolver failed", "path": path }]
            }))
            .unwrap();
            let remotes = [
                remote_query("one", vec!["failed".to_string(), "healthy".to_string()]),
                remote_query("two", vec!["healthy".to_string()]),
            ];
            let mut response = Some(response);
            let outcomes = execute_remote_chunk_with(&remotes, &mut |_, _| {
                Ok(GraphqlCommandResponse {
                    success: false,
                    response: response.take().unwrap(),
                })
            });

            let first = outcomes[0].as_ref().unwrap();
            assert!(!first.contains_key("failed"), "{failed_alias}: {first:?}");
            let healthy = BranchSummary {
                pr: None,
                checks: None,
            };
            assert_eq!(first.get("healthy"), Some(&healthy));
            assert_eq!(outcomes[1].as_ref().unwrap().get("healthy"), Some(&healthy));
        }
    }

    #[test]
    fn batch_parser_leaves_directly_failed_branch_unanswered() {
        let response: BatchGraphqlResponse = serde_json::from_value(serde_json::json!({
            "data": {
                "repo_0": {
                    "pr_r0_b0": null,
                    "ref_r0_b0": null
                }
            },
            "errors": [
                { "message": "field failed", "path": ["repo_0", "pr_r0_b0"] }
            ]
        }))
        .unwrap();
        let remote = remote_query("repo", vec!["feature".to_string()]);
        let outcome =
            parse_remote_query(&response.data.unwrap(), &response.errors, 0, &remote).unwrap();

        assert!(outcome.is_empty());
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
