//! Gemini CLI status tracking setup.
//!
//! Detects Gemini CLI via the `~/.gemini/` directory.
//! Installs hooks by merging into `~/.gemini/settings.json`.

use anyhow::{Context, Result};
use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::{StatusCheck, UpdatePreview};
use crate::agent_setup::hooks;
use crate::agent_setup::json_config::{
    self, EmptyJsonRoot, JsonHookInstallSpec, JsonHookUninstallSpec,
};

/// Hooks configuration embedded at compile time.
const HOOKS_JSON: &str = include_str!("../../resources/gemini/settings.json");

fn gemini_dir() -> Option<PathBuf> {
    gemini_dir_from_env(
        home::home_dir(),
        std::env::var_os("GEMINI_CLI_HOME"),
        std::env::var_os("GEMINI_CONFIG_DIR"),
    )
}

fn gemini_dir_from_env(
    home: Option<PathBuf>,
    cli_home: Option<OsString>,
    legacy_config_dir: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(home) = cli_home.filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(home).join(".gemini"));
    }
    legacy_config_dir
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".gemini")))
}

fn settings_path() -> Option<PathBuf> {
    gemini_dir().map(|d| d.join("settings.json"))
}

/// Detect if Gemini CLI is present via filesystem.
pub fn detect() -> Option<&'static str> {
    if gemini_dir().is_some_and(|d| d.is_dir()) {
        return Some("found ~/.gemini/");
    }
    None
}

/// Check if workmux hooks are installed in Gemini settings.json.
pub fn check() -> Result<StatusCheck> {
    let Some(path) = settings_path() else {
        return Ok(StatusCheck::NotInstalled);
    };

    check_at(&path)
}

fn check_at(path: &Path) -> Result<StatusCheck> {
    if !path.exists() {
        return Ok(StatusCheck::NotInstalled);
    }

    let content = fs::read_to_string(path).context("Failed to read ~/.gemini/settings.json")?;
    let config: Value =
        serde_json::from_str(&content).context("~/.gemini/settings.json is not valid JSON")?;
    let required = load_hooks()?;
    if hooks::has_required_hook_commands(&config, &required) {
        Ok(StatusCheck::Installed)
    } else if hooks::has_workmux_hooks(&config) {
        Ok(StatusCheck::UpdateAvailable)
    } else {
        Ok(StatusCheck::NotInstalled)
    }
}

/// Remove workmux hooks from Gemini CLI settings.json.
///
/// Uses shared JSON helpers to surgically remove only workmux entries,
/// preserving any user-configured hooks. Returns a description of what
/// was done.
pub fn uninstall() -> Result<String> {
    let Some(path) = settings_path() else {
        return Ok("Gemini CLI config dir not found, nothing to uninstall".to_string());
    };
    uninstall_at(path)
}

fn uninstall_at(path: PathBuf) -> Result<String> {
    json_config::json_hook_uninstall(
        &path,
        &JsonHookUninstallSpec {
            messages: json_config::JsonHookUninstallMessages {
                file_missing: "No Gemini CLI settings.json found",
                not_found: "No workmux hooks found in Gemini CLI settings",
                soft_read_error: Some("Could not read Gemini CLI settings.json"),
                soft_parse_error: Some("Could not parse Gemini CLI settings.json"),
            },
            delete_if_no_hooks_remain: false,
            remove_plugins: false,
            soft_errors: true,
        },
    )
}

fn load_hooks() -> Result<Value> {
    json_config::hooks_from_embedded(HOOKS_JSON, "hooks config missing hooks key")
}

pub(crate) fn update_preview() -> Result<Option<UpdatePreview>> {
    let Some(path) = settings_path().filter(|path| path.exists()) else {
        return Ok(None);
    };
    let content = fs::read_to_string(&path).context("Failed to read ~/.gemini/settings.json")?;
    let installed: Value =
        serde_json::from_str(&content).context("~/.gemini/settings.json is not valid JSON")?;
    let mut bundled = installed.clone();
    hooks::merge_missing_hook_commands(&mut bundled, &load_hooks()?)?;

    Ok(Some(UpdatePreview {
        label: path.display().to_string(),
        installed: serde_json::to_string_pretty(&installed)? + "\n",
        bundled: serde_json::to_string_pretty(&bundled)? + "\n",
    }))
}

fn install_at(path: &Path) -> Result<()> {
    json_config::json_hook_install_with(
        path,
        &load_hooks()?,
        &JsonHookInstallSpec {
            read_context: "Failed to read ~/.gemini/settings.json",
            parse_context: "~/.gemini/settings.json is not valid JSON",
            write_context: "Failed to write ~/.gemini/settings.json",
            mkdir_context: "Failed to create ~/.gemini/ directory",
            empty_root: EmptyJsonRoot::HooksObject,
        },
        hooks::merge_missing_hook_commands,
    )
}

