//! `node gc`: what the node holds that no workspace accounts for, and its
//! removal.
//!
//! Every call recomputes the findings. A call with `apply` removes only the
//! listed findings that are still findings, so nothing that started or
//! changed after the laptop listed is touched. Runtime state (sessions,
//! containers, processes) is read before the records: whatever starts in
//! between has saved its record by the time the records are read.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::execution::{sessions, tmux, tmux_socket};
use super::{NodeRoot, Workspace, git};
use crate::remote::protocol::{RemoveRequest, session_name};

/// Files younger than this may belong to a call in progress.
const GRACE: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GcRequest {
    /// Workspaces the laptop has records of.
    pub known: Vec<String>,
    /// Node binaries the laptop may still run.
    #[serde(default)]
    pub keep_pins: Vec<String>,
    /// Ids of findings to remove; without them nothing is removed.
    #[serde(default)]
    pub apply: Vec<String>,
    /// Also remove workspaces whose work exists only on the node.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GcResponse {
    /// Every workspace the node has.
    pub workspaces: Vec<String>,
    pub findings: Vec<Finding>,
    /// What looked unused but stays, and why.
    pub kept: Vec<String>,
    /// Applied ids that were removed.
    pub removed: Vec<String>,
    /// Applied ids that could not be removed.
    pub failed: Vec<Failure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// `<kind>:<name>`, stable across calls.
    pub id: String,
    pub detail: String,
    /// Removing it discards work that exists only on the node; applied only
    /// with `force`.
    #[serde(default)]
    pub unsaved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub id: String,
    pub message: String,
}

pub fn gc(root: &NodeRoot, request: GcRequest) -> Result<GcResponse> {
    let (findings, kept, workspaces) = survey(root, &request)?;
    let mut response = GcResponse {
        workspaces,
        findings,
        kept,
        ..GcResponse::default()
    };
    let apply: HashSet<&str> = request.apply.iter().map(String::as_str).collect();
    for finding in &response.findings {
        if !apply.contains(finding.id.as_str()) {
            continue;
        }
        if finding.unsaved && !request.force {
            response.failed.push(Failure {
                id: finding.id.clone(),
                message: "holds work that exists only on the node; pass --force".into(),
            });
            continue;
        }
        match remove(root, &finding.id) {
            Ok(()) => response.removed.push(finding.id.clone()),
            Err(err) => response.failed.push(Failure {
                id: finding.id.clone(),
                message: format!("{err:#}"),
            }),
        }
    }
    Ok(response)
}

