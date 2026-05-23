//! Codex status tracking setup.
//!
//! Detects Codex via the `~/.codex/` directory.
//! Installs hooks by merging into `~/.codex/hooks.json`.
//!
//! Codex hooks require enabling the feature flag in `~/.codex/config.toml`:
//! ```toml
//! [features]
//! hooks = true
//! ```

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

use super::StatusCheck;

/// Hooks configuration embedded at compile time.
const HOOKS_JSON: &str = include_str!("../../.codex/hooks/workmux-status.json");

fn codex_dir() -> Option<PathBuf> {
    home::home_dir().map(|h| h.join(".codex"))
}

fn hooks_path() -> Option<PathBuf> {
    codex_dir().map(|d| d.join("hooks.json"))
}

/// Detect if Codex is present via filesystem.
pub fn detect() -> Option<&'static str> {
    if codex_dir().is_some_and(|d| d.is_dir()) {
        return Some("found ~/.codex/");
    }
    None
}

/// Check if workmux hooks are installed in Codex hooks.json.
pub fn check() -> Result<StatusCheck> {
    let Some(path) = hooks_path() else {
        return Ok(StatusCheck::NotInstalled);
    };

    if !path.exists() {
        return Ok(StatusCheck::NotInstalled);
    }

    let content = fs::read_to_string(&path).context("Failed to read ~/.codex/hooks.json")?;
    let config: Value =
        serde_json::from_str(&content).context("~/.codex/hooks.json is not valid JSON")?;

    if has_workmux_hooks(&config) {
        Ok(StatusCheck::Installed)
    } else {
        Ok(StatusCheck::NotInstalled)
    }
}

/// Check if the hooks object contains any workmux set-window-status commands.
fn has_workmux_hooks(config: &Value) -> bool {
    let Some(hooks) = config.get("hooks").and_then(|v| v.as_object()) else {
        return false;
    };

    for (_event, groups) in hooks {
        let Some(groups_arr) = groups.as_array() else {
            continue;
        };
        for group in groups_arr {
            let Some(hook_list) = group.get("hooks").and_then(|v| v.as_array()) else {
                continue;
            };
            for hook in hook_list {
                if let Some(cmd) = hook.get("command").and_then(|v| v.as_str())
                    && cmd.contains("workmux set-window-status")
                {
                    return true;
                }
            }
        }
    }

    false
}

/// Load the hooks portion from the embedded config.
fn load_hooks() -> Result<Value> {
    let config: Value =
        serde_json::from_str(HOOKS_JSON).expect("embedded hooks config is valid JSON");
    config
        .get("hooks")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("hooks config missing hooks key"))
}

fn config_toml_path() -> Option<PathBuf> {
    codex_dir().map(|d| d.join("config.toml"))
}

