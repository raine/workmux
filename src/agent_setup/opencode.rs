//! OpenCode status tracking setup.
//!
//! Resolves the plugin directory from `OPENCODE_CONFIG_DIR`,
//! `XDG_CONFIG_HOME/opencode`, or `~/.config/opencode`. `OPENCODE_CONFIG`
//! identifies a config file and does not change plugin discovery directories.
//! Installs the status plugin while preserving other OpenCode configuration.

use anyhow::{Context, Result};
use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::{StatusCheck, UpdatePreview};

/// OpenCode distribution files, embedded at compile time.
const PLUGIN_SOURCE: &str = include_str!("../../resources/opencode/plugins/workmux-status.ts");
const PACKAGE_JSON: &str = include_str!("../../resources/opencode/package.json");

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

fn resolve_config_dir(
    config_dir: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(dir) = non_empty(config_dir) {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = non_empty(xdg_config_home) {
        return Some(PathBuf::from(xdg).join("opencode"));
    }
    home.map(|home| home.join(".config/opencode"))
}

pub fn opencode_config_dir() -> Option<PathBuf> {
    resolve_config_dir(
        std::env::var_os("OPENCODE_CONFIG_DIR"),
        std::env::var_os("XDG_CONFIG_HOME"),
        home::home_dir(),
    )
}

/// Detect if OpenCode is present via filesystem.
/// Returns the reason string if detected, None otherwise.
pub fn detect() -> Option<&'static str> {
    if non_empty(std::env::var_os("OPENCODE_CONFIG_DIR"))
        .is_some_and(|dir| PathBuf::from(dir).is_dir())
    {
        return Some("found $OPENCODE_CONFIG_DIR");
    }
    if non_empty(std::env::var_os("OPENCODE_CONFIG"))
        .is_some_and(|file| PathBuf::from(file).is_file())
    {
        return Some("found $OPENCODE_CONFIG");
    }
    if opencode_config_dir().is_some_and(|dir| dir.is_dir()) {
        return Some("found OpenCode config directory");
    }

    None
}

fn check_at(config_dir: &Path) -> Result<StatusCheck> {
    let plugin = config_dir.join("plugins/workmux-status.ts");
    if plugin.exists() {
        let installed = fs::read_to_string(&plugin)
            .with_context(|| format!("Failed to read OpenCode plugin {}", plugin.display()))?;
        return if installed == PLUGIN_SOURCE {
            Ok(StatusCheck::Installed)
        } else {
            Ok(StatusCheck::UpdateAvailable)
        };
    }

    if config_dir.join("plugin/workmux-status.ts").exists() {
        Ok(StatusCheck::UpdateAvailable)
    } else {
        Ok(StatusCheck::NotInstalled)
    }
}

/// Check if workmux plugin is installed for OpenCode.
pub fn check() -> Result<StatusCheck> {
    let Some(config_dir) = opencode_config_dir() else {
        return Ok(StatusCheck::NotInstalled);
    };
    check_at(&config_dir)
}

pub(crate) fn update_preview() -> Result<Option<UpdatePreview>> {
    let Some(config_dir) = opencode_config_dir() else {
        return Ok(None);
    };
    let plugin = config_dir.join("plugins/workmux-status.ts");
    let legacy = config_dir.join("plugin/workmux-status.ts");
    let installed_path = if plugin.exists() {
        plugin
    } else if legacy.exists() {
        legacy
    } else {
        return Ok(None);
    };
    Ok(Some(UpdatePreview {
        label: installed_path.display().to_string(),
        installed: fs::read_to_string(&installed_path)?,
        bundled: PLUGIN_SOURCE.to_string(),
    }))
}

fn install_at(config_dir: &Path) -> Result<()> {
    let plugin = config_dir.join("plugins/workmux-status.ts");

    fs::create_dir_all(plugin.parent().expect("plugin path has a parent"))
        .context("Failed to create OpenCode plugin directory")?;
    fs::write(&plugin, PLUGIN_SOURCE).context("Failed to write OpenCode plugin")?;

    let legacy_plugin = config_dir.join("plugin/workmux-status.ts");
    if legacy_plugin.exists() {
        fs::remove_file(&legacy_plugin).context("Failed to remove legacy OpenCode plugin")?;
    }
    Ok(())
}

/// Install workmux plugin for OpenCode.
/// Returns a description of what was done.
pub fn install() -> Result<String> {
    let config_dir = opencode_config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine OpenCode config directory"))?;
    install_at(&config_dir)?;

    Ok(format!(
        "Installed OpenCode plugin to {}. Restart OpenCode for it to take effect.",
        config_dir.join("plugins/workmux-status.ts").display()
    ))
}

/// Remove workmux plugin files from OpenCode config directory.
///
/// Removes plugin files from both supported locations. It removes package.json
/// only when the file consists entirely of the bundled package configuration.
pub fn uninstall() -> Result<String> {
    let Some(config_dir) = opencode_config_dir() else {
        return Ok("No OpenCode config directory found".to_string());
    };
    uninstall_at(config_dir)
}

