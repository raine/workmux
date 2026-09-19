use std::fs::{File, OpenOptions};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::config::{Config, ConfigLocation};

/// Private config transport set only on host Workmux child commands created by RPC.
pub const FROZEN_CONFIG_ENV: &str = "WORKMUX_FROZEN_CONFIG";
const SNAPSHOT_VERSION: u32 = 1;

/// Shell assignment that preserves a frozen config for a child command.
pub fn shell_assignment() -> Option<String> {
    shell_assignment_with(|key| std::env::var_os(key))
}

fn shell_assignment_with(
    get_env: impl FnOnce(&str) -> Option<std::ffi::OsString>,
) -> Option<String> {
    get_env(FROZEN_CONFIG_ENV).map(|path| {
        format!(
            "{}='{}'",
            FROZEN_CONFIG_ENV,
            crate::shell::shell_escape(&path.to_string_lossy())
        )
    })
}

#[derive(Serialize, Deserialize)]
struct FrozenConfigFile {
    version: u32,
    config: Config,
    selected_agent: Option<String>,
    agent_type: Option<String>,
    #[serde(default)]
    agent_declared_config_dir: Option<PathBuf>,
    location: Option<ConfigLocation>,
}

/// Owns a secrets-bearing snapshot in the host's temporary directory.
pub struct FrozenConfigGuard {
    file: NamedTempFile,
}

impl FrozenConfigGuard {
    pub fn capture(
        config: &Config,
        location: Option<&ConfigLocation>,
        worktree: &Path,
    ) -> Result<Self> {
        let directory = snapshot_directory(worktree)?;
        Self::capture_in(config, location, &directory)
    }

    pub(crate) fn capture_in(
        config: &Config,
        location: Option<&ConfigLocation>,
        directory: &Path,
    ) -> Result<Self> {
        let snapshot = FrozenConfigFile {
            version: SNAPSHOT_VERSION,
            config: config.clone(),
            selected_agent: config.selected_agent.clone(),
            agent_type: config.agent_type.clone(),
            agent_declared_config_dir: config.sandbox.agent_declared_config_dir.clone(),
            location: location.cloned(),
        };
        let mut file = tempfile::Builder::new()
            .prefix("workmux-frozen-config-")
            .suffix(".json")
            .tempfile_in(directory)
            .context("Failed to create frozen configuration snapshot")?;

        serde_json::to_writer(file.as_file_mut(), &snapshot)
            .context("Failed to write frozen configuration snapshot")?;
        file.flush()
            .context("Failed to flush frozen configuration snapshot")?;

        Ok(Self { file })
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }
}

fn snapshot_directory(worktree: &Path) -> Result<PathBuf> {
    let temporary_directory = std::env::temp_dir();
    let directory = temporary_directory.canonicalize().with_context(|| {
        format!(
            "Failed to resolve host temporary directory: {}",
            temporary_directory.display()
        )
    })?;
    let worktree = worktree
        .canonicalize()
        .with_context(|| format!("Failed to resolve worktree: {}", worktree.display()))?;
    ensure_outside_worktree(&directory, &worktree)?;

    Ok(directory)
}

fn ensure_outside_worktree(directory: &Path, worktree: &Path) -> Result<()> {
    if directory.starts_with(worktree) {
        bail!(
            "Frozen configuration directory cannot be inside the worktree: {}",
            directory.display()
        );
    }
    Ok(())
}

pub fn load(path: &Path) -> Result<(Config, Option<ConfigLocation>)> {
    let file = open_snapshot(path)?;
    let mut snapshot: FrozenConfigFile = serde_json::from_reader(BufReader::new(file))
        .with_context(|| {
            format!(
                "Failed to parse frozen configuration snapshot: {}",
                path.display()
            )
        })?;

    if snapshot.version != SNAPSHOT_VERSION {
        bail!(
            "Unsupported frozen configuration snapshot version {}",
            snapshot.version
        );
    }

    snapshot.config.selected_agent = snapshot.selected_agent;
    snapshot.config.agent_type = snapshot.agent_type;
    snapshot.config.sandbox.agent_declared_config_dir = snapshot.agent_declared_config_dir;
    Ok((snapshot.config, snapshot.location))
}