/// Ensure `hooks = true` is set under `[features]` in config.toml.
/// Returns true if the file was modified.
fn ensure_hooks_feature_flag() -> Result<bool> {
    let path =
        config_toml_path().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    let content = if path.exists() {
        fs::read_to_string(&path).context("Failed to read ~/.codex/config.toml")?
    } else {
        String::new()
    };

    // Check if already enabled
    if is_hooks_feature_enabled(&content) {
        return Ok(false);
    }

    let updated = if has_hooks_feature_key(&content) {
        // Replace existing hooks value
        content
            .lines()
            .map(|line| {
                if line
                    .trim()
                    .split_once('=')
                    .is_some_and(|(key, _)| key.trim() == "hooks")
                {
                    "hooks = true"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + if content.ends_with('\n') { "\n" } else { "" }
    } else if content.contains("[features]") {
        // Insert after the [features] line
        content.replacen("[features]", "[features]\nhooks = true", 1)
    } else {
        // Append a new [features] section
        let sep = if content.is_empty() || content.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        format!("{content}{sep}\n[features]\nhooks = true\n")
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("Failed to create ~/.codex/ directory")?;
    }
    fs::write(&path, &updated).context("Failed to write ~/.codex/config.toml")?;

    Ok(true)
}

/// Check if `hooks = true` is set in the config content.
fn is_hooks_feature_enabled(content: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == "hooks = true" || trimmed == "hooks=true"
    })
}

/// Check if `hooks` key exists at all (regardless of value).
fn has_hooks_feature_key(content: &str) -> bool {
    content.lines().any(|line| {
        line.trim()
            .split_once('=')
            .is_some_and(|(key, _)| key.trim() == "hooks")
    })
}

/// Install workmux hooks into `~/.codex/hooks.json`.
///
/// Merges hook groups into existing hooks without clobbering or creating
/// duplicates. Returns a description of what was done.
pub fn install() -> Result<String> {
    let path = hooks_path().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    // Read existing config or start fresh
    let mut config: Value = if path.exists() {
        let content = fs::read_to_string(&path).context("Failed to read ~/.codex/hooks.json")?;
        serde_json::from_str(&content).context("~/.codex/hooks.json is not valid JSON")?
    } else {
        // Ensure ~/.codex/ directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("Failed to create ~/.codex/ directory")?;
        }
        serde_json::json!({ "hooks": {} })
    };

    let hooks_to_add = load_hooks()?;

    // Ensure config.hooks exists as an object
    let config_obj = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("hooks.json root is not an object"))?;

    if !config_obj.contains_key("hooks") {
        config_obj.insert("hooks".to_string(), Value::Object(serde_json::Map::new()));
    }

    let existing_hooks = config_obj
        .get_mut("hooks")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("hooks.json hooks is not an object"))?;

    // Merge each hook event, deduplicating by value equality
    let hooks_map = hooks_to_add.as_object().expect("hooks is an object");
    for (event, hook_groups) in hooks_map {
        let Some(new_groups) = hook_groups.as_array() else {
            continue;
        };

        if let Some(existing_groups) = existing_hooks.get_mut(event) {
            let arr = existing_groups
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("hooks.{event} is not an array"))?;
            for group in new_groups {
                if !arr.contains(group) {
                    arr.push(group.clone());
                }
            }
        } else {
            existing_hooks.insert(event.clone(), hook_groups.clone());
        }
    }

    // Write back with pretty formatting
    let output = serde_json::to_string_pretty(&config)?;
    fs::write(&path, output + "\n").context("Failed to write ~/.codex/hooks.json")?;

    // Ensure hooks feature flag is enabled in config.toml
    let feature_msg = match ensure_hooks_feature_flag() {
        Ok(true) => ", enabled hooks in ~/.codex/config.toml",
        _ => "",
    };

    Ok(format!(
        "Installed hooks to ~/.codex/hooks.json{feature_msg}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_hooks_json_is_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(HOOKS_JSON).expect("embedded hooks config is valid JSON");
        let hooks = parsed.get("hooks").unwrap().as_object().unwrap();
        assert!(hooks.contains_key("UserPromptSubmit"));
        assert!(hooks.contains_key("PostToolUse"));
        assert!(hooks.contains_key("Stop"));
        assert!(hooks.contains_key("SubagentStop"));
    }

    #[test]
    fn test_hooks_json_contains_workmux_command() {
        assert!(HOOKS_JSON.contains("workmux set-window-status"));
    }

    #[test]
    fn test_has_workmux_hooks_empty() {
        let config = json!({});
        assert!(!has_workmux_hooks(&config));
    }

    #[test]
    fn test_has_workmux_hooks_present() {
        let config = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": "workmux set-window-status done"
                    }]
                }]
            }
        });
        assert!(has_workmux_hooks(&config));
    }

    #[test]
    fn test_has_workmux_hooks_other_hooks_only() {
        let config = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": "python3 my-script.py"
                    }]
                }]
            }
        });
        assert!(!has_workmux_hooks(&config));
    }

    #[test]
    fn test_load_hooks() {
        let hooks = load_hooks().unwrap();
        let obj = hooks.as_object().unwrap();
        assert!(obj.contains_key("UserPromptSubmit"));
        assert!(obj.contains_key("PostToolUse"));
        assert!(obj.contains_key("Stop"));
        assert!(obj.contains_key("SubagentStop"));
    }

    #[test]
    fn test_merge_into_empty_config() {
        let mut config = json!({ "hooks": {} });
        let hooks_to_add = load_hooks().unwrap();
        let hooks_map = hooks_to_add.as_object().unwrap();

        let existing_hooks = config.get_mut("hooks").unwrap().as_object_mut().unwrap();

        for (event, hook_groups) in hooks_map {
            existing_hooks.insert(event.clone(), hook_groups.clone());
        }

        let hooks = config.get("hooks").unwrap().as_object().unwrap();
        assert_eq!(hooks.len(), 4);
    }

    #[test]
    fn test_merge_deduplicates() {
        let mut config = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": "workmux set-window-status done"
                    }]
                }]
            }
        });

        let hooks_to_add = load_hooks().unwrap();
        let hooks_map = hooks_to_add.as_object().unwrap();

        let existing_hooks = config.get_mut("hooks").unwrap().as_object_mut().unwrap();

        for (event, hook_groups) in hooks_map {
            let new_groups = hook_groups.as_array().unwrap();
            if let Some(existing_groups) = existing_hooks.get_mut(event) {
                let arr = existing_groups.as_array_mut().unwrap();
                for group in new_groups {
                    if !arr.contains(group) {
                        arr.push(group.clone());
                    }
                }
            } else {
                existing_hooks.insert(event.clone(), hook_groups.clone());
            }
        }

        // Stop should still have exactly 1 group (not duplicated)
        let stop = config
            .get("hooks")
            .unwrap()
            .get("Stop")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(stop.len(), 1);
    }

    #[test]
    fn test_merge_preserves_existing_hooks() {
        let mut config = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": "python3 my-stop-hook.py"
                    }]
                }]
            }
        });

        let hooks_to_add = load_hooks().unwrap();
        let hooks_map = hooks_to_add.as_object().unwrap();

        let existing_hooks = config.get_mut("hooks").unwrap().as_object_mut().unwrap();

        for (event, hook_groups) in hooks_map {
            let new_groups = hook_groups.as_array().unwrap();
            if let Some(existing_groups) = existing_hooks.get_mut(event) {
                let arr = existing_groups.as_array_mut().unwrap();
                for group in new_groups {
                    if !arr.contains(group) {
                        arr.push(group.clone());
                    }
                }
            } else {
                existing_hooks.insert(event.clone(), hook_groups.clone());
            }
        }

        // Stop should have 2 groups (original + workmux)
        let stop = config
            .get("hooks")
            .unwrap()
            .get("Stop")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(stop.len(), 2);

        // All 4 events should be present
        let hooks = config.get("hooks").unwrap().as_object().unwrap();
        assert_eq!(hooks.len(), 4);
    }

    #[test]
    fn test_is_hooks_feature_enabled_true() {
        assert!(is_hooks_feature_enabled("[features]\nhooks = true\n"));
    }

    #[test]
    fn test_is_hooks_feature_enabled_no_spaces() {
        assert!(is_hooks_feature_enabled("[features]\nhooks=true\n"));
    }

    #[test]
    fn test_is_hooks_feature_enabled_with_other_settings() {
        let content = "[model]\ndefault = \"gpt-4\"\n\n[features]\nhooks = true\n";
        assert!(is_hooks_feature_enabled(content));
    }

    #[test]
    fn test_is_hooks_feature_enabled_false() {
        assert!(!is_hooks_feature_enabled("[features]\nhooks = false\n"));
    }

    #[test]
    fn test_has_hooks_feature_key_true() {
        assert!(has_hooks_feature_key("[features]\nhooks = true\n"));
    }

    #[test]
    fn test_has_hooks_feature_key_false() {
        assert!(has_hooks_feature_key("[features]\nhooks = false\n"));
    }

    #[test]
    fn test_has_hooks_feature_key_missing() {
        assert!(!has_hooks_feature_key("[features]\n"));
    }

    #[test]
    fn test_is_hooks_feature_enabled_empty() {
        assert!(!is_hooks_feature_enabled(""));
    }

    #[test]
    fn test_is_hooks_feature_enabled_no_features_section() {
        assert!(!is_hooks_feature_enabled("[model]\ndefault = \"gpt-4\"\n"));
    }
}
