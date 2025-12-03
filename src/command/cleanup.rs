use crate::{config, git, workflow};
use anyhow::{Context, Result};
use std::io::{self, Write};

pub fn run(force: bool) -> Result<()> {
    let config = config::Config::load(None)?;
    let worktrees = workflow::list(&config)?;

    // Find orphaned worktrees
    let orphaned: Vec<_> = worktrees.iter().filter(|wt| wt.is_orphaned).collect();

    if orphaned.is_empty() {
        println!("No orphaned worktrees found.");
        return Ok(());
    }

    // Display orphaned worktrees
    println!("Found {} orphaned worktree(s):", orphaned.len());
    for wt in &orphaned {
        println!("  - {} ({})", wt.branch, wt.path.display());
    }
    println!();

    // Confirm unless --force
    if !force {
        print!("Clean up all orphaned worktrees? [y/N] ");
        io::stdout().flush().context("Failed to flush stdout")?;

        let mut confirmation = String::new();
        io::stdin()
            .read_line(&mut confirmation)
            .context("Failed to read user confirmation")?;

        if confirmation.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Prune git worktree metadata
    println!("Pruning git worktree metadata...");
    git::prune_worktrees().context("Failed to prune worktrees")?;

    // Delete orphaned branches
    let mut deleted_count = 0;
    for wt in &orphaned {
        // Skip detached HEAD and main branch
        if wt.branch == "(detached)" {
            continue;
        }

        print!("Deleting branch '{}'... ", wt.branch);
        io::stdout().flush().ok();

        match git::delete_branch(&wt.branch, true) {
            Ok(_) => {
                println!("done");
                deleted_count += 1;
            }
            Err(e) => {
                println!("failed: {}", e);
            }
        }
    }

    println!(
        "\n✓ Cleaned up {} orphaned worktree(s)",
        orphaned.len()
    );
    if deleted_count > 0 {
        println!("✓ Deleted {} branch(es)", deleted_count);
    }

    Ok(())
}
