//! Antigravity CLI (`agy`) status tracking setup.
//!
//! Antigravity exposes a documented `statusLine` command that receives a JSON
//! payload containing `agent_state`. Workmux uses that as the primary status
//! source because it reliably reports the transition back to `idle` after a
//! turn. JSON lifecycle hooks are still installed as a compatibility fallback
//! and to mark `working` promptly on older/limited `agy` builds.
//!
//! Current `agy` versions have been observed to consult both
//! `~/.gemini/config/hooks.json` and `~/.gemini/antigravity-cli/hooks.json`, so
//! workmux installs hooks to both.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

use super::StatusCheck;

const WORKMUX_GROUP: &str = "workmux-status";
const WORKING_WRAPPER: &str = "workmux-status-working";
const DONE_WRAPPER: &str = "workmux-status-done";
const STATUSLINE_WRAPPER: &str = "workmux-statusline";
const STATUSLINE_CHAIN_FILE: &str = "workmux-statusline-chain";

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

fn settings_path() -> Option<PathBuf> {
    gemini_dir().map(|d| d.join("antigravity-cli").join("settings.json"))
}

fn settings_path_from_hooks_paths(paths: &[PathBuf]) -> Result<PathBuf> {
    let root = paths
        .first()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("could not determine Antigravity settings path"))?;
    Ok(root.join("antigravity-cli").join("settings.json"))
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

