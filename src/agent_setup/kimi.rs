//! Kimi Code (Moonshot AI) status tracking setup.
//!
//! Detects Kimi Code via the `~/.kimi-code/` directory (or `$KIMI_CODE_HOME`).
//! Installs hooks by merging into `~/.kimi-code/config.toml`.

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

use crate::agent_setup::hooks;

use super::{StatusCheck, UpdatePreview};

const HOOKS_TOML: &str = include_str!("../../resources/kimi-code/config.toml");

fn kimi_code_dir() -> Option<PathBuf> {
    kimi_code_dir_from_env(home::home_dir(), std::env::var_os("KIMI_CODE_HOME"))
}

fn kimi_code_dir_from_env(home: Option<PathBuf>, cli_home: Option<OsString>) -> Option<PathBuf> {
    cli_home
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".kimi-code")))
}

fn config_path() -> Option<PathBuf> {
    kimi_code_dir().map(|d| d.join("config.toml"))
}

/// Detect if Kimi Code is present via filesystem.
pub fn detect() -> Option<&'static str> {
    if kimi_code_dir().is_some_and(|d| d.is_dir()) {
        return Some("found ~/.kimi-code/");
    }
    None
}

/// Check if workmux hooks are installed in Kimi Code config.toml
pub fn check() -> Result<StatusCheck> {
    let Some(path) = config_path() else {
        return Ok(StatusCheck::NotInstalled);
    };

    check_at(&path)
}

