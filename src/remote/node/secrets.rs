//! Secret environment values on the node, kept out of records, argv and
//! logs. Every file lives under `secrets/` (0700) and is 0600:
//!
//! ```text
//! secrets/node.json      node-wide values `workmux remote login` manages
//! secrets/<ws-id>.json   a workspace's environment, read on the laptop when
//!                        the workspace was created
//! secrets/<name>.env     named pipe docker reads a container's env from,
//!                        removed once read
//! secrets/<name>.tmux    tmux commands, removed once sourced
//! ```

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nix::sys::stat::Mode;
use tempfile::TempPath;

use super::NodeRoot;
use crate::remote::protocol::Secrets;

/// The long-lived token `claude setup-token` prints; Claude Code reads it
/// from the environment.
pub const CLAUDE_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

impl NodeRoot {
    fn secrets_dir(&self) -> PathBuf {
        self.path.join("secrets")
    }

    /// `secrets/`, created private to the node user.
    fn private_secrets_dir(&self) -> Result<PathBuf> {
        let dir = self.secrets_dir();
        crate::remote::transport::ensure_private_dir(&dir, unsafe { libc::getuid() })?;
        Ok(dir)
    }

    pub fn workspace_secrets_path(&self, ws_id: &str) -> PathBuf {
        self.secrets_dir().join(format!("{ws_id}.json"))
    }

    fn node_secrets_path(&self) -> PathBuf {
        self.secrets_dir().join("node.json")
    }
}

/// Whether docker's `--env-file` and tmux can carry `value` unchanged.
pub fn representable(value: &str) -> bool {
    !value.contains(['\n', '\r', '\0'])
}

fn load(path: &Path) -> Result<Secrets> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Corrupt secrets file {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Secrets::default()),
        Err(err) => Err(err).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn save(root: &NodeRoot, path: &Path, secrets: &Secrets) -> Result<()> {
    root.private_secrets_dir()?;
    if secrets.0.is_empty() {
        return match fs::remove_file(path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                Err(err).with_context(|| format!("Failed to remove {}", path.display()))
            }
            _ => Ok(()),
        };
    }
    crate::util::write_atomic(path, &serde_json::to_vec(secrets)?)
}

pub fn node_secrets(root: &NodeRoot) -> Result<Secrets> {
    load(&root.node_secrets_path())
}

pub fn set_node_secret(root: &NodeRoot, name: &str, value: &str) -> Result<()> {
    let mut secrets = node_secrets(root)?;
    secrets.0.insert(name.to_string(), value.to_string());
    save(root, &root.node_secrets_path(), &secrets)
}

pub fn save_workspace(root: &NodeRoot, ws_id: &str, secrets: &Secrets) -> Result<()> {
    save(root, &root.workspace_secrets_path(ws_id), secrets)
}

/// What an execution of the workspace gets: its snapshot, with the
/// node-wide token on top for Claude. The token wins because it is the
/// value `remote login` manages.
pub fn for_execution(root: &NodeRoot, ws_id: &str, agent: Option<&str>) -> Result<Secrets> {
    let mut secrets = load(&root.workspace_secrets_path(ws_id))?;
    if agent == Some("claude")
        && let Some(token) = node_secrets(root)?.0.remove(CLAUDE_TOKEN)
    {
        secrets.0.insert(CLAUDE_TOKEN.to_string(), token);
    }
    Ok(secrets)
}

fn checked(secrets: &Secrets) -> Result<impl Iterator<Item = (&String, &String)>> {
    for (name, value) in &secrets.0 {
        if !crate::config::is_valid_env_name(name) {
            bail!("{name:?} is not a valid environment variable name");
        }
        if !representable(value) {
            bail!("the value of {name} holds a line break or NUL");
        }
    }
    Ok(secrets.0.iter())
}

/// A docker `--env-file` holding `secrets`, `NAME=VALUE` per line. It is a
/// named pipe, so the values never rest on disk: the first reader gets them
/// and the pipe is removed, and a pipe nobody read goes when the guard drops.
/// A supervisor killed by a signal therefore leaves no values behind.
pub fn env_file(root: &NodeRoot, name: &str, secrets: &Secrets) -> Result<TempPath> {
    let contents: String = checked(secrets)?
        .map(|(name, value)| format!("{name}={value}\n"))
        .collect();
    let path = root.private_secrets_dir()?.join(format!("{name}.env"));
    let _ = fs::remove_file(&path);
    nix::unistd::mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    let pipe = path.clone();
    std::thread::spawn(move || {
        if let Ok(mut writer) = fs::OpenOptions::new().write(true).open(&pipe) {
            let _ = writer.write_all(contents.as_bytes());
        }
        let _ = fs::remove_file(&pipe);
    });
    Ok(TempPath::try_from_path(path)?)
}