fn open_snapshot(path: &Path) -> Result<File> {
    if !path.is_absolute() {
        bail!(
            "Frozen configuration snapshot path must be absolute: {}",
            path.display()
        );
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }

    let file = options.open(path).with_context(|| {
        format!(
            "Failed to open frozen configuration snapshot: {}",
            path.display()
        )
    })?;
    if !file
        .metadata()
        .context("Failed to inspect frozen configuration snapshot")?
        .is_file()
    {
        bail!(
            "Frozen configuration snapshot must be a regular file: {}",
            path.display()
        );
    }

    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path)
        .context("Failed to inspect frozen configuration snapshot path")?
        .file_type()
        .is_symlink()
    {
        bail!(
            "Frozen configuration snapshot cannot be a symlink: {}",
            path.display()
        );
    }

    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ConfigLocation, StatusIcons};

    fn sample_config() -> Config {
        Config {
            agent: Some("claude --model sonnet".to_string()),
            agent_type: Some("claude".to_string()),
            selected_agent: Some("reviewer".to_string()),
            status_icons: StatusIcons {
                working: Some("work".to_string()),
                waiting: Some("wait".to_string()),
                done: Some("done".to_string()),
            },
            ..Default::default()
        }
    }

    fn sample_location(root: &Path) -> ConfigLocation {
        ConfigLocation {
            config_path: root.join("app/.workmux.yaml"),
            config_dir: root.join("app"),
            rel_dir: PathBuf::from("app"),
        }
    }

    fn capture(
        config: &Config,
        location: Option<&ConfigLocation>,
    ) -> (tempfile::TempDir, FrozenConfigGuard) {
        let directory = tempfile::tempdir().unwrap();
        let guard = FrozenConfigGuard::capture_in(config, location, directory.path()).unwrap();
        (directory, guard)
    }

    #[test]
    fn shell_assignment_preserves_snapshot_for_supervisor() {
        let assignment = shell_assignment_with(|key| {
            assert_eq!(key, FROZEN_CONFIG_ENV);
            Some("/tmp/config path/it's.json".into())
        });

        assert_eq!(
            assignment.as_deref(),
            Some("WORKMUX_FROZEN_CONFIG='/tmp/config path/it'\\''s.json'")
        );
    }

    #[test]
    fn round_trip_preserves_resolved_config_and_location() {
        let root = tempfile::tempdir().unwrap();
        let mut config = sample_config();
        config.sandbox.agent_declared_config_dir = Some(PathBuf::from("/work/.claude"));
        let location = sample_location(root.path());
        let (_directory, guard) = capture(&config, Some(&location));

        let (loaded, loaded_location) = load(guard.path()).unwrap();

        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&config).unwrap()
        );
        assert_eq!(loaded.agent_type, config.agent_type);
        assert_eq!(loaded.selected_agent, config.selected_agent);
        assert_eq!(
            loaded.sandbox.agent_declared_config_dir,
            Some(PathBuf::from("/work/.claude"))
        );
        assert_eq!(loaded_location, Some(location));
    }

    #[test]
    fn round_trip_preserves_structured_config() {
        let mut config: Config = serde_yaml::from_str(
            r#"
theme:
  scheme: glacier-signal
  mode: light
agents:
  reviewer:
    command: claude
    type: claude
    args: [--model, sonnet]
    env:
      CLAUDE_PROFILE: review
panes:
  - command: "{agent}"
    working_dir: backend
sandbox:
  enabled: true
  env:
    PROJECT_MODE: review
  host_commands: [open]
"#,
        )
        .unwrap();
        config.selected_agent = Some("reviewer".to_string());
        config.agent_type = Some("claude".to_string());
        let (_directory, guard) = capture(&config, None);

        let (loaded, _) = load(guard.path()).unwrap();

        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&config).unwrap()
        );
        assert_eq!(loaded.selected_agent, config.selected_agent);
        assert_eq!(loaded.agent_type, config.agent_type);
    }

    #[test]
    fn snapshot_file_is_private() {
        let (_directory, guard) = capture(&sample_config(), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(guard.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn rejects_snapshot_directory_inside_worktree() {
        let worktree = Path::new("/tmp/project");
        let directory = worktree.join(".workmux/frozen-config");

        let error = ensure_outside_worktree(&directory, worktree).unwrap_err();

        assert!(error.to_string().contains("cannot be inside the worktree"));
    }

    #[test]
    fn accepts_snapshot_directory_outside_worktree() {
        ensure_outside_worktree(Path::new("/tmp/workmux-state"), Path::new("/tmp/project"))
            .unwrap();
    }

    #[test]
    fn rejects_relative_path() {
        let error = load(Path::new("snapshot.json")).unwrap_err();
        assert!(error.to_string().contains("must be absolute"));
    }

    #[test]
    fn rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let error = load(dir.path()).unwrap_err();
        assert!(error.to_string().contains("must be a regular file"));
    }

    #[test]
    fn rejects_symlink() {
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target.json");
            std::fs::write(&target, "{}").unwrap();
            let link = dir.path().join("snapshot.json");
            std::os::unix::fs::symlink(target, &link).unwrap();

            let error = load(&link).unwrap_err();
            assert!(error.to_string().contains("Failed to open"));
        }
    }

    #[test]
    fn rejects_malformed_snapshot() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"not json").unwrap();
        file.flush().unwrap();

        let error = load(file.path()).unwrap_err();
        assert!(error.to_string().contains("Failed to parse"));
    }

    #[test]
    fn rejects_unknown_version() {
        let config = sample_config();
        let snapshot = FrozenConfigFile {
            version: SNAPSHOT_VERSION + 1,
            selected_agent: config.selected_agent.clone(),
            agent_type: config.agent_type.clone(),
            agent_declared_config_dir: config.sandbox.agent_declared_config_dir.clone(),
            config,
            location: None,
        };
        let mut file = NamedTempFile::new().unwrap();
        serde_json::to_writer(&mut file, &snapshot).unwrap();
        file.flush().unwrap();

        let error = load(file.path()).unwrap_err();
        assert!(error.to_string().contains("Unsupported"));
    }
}
