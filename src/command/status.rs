use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Serialize;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, object::Columns},
};

use crate::multiplexer::{AgentStatus, create_backend, detect_backend};
use crate::vcs;
use crate::state::StateStore;
use crate::util;
use crate::workflow;

#[derive(Serialize)]
struct StatusEntry {
    worktree: String,
    branch: String,
    status: String,
    elapsed_secs: Option<u64>,
    title: Option<String>,
    pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    git: Option<GitInfo>,
}

#[derive(Serialize, Clone)]
struct GitInfo {
    has_staged: bool,
    has_unstaged: bool,
    has_unmerged_commits: bool,
}

#[derive(Tabled)]
struct StatusRow {
    #[tabled(rename = "WORKTREE")]
    worktree: String,
    #[tabled(rename = "STATUS")]
    status: String,
    #[tabled(rename = "ELAPSED")]
    elapsed: String,
    #[tabled(rename = "GIT")]
    git: String,
    #[tabled(rename = "TITLE")]
    title: String,
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

fn status_label(status: Option<AgentStatus>) -> String {
    match status {
        Some(AgentStatus::Working) => "working".to_string(),
        Some(AgentStatus::Waiting) => "waiting".to_string(),
        Some(AgentStatus::Done) => "done".to_string(),
        None => "-".to_string(),
    }
}

pub fn run(worktrees: &[String], json: bool, show_git: bool) -> Result<()> {
    let mux = create_backend(detect_backend());

    let agent_panes =
        StateStore::new().and_then(|store| store.load_reconciled_agents(mux.as_ref()))?;

    if agent_panes.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No active agents");
        }
        return Ok(());
    }

    let vcs = vcs::detect_vcs()?;

    // Get all worktrees for mapping (propagate errors)
    let all_worktrees = vcs.list_workspaces()?;

    // Get unmerged info if --git flag
    let main_branch = if show_git {
        vcs.get_default_branch().ok()
    } else {
        None
    };
    let unmerged_branches = if show_git {
        main_branch
            .as_deref()
            .and_then(|main| vcs.get_merge_base(main).ok())
            .and_then(|base| vcs.get_unmerged_branches(&base).ok())
            .unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Build entries: match each agent pane to its worktree using shared helper
    let mut entries: Vec<StatusEntry> = Vec::new();

    for (wt_path, branch) in &all_worktrees {
        let matching = workflow::match_agents_to_worktree(&agent_panes, wt_path);
        if matching.is_empty() {
            continue;
        }

        let worktree_name = wt_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let git_info = if show_git {
            Some(GitInfo {
                has_staged: vcs.has_staged_changes(wt_path).unwrap_or(false),
                has_unstaged: vcs.has_unstaged_changes(wt_path).unwrap_or(false),
                has_unmerged_commits: unmerged_branches.contains(branch),
            })
        } else {
            None
        };

        // Each agent pane in the worktree gets its own entry
        for agent in matching {
            let elapsed_secs = agent.status_ts.map(|ts| now.saturating_sub(ts));

            entries.push(StatusEntry {
                worktree: worktree_name.clone(),
                branch: branch.clone(),
                status: status_label(agent.status),
                elapsed_secs,
                title: agent.pane_title.clone(),
                pane_id: agent.pane_id.clone(),
                git: git_info.clone(),
            });
        }
    }

    // Filter to requested worktrees if specified (handle-first, then branch fallback)
    if !worktrees.is_empty() {
        entries.retain(|e| worktrees.iter().any(|w| w == &e.worktree || w == &e.branch));
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        if entries.is_empty() {
            println!("No active agents");
            return Ok(());
        }

        let rows: Vec<StatusRow> = entries
            .iter()
            .map(|e| {
                let worktree = if e.branch != e.worktree {
                    format!("{} ({})", e.worktree, e.branch)
                } else {
                    e.worktree.clone()
                };
                StatusRow {
                    worktree,
                    status: e.status.clone(),
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
        if !show_git {
            table.with(tabled::settings::Remove::column(
                tabled::settings::location::ByColumnName::new("GIT"),
            ));
        }
        println!("{table}");
    }

    Ok(())
}
