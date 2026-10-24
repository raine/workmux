//! `node remove-workspace`: stop a workspace and delete everything the node
//! holds for it. Removing an unknown workspace succeeds, so the laptop can
//! retry until it sees an acknowledgement.

use anyhow::{Context, Result};

use super::execution::{kill_execution, tmux_socket};
use super::{
    NodeRoot, base_ref, git, hook_env, refuse, run_hook, stop_containers_mounting, validate_id,
};
use crate::config::RemoteIsolation;
use crate::remote::protocol::{RemoveRequest, RemoveResponse, code};

pub fn remove(root: &NodeRoot, request: RemoveRequest) -> Result<RemoveResponse> {
    validate_id(&request.ws_id, 'w', "workspace id")?;
    let _lock = root.lock_workspace(&request.ws_id)?;
    let Some(workspace) = root.load(&request.ws_id)? else {
        root.forget(&request.ws_id)?;
        return Ok(RemoveResponse {
            already_removed: true,
        });
    };
    let worktree = &workspace.worktree_path;
    let has_worktree = worktree.join(".git").exists();

    if !request.force && has_worktree {
        let status = git(worktree, &["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            return Err(refuse(
                code::UNSAVED_WORK,
                format!(
                    "the worktree on the node has uncommitted changes:\n{}",
                    status.trim_end()
                ),
            ));
        }
        let range = format!("{}..HEAD", workspace.identity.base_sha);
        let commits = git(worktree, &["rev-list", "--count", &range])?;
        if commits.trim() != "0" {
            return Err(refuse(
                code::UNSAVED_WORK,
                format!(
                    "branch '{}' has {} commit(s) that exist only on the node",
                    workspace.identity.branch,
                    commits.trim()
                ),
            ));
        }
    }

    let config_path = root.config_path(&request.ws_id);
    if request.run_hooks && has_worktree && config_path.exists() {
        let (config, _) = crate::frozen_config::load(&config_path)?;
        let hooks = config.pre_remove.clone().unwrap_or_default();
        if !hooks.is_empty() {
            let env = hook_env(&workspace, None);
            for command in &hooks {
                run_hook(&config, command, worktree, &env)
                    .with_context(|| format!("Failed to run pre-remove command: '{command}'"))?;
            }
        }
    }

    if let Some(execution) = &workspace.execution {
        kill_execution(&tmux_socket(root)?, &execution.ex_id);
    }
    if workspace.isolation == RemoteIsolation::Container {
        stop_containers_mounting(worktree)?;
    }

    let repo = &workspace.repo_path;
    if repo.join(".git").is_dir() {
        let _project = root.lock_project(&workspace.identity.project_id)?;
        if has_worktree {
            let path = worktree.to_string_lossy();
            git(repo, &["worktree", "remove", "--force", "--force", &path])
                .context("Failed to remove the worktree")?;
        }
        if worktree.symlink_metadata().is_ok() {
            std::fs::remove_dir_all(worktree)
                .with_context(|| format!("Failed to remove {}", worktree.display()))?;
        }
        let _ = git(repo, &["worktree", "prune"]);
        let branch_ref = format!("refs/heads/{}", workspace.identity.branch);
        if !request.keep_branch && git(repo, &["rev-parse", "--verify", "-q", &branch_ref]).is_ok()
        {
            git(repo, &["branch", "-D", &workspace.identity.branch])
                .context("Failed to delete the branch")?;
        }
        let _ = git(repo, &["update-ref", "-d", &base_ref(&request.ws_id)]);
    }
    root.forget(&request.ws_id)?;
    Ok(RemoveResponse {
        already_removed: false,
    })
}
