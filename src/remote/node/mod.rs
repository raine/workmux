//! The node side of remote workspaces: records under the node root and the
//! verbs that act on them.
//!
//! Layout under the node root:
//!
//! ```text
//! projects/<project-id>/repo                 git repository, base refs pushed here
//! projects/<project-id>/worktrees/<handle>   workspace worktrees
//! projects/<project-id>/lock                 serializes git mutations
//! state/workspaces/<ws-id>.json              workspace record
//! state/workspaces/<ws-id>.config.json       portable config snapshot (0600)
//! state/workspaces/<ws-id>.lock              held for the duration of a verb
//! run/tmux.sock, tmux.conf                   node tmux server
//! ```
//!
//! Verbs never read the remote user's workmux config: a workspace runs the
//! snapshot the laptop sent when it was provisioned.

pub mod attach;
pub mod execution;
pub mod provision;
pub mod remove;

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::protocol::{NodeError, WorkspaceIdentity, code};
use crate::cmd::Cmd;
use crate::config::{Config, RemoteIsolation};

/// Environment variable naming the node root for node verbs.
pub const NODE_ROOT_ENV: &str = "WORKMUX_NODE_ROOT";

const RECORD_VERSION: u32 = 1;
const LOGIN_ENV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// An error the laptop can act on, carrying a [`code`].
pub fn refuse(code: &str, message: impl Into<String>) -> anyhow::Error {
    NodeError {
        code: code.to_string(),
        message: message.into(),
    }
    .into()
}

#[derive(Debug, Clone)]
pub struct NodeRoot {
    pub path: PathBuf,
}

impl NodeRoot {
    pub fn from_env() -> Result<Self> {
        let path = std::env::var_os(NODE_ROOT_ENV)
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("{NODE_ROOT_ENV} is not set"))?;
        Self::at(path)
    }

    pub fn at(path: PathBuf) -> Result<Self> {
        if !path.is_absolute() {
            bail!("node root must be absolute: {}", path.display());
        }
        if !path.is_dir() {
            bail!("node root does not exist: {}", path.display());
        }
        Ok(Self { path })
    }

    fn workspaces_dir(&self) -> PathBuf {
        self.path.join("state").join("workspaces")
    }

    pub fn record_path(&self, ws_id: &str) -> PathBuf {
        self.workspaces_dir().join(format!("{ws_id}.json"))
    }

    pub fn config_path(&self, ws_id: &str) -> PathBuf {
        self.workspaces_dir().join(format!("{ws_id}.config.json"))
    }

    fn lock_path(&self, ws_id: &str) -> PathBuf {
        self.workspaces_dir().join(format!("{ws_id}.lock"))
    }

    fn project_dir(&self, project_id: &str) -> PathBuf {
        self.path.join("projects").join(project_id)
    }

    pub fn repo_path(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join("repo")
    }

    pub fn worktree_path(&self, project_id: &str, handle: &str) -> PathBuf {
        self.project_dir(project_id).join("worktrees").join(handle)
    }

    /// Hold the workspace for the duration of a verb; a concurrent verb on
    /// the same workspace gets `busy` instead of waiting.
    pub fn lock_workspace(&self, ws_id: &str) -> Result<File> {
        fs::create_dir_all(self.workspaces_dir())?;
        let file = open_lock(&self.lock_path(ws_id))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(fs::TryLockError::WouldBlock) => Err(refuse(
                code::BUSY,
                format!("workspace {ws_id} is busy with another request; try again"),
            )),
            Err(fs::TryLockError::Error(err)) => Err(err).context("Failed to lock workspace"),
        }
    }

    /// Serialize repository mutations of one project.
    pub fn lock_project(&self, project_id: &str) -> Result<File> {
        let dir = self.project_dir(project_id);
        fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
        let file = open_lock(&dir.join("lock"))?;
        file.lock().context("Failed to lock project")?;
        Ok(file)
    }

    pub fn load(&self, ws_id: &str) -> Result<Option<Workspace>> {
        let path = self.record_path(ws_id);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .with_context(|| format!("Corrupt workspace record {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("Failed to read {}", path.display())),
        }
    }

    pub fn save(&self, workspace: &Workspace) -> Result<()> {
        fs::create_dir_all(self.workspaces_dir())?;
        let json = serde_json::to_vec_pretty(workspace)?;
        crate::util::write_atomic(&self.record_path(&workspace.identity.ws_id), &json)
    }

    /// Every workspace record, skipping unreadable ones.
    pub fn workspaces(&self) -> Vec<Workspace> {
        let Ok(entries) = fs::read_dir(self.workspaces_dir()) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let ws_id = name.strip_suffix(".json")?;
                if ws_id.ends_with(".config") {
                    return None;
                }
                self.load(ws_id).ok().flatten()
            })
            .collect()
    }

    /// Delete a workspace's records; the lock file goes last.
    pub fn forget(&self, ws_id: &str) -> Result<()> {
        for path in [
            self.config_path(ws_id),
            self.record_path(ws_id),
            self.lock_path(ws_id),
        ] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("Failed to remove {}", path.display()));
                }
            }
        }
        Ok(())
    }
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))
}

