//! Antigravity CLI (`agy`) status tracking setup.
//!
//! Antigravity uses JSON hooks with named top-level hook groups. Current
//! `agy` versions have been observed to consult both `~/.gemini/config/hooks.json`
//! and `~/.gemini/antigravity-cli/hooks.json`, so workmux installs to both.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

use super::StatusCheck;

/// Hooks configuration embedded at compile time.
const HOOKS_JSON: &str = include_str!("../../resources/antigravity/hooks.json");
const WORKMUX_GROUP: &str = "workmux-status";

fn gemini_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GEMINI_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    home::home_dir().map(|h| h.join(".gemini"))
}

fn hooks_paths() -> Option<Vec<PathBuf>> {
    gemini_dir().map(|d| {
        vec![
            d.join("config").join("hooks.json"),
            d.join("antigravity-cli").join("hooks.json"),
        ]
    })
}

/// Detect if Antigravity CLI is present via filesystem.
pub fn detect() -> Option<&'static str> {
    if std::env::var_os("WORKMUX_TEST_AGY_DETECT").is_some() {
        return Some("test override");
    }

    if which::which("agy").is_ok() {
        return Some("found agy executable");
    }

    if gemini_dir().is_some_and(|d| d.join("antigravity-cli").is_dir()) {
        return Some("found ~/.gemini/antigravity-cli/");
    }

    None
}

/// Check if workmux hooks are installed in Antigravity hooks.json files.
pub fn check() -> Result<StatusCheck> {
    let Some(paths) = hooks_paths() else {
        return Ok(StatusCheck::NotInstalled);
    };

    let mut found_any = false;
    for path in paths {
        if !path.exists() {
            return Ok(StatusCheck::NotInstalled);
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let config: Value = serde_json::from_str(&content)
            .with_context(|| format!("{} is not valid JSON", path.display()))?;

        if has_workmux_group(&config) {
            found_any = true;
        } else {
            return Ok(StatusCheck::NotInstalled);
        }
    }

    if found_any {
        Ok(StatusCheck::Installed)
    } else {
        Ok(StatusCheck::NotInstalled)
    }
}

/// Install workmux hooks into Antigravity hook config files.
pub fn install() -> Result<String> {
    let paths =
        hooks_paths().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    install_at(&paths)
}

fn install_at(paths: &[PathBuf]) -> Result<String> {
    let hooks_to_add = load_hooks()?;
    let mut installed = Vec::new();

    for path in paths {
        merge_hooks_file(path, &hooks_to_add)?;
        installed.push(path.display().to_string());
    }

    Ok(format!(
        "Installed Antigravity hooks to {}",
        installed.join(", ")
    ))
}

/// Remove workmux hooks from Antigravity hook config files.
pub fn uninstall() -> Result<String> {
    let Some(paths) = hooks_paths() else {
        return Ok("Antigravity config dir not found, nothing to uninstall".to_string());
    };
    uninstall_at(&paths)
}

fn uninstall_at(paths: &[PathBuf]) -> Result<String> {
    let mut messages = Vec::new();

    for path in paths {
        if !path.exists() {
            messages.push(format!("No {} found", path.display()));
            continue;
        }

        let content = fs::read_to_string(path)?;
        let mut config: Value = serde_json::from_str(&content)?;
        let removed = remove_workmux_group(&mut config);

        if removed {
            if config.as_object().is_some_and(|o| o.is_empty()) {
                fs::remove_file(path)?;
                messages.push(format!("Removed {} (no hooks remain)", path.display()));
            } else {
                fs::write(path, serde_json::to_string_pretty(&config)? + "\n")?;
                messages.push(format!("Removed workmux hooks from {}", path.display()));
            }
        } else {
            messages.push(format!("No workmux hooks found in {}", path.display()));
        }
    }

    Ok(messages.join("; "))
}

fn load_hooks() -> Result<Value> {
    serde_json::from_str(HOOKS_JSON).context("embedded Antigravity hooks config is valid JSON")
}

fn merge_hooks_file(path: &Path, hooks_to_add: &Value) -> Result<()> {
    let mut config: Value = if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("{} is not valid JSON", path.display()))?
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        Value::Object(serde_json::Map::new())
    };

    let config_obj = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} root is not an object", path.display()))?;

    let hooks_obj = hooks_to_add
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("embedded Antigravity hooks root is not an object"))?;

    for (group_name, group_value) in hooks_obj {
        match config_obj.get_mut(group_name) {
            Some(existing_group) => merge_group(existing_group, group_value)?,
            None => {
                config_obj.insert(group_name.clone(), group_value.clone());
            }
        }
    }

    fs::write(path, serde_json::to_string_pretty(&config)? + "\n")
        .with_context(|| format!("Failed to write {}", path.display()))?;

    Ok(())
}

