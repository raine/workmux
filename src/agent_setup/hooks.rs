//! Shared JSON hook utilities for agent status tracking setup.
//!
//! Agents like Claude Code, Codex, and Gemini all use the same pattern:
//! hooks are stored as JSON objects under a "hooks" key, with workmux
//! commands in the "command" fields. This module provides shared helpers.

use anyhow::{Context, Result};
use serde_json::Value;

fn is_workmux_hook_command(command: &str) -> bool {
    command.contains("workmux set-window-status") || command.contains("workmux register-agent")
}

/// Check if a parsed JSON value contains any workmux status hooks.
pub fn has_workmux_hooks(settings: &Value) -> bool {
    let Some(hooks) = settings.get("hooks").and_then(|v| v.as_object()) else {
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
                    && is_workmux_hook_command(cmd)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Remove workmux hook commands from `settings` in place.
///
/// Removes individual hook entries whose command contains
/// `workmux set-window-status`, then cleans up empty groups
/// and events. Returns true if anything was removed.
pub fn remove_workmux_hooks(settings: &mut Value) -> bool {
    let Some(hooks) = settings.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return false;
    };

    let mut modified = false;
    let mut events_to_remove: Vec<String> = Vec::new();

    for (event, groups) in hooks.iter_mut() {
        let Some(groups_arr) = groups.as_array_mut() else {
            continue;
        };

        // For each group, remove only workmux hooks from its inner hooks array
        for group in groups_arr.iter_mut() {
            if let Some(hooks_list) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                let len_before = hooks_list.len();
                hooks_list.retain(|e| {
                    !e.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(is_workmux_hook_command)
                });
                if hooks_list.len() < len_before {
                    modified = true;
                }
            }
        }

        // Remove groups that now have empty hooks arrays
        groups_arr.retain(|group| {
            group
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|h| !h.is_empty())
        });

        if groups_arr.is_empty() {
            events_to_remove.push(event.clone());
        }
    }

    for event in &events_to_remove {
        hooks.remove(event);
    }

    modified
}

/// Remove workmux-status plugin entries from enabledPlugins.
pub fn remove_workmux_plugins(settings: &mut Value) -> bool {
    let Some(plugins) = settings
        .get_mut("enabledPlugins")
        .and_then(|v| v.as_object_mut())
    else {
        return false;
    };
    let keys: Vec<String> = plugins
        .keys()
        .filter(|k| k.starts_with("workmux-status@"))
        .cloned()
        .collect();
    let modified = !keys.is_empty();
    for key in &keys {
        plugins.remove(key);
    }
    modified
}

/// Remove empty wrapper objects from the JSON tree.
/// E.g., if "hooks" is now an empty object, remove the "hooks" key.
pub fn remove_empty_hooks_wrapper(settings: &mut Value) -> bool {
    let root = settings.as_object_mut().map(|o| {
        let mut modified = false;
        if let Some(hooks) = o.get("hooks")
            && hooks.as_object().is_some_and(|m| m.is_empty())
        {
            o.remove("hooks");
            modified = true;
        }
        if let Some(plugins) = o.get("enabledPlugins")
            && plugins.as_object().is_some_and(|m| m.is_empty())
        {
            o.remove("enabledPlugins");
            modified = true;
        }
        modified
    });
    root.unwrap_or(false)
}

fn groups_have_hook(groups: &[Value], required_group: &Value, required_hook: &Value) -> bool {
    groups.iter().any(|group| {
        group.get("matcher") == required_group.get("matcher")
            && group
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|installed_hooks| {
                    installed_hooks.iter().any(|installed| {
                        match required_hook.get("command").and_then(Value::as_str) {
                            Some(command) => {
                                installed.get("command").and_then(Value::as_str) == Some(command)
                            }
                            None => installed == required_hook,
                        }
                    })
                })
    })
}

/// Check whether every required hook command is present in the same event and matcher scope.
pub fn has_required_hook_commands(config_root: &Value, required_hooks: &Value) -> bool {
    let Some(installed_events) = config_root.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    let Some(required_events) = required_hooks.as_object() else {
        return false;
    };

    required_events.iter().all(|(event, required_groups)| {
        let Some(installed_groups) = installed_events.get(event).and_then(Value::as_array) else {
            return false;
        };
        required_groups
            .as_array()
            .into_iter()
            .flatten()
            .all(|required_group| {
                required_group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .all(|required_hook| {
                        groups_have_hook(installed_groups, required_group, required_hook)
                    })
            })
    })
}