/// Install workmux hooks into `~/.gemini/settings.json`.
///
/// Merges hook groups into existing hooks without clobbering or creating
/// duplicates. Returns a description of what was done.
pub fn install() -> Result<String> {
    let path =
        settings_path().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    install_at(&path)?;

    Ok(format!("Installed hooks to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_root_uses_cli_home_then_legacy_config_dir() {
        let home = Some(PathBuf::from("/home/tester"));
        assert_eq!(
            gemini_dir_from_env(
                home.clone(),
                Some(OsString::from("/profile")),
                Some(OsString::from("/legacy")),
            ),
            Some(PathBuf::from("/profile/.gemini"))
        );
        assert_eq!(
            gemini_dir_from_env(
                home.clone(),
                Some(OsString::new()),
                Some(OsString::from("/legacy")),
            ),
            Some(PathBuf::from("/legacy"))
        );
        assert_eq!(
            gemini_dir_from_env(home, Some(OsString::new()), Some(OsString::new())),
            Some(PathBuf::from("/home/tester/.gemini"))
        );
    }

    #[test]
    fn test_hooks_json_is_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(HOOKS_JSON).expect("embedded hooks config is valid JSON");
        let hooks = parsed.get("hooks").unwrap().as_object().unwrap();
        assert!(hooks.contains_key("SessionStart"));
        assert!(hooks.contains_key("BeforeAgent"));
        assert!(hooks.contains_key("Notification"));
        assert!(hooks.contains_key("AfterTool"));
        assert!(hooks.contains_key("AfterAgent"));
        assert!(hooks.contains_key("SessionEnd"));
    }

    #[test]
    fn test_hooks_json_contains_workmux_command() {
        assert!(HOOKS_JSON.contains("workmux set-window-status"));

        let config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        let session_start = &config["hooks"]["SessionStart"];
        assert!(session_start[0].get("matcher").is_none());
        assert_eq!(
            session_start[0]["hooks"][0]["command"],
            "workmux register-agent"
        );
    }

    #[test]
    fn test_load_hooks() {
        let hooks = load_hooks().unwrap();
        let obj = hooks.as_object().unwrap();
        assert!(obj.contains_key("SessionStart"));
        assert!(obj.contains_key("BeforeAgent"));
        assert!(obj.contains_key("Notification"));
        assert!(obj.contains_key("AfterTool"));
        assert!(obj.contains_key("AfterAgent"));
        assert!(obj.contains_key("SessionEnd"));
    }

    #[test]
    fn test_check_reports_update_for_status_only_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let mut config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        config["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("SessionStart");
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        assert!(matches!(
            check_at(&path).unwrap(),
            StatusCheck::UpdateAvailable
        ));
    }

    #[test]
    fn test_install_upgrade_is_idempotent_and_preserves_user_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let mut config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        config["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("SessionStart");
        config["hooks"]["AfterAgent"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "type": "command",
                "command": "python3 my-hook.py"
            }));
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        install_at(&path).unwrap();
        let after_first = fs::read_to_string(&path).unwrap();
        install_at(&path).unwrap();
        let after_second = fs::read_to_string(&path).unwrap();

        assert_eq!(after_first, after_second);
        let installed: Value = serde_json::from_str(&after_second).unwrap();
        assert!(matches!(check_at(&path).unwrap(), StatusCheck::Installed));
        assert_eq!(
            installed["hooks"]["SessionStart"].as_array().unwrap().len(),
            1
        );
        let groups = installed["hooks"]["AfterAgent"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert!(
            groups[0]["hooks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hook| hook["command"] == "python3 my-hook.py")
        );
    }

    #[test]
    fn test_uninstall_no_settings_file() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        let result = uninstall_at(settings_path).unwrap();
        assert!(result.contains("No Gemini CLI settings.json"));
    }

    #[test]
    fn test_uninstall_removes_hooks_only() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"hooks":{"AfterAgent":[{"hooks":[{"type":"command","command":"workmux set-window-status done"}]},{"hooks":[{"type":"command","command":"python3 my-hook.py"}]}]}}"#,
        )
        .unwrap();
        let result = uninstall_at(settings_path.clone()).unwrap();
        assert!(result.contains("Removed workmux hooks"));
        let content = std::fs::read_to_string(&settings_path).unwrap();
        let config: Value = serde_json::from_str(&content).unwrap();
        let after = config["hooks"]["AfterAgent"].as_array().unwrap();
        assert_eq!(after.len(), 1);
        assert!(
            after[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("my-hook")
        );
    }

    #[test]
    fn test_uninstall_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"hooks":{"AfterAgent":[{"hooks":[{"type":"command","command":"workmux set-window-status done"}]}]}}"#,
        )
        .unwrap();
        let result1 = uninstall_at(settings_path.clone()).unwrap();
        assert!(result1.contains("Removed workmux hooks"));
        let result2 = uninstall_at(settings_path).unwrap();
        assert!(result2.contains("No workmux hooks found"));
    }
}