/// A workspace as the node knows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub version: u32,
    #[serde(flatten)]
    pub identity: WorkspaceIdentity,
    pub isolation: RemoteIsolation,
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    #[serde(default)]
    pub progress: Progress,
    pub agent: Option<String>,
    /// Directory hooks and panes start in: the config's subdirectory of the
    /// worktree when it exists, else the worktree.
    pub working_dir: Option<PathBuf>,
    pub prompt_path: Option<PathBuf>,
    #[serde(default)]
    pub prompt_file_only: bool,
    pub execution: Option<Execution>,
}

impl Workspace {
    pub fn new(root: &NodeRoot, identity: WorkspaceIdentity, isolation: RemoteIsolation) -> Self {
        Self {
            version: RECORD_VERSION,
            repo_path: root.repo_path(&identity.project_id),
            worktree_path: root.worktree_path(&identity.project_id, &identity.handle),
            identity,
            isolation,
            progress: Progress::default(),
            agent: None,
            working_dir: None,
            prompt_path: None,
            prompt_file_only: false,
            execution: None,
        }
    }

    pub fn working_dir(&self) -> &Path {
        self.working_dir.as_deref().unwrap_or(&self.worktree_path)
    }
}

/// Completed provisioning steps; each is done at most once, except that a
/// hook interrupted before its completion was recorded runs again.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub worktree: bool,
    pub files: bool,
    pub prompt: bool,
    pub git_identity: bool,
    /// Number of `post_create` hooks that finished.
    pub hooks_done: usize,
    /// Index of a hook that started and has not finished.
    pub hook_started: Option<usize>,
    pub ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Starting,
    Running,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub ex_id: String,
    pub state: ExecutionState,
    pub panes: Vec<PaneProgress>,
}

/// A pane whose command was sent to its session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneProgress {
    pub index: usize,
}

/// Ids are generated by the laptop: a prefix letter and 12 hex digits.
pub fn validate_id(id: &str, prefix: char, what: &str) -> Result<()> {
    let valid = id.len() == 13
        && id.starts_with(prefix)
        && id[1..]
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !valid {
        return Err(refuse(code::BAD_REQUEST, format!("invalid {what}: {id:?}")));
    }
    Ok(())
}

pub fn validate_identity(identity: &WorkspaceIdentity) -> Result<()> {
    validate_id(&identity.ws_id, 'w', "workspace id")?;
    let slug = |text: &str| {
        !text.is_empty()
            && !text.starts_with('-')
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if !slug(&identity.project_id) {
        return Err(refuse(
            code::BAD_REQUEST,
            format!("invalid project id: {:?}", identity.project_id),
        ));
    }
    if !slug(&identity.handle) {
        return Err(refuse(
            code::BAD_REQUEST,
            format!("invalid handle: {:?}", identity.handle),
        ));
    }
    let sha = &identity.base_sha;
    if !(sha.len() == 40 || sha.len() == 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(refuse(
            code::BAD_REQUEST,
            format!("invalid base commit: {sha:?}"),
        ));
    }
    let branch_ok = !identity.branch.starts_with('-')
        && Cmd::new("git")
            .args(&["check-ref-format", "--branch", &identity.branch])
            .run_as_check()?;
    if !branch_ok {
        return Err(refuse(
            code::BAD_REQUEST,
            format!("invalid branch name: {:?}", identity.branch),
        ));
    }
    Ok(())
}

/// `git` in `dir`, failing with its stderr.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    Cmd::new("git")
        .workdir(dir)
        .args(args)
        .run_and_capture_stdout()
}

/// Ref the laptop pushes a workspace's base commit to.
pub fn base_ref(ws_id: &str) -> String {
    format!("refs/workmux/base/{ws_id}")
}

/// Environment for a workspace's lifecycle hooks: the login environment
/// plus the variables local hooks get. `WM_CONFIG_DIR` is only set for
/// hooks that run in the config directory.
pub fn hook_env(workspace: &Workspace, config_dir: Option<&Path>) -> Vec<(String, String)> {
    let mut env = login_env();
    let handle = workspace.identity.handle.clone();
    env.push(("WORKMUX_HANDLE".into(), handle.clone()));
    env.push(("WM_HANDLE".into(), handle));
    env.push((
        "WM_WORKTREE_PATH".into(),
        workspace.worktree_path.to_string_lossy().into_owned(),
    ));
    env.push((
        "WM_PROJECT_ROOT".into(),
        workspace.repo_path.to_string_lossy().into_owned(),
    ));
    if let Some(dir) = config_dir {
        env.push(("WM_CONFIG_DIR".into(), dir.to_string_lossy().into_owned()));
    }
    env
}

/// Run one hook command in `dir`; its output goes to stderr, which the
/// laptop shows as it arrives.
pub fn run_hook(
    config: &Config,
    command: &str,
    dir: &Path,
    env: &[(String, String)],
) -> Result<()> {
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    crate::cmd::shell_command_with_env_mode(
        config.hook_shell.as_deref(),
        command,
        dir,
        &env,
        crate::cmd::ShellOutput::RedirectToStderr,
    )
}

