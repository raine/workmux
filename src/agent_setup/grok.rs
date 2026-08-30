//! Grok (SpaceXAI) status tracking setup.
//!
//! Detects Grok via the `~/.grok/` directory (or `$GROK_HOME`).
//! Installs hooks by merging into `~/.grok/hooks/workmux-status.json`.
//! Grok's hook runner discovers JSON files in `~/.grok/hooks/` automatically,
//! so no feature flag or config.toml change is required.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

use super::{StatusCheck, UpdatePreview};
use crate::agent_setup::hooks;
use crate::agent_setup::json_config::{
    self, EmptyJsonRoot, JsonHookInstallSpec, JsonHookUninstallSpec,
};

const HOOKS_JSON: &str = include_str!("../../.grok/hooks/workmux-status.json");

fn grok_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GROK_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    home::home_dir().map(|h| h.join(".grok"))
}

fn hooks_dir() -> Option<PathBuf> {
    grok_dir().map(|d| d.join("hooks"))
}

fn hooks_path() -> Option<PathBuf> {
    hooks_dir().map(|d| d.join("workmux-status.json"))
}

pub fn detect() -> Option<&'static str> {
    if grok_dir().is_some_and(|d| d.is_dir()) {
        return Some("found ~/.grok/");
    }
    None
}

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

    let content = fs::read_to_string(path).context("Failed to read grok hooks file")?;
    let config: Value =
        serde_json::from_str(&content).context("grok hooks file is not valid JSON")?;

    if hooks::has_required_hook_commands(&config, &load_hooks()?) {
        Ok(StatusCheck::Installed)
    } else if hooks::has_workmux_hooks(&config) {
        Ok(StatusCheck::UpdateAvailable)
    } else {
        Ok(StatusCheck::NotInstalled)
    }
}

pub fn uninstall() -> Result<String> {
    let Some(path) = hooks_path() else {
        return Ok("Grok dir not found, nothing to uninstall".to_string());
    };
    uninstall_at(path)
}

fn uninstall_at(path: PathBuf) -> Result<String> {
    let result = json_config::json_hook_uninstall(
        &path,
        &JsonHookUninstallSpec {
            messages: json_config::JsonHookUninstallMessages {
                file_missing: "No Grok workmux-status.json found",
                not_found: "No workmux hooks found in Grok hooks file",
                soft_read_error: None,
                soft_parse_error: None,
            },
            delete_if_no_hooks_remain: true,
            remove_plugins: false,
            soft_errors: false,
        },
    )?;
    Ok(result)
}

fn load_hooks() -> Result<Value> {
    json_config::hooks_from_embedded(HOOKS_JSON, "hooks config missing hooks key")
}

pub(crate) fn update_preview() -> Result<Option<UpdatePreview>> {
    let Some(path) = hooks_path().filter(|path| path.exists()) else {
        return Ok(None);
    };
    let content = fs::read_to_string(&path).context("Failed to read grok hooks file")?;
    let installed: Value =
        serde_json::from_str(&content).context("grok hooks file is not valid JSON")?;
    let mut bundled = installed.clone();
    hooks::merge_missing_hook_commands(&mut bundled, &load_hooks()?)?;

    Ok(Some(UpdatePreview {
        label: path.display().to_string(),
        installed: serde_json::to_string_pretty(&installed)? + "\n",
        bundled: serde_json::to_string_pretty(&bundled)? + "\n",
    }))
}

fn install_hooks_at(path: &Path) -> Result<()> {
    json_config::json_hook_install(
        path,
        &load_hooks()?,
        &JsonHookInstallSpec {
            read_context: "Failed to read grok hooks file",
            parse_context: "grok hooks file is not valid JSON",
            write_context: "Failed to write grok hooks file",
            mkdir_context: "Failed to create ~/.grok/hooks/ directory",
            empty_root: EmptyJsonRoot::HooksObject,
        },
    )
}

