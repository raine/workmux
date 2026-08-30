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
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue, value};

use super::{StatusCheck, UpdatePreview};
use crate::agent_setup::hooks;
use crate::agent_setup::json_config::{
    self, EmptyJsonRoot, JsonHookInstallSpec, JsonHookUninstallSpec,
};

/// Hooks configuration embedded at compile time.
const HOOKS_JSON: &str = include_str!("../../resources/codex/hooks/workmux-status.json");

fn codex_dir() -> Option<PathBuf> {
    codex_dir_from_env(
        home::home_dir(),
        std::env::var_os("CODEX_HOME"),
        std::env::var_os("CODEX_CONFIG_DIR"),
    )
}

fn codex_dir_from_env(
    home: Option<PathBuf>,
    codex_home: Option<OsString>,
    legacy_config_dir: Option<OsString>,
) -> Option<PathBuf> {
    codex_home
        .filter(|dir| !dir.is_empty())
        .or_else(|| legacy_config_dir.filter(|dir| !dir.is_empty()))
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".codex")))
}

fn hooks_path() -> Option<PathBuf> {
    codex_dir().map(|d| d.join("hooks.json"))
}

fn config_toml_path() -> Option<PathBuf> {
    codex_dir().map(|d| d.join("config.toml"))
}

/// Detect if Codex is present via filesystem.
pub fn detect() -> Option<&'static str> {
    if codex_dir().is_some_and(|d| d.is_dir()) {
        return Some("found ~/.codex/");
    }
    None
}

/// Check if all workmux hooks are installed in Codex hooks.json.
pub fn check() -> Result<StatusCheck> {
    let (Some(hooks_path), Some(config_path)) = (hooks_path(), config_toml_path()) else {
        return Ok(StatusCheck::NotInstalled);
    };

    check_at(&hooks_path, &config_path)
}

fn check_at(hooks_path: &Path, config_path: &Path) -> Result<StatusCheck> {
    if !hooks_path.exists() {
        return Ok(StatusCheck::NotInstalled);
    }

    let content = fs::read_to_string(hooks_path).context("Failed to read ~/.codex/hooks.json")?;
    let config: Value =
        serde_json::from_str(&content).context("~/.codex/hooks.json is not valid JSON")?;

    let required = load_hooks()?;
    if hooks::has_required_hook_commands(&config, &required)
        && hooks_feature_enabled_at(config_path)?
    {
        Ok(StatusCheck::Installed)
    } else if hooks::has_workmux_hooks(&config) {
        Ok(StatusCheck::UpdateAvailable)
    } else {
        Ok(StatusCheck::NotInstalled)
    }
}

