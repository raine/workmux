//! Shared JSON config read, merge, and write helpers for hook installation.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;

use crate::agent_setup::hooks;

/// Root value seeded when a hook config file does not exist yet.
#[derive(Clone, Copy)]
pub enum EmptyJsonRoot {
    Object,
    HooksObject,
}

/// Context strings for JSON hook file installation.
pub struct JsonHookInstallSpec<'a> {
    pub read_context: &'a str,
    pub parse_context: &'a str,
    pub write_context: &'a str,
    pub mkdir_context: &'a str,
    pub empty_root: EmptyJsonRoot,
}

/// Caller-facing messages for JSON hook file uninstall.
pub struct JsonHookUninstallMessages<'a> {
    pub file_missing: &'a str,
    pub not_found: &'a str,
    pub soft_read_error: Option<&'a str>,
    pub soft_parse_error: Option<&'a str>,
}

/// Uninstall behavior for JSON hook files.
pub struct JsonHookUninstallSpec<'a> {
    pub messages: JsonHookUninstallMessages<'a>,
    pub delete_if_no_hooks_remain: bool,
    pub remove_plugins: bool,
    pub soft_errors: bool,
}

/// Extract the hooks object from embedded JSON config source.
pub fn hooks_from_embedded(source: &str, missing_hooks_message: &str) -> Result<Value> {
    let config: Value = serde_json::from_str(source).expect("embedded config is valid JSON");
    config
        .get("hooks")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{missing_hooks_message}"))
}

fn empty_root_value(empty_root: EmptyJsonRoot) -> Value {
    match empty_root {
        EmptyJsonRoot::Object => Value::Object(serde_json::Map::new()),
        EmptyJsonRoot::HooksObject => serde_json::json!({ "hooks": {} }),
    }
}

/// Read or seed a JSON config file, merge hooks, and write pretty JSON.
pub fn json_hook_install(
    path: &Path,
    hooks_to_add: &Value,
    spec: &JsonHookInstallSpec<'_>,
) -> Result<()> {
    json_hook_install_with(path, hooks_to_add, spec, hooks::merge_hook_groups)
}

/// Install JSON hooks using the caller's merge semantics.
pub fn json_hook_install_with(
    path: &Path,
    hooks_to_add: &Value,
    spec: &JsonHookInstallSpec<'_>,
    merge: fn(&mut Value, &Value) -> Result<()>,
) -> Result<()> {
    let mut config: Value = if path.exists() {
        let content = fs::read_to_string(path).context(spec.read_context.to_string())?;
        serde_json::from_str(&content).context(spec.parse_context.to_string())?
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context(spec.mkdir_context.to_string())?;
        }
        empty_root_value(spec.empty_root)
    };

    merge(&mut config, hooks_to_add)?;

    let output = serde_json::to_string_pretty(&config)?;
    fs::write(path, output + "\n").context(spec.write_context.to_string())?;

    Ok(())
}

fn write_pretty(path: &Path, config: &Value) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(config)? + "\n")?;
    Ok(())
}

/// Remove workmux hooks from a JSON config file.
pub fn json_hook_uninstall(path: &Path, spec: &JsonHookUninstallSpec<'_>) -> Result<String> {
    if !path.exists() {
        return Ok(spec.messages.file_missing.to_string());
    }

    if spec.soft_errors {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => {
                return Ok(spec
                    .messages
                    .soft_read_error
                    .unwrap_or("Could not read config file")
                    .to_string());
            }
        };

        let mut settings = match serde_json::from_str::<Value>(&content) {
            Ok(settings) => settings,
            Err(_) => {
                return Ok(spec
                    .messages
                    .soft_parse_error
                    .unwrap_or("Could not parse config file")
                    .to_string());
            }
        };

        return finalize_uninstall(path, &mut settings, spec);
    }

    let content = fs::read_to_string(path)?;
    let mut settings: Value = serde_json::from_str(&content)?;

    finalize_uninstall(path, &mut settings, spec)
}

fn finalize_uninstall(
    path: &Path,
    settings: &mut Value,
    spec: &JsonHookUninstallSpec<'_>,
) -> Result<String> {
    let removed = hooks::remove_workmux_hooks(settings);
    let plugins_removed = if spec.remove_plugins {
        hooks::remove_workmux_plugins(settings)
    } else {
        false
    };
    hooks::remove_empty_hooks_wrapper(settings);

    if removed || plugins_removed {
        if spec.delete_if_no_hooks_remain && settings.get("hooks").is_none() {
            fs::remove_file(path)?;
            Ok(format!("Removed {} (no hooks remain)", path.display()))
        } else {
            write_pretty(path, settings)?;
            Ok(format!("Removed workmux hooks from {}", path.display()))
        }
    } else {
        Ok(spec.messages.not_found.to_string())
    }
}
