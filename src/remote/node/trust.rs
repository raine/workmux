//! Folder-trust dialogs for node worktrees.
//!
//! Claude and Codex ask whether to trust a folder the first time they start
//! there, even with permission checks skipped, and wait for an answer nobody
//! is there to give.
//!
//! - Claude trusts a folder when its config marks that folder or a parent as
//!   accepted, so one entry for `<root>/projects` covers every worktree the
//!   node creates.
//! - Codex looks up only the working directory and the main repository a
//!   worktree belongs to, so each project's node repository gets an entry.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use super::{NodeRoot, Workspace};
use crate::config::{AgentEnvValue, Config, RemoteIsolation};

const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Make sure the config the workspace's agent reads trusts the node's
/// worktrees, for agents that ask. A failure is reported and leaves the
/// file alone; the agent then shows the dialog.
pub fn ensure_for_execution(root: &NodeRoot, workspace: &Workspace, config: &Config) {
    let result = match workspace.agent_kind(config) {
        Some("claude") => ensure_claude(root, workspace, config),
        Some("codex") => ensure_codex(root, workspace, config),
        _ => return,
    };
    if let Err(err) = result {
        eprintln!("workmux: could not mark the node's worktrees as trusted: {err:#}");
    }
}

fn ensure_claude(root: &NodeRoot, workspace: &Workspace, config: &Config) -> Result<()> {
    let (path, seed) = match workspace.isolation {
        RemoteIsolation::Container => match crate::sandbox::SandboxPaths::new() {
            Some(paths) => (paths.config_file, crate::sandbox::CLAUDE_ONBOARDING_JSON),
            None => return Ok(()),
        },
        RemoteIsolation::None => {
            match host_dir(root, workspace, config, "claude", CLAUDE_CONFIG_DIR_ENV, "") {
                Some(dir) => (dir.join(".claude.json"), "{}"),
                None => return Ok(()),
            }
        }
    };
    ensure_trusted(&path, &with_canonical(&root.path.join("projects")), seed)
}

/// Codex's `config.toml` in the codex home the agent uses: the one mounted
/// into the container, or the host's.
fn ensure_codex(root: &NodeRoot, workspace: &Workspace, config: &Config) -> Result<()> {
    let home = match workspace.isolation {
        RemoteIsolation::Container => config.sandbox.resolved_agent_config_dir("codex"),
        RemoteIsolation::None => {
            host_dir(root, workspace, config, "codex", CODEX_HOME_ENV, ".codex")
        }
    };
    let Some(home) = home else {
        return Ok(());
    };
    ensure_codex_trusted(
        &home.join("config.toml"),
        &with_canonical(&workspace.repo_path),
    )
}

/// `path` as given and, when it differs, with symlinks resolved.
fn with_canonical(path: &Path) -> Vec<String> {
    let mut dirs = vec![path.to_string_lossy().into_owned()];
    if let Ok(canonical) = path.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !dirs.contains(&canonical) {
            dirs.push(canonical);
        }
    }
    dirs
}

/// `name` in the environment of the workspace's agent on the node host:
/// from the agent's `env` (which the command line sets last), its inline
/// assignments, the workspace's secrets, the login environment or this
/// process. An `env` entry taken `from_env` reads that variable instead.
fn host_env(
    root: &NodeRoot,
    workspace: &Workspace,
    config: &Config,
    kind: &str,
    name: &str,
) -> Option<String> {
    let mut name = name.to_string();
    let command =
        crate::multiplexer::agent::resolve_selected_agent(config, workspace.agent.as_deref())
            .map(|agent| agent.command);
    let from_command = command.and_then(|command| match command.env.get(&name) {
        Some(AgentEnvValue::Literal(value)) => Some(value.clone()),
        Some(AgentEnvValue::FromEnv { from_env }) => {
            name = from_env.clone();
            None
        }
        None => {
            let prefix = format!("{name}=");
            command
                .env_assignments
                .iter()
                .rev()
                .find_map(|assignment| assignment.strip_prefix(prefix.as_str()))
                .map(str::to_string)
        }
    });
    from_command
        .or_else(|| {
            super::secrets::for_execution(root, &workspace.identity.ws_id, Some(kind))
                .ok()
                .and_then(|secrets| secrets.0.get(&name).cloned())
        })
        .or_else(|| {
            super::login_env()
                .into_iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value)
        })
        .or_else(|| std::env::var(&name).ok())
        .filter(|dir| !dir.is_empty())
}

