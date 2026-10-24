//! Laptop records of remote workspaces, under
//! `~/.local/state/workmux/remote/<name>/workspaces/<ws-id>.json`.
//!
//! A record is the laptop's handle on a node workspace: it exists from the
//! first provisioning call until the node acknowledges removal, whether or
//! not a local window is attached. Nothing here prunes records on its own.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::PaneConfig;
use crate::remote::git::Snapshot;
use crate::remote::protocol::DiffMode;

const RECORD_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    /// Created on the laptop; the node may not have finished provisioning.
    Provisioning,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub ex_id: String,
    /// Indices of the configured panes with a remote session.
    pub panes: Vec<usize>,
    /// Pinned node binary that started the execution; attaching uses it, so
    /// a laptop upgrade does not strand running agents.
    pub node_bin: String,
}

/// How far bringing the workspace's work to the laptop got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Homecoming {
    /// A local copy of `snapshot` is being made at `local_path`; anything
    /// there belongs to this transfer. `created_branch`: the local branch
    /// did not exist before.
    Hydrating {
        snapshot: Snapshot,
        created_branch: bool,
    },
    /// Merging the local copy stopped on conflicts for the user to resolve
    /// there.
    Conflicted {
        snapshot: Snapshot,
        created_branch: bool,
    },
    /// `snapshot` is merged; only the remote workspace is left to remove.
    Merged { snapshot: Snapshot },
    /// `snapshot` is a local worktree now; only the remote workspace is left
    /// to remove.
    Pulled { snapshot: Snapshot },
}

impl Homecoming {
    /// Whether only the removal of the remote workspace is left.
    pub fn cleanup_pending(&self) -> bool {
        matches!(self, Self::Merged { .. } | Self::Pulled { .. })
    }
}

/// A pane of the workspace's window, as the node laid out its sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutPane {
    /// Index in the configured pane list.
    pub index: usize,
    pub pane: PaneConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub version: u32,
    pub ws_id: String,
    pub remote: String,
    pub project_id: String,
    /// Canonical main worktree of the local repository.
    pub project_root: PathBuf,
    pub handle: String,
    pub branch: String,
    pub base_sha: String,
    /// The ref the base was resolved from, for display.
    pub base_label: Option<String>,
    /// Full local window name.
    pub window_name: String,
    /// Worktree path on the node, once known.
    pub remote_worktree: Option<String>,
    pub state: WorkspaceState,
    pub layout: Vec<LayoutPane>,
    pub execution: Option<ExecutionRecord>,
    pub created_at: u64,
    /// When the node last answered a lifecycle call for this workspace.
    pub observed_at: u64,
    /// Where a local worktree of the handle would live. Remote agents show
    /// under this path, so views group and name them like local ones.
    pub local_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homecoming: Option<Homecoming>,
    /// Project repository path on the node, once an export reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_repo: Option<String>,
}

impl WorkspaceRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ws_id: String,
        remote: &str,
        project_id: String,
        project_root: PathBuf,
        handle: &str,
        branch: &str,
        base_sha: String,
        base_label: Option<String>,
        window_name: String,
        local_path: PathBuf,
    ) -> Self {
        let now = now();
        Self {
            version: RECORD_VERSION,
            ws_id,
            remote: remote.to_string(),
            project_id,
            project_root,
            handle: handle.to_string(),
            branch: branch.to_string(),
            base_sha,
            base_label,
            window_name,
            remote_worktree: None,
            state: WorkspaceState::Provisioning,
            layout: Vec::new(),
            execution: None,
            created_at: now,
            observed_at: now,
            local_path,
            homecoming: None,
            remote_repo: None,
        }
    }

    /// Whether only the removal of the remote workspace is left.
    pub fn cleanup_pending(&self) -> bool {
        self.homecoming
            .as_ref()
            .is_some_and(Homecoming::cleanup_pending)
    }

    /// The recorded base when it is another local branch, which a local
    /// worktree of the branch would have stored as its base.
    pub fn base_label_branch(&self) -> Option<&str> {
        let dir = Some(self.project_root.as_path());
        self.base_label.as_deref().filter(|base| {
            *base != self.branch && crate::git::local_branch_exists_in(base, dir).unwrap_or(false)
        })
    }

    /// The branch the workspace's work merges into: its recorded base when
    /// that is another local branch, else the base the local branch has
    /// stored, else the default branch.
    pub fn base_branch(&self) -> String {
        let dir = Some(self.project_root.as_path());
        self.base_label_branch()
            .map(str::to_string)
            .or_else(|| crate::git::get_branch_base_in(&self.branch, dir).ok())
            .unwrap_or_else(|| {
                crate::git::get_default_branch_in(dir).unwrap_or_else(|_| "main".to_string())
            })
    }

    /// Record that the node just answered for this workspace.
    pub fn observed(&mut self) {
        self.observed_at = now();
    }

    pub fn save(&self) -> Result<()> {
        let path = record_path(&root()?, &self.remote, &self.ws_id);
        let dir = path.parent().expect("record path has a parent");
        fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
        let json = serde_json::to_vec_pretty(self)?;
        crate::util::write_atomic(&path, &json)
    }

    pub fn delete(&self) -> Result<()> {
        for mode in [DiffMode::Wip, DiffMode::Branch] {
            let _ = fs::remove_file(diff_path(&self.remote, &self.ws_id, mode)?);
        }
        let path = record_path(&root()?, &self.remote, &self.ws_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("Failed to remove {}", path.display())),
        }
    }
}

