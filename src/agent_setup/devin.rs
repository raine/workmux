//! Devin CLI status tracking setup.
//!
//! Detects Devin via the `~/.config/devin/` directory.
//! Installs hooks by merging into `~/.config/devin/config.json`, which
//! also holds Devin's other user settings (mcpServers, permissions, ...),
//! so uninstall must only ever touch the `hooks` key.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

use super::StatusCheck;
use crate::agent_setup::json_config::{
    self, EmptyJsonRoot, JsonHookInstallSpec, JsonHookUninstallSpec,
};

/// Hooks configuration embedded at compile time.
const HOOKS_JSON: &str = include_str!("../../.devin/hooks/workmux-status.json");

fn devin_dir() -> Option<PathBuf> {
    home::home_dir().map(|h| h.join(".config/devin"))
}

fn config_path() -> Option<PathBuf> {
    devin_dir().map(|d| d.join("config.json"))
}

/// Detect if Devin is present via filesystem.
pub fn detect() -> Option<&'static str> {
    if devin_dir().is_some_and(|d| d.is_dir()) {
        return Some("found ~/.config/devin/");
    }
    None
}

/// Check if workmux hooks are installed in Devin's config.json.
pub fn check() -> Result<StatusCheck> {
    let Some(path) = config_path() else {
        return Ok(StatusCheck::NotInstalled);
    };

    json_config::check_hook_file(
        &path,
        "Failed to read ~/.config/devin/config.json",
        "~/.config/devin/config.json is not valid JSON",
    )
}

/// Remove workmux hooks from Devin's config.json.
///
/// Only removes workmux's own hook entries; config.json's other keys
/// (mcpServers, permissions, ...) are left untouched, so the file is
/// never deleted outright.
pub fn uninstall() -> Result<String> {
    let Some(path) = config_path() else {
        return Ok("Devin config dir not found, nothing to uninstall".to_string());
    };
    uninstall_at(path)
}

fn uninstall_at(path: PathBuf) -> Result<String> {
    json_config::json_hook_uninstall(
        &path,
        &JsonHookUninstallSpec {
            messages: json_config::JsonHookUninstallMessages {
                file_missing: "No Devin config.json found",
                not_found: "No workmux hooks found in Devin config.json",
                soft_read_error: None,
                soft_parse_error: None,
            },
            delete_if_no_hooks_remain: false,
            remove_plugins: false,
            soft_errors: false,
        },
    )
}

fn load_hooks() -> Result<Value> {
    json_config::hooks_from_embedded(HOOKS_JSON, "hooks config missing hooks key")
}

fn install_hooks_at(path: &std::path::Path) -> Result<()> {
    json_config::json_hook_install(
        path,
        &load_hooks()?,
        &JsonHookInstallSpec {
            read_context: "Failed to read ~/.config/devin/config.json",
            parse_context: "~/.config/devin/config.json is not valid JSON",
            write_context: "Failed to write ~/.config/devin/config.json",
            mkdir_context: "Failed to create ~/.config/devin/ directory",
            empty_root: EmptyJsonRoot::Object,
        },
    )
}

/// Install workmux hooks into `~/.config/devin/config.json`.
///
/// Merges hook groups into existing hooks without clobbering or creating
/// duplicates. Returns a description of what was done.
pub fn install() -> Result<String> {
    let path =
        config_path().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    install_hooks_at(&path).context("Failed to install Devin hooks")?;

    Ok(format!("Installed hooks to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_hooks_json_is_valid() {
        let parsed: Value =
            serde_json::from_str(HOOKS_JSON).expect("embedded hooks config is valid JSON");
        let hooks = parsed.get("hooks").unwrap().as_object().unwrap();
        assert!(hooks.contains_key("UserPromptSubmit"));
        assert!(hooks.contains_key("PostToolUse"));
        assert!(hooks.contains_key("Stop"));
    }

    #[test]
    fn test_load_hooks() {
        let hooks = load_hooks().unwrap();
        let obj = hooks.as_object().unwrap();
        assert!(obj.contains_key("UserPromptSubmit"));
        assert!(obj.contains_key("PostToolUse"));
        assert!(obj.contains_key("Stop"));
    }

    #[test]
    fn test_install_preserves_other_config_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            json!({
                "version": 1,
                "mcpServers": {"icm": {"command": "icm"}},
            })
            .to_string(),
        )
        .unwrap();

        install_hooks_at(&path).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(content["version"], 1);
        assert!(content["mcpServers"]["icm"].is_object());
        assert!(content["hooks"]["Stop"].is_array());
    }

    #[test]
    fn test_install_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();

        install_hooks_at(&path).unwrap();
        install_hooks_at(&path).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let stop = content["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
    }

    #[test]
    fn test_uninstall_no_config_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        let result = uninstall_at(path).unwrap();
        assert!(result.contains("No Devin config.json found"));
    }

    #[test]
    fn test_uninstall_removes_hooks_keeps_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            json!({
                "version": 1,
                "hooks": {
                    "Stop": [{
                        "matcher": "",
                        "hooks": [{"type": "command", "command": "workmux set-window-status done"}]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        let result = uninstall_at(path.clone()).unwrap();
        assert!(result.contains("Removed workmux hooks"));

        let content: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(content["version"], 1);
        assert!(content.get("hooks").is_none());
    }

    #[test]
    fn test_uninstall_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            json!({
                "hooks": {
                    "Stop": [{
                        "matcher": "",
                        "hooks": [{"type": "command", "command": "workmux set-window-status done"}]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        let result1 = uninstall_at(path.clone()).unwrap();
        assert!(result1.contains("Removed workmux hooks"));
        let result2 = uninstall_at(path).unwrap();
        assert!(result2.contains("No workmux hooks found"));
    }
}
