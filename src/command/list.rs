use std::borrow::Cow;
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::config;
use crate::config::MuxMode;
use crate::multiplexer::{AgentStatus, create_backend, detect_backend};
use crate::remote::cache::{self, Homecoming, WorkspaceRecord, WorkspaceState};
use crate::util::format_compact_age;
use crate::workflow::types::AgentStatusSummary;
use crate::{git, nerdfont, workflow};
use anyhow::Result;
use pathdiff::diff_paths;
use serde::Serialize;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, disable::Remove, location::ByColumnName, object::Columns},
};

struct WorktreeRow {
    project: String,
    branch: String,
    age: String,
    pr_status: String,
    agent_status: String,
    mux_status: String,
    unmerged_status: String,
    path_str: String,
}

impl Tabled for WorktreeRow {
    const LENGTH: usize = 8;

    fn fields(&self) -> Vec<Cow<'_, str>> {
        vec![
            Cow::Borrowed(&self.project),
            Cow::Borrowed(&self.branch),
            Cow::Borrowed(&self.age),
            Cow::Borrowed(&self.pr_status),
            Cow::Borrowed(&self.agent_status),
            Cow::Borrowed(&self.mux_status),
            Cow::Borrowed(&self.unmerged_status),
            Cow::Borrowed(&self.path_str),
        ]
    }

    fn headers() -> Vec<Cow<'static, str>> {
        vec![
            Cow::Borrowed("PROJECT"),
            Cow::Borrowed("BRANCH"),
            Cow::Borrowed("AGE"),
            Cow::Borrowed("PR"),
            Cow::Borrowed("AGENT"),
            Cow::Borrowed("MUX"),
            Cow::Borrowed("UNMERGED"),
            Cow::Borrowed("PATH"),
        ]
    }
}

fn format_pr_status(pr_info: Option<crate::github::PrSummary>) -> String {
    pr_info
        .map(|pr| {
            let icons = nerdfont::pr_icons();
            // GitHub-style colors: green for open, gray for draft, purple for merged, red for closed
            let (icon, color) = match pr.state.as_str() {
                "OPEN" if pr.is_draft => (icons.draft, "\x1b[90m"), // gray
                "OPEN" => (icons.open, "\x1b[32m"),                 // green
                "MERGED" => (icons.merged, "\x1b[35m"),             // purple/magenta
                "CLOSED" => (icons.closed, "\x1b[31m"),             // red
                _ => (icons.open, "\x1b[32m"),
            };
            format!("#{} {}{}\x1b[0m", pr.number, color, icon)
        })
        .unwrap_or_else(|| "-".to_string())
}

/// Format a single agent status as either an icon (TTY) or text label (piped).
fn format_status_label(status: AgentStatus, config: &config::Config, use_icons: bool) -> String {
    if use_icons {
        match status {
            AgentStatus::Working => config.status_icons.working().to_string(),
            AgentStatus::Waiting => config.status_icons.waiting().to_string(),
            AgentStatus::Done => config.status_icons.done().to_string(),
        }
    } else {
        match status {
            AgentStatus::Working => "working".to_string(),
            AgentStatus::Waiting => "waiting".to_string(),
            AgentStatus::Done => "done".to_string(),
        }
    }
}

fn format_agent_status(
    summary: Option<&AgentStatusSummary>,
    config: &config::Config,
    use_icons: bool,
) -> String {
    let summary = match summary {
        Some(s) if !s.statuses.is_empty() => s,
        _ => return "-".to_string(),
    };

    let total = summary.statuses.len();
    if total == 1 {
        format_status_label(summary.statuses[0], config, use_icons)
    } else {
        // Multiple agents: show breakdown
        let working = summary
            .statuses
            .iter()
            .filter(|s| matches!(s, AgentStatus::Working))
            .count();
        let waiting = summary
            .statuses
            .iter()
            .filter(|s| matches!(s, AgentStatus::Waiting))
            .count();
        let done = summary
            .statuses
            .iter()
            .filter(|s| matches!(s, AgentStatus::Done))
            .count();

        let mut parts = Vec::new();
        if working > 0 {
            let label = format_status_label(AgentStatus::Working, config, use_icons);
            parts.push(format!("{}{}", working, label));
        }
        if waiting > 0 {
            let label = format_status_label(AgentStatus::Waiting, config, use_icons);
            parts.push(format!("{}{}", waiting, label));
        }
        if done > 0 {
            let label = format_status_label(AgentStatus::Done, config, use_icons);
            parts.push(format!("{}{}", done, label));
        }
        parts.join(" ")
    }
}

