use crate::{config, workflow};
use anyhow::Result;
use pathdiff::diff_paths;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, object::Columns},
};

#[derive(Tabled)]
struct WorktreeRow {
    #[tabled(rename = "BRANCH")]
    branch: String,
    #[tabled(rename = "TMUX")]
    tmux_status: String,
    #[tabled(rename = "UNMERGED")]
    unmerged_status: String,
    #[tabled(rename = "PATH")]
    path_str: String,
    #[tabled(rename = "STATUS")]
    status: String,
}

pub fn run() -> Result<()> {
    let config = config::Config::load(None)?;
    let worktrees = workflow::list(&config)?;

    if worktrees.is_empty() {
        println!("No worktrees found");
        return Ok(());
    }

    let current_dir = std::env::current_dir()?;

    // Count orphaned worktrees before consuming the iterator
    let orphan_count = worktrees.iter().filter(|wt| wt.is_orphaned).count();

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
                path_str,
                tmux_status: if wt.has_tmux {
                    "✓".to_string()
                } else {
                    "-".to_string()
                },
                unmerged_status: if wt.has_unmerged {
                    "●".to_string()
                } else {
                    "-".to_string()
                },
                status: if wt.is_orphaned {
                    "orphaned".to_string()
                } else {
                    "-".to_string()
                },
            }
        })
        .collect();

    let mut table = Table::new(display_data);
    table
        .with(Style::blank())
        .modify(Columns::new(0..3), Padding::new(0, 1, 0, 0));

    println!("{table}");

    if orphan_count > 0 {
        eprintln!();
        eprintln!(
            "Hint: {} orphaned worktree(s) found (directories no longer exist).",
            orphan_count
        );
        eprintln!("Run 'workmux cleanup' to remove them.");
    }

    Ok(())
}
