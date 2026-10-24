//! Claude's folder-trust dialog for node worktrees.
//!
//! Claude asks whether to trust a folder the first time it starts there,
//! even with permission checks skipped, and waits for an answer nobody is
//! there to give. It trusts a folder when its config marks that folder or a
//! parent as accepted, so one entry for `<root>/projects` covers every
//! worktree the node creates.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use super::{NodeRoot, Workspace};
use crate::config::{AgentEnvValue, Config, RemoteIsolation};

const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Make sure the Claude config the workspace's agent reads trusts the
/// node's worktrees. A failure is reported and leaves the file alone; the
/// agent then shows the dialog.
pub fn ensure_for_execution(root: &NodeRoot, workspace: &Workspace, config: &Config) {
    let (path, seed) = match workspace.isolation {
        RemoteIsolation::Container => match crate::sandbox::SandboxPaths::new() {
            Some(paths) => (paths.config_file, crate::sandbox::CLAUDE_ONBOARDING_JSON),
            None => return,
        },
        RemoteIsolation::None => match host_config_path(root, workspace, config) {
            Some(path) => (path, "{}"),
            None => return,
        },
    };
    if let Err(err) = ensure_trusted(&path, &trusted_dirs(root), seed) {
        eprintln!(
            "workmux: could not mark the node's worktrees as trusted in {}: {err:#}",
            path.display()
        );
    }
}

/// `<root>/projects` as Claude may see it: as given and, when it differs,
/// with symlinks resolved.
fn trusted_dirs(root: &NodeRoot) -> Vec<String> {
    let projects = root.path.join("projects");
    let mut dirs = vec![projects.to_string_lossy().into_owned()];
    if let Ok(canonical) = projects.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !dirs.contains(&canonical) {
            dirs.push(canonical);
        }
    }
    dirs
}

/// The `.claude.json` Claude reads on the node host: under
/// `CLAUDE_CONFIG_DIR` when the agent command, the workspace environment,
/// the login environment or this process sets it, else in the home
/// directory.
fn host_config_path(root: &NodeRoot, workspace: &Workspace, config: &Config) -> Option<PathBuf> {
    let from_command =
        crate::multiplexer::agent::resolve_selected_agent(config, workspace.agent.as_deref())
            .and_then(|agent| {
                let command = agent.command;
                command
                    .env_assignments
                    .iter()
                    .rev()
                    .find_map(|assignment| assignment.strip_prefix("CLAUDE_CONFIG_DIR="))
                    .map(str::to_string)
                    .or_else(|| match command.env.get(CONFIG_DIR_ENV) {
                        Some(AgentEnvValue::Literal(value)) => Some(value.clone()),
                        _ => None,
                    })
            });
    let dir = from_command
        .or_else(|| {
            super::secrets::for_execution(root, &workspace.identity.ws_id, Some("claude"))
                .ok()
                .and_then(|secrets| secrets.0.get(CONFIG_DIR_ENV).cloned())
        })
        .or_else(|| {
            super::login_env()
                .into_iter()
                .find(|(key, _)| key == CONFIG_DIR_ENV)
                .map(|(_, value)| value)
        })
        .or_else(|| std::env::var(CONFIG_DIR_ENV).ok())
        .filter(|dir| !dir.is_empty());
    let home = home::home_dir()?;
    Some(match dir {
        Some(dir) => {
            let dir = dir.trim_matches(['"', '\'']);
            match dir.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(dir),
            }
            .join(".claude.json")
        }
        None => home.join(".claude.json"),
    })
}