#[derive(Serialize)]
struct JsonWorktree {
    project: String,
    project_path: String,
    handle: String,
    branch: String,
    path: String,
    is_main: bool,
    mode: String,
    has_uncommitted_changes: bool,
    is_open: bool,
    agent_statuses: Vec<AgentStatus>,
    created_at: Option<u64>,
    /// The remote a remote workspace lives on.
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<String>,
}

/// Remote workspaces from the local cache; listing never contacts a node.
fn remote_records(project_root: Option<&Path>, filter: &[String]) -> Vec<WorkspaceRecord> {
    match project_root {
        Some(root) => cache::for_project(root),
        None => cache::all(),
    }
    .into_iter()
    .filter(|record| {
        filter.is_empty()
            || filter
                .iter()
                .any(|token| *token == record.handle || *token == record.branch)
    })
    .collect()
}

struct ProjectWorktree {
    project: String,
    project_path: PathBuf,
    worktree: workflow::types::WorktreeInfo,
}

fn project_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

fn discover_project_roots(mux: &dyn crate::multiplexer::Multiplexer) -> Result<Vec<PathBuf>> {
    let agents = crate::state::StateStore::new()?.load_reconciled_agents(mux)?;
    let roots: BTreeSet<PathBuf> = agents
        .iter()
        .filter_map(|agent| git::get_main_worktree_root_in(Some(&agent.path)).ok())
        .map(|root| crate::util::canon_or_self(&root))
        .collect();
    Ok(roots.into_iter().collect())
}

fn list_project(
    root: &Path,
    mux: &dyn crate::multiplexer::Multiplexer,
    show_pr: bool,
    filter: &[String],
) -> Result<Vec<ProjectWorktree>> {
    let (config, _) = config::Config::load_with_location_from(root, None)?;
    let project = project_name(root);
    let worktrees = workflow::list_in(&config, mux, show_pr, filter, Some(root))?;
    Ok(worktrees
        .into_iter()
        .map(|worktree| ProjectWorktree {
            project: project.clone(),
            project_path: root.to_path_buf(),
            worktree,
        })
        .collect())
}

