//! Pi agent status tracking setup.
//!
//! Detects pi via its config directory at `~/.pi/agent/`.
//! Override with `PI_CODING_AGENT_DIR` env var.
//!
//! Installs extension by writing `workmux-status.ts` to the extensions directory.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::{StatusCheck, UpdatePreview};
use crate::agent_setup::extension_file;

/// The pi extension source, embedded at compile time.
const EXTENSION_SOURCE: &str = include_str!("../../resources/pi/extensions/workmux-status.ts");

fn pi_agent_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR") {
        return Some(PathBuf::from(dir));
    }
    home::home_dir().map(|h| h.join(".pi/agent"))
}

pub(crate) fn extension_path() -> Option<PathBuf> {
    pi_agent_dir().map(|d| extension_path_in(&d))
}

fn extension_path_in(agent_dir: &Path) -> PathBuf {
    agent_dir.join("extensions/workmux-status.ts")
}

/// Detect if pi is present via filesystem.
/// Returns the reason string if detected, None otherwise.
pub fn detect() -> Option<&'static str> {
    if std::env::var("PI_CODING_AGENT_DIR").is_ok_and(|d| PathBuf::from(d).is_dir()) {
        return Some("found $PI_CODING_AGENT_DIR");
    }
    if pi_agent_dir().is_some_and(|d| d.is_dir()) {
        return Some("found ~/.pi/agent/");
    }
    None
}

/// Check if workmux extension is installed for pi.
pub fn check() -> Result<StatusCheck> {
    extension_file::check_installed(extension_path().as_deref(), EXTENSION_SOURCE)
}

/// [`check`] for the pi agent directory `dir`.
pub(crate) fn check_dir(dir: &Path) -> Result<StatusCheck> {
    extension_file::check_installed(Some(&extension_path_in(dir)), EXTENSION_SOURCE)
}

pub(crate) fn update_preview() -> Result<Option<UpdatePreview>> {
    let Some(path) = extension_path().filter(|path| path.exists()) else {
        return Ok(None);
    };
    Ok(Some(UpdatePreview {
        label: path.display().to_string(),
        installed: extension_file::normalize_documentation_link(&std::fs::read_to_string(&path)?),
        bundled: extension_file::normalize_documentation_link(EXTENSION_SOURCE),
    }))
}

/// Install workmux extension for pi.
/// Returns a description of what was done.
pub fn install() -> Result<String> {
    let dir =
        pi_agent_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    install_dir(&dir)
}

/// [`install`] into the pi agent directory `dir`.
pub(crate) fn install_dir(dir: &Path) -> Result<String> {
    let path = extension_path_in(dir);
    extension_file::install_extension_file(
        &path,
        EXTENSION_SOURCE,
        "Failed to create pi extensions directory",
        "Failed to write pi extension",
    )?;

    Ok(format!(
        "Installed extension to {}. Restart pi for it to take effect.",
        path.display()
    ))
}

/// Remove workmux extension for pi agent.
///
/// Deletes the extension file and cleans up empty parent directories.
pub fn uninstall() -> Result<String> {
    let Some(path) = extension_path() else {
        return Ok("pi config dir not found, nothing to uninstall".to_string());
    };
    uninstall_at(path)
}

fn uninstall_at(path: PathBuf) -> Result<String> {
    if extension_file::remove_extension_file(&path)? {
        Ok(format!("Removed pi extension at {}", path.display()))
    } else {
        Ok("No pi extension found".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uninstall_no_extension_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_path = tmp.path().join("extensions/workmux-status.ts");
        let result = uninstall_at(ext_path).unwrap();
        assert!(result.contains("No pi extension found"));
    }

    #[test]
    fn test_uninstall_removes_extension_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_dir = tmp.path().join("extensions");
        std::fs::create_dir_all(&ext_dir).unwrap();
        let ext_path = ext_dir.join("workmux-status.ts");
        std::fs::write(&ext_path, "// extension").unwrap();

        let result = uninstall_at(ext_path.clone()).unwrap();
        assert!(result.contains("Removed pi extension"));
        assert!(!ext_path.exists());
    }

    #[test]
    fn test_uninstall_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_path = tmp.path().join("extensions/workmux-status.ts");
        let result1 = uninstall_at(ext_path.clone()).unwrap();
        assert!(result1.contains("No pi extension found"));
        let result2 = uninstall_at(ext_path).unwrap();
        assert!(result2.contains("No pi extension found"));
    }
}