/// Environment of a login shell, so hooks see the PATH and variables an
/// interactive login would. Falls back to the current environment.
fn login_env() -> Vec<(String, String)> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut cmd = std::process::Command::new(&shell);
    cmd.args(["-l", "-c", "env -0"]);
    let outcome = crate::remote::transport::run_bounded(
        cmd,
        std::process::Stdio::null(),
        Some(LOGIN_ENV_TIMEOUT),
    );
    match outcome {
        Ok(Some(output)) if output.success() => parse_env0(&output.stdout),
        outcome => {
            let detail = match outcome {
                Ok(Some(output)) => output.stderr_line(),
                Ok(None) => "timed out".to_string(),
                Err(err) => format!("{err:#}"),
            };
            eprintln!(
                "workmux: could not read the login environment of {shell} ({detail}); \
                 hooks run with the ssh session environment"
            );
            Vec::new()
        }
    }
}

/// Parse `env -0` output, ignoring anything a login script printed first.
fn parse_env0(bytes: &[u8]) -> Vec<(String, String)> {
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let entry = String::from_utf8_lossy(entry);
            let entry = entry.rsplit_once('\n').map_or(&*entry, |(_, last)| last);
            let (key, value) = entry.split_once('=')?;
            let valid = key
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            valid.then(|| (key.to_string(), value.to_string()))
        })
        .collect()
}

/// Docker containers with a mount of `path`, running or not.
pub fn containers_mounting(path: &Path) -> Result<Vec<String>> {
    if which::which("docker").is_err() {
        return Ok(Vec::new());
    }
    let filter = format!("volume={}", path.display());
    let output = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &filter])
        .output()
        .context("Failed to run docker ps")?;
    if !output.status.success() {
        bail!(
            "docker ps failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect())
}

/// Remove every container that mounts `path` and prove none is left.
pub fn stop_containers_mounting(path: &Path) -> Result<()> {
    let ids = containers_mounting(path)?;
    if ids.is_empty() {
        return Ok(());
    }
    let mut cmd = std::process::Command::new("docker");
    cmd.args(["rm", "-f"]).args(&ids);
    let _ = cmd.output();
    let left = containers_mounting(path)?;
    if !left.is_empty() {
        bail!(
            "containers still mount {}: {}",
            path.display(),
            left.join(" ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> WorkspaceIdentity {
        WorkspaceIdentity {
            ws_id: "w0123456789ab".into(),
            project_id: "repo-0123456789ab".into(),
            handle: "feature-x".into(),
            branch: "feature/x".into(),
            base_sha: "a".repeat(40),
        }
    }

    fn code_of(err: anyhow::Error) -> String {
        err.downcast::<NodeError>().unwrap().code
    }

    #[test]
    fn identity_validation() {
        validate_identity(&identity()).unwrap();
        for bad in [
            WorkspaceIdentity {
                ws_id: "x0123456789ab".into(),
                ..identity()
            },
            WorkspaceIdentity {
                handle: "../etc".into(),
                ..identity()
            },
            WorkspaceIdentity {
                project_id: "a/b".into(),
                ..identity()
            },
            WorkspaceIdentity {
                branch: "bad..branch".into(),
                ..identity()
            },
            WorkspaceIdentity {
                base_sha: "HEAD".into(),
                ..identity()
            },
        ] {
            assert_eq!(code_of(validate_identity(&bad).unwrap_err()), "bad_request");
        }
    }

    #[test]
    fn env0_skips_banner_text() {
        let parsed = parse_env0(b"Welcome!\nPATH=/usr/bin\0HOME=/home/u\0A=b=c\0junk\0");
        assert_eq!(
            parsed,
            vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("HOME".to_string(), "/home/u".to_string()),
                ("A".to_string(), "b=c".to_string()),
            ]
        );
    }

    #[test]
    fn concurrent_lock_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let root = NodeRoot::at(dir.path().to_path_buf()).unwrap();
        let held = root.lock_workspace("w0123456789ab").unwrap();
        assert_eq!(
            code_of(root.lock_workspace("w0123456789ab").unwrap_err()),
            "busy"
        );
        drop(held);
        // A process forked by a parallel test shares the lock until it execs.
        let relocked = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            root.lock_workspace("w0123456789ab").is_ok()
        });
        assert!(relocked);
    }

    #[test]
    fn records_round_trip_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let root = NodeRoot::at(dir.path().to_path_buf()).unwrap();
        let ws = Workspace::new(&root, identity(), RemoteIsolation::None);
        root.save(&ws).unwrap();
        fs::write(root.config_path("w0123456789ab"), "{}").unwrap();
        let listed = root.workspaces();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].identity, identity());
        root.forget("w0123456789ab").unwrap();
        assert!(root.load("w0123456789ab").unwrap().is_none());
    }
}
