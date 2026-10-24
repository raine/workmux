use std::borrow::Cow;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use serde::Serialize;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, object::Columns},
};

use crate::git;
use crate::multiplexer::{AgentPane, AgentStatus, create_backend, detect_backend_strict};
use crate::remote::{self, cache::WorkspaceRecord};
use crate::state::StateStore;
use crate::state::store::ReconciledAgentReport;
use crate::util;
use crate::workflow;

#[derive(Serialize)]
struct StatusEntry {
    worktree: String,
    branch: String,
    project: Option<String>,
    project_path: Option<PathBuf>,
    status: String,
    elapsed_secs: Option<u64>,
    title: Option<String>,
    pane_id: String,
    workdir: PathBuf,
    agent_kind: Option<String>,
    session: Option<String>,
    window_name: Option<String>,
    updated_ts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git: Option<GitInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<RemoteStatus>,
}

/// Where a remote agent runs and whether the laptop can see it.
#[derive(Serialize)]
struct RemoteStatus {
    name: String,
    host: String,
    ws_id: String,
    pane: usize,
    /// Whether a local pane relays the agent.
    attached: bool,
    connected: bool,
    /// When the remote last answered.
    last_seen: Option<u64>,
}

#[derive(Serialize)]
struct StatusContext {
    backend: String,
    instance: String,
}

#[derive(Serialize)]
struct StatusScope {
    repository: Option<PathBuf>,
    all: bool,
    targets: Vec<String>,
}

#[derive(Serialize)]
struct StatusTargetError {
    target: String,
    code: &'static str,
    message: String,
}

#[derive(Serialize)]
struct StatusOutput {
    context: StatusContext,
    scope: StatusScope,
    state_files_total: usize,
    state_files_invalid: usize,
    state_files_invalid_unattributed: usize,
    state_files_invalid_matching_context: usize,
    state_files_matching_context: usize,
    reconciled_agent_count: usize,
    agents: Vec<StatusEntry>,
    target_errors: Vec<StatusTargetError>,
}

#[derive(Serialize, Clone)]
struct GitInfo {
    has_staged: bool,
    has_unstaged: bool,
    has_unmerged_commits: bool,
}

struct StatusRow {
    project: String,
    session: String,
    worktree: String,
    status: String,
    elapsed: String,
    git: String,
    title: String,
}

impl Tabled for StatusRow {
    const LENGTH: usize = 7;

    fn fields(&self) -> Vec<Cow<'_, str>> {
        vec![
            Cow::Borrowed(&self.project),
            Cow::Borrowed(&self.session),
            Cow::Borrowed(&self.worktree),
            Cow::Borrowed(&self.status),
            Cow::Borrowed(&self.elapsed),
            Cow::Borrowed(&self.git),
            Cow::Borrowed(&self.title),
        ]
    }

    fn headers() -> Vec<Cow<'static, str>> {
        vec![
            Cow::Borrowed("PROJECT"),
            Cow::Borrowed("SESSION"),
            Cow::Borrowed("WORKTREE"),
            Cow::Borrowed("STATUS"),
            Cow::Borrowed("ELAPSED"),
            Cow::Borrowed("GIT"),
            Cow::Borrowed("TITLE"),
        ]
    }
}

fn git_label(git: &Option<GitInfo>) -> String {
    let Some(g) = git else {
        return "-".to_string();
    };
    let mut parts = Vec::new();
    if g.has_staged {
        parts.push("staged");
    }
    if g.has_unstaged {
        parts.push("unstaged");
    }
    if g.has_unmerged_commits {
        parts.push("unmerged");
    }
    if parts.is_empty() {
        "clean".to_string()
    } else {
        parts.join(",")
    }
}

/// Whether the table needs project and session columns to disambiguate rows.
/// True when the scope is not a single repository or the entries resolve to
/// more than one project.
fn spans_projects(unscoped: bool, entries: &[StatusEntry]) -> bool {
    unscoped
        || entries
            .iter()
            .any(|e| e.project_path != entries[0].project_path)
}

/// Whether every entry's session is named after its project, making the
/// session column redundant next to the project column.
fn sessions_match_projects(entries: &[StatusEntry]) -> bool {
    entries.iter().all(|e| e.session == e.project)
}

