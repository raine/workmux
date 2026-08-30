//! Copilot CLI status tracking setup.
//!
//! Detects Copilot CLI through its configuration directory and installs a
//! personal hook under `~/.copilot/hooks/`.

use anyhow::{Context, Result};
use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::{StatusCheck, UpdatePreview};

/// Hooks configuration embedded at compile time.
const HOOKS_JSON: &str = include_str!("../../resources/copilot/hooks/workmux-status/hooks.json");
const HOOKS_FILE_NAME: &str = "workmux-status.json";

fn copilot_dir() -> Option<PathBuf> {
    copilot_dir_from_env(
        home::home_dir(),
        std::env::var_os("COPILOT_HOME"),
        std::env::var_os("COPILOT_CONFIG_DIR"),
    )
}

fn copilot_dir_from_env(
    home: Option<PathBuf>,
    copilot_home: Option<OsString>,
    legacy_config_dir: Option<OsString>,
) -> Option<PathBuf> {
    copilot_home
        .filter(|dir| !dir.is_empty())
        .or_else(|| legacy_config_dir.filter(|dir| !dir.is_empty()))
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".copilot")))
}

fn hooks_file() -> Option<PathBuf> {
    copilot_dir().map(|root| hooks_file_at(&root))
}

fn hooks_file_at(root: &Path) -> PathBuf {
    root.join("hooks").join(HOOKS_FILE_NAME)
}

/// Detect Copilot CLI through its configuration directory.
pub fn detect() -> Option<&'static str> {
    if copilot_dir().is_some_and(|d| d.is_dir()) {
        return Some("found Copilot config directory");
    }
    None
}

/// Check whether the workmux personal hook is installed for Copilot CLI.
pub fn check() -> Result<StatusCheck> {
    let Some(path) = hooks_file() else {
        return Ok(StatusCheck::NotInstalled);
    };
    check_at(&path)
}

fn check_at(path: &Path) -> Result<StatusCheck> {
    if !path.is_file() {
        return Ok(StatusCheck::NotInstalled);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read Copilot hooks from {}", path.display()))?;
    let config: Value = serde_json::from_str(&content)
        .with_context(|| format!("Copilot hooks file is not valid JSON: {}", path.display()))?;

    let required = [
        ("sessionStart", "workmux register-agent"),
        ("userPromptSubmitted", "workmux set-window-status working"),
        ("postToolUse", "workmux set-window-status working"),
        ("agentStop", "workmux set-window-status done"),
    ];
    if config.get("version").and_then(Value::as_u64) == Some(1)
        && required
            .iter()
            .all(|(event, command)| has_command_hook(&config, event, command))
    {
        Ok(StatusCheck::Installed)
    } else if has_workmux_hook(&config) {
        Ok(StatusCheck::UpdateAvailable)
    } else {
        Ok(StatusCheck::NotInstalled)
    }
}

fn has_command_hook(config: &Value, event: &str, expected: &str) -> bool {
    config["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .any(|hook| {
            hook.get("type").and_then(Value::as_str) == Some("command")
                && hook.get("bash").and_then(Value::as_str) == Some(expected)
        })
}

fn has_workmux_hook(config: &Value) -> bool {
    config["hooks"]
        .as_object()
        .into_iter()
        .flat_map(|hooks| hooks.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|hook| hook.get("bash").and_then(Value::as_str))
        .any(|command| {
            command.contains("workmux set-window-status")
                || command.contains("workmux register-agent")
        })
}

pub(crate) fn update_preview() -> Result<Option<UpdatePreview>> {
    let Some(path) = hooks_file().filter(|path| path.exists()) else {
        return Ok(None);
    };
    Ok(Some(UpdatePreview {
        label: path.display().to_string(),
        installed: fs::read_to_string(&path)?,
        bundled: HOOKS_JSON.to_string(),
    }))
}

/// Install the workmux personal hook for Copilot CLI.
pub fn install() -> Result<String> {
    let path = hooks_file().context("Could not determine home directory")?;
    install_at(&path)
}

fn install_at(path: &Path) -> Result<String> {
    let hooks_dir = path
        .parent()
        .context("Copilot hooks path has no parent directory")?;
    fs::create_dir_all(hooks_dir)
        .with_context(|| format!("Failed to create {}", hooks_dir.display()))?;
    fs::write(path, HOOKS_JSON)
        .with_context(|| format!("Failed to write Copilot hooks to {}", path.display()))?;

    Ok(format!("Installed hooks to {}", path.display()))
}

/// Remove the workmux personal hook for Copilot CLI.
pub fn uninstall() -> Result<String> {
    let Some(path) = hooks_file() else {
        return Ok("Home directory not found, no Copilot hooks removed".to_string());
    };
    uninstall_at(&path)
}