fn survey(
    root: &NodeRoot,
    request: &GcRequest,
) -> Result<(Vec<Finding>, Vec<String>, Vec<String>)> {
    let socket = tmux_socket(root)?;
    let sessions = sessions(&socket);
    let containers = containers(root)?;
    let supervisors = supervisors(root);
    let workspaces = root.workspaces();

    let ids: HashSet<&str> = workspaces
        .iter()
        .map(|ws| ws.identity.ws_id.as_str())
        .collect();
    let live = |ws: &Workspace| {
        ws.execution.as_ref().is_some_and(|execution| {
            execution
                .panes
                .iter()
                .any(|pane| sessions.contains(&session_name(&execution.ex_id, pane.index)))
        })
    };
    let known: HashSet<&str> = request.known.iter().map(String::as_str).collect();
    let now = super::super::cache::now();
    let mut findings = Vec::new();
    let mut kept = Vec::new();

    for ws in &workspaces {
        let id = &ws.identity;
        if known.contains(id.ws_id.as_str()) {
            continue;
        }
        if let Some(origin) = &ws.origin {
            let age = now.saturating_sub(origin.spawned_at);
            if live(ws) || age < GRACE.as_secs() {
                kept.push(format!(
                    "workspace '{}' ({}): an agent spawned it and it is not adopted yet",
                    id.handle, id.ws_id
                ));
                continue;
            }
        }
        let unsaved = unsaved_work(ws).unwrap_or_else(|err| Some(format!("{err:#}")));
        let mut detail = format!("workspace '{}' (branch {})", id.handle, id.branch);
        if live(ws) {
            detail.push_str(", agent running");
        }
        if let Some(unsaved) = &unsaved {
            detail.push_str(&format!("; {}", first_line(unsaved).trim_end_matches(':')));
        }
        findings.push(Finding {
            id: format!("workspace:{}", id.ws_id),
            detail,
            unsaved: unsaved.is_some(),
        });
    }

    let worktrees: HashSet<&Path> = workspaces
        .iter()
        .map(|ws| ws.worktree_path.as_path())
        .collect();
    for dir in worktree_dirs(root) {
        if worktrees.contains(dir.as_path()) {
            continue;
        }
        let name = dir
            .strip_prefix(root.path.join("projects"))
            .unwrap_or(&dir)
            .display()
            .to_string();
        let unsaved = !saved_checkout(&dir);
        findings.push(Finding {
            id: format!("worktree:{name}"),
            detail: format!(
                "worktree {name} of no workspace{}",
                if unsaved {
                    "; may hold uncommitted changes or commits on no branch"
                } else {
                    ""
                }
            ),
            unsaved,
        });
    }

    let current: HashSet<String> = workspaces
        .iter()
        .filter_map(|ws| ws.execution.as_ref())
        .flat_map(|execution| {
            execution
                .panes
                .iter()
                .map(|pane| session_name(&execution.ex_id, pane.index))
                .collect::<Vec<_>>()
        })
        .collect();
    for session in &sessions {
        if is_execution_session(session) && !current.contains(session) {
            findings.push(Finding {
                id: format!("session:{session}"),
                detail: format!("tmux session {session} of no execution"),
                unsaved: false,
            });
        }
    }

    for container in &containers {
        let owner = workspaces
            .iter()
            .find(|ws| container.mounts.contains(&ws.worktree_path));
        if owner.is_some_and(live) {
            continue;
        }
        findings.push(Finding {
            id: format!("container:{}", container.id),
            detail: match owner {
                Some(ws) => format!(
                    "container {} of workspace '{}', whose agent is not running",
                    container.name, ws.identity.handle
                ),
                None => format!("container {} of no workspace", container.name),
            },
            unsaved: false,
        });
    }

    for (pid, ws_id) in &supervisors {
        let owner = workspaces.iter().find(|ws| ws.identity.ws_id == *ws_id);
        if owner.is_some_and(live) {
            continue;
        }
        findings.push(Finding {
            id: format!("supervisor:{pid}"),
            detail: format!(
                "sandbox supervisor (pid {pid}) of {ws_id}, which has no running agent"
            ),
            unsaved: false,
        });
    }

    for (repo, reference) in workmux_refs(root) {
        let ws_id = reference.rsplit('/').next().unwrap_or_default();
        if ids.contains(ws_id) {
            continue;
        }
        let project = repo
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        findings.push(Finding {
            id: format!("ref:{project}:{reference}"),
            detail: format!("{reference} in {project}, of no workspace"),
            unsaved: false,
        });
    }

    for path in stale_files(root, &ids) {
        let name = path
            .strip_prefix(&root.path)
            .unwrap_or(&path)
            .display()
            .to_string();
        findings.push(Finding {
            id: format!("file:{name}"),
            detail: format!("{name}, of no workspace"),
            unsaved: false,
        });
    }

    match unused_pins(root, request, &workspaces) {
        Ok(pins) => findings.extend(pins.into_iter().map(|pin| Finding {
            id: format!("pin:{pin}"),
            detail: format!("node binary bin/{pin}, used by no execution"),
            unsaved: false,
        })),
        Err(reason) => kept.push(format!("node binaries: {reason}")),
    }

    let mut workspace_ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    workspace_ids.sort();
    Ok((findings, kept, workspace_ids))
}

fn remove(root: &NodeRoot, id: &str) -> Result<()> {
    let (kind, name) = id.split_once(':').context("malformed finding id")?;
    match kind {
        "workspace" => {
            super::remove::remove(
                root,
                RemoveRequest {
                    ws_id: name.to_string(),
                    expect: None,
                    keep_branch: false,
                    run_hooks: false,
                },
            )?;
        }
        "worktree" => {
            let path = root.path.join("projects").join(name);
            let project = name.split('/').next().unwrap_or_default();
            let _lock = root.lock_project(project)?;
            let repo = root.repo_path(project);
            if repo.join(".git").is_dir() {
                let _ = git(
                    &repo,
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        "--force",
                        &path.to_string_lossy(),
                    ],
                );
            }
            if path.symlink_metadata().is_ok() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("Failed to remove {}", path.display()))?;
            }
            if repo.join(".git").is_dir() {
                let _ = git(&repo, &["worktree", "prune"]);
            }
        }
        "session" => {
            let socket = tmux_socket(root)?;
            let output = tmux(&socket)
                .args(["kill-session", "-t", &format!("={name}")])
                .output()
                .context("Failed to run tmux")?;
            if !output.status.success() {
                anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
            }
            super::execution::AgentMarker::sweep(root, &socket);
        }
        "container" => {
            let output = std::process::Command::new("docker")
                .args(["rm", "-f", name])
                .output()
                .context("Failed to run docker")?;
            if !output.status.success() {
                anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
            }
        }
        "supervisor" => {
            let pid: i32 = name.parse().context("malformed pid")?;
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            )
            .context("Failed to signal the supervisor")?;
        }
        "ref" => {
            let (project, reference) = name.split_once(':').context("malformed ref finding")?;
            let _lock = root.lock_project(project)?;
            git(&root.repo_path(project), &["update-ref", "-d", reference])?;
        }
        "file" => {
            let path = root.path.join(name);
            if path.extension().is_some_and(|ext| ext == "lock") {
                // Held means a call is using it after all.
                let file = fs::File::open(&path)?;
                file.try_lock().context("in use")?;
            }
            fs::remove_file(&path)
                .with_context(|| format!("Failed to remove {}", path.display()))?;
        }
        "pin" => {
            let path = root.path.join("bin").join(name);
            fs::remove_file(&path)
                .with_context(|| format!("Failed to remove {}", path.display()))?;
        }
        _ => anyhow::bail!("unknown finding kind '{kind}'"),
    }
    Ok(())
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// `<ex-id>-<pane>`, as [`session_name`] names them.
fn is_execution_session(name: &str) -> bool {
    name.rsplit_once('-').is_some_and(|(ex_id, pane)| {
        super::validate_id(ex_id, 'x', "execution id").is_ok()
            && !pane.is_empty()
            && pane.bytes().all(|b| b.is_ascii_digit())
    })
}