/// Where the diff of workspace `ws_id` last fetched from `remote` is kept.
pub fn diff_path(remote: &str, ws_id: &str, mode: DiffMode) -> Result<PathBuf> {
    let mode = match mode {
        DiffMode::Wip => "wip",
        DiffMode::Branch => "branch",
    };
    Ok(remote_dir(remote)?
        .join("diff")
        .join(format!("{ws_id}-{mode}.json")))
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn root() -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?.join("remote"))
}

/// Directory of the laptop's files about remote `name`.
pub fn remote_dir(name: &str) -> Result<PathBuf> {
    Ok(root()?.join(name))
}

fn record_path(root: &Path, remote: &str, ws_id: &str) -> PathBuf {
    root.join(remote)
        .join("workspaces")
        .join(format!("{ws_id}.json"))
}

/// Every readable record; empty when no remote workspace was ever created.
pub fn all() -> Vec<WorkspaceRecord> {
    let Ok(root) = root() else {
        return Vec::new();
    };
    records_in(&root)
}

fn records_in(root: &Path) -> Vec<WorkspaceRecord> {
    let Ok(remotes) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for remote in remotes.flatten() {
        let Ok(entries) = fs::read_dir(remote.path().join("workspaces")) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            match fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| Ok(serde_json::from_slice::<WorkspaceRecord>(&bytes)?))
            {
                Ok(record) => records.push(record),
                Err(err) => {
                    tracing::warn!(path = %path.display(), "unreadable remote record: {err:#}")
                }
            }
        }
    }
    records.sort_by_key(|record| record.created_at);
    records
}

/// Records of the repository whose main worktree is `project_root`.
pub fn for_project(project_root: &Path) -> Vec<WorkspaceRecord> {
    let project_root = crate::util::canon_or_self(project_root);
    all()
        .into_iter()
        .filter(|record| record.project_root == project_root)
        .collect()
}

/// The workspace named `name`: by handle first, then by branch, the way
/// local worktrees resolve.
fn pick(records: Vec<WorkspaceRecord>, name: &str) -> Option<WorkspaceRecord> {
    let mut by_branch = None;
    for record in records {
        if record.handle == name {
            return Some(record);
        }
        if record.branch == name && by_branch.is_none() {
            by_branch = Some(record);
        }
    }
    by_branch
}

/// The remote workspace of this repository that already uses `handle` or
/// `branch`, preferring the one named `handle`. A branch belongs to one
/// workspace, local or remote, so names resolve to a single place.
pub fn claimed(project_root: &Path, handle: &str, branch: &str) -> Option<WorkspaceRecord> {
    let records = for_project(project_root);
    let by_handle = records.iter().find(|record| record.handle == handle);
    by_handle
        .or_else(|| records.iter().find(|record| record.branch == branch))
        .cloned()
}

