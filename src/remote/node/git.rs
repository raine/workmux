//! Node verbs that move work between the node and the laptop: `export`,
//! `checkout`, `rebase` and `diff`.

use std::path::Path;

use anyhow::{Context, Result};

use super::execution::{kill_execution, tmux_socket};
use super::{NodeRoot, Workspace, base_ref, refuse, stop_containers_mounting, validate_id};
use crate::config::RemoteIsolation;
use crate::remote::git::{self, Snapshot};
use crate::remote::protocol::{
    CheckoutRequest, CheckoutResponse, DiffMode, DiffRequest, DiffResponse, ExportRequest,
    ExportResponse, MAX_DIFF_BYTES, MAX_EXPORT_PATHS, RebaseRequest, RebaseResponse, code,
};

/// Ref `export` stores the commit of a snapshot tree under; `part` is
/// `index` or `worktree`.
pub fn export_ref(ws_id: &str, part: &str) -> String {
    format!("refs/workmux/export/{ws_id}/{part}")
}

/// Ref the laptop pushes the head of a `checkout` to.
pub fn kept_ref(ws_id: &str) -> String {
    format!("refs/workmux/kept/{ws_id}")
}

/// Ref the laptop pushes the target of a `rebase` to.
pub fn onto_ref(ws_id: &str) -> String {
    format!("refs/workmux/onto/{ws_id}")
}

/// Every ref the node keeps for workspace `ws_id`.
pub fn workspace_refs(ws_id: &str) -> Vec<String> {
    vec![
        base_ref(ws_id),
        export_ref(ws_id, "index"),
        export_ref(ws_id, "worktree"),
        kept_ref(ws_id),
        onto_ref(ws_id),
    ]
}

/// The workspace `ws_id` with a worktree, locked for the caller.
fn open(root: &NodeRoot, ws_id: &str) -> Result<(std::fs::File, Workspace)> {
    validate_id(ws_id, 'w', "workspace id")?;
    let lock = root.lock_workspace(ws_id)?;
    let workspace = root.load(ws_id)?.ok_or_else(|| {
        refuse(
            code::NOT_FOUND,
            format!("workspace {ws_id} does not exist on the node"),
        )
    })?;
    if !workspace.worktree_path.join(".git").exists() {
        return Err(refuse(
            code::NO_WORKTREE,
            format!("workspace '{}' has no worktree", workspace.identity.handle),
        ));
    }
    Ok((lock, workspace))
}

/// Stop the workspace's execution and containers and forget the execution.
pub fn quiesce(root: &NodeRoot, workspace: &mut Workspace) -> Result<()> {
    if let Some(execution) = workspace.execution.take() {
        kill_execution(root, &tmux_socket(root)?, &execution.ex_id);
    }
    if workspace.isolation == RemoteIsolation::Container {
        stop_containers_mounting(&workspace.worktree_path)?;
    }
    root.save(workspace)
}

/// The workspace's snapshot, refusing states that cannot leave the node.
fn take_snapshot(workspace: &Workspace) -> Result<Snapshot> {
    let worktree = &workspace.worktree_path;
    let handle = &workspace.identity.handle;
    if let Some(reason) = git::unexportable(worktree, &workspace.identity.branch)? {
        return Err(refuse(
            code::CONFLICT,
            format!(
                "{reason} in the worktree of '{handle}' on the node; finish it there \
                 (`workmux open {handle}`)"
            ),
        ));
    }
    if let Some(path) = git::nested_repository(worktree)? {
        return Err(refuse(
            code::CONFLICT,
            format!(
                "'{path}' in the worktree of '{handle}' on the node is a separate git \
                 repository, which cannot be brought home; move it out of the worktree or \
                 ignore it there (`workmux open {handle}`)"
            ),
        ));
    }
    git::snapshot(worktree)
}

/// Whether the workspace still has exactly the state `expect`.
pub fn matches(workspace: &Workspace, expect: &Snapshot) -> Result<bool> {
    let worktree = &workspace.worktree_path;
    if git::unexportable(worktree, &workspace.identity.branch)?.is_some() {
        return Ok(false);
    }
    Ok(git::snapshot(worktree)? == *expect)
}