/// Mark each of `dirs` as trusted in the Claude config at `path`. Reads
/// first and writes only when an entry is missing, keeping every other key.
/// The file is rewritten in place, so a bind mount of it keeps seeing the
/// new contents. A missing file starts from `seed`; an empty one from `{}`.
/// A file that is not a JSON object is left untouched.
pub fn ensure_trusted(path: &Path, dirs: &[String], seed: &str) -> Result<()> {
    let existed = path.exists();
    let mut file = open_locked(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let source = match text.trim() {
        "" if existed => "{}",
        "" => seed,
        _ => text.as_str(),
    };
    let mut document: Value = serde_json::from_str(source)
        .with_context(|| format!("{} is not valid JSON; not changing it", path.display()))?;
    let Some(object) = document.as_object_mut() else {
        bail!("{} is not a JSON object; not changing it", path.display());
    };
    if !mark_trusted(object, dirs).with_context(|| {
        format!(
            "{} has an unexpected shape; not changing it",
            path.display()
        )
    })? {
        return Ok(());
    }
    let mut bytes = serde_json::to_vec_pretty(&document)?;
    bytes.push(b'\n');
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&bytes)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    file.set_len(bytes.len() as u64)?;
    file.sync_all()?;
    Ok(())
}

/// Open `path` for reading and writing, creating it private when missing,
/// and hold an advisory lock so concurrent workmux writers take turns.
fn open_locked(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("Failed to lock {}", path.display()))?;
    Ok(file)
}

/// Set `projects[dir].hasTrustDialogAccepted` for each of `dirs`; returns
/// whether anything changed.
fn mark_trusted(object: &mut Map<String, Value>, dirs: &[String]) -> Result<bool> {
    let projects = object
        .entry("projects")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("`projects` is not an object")?;
    let mut changed = false;
    for dir in dirs {
        let entry = projects
            .entry(dir.clone())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .with_context(|| format!("`projects[{dir:?}]` is not an object"))?;
        if entry.get("hasTrustDialogAccepted") != Some(&Value::Bool(true)) {
            entry.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
            changed = true;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    const SEED: &str = r#"{"hasCompletedOnboarding":true}"#;

    fn dirs() -> Vec<String> {
        vec!["/node/projects".to_string()]
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn trusted(value: &Value) -> bool {
        value["projects"]["/node/projects"]["hasTrustDialogAccepted"] == Value::Bool(true)
    }

    #[test]
    fn missing_file_starts_from_the_seed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude-sandbox.json");
        ensure_trusted(&path, &dirs(), SEED).unwrap();
        let value = read(&path);
        assert!(trusted(&value));
        assert_eq!(value["hasCompletedOnboarding"], Value::Bool(true));
    }

    #[test]
    fn empty_file_starts_from_an_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, "  \n").unwrap();
        ensure_trusted(&path, &dirs(), SEED).unwrap();
        let value = read(&path);
        assert!(trusted(&value));
        assert!(value.get("hasCompletedOnboarding").is_none());
    }

    #[test]
    fn keeps_other_keys_and_the_inode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        let original = r#"{"oauthAccount":{"x":1},"projects":{"/other":{"allowedTools":["a"]},"/node/projects":{"history":[1]}},"numStartups":3}"#;
        std::fs::write(&path, original).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        ensure_trusted(&path, &dirs(), SEED).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        let value = read(&path);
        assert!(trusted(&value));
        assert_eq!(value["oauthAccount"]["x"], 1);
        assert_eq!(value["numStartups"], 3);
        assert_eq!(value["projects"]["/other"]["allowedTools"][0], "a");
        assert_eq!(value["projects"]["/node/projects"]["history"][0], 1);
    }

    #[test]
    fn present_entry_means_no_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        let original = "{\"projects\": {\"/node/projects\": {\"hasTrustDialogAccepted\": true}}}";
        std::fs::write(&path, original).unwrap();
        ensure_trusted(&path, &dirs(), SEED).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn invalid_files_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        for original in ["{not json", "[1, 2]", r#"{"projects": []}"#] {
            std::fs::write(&path, original).unwrap();
            assert!(ensure_trusted(&path, &dirs(), SEED).is_err(), "{original}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[test]
    fn writes_through_a_symlinked_dotfile() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles-claude.json");
        std::fs::write(&target, "{}").unwrap();
        let link = dir.path().join(".claude.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        ensure_trusted(&link, &dirs(), SEED).unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(trusted(&read(&target)));
    }
}