pub fn run(show_pr: bool, json: bool, all: bool, filter: &[String]) -> Result<()> {
    let display_config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let mut worktrees = Vec::new();
    let remotes;
    if all {
        for root in discover_project_roots(mux.as_ref())? {
            worktrees.extend(list_project(&root, mux.as_ref(), show_pr && !json, filter)?);
        }
        remotes = remote_records(None, filter);
    } else {
        let root = git::get_main_worktree_root()?;
        remotes = remote_records(Some(&root), filter);
        let project = project_name(&root);
        worktrees.extend(
            workflow::list(&display_config, mux.as_ref(), show_pr && !json, filter)?
                .into_iter()
                .map(|worktree| ProjectWorktree {
                    project: project.clone(),
                    project_path: root.clone(),
                    worktree,
                }),
        );
    }

    let remote_open = |record: &WorkspaceRecord| {
        mux.window_exists_by_full_name(&record.window_name)
            .unwrap_or(false)
    };

    if worktrees.is_empty() && remotes.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No worktrees found");
        }
        return Ok(());
    }

    if json {
        let entries: Vec<JsonWorktree> = worktrees
            .into_iter()
            .map(|entry| {
                let wt = entry.worktree;
                JsonWorktree {
                    project: entry.project,
                    project_path: entry.project_path.to_string_lossy().into_owned(),
                    handle: wt.handle,
                    branch: wt.branch,
                    path: wt.path.to_string_lossy().to_string(),
                    is_main: wt.is_main,
                    mode: match wt.mode {
                        MuxMode::Window => "window".to_string(),
                        MuxMode::Session => "session".to_string(),
                    },
                    has_uncommitted_changes: git::has_uncommitted_changes(&wt.path).unwrap_or(true),
                    is_open: wt.has_mux_window,
                    agent_statuses: wt
                        .agent_status
                        .map(|summary| summary.statuses)
                        .unwrap_or_default(),
                    created_at: wt.created_at,
                    remote: None,
                }
            })
            .chain(remotes.iter().map(|record| JsonWorktree {
                project: project_name(&record.project_root),
                project_path: record.project_root.to_string_lossy().into_owned(),
                handle: record.handle.clone(),
                branch: record.branch.clone(),
                path: record.remote_worktree.clone().unwrap_or_default(),
                is_main: false,
                mode: "window".to_string(),
                has_uncommitted_changes: false,
                is_open: remote_open(record),
                agent_statuses: Vec::new(),
                created_at: Some(record.created_at),
                remote: Some(record.remote.clone()),
            }))
            .collect();
        println!("{}", serde_json::to_string(&entries)?);
        return Ok(());
    }

    // Use icons when outputting to a terminal, text labels when piped (for agents)
    let use_icons = std::io::stdout().is_terminal();
    let current_dir = std::env::current_dir()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let display_data: Vec<WorktreeRow> = worktrees
        .into_iter()
        .map(|entry| {
            let wt = entry.worktree;
            let path_str = diff_paths(&wt.path, &current_dir)
                .map(|p| {
                    let s = p.display().to_string();
                    if s.is_empty() || s == "." {
                        "(here)".to_string()
                    } else {
                        s
                    }
                })
                .unwrap_or_else(|| wt.path.display().to_string());

            let age = if wt.is_main {
                "-".to_string()
            } else {
                wt.created_at
                    .map(|ts| format_compact_age(now.saturating_sub(ts)))
                    .unwrap_or_else(|| "-".to_string())
            };

            WorktreeRow {
                project: entry.project,
                branch: wt.branch,
                age,
                pr_status: format_pr_status(wt.pr_info),
                agent_status: format_agent_status(
                    wt.agent_status.as_ref(),
                    &display_config,
                    use_icons,
                ),
                mux_status: if wt.has_mux_window {
                    "✓".to_string()
                } else {
                    "-".to_string()
                },
                unmerged_status: if wt.has_unmerged {
                    "●".to_string()
                } else {
                    "-".to_string()
                },
                path_str,
            }
        })
        .chain(remotes.iter().map(|record| WorktreeRow {
            project: project_name(&record.project_root),
            branch: record.branch.clone(),
            age: format_compact_age(now.saturating_sub(record.created_at)),
            pr_status: "-".to_string(),
            agent_status: "-".to_string(),
            mux_status: if remote_open(record) {
                "✓".to_string()
            } else {
                "-".to_string()
            },
            unmerged_status: "-".to_string(),
            path_str: match (&record.homecoming, record.state) {
                (Some(Homecoming::Merged { .. }), _) => {
                    format!("remote:{} (merged, cleanup pending)", record.remote)
                }
                (Some(Homecoming::Pulled { .. }), _) => {
                    format!("remote:{} (pulled, cleanup pending)", record.remote)
                }
                (Some(_), _) => format!("remote:{} (merge in progress)", record.remote),
                (None, WorkspaceState::Ready) => format!("remote:{}", record.remote),
                (None, WorkspaceState::Provisioning) => {
                    format!("remote:{} (provisioning)", record.remote)
                }
            },
        }))
        .collect();

    let mut table = Table::new(display_data);
    table
        .with(Style::blank())
        .modify(Columns::new(0..8), Padding::new(0, 1, 0, 0));

    if !all {
        table.with(Remove::column(ByColumnName::new("PROJECT")));
    }
    if !show_pr {
        table.with(Remove::column(ByColumnName::new("PR")));
    }

    println!("{table}");

    Ok(())
}