pub fn export(root: &NodeRoot, request: ExportRequest) -> Result<ExportResponse> {
    let (_lock, mut workspace) = open(root, &request.ws_id)?;
    if request.stop {
        quiesce(root, &mut workspace)?;
    }
    let worktree = &workspace.worktree_path;
    let snapshot = take_snapshot(&workspace)?;
    for (part, tree) in [("index", &snapshot.index), ("worktree", &snapshot.worktree)] {
        let commit = git::commit_tree(worktree, tree, &snapshot.head)?;
        git::run(
            worktree,
            &["update-ref", &export_ref(&request.ws_id, part), &commit],
        )?;
    }
    let mut changes = git::changes(worktree, &snapshot)?;
    let mut truncated = false;
    for list in [
        &mut changes.staged,
        &mut changes.unstaged,
        &mut changes.untracked,
    ] {
        if list.len() > MAX_EXPORT_PATHS {
            list.truncate(MAX_EXPORT_PATHS);
            truncated = true;
        }
    }
    Ok(ExportResponse {
        snapshot,
        changes,
        truncated,
        repo_path: workspace.repo_path.to_string_lossy().into_owned(),
    })
}

pub fn checkout(root: &NodeRoot, request: CheckoutRequest) -> Result<CheckoutResponse> {
    let (_lock, workspace) = open(root, &request.ws_id)?;
    let worktree = &workspace.worktree_path;
    if !matches(&workspace, &request.expect)? {
        return Err(refuse(
            code::DIVERGED,
            format!(
                "'{}' changed on the node after it was exported",
                workspace.identity.handle
            ),
        ));
    }
    let kept = git::run(
        worktree,
        &[
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", kept_ref(&request.ws_id)),
        ],
    )?;
    if kept != request.head {
        return Err(refuse(
            code::BAD_REQUEST,
            format!(
                "{} does not hold {}",
                kept_ref(&request.ws_id),
                request.head
            ),
        ));
    }
    let output = crate::git::pinned_git(worktree)?
        .args([
            "checkout",
            "-q",
            "-B",
            &workspace.identity.branch,
            &request.head,
        ])
        .output()
        .context("Failed to run git checkout")?;
    if !output.status.success() {
        return Err(refuse(
            code::CONFLICT,
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(CheckoutResponse {})
}

pub fn rebase(root: &NodeRoot, request: RebaseRequest) -> Result<RebaseResponse> {
    let (_lock, workspace) = open(root, &request.ws_id)?;
    let worktree = &workspace.worktree_path;
    let handle = &workspace.identity.handle;
    if let Some(reason) = git::unexportable(worktree, &workspace.identity.branch)? {
        return Err(refuse(
            code::CONFLICT,
            format!("{reason} in the worktree of '{handle}' on the node"),
        ));
    }
    let onto = git::run(
        worktree,
        &[
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", onto_ref(&request.ws_id)),
        ],
    )?;
    if onto != request.onto {
        return Err(refuse(
            code::BAD_REQUEST,
            format!(
                "{} does not hold {}",
                onto_ref(&request.ws_id),
                request.onto
            ),
        ));
    }
    let mut command = crate::git::pinned_git(worktree)?;
    if let Some(committer) = &request.committer {
        command
            .env("GIT_COMMITTER_NAME", &committer.name)
            .env("GIT_COMMITTER_EMAIL", &committer.email);
    }
    let output = command
        .args(["rebase", "--quiet", &onto])
        .output()
        .context("Failed to run git rebase")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let rebasing = git::unexportable(worktree, &workspace.identity.branch)?
            .is_some_and(|reason| reason.contains("rebase"));
        let message = if rebasing {
            format!(
                "the rebase of '{handle}' stopped on conflicts on the node. Resolve them there \
                 (`workmux open {handle}`), then run `git rebase --continue`, or \
                 `git rebase --abort` to cancel\n{detail}"
            )
        } else {
            format!("the rebase of '{handle}' failed on the node\n{detail}")
        };
        return Err(refuse(code::CONFLICT, message));
    }
    git::run(worktree, &["update-ref", &base_ref(&request.ws_id), &onto])?;
    Ok(RebaseResponse {})
}

pub fn diff(root: &NodeRoot, request: DiffRequest) -> Result<DiffResponse> {
    let (_lock, workspace) = open(root, &request.ws_id)?;
    let worktree: &Path = &workspace.worktree_path;
    let (arg, untracked) = match request.mode {
        DiffMode::Wip => ("HEAD".to_string(), true),
        DiffMode::Branch => (format!("{}...HEAD", base_ref(&request.ws_id)), false),
    };
    let mut bytes = crate::command::dashboard::diff::raw_diff(worktree, &arg, untracked)
        .map_err(|err| refuse(code::FAILED, err))?;
    let truncated = bytes.len() > MAX_DIFF_BYTES;
    if truncated {
        bytes.truncate(MAX_DIFF_BYTES);
        if let Some(end) = bytes.iter().rposition(|b| *b == b'\n') {
            bytes.truncate(end + 1);
        }
    }
    Ok(DiffResponse {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
    })
}