fn uninstall_at(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok("No Copilot personal hooks found".to_string());
    }

    fs::remove_file(path)
        .with_context(|| format!("Failed to remove Copilot hooks from {}", path.display()))?;

    if let Some(hooks_dir) = path.parent()
        && hooks_dir
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_none())
    {
        let _ = fs::remove_dir(hooks_dir);
    }

    Ok(format!("Removed Copilot hooks from {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hooks_json_is_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(HOOKS_JSON).expect("embedded hooks.json is valid JSON");
        assert_eq!(parsed.get("version").and_then(|v| v.as_u64()), Some(1));
        let hooks = parsed.get("hooks").unwrap().as_object().unwrap();
        assert!(hooks.contains_key("sessionStart"));
        assert!(hooks.contains_key("userPromptSubmitted"));
        assert!(hooks.contains_key("postToolUse"));
        assert!(hooks.contains_key("agentStop"));
    }

    #[test]
    fn test_hooks_json_contains_workmux_commands() {
        assert!(HOOKS_JSON.contains("workmux register-agent"));
        assert!(HOOKS_JSON.contains("workmux set-window-status"));
    }

    #[test]
    fn copilot_root_prefers_non_empty_current_then_legacy_env() {
        let home = Some(PathBuf::from("/home/tester"));
        assert_eq!(
            copilot_dir_from_env(
                home.clone(),
                Some(OsString::from("/current")),
                Some(OsString::from("/legacy")),
            ),
            Some(PathBuf::from("/current"))
        );
        assert_eq!(
            copilot_dir_from_env(
                home.clone(),
                Some(OsString::new()),
                Some(OsString::from("/legacy")),
            ),
            Some(PathBuf::from("/legacy"))
        );
        assert_eq!(
            copilot_dir_from_env(home, Some(OsString::new()), Some(OsString::new())),
            Some(PathBuf::from("/home/tester/.copilot"))
        );
    }

    #[test]
    fn personal_hooks_path_is_under_resolved_root() {
        let root = Path::new("/custom/copilot");
        assert_eq!(hooks_file_at(root), root.join("hooks/workmux-status.json"));
    }

    #[test]
    fn check_requires_personal_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let repository_hook = tmp
            .path()
            .join("repo/.github/hooks/workmux-status/hooks.json");
        fs::create_dir_all(repository_hook.parent().unwrap()).unwrap();
        fs::write(&repository_hook, HOOKS_JSON).unwrap();

        let personal_hook = hooks_file_at(&tmp.path().join(".copilot"));
        assert!(matches!(
            check_at(&personal_hook).unwrap(),
            StatusCheck::NotInstalled
        ));
    }

    #[test]
    fn install_and_check_are_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = hooks_file_at(tmp.path());

        install_at(&path).unwrap();
        install_at(&path).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), HOOKS_JSON);
        assert!(matches!(check_at(&path).unwrap(), StatusCheck::Installed));
    }

    #[test]
    fn status_only_hook_needs_registration_upgrade() {
        let tmp = tempfile::tempdir().unwrap();
        let path = hooks_file_at(tmp.path());
        let mut config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        config["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("sessionStart");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_string(&config).unwrap()).unwrap();

        assert!(matches!(
            check_at(&path).unwrap(),
            StatusCheck::UpdateAvailable
        ));
    }

    #[test]
    fn missing_or_unknown_schema_version_needs_update() {
        let tmp = tempfile::tempdir().unwrap();
        let path = hooks_file_at(tmp.path());
        let mut config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        config.as_object_mut().unwrap().remove("version");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_string(&config).unwrap()).unwrap();
        assert!(matches!(
            check_at(&path).unwrap(),
            StatusCheck::UpdateAvailable
        ));

        config["version"] = Value::from(2);
        fs::write(&path, serde_json::to_string(&config).unwrap()).unwrap();
        assert!(matches!(
            check_at(&path).unwrap(),
            StatusCheck::UpdateAvailable
        ));
    }

    #[test]
    fn check_requires_exact_bundled_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let path = hooks_file_at(tmp.path());
        let mut config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        config["hooks"]["sessionStart"][0]["bash"] =
            Value::String("bash -c 'workmux register-agent'".to_string());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_string(&config).unwrap()).unwrap();

        assert!(matches!(
            check_at(&path).unwrap(),
            StatusCheck::UpdateAvailable
        ));
    }

    #[test]
    fn uninstall_without_personal_hook_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = hooks_file_at(tmp.path());

        let result = uninstall_at(&path).unwrap();
        assert!(result.contains("No Copilot personal hooks found"));
    }

    #[test]
    fn uninstall_preserves_other_personal_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = hooks_file_at(tmp.path());
        install_at(&path).unwrap();
        let other_hook = path.parent().unwrap().join("other.json");
        fs::write(&other_hook, "{}").unwrap();

        let result = uninstall_at(&path).unwrap();

        assert!(result.contains("Removed Copilot hooks"));
        assert!(!path.exists());
        assert!(other_hook.exists());
        assert!(path.parent().unwrap().exists());
    }

    #[test]
    fn uninstall_removes_empty_hooks_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = hooks_file_at(tmp.path());
        install_at(&path).unwrap();

        uninstall_at(&path).unwrap();

        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
        assert!(tmp.path().exists());
    }
}
