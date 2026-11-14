use anyhow::{Result, anyhow};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::MuxMode;
use crate::multiplexer::{Multiplexer, util};
use crate::state::StateStore;
use crate::util::canon_or_self;
use crate::{config, git, github, spinner};

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
    fetch_pr_status: bool,
    filter: &[String],
) -> Result<Vec<WorktreeInfo>> {
    list_in(config, mux, fetch_pr_status, filter, None)
}

/// List all worktrees with their status, optionally for a specific repo path
pub fn list_in(
    config: &config::Config,
    mux: &dyn Multiplexer,
    fetch_pr_status: bool,
    filter: &[String],
    repo: Option<&Path>,
) -> Result<Vec<WorktreeInfo>> {
    if repo.is_none() && !git::is_git_repo()? {
        return Err(anyhow!("Not in a git repository"));
    }

    let listing = git::list_worktrees_with_main_in(repo)?;
    let main_worktree_path = listing.main_path;
    let worktrees_data = listing.worktrees;

    if worktrees_data.is_empty() {
        return Ok(Vec::new());
    }

    let repo_key = git::get_repository_key_in(repo)?;

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
    let main_branch = git::get_default_branch_in(repo).ok();

    // Get all unmerged branches in one go for efficiency
    // Prefer checking against remote tracking branch for more accurate results
    let unmerged_branches = main_branch
        .as_deref()
        .and_then(|main| git::get_merge_base_in(repo, main).ok())
        .and_then(|base| git::get_unmerged_branches_in(repo, &base).ok())
        .unwrap_or_default(); // Use an empty set on failure

    // Batch fetch all PRs if requested (single API call)
    let pr_map = if fetch_pr_status {
        spinner::with_spinner("Fetching PR status", || {
            Ok(github::list_prs_in(repo).unwrap_or_default())
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

    // Batch-load all worktree modes in a single git config call
    let worktree_modes = git::get_all_worktree_modes_in(repo);
    let target_windows = git::get_all_worktree_meta_key_in(repo, "target-window");
    let target_sessions = git::get_all_worktree_meta_key_in(repo, "target-session");
    let window_sessions = git::get_all_worktree_meta_key_in(repo, "window-session");
    let window_tokens = git::get_all_worktree_meta_key_in(repo, "window-token");
    let attachments = git::get_all_worktree_meta_key_in(repo, "attachment");
    let active_window_tokens = if mux_running {
        mux.owned_window_tokens().unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };
    let mux_windows_with_sessions = if mux_running {
        mux.get_all_windows_with_sessions().unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };

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
            let mode = worktree_modes
                .get(&handle)
                .copied()
                .unwrap_or(MuxMode::Window);
            let target_name = if mode == MuxMode::Session {
                target_sessions.get(&handle).unwrap_or(&handle)
            } else {
                target_windows.get(&handle).unwrap_or(&handle)
            };
            let prefixed_name = util::prefixed(prefix, target_name);
            let has_mux_window = if attachments
                .get(&handle)
                .is_some_and(|value| value != "multiplexer")
            {
                false
            } else if mode == MuxMode::Session {
                mux_sessions.contains(&prefixed_name)
            } else if mux.supports_window_ownership() && window_tokens.contains_key(&handle) {
                window_tokens.get(&handle).is_some_and(|token| {
                    active_window_tokens.contains(token)
                        || mux
                            .resolve_owned_window_targets(
                                token,
                                &prefixed_name,
                                window_sessions.get(&handle).map(String::as_str),
                                &path,
                            )
                            .is_ok_and(|targets| !targets.is_empty())
                })
            } else if let Some(parent_session) = window_sessions.get(&handle) {
                mux_windows_with_sessions.contains(&(prefixed_name.clone(), parent_session.clone()))
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

            let is_main = main_worktree_path == path;

            let created_at = crate::creation_time::filesystem_birth_time(&path);

            let base_branch = git::get_branch_base_in(&branch, repo).ok();

            WorktreeInfo {
                repo_key: repo_key.clone(),
                handle,
                branch,
                path,
                is_main,
                mode,
                has_mux_window,
                has_unmerged,
                pr_info,
                agent_status,
                created_at,
                base_branch,
            }
        })
        .collect();

    Ok(worktrees)
}