fn merge_group(existing_group: &mut Value, new_group: &Value) -> Result<()> {
    let existing_events = existing_group
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("existing Antigravity hook group is not an object"))?;
    let new_events = new_group
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("new Antigravity hook group is not an object"))?;

    for (event, new_groups) in new_events {
        let Some(new_groups_arr) = new_groups.as_array() else {
            continue;
        };

        if let Some(existing_groups) = existing_events.get_mut(event) {
            let arr = existing_groups
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("Antigravity hook event {event} is not an array"))?;
            for group in new_groups_arr {
                if !arr.contains(group) {
                    arr.push(group.clone());
                }
            }
        } else {
            existing_events.insert(event.clone(), new_groups.clone());
        }
    }

    Ok(())
}

fn has_workmux_group(config: &Value) -> bool {
    config
        .get(WORKMUX_GROUP)
        .is_some_and(group_contains_workmux_command)
}

fn group_contains_workmux_command(group: &Value) -> bool {
    let Some(events) = group.as_object() else {
        return false;
    };

    events.values().any(|groups| {
        groups.as_array().is_some_and(|groups| {
            groups.iter().any(|group| {
                group
                    .get("hooks")
                    .and_then(|v| v.as_array())
                    .is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("command")
                                .and_then(|v| v.as_str())
                                .is_some_and(|cmd| cmd.contains("workmux set-window-status"))
                        })
                    })
            })
        })
    })
}

fn remove_workmux_group(config: &mut Value) -> bool {
    let Some(obj) = config.as_object_mut() else {
        return false;
    };
    obj.remove(WORKMUX_GROUP).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_hooks_json_is_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(HOOKS_JSON).expect("embedded hooks config is valid JSON");
        let group = parsed.get(WORKMUX_GROUP).unwrap().as_object().unwrap();
        assert!(group.contains_key("PreInvocation"));
        assert!(group.contains_key("PostInvocation"));
        assert!(group.contains_key("PreToolUse"));
        assert!(group.contains_key("PostToolUse"));
        assert!(group.contains_key("Stop"));
    }

    #[test]
    fn test_hooks_json_contains_workmux_command() {
        assert!(HOOKS_JSON.contains("workmux set-window-status"));
    }

    #[test]
    fn test_has_workmux_group() {
        assert!(has_workmux_group(&load_hooks().unwrap()));
        assert!(!has_workmux_group(&json!({ "other": {} })));
    }

    #[test]
    fn test_install_writes_both_paths_and_preserves_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let path1 = tmp.path().join("config/hooks.json");
        let path2 = tmp.path().join("antigravity-cli/hooks.json");
        fs::create_dir_all(path1.parent().unwrap()).unwrap();
        fs::write(
            &path1,
            r#"{"user-hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo bye"}]}]}}"#,
        )
        .unwrap();

        install_at(&[path1.clone(), path2.clone()]).unwrap();

        for path in [&path1, &path2] {
            let content = fs::read_to_string(path).unwrap();
            let config: Value = serde_json::from_str(&content).unwrap();
            assert!(has_workmux_group(&config));
        }

        let config: Value = serde_json::from_str(&fs::read_to_string(&path1).unwrap()).unwrap();
        assert!(config.get("user-hooks").is_some());
    }

    #[test]
    fn test_install_deduplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config/hooks.json");
        install_at(std::slice::from_ref(&path)).unwrap();
        install_at(std::slice::from_ref(&path)).unwrap();

        let config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let pre = config[WORKMUX_GROUP]["PreInvocation"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
    }

    #[test]
    fn test_uninstall_removes_only_workmux_group() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config/hooks.json");
        install_at(std::slice::from_ref(&path)).unwrap();

        let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        config["user-hooks"] = json!({
            "Stop": [{ "hooks": [{ "type": "command", "command": "echo bye" }] }]
        });
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        let result = uninstall_at(std::slice::from_ref(&path)).unwrap();
        assert!(result.contains("Removed workmux hooks"));

        let config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(config.get(WORKMUX_GROUP).is_none());
        assert!(config.get("user-hooks").is_some());
    }
}
