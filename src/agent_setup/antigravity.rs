//! Antigravity CLI (`agy`) status tracking setup.
//!
//! Antigravity lifecycle hooks report when an invocation begins and when the
//! agent loop becomes fully idle. Workmux maps those signals to working and
//! done status.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

use super::{StatusCheck, UpdatePreview};

const WORKMUX_GROUP: &str = "workmux-status";
const REGISTER_COMMAND: &str = "workmux register-agent >/dev/null 2>&1 || true; printf '{}\\n'";
const WORKING_COMMAND: &str =
    "workmux set-window-status working >/dev/null 2>&1 || true; printf '{}\\n'";
const STOP_COMMAND: &str = "workmux set-window-status done >/dev/null 2>&1 || true; printf '{}\\n'";

fn gemini_dir() -> Option<PathBuf> {
    home::home_dir().map(|home| home.join(".gemini"))
}

fn hooks_path() -> Option<PathBuf> {
    gemini_dir().map(|dir| dir.join("config").join("hooks.json"))
}

fn detection_reason(
    explicit_test_detection: bool,
    test_environment: bool,
    executable_found: bool,
    config_found: bool,
) -> Option<&'static str> {
    if explicit_test_detection {
        Some("test override")
    } else if !test_environment && executable_found {
        Some("found agy executable")
    } else if config_found {
        Some("found ~/.gemini/antigravity-cli/")
    } else {
        None
    }
}

/// Detect if Antigravity CLI is present via filesystem.
pub fn detect() -> Option<&'static str> {
    detection_reason(
        std::env::var_os("WORKMUX_TEST_AGY_DETECT").is_some(),
        std::env::var_os("WORKMUX_TEST").is_some(),
        which::which("agy").is_ok(),
        gemini_dir().is_some_and(|dir| dir.join("antigravity-cli").is_dir()),
    )
}

/// Check if workmux status tracking is installed for Antigravity.
pub fn check() -> Result<StatusCheck> {
    let Some(path) = hooks_path() else {
        return Ok(StatusCheck::NotInstalled);
    };
    check_at(&path)
}

fn check_at(path: &Path) -> Result<StatusCheck> {
    if !path.exists() {
        return Ok(StatusCheck::NotInstalled);
    }

    let config = read_json(path)?;
    if has_workmux_hooks(&config) {
        Ok(StatusCheck::Installed)
    } else if config.get(WORKMUX_GROUP).is_some() {
        Ok(StatusCheck::UpdateAvailable)
    } else {
        Ok(StatusCheck::NotInstalled)
    }
}

/// Install workmux lifecycle hooks for Antigravity.
pub fn install() -> Result<String> {
    let path = hooks_path().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    install_at(&path)
}

fn install_at(path: &Path) -> Result<String> {
    merge_hooks_file(path, &workmux_hooks())?;
    Ok(format!(
        "Installed Antigravity lifecycle hooks to {}",
        path.display()
    ))
}

/// Remove workmux status tracking from Antigravity.
pub fn uninstall() -> Result<String> {
    let Some(path) = hooks_path() else {
        return Ok("Antigravity config dir not found, nothing to uninstall".to_string());
    };
    uninstall_at(&path)
}

fn uninstall_at(path: &Path) -> Result<String> {
    remove_workmux_group_file(path)?
        .map_or_else(|| Ok("No Antigravity workmux hooks found".to_string()), Ok)
}

pub(crate) fn update_preview() -> Result<Option<UpdatePreview>> {
    let Some(path) = hooks_path().filter(|path| path.exists()) else {
        return Ok(None);
    };
    let installed = read_json(&path)?;
    let mut bundled = installed.clone();
    let bundled_object = bundled
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} root is not an object", path.display()))?;
    bundled_object.insert(
        WORKMUX_GROUP.to_string(),
        workmux_hooks()[WORKMUX_GROUP].clone(),
    );

    Ok(Some(UpdatePreview {
        label: path.display().to_string(),
        installed: serde_json::to_string_pretty(&installed)? + "\n",
        bundled: serde_json::to_string_pretty(&bundled)? + "\n",
    }))
}

fn workmux_hooks() -> Value {
    serde_json::json!({
        WORKMUX_GROUP: {
            "PreInvocation": [{
                "type": "command",
                "command": REGISTER_COMMAND
            }, {
                "type": "command",
                "command": WORKING_COMMAND
            }],
            "PreToolUse": [{
                "matcher": ".*",
                "hooks": [{
                    "type": "command",
                    "command": WORKING_COMMAND
                }]
            }],
            "Stop": [{
                "type": "command",
                "command": STOP_COMMAND
            }]
        }
    })
}

fn has_workmux_hooks(config: &Value) -> bool {
    let Some(group) = config.get(WORKMUX_GROUP) else {
        return false;
    };

    plain_event_has_command(group, "PreInvocation", REGISTER_COMMAND)
        && plain_event_has_command(group, "PreInvocation", WORKING_COMMAND)
        && matcher_event_has_command(group, "PreToolUse", WORKING_COMMAND)
        && plain_event_has_command(group, "Stop", STOP_COMMAND)
}

fn plain_event_has_command(group: &Value, event: &str, command: &str) -> bool {
    group
        .get(event)
        .and_then(Value::as_array)
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry.get("command").and_then(Value::as_str) == Some(command)
                    && entry.get("type").and_then(Value::as_str) == Some("command")
            })
        })
}