/// The workspace on `remote` whose current execution is `ex_id`.
pub fn by_execution(remote: &str, ex_id: &str) -> Option<WorkspaceRecord> {
    all().into_iter().find(|record| {
        record.remote == remote
            && record
                .execution
                .as_ref()
                .is_some_and(|execution| execution.ex_id == ex_id)
    })
}

/// The remote workspace named `name` in the current repository, if any.
///
/// Costs nothing unless a remote workspace was created at some point.
pub fn find_here(name: &str) -> Option<WorkspaceRecord> {
    let root = root().ok()?;
    if !root.is_dir() {
        return None;
    }
    let project_root = crate::git::get_main_worktree_root().ok()?;
    // A pulled workspace's handle belongs to its local worktree.
    let records = for_project(&project_root)
        .into_iter()
        .filter(|record| !matches!(record.homecoming, Some(Homecoming::Pulled { .. })))
        .collect();
    let record = pick(records, name)?;
    // Handles outrank branches: a local worktree named `name` wins over a
    // remote workspace that only matched by branch.
    if record.handle != name && local_handle_exists(&project_root, name) {
        return None;
    }
    Some(record)
}

fn local_handle_exists(project_root: &Path, name: &str) -> bool {
    crate::git::list_worktrees_in(Some(project_root)).is_ok_and(|worktrees| {
        worktrees
            .iter()
            .any(|(path, _)| path.file_name().is_some_and(|dir| dir == name))
    })
}

/// Refuse a command that only works on local worktrees when `name` is a
/// remote workspace.
pub fn reject_remote_handle(name: &str, command: &str) -> Result<()> {
    match find_here(name) {
        Some(record) => Err(anyhow!(
            "'{}' is a remote workspace on '{}'; `workmux {command}` does not support \
             remote workspaces yet",
            record.handle,
            record.remote
        )),
        None => Ok(()),
    }
}

/// A fresh workspace (`w`) or execution (`x`) id.
pub fn new_id(prefix: char) -> String {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("failed to get random bytes");
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{prefix}{hex}")
}

/// Stable id of a repository on every node: its directory name for humans
/// and a hash of where it lives and where it comes from.
pub fn project_id(main_worktree: &Path, origin_url: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(main_worktree.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update(origin_url.unwrap_or_default().as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let name = main_worktree
        .file_name()
        .map(|name| slug::slugify(name.to_string_lossy()))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_string());
    format!("{name}-{}", &hash[..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(ws: &str, handle: &str, branch: &str) -> WorkspaceRecord {
        WorkspaceRecord::new(
            ws.into(),
            "r",
            "p".into(),
            PathBuf::from("/repo"),
            handle,
            branch,
            "0".repeat(40),
            None,
            format!("wm-{handle}"),
            PathBuf::from("/repo__worktrees").join(handle),
        )
    }

    #[test]
    fn ids_have_prefix_and_differ() {
        let a = new_id('w');
        assert!(a.starts_with('w'));
        assert_eq!(a.len(), 13);
        assert!(a[1..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, new_id('w'));
    }

    #[test]
    fn project_id_depends_on_path_and_origin() {
        let a = project_id(Path::new("/src/My Repo"), Some("git@x:me/r.git"));
        assert!(a.starts_with("my-repo-"), "{a}");
        assert_eq!(
            a,
            project_id(Path::new("/src/My Repo"), Some("git@x:me/r.git"))
        );
        assert_ne!(a, project_id(Path::new("/src/My Repo"), None));
        assert_ne!(
            a,
            project_id(Path::new("/other/My Repo"), Some("git@x:me/r.git"))
        );
    }

    #[test]
    fn handle_wins_over_branch() {
        let records = vec![record("w1", "a", "b"), record("w2", "b", "c")];
        assert_eq!(pick(records.clone(), "b").unwrap().ws_id, "w2");
        assert_eq!(pick(records.clone(), "c").unwrap().ws_id, "w2");
        assert!(pick(records, "z").is_none());
    }

    #[test]
    fn records_round_trip_through_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record("w1", "a", "b");
        let path = record_path(dir.path(), &rec.remote, &rec.ws_id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec(&rec).unwrap()).unwrap();
        fs::write(path.with_file_name("junk.json"), b"{").unwrap();
        let read = records_in(dir.path());
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].handle, "a");
        assert!(records_in(&dir.path().join("missing")).is_empty());
    }
}