/// Merge only missing hook commands into their event and matcher scope.
///
/// Existing groups and user hooks remain unchanged. A required command already
/// present in a group with the same matcher is not added again, even when that
/// group also contains user hooks.
pub fn merge_missing_hook_commands(config_root: &mut Value, hooks_to_add: &Value) -> Result<()> {
    let config_obj = config_root
        .as_object_mut()
        .context("config root is not an object")?;

    if !config_obj.contains_key("hooks") {
        config_obj.insert("hooks".to_string(), Value::Object(serde_json::Map::new()));
    }

    let existing_hooks = config_obj
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .context("hooks value is not an object")?;
    let hooks_map = hooks_to_add
        .as_object()
        .context("hooks to add is not an object")?;

    for (event, hook_groups) in hooks_map {
        let Some(required_groups) = hook_groups.as_array() else {
            continue;
        };
        let existing_groups = existing_hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} is not an array"))?;

        for required_group in required_groups {
            let Some(required_hook_list) = required_group.get("hooks").and_then(Value::as_array)
            else {
                continue;
            };
            let missing: Vec<Value> = required_hook_list
                .iter()
                .filter(|required_hook| {
                    !groups_have_hook(existing_groups, required_group, required_hook)
                })
                .cloned()
                .collect();
            if missing.is_empty() {
                continue;
            }

            let mut addition = required_group.clone();
            addition["hooks"] = Value::Array(missing);
            existing_groups.push(addition);
        }
    }

    Ok(())
}