/// `projects/*/worktrees/*`.
fn worktree_dirs(root: &NodeRoot) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for project in read_dir(&root.path.join("projects")) {
        dirs.extend(read_dir(&project.join("worktrees")));
    }
    dirs
}

/// Output of git in checkout `dir`, run without the hooks or config a guest
/// could have planted in it.
fn checkout_git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = crate::git::pinned_git(dir)?.args(args).output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        dir.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// What removing `workspace` would discard that exists only on the node:
/// uncommitted changes or commits past its base.
fn unsaved_work(workspace: &Workspace) -> Result<Option<String>> {
    let worktree = &workspace.worktree_path;
    if !worktree.join(".git").exists() {
        return Ok(None);
    }
    let status = checkout_git(worktree, &["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        return Ok(Some(format!(
            "the worktree on the node has uncommitted changes:\n{}",
            status.trim_end()
        )));
    }
    let range = format!("{}..HEAD", workspace.identity.base_sha);
    let commits = checkout_git(worktree, &["rev-list", "--count", &range])?;
    if commits.trim() != "0" {
        return Ok(Some(format!(
            "branch '{}' has {} commit(s) that exist only on the node",
            workspace.identity.branch,
            commits.trim()
        )));
    }
    Ok(None)
}

/// Whether all work in checkout `dir` is in the repository: no uncommitted
/// changes, and no commits that only its (detached) HEAD reaches.
fn saved_checkout(dir: &Path) -> bool {
    let run = |args: &[&str]| checkout_git(dir, args).ok();
    run(&["status", "--porcelain"]).is_some_and(|out| out.trim().is_empty())
        && run(&[
            "rev-list",
            "--count",
            "HEAD",
            "--not",
            "--branches",
            "--tags",
            "--remotes",
        ])
        .is_some_and(|out| out.trim() == "0")
}

struct Container {
    id: String,
    name: String,
    mounts: Vec<PathBuf>,
}

