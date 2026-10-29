//! `node remove-workspace`: stop a workspace and delete everything the node
//! holds for it. Removing an unknown workspace succeeds, so the laptop can
//! retry until it sees an acknowledgement.

use anyhow::{Context, Result};

use super::git::{matches, quiesce, workspace_refs};
use super::{NodeRoot, git, hook_env, refuse, run_hook, validate_id};
use crate::remote::protocol::{RemoveRequest, RemoveResponse, code};

pub fn remove(root: &NodeRoot, request: RemoveRequest) -> Result<RemoveResponse> {
    validate_id(&request.ws_id, 'w', "workspace id")?;
    let _lock = root.lock_workspace(&request.ws_id)?;
    let Some(mut workspace) = root.load(&request.ws_id)? else {
        root.forget(&request.ws_id)?;
        return Ok(RemoveResponse {
            already_removed: true,
        });
    };
    let worktree = &workspace.worktree_path.clone();
    let has_worktree = worktree.join(".git").exists();

    // An expected snapshot is checked again once the agent has stopped, so
    // nothing it wrote in between is deleted; hooks run only after that.
    if let Some(expect) = &request.expect
        && has_worktree
    {
        let handle = workspace.identity.handle.clone();
        if !matches(&workspace, expect)? {
            return Err(refuse(
                code::DIVERGED,
                format!("'{handle}' changed on the node after it was exported"),
            ));
        }
        quiesce(root, &mut workspace)?;
        if !matches(&workspace, expect)? {
            return Err(refuse(
                code::DIVERGED,
                format!(
                    "'{handle}' changed on the node after it was exported; its agent is \
                     stopped"
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

    quiesce(root, &mut workspace)?;

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
        for name in workspace_refs(&request.ws_id) {
            let _ = git(repo, &["update-ref", "-d", &name]);
        }
    }
    root.forget(&request.ws_id)?;
    Ok(RemoveResponse {
        already_removed: false,
    })
}
