use std::io::IsTerminal;

use crate::config;
use crate::multiplexer::{AgentStatus, create_backend, detect_backend};
use crate::workflow::types::AgentStatusSummary;
use crate::{nerdfont, workflow};
use anyhow::Result;
use pathdiff::diff_paths;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, disable::Remove, object::Columns},
};

#[derive(Tabled)]
struct WorktreeRow {
    #[tabled(rename = "BRANCH")]
    branch: String,
    #[tabled(rename = "PR")]
    pr_status: String,
    #[tabled(rename = "AGENT")]
    agent_status: String,
    #[tabled(rename = "MUX")]
    mux_status: String,
    #[tabled(rename = "UNMERGED")]
    unmerged_status: String,
    #[tabled(rename = "PATH")]
    path_str: String,
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

pub fn run(show_pr: bool, filter: &[String]) -> Result<()> {
    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let vcs = crate::vcs::detect_vcs()?;
    let worktrees = workflow::list(&config, mux.as_ref(), vcs.as_ref(), show_pr, filter)?;

    if worktrees.is_empty() {
        println!("No worktrees found");
        return Ok(());
    }

    // Use icons when outputting to a terminal, text labels when piped (for agents)
    let use_icons = std::io::stdout().is_terminal();
    let current_dir = std::env::current_dir()?;

    let display_data: Vec<WorktreeRow> = worktrees
        .into_iter()
        .map(|wt| {
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

            WorktreeRow {
                branch: wt.branch,
                pr_status: format_pr_status(wt.pr_info),
                agent_status: format_agent_status(wt.agent_status.as_ref(), &config, use_icons),
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
        .collect();

    let mut table = Table::new(display_data);
    table
        .with(Style::blank())
        .modify(Columns::new(0..6), Padding::new(0, 1, 0, 0));

    // Hide PR column if --pr flag not used (column 1)
    if !show_pr {
        table.with(Remove::column(Columns::new(1..2)));
    }

    println!("{table}");

    Ok(())
}