pub fn install() -> Result<String> {
    let path = hooks_path().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    install_hooks_at(&path)?;

    Ok(format!("Installed hooks to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hooks_json_is_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(HOOKS_JSON).expect("embedded hooks config is valid JSON");
        let hooks = parsed.get("hooks").unwrap().as_object().unwrap();
        assert!(hooks.contains_key("SessionStart"));
        assert!(hooks.contains_key("UserPromptSubmit"));
        assert!(hooks.contains_key("Notification"));
        assert!(hooks.contains_key("PostToolUse"));
        assert!(hooks.contains_key("Stop"));
        assert!(hooks.contains_key("SessionEnd"));
    }

    #[test]
    fn test_hooks_json_contains_workmux_commands() {
        assert!(HOOKS_JSON.contains("workmux register-agent"));
        assert!(HOOKS_JSON.contains("workmux set-window-status working"));
        assert!(HOOKS_JSON.contains("workmux set-window-status waiting"));
        assert!(HOOKS_JSON.contains("workmux set-window-status done"));
    }

    #[test]
    fn test_load_hooks() {
        let hooks = load_hooks().unwrap();
        let obj = hooks.as_object().unwrap();
        assert!(obj.contains_key("SessionStart"));
        assert!(obj.contains_key("UserPromptSubmit"));
        assert!(obj.contains_key("Notification"));
        assert!(obj.contains_key("PostToolUse"));
        assert!(obj.contains_key("Stop"));
        assert!(obj.contains_key("SessionEnd"));
    }

    #[test]
    fn test_check_requires_complete_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workmux-status.json");
        let mut config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        config["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("Notification");
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        assert!(matches!(
            check_at(&path).unwrap(),
            StatusCheck::UpdateAvailable
        ));

        install_hooks_at(&path).unwrap();

        assert!(matches!(check_at(&path).unwrap(), StatusCheck::Installed));
    }

    #[test]
    fn test_install_upgrades_status_only_hooks_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workmux-status.json");
        let mut config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        config["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("SessionStart");
        config["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "hooks": [{
                    "type": "command",
                    "command": "python3 my-hook.py"
                }]
            }));
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        install_hooks_at(&path).unwrap();
        install_hooks_at(&path).unwrap();

        let installed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(hooks::has_required_hook_commands(
            &installed,
            &load_hooks().unwrap()
        ));
        let registration_groups = installed["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(registration_groups.len(), 1);
        assert!(
            installed["hooks"]["Stop"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|group| group["hooks"].as_array().unwrap())
                .any(|hook| hook["command"] == "python3 my-hook.py")
        );
    }

    #[test]
    fn test_uninstall_no_hooks_file() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("workmux-status.json");
        let result = uninstall_at(hooks_path).unwrap();
        assert!(result.contains("No Grok workmux-status.json found"));
    }

    #[test]
    fn test_uninstall_removes_hooks_keeps_others() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("workmux-status.json");
        std::fs::write(
            &hooks_path,
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"workmux register-agent"},{"type":"command","command":"python3 session-hook.py"}]}],"Stop":[{"hooks":[{"type":"command","command":"workmux set-window-status done"}]},{"hooks":[{"type":"command","command":"python3 my-hook.py"}]}]}}"#,
        )
        .unwrap();
        let result = uninstall_at(hooks_path.clone()).unwrap();
        assert!(result.contains("Removed workmux hooks"));
        assert!(hooks_path.exists());
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        let config: Value = serde_json::from_str(&content).unwrap();
        let stop = config["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(
            stop[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("my-hook")
        );
        assert_eq!(
            config["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "python3 session-hook.py"
        );
    }

    #[test]
    fn test_uninstall_deletes_file_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("workmux-status.json");
        std::fs::write(
            &hooks_path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"workmux set-window-status done"}]}]}}"#,
        )
        .unwrap();
        let result = uninstall_at(hooks_path.clone()).unwrap();
        assert!(result.contains("no hooks remain"));
        assert!(!hooks_path.exists());
    }

    #[test]
    fn test_uninstall_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("workmux-status.json");
        std::fs::write(
            &hooks_path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"workmux set-window-status done"}]}]}}"#,
        )
        .unwrap();
        let result1 = uninstall_at(hooks_path.clone()).unwrap();
        assert!(result1.contains("no hooks remain"));
        assert!(!hooks_path.exists());
        let result2 = uninstall_at(hooks_path).unwrap();
        assert!(result2.contains("No Grok"), "result2: {result2}");
    }
}