/// The agent's directory on the node host: `name` from its environment
/// (unquoted, with `~/` expanded, relative to the agent's working
/// directory), else `default` under the home directory.
fn host_dir(
    root: &NodeRoot,
    workspace: &Workspace,
    config: &Config,
    kind: &str,
    name: &str,
    default: &str,
) -> Option<PathBuf> {
    let home = home::home_dir()?;
    let Some(dir) = host_env(root, workspace, config, kind, name) else {
        return Some(home.join(default));
    };
    let dir = dir.trim_matches(['"', '\'']);
    Some(match dir.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => workspace.working_dir().join(dir),
    })
}

/// Mark each of `dirs` as trusted in the Claude config at `path`. Reads
/// first and writes only when an entry is missing, keeping every other key.
/// The file is rewritten in place, so a bind mount of it keeps seeing the
/// new contents. A missing file starts from `seed`; an empty one from `{}`.
/// A file that is not a JSON object is left untouched.
pub fn ensure_trusted(path: &Path, dirs: &[String], seed: &str) -> Result<()> {
    let existed = path.exists();
    edit_locked(path, |text| {
        let source = match text.trim() {
            "" if existed => "{}",
            "" => seed,
            _ => text,
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
            return Ok(None);
        }
        let mut text = serde_json::to_string_pretty(&document)?;
        text.push('\n');
        Ok(Some(text))
    })
}

/// Give each of `dirs` `trust_level = "trusted"` under `[projects]` in the
/// Codex config at `path`, unless it already has a trust level (an
/// explicit "untrusted" stays). Reads first and writes only when something
/// is missing, keeping the rest of the file as written. A missing or empty
/// file starts empty; a file that is not valid TOML, or whose `projects`
/// has an unexpected shape, is left untouched.
pub fn ensure_codex_trusted(path: &Path, dirs: &[String]) -> Result<()> {
    edit_locked(path, |text| {
        let mut document: toml_edit::DocumentMut = text
            .parse()
            .with_context(|| format!("{} is not valid TOML; not changing it", path.display()))?;
        let changed = mark_codex_trusted(&mut document, dirs).with_context(|| {
            format!(
                "{} has an unexpected shape; not changing it",
                path.display()
            )
        })?;
        Ok(changed.then(|| document.to_string()))
    })
}