/// Docker containers, running or not, that mount something under this
/// root's `projects/`.
fn containers(root: &NodeRoot) -> Result<Vec<Container>> {
    if which::which("docker").is_err() {
        return Ok(Vec::new());
    }
    let docker = |args: &[&str]| -> Result<String> {
        let output = std::process::Command::new("docker")
            .args(args)
            .output()
            .context("Failed to run docker")?;
        if !output.status.success() {
            anyhow::bail!(
                "docker {} failed: {}",
                args[0],
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let ids: Vec<String> = docker(&["ps", "-aq"])?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let projects = root.path.join("projects");
    let canonical = projects.canonicalize().unwrap_or_else(|_| projects.clone());
    let mut args = vec![
        "inspect",
        "--format",
        "{{.Id}}\t{{.Name}}{{range .Mounts}}\t{{.Source}}{{end}}",
    ];
    args.extend(ids.iter().map(String::as_str));
    // A container that went away in between fails the whole inspect; it
    // is gone either way.
    let inspected = docker(&args).unwrap_or_default();
    Ok(inspected
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?.to_string();
            let name = fields.next()?.trim_start_matches('/').to_string();
            let mounts: Vec<PathBuf> = fields.map(PathBuf::from).collect();
            mounts
                .iter()
                .any(|mount| mount.starts_with(&projects) || mount.starts_with(&canonical))
                .then_some(Container {
                    id: id.chars().take(12).collect(),
                    name,
                    mounts,
                })
        })
        .collect())
}

/// Sandbox supervisors of this root's workspaces, by pid; Linux only.
fn supervisors(root: &NodeRoot) -> Vec<(i32, String)> {
    let root_arg = root.path.to_string_lossy();
    let mut found = Vec::new();
    for entry in read_dir(Path::new("/proc")) {
        let Some(pid) = entry
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(cmdline) = fs::read(entry.join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        let value = |flag: &str| {
            args.iter()
                .position(|arg| arg == flag)
                .and_then(|i| args.get(i + 1))
        };
        let is_supervisor = args
            .windows(2)
            .any(|pair| pair[0] == "sandbox" && pair[1] == "run");
        if is_supervisor
            && value("--node-root").is_some_and(|value| *value == root_arg)
            && let Some(ws_id) = value("--node-workspace")
        {
            found.push((pid, ws_id.clone()));
        }
    }
    found
}

/// Workspace-owned `refs/workmux/**` of every project repository.
fn workmux_refs(root: &NodeRoot) -> Vec<(PathBuf, String)> {
    let mut refs = Vec::new();
    for project in read_dir(&root.path.join("projects")) {
        let repo = project.join("repo");
        if !repo.join(".git").is_dir() {
            continue;
        }
        if let Ok(output) = git(
            &repo,
            &["for-each-ref", "--format=%(refname)", "refs/workmux/"],
        ) {
            refs.extend(
                output
                    .lines()
                    .filter(|line| *line != super::PROJECT_TRANSFER_REF)
                    .map(|line| (repo.clone(), line.to_string())),
            );
        }
    }
    refs
}

/// Per-workspace files of workspaces the node has no record of, and the
/// secret files supervisors and sessions remove once read, when they are
/// older than [`GRACE`].
fn stale_files(root: &NodeRoot, ids: &HashSet<&str>) -> Vec<PathBuf> {
    let old = |path: &Path| {
        fs::symlink_metadata(path)
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| {
                SystemTime::now()
                    .duration_since(modified)
                    .is_ok_and(|age| age >= GRACE)
            })
    };
    let mut files = Vec::new();
    for path in read_dir(&root.workspaces_dir()) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((ws_id, rest)) = name.split_once('.') else {
            continue;
        };
        if rest != "json" && !ids.contains(ws_id) && old(&path) {
            files.push(path);
        }
    }
    for path in read_dir(&root.path.join("secrets")) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let orphan = match name.rsplit_once('.') {
            Some((_, "env" | "tmux")) => true,
            Some((ws_id, "json")) => ws_id.starts_with('w') && !ids.contains(ws_id),
            _ => false,
        };
        if orphan && old(&path) {
            files.push(path);
        }
    }
    files
}

/// Binaries under `bin/` nothing uses: not this one, not one the laptop
/// keeps, not an execution's, not the one agents without a sandbox report
/// through, and not one a process runs. Errs when that cannot be told.
fn unused_pins(
    root: &NodeRoot,
    request: &GcRequest,
    workspaces: &[Workspace],
) -> std::result::Result<Vec<String>, String> {
    let mut used: HashSet<String> = request.keep_pins.iter().cloned().collect();
    if let Some(current) = std::env::current_exe().ok().and_then(|exe| {
        exe.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }) {
        used.insert(current);
    }
    for execution in workspaces.iter().filter_map(|ws| ws.execution.as_ref()) {
        match &execution.pin {
            Some(pin) => {
                used.insert(pin.clone());
            }
            None => {
                return Err(
                    "an execution started by an older workmux does not say which it runs".into(),
                );
            }
        }
    }
    let wrapper =
        fs::read_to_string(root.path.join("run").join("bin").join("workmux")).unwrap_or_default();
    let bin = root.path.join("bin");
    for entry in read_dir(Path::new("/proc")) {
        if let Ok(exe) = fs::read_link(entry.join("exe"))
            && exe.parent() == Some(bin.as_path())
            && let Some(name) = exe.file_name()
        {
            used.insert(
                name.to_string_lossy()
                    .trim_end_matches(" (deleted)")
                    .to_string(),
            );
        }
    }
    let mut pins: Vec<String> = read_dir(&bin)
        .into_iter()
        .filter(|path| path.is_file())
        .filter_map(|path| Some(path.file_name()?.to_str()?.to_string()))
        .filter(|name| name.starts_with("workmux-"))
        .filter(|name| !used.contains(name) && !wrapper.contains(&format!("/bin/{name}")))
        .collect();
    pins.sort();
    Ok(pins)
}

fn read_dir(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_sessions_are_recognized() {
        assert!(is_execution_session("x0123456789ab-0"));
        assert!(is_execution_session("x0123456789ab-12"));
        assert!(!is_execution_session("x0123456789ab-"));
        assert!(!is_execution_session("w0123456789ab-0"));
        assert!(!is_execution_session("login-claude"));
    }
}