/// Check if workmux status tracking is installed for Antigravity.
pub fn check() -> Result<StatusCheck> {
    let Some(path) = settings_path() else {
        return Ok(StatusCheck::NotInstalled);
    };

    if !path.exists() {
        return Ok(StatusCheck::NotInstalled);
    }

    let content =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let config: Value = serde_json::from_str(&content)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;

    if has_workmux_statusline(&config) {
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
    let wrappers = install_wrappers(paths)?;
    let settings = settings_path_from_hooks_paths(paths)?;
    install_statusline_at(&settings, &wrappers.statusline, &wrappers.chain_file)?;

    let hooks_to_add = load_hooks(&wrappers.working, &wrappers.done)?;
    let mut installed = Vec::new();

    for path in paths {
        merge_hooks_file(path, &hooks_to_add)?;
        installed.push(path.display().to_string());
    }

    Ok(format!(
        "Installed Antigravity statusLine to {}; hooks to {}; wrappers in {}",
        settings.display(),
        installed.join(", "),
        wrappers.dir.display()
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
    let settings = settings_path_from_hooks_paths(paths).ok();
    let wrapper_dir = paths
        .first()
        .and_then(|p| p.parent())
        .map(|base| base.join("workmux-hooks"));
    let chain_file = wrapper_dir.as_ref().map(|d| d.join(STATUSLINE_CHAIN_FILE));

    if let (Some(settings), Some(chain_file)) = (&settings, &chain_file) {
        messages.push(uninstall_statusline_at(settings, chain_file)?);
    }

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

    if let Some(dir) = wrapper_dir {
        let _ = fs::remove_file(dir.join(WORKING_WRAPPER));
        let _ = fs::remove_file(dir.join(DONE_WRAPPER));
        let _ = fs::remove_file(dir.join(STATUSLINE_WRAPPER));
        let _ = fs::remove_file(dir.join(STATUSLINE_CHAIN_FILE));
        let _ = fs::remove_dir(dir);
    }

    Ok(messages.join("; "))
}

struct WrapperPaths {
    dir: PathBuf,
    working: PathBuf,
    done: PathBuf,
    statusline: PathBuf,
    chain_file: PathBuf,
}

fn install_wrappers(paths: &[PathBuf]) -> Result<WrapperPaths> {
    let base = paths
        .first()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("could not determine Antigravity hooks wrapper dir"))?;
    let dir = base.join("workmux-hooks");
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;

    let working = dir.join(WORKING_WRAPPER);
    let done = dir.join(DONE_WRAPPER);
    let statusline = dir.join(STATUSLINE_WRAPPER);
    let chain_file = dir.join(STATUSLINE_CHAIN_FILE);
    write_status_wrapper(&working, "working")?;
    write_status_wrapper(&done, "done")?;
    write_statusline_wrapper(&statusline, &chain_file)?;

    Ok(WrapperPaths {
        dir,
        working,
        done,
        statusline,
        chain_file,
    })
}

fn write_status_wrapper(path: &Path, status: &str) -> Result<()> {
    let content = if status == "working" {
        r#"#!/bin/sh
pane="${TMUX_PANE-}"
export WORKMUX_TARGET_PANE="$pane"
exec workmux set-window-status working
"#
        .to_string()
    } else {
        r#"#!/bin/sh
pane="${TMUX_PANE-}"
export WORKMUX_TARGET_PANE="$pane"
exec workmux set-window-status done
"#
        .to_string()
    };
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn write_statusline_wrapper(path: &Path, chain_file: &Path) -> Result<()> {
    let chain_file = shell_single_quote(&chain_file.to_string_lossy());
    let content = format!(
        r#"#!/bin/sh
pane="${{TMUX_PANE-}}"
export WORKMUX_TARGET_PANE="$pane"
data=$(cat)
printf '%s' "$data" | workmux set-window-status antigravity-statusline >/dev/null 2>&1 || true
if [ -s {chain_file} ]; then
  chain=$(cat {chain_file} 2>/dev/null || true)
  if [ -n "$chain" ]; then
    printf '%s' "$data" | sh -c "$chain" || true
  fi
fi
exit 0
"#
    );
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn install_statusline_at(path: &Path, statusline_wrapper: &Path, chain_file: &Path) -> Result<()> {
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

    let current_statusline = config.get("statusLine").cloned();
    let current_command = current_statusline
        .as_ref()
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
        .filter(|cmd| !cmd.trim().is_empty())
        .map(str::to_string);

    if let Some(command) = current_command
        && !is_workmux_statusline_command(&command)
    {
        fs::write(chain_file, command)
            .with_context(|| format!("Failed to write {}", chain_file.display()))?;
    }

    let obj = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} root is not an object", path.display()))?;

    let statusline = obj
        .entry("statusLine".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}.statusLine is not an object", path.display()))?;
    statusline.insert(
        "command".to_string(),
        Value::String(statusline_wrapper.to_string_lossy().into_owned()),
    );
    statusline.insert("enabled".to_string(), Value::Bool(true));

    fs::write(path, serde_json::to_string_pretty(&config)? + "\n")
        .with_context(|| format!("Failed to write {}", path.display()))?;

    Ok(())
}

fn uninstall_statusline_at(path: &Path, chain_file: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(format!("No {} found", path.display()));
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let mut config: Value = serde_json::from_str(&content)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;

    if !has_workmux_statusline(&config) {
        return Ok(format!("No workmux statusLine found in {}", path.display()));
    }

    let original = fs::read_to_string(chain_file)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let Some(obj) = config.as_object_mut() else {
        return Ok(format!("Could not update {}", path.display()));
    };

    if let Some(command) = original {
        let statusline = obj
            .entry("statusLine".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("{}.statusLine is not an object", path.display()))?;
        statusline.insert("command".to_string(), Value::String(command));
        statusline.insert("enabled".to_string(), Value::Bool(true));
        fs::write(path, serde_json::to_string_pretty(&config)? + "\n")
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(format!(
            "Restored previous Antigravity statusLine in {}",
            path.display()
        ))
    } else {
        obj.remove("statusLine");
        fs::write(path, serde_json::to_string_pretty(&config)? + "\n")
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(format!(
            "Removed workmux statusLine from {}",
            path.display()
        ))
    }
}

fn load_hooks(working_wrapper: &Path, done_wrapper: &Path) -> Result<Value> {
    let working = working_wrapper.to_string_lossy();
    let done = done_wrapper.to_string_lossy();
    Ok(serde_json::json!({
        WORKMUX_GROUP: {
            "PreInvocation": [{ "hooks": [{ "type": "command", "command": working }] }],
            "PostInvocation": [{ "hooks": [{ "type": "command", "command": done }] }],
            "PreToolUse": [{
                "matcher": ".*",
                "hooks": [{ "type": "command", "command": working }]
            }],
            "PostToolUse": [{
                "matcher": ".*",
                "hooks": [{ "type": "command", "command": working }]
            }],
            "Stop": [{ "hooks": [{ "type": "command", "command": done }] }]
        }
    }))
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
        if group_name == WORKMUX_GROUP {
            // The whole group is owned by workmux. Replace it rather than
            // merging so setup can repair older/broken variants in place.
            config_obj.insert(group_name.clone(), group_value.clone());
        } else {
            match config_obj.get_mut(group_name) {
                Some(existing_group) => merge_group(existing_group, group_value)?,
                None => {
                    config_obj.insert(group_name.clone(), group_value.clone());
                }
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

#[cfg(test)]
fn has_workmux_group(config: &Value) -> bool {
    config
        .get(WORKMUX_GROUP)
        .is_some_and(group_contains_workmux_command)
}

fn has_workmux_statusline(config: &Value) -> bool {
    let Some(statusline) = config.get("statusLine") else {
        return false;
    };
    let enabled = statusline
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    enabled
        && statusline
            .get("command")
            .and_then(|v| v.as_str())
            .is_some_and(is_workmux_statusline_command)
}

fn is_workmux_statusline_command(command: &str) -> bool {
    command.ends_with(STATUSLINE_WRAPPER)
        || command.contains("workmux set-window-status antigravity-statusline")
}

#[cfg(test)]
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
                                .is_some_and(|cmd| {
                                    cmd.contains("workmux set-window-status")
                                        || cmd.ends_with(WORKING_WRAPPER)
                                        || cmd.ends_with(DONE_WRAPPER)
                                })
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
        let hooks = load_hooks(
            Path::new("/tmp/workmux-status-working"),
            Path::new("/tmp/workmux-status-done"),
        )
        .unwrap();
        let group = hooks.get(WORKMUX_GROUP).unwrap().as_object().unwrap();
        assert!(group.contains_key("PreInvocation"));
        assert!(group.contains_key("PostInvocation"));
        assert!(group.contains_key("PreToolUse"));
        assert!(group.contains_key("PostToolUse"));
        assert!(group.contains_key("Stop"));
    }

    #[test]
    fn test_hooks_json_contains_workmux_wrappers() {
        let hooks = load_hooks(
            Path::new("/tmp/workmux-status-working"),
            Path::new("/tmp/workmux-status-done"),
        )
        .unwrap();
        let text = serde_json::to_string(&hooks).unwrap();
        assert!(text.contains("workmux-status-working"));
        assert!(text.contains("workmux-status-done"));
    }

    #[test]
    fn test_has_workmux_group() {
        let hooks = load_hooks(
            Path::new("/tmp/workmux-status-working"),
            Path::new("/tmp/workmux-status-done"),
        )
        .unwrap();
        assert!(has_workmux_group(&hooks));
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

    #[test]
    fn test_install_writes_antigravity_statusline() {
        let tmp = tempfile::tempdir().unwrap();
        let path1 = tmp.path().join("config/hooks.json");
        let path2 = tmp.path().join("antigravity-cli/hooks.json");

        install_at(&[path1, path2]).unwrap();

        let settings_path = tmp.path().join("antigravity-cli/settings.json");
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
        assert!(has_workmux_statusline(&settings));
        assert_eq!(settings["statusLine"]["enabled"], Value::Bool(true));
        assert!(
            settings["statusLine"]["command"]
                .as_str()
                .unwrap()
                .ends_with(STATUSLINE_WRAPPER)
        );

        let wrapper = tmp.path().join("config/workmux-hooks/workmux-statusline");
        let wrapper_text = fs::read_to_string(wrapper).unwrap();
        assert!(wrapper_text.contains("workmux set-window-status antigravity-statusline"));
    }

    #[test]
    fn test_statusline_install_chains_and_uninstall_restores_existing_command() {
        let tmp = tempfile::tempdir().unwrap();
        let path1 = tmp.path().join("config/hooks.json");
        let path2 = tmp.path().join("antigravity-cli/hooks.json");
        let settings_path = tmp.path().join("antigravity-cli/settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&json!({
                "statusLine": {
                    "command": "/usr/local/bin/custom-agy-statusline --compact",
                    "enabled": true
                }
            }))
            .unwrap(),
        )
        .unwrap();

        install_at(&[path1, path2]).unwrap();

        let chain = tmp
            .path()
            .join("config/workmux-hooks/workmux-statusline-chain");
        assert_eq!(
            fs::read_to_string(&chain).unwrap(),
            "/usr/local/bin/custom-agy-statusline --compact"
        );
        let installed_settings: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(has_workmux_statusline(&installed_settings));

        let result = uninstall_at(&[tmp.path().join("config/hooks.json")]).unwrap();
        assert!(result.contains("Restored previous Antigravity statusLine"));
        let restored_settings: Value =
            serde_json::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
        assert_eq!(
            restored_settings["statusLine"]["command"],
            Value::String("/usr/local/bin/custom-agy-statusline --compact".to_string())
        );
    }
}