/// Write tmux commands that set `secrets` in `session`'s environment; the
/// file is removed when dropped.
pub fn tmux_file(root: &NodeRoot, session: &str, secrets: &Secrets) -> Result<TempPath> {
    let contents: String = checked(secrets)?
        .map(|(name, value)| {
            format!(
                "set-environment -t {} {name} \"{}\"\n",
                tmux_quote(&format!("={session}")),
                tmux_escape(value)
            )
        })
        .collect();
    let path = root.private_secrets_dir()?.join(format!("{session}.tmux"));
    crate::util::write_atomic(&path, contents.as_bytes())?;
    Ok(TempPath::try_from_path(path)?)
}

/// `value` as a tmux double-quoted string body: `\`, `"`, `$` and `~` are
/// the characters tmux would interpret there.
fn tmux_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '"' | '$' | '~') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

fn tmux_quote(value: &str) -> String {
    format!("\"{}\"", tmux_escape(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    fn root() -> (tempfile::TempDir, NodeRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = NodeRoot::at(dir.path().to_path_buf()).unwrap();
        (dir, root)
    }

    fn secrets(pairs: &[(&str, &str)]) -> Secrets {
        Secrets(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn files_are_private_and_empty_snapshots_leave_none() {
        let (_dir, root) = root();
        save_workspace(&root, "w1", &secrets(&[("GITHUB_TOKEN", "ghp_x")])).unwrap();
        set_node_secret(&root, CLAUDE_TOKEN, "sk-ant-oat01-x").unwrap();
        assert_eq!(mode(&root.secrets_dir()), 0o700);
        assert_eq!(mode(&root.workspace_secrets_path("w1")), 0o600);
        assert_eq!(mode(&root.node_secrets_path()), 0o600);

        save_workspace(&root, "w1", &Secrets::default()).unwrap();
        assert!(!root.workspace_secrets_path("w1").exists());
    }

    #[test]
    fn the_node_token_wins_for_claude_only() {
        let (_dir, root) = root();
        let snapshot = secrets(&[("A", "1"), (CLAUDE_TOKEN, "laptop")]);
        save_workspace(&root, "w1", &snapshot).unwrap();
        assert_eq!(
            for_execution(&root, "w1", Some("claude")).unwrap(),
            snapshot
        );

        set_node_secret(&root, CLAUDE_TOKEN, "node").unwrap();
        let claude = for_execution(&root, "w1", Some("claude")).unwrap();
        assert_eq!(claude.0[CLAUDE_TOKEN], "node");
        assert_eq!(claude.0["A"], "1");
        assert_eq!(for_execution(&root, "w1", Some("codex")).unwrap(), snapshot);
        assert!(
            for_execution(&root, "w2", Some("codex"))
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[test]
    fn debug_shows_names_only() {
        let text = format!("{:?}", secrets(&[("API_KEY", "hunter2")]));
        assert!(text.contains("API_KEY"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn env_file_is_a_private_pipe_read_once_verbatim() {
        let (_dir, root) = root();
        let file = env_file(&root, "c1", &secrets(&[("A", "x = 'y' $Z \"")])).unwrap();
        let path = file.to_path_buf();
        assert_eq!(mode(&path), 0o600);
        assert!(fs::metadata(&path).unwrap().file_type().is_fifo());
        assert_eq!(fs::read_to_string(&path).unwrap(), "A=x = 'y' $Z \"\n");
        for _ in 0..100 {
            if !path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!path.exists());

        let unread = env_file(&root, "c4", &secrets(&[("A", "x")])).unwrap();
        let path = unread.to_path_buf();
        drop(unread);
        assert!(!path.exists());

        assert!(env_file(&root, "c2", &secrets(&[("A", "x\nB=y")])).is_err());
        assert!(env_file(&root, "c3", &secrets(&[("A B", "x")])).is_err());
    }

    #[test]
    fn tmux_file_sets_values_tmux_reads_back_unchanged() {
        if which::which("tmux").is_err() {
            return;
        }
        let (dir, root) = root();
        let socket = dir.path().join("s");
        let tmux = |args: &[&str]| {
            std::process::Command::new("tmux")
                .arg("-S")
                .arg(&socket)
                .args(["-f", "/dev/null"])
                .args(args)
                .env_remove("TMUX")
                .output()
                .unwrap()
        };
        assert!(tmux(&["new-session", "-d", "-s", "x-0"]).status.success());
        let value = r#"~a"b\c$HOME ${X} 'q' #{pane_id} ;"#;
        let file = tmux_file(&root, "x-0", &secrets(&[("TOKEN", value)])).unwrap();
        let sourced = tmux(&["source-file", &file.to_string_lossy()]);
        let shown = tmux(&["show-environment", "-t", "=x-0", "TOKEN"]);
        tmux(&["kill-server"]);
        assert!(sourced.status.success(), "{sourced:?}");
        assert_eq!(
            String::from_utf8_lossy(&shown.stdout),
            format!("TOKEN={value}\n")
        );
    }
}