/// Merge hook groups into a config root, deduplicating by value equality.
///
/// `config_root` is the parsed JSON settings file (must be an object).
/// `hooks_to_add` is the hooks map from the embedded config file
/// (e.g. `{"Stop": [{"hooks": [...]}]}`).
///
/// Ensures a "hooks" key exists in config_root, then for each event in
/// hooks_to_add: if the event already exists in config, merges groups
/// avoiding duplicates by serde_json::Value equality; otherwise inserts
/// the new groups. Skips incoming hook_groups that are not arrays.
/// Errors if an existing hooks.<event> value is not an array.
pub fn merge_hook_groups(config_root: &mut Value, hooks_to_add: &Value) -> Result<()> {
    let config_obj = config_root
        .as_object_mut()
        .context("config root is not an object")?;

    if !config_obj.contains_key("hooks") {
        config_obj.insert("hooks".to_string(), Value::Object(serde_json::Map::new()));
    }

    let existing_hooks = config_obj
        .get_mut("hooks")
        .and_then(|v| v.as_object_mut())
        .context("hooks value is not an object")?;

    let hooks_map = hooks_to_add
        .as_object()
        .context("hooks to add is not an object")?;

    for (event, hook_groups) in hooks_map {
        let Some(new_groups) = hook_groups.as_array() else {
            continue;
        };

        if let Some(existing_groups) = existing_hooks.get_mut(event) {
            let arr = existing_groups
                .as_array_mut()
                .with_context(|| format!("hooks.{event} is not an array"))?;
            for group in new_groups {
                if !arr.contains(group) {
                    arr.push(group.clone());
                }
            }
        } else {
            existing_hooks.insert(event.clone(), hook_groups.clone());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_has_workmux_hooks_empty() {
        let settings = json!({});
        assert!(!has_workmux_hooks(&settings));
    }

    #[test]
    fn test_has_workmux_hooks_present() {
        let settings = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": "workmux set-window-status done"
                    }]
                }]
            }
        });
        assert!(has_workmux_hooks(&settings));
    }

    #[test]
    fn test_has_workmux_hooks_other_hooks_only() {
        let settings = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": "afplay /System/Library/Sounds/Glass.aiff"
                    }]
                }]
            }
        });
        assert!(!has_workmux_hooks(&settings));
    }

    #[test]
    fn test_remove_workmux_hooks_mixed() {
        let mut settings = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{ "type": "command", "command": "workmux set-window-status done" }]
                }, {
                    "hooks": [{ "type": "command", "command": "afplay /System/Library/Sounds/Glass.aiff" }]
                }]
            },
            "enabledPlugins": {
                "workmux-status@workmux": true,
                "other-plugin@1.0": true
            }
        });

        assert!(remove_workmux_hooks(&mut settings));
        assert!(remove_workmux_plugins(&mut settings));
        remove_empty_hooks_wrapper(&mut settings);

        // Workmux hook group removed, non-workmux group preserved
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(
            stop[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("Glass")
        );

        // Workmux plugin removed, other plugin preserved
        assert!(
            settings["enabledPlugins"]
                .as_object()
                .unwrap()
                .contains_key("other-plugin@1.0")
        );
        assert!(
            !settings["enabledPlugins"]
                .as_object()
                .unwrap()
                .contains_key("workmux-status@workmux")
        );
    }

    #[test]
    fn test_remove_workmux_registration_hook() {
        let mut settings = json!({
            "hooks": {
                "SessionStart": [{
                    "hooks": [{ "type": "command", "command": "workmux register-agent" }]
                }]
            }
        });

        assert!(remove_workmux_hooks(&mut settings));
        remove_empty_hooks_wrapper(&mut settings);
        assert!(settings.get("hooks").is_none());
    }

    #[test]
    fn test_remove_workmux_hooks_only_workmux() {
        let mut settings = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{ "type": "command", "command": "workmux set-window-status done" }]
                }]
            }
        });

        assert!(remove_workmux_hooks(&mut settings));
        remove_empty_hooks_wrapper(&mut settings);
        // Empty hooks object should be removed
        assert!(settings.get("hooks").is_none());
    }

    #[test]
    fn test_remove_workmux_hooks_mixed_in_same_group() {
        let mut settings = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [
                        { "type": "command", "command": "workmux set-window-status done" },
                        { "type": "command", "command": "afplay /System/Library/Sounds/Glass.aiff" },
                        { "type": "command", "command": "echo user-hook" }
                    ]
                }]
            }
        });

        assert!(remove_workmux_hooks(&mut settings));

        // The group should still exist with non-workmux hooks preserved
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        let hooks = stop[0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 2);
        assert!(hooks[0]["command"].as_str().unwrap().contains("Glass"));
        assert!(hooks[1]["command"].as_str().unwrap().contains("echo"));
    }

    #[test]
    fn test_remove_workmux_hooks_idempotent() {
        let mut settings = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{ "type": "command", "command": "workmux set-window-status done" }]
                }]
            }
        });
        assert!(remove_workmux_hooks(&mut settings));
        // Second call should return false (nothing to remove)
        assert!(!remove_workmux_hooks(&mut settings));
    }

    #[test]
    fn test_remove_workmux_hooks_empty_settings() {
        let mut settings = json!({});
        assert!(!remove_workmux_hooks(&mut settings));
    }

    #[test]
    fn test_remove_workmux_plugins_empty() {
        let mut settings = json!({});
        assert!(!remove_workmux_plugins(&mut settings));
    }

    #[test]
    fn test_remove_workmux_plugins_only_workmux() {
        let mut settings = json!({
            "enabledPlugins": {
                "workmux-status@workmux": true
            }
        });
        assert!(remove_workmux_plugins(&mut settings));
        assert!(settings["enabledPlugins"].as_object().unwrap().is_empty());
    }

    #[test]
    fn test_remove_workmux_plugins_idempotent() {
        let mut settings = json!({
            "enabledPlugins": {
                "workmux-status@workmux": true
            }
        });
        assert!(remove_workmux_plugins(&mut settings));
        assert!(!remove_workmux_plugins(&mut settings));
    }

    #[test]
    fn test_remove_empty_hooks_wrapper_none() {
        let mut settings = json!({
            "hooks": {
                "Stop": [{"hooks": [{"command": "echo hi"}]}]
            }
        });
        assert!(!remove_empty_hooks_wrapper(&mut settings));
    }

    #[test]
    fn test_remove_empty_hooks_wrapper_empty_hooks() {
        let mut settings = json!({ "hooks": {} });
        assert!(remove_empty_hooks_wrapper(&mut settings));
        assert!(settings.get("hooks").is_none());
    }

    #[test]
    fn test_remove_empty_hooks_wrapper_empty_plugins() {
        let mut settings = json!({ "enabledPlugins": {} });
        assert!(remove_empty_hooks_wrapper(&mut settings));
        assert!(settings.get("enabledPlugins").is_none());
    }

    #[test]
    fn test_required_hooks_can_share_group_with_user_hooks() {
        let config = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [
                        { "type": "command", "command": "afplay Glass.aiff" },
                        { "type": "command", "command": "workmux set-window-status done" }
                    ]
                }]
            }
        });
        let required = json!({
            "Stop": [{
                "hooks": [
                    { "type": "command", "command": "workmux set-window-status done" }
                ]
            }]
        });

        assert!(has_required_hook_commands(&config, &required));
    }

    #[test]
    fn test_required_hooks_respect_matcher_scope() {
        let config = json!({
            "hooks": {
                "Notification": [{
                    "matcher": "other_event",
                    "hooks": [{ "command": "workmux set-window-status waiting" }]
                }]
            }
        });
        let required = json!({
            "Notification": [{
                "matcher": "permission_prompt|elicitation_dialog",
                "hooks": [{ "command": "workmux set-window-status waiting" }]
            }]
        });

        assert!(!has_required_hook_commands(&config, &required));
    }

    #[test]
    fn test_merge_missing_commands_preserves_mixed_group_without_duplicate() {
        let mut config = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [
                        { "type": "command", "command": "afplay Glass.aiff" },
                        { "type": "command", "command": "workmux set-window-status done" }
                    ]
                }]
            }
        });
        let original = config.clone();
        let required = json!({
            "Stop": [{
                "hooks": [
                    { "type": "command", "command": "workmux set-window-status done" }
                ]
            }]
        });

        merge_missing_hook_commands(&mut config, &required).unwrap();
        assert_eq!(config, original);
    }

    #[test]
    fn test_merge_missing_commands_adds_only_missing_hooks() {
        let mut config = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{ "command": "workmux set-window-status done" }]
                }]
            }
        });
        let required = json!({
            "Stop": [{
                "hooks": [
                    { "command": "workmux set-window-status done" },
                    { "command": "workmux register-agent" }
                ]
            }]
        });

        merge_missing_hook_commands(&mut config, &required).unwrap();
        let groups = config["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(groups[1]["hooks"][0]["command"], "workmux register-agent");
    }

    #[test]
    fn test_merge_hook_groups_into_empty() {
        let mut config = json!({ "hooks": {} });
        let hooks_to_add = json!({
            "Stop": [{"hooks": [{"type": "command", "command": "workmux set-window-status done"}]}]
        });
        merge_hook_groups(&mut config, &hooks_to_add).unwrap();
        let hooks = config["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(
            config["hooks"]["Stop"][0]["hooks"][0]["command"],
            "workmux set-window-status done"
        );
    }

    #[test]
    fn test_merge_hook_groups_into_empty_root_creates_hooks() {
        let mut config = json!({});
        let hooks_to_add = json!({
            "Stop": [{"hooks": [{"type": "command", "command": "workmux set-window-status done"}]}]
        });
        merge_hook_groups(&mut config, &hooks_to_add).unwrap();
        let hooks = config["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 1);
    }

    #[test]
    fn test_merge_hook_groups_deduplicates() {
        let mut config = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{"type": "command", "command": "workmux set-window-status done"}]
                }]
            }
        });
        let hooks_to_add = json!({
            "Stop": [{"hooks": [{"type": "command", "command": "workmux set-window-status done"}]}]
        });
        merge_hook_groups(&mut config, &hooks_to_add).unwrap();
        let stop = config["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
    }

    #[test]
    fn test_merge_hook_groups_preserves_existing() {
        let mut config = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{"type": "command", "command": "python3 my-hook.py"}]
                }]
            }
        });
        let hooks_to_add = json!({
            "Stop": [{"hooks": [{"type": "command", "command": "workmux set-window-status done"}]}]
        });
        merge_hook_groups(&mut config, &hooks_to_add).unwrap();
        let stop = config["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
    }

    #[test]
    fn test_merge_hook_groups_adds_new_event() {
        let mut config = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{"type": "command", "command": "python3 my-hook.py"}]
                }]
            }
        });
        let hooks_to_add = json!({
            "UserPromptSubmit": [{
                "hooks": [{"type": "command", "command": "workmux set-window-status working"}]
            }]
        });
        merge_hook_groups(&mut config, &hooks_to_add).unwrap();
        assert!(
            config["hooks"]
                .as_object()
                .unwrap()
                .contains_key("UserPromptSubmit")
        );
        assert!(config["hooks"].as_object().unwrap().contains_key("Stop"));
    }

    #[test]
    fn test_merge_hook_groups_skips_non_array() {
        let mut config = json!({ "hooks": {} });
        let hooks_to_add = json!({
            "Stop": [{"hooks": [{"type": "command", "command": "workmux set-window-status done"}]}],
            "InvalidEvent": "not an array"
        });
        merge_hook_groups(&mut config, &hooks_to_add).unwrap();
        // InvalidEvent should be silently skipped, Stop should be merged
        assert!(config["hooks"].as_object().unwrap().contains_key("Stop"));
        assert!(
            !config["hooks"]
                .as_object()
                .unwrap()
                .contains_key("InvalidEvent")
        );
    }

    #[test]
    fn test_merge_hook_groups_errors_on_non_array_existing() {
        let mut config = json!({
            "hooks": {
                "Stop": "not an array"
            }
        });
        let hooks_to_add = json!({
            "Stop": [{"hooks": [{"type": "command", "command": "workmux set-window-status done"}]}]
        });
        let result = merge_hook_groups(&mut config, &hooks_to_add);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("hooks.Stop is not an array")
        );
    }

    #[test]
    fn test_merge_hook_groups_errors_on_non_object_root() {
        let mut config = json!("not an object");
        let hooks_to_add = json!({});
        let result = merge_hook_groups(&mut config, &hooks_to_add);
        assert!(result.is_err());
    }
}