fn matcher_event_has_command(group: &Value, event: &str, command: &str) -> bool {
    group
        .get(event)
        .and_then(Value::as_array)
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("command").and_then(Value::as_str) == Some(command)
                                && hook.get("type").and_then(Value::as_str) == Some("command")
                        })
                    })
            })
        })
}

fn merge_hooks_file(path: &Path, hooks_to_add: &Value) -> Result<()> {
    let mut config = if path.exists() {
        read_json(path)?
    } else {
        Value::Object(serde_json::Map::new())
    };
    let config_object = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} root is not an object", path.display()))?;
    let group = hooks_to_add
        .get(WORKMUX_GROUP)
        .ok_or_else(|| anyhow::anyhow!("embedded Antigravity hooks are invalid"))?;
    config_object.insert(WORKMUX_GROUP.to_string(), group.clone());
    write_json(path, &config)
}

fn remove_workmux_group_file(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }

    let mut config = read_json(path)?;
    let Some(object) = config.as_object_mut() else {
        anyhow::bail!("{} root is not an object", path.display());
    };
    if object.remove(WORKMUX_GROUP).is_none() {
        return Ok(None);
    }

    if object.is_empty() {
        fs::remove_file(path).with_context(|| format!("Failed to remove {}", path.display()))?;
    } else {
        write_json(path, &config)?;
    }
    Ok(Some(format!(
        "Removed Workmux hooks from {}",
        path.display()
    )))
}

fn read_json(path: &Path) -> Result<Value> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("{} is not valid JSON", path.display()))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    fs::write(path, serde_json::to_string_pretty(value)? + "\n")
        .with_context(|| format!("Failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_environment_ignores_host_executable_detection() {
        assert_eq!(detection_reason(false, true, true, false), None);
        assert_eq!(
            detection_reason(true, true, true, false),
            Some("test override")
        );
        assert_eq!(
            detection_reason(false, true, true, true),
            Some("found ~/.gemini/antigravity-cli/")
        );
        assert_eq!(
            detection_reason(false, false, true, false),
            Some("found agy executable")
        );
    }

    #[test]
    fn hook_schema_uses_plain_lifecycle_commands() {
        let hooks = workmux_hooks();
        let group = &hooks[WORKMUX_GROUP];

        assert_eq!(
            group["PreInvocation"][0],
            json!({
                "type": "command",
                "command": REGISTER_COMMAND
            })
        );
        assert_eq!(
            group["PreInvocation"][1]["command"],
            Value::String(WORKING_COMMAND.to_string())
        );
        assert!(group["PreInvocation"][0].get("hooks").is_none());
        assert_eq!(
            group["PreToolUse"][0]["hooks"][0]["command"],
            Value::String(WORKING_COMMAND.to_string())
        );
        assert_eq!(
            group["Stop"][0]["command"],
            Value::String(STOP_COMMAND.to_string())
        );
        assert!(group.get("PostToolUse").is_none());
        assert!(group.get("PostInvocation").is_none());
        assert!(has_workmux_hooks(&hooks));
    }

    #[test]
    fn hook_schema_uses_only_supported_events() {
        let hooks = workmux_hooks();
        let group = hooks[WORKMUX_GROUP].as_object().unwrap();
        let supported = [
            "PreToolUse",
            "PostToolUse",
            "PreInvocation",
            "PostInvocation",
            "Stop",
        ];

        assert!(
            group
                .keys()
                .all(|event| supported.contains(&event.as_str()))
        );
    }

    #[test]
    fn install_writes_hooks_and_preserves_other_groups() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config/hooks.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "user-hooks": {
                    "Stop": [{ "type": "command", "command": "echo done" }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        install_at(&path).unwrap();
        install_at(&path).unwrap();

        let config = read_json(&path).unwrap();
        assert!(has_workmux_hooks(&config));
        assert!(config.get("user-hooks").is_some());
        assert!(matches!(check_at(&path).unwrap(), StatusCheck::Installed));
    }

    #[test]
    fn check_status_only_hooks_need_registration_update() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hooks.json");
        write_json(
            &path,
            &json!({
                WORKMUX_GROUP: {
                    "PreInvocation": [{
                        "type": "command",
                        "command": WORKING_COMMAND
                    }],
                    "PreToolUse": [{
                        "matcher": ".*",
                        "hooks": [{
                            "type": "command",
                            "command": WORKING_COMMAND
                        }]
                    }],
                    "Stop": [{
                        "type": "command",
                        "command": STOP_COMMAND
                    }]
                }
            }),
        )
        .unwrap();

        assert!(matches!(
            check_at(&path).unwrap(),
            StatusCheck::UpdateAvailable
        ));
    }

    #[test]
    fn uninstall_removes_only_workmux_group() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config/hooks.json");
        install_at(&path).unwrap();
        let mut config = read_json(&path).unwrap();
        config["user-hooks"] = json!({
            "Stop": [{ "type": "command", "command": "echo done" }]
        });
        write_json(&path, &config).unwrap();

        uninstall_at(&path).unwrap();

        let config = read_json(&path).unwrap();
        assert!(config.get(WORKMUX_GROUP).is_none());
        assert!(config.get("user-hooks").is_some());
    }

    #[test]
    fn uninstall_removes_empty_hooks_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config/hooks.json");
        install_at(&path).unwrap();

        uninstall_at(&path).unwrap();

        assert!(!path.exists());
    }
}