fn status_label(status: Option<AgentStatus>) -> String {
    match status {
        Some(AgentStatus::Working) => "working".to_string(),
        Some(AgentStatus::Waiting) => "waiting".to_string(),
        Some(AgentStatus::Done) => "done".to_string(),
        None => "-".to_string(),
    }
}

fn optional_name(name: &str) -> Option<String> {
    (!name.is_empty()).then(|| name.to_string())
}

fn normalized_branch(branch: String) -> String {
    if branch.is_empty() {
        "(detached)".to_string()
    } else {
        branch
    }
}

/// `project` is the name and main worktree of the agent's repository.
fn status_entry(
    agent: &AgentPane,
    worktree: String,
    branch: String,
    (project, project_path): (Option<String>, Option<PathBuf>),
    now: u64,
    git: Option<GitInfo>,
) -> StatusEntry {
    StatusEntry {
        worktree,
        branch,
        project,
        project_path,
        status: status_label(agent.status),
        elapsed_secs: agent.status_ts.map(|ts| now.saturating_sub(ts)),
        title: agent.pane_title.clone(),
        pane_id: agent.pane_id.clone(),
        workdir: agent.path.clone(),
        agent_kind: agent.agent_kind.clone(),
        session: optional_name(&agent.session),
        window_name: optional_name(&agent.window_name),
        updated_ts: agent.updated_ts,
        git,
        remote: None,
    }
}

fn remote_entry(agent: &AgentPane, record: &WorkspaceRecord, now: u64) -> StatusEntry {
    let project = record
        .project_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    StatusEntry {
        remote: agent.remote.as_ref().map(|remote| RemoteStatus {
            name: remote.at.remote.clone(),
            host: remote.host.clone(),
            ws_id: remote.at.ws_id.clone(),
            pane: remote.at.pane,
            attached: remote.attached,
            connected: remote.connected,
            last_seen: remote.last_seen,
        }),
        ..status_entry(
            agent,
            record.handle.clone(),
            record.branch.clone(),
            (project, Some(record.project_root.clone())),
            now,
            None,
        )
    }
}

/// Status entries of the agents of remote workspaces `records`, asking
/// nodes whose observation is stale.
fn remote_entries(
    records: &[WorkspaceRecord],
    attached: &[AgentPane],
    now: u64,
) -> Vec<StatusEntry> {
    if records.is_empty() {
        return Vec::new();
    }
    let config = crate::config::Config::load(None).unwrap_or_default();
    let observations = remote::agents::observations(&config, records, true);
    let hosts = remote::agents::hosts(&config);
    records
        .iter()
        .flat_map(|record| {
            remote::agents::rows(
                std::slice::from_ref(record),
                attached,
                &observations,
                &hosts,
                now,
            )
            .into_iter()
            .map(|agent| remote_entry(&agent, record, now))
            .collect::<Vec<_>>()
        })
        .collect()
}

/// Compute git info for a worktree path.
///
/// Runs git commands with the worktree's directory as the working dir,
/// so it works correctly for cross-project agents.
fn compute_git_info(wt_path: &std::path::Path, branch: &str) -> Result<GitInfo> {
    let has_staged = git::has_staged_changes(wt_path)?;
    let has_unstaged = git::has_unstaged_changes(wt_path)?;
    let main = git::get_default_branch_in(Some(wt_path))?;
    let base = git::get_merge_base_in(Some(wt_path), &main)?;
    let unmerged = git::get_unmerged_branches_in(Some(wt_path), &base)?;

    Ok(GitInfo {
        has_staged,
        has_unstaged,
        has_unmerged_commits: unmerged.contains(branch),
    })
}

