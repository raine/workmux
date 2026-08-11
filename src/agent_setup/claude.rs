//! Claude Code status tracking setup.
//!
//! Detects Claude Code via the Claude config directory.
//! Installs hooks by merging into Claude Code settings.json.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

use super::StatusCheck;
use crate::agent_setup::hooks;
use crate::agent_setup::json_config::{
    self, EmptyJsonRoot, JsonHookInstallSpec, JsonHookUninstallSpec,
};

/// Hooks extracted from `.claude-plugin/plugin.json` at compile time.
const PLUGIN_JSON: &str = include_str!("../../.claude-plugin/plugin.json");

fn claude_dir_from_config(home: PathBuf, config_dir: Option<std::ffi::OsString>) -> PathBuf {
    config_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"))
}

pub(crate) fn claude_dir() -> Option<PathBuf> {
    home::home_dir().map(|home| claude_dir_from_config(home, std::env::var_os("CLAUDE_CONFIG_DIR")))
}

fn settings_path() -> Option<PathBuf> {
    claude_dir().map(|d| d.join("settings.json"))
}

/// Detect if Claude Code is present via filesystem.
/// Returns the reason string if detected, None otherwise.
pub fn detect() -> Option<&'static str> {
    if claude_dir().is_some_and(|d| d.is_dir()) {
        return Some("found Claude config directory");
    }

    None
}

/// Check if workmux hooks are installed in Claude Code settings.
///
/// Checks two paths:
/// 1. Plugin: `enabledPlugins` has a key starting with `workmux-status@`
///    (regardless of enabled/disabled -- user knows about it)
/// 2. Manual hooks: `hooks` object contains a command with `workmux set-window-status`
pub fn check() -> Result<StatusCheck> {
    let Some(path) = settings_path() else {
        return Ok(StatusCheck::NotInstalled);
    };

    if !path.exists() {
        return Ok(StatusCheck::NotInstalled);
    }

    let content =
        std::fs::read_to_string(&path).context("Failed to read ~/.claude/settings.json")?;

    let settings: Value =
        serde_json::from_str(&content).context("~/.claude/settings.json is not valid JSON")?;

    Ok(check_settings(&settings))
}

/// Check a parsed settings.json value for workmux status tracking configuration.
fn check_settings(settings: &Value) -> StatusCheck {
    // Check for plugin installation
    if let Some(plugins) = settings.get("enabledPlugins").and_then(|v| v.as_object())
        && plugins.keys().any(|k| k.starts_with("workmux-status@"))
    {
        return StatusCheck::Installed;
    }

    // Check for manual hooks by traversing the hooks structure
    if hooks::has_workmux_hooks(settings) {
        return StatusCheck::Installed;
    }

    StatusCheck::NotInstalled
}

/// Remove workmux hooks from Claude Code settings.json.
///
/// Uses shared JSON helpers to surgically remove only workmux entries,
/// preserving any user-configured hooks. Returns a description of what
/// was done.
pub fn uninstall() -> Result<String> {
    let Some(path) = settings_path() else {
        return Ok("Claude Code config dir not found, nothing to uninstall".to_string());
    };
    uninstall_at(path)
}

fn uninstall_at(path: PathBuf) -> Result<String> {
    json_config::json_hook_uninstall(
        &path,
        &JsonHookUninstallSpec {
            messages: json_config::JsonHookUninstallMessages {
                file_missing: "No Claude Code settings.json found",
                not_found: "No workmux hooks found in Claude Code settings",
                soft_read_error: None,
                soft_parse_error: None,
            },
            delete_if_no_hooks_remain: false,
            remove_plugins: true,
            soft_errors: false,
        },
    )
}

fn load_hooks_from_plugin() -> Result<Value> {
    json_config::hooks_from_embedded(PLUGIN_JSON, "plugin.json missing hooks key")
}