fn uninstall_at(config_dir: PathBuf) -> Result<String> {
    let mut removed = Vec::new();

    let plugin_path = config_dir.join("plugins/workmux-status.ts");
    if plugin_path.exists() {
        fs::remove_file(&plugin_path)?;
        removed.push(plugin_path.display().to_string());
        if let Some(parent) = plugin_path.parent()
            && parent
                .read_dir()
                .is_ok_and(|mut entries| entries.next().is_none())
        {
            let _ = fs::remove_dir(parent);
        }
    }

    let legacy_path = config_dir.join("plugin/workmux-status.ts");
    if legacy_path.exists() {
        fs::remove_file(&legacy_path)?;
        removed.push(legacy_path.display().to_string());
    }

    let package_path = config_dir.join("package.json");
    if package_path.exists() {
        let content = fs::read_to_string(&package_path)?;
        if let (Ok(installed), Ok(existing)) = (
            serde_json::from_str::<Value>(PACKAGE_JSON),
            serde_json::from_str::<Value>(&content),
        ) && installed == existing
        {
            fs::remove_file(&package_path)?;
            removed.push(package_path.display().to_string());
        }
    }

    if removed.is_empty() {
        Ok("No OpenCode plugin files found".to_string())
    } else {
        Ok(format!(
            "Removed OpenCode plugin files: {}",
            removed.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_dir_prefers_non_empty_directory_override() {
        assert_eq!(
            resolve_config_dir(
                Some("/config-dir".into()),
                Some("/xdg".into()),
                Some("/home/user".into()),
            ),
            Some(PathBuf::from("/config-dir"))
        );
    }

    #[test]
    fn config_dir_uses_xdg_then_home_defaults() {
        assert_eq!(
            resolve_config_dir(None, Some("/xdg".into()), Some("/home/user".into())),
            Some(PathBuf::from("/xdg/opencode"))
        );
        assert_eq!(
            resolve_config_dir(None, Some("".into()), Some("/home/user".into())),
            Some(PathBuf::from("/home/user/.config/opencode"))
        );
    }

    #[test]
    fn check_compares_installed_source_and_flags_legacy_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            check_at(tmp.path()).unwrap(),
            StatusCheck::NotInstalled
        ));

        let legacy = tmp.path().join("plugin/workmux-status.ts");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, PLUGIN_SOURCE).unwrap();
        assert!(matches!(
            check_at(tmp.path()).unwrap(),
            StatusCheck::UpdateAvailable
        ));

        let plugin = tmp.path().join("plugins/workmux-status.ts");
        fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        fs::write(&plugin, "// old plugin").unwrap();
        assert!(matches!(
            check_at(tmp.path()).unwrap(),
            StatusCheck::UpdateAvailable
        ));
        fs::write(&plugin, PLUGIN_SOURCE).unwrap();
        assert!(matches!(
            check_at(tmp.path()).unwrap(),
            StatusCheck::Installed
        ));
    }

    #[test]
    fn install_preserves_package_json_and_other_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("plugins")).unwrap();
        fs::write(tmp.path().join("plugins/custom.ts"), "// custom").unwrap();
        fs::create_dir_all(tmp.path().join("plugin")).unwrap();
        fs::write(
            tmp.path().join("plugin/workmux-status.ts"),
            "// legacy workmux plugin",
        )
        .unwrap();
        fs::write(tmp.path().join("plugin/custom.ts"), "// legacy custom").unwrap();
        let package = serde_json::to_string_pretty(&json!({
            "name": "custom-config",
            "scripts": { "check": "echo ok" },
            "dependencies": {
                "@opencode-ai/plugin": "9.0.0",
                "other-package": "2.0.0"
            }
        }))
        .unwrap();
        fs::write(tmp.path().join("package.json"), &package).unwrap();

        install_at(tmp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("package.json")).unwrap(),
            package
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("plugins/custom.ts")).unwrap(),
            "// custom"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("plugins/workmux-status.ts")).unwrap(),
            PLUGIN_SOURCE
        );
        assert!(!tmp.path().join("plugin/workmux-status.ts").exists());
        assert_eq!(
            fs::read_to_string(tmp.path().join("plugin/custom.ts")).unwrap(),
            "// legacy custom"
        );
    }

    #[test]
    fn install_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        install_at(tmp.path()).unwrap();
        let first_plugin = fs::read(tmp.path().join("plugins/workmux-status.ts")).unwrap();
        install_at(tmp.path()).unwrap();
        assert_eq!(
            fs::read(tmp.path().join("plugins/workmux-status.ts")).unwrap(),
            first_plugin
        );
        assert!(!tmp.path().join("package.json").exists());
    }

    #[test]
    fn uninstall_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let result = uninstall_at(tmp.path().to_path_buf()).unwrap();
        assert!(result.contains("No OpenCode plugin files found"));
    }

    #[test]
    fn uninstall_removes_plugin_and_exact_bundled_package() {
        let tmp = tempfile::tempdir().unwrap();
        install_at(tmp.path()).unwrap();
        fs::write(tmp.path().join("package.json"), PACKAGE_JSON).unwrap();

        let result = uninstall_at(tmp.path().to_path_buf()).unwrap();
        assert!(result.contains("Removed OpenCode plugin files"));
        assert!(!tmp.path().join("plugins/workmux-status.ts").exists());
        assert!(!tmp.path().join("package.json").exists());
    }

    #[test]
    fn uninstall_keeps_modified_package_json() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("plugins");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(plugin_dir.join("workmux-status.ts"), "// plugin code").unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"name": "custom"}"#).unwrap();

        uninstall_at(tmp.path().to_path_buf()).unwrap();
        assert!(tmp.path().join("package.json").exists());
    }

    #[test]
    fn uninstall_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            uninstall_at(tmp.path().to_path_buf())
                .unwrap()
                .contains("No OpenCode plugin files found")
        );
        assert!(
            uninstall_at(tmp.path().to_path_buf())
                .unwrap()
                .contains("No OpenCode plugin files found")
        );
    }
}
