use anyhow::{Result, anyhow};

use crate::{config, git, tmux};

use super::types::WorktreeInfo;

/// List all worktrees with their status
pub fn list(config: &config::Config) -> Result<Vec<WorktreeInfo>> {
    if !git::is_git_repo()? {
        return Err(anyhow!("Not in a git repository"));
    }

    let worktrees_data = git::list_worktrees()?;

    if worktrees_data.is_empty() {
        return Ok(Vec::new());
    }

    // Check tmux status and get all windows once to avoid repeated process calls
    let tmux_windows: std::collections::HashSet<String> = if tmux::is_running().unwrap_or(false) {
        tmux::get_all_window_names().unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };

    // Get the main branch for unmerged checks
    let main_branch = git::get_default_branch().ok();

    // Get all unmerged branches in one go for efficiency
    // Prefer checking against remote tracking branch for more accurate results
    let unmerged_branches = main_branch
        .as_deref()
        .and_then(|main| git::get_merge_base(main).ok())
        .and_then(|base| git::get_unmerged_branches(&base).ok())
        .unwrap_or_default(); // Use an empty set on failure

    let prefix = config.window_prefix();
    let worktrees: Vec<WorktreeInfo> = worktrees_data
        .into_iter()
        .map(|(path, branch)| {
            // Check if the worktree directory still exists on disk
            let is_orphaned = !path.exists();

            // Extract handle from worktree path basename (the source of truth)
            let handle = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&branch)
                .to_string();

            // Use handle for tmux window check, not branch name
            let prefixed_window_name = tmux::prefixed(prefix, &handle);
            let has_tmux = tmux_windows.contains(&prefixed_window_name);

            // Check for unmerged commits, but only if this isn't the main branch.
            // Skip unmerged check for orphaned worktrees since we can't check git status.
            let has_unmerged = if is_orphaned {
                false
            } else if let Some(ref main) = main_branch {
                if branch == *main || branch == "(detached)" {
                    false
                } else {
                    unmerged_branches.contains(&branch)
                }
            } else {
                false
            };

            WorktreeInfo {
                branch,
                path,
                has_tmux,
                has_unmerged,
                is_orphaned,
            }
        })
        .collect();

    Ok(worktrees)
}