#[cfg(test)]
fn has_status_hook(config: &Value, event: &str, status: &str) -> bool {
    let expected = format!("workmux set-window-status {status}");
    config["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .any(|command| command.contains(&expected))
}

/// Remove workmux hooks from Codex hooks.json.
///
/// Removes only workmux hook entries from hooks.json. If the file
/// becomes empty of all hooks, deletes it entirely. Preserves any
/// user-configured hooks from other tools.
pub fn uninstall() -> Result<String> {
    let Some(path) = hooks_path() else {
        return Ok("Codex dir not found, nothing to uninstall".to_string());
    };
    uninstall_at(path)
}

fn uninstall_at(path: PathBuf) -> Result<String> {
    json_config::json_hook_uninstall(
        &path,
        &JsonHookUninstallSpec {
            messages: json_config::JsonHookUninstallMessages {
                file_missing: "No Codex hooks.json found",
                not_found: "No workmux hooks found in Codex hooks.json",
                soft_read_error: None,
                soft_parse_error: None,
            },
            delete_if_no_hooks_remain: true,
            remove_plugins: false,
            soft_errors: false,
        },
    )
}

fn load_hooks() -> Result<Value> {
    json_config::hooks_from_embedded(HOOKS_JSON, "hooks config missing hooks key")
}

pub(crate) fn update_preview() -> Result<Option<UpdatePreview>> {
    let (Some(hooks_path), Some(config_path)) = (hooks_path(), config_toml_path()) else {
        return Ok(None);
    };
    if !hooks_path.exists() {
        return Ok(None);
    }

    let hooks_content =
        fs::read_to_string(&hooks_path).context("Failed to read ~/.codex/hooks.json")?;
    let installed_hooks: Value =
        serde_json::from_str(&hooks_content).context("~/.codex/hooks.json is not valid JSON")?;
    let mut bundled_hooks = installed_hooks.clone();
    normalize_subagent_stop_status(&mut bundled_hooks);
    hooks::merge_missing_hook_commands(&mut bundled_hooks, &load_hooks()?)?;
    let installed_hooks = serde_json::to_string_pretty(&installed_hooks)? + "\n";
    let bundled_hooks = serde_json::to_string_pretty(&bundled_hooks)? + "\n";

    let installed_config = if config_path.exists() {
        fs::read_to_string(&config_path).context("Failed to read ~/.codex/config.toml")?
    } else {
        String::new()
    };
    let bundled_config = enable_hooks_feature(&installed_config)?;

    let (label, installed, bundled) = if installed_config == bundled_config {
        (
            hooks_path.display().to_string(),
            installed_hooks,
            bundled_hooks,
        )
    } else if installed_hooks == bundled_hooks {
        (
            config_path.display().to_string(),
            installed_config,
            bundled_config,
        )
    } else {
        let label = codex_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Codex configuration".to_string());
        (
            label,
            format!(
                "{}:\n{}{}:\n{}",
                hooks_path.display(),
                installed_hooks,
                config_path.display(),
                installed_config
            ),
            format!(
                "{}:\n{}{}:\n{}",
                hooks_path.display(),
                bundled_hooks,
                config_path.display(),
                bundled_config
            ),
        )
    };

    Ok(Some(UpdatePreview {
        label,
        installed,
        bundled,
    }))
}

fn parse_config_toml(content: &str) -> Result<DocumentMut> {
    if content.trim().is_empty() {
        Ok(DocumentMut::new())
    } else {
        content
            .parse::<DocumentMut>()
            .context("~/.codex/config.toml is not valid TOML")
    }
}

fn is_hooks_feature_enabled(content: &str) -> Result<bool> {
    let config = parse_config_toml(content)?;
    Ok(config
        .get("features")
        .and_then(|features| features.get("hooks"))
        .and_then(Item::as_bool)
        == Some(true))
}

fn hooks_feature_enabled_at(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let content = fs::read_to_string(path).context("Failed to read ~/.codex/config.toml")?;
    is_hooks_feature_enabled(&content)
}

fn enable_hooks_feature(content: &str) -> Result<String> {
    if is_hooks_feature_enabled(content)? {
        return Ok(content.to_string());
    }

    let mut config = parse_config_toml(content)?;
    if !config.contains_key("features") {
        config["features"] = Item::Table(Table::new());
    }
    let features = &mut config["features"];
    if let Some(table) = features.as_table_mut() {
        table["hooks"] = value(true);
    } else if let Some(table) = features.as_inline_table_mut() {
        table.insert("hooks", TomlValue::from(true));
    } else {
        anyhow::bail!("~/.codex/config.toml features value is not a table");
    }
    Ok(config.to_string())
}

/// Ensure `hooks = true` is set under `[features]` in config.toml.
/// Returns true if the file was modified.
fn ensure_hooks_feature_flag() -> Result<bool> {
    let path =
        config_toml_path().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    ensure_hooks_feature_flag_at(&path)
}

fn ensure_hooks_feature_flag_at(path: &Path) -> Result<bool> {
    let content = if path.exists() {
        fs::read_to_string(path).context("Failed to read ~/.codex/config.toml")?
    } else {
        String::new()
    };
    let updated = enable_hooks_feature(&content)?;
    if updated == content {
        return Ok(false);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("Failed to create ~/.codex/ directory")?;
    }
    fs::write(path, updated).context("Failed to write ~/.codex/config.toml")?;
    Ok(true)
}

fn normalize_subagent_stop_status(config: &mut Value) -> bool {
    let Some(groups) = config["hooks"]["SubagentStop"].as_array_mut() else {
        return false;
    };

    let mut modified = false;
    for hook in groups
        .iter_mut()
        .filter_map(|group| group.get_mut("hooks").and_then(Value::as_array_mut))
        .flatten()
    {
        let Some(command) = hook.get_mut("command") else {
            continue;
        };
        if command.as_str() == Some("workmux set-window-status done") {
            *command = Value::String("workmux set-window-status working".to_string());
            modified = true;
        }
    }
    modified
}

fn normalize_installed_hooks(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(path).context("Failed to read ~/.codex/hooks.json")?;
    let mut config: Value =
        serde_json::from_str(&content).context("~/.codex/hooks.json is not valid JSON")?;
    if normalize_subagent_stop_status(&mut config) {
        let output = serde_json::to_string_pretty(&config)?;
        fs::write(path, output + "\n").context("Failed to write ~/.codex/hooks.json")?;
    }
    Ok(())
}

fn install_hooks_at(path: &Path) -> Result<()> {
    normalize_installed_hooks(path)?;
    json_config::json_hook_install_with(
        path,
        &load_hooks()?,
        &JsonHookInstallSpec {
            read_context: "Failed to read ~/.codex/hooks.json",
            parse_context: "~/.codex/hooks.json is not valid JSON",
            write_context: "Failed to write ~/.codex/hooks.json",
            mkdir_context: "Failed to create ~/.codex/ directory",
            empty_root: EmptyJsonRoot::HooksObject,
        },
        hooks::merge_missing_hook_commands,
    )
}

/// Install workmux hooks into `~/.codex/hooks.json`.
///
/// Merges hook groups into existing hooks without clobbering or creating
/// duplicates. Returns a description of what was done.
pub fn install() -> Result<String> {
    let path = hooks_path().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    install_hooks_at(&path)?;

    let mut message = format!("Installed hooks to {}", path.display());
    if ensure_hooks_feature_flag()?
        && let Some(config_path) = config_toml_path()
    {
        message.push_str(&format!(", enabled hooks in {}", config_path.display()));
    }

    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_root_prefers_non_empty_current_then_legacy_env() {
        let home = Some(PathBuf::from("/home/tester"));
        assert_eq!(
            codex_dir_from_env(
                home.clone(),
                Some(OsString::from("/current")),
                Some(OsString::from("/legacy")),
            ),
            Some(PathBuf::from("/current"))
        );
        assert_eq!(
            codex_dir_from_env(
                home.clone(),
                Some(OsString::new()),
                Some(OsString::from("/legacy")),
            ),
            Some(PathBuf::from("/legacy"))
        );
        assert_eq!(
            codex_dir_from_env(home, Some(OsString::new()), Some(OsString::new())),
            Some(PathBuf::from("/home/tester/.codex"))
        );
    }

    #[test]
    fn test_hooks_json_is_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(HOOKS_JSON).expect("embedded hooks config is valid JSON");
        let hooks = parsed.get("hooks").unwrap().as_object().unwrap();
        assert!(hooks.contains_key("SessionStart"));
        assert!(hooks.contains_key("UserPromptSubmit"));
        assert!(hooks.contains_key("PermissionRequest"));
        assert!(hooks.contains_key("PostToolUse"));
        assert!(hooks.contains_key("SubagentStart"));
        assert!(hooks.contains_key("SubagentStop"));
        assert!(hooks.contains_key("Stop"));
    }

    #[test]
    fn test_hooks_json_contains_workmux_commands() {
        assert!(HOOKS_JSON.contains("workmux set-window-status working"));
        assert!(HOOKS_JSON.contains("workmux set-window-status waiting"));
        assert!(HOOKS_JSON.contains("workmux set-window-status done"));

        let config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        let session_start = &config["hooks"]["SessionStart"];
        assert_eq!(session_start[0]["matcher"], "startup|resume|clear");
        assert_eq!(
            session_start[0]["hooks"][0]["command"],
            "workmux register-agent"
        );
        assert!(
            !session_start[0]["matcher"]
                .as_str()
                .unwrap()
                .contains("compact")
        );
    }

    #[test]
    fn test_load_hooks() {
        let hooks = load_hooks().unwrap();
        let obj = hooks.as_object().unwrap();
        assert!(obj.contains_key("SessionStart"));
        assert!(obj.contains_key("UserPromptSubmit"));
        assert!(obj.contains_key("PermissionRequest"));
        assert!(obj.contains_key("PostToolUse"));
        assert!(obj.contains_key("SubagentStart"));
        assert!(obj.contains_key("SubagentStop"));
        assert!(obj.contains_key("Stop"));
    }

    #[test]
    fn test_install_normalizes_subagent_stop_status() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hooks.json");
        let mut config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        config["hooks"]["SubagentStop"][0]["hooks"][0]["command"] =
            Value::String("workmux set-window-status done".to_string());
        config["hooks"]["SubagentStop"]
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
        assert!(has_status_hook(&installed, "SubagentStop", "working"));
        assert!(!has_status_hook(&installed, "SubagentStop", "done"));
        assert!(
            installed["hooks"]["SubagentStop"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|group| group["hooks"].as_array().unwrap())
                .any(|hook| hook["command"] == "python3 my-hook.py")
        );
    }

    #[test]
    fn test_check_reports_update_for_status_only_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("hooks.json");
        let config_path = tmp.path().join("config.toml");
        let mut config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        config["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("SessionStart");
        fs::write(&hooks_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        fs::write(&config_path, "[features]\nhooks = true\n").unwrap();

        assert!(matches!(
            check_at(&hooks_path, &config_path).unwrap(),
            StatusCheck::UpdateAvailable
        ));

        install_hooks_at(&hooks_path).unwrap();

        assert!(matches!(
            check_at(&hooks_path, &config_path).unwrap(),
            StatusCheck::Installed
        ));
    }

    #[test]
    fn test_check_reports_update_when_hooks_feature_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("hooks.json");
        let config_path = tmp.path().join("config.toml");
        fs::write(&hooks_path, HOOKS_JSON).unwrap();
        fs::write(&config_path, "[features]\nhooks = false\n").unwrap();

        assert!(matches!(
            check_at(&hooks_path, &config_path).unwrap(),
            StatusCheck::UpdateAvailable
        ));
    }

    #[test]
    fn test_install_upgrades_legacy_hooks_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hooks.json");
        let mut config: Value = serde_json::from_str(HOOKS_JSON).unwrap();
        config["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("SessionStart");
        config["hooks"]["Stop"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "type": "command",
                "command": "python3 my-hook.py"
            }));
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        install_hooks_at(&path).unwrap();
        let after_first = fs::read_to_string(&path).unwrap();
        install_hooks_at(&path).unwrap();
        let after_second = fs::read_to_string(&path).unwrap();

        assert_eq!(after_first, after_second);
        let installed: Value = serde_json::from_str(&after_second).unwrap();
        assert_eq!(
            installed["hooks"]["SessionStart"].as_array().unwrap().len(),
            1
        );
        let stop_groups = installed["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop_groups.len(), 1);
        assert!(
            stop_groups[0]["hooks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hook| hook["command"] == "python3 my-hook.py")
        );
    }

    #[test]
    fn test_is_hooks_feature_enabled_only_in_features_table() {
        assert!(is_hooks_feature_enabled("[features]\nhooks = true\n").unwrap());
        assert!(!is_hooks_feature_enabled("[features]\nhooks = false\n").unwrap());
        assert!(!is_hooks_feature_enabled("[other]\nhooks = true\n").unwrap());
        assert!(!is_hooks_feature_enabled("").unwrap());
    }

    #[test]
    fn test_enable_hooks_feature_preserves_unrelated_tables() {
        let content = "[other]\nhooks = false\n\n[features]\nimages = true\n";
        let updated = enable_hooks_feature(content).unwrap();
        let config = updated.parse::<DocumentMut>().unwrap();

        assert_eq!(
            config["other"]["hooks"].as_bool(),
            Some(false),
            "unrelated hooks key is preserved"
        );
        assert_eq!(config["features"]["images"].as_bool(), Some(true));
        assert_eq!(config["features"]["hooks"].as_bool(), Some(true));
    }

    #[test]
    fn test_enable_hooks_feature_handles_inline_features() {
        let updated =
            enable_hooks_feature("features = { hooks = false, images = true }\n").unwrap();
        let config = updated.parse::<DocumentMut>().unwrap();

        assert_eq!(config["features"]["hooks"].as_bool(), Some(true));
        assert_eq!(config["features"]["images"].as_bool(), Some(true));
    }

    #[test]
    fn test_ensure_hooks_feature_flag_is_structural_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[other]\nhooks = false\n").unwrap();

        assert!(ensure_hooks_feature_flag_at(&path).unwrap());
        assert!(!ensure_hooks_feature_flag_at(&path).unwrap());
        let config = fs::read_to_string(&path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(config["other"]["hooks"].as_bool(), Some(false));
        assert_eq!(config["features"]["hooks"].as_bool(), Some(true));
    }

    #[test]
    fn test_uninstall_no_hooks_file() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("hooks.json");
        let result = uninstall_at(hooks_path).unwrap();
        assert!(result.contains("No Codex hooks.json found"));
    }

    #[test]
    fn test_uninstall_removes_hooks_keeps_others() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("hooks.json");
        std::fs::write(
            &hooks_path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"workmux set-window-status done"}]},{"hooks":[{"type":"command","command":"python3 my-hook.py"}]}]}}"#,
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
    }

    #[test]
    fn test_uninstall_deletes_file_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("hooks.json");
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
        let hooks_path = tmp.path().join("hooks.json");
        std::fs::write(
            &hooks_path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"workmux set-window-status done"}]}]}}"#,
        )
        .unwrap();
        let result1 = uninstall_at(hooks_path.clone()).unwrap();
        assert!(result1.contains("no hooks remain"));
        assert!(!hooks_path.exists());
        let result2 = uninstall_at(hooks_path).unwrap();
        assert!(result2.contains("No Codex"), "result2: {result2}");
    }
}