pub fn run(worktrees: &[String], json: bool, all: bool, show_git: bool) -> Result<()> {
    let mux = create_backend(detect_backend_strict()?);
    let store = StateStore::open_read_only()?;
    let mut report = match store.load_reconciled_agent_report(mux.as_ref()) {
        Ok(report) => report,
        // Nodes answer for remote agents, which need no local multiplexer
        // unless a local pane relays them.
        Err(error)
            if !worktrees.is_empty()
                && worktrees
                    .iter()
                    .all(|name| remote::agents::find_target(name).is_some()) =>
        {
            tracing::debug!(%error, "status: no local agents");
            ReconciledAgentReport {
                backend: mux.name().to_string(),
                ..Default::default()
            }
        }
        Err(error) => return Err(error),
    };
    let agent_panes = std::mem::take(&mut report.agents);
    let attached = std::mem::take(&mut report.remote_agents);
    let invalid_state_failures =
        report.state_files_invalid_matching_context + report.state_files_invalid_unattributed;
    if invalid_state_failures > 0 {
        return Err(anyhow!(
            "{} invalid agent state file(s) for {} instance {} or with unattributable filenames; inspect the workmux agents state directory",
            invalid_state_failures,
            report.backend,
            report.instance
        ));
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let reconciled_agent_count = agent_panes.len();
    let mut entries: Vec<StatusEntry> = Vec::new();
    let mut target_errors = Vec::new();
    let mut remote_records = Vec::new();
    let repository;

    if worktrees.is_empty() {
        if !all && git::get_repo_root_if_present()?.is_some() {
            let all_worktrees = git::list_worktrees()?;
            let main_root = git::get_main_worktree_root()?;
            remote_records = remote::cache::for_project(&main_root);
            repository = Some(main_root);
            let has_scoped_agents = all_worktrees.iter().any(|(wt_path, _)| {
                !workflow::match_agents_to_worktree(&agent_panes, wt_path).is_empty()
            });
            let unmerged_branches = if show_git && has_scoped_agents {
                let main = git::get_default_branch()?;
                let base = git::get_merge_base(&main)?;
                git::get_unmerged_branches(&base)?
            } else {
                std::collections::HashSet::new()
            };

            for (wt_path, branch) in &all_worktrees {
                let matching = workflow::match_agents_to_worktree(&agent_panes, wt_path);
                if matching.is_empty() {
                    continue;
                }
                let worktree_name = wt_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let git_info = if show_git {
                    Some(GitInfo {
                        has_staged: git::has_staged_changes(wt_path)?,
                        has_unstaged: git::has_unstaged_changes(wt_path)?,
                        has_unmerged_commits: unmerged_branches.contains(branch),
                    })
                } else {
                    None
                };

                entries.extend(matching.into_iter().map(|agent| {
                    status_entry(
                        agent,
                        worktree_name.clone(),
                        branch.clone(),
                        git::project_identity(&agent.path),
                        now,
                        git_info.clone(),
                    )
                }));
            }
        } else {
            repository = None;
            remote_records = remote::cache::all();
            for agent in &agent_panes {
                let worktree_path =
                    workflow::find_worktree_root(&agent.path).unwrap_or_else(|| agent.path.clone());
                let worktree_name = worktree_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let branch = match git::get_branch_for_worktree(&worktree_path) {
                    Ok(branch) => normalized_branch(branch),
                    Err(error) if show_git => return Err(error),
                    Err(_) => worktree_name.clone(),
                };
                let git_info = if show_git {
                    Some(compute_git_info(&worktree_path, &branch)?)
                } else {
                    None
                };
                entries.push(status_entry(
                    agent,
                    worktree_name,
                    branch,
                    git::project_identity(&agent.path),
                    now,
                    git_info,
                ));
            }
        }
    } else {
        repository = if git::get_repo_root_if_present()?.is_some() {
            Some(git::get_main_worktree_root()?)
        } else {
            None
        };
        for name in worktrees {
            if let Some(record) = remote::agents::find_target(name) {
                remote_records.push(record);
                continue;
            }
            let (wt_path, matching) =
                match workflow::resolve_worktree_agents_from_snapshot(name, &agent_panes) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        target_errors.push(StatusTargetError {
                            target: name.clone(),
                            code: "target_resolution_failed",
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
            if matching.is_empty() {
                continue;
            }
            let worktree_name = wt_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
                .to_string();
            let branch = match git::get_branch_for_worktree(&wt_path) {
                Ok(branch) => branch,
                Err(error) if show_git => return Err(error),
                Err(_) => worktree_name.clone(),
            };
            let git_info = if show_git {
                Some(compute_git_info(&wt_path, &branch)?)
            } else {
                None
            };

            entries.extend(matching.iter().map(|agent| {
                status_entry(
                    agent,
                    worktree_name.clone(),
                    branch.clone(),
                    git::project_identity(&agent.path),
                    now,
                    git_info.clone(),
                )
            }));
        }
    }

    entries.extend(remote_entries(&remote_records, &attached, now));

    let target_failure_count = target_errors.len();
    if json {
        let output = StatusOutput {
            context: StatusContext {
                backend: report.backend,
                instance: report.instance,
            },
            scope: StatusScope {
                repository,
                all,
                targets: worktrees.to_vec(),
            },
            state_files_total: report.state_files_total,
            state_files_invalid: report.state_files_invalid,
            state_files_invalid_unattributed: report.state_files_invalid_unattributed,
            state_files_invalid_matching_context: report.state_files_invalid_matching_context,
            state_files_matching_context: report.state_files_matching_context,
            reconciled_agent_count,
            agents: entries,
            target_errors,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        for error in &target_errors {
            eprintln!("{}: {}", error.target, error.message);
        }

        if entries.is_empty() && target_errors.is_empty() {
            if report.state_files_total > 0 && report.state_files_matching_context == 0 {
                eprintln!(
                    "{} agent state file(s) exist, but none match {} instance {}",
                    report.state_files_total, report.backend, report.instance
                );
            } else if reconciled_agent_count > 0 {
                eprintln!(
                    "{reconciled_agent_count} tracked agent(s) exist outside the requested scope"
                );
            }
            println!("No active agents");
        } else if !entries.is_empty() {
            let rows: Vec<StatusRow> = entries
                .iter()
                .map(|e| {
                    let name = match &e.remote {
                        Some(remote) => remote::agents::label(&e.worktree, &remote.name),
                        None => e.worktree.clone(),
                    };
                    let worktree = if e.branch != e.worktree {
                        format!("{name} ({})", e.branch)
                    } else {
                        name
                    };
                    let status = match &e.remote {
                        Some(remote) if !remote.connected => {
                            format!("{} (disconnected)", e.status)
                        }
                        _ => e.status.clone(),
                    };
                    StatusRow {
                        project: e.project.clone().unwrap_or("-".to_string()),
                        session: e.session.clone().unwrap_or("-".to_string()),
                        worktree,
                        status,
                        elapsed: e
                            .elapsed_secs
                            .map(util::format_elapsed_secs)
                            .unwrap_or("-".to_string()),
                        git: git_label(&e.git),
                        title: e.title.clone().unwrap_or("-".to_string()),
                    }
                })
                .collect();

            let mut table = Table::new(rows);
            table
                .with(Style::blank())
                .modify(Columns::new(..), Padding::new(0, 1, 0, 0));
            let mut hidden_columns = Vec::new();
            if !spans_projects(repository.is_none(), &entries) {
                hidden_columns.extend(["PROJECT", "SESSION"]);
            } else if sessions_match_projects(&entries) {
                hidden_columns.push("SESSION");
            }
            if !show_git {
                hidden_columns.push("GIT");
            }
            for column in hidden_columns {
                table.with(tabled::settings::Remove::column(
                    tabled::settings::location::ByColumnName::new(column),
                ));
            }
            println!("{table}");
        }
    }

    if target_failure_count > 0 {
        return Err(anyhow!(
            "failed to resolve {target_failure_count} status target(s)"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_info_fails_for_non_repository_path() {
        let dir = tempfile::tempdir().unwrap();

        assert!(compute_git_info(dir.path(), "feature").is_err());
    }

    fn entry(project_path: &str) -> StatusEntry {
        StatusEntry {
            worktree: "wt".to_string(),
            branch: "wt".to_string(),
            project: None,
            project_path: Some(PathBuf::from(project_path)),
            status: "done".to_string(),
            elapsed_secs: None,
            title: None,
            pane_id: "%1".to_string(),
            workdir: PathBuf::from(project_path),
            agent_kind: None,
            session: None,
            window_name: None,
            updated_ts: None,
            git: None,
            remote: None,
        }
    }

    #[test]
    fn spans_projects_only_when_unscoped_or_mixed() {
        let same = [entry("/a"), entry("/a")];
        let mixed = [entry("/a"), entry("/b")];

        assert!(!spans_projects(false, &same));
        assert!(spans_projects(true, &same));
        assert!(spans_projects(false, &mixed));
    }

    #[test]
    fn sessions_match_projects_requires_every_entry_to_match() {
        let named = |project: &str, session: &str| StatusEntry {
            project: Some(project.to_string()),
            session: Some(session.to_string()),
            ..entry(project)
        };

        assert!(sessions_match_projects(&[named("a", "a"), named("b", "b")]));
        assert!(!sessions_match_projects(&[
            named("a", "a"),
            named("b", "generic")
        ]));
    }
}
