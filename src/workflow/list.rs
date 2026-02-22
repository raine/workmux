use anyhow::{Result, anyhow};
use std::collections::HashSet;
use std::path::PathBuf;

use crate::config::MuxMode;
use crate::multiplexer::{Multiplexer, util};
use crate::state::StateStore;
use crate::util::canon_or_self;
use crate::vcs::Vcs;
use crate::{config, github, spinner};

use super::types::{AgentStatusSummary, WorktreeInfo};

/// Filter worktrees by handle (directory name) or branch name.
/// Uses handle-first precedence: if a filter token matches a handle, that takes
/// priority over branch name matches.
fn filter_worktrees(
    worktrees: Vec<(PathBuf, String)>,
    filter: &[String],
) -> Vec<(PathBuf, String)> {
    if filter.is_empty() {
        return worktrees;
    }

    let mut matched_paths = HashSet::new();

    for token in filter {
        // First: try to match by handle (directory name)
        let handle_match = worktrees.iter().find(|(path, _)| {
            path.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|name| name == token)
        });

        if let Some((path, _)) = handle_match {
            matched_paths.insert(path.clone());
            continue;
        }

        // Fallback: try to match by branch name
        for (path, branch) in &worktrees {
            if branch == token {
                matched_paths.insert(path.clone());
            }
        }
    }

    worktrees
        .into_iter()
        .filter(|(path, _)| matched_paths.contains(path))
        .collect()
}

/// List all worktrees with their status
pub fn list(
    config: &config::Config,
    mux: &dyn Multiplexer,
    vcs: &dyn Vcs,
    fetch_pr_status: bool,
    filter: &[String],
) -> Result<Vec<WorktreeInfo>> {
    if !vcs.is_repo()? {
        return Err(anyhow!("Not in a {} repository", vcs.name()));
    }

    let worktrees_data = vcs.list_workspaces()?;

    if worktrees_data.is_empty() {
        return Ok(Vec::new());
    }

    // Apply filter early before expensive operations
    let worktrees_data = filter_worktrees(worktrees_data, filter);

    if worktrees_data.is_empty() {
        return Ok(Vec::new());
    }

    // Check mux status and get all windows/sessions once to avoid repeated process calls
    let mux_running = mux.is_running().unwrap_or(false);
    let mux_windows: HashSet<String> = if mux_running {
        mux.get_all_window_names().unwrap_or_default()
    } else {
        HashSet::new()
    };
    let mux_sessions: HashSet<String> = if mux_running {
        mux.get_all_session_names().unwrap_or_default()
    } else {
        HashSet::new()
    };

    // Get the main branch for unmerged checks
    let main_branch = vcs.get_default_branch().ok();

    // Get all unmerged branches in one go for efficiency
    // Prefer checking against remote tracking branch for more accurate results
    let unmerged_branches = main_branch
        .as_deref()
        .and_then(|main| vcs.get_merge_base(main).ok())
        .and_then(|base| vcs.get_unmerged_branches(&base).ok())
        .unwrap_or_default(); // Use an empty set on failure

    // Batch fetch all PRs if requested (single API call)
    let pr_map = if fetch_pr_status {
        spinner::with_spinner("Fetching PR status", || {
            Ok(github::list_prs().unwrap_or_default())
        })?
    } else {
        std::collections::HashMap::new()
    };

    // Load reconciled agent states (only if multiplexer is running)
    let agent_panes = if mux_running {
        StateStore::new()
            .ok()
            .and_then(|store| store.load_reconciled_agents(mux).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Pre-calculate canonical paths for agents to avoid repeated syscalls
    let agent_panes_canon: Vec<_> = agent_panes
        .iter()
        .map(|a| (canon_or_self(&a.path), a.status))
        .collect();

    // Batch-load all workspace modes in a single VCS config call
    let worktree_modes = vcs.get_all_workspace_modes();

    let prefix = config.window_prefix();
    let worktrees: Vec<WorktreeInfo> = worktrees_data
        .into_iter()
        .map(|(path, branch)| {
            // Extract handle from worktree path basename (the source of truth)
            let handle = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&branch)
                .to_string();

            // Check if mux target exists (window or session based on stored mode)
            let prefixed_name = util::prefixed(prefix, &handle);
            let mode = worktree_modes
                .get(&handle)
                .copied()
                .unwrap_or(MuxMode::Window);
            let has_mux_window = if mode == MuxMode::Session {
                mux_sessions.contains(&prefixed_name)
            } else {
                mux_windows.contains(&prefixed_name)
            };

            // Check for unmerged commits, but only if this isn't the main branch
            let has_unmerged = if let Some(ref main) = main_branch {
                if branch == *main || branch == "(detached)" {
                    false
                } else {
                    unmerged_branches.contains(&branch)
                }
            } else {
                false
            };

            // Lookup PR info from batch fetch
            let pr_info = pr_map.get(&branch).cloned();

            // Match agents to this worktree by comparing canonicalized paths.
            // An agent's workdir should be within the worktree directory.
            let canon_wt_path = canon_or_self(&path);
            let matching_statuses: Vec<_> = agent_panes_canon
                .iter()
                .filter(|(canon_agent_path, _)| {
                    *canon_agent_path == canon_wt_path
                        || canon_agent_path.starts_with(&canon_wt_path)
                })
                .filter_map(|(_, status)| *status)
                .collect();

            let agent_status = if matching_statuses.is_empty() {
                None
            } else {
                Some(AgentStatusSummary {
                    statuses: matching_statuses,
                })
            };

            WorktreeInfo {
                branch,
                path,
                has_mux_window,
                has_unmerged,
                pr_info,
                agent_status,
            }
        })
        .collect();

    Ok(worktrees)
}