/// Read `path` under the lock, and write back what `edit` returns, if
/// anything. The file is rewritten in place, so a bind mount of it keeps
/// seeing the new contents.
fn edit_locked(path: &Path, edit: impl FnOnce(&str) -> Result<Option<String>>) -> Result<()> {
    let mut file = open_locked(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let Some(text) = edit(&text)? else {
        return Ok(());
    };
    file.seek(SeekFrom::Start(0))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("Failed to write {}", path.display()))?;
    file.set_len(text.len() as u64)?;
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

/// Set `projects[dir].trust_level` for each of `dirs` that has none;
/// returns whether anything changed.
fn mark_codex_trusted(document: &mut toml_edit::DocumentMut, dirs: &[String]) -> Result<bool> {
    let projects = document.entry("projects").or_insert_with(|| {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        toml_edit::Item::Table(table)
    });
    let projects = projects
        .as_table_like_mut()
        .context("`projects` is not a table")?;
    let mut changed = false;
    for dir in dirs {
        if projects.get(dir).is_none() {
            projects.insert(dir, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let entry = projects
            .get_mut(dir)
            .and_then(|item| item.as_table_like_mut())
            .with_context(|| format!("`projects.{dir:?}` is not a table"))?;
        if !entry.contains_key("trust_level") {
            entry.insert("trust_level", toml_edit::value("trusted"));
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

    const REPO: &str = "/node/projects/app-0123456789ab/repo";

    fn codex_dirs() -> Vec<String> {
        vec![REPO.to_string()]
    }

    fn codex_trust(path: &Path) -> Option<String> {
        let document: toml_edit::DocumentMut =
            std::fs::read_to_string(path).unwrap().parse().unwrap();
        document["projects"][REPO]["trust_level"]
            .as_str()
            .map(str::to_string)
    }

    #[test]
    fn codex_missing_and_empty_files_get_an_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex/config.toml");
        ensure_codex_trusted(&path, &codex_dirs()).unwrap();
        assert_eq!(codex_trust(&path).as_deref(), Some("trusted"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("[projects.\"{REPO}\"]\ntrust_level = \"trusted\"\n")
        );

        std::fs::write(&path, "\n").unwrap();
        ensure_codex_trusted(&path, &codex_dirs()).unwrap();
        assert_eq!(codex_trust(&path).as_deref(), Some("trusted"));
    }

    #[test]
    fn codex_keeps_the_rest_of_the_file_and_the_inode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# my settings\nmodel = \"o3\"  # pinned\n\n[features]\nhooks = true\n\n[projects.\"/other\"]\ntrust_level = \"trusted\"\n";
        std::fs::write(&path, original).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        ensure_codex_trusted(&path, &codex_dirs()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(original), "{text}");
        assert_eq!(codex_trust(&path).as_deref(), Some("trusted"));

        // Trusted now: nothing to write.
        ensure_codex_trusted(&path, &codex_dirs()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn codex_fills_in_a_project_without_a_trust_level() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, format!("[projects.\"{REPO}\"]\n# note\n")).unwrap();
        ensure_codex_trusted(&path, &codex_dirs()).unwrap();
        assert_eq!(codex_trust(&path).as_deref(), Some("trusted"));
        assert!(std::fs::read_to_string(&path).unwrap().contains("# note"));

        std::fs::write(&path, format!("projects = {{ \"{REPO}\" = {{}} }}\n")).unwrap();
        ensure_codex_trusted(&path, &codex_dirs()).unwrap();
        assert_eq!(codex_trust(&path).as_deref(), Some("trusted"));
    }

    #[test]
    fn codex_keeps_an_explicit_decision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = format!("[projects.\"{REPO}\"]\ntrust_level = \"untrusted\"\n");
        std::fs::write(&path, &original).unwrap();
        ensure_codex_trusted(&path, &codex_dirs()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn codex_invalid_files_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let bad = [
            "not = [valid".to_string(),
            "projects = 3\n".to_string(),
            format!("[projects]\n\"{REPO}\" = \"yes\"\n"),
        ];
        for original in bad {
            std::fs::write(&path, &original).unwrap();
            assert!(
                ensure_codex_trusted(&path, &codex_dirs()).is_err(),
                "{original}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[test]
    fn host_env_prefers_the_agent_env_over_inline_assignments() {
        let dir = tempfile::tempdir().unwrap();
        let root = NodeRoot {
            path: dir.path().to_path_buf(),
        };
        let mut workspace = Workspace::new(
            &root,
            crate::remote::protocol::WorkspaceIdentity {
                ws_id: "w0123456789ab".into(),
                project_id: "app-0123456789ab".into(),
                handle: "feature".into(),
                branch: "feature".into(),
                base_sha: "a".repeat(40),
            },
            RemoteIsolation::None,
        );
        workspace.agent = Some("cx".into());
        let config: Config = serde_yaml::from_str(
            "agents:\n  cx:\n    command: CODEX_HOME=/srv/codex-a codex\n    type: codex\n    env:\n      CODEX_HOME: /srv/codex-b\n",
        )
        .unwrap();
        assert_eq!(
            host_env(&root, &workspace, &config, "codex", CODEX_HOME_ENV).as_deref(),
            Some("/srv/codex-b")
        );
        let config: Config = serde_yaml::from_str(
            "agents:\n  cx:\n    command: CODEX_HOME=/srv/codex-a codex\n    type: codex\n",
        )
        .unwrap();
        assert_eq!(
            host_env(&root, &workspace, &config, "codex", CODEX_HOME_ENV).as_deref(),
            Some("/srv/codex-a")
        );
        let config: Config = serde_yaml::from_str(
            "agents:\n  cx:\n    command: CODEX_HOME=/srv/codex-a codex\n    type: codex\n    env:\n      CODEX_HOME:\n        from_env: HOME\n",
        )
        .unwrap();
        assert_eq!(
            host_env(&root, &workspace, &config, "codex", CODEX_HOME_ENV),
            std::env::var("HOME").ok()
        );
        let config: Config = serde_yaml::from_str(
            "agents:\n  cx:\n    command: CODEX_HOME=/srv/codex-a codex\n    type: codex\n    env:\n      CODEX_HOME:\n        from_env: WORKMUX_TEST_UNSET_CODEX_HOME\n",
        )
        .unwrap();
        assert_eq!(
            host_env(&root, &workspace, &config, "codex", CODEX_HOME_ENV),
            None
        );
        let config: Config = serde_yaml::from_str(
            "agents:\n  cx:\n    command: CODEX_HOME=.codex-state codex\n    type: codex\n",
        )
        .unwrap();
        assert_eq!(
            host_dir(
                &root,
                &workspace,
                &config,
                "codex",
                CODEX_HOME_ENV,
                ".codex"
            ),
            Some(workspace.working_dir().join(".codex-state"))
        );
    }
}