/// Install workmux hooks into `~/.claude/settings.json`.
///
/// Merges hook groups into existing hooks without clobbering or creating
/// duplicates. Returns a description of what was done.
pub fn install() -> Result<String> {
    let path =
        settings_path().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    json_config::json_hook_install(
        &path,
        &load_hooks_from_plugin()?,
        &JsonHookInstallSpec {
            read_context: "Failed to read ~/.claude/settings.json",
            parse_context: "~/.claude/settings.json is not valid JSON",
            write_context: "Failed to write ~/.claude/settings.json",
            mkdir_context: "Failed to create ~/.claude/ directory",
            empty_root: EmptyJsonRoot::Object,
        },
    )?;

    Ok(format!("Installed hooks to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_load_hooks_from_plugin() {
        let hooks = load_hooks_from_plugin().unwrap();
        let obj = hooks.as_object().unwrap();
        assert!(obj.contains_key("UserPromptSubmit"));
        assert!(obj.contains_key("Notification"));
        assert!(obj.contains_key("PostToolUse"));
        assert!(obj.contains_key("Stop"));
    }

    #[test]
    fn test_claude_dir_respects_env() {
        let path = claude_dir_from_config(
            PathBuf::from("/home/test"),
            Some(std::ffi::OsString::from("/tmp/workmux-test-claude-cfg")),
        );
        assert_eq!(path, PathBuf::from("/tmp/workmux-test-claude-cfg"));
    }

    #[test]
    fn test_claude_dir_defaults_to_home() {
        let path = claude_dir_from_config(PathBuf::from("/home/test"), None);
        assert_eq!(path, PathBuf::from("/home/test/.claude"));
    }

    #[test]
    fn test_check_settings_empty() {
        let settings = json!({});
        assert!(matches!(
            check_settings(&settings),
            StatusCheck::NotInstalled
        ));
    }

    #[test]
    fn test_check_settings_plugin_enabled() {
        let settings = json!({
            "enabledPlugins": {
                "workmux-status@workmux": true
            }
        });
        assert!(matches!(check_settings(&settings), StatusCheck::Installed));
    }

    #[test]
    fn test_check_settings_plugin_disabled() {
        let settings = json!({
            "enabledPlugins": {
                "workmux-status@workmux": false
            }
        });
        assert!(matches!(check_settings(&settings), StatusCheck::Installed));
    }

    #[test]
    fn test_check_settings_plugin_different_version() {
        let settings = json!({
            "enabledPlugins": {
                "workmux-status@1.2.3": true
            }
        });
        assert!(matches!(check_settings(&settings), StatusCheck::Installed));
    }

    #[test]
    fn test_check_settings_other_plugins_only() {
        let settings = json!({
            "enabledPlugins": {
                "some-other-plugin@1.0": true
            }
        });
        assert!(matches!(
            check_settings(&settings),
            StatusCheck::NotInstalled
        ));
    }

    #[test]
    fn test_check_settings_hooks_installed() {
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
        assert!(matches!(check_settings(&settings), StatusCheck::Installed));
    }

    #[test]
    fn test_check_settings_both_plugin_and_hooks() {
        let settings = json!({
            "enabledPlugins": {
                "workmux-status@workmux": true
            },
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": "workmux set-window-status done"
                    }]
                }]
            }
        });
        assert!(matches!(check_settings(&settings), StatusCheck::Installed));
    }

    #[test]
    fn test_uninstall_no_settings_file() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        let result = uninstall_at(settings_path).unwrap();
        assert!(result.contains("No Claude Code settings.json"));
    }

    #[test]
    fn test_uninstall_no_hooks_present() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(&settings_path, r#"{"someSetting": true}"#).unwrap();
        let result = uninstall_at(settings_path).unwrap();
        assert!(result.contains("No workmux hooks found"));
    }

    #[test]
    fn test_uninstall_removes_hooks_only() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"workmux set-window-status done"}]},{"hooks":[{"type":"command","command":"afplay /System/Library/Sounds/Glass.aiff"}]}]}}"#,
        )
        .unwrap();
        let result = uninstall_at(settings_path.clone()).unwrap();
        assert!(result.contains("Removed workmux hooks"), "result: {result}");
        // Verify the user hook is preserved
        let content = std::fs::read_to_string(&settings_path).unwrap();
        let settings: Value = serde_json::from_str(&content).unwrap();
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(
            stop[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("Glass")
        );
    }

    #[test]
    fn test_uninstall_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"workmux set-window-status done"}]}]}}"#,
        )
        .unwrap();
        // First run
        let result1 = uninstall_at(settings_path.clone()).unwrap();
        assert!(result1.contains("Removed workmux hooks"));
        // Second run -- noop
        let result2 = uninstall_at(settings_path).unwrap();
        assert!(result2.contains("No workmux hooks found"));
    }
}
