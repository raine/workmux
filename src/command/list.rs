use std::borrow::Cow;
use std::io::IsTerminal;

use crate::config;
use crate::config::MuxMode;
use crate::multiplexer::{AgentStatus, create_backend, detect_backend};
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
    branch: String,
    age: String,
    pr_status: String,
    agent_status: String,
    mux_status: String,
    unmerged_status: String,
    path_str: String,
}

impl Tabled for WorktreeRow {
    const LENGTH: usize = 7;

    fn fields(&self) -> Vec<Cow<'_, str>> {
        vec![
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
    handle: String,
    branch: String,
    path: String,
    is_main: bool,
    mode: String,
    has_uncommitted_changes: bool,
    is_open: bool,
    created_at: Option<u64>,
}

pub fn run(show_pr: bool, json: bool, filter: &[String]) -> Result<()> {
    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    // Skip PR fetch when outputting JSON since it's not included in the JSON schema
    let worktrees = workflow::list(&config, mux.as_ref(), show_pr && !json, filter)?;

    if worktrees.is_empty() {
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
            .map(|wt| JsonWorktree {
                handle: wt.handle,
                branch: wt.branch,
                path: wt.path.to_string_lossy().to_string(),
                is_main: wt.is_main,
                mode: match wt.mode {
                    MuxMode::Window => "window".to_string(),
                    MuxMode::Session => "session".to_string(),
                },
                has_uncommitted_changes: git::has_uncommitted_changes(&wt.path).unwrap_or(false),
                is_open: wt.has_mux_window,
                created_at: wt.created_at,
            })
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

            let age = if wt.is_main {
                "-".to_string()
            } else {
                wt.created_at
                    .map(|ts| format_compact_age(now.saturating_sub(ts)))
                    .unwrap_or_else(|| "-".to_string())
            };

            WorktreeRow {
                branch: wt.branch,
                age,
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
        .modify(Columns::new(0..7), Padding::new(0, 1, 0, 0));

    // Hide PR column if --pr flag not used
    if !show_pr {
        table.with(Remove::column(ByColumnName::new("PR")));
    }

    println!("{table}");

    Ok(())
}
