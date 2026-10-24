//! `node spawn`: a remote agent's `workmux add`, served on its node.
//!
//! The new workspace is a sibling of the agent's own: same project,
//! isolation, agent, config and environment snapshot, with a branch that
//! starts at the tip of the parent's branch. Everything but the branch
//! name, prompt and background flag comes from the parent's records, never
//! from the agent, and no hooks run. The laptop learns about it from `node
//! watch` and adopts it when it next looks (see `remote::siblings`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::provision::{self, FileRecipe};
use super::{NodeRoot, Origin, base_ref, git, refuse, validate_id, validate_identity};
use crate::remote::cache;
use crate::remote::protocol::{
    ProvisionPayload, RemoveRequest, StartRequest, WorkspaceIdentity, code,
};

/// Set in the sessions of executions without a sandbox, where the agent's
/// `workmux add` reaches the node directly instead of through a supervisor.
pub const SESSION_ROOT_ENV: &str = "WM_NODE_ROOT";
pub const SESSION_WORKSPACE_ENV: &str = "WM_NODE_WORKSPACE";

/// Node root and workspace of the node session this process runs in.
pub fn session_parent() -> Option<(std::path::PathBuf, String)> {
    let root = std::env::var_os(SESSION_ROOT_ENV)?;
    let ws_id = std::env::var(SESSION_WORKSPACE_ENV).ok()?;
    Some((root.into(), ws_id))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// Workspace of the agent asking; supplied by its supervisor.
    pub parent: String,
    pub branch: Option<String>,
    pub prompt: Option<String>,
    #[serde(default)]
    pub background: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnResponse {
    pub ws_id: String,
    pub handle: String,
    pub branch: String,
}

pub fn spawn(root: &NodeRoot, request: SpawnRequest) -> Result<SpawnResponse> {
    validate_id(&request.parent, 'w', "workspace id")?;
    let parent = root.load(&request.parent)?.ok_or_else(|| {
        refuse(
            code::NOT_FOUND,
            format!("workspace {} does not exist on the node", request.parent),
        )
    })?;
    if !parent.progress.ready {
        return Err(refuse(
            code::FAILED,
            format!("workspace {} is not provisioned yet", request.parent),
        ));
    }
    let branch = request
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| {
            refuse(
                code::BAD_REQUEST,
                "remote agents must name the branch: `workmux add <branch> -b -P <prompt-file>`",
            )
        })?
        .to_string();

    let (config, location) = crate::frozen_config::load(&root.config_path(&parent.identity.ws_id))?;
    let handle = crate::naming::derive_handle(&branch, None, &config)
        .map_err(|err| refuse(code::BAD_REQUEST, format!("{err:#}")))?;
    let repo = &parent.repo_path;
    let branch_ref = format!("refs/heads/{branch}");
    if git(repo, &["rev-parse", "--verify", "-q", &branch_ref]).is_ok() {
        return Err(refuse(
            code::CONFLICT,
            format!("branch '{branch}' already exists on the node; pick another name"),
        ));
    }
    let identity = WorkspaceIdentity {
        ws_id: cache::new_id('w'),
        project_id: parent.identity.project_id.clone(),
        handle,
        branch,
        base_sha: parent_tip(&parent)?,
    };
    validate_identity(&identity)?;

    let ws_id = identity.ws_id.clone();
    let lock = root.lock_workspace(&ws_id)?;
    let mut workspace = provision::claim(root, &identity, parent.isolation)?;
    let provisioned = (|| -> Result<()> {
        let location = location.map(|location| {
            crate::remote::portable::rebase_location(&location, &workspace.worktree_path)
        });
        let layout = super::execution::layout(&config, parent.agent.as_deref());
        workspace.origin = Some(Origin {
            parent: parent.identity.ws_id.clone(),
            base_label: parent.identity.branch.clone(),
            background: request.background,
            spawned_at: cache::now(),
            layout,
        });
        root.save(&workspace)?;
        git(repo, &["update-ref", &base_ref(&ws_id), &identity.base_sha])?;
        let recipe = provision::load_recipe(root, &parent.identity.ws_id)?;
        let run_file_ops = recipe.is_some();
        let FileRecipe {
            copies,
            symlink_patterns,
            ignored,
        } = recipe.unwrap_or_default();
        let payload = ProvisionPayload {
            config: crate::frozen_config::snapshot_document(&config, location.as_ref())?,
            agent: parent.agent.clone(),
            prompt: request.prompt.clone().filter(|prompt| !prompt.is_empty()),
            prompt_file_only: config.prompt_file_only.unwrap_or(false),
            copies,
            seeds: Vec::new(),
            symlink_patterns,
            ignored,
            run_file_ops,
            run_hooks: false,
            git_user_name: None,
            git_user_email: None,
            secrets: super::secrets::workspace(root, &parent.identity.ws_id)?,
        };
        provision::provision_worktree(root, &mut workspace, payload)
    })();
    drop(lock);
    let started = provisioned.and_then(|()| {
        super::execution::start(
            root,
            StartRequest {
                ws_id: ws_id.clone(),
                ex_id: cache::new_id('x'),
                use_prompt: true,
                run_commands: true,
            },
        )
    });
    if let Err(err) = started {
        let cleanup = super::remove::remove(
            root,
            RemoveRequest {
                ws_id: ws_id.clone(),
                expect: None,
                keep_branch: false,
                run_hooks: false,
            },
        );
        if let Err(cleanup) = cleanup {
            eprintln!("workmux: could not clean up workspace {ws_id}: {cleanup:#}");
        }
        return Err(err);
    }
    Ok(SpawnResponse {
        ws_id,
        handle: identity.handle,
        branch: identity.branch,
    })
}

/// Tip of the parent's branch in the node repository, read without the
/// agent's hooks or config: the agent can write refs and objects there.
/// Falls back to the parent's base.
fn parent_tip(parent: &super::Workspace) -> Result<String> {
    let spec = format!("refs/heads/{}^{{commit}}", parent.identity.branch);
    let mut cmd = crate::git::pinned_git(&parent.repo_path)
        .context("Refusing to read the parent's repository")?;
    let output = cmd
        .args(["rev-parse", "--verify", "-q", &spec])
        .output()
        .context("Failed to run git")?;
    let tip = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && !tip.is_empty() {
        return Ok(tip);
    }
    Ok(parent.identity.base_sha.clone())
}