fn check_at(path: &Path) -> Result<StatusCheck> {
    if !path.exists() {
        return Ok(StatusCheck::NotInstalled);
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;

    let config = parse_config_toml(&content)?;

    let Some(installed) = hooks_array(&config)? else {
        return Ok(StatusCheck::NotInstalled);
    };
    let required = load_hooks()?;

    if has_required_hook_commands(installed, &required) {
        Ok(StatusCheck::Installed)
    } else if has_workmux_hooks(installed) {
        Ok(StatusCheck::UpdateAvailable)
    } else {
        Ok(StatusCheck::NotInstalled)
    }
}

/// Remove workmux hooks from Kimi Code `config.toml`
///
/// Uses local TOML helpers to surgically remove only workmux entries,
/// preserving any user-configured hooks. Returns a description of what
/// was done.
pub fn uninstall() -> Result<String> {
    let Some(path) = config_path() else {
        return Ok("Kimi Code config dir not found, nothing to uninstall".to_string());
    };
    uninstall_at(&path)
}

fn uninstall_at(path: &Path) -> Result<String> {
    toml_hook_uninstall(path)
}

pub(crate) fn update_preview() -> Result<Option<UpdatePreview>> {
    let Some(path) = config_path().filter(|path| path.exists()) else {
        return Ok(None);
    };
    let content =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;

    let installed = parse_config_toml(&content)?;

    let mut bundled = installed.clone();
    replace_workmux_hooks(&mut bundled, &load_hooks()?)?;

    Ok(Some(UpdatePreview {
        label: path.display().to_string(),
        installed: installed.to_string(),
        bundled: bundled.to_string(),
    }))
}

/// Install workmux hooks into `~/.kimi-code/config.toml`.
///
/// Appends or replaces workmux hooks to existing config.
/// Returns a description of what was done.
pub fn install() -> Result<String> {
    let path =
        config_path().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    install_at(&path)
}

fn install_at(path: &Path) -> Result<String> {
    toml_hook_install(path)
}

fn has_required_hook_commands(installed: &ArrayOfTables, required: &ArrayOfTables) -> bool {
    required
        .iter()
        .all(|req| installed.iter().any(|inst| hook_key(inst) == hook_key(req)))
}

fn has_workmux_hooks(hooks: &ArrayOfTables) -> bool {
    hooks.iter().any(is_workmux_entry)
}

// TOML helpers

fn parse_config_toml(content: &str) -> Result<DocumentMut> {
    if content.trim().is_empty() {
        Ok(DocumentMut::new())
    } else {
        content
            .parse::<DocumentMut>()
            .context("~/.kimi-code/config.toml is not valid TOML")
    }
}

fn is_workmux_entry(entry: &Table) -> bool {
    entry
        .get("command")
        .and_then(Item::as_str)
        .is_some_and(hooks::is_workmux_hook_command)
}

fn hook_key(entry: &Table) -> (Option<&str>, Option<&str>, Option<&str>) {
    let field = |key| entry.get(key).and_then(Item::as_str);
    let matcher = field("matcher").filter(|m| !m.is_empty());
    (field("event"), matcher, field("command"))
}

/// removes previous workmux hooks and
/// installs the updated ones.
fn replace_workmux_hooks(
    config_root: &mut DocumentMut,
    hooks_to_add: &ArrayOfTables,
) -> Result<()> {
    if !config_root.contains_key("hooks") {
        config_root["hooks"] = Item::ArrayOfTables(ArrayOfTables::new());
    };

    let previous_hooks =
        hooks_array_mut(config_root)?.context("hooks array missing after insert")?;

    previous_hooks.retain(|entry| !is_workmux_entry(entry));

    for entry in hooks_to_add {
        previous_hooks.push(entry.clone());
    }

    Ok(())
}

fn load_hooks() -> Result<ArrayOfTables> {
    let doc = parse_config_toml(HOOKS_TOML)?;
    load_hooks_at(&doc)
}

fn load_hooks_at(doc: &DocumentMut) -> Result<ArrayOfTables> {
    hooks_array(doc)?
        .cloned()
        .context("hooks template missing [[hooks]] entries")
}

fn hooks_array(doc: &DocumentMut) -> Result<Option<&ArrayOfTables>> {
    let Some(item) = doc.get("hooks") else {
        return Ok(None);
    };
    item.as_array_of_tables().map(Some).ok_or_else(|| {
        anyhow::anyhow!(
            "`hooks` in config.toml must be written as [[hooks]] tables. Move each entry into its own [[hooks]] block and run `workmux setup` again"
        )
    })
}

fn hooks_array_mut(doc: &mut DocumentMut) -> Result<Option<&mut ArrayOfTables>> {
    let Some(item) = doc.get_mut("hooks") else {
        return Ok(None);
    };
    item.as_array_of_tables_mut().map(Some).ok_or_else(|| {
        anyhow::anyhow!(
            "`hooks` in config.toml must be written as [[hooks]] tables. Move each entry into its own [[hooks]] block and run `workmux setup` again"
        )
    })
}

fn toml_hook_uninstall(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok("No Kimi Code config.toml found".to_string());
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;

    let mut config = parse_config_toml(&content)?;
    let Some(hooks) = hooks_array_mut(&mut config)? else {
        return Ok("No workmux hooks found in Kimi Code config.toml".to_string());
    };

    let previous_len = hooks.len();
    hooks.retain(|entry| !is_workmux_entry(entry));
    let remaining = hooks.len();

    if remaining == previous_len {
        return Ok("No workmux hooks found in Kimi Code config.toml".to_string());
    }
    if remaining == 0 {
        config.remove("hooks");
    }

    fs::write(path, config.to_string())
        .with_context(|| format!("Failed to write {}", path.display()))?;

    Ok(format!("Removed workmux hooks from {}", path.display()))
}

fn toml_hook_install(path: &Path) -> Result<String> {
    let mut config = if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        parse_config_toml(&content)?
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {} directory", parent.display()))?;
        }
        DocumentMut::new()
    };

    replace_workmux_hooks(&mut config, &load_hooks()?)?;

    let output = config.to_string();
    fs::write(path, output).with_context(|| format!("Failed to write {}", path.display()))?;

    Ok(format!("Installed hooks to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_HOOK: &str = r#"[[hooks]]
event = "Notification" # Trigger: when a background task status changes
matcher = "task\\.completed"
command = "terminal-notifier -title Kimi -message 'Task done'"
"#;

    #[test]
    fn kimi_code_root_uses_env_config_dir_then_home() {
        let home = Some(PathBuf::from("/home/tester"));
        assert_eq!(
            kimi_code_dir_from_env(home.clone(), Some(OsString::from("/profile/.kimi-code")),),
            Some(PathBuf::from("/profile/.kimi-code"))
        );
        assert_eq!(
            kimi_code_dir_from_env(home, Some(OsString::new())),
            Some(PathBuf::from("/home/tester/.kimi-code"))
        );
    }

    #[test]
    fn test_hooks_toml_is_valid() {
        let hooks = load_hooks().unwrap();

        let has_event = |expected: &str| {
            hooks.iter().any(|table| {
                table
                    .get("event")
                    .and_then(toml_edit::Item::as_str)
                    .is_some_and(|name| name == expected)
            })
        };

        assert!(has_event("SessionStart"));
        assert!(has_event("UserPromptSubmit"));
        assert!(has_event("PermissionRequest"));
        assert!(has_event("PermissionResult"));
        assert!(has_event("PostToolUse"));
        assert!(has_event("SubagentStart"));
        assert!(has_event("SubagentStop"));
        assert!(has_event("Stop"));

        let session_start = hooks
            .iter()
            .find(|entry| entry.get("event").and_then(Item::as_str) == Some("SessionStart"))
            .expect("template has a SessionStart hook");

        assert_eq!(
            session_start.get("matcher").and_then(Item::as_str),
            Some("startup|resume")
        );

        assert_eq!(
            session_start.get("command").and_then(Item::as_str),
            Some("workmux register-agent")
        );
    }

    #[test]
    fn test_install_then_check_reports_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        toml_hook_install(&path).unwrap();

        assert!(matches!(check_at(&path).unwrap(), StatusCheck::Installed));
    }

    #[test]
    fn test_check_reports_update_for_incomplete_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut config = parse_config_toml(HOOKS_TOML).unwrap();
        let hooks = hooks_array_mut(&mut config)
            .unwrap()
            .expect("config has a `hooks` key");
        hooks.retain(|entry| entry.get("event").and_then(Item::as_str) != Some("SessionStart"));

        fs::write(&config_path, config.to_string()).unwrap();
        assert!(matches!(
            check_at(&config_path).unwrap(),
            StatusCheck::UpdateAvailable
        ));

        install_at(&config_path).unwrap();
        assert!(matches!(
            check_at(&config_path).unwrap(),
            StatusCheck::Installed
        ));
    }

    #[test]
    fn test_no_hooks_reports_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        fs::write(&config_path, "default_model = \"kimi-code/k3\"").unwrap();

        assert!(matches!(
            check_at(&config_path).unwrap(),
            StatusCheck::NotInstalled
        ));
    }

    #[test]
    fn test_user_hooks_reports_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        fs::write(&config_path, USER_HOOK).unwrap();

        assert!(matches!(
            check_at(&config_path).unwrap(),
            StatusCheck::NotInstalled
        ));
    }

    #[test]
    fn test_empty_matcher_reports_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut config = parse_config_toml(HOOKS_TOML).unwrap();
        let hooks = hooks_array_mut(&mut config)
            .unwrap()
            .expect("template has hooks");
        for entry in hooks.iter_mut() {
            if entry.get("matcher").is_none() {
                entry["matcher"] = toml_edit::value("");
            }
        }
        fs::write(&config_path, config.to_string()).unwrap();

        assert!(matches!(
            check_at(&config_path).unwrap(),
            StatusCheck::Installed
        ));
    }

    #[test]
    fn test_inline_hooks_reports_error() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let contents_i = "hooks = [{ event = \"Stop\", command = \"script.sh\" }]";
        fs::write(&config_path, contents_i).unwrap();

        assert!(install_at(&config_path).is_err());
        let contents = fs::read_to_string(&config_path).unwrap();
        assert_eq!(contents_i, contents);
    }

    #[test]
    fn test_invalid_toml_reports_error() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let contents_i = "hooks = [[";
        fs::write(&config_path, contents_i).unwrap();

        assert!(install_at(&config_path).is_err());

        let contents = fs::read_to_string(&config_path).unwrap();
        assert_eq!(contents_i, contents);
    }

    #[test]
    fn test_malformed_entry_survives_uninstall() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let contents = format!(
            "{HOOKS_TOML}\n[[hooks]]\nevent = \"SubagentStop\"\nmatcher = \"\"\ncommand = 1\n[[hooks]]\nevent = \"SubagentStop\"\nmatcher = \"\""
        );

        fs::write(&config_path, &contents).unwrap();

        let result = uninstall_at(&config_path).unwrap();
        assert!(result.contains("Removed workmux hooks"));
        let after = fs::read_to_string(&config_path).unwrap();
        assert_eq!(
            hooks_array(&parse_config_toml(&after).unwrap())
                .unwrap()
                .expect("config has `hooks` key")
                .len(),
            2
        );
    }

    #[test]
    fn test_empty_config_installs_successfully() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        fs::write(&config_path, "").unwrap();

        install_at(&config_path).unwrap();

        assert!(matches!(
            check_at(&config_path).unwrap(),
            StatusCheck::Installed
        ));
    }

    #[test]
    fn test_install_replaces_outdated_hooks_and_preserves_user_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let contents = format!(
            "{USER_HOOK}\n[[hooks]]\nevent = \"Notification\"\nmatcher = \"permission_prompt\"\ncommand = \"workmux set-window-status waiting\""
        );

        fs::write(&config_path, contents).unwrap();
        assert!(matches!(
            check_at(&config_path).unwrap(),
            StatusCheck::UpdateAvailable
        ));

        install_at(&config_path).unwrap();
        assert!(matches!(
            check_at(&config_path).unwrap(),
            StatusCheck::Installed
        ));

        let config = fs::read_to_string(&config_path).unwrap();
        assert!(config.contains("terminal-notifier"));

        let content = fs::read_to_string(&config_path).unwrap();
        let config = parse_config_toml(&content).unwrap();
        let hooks = hooks_array(&config)
            .unwrap()
            .expect("config has a `hooks` key");

        assert!(hooks.iter().any(|entry| {
            entry.get("command").and_then(Item::as_str)
                == Some("terminal-notifier -title Kimi -message 'Task done'")
        }));

        assert!(!hooks.iter().any(|entry| {
            entry.get("matcher").and_then(Item::as_str) == Some("permission_prompt")
        }));

        assert_eq!(hooks.len(), 1 + load_hooks().unwrap().len());
    }

    #[test]
    fn test_install_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        fs::write(&config_path, USER_HOOK).unwrap();

        install_at(&config_path).unwrap();
        let config1 = fs::read_to_string(&config_path).unwrap();

        install_at(&config_path).unwrap();
        let config2 = fs::read_to_string(&config_path).unwrap();

        assert_eq!(config1, config2);
    }

    #[test]
    fn test_uninstall_no_config_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let result = uninstall_at(&config_path).unwrap();
        assert!(result.contains("No Kimi Code config.toml"));
    }

    #[test]
    fn test_uninstall_removes_hooks_only() {
        let contents = format!("default_model = \"kimi-code/k3\"\n{HOOKS_TOML}");

        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, contents).unwrap();
        let result = uninstall_at(&config_path).unwrap();
        assert!(result.contains("Removed workmux hooks"));
        let content = fs::read_to_string(&config_path).unwrap();
        let config = parse_config_toml(&content).unwrap();

        assert_eq!(
            config.get("default_model").and_then(Item::as_str),
            Some("kimi-code/k3")
        );

        assert!(config.get("hooks").is_none());
    }

    #[test]
    fn test_uninstall_removes_hooks_keeps_others() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        let contents = format!("{USER_HOOK}\n{HOOKS_TOML}");

        std::fs::write(&config_path, contents).unwrap();
        let result = uninstall_at(&config_path).unwrap();
        assert!(result.contains("Removed workmux hooks"));
        assert!(config_path.exists());
        let content = fs::read_to_string(&config_path).unwrap();
        let config = parse_config_toml(&content).unwrap();
        let hooks = load_hooks_at(&config).unwrap();
        let notification = hooks
            .iter()
            .find(|entry| entry.get("event").and_then(Item::as_str) == Some("Notification"))
            .expect("config has a Notification hook");

        assert!(content.contains("# Trigger"));

        assert_eq!(hooks.len(), 1);

        assert_eq!(
            notification.get("command").and_then(Item::as_str),
            Some("terminal-notifier -title Kimi -message 'Task done'")
        );
    }

    #[test]
    fn test_uninstall_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        let contents = format!("{USER_HOOK}\n{HOOKS_TOML}");
        std::fs::write(&config_path, contents).unwrap();

        let result1 = uninstall_at(&config_path).unwrap();
        assert!(result1.contains("Removed workmux hooks"));
        let result2 = uninstall_at(&config_path).unwrap();
        assert!(result2.contains("No workmux hooks found"));
    }
}
