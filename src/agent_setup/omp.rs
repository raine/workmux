//! OMP agent status tracking setup.
//!
//! Detects OMP via its agent directory at `~/.omp/agent/` and writes
//! `workmux-status.ts` to that agent directory.
//!
//! Installs extension by writing `workmux-status.ts` to the extensions directory.

use anyhow::Result;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{StatusCheck, UpdatePreview};
use crate::agent_setup::extension_file;

/// The OMP extension source, embedded at compile time.
const EXTENSION_SOURCE: &str = include_str!("../../resources/omp/extensions/workmux-status.ts");

fn omp_agent_dir() -> Option<PathBuf> {
    let home = home::home_dir()?;
    Some(omp_agent_dir_with_env(&home, |key| std::env::var_os(key)))
}

pub(crate) fn omp_agent_dir_with_env(
    home: &Path,
    get_env: impl Fn(&str) -> Option<OsString>,
) -> PathBuf {
    if let Some(dir) = get_env("PI_CODING_AGENT_DIR") {
        return PathBuf::from(dir);
    }

    let config_dir = get_env("PI_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".omp"));

    if config_dir.is_absolute() {
        config_dir.join("agent")
    } else {
        home.join(config_dir).join("agent")
    }
}

fn extension_path() -> Option<PathBuf> {
    omp_agent_dir().map(|d| d.join("extensions/workmux-status.ts"))
}

/// Detect if OMP is present via filesystem.
/// Returns the reason string if detected, None otherwise.
pub fn detect() -> Option<&'static str> {
    if omp_agent_dir().is_some_and(|d| d.is_dir()) {
        return Some("found ~/.omp/agent/");
    }
    None
}

/// Check if workmux extension is installed for OMP.
pub fn check() -> Result<StatusCheck> {
    extension_file::check_installed(extension_path().as_deref(), EXTENSION_SOURCE)
}

pub(crate) fn update_preview() -> Result<Option<UpdatePreview>> {
    let Some(path) = extension_path().filter(|path| path.exists()) else {
        return Ok(None);
    };
    Ok(Some(UpdatePreview {
        label: path.display().to_string(),
        installed: std::fs::read_to_string(&path)?,
        bundled: EXTENSION_SOURCE.to_string(),
    }))
}

/// Install workmux extension for OMP.
/// Returns a description of what was done.
pub fn install() -> Result<String> {
    let path =
        extension_path().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    extension_file::install_extension_file(
        &path,
        EXTENSION_SOURCE,
        "Failed to create omp extensions directory",
        "Failed to write omp extension",
    )?;

    Ok(format!(
        "Installed extension to {}. Restart omp for it to take effect.",
        path.display()
    ))
}

/// Remove workmux extension for OMP agent.
///
/// Deletes the extension file and cleans up empty parent directories.
pub fn uninstall() -> Result<String> {
    let Some(path) = extension_path() else {
        return Ok("omp config dir not found, nothing to uninstall".to_string());
    };
    uninstall_at(path)
}

fn uninstall_at(path: PathBuf) -> Result<String> {
    if extension_file::remove_extension_file(&path)? {
        Ok(format!("Removed omp extension at {}", path.display()))
    } else {
        Ok("No omp extension found".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_tracks_waiting_status() {
        assert!(EXTENSION_SOURCE.contains("lastStatus"));
        assert!(EXTENSION_SOURCE.contains("statusQueue"));
        assert!(EXTENSION_SOURCE.contains("status === lastStatus"));
        assert!(EXTENSION_SOURCE.contains("pi.on(\"message_end\""));
        assert!(EXTENSION_SOURCE.contains("\"role\" in event.message"));
        assert!(EXTENSION_SOURCE.contains("event.message.role === \"assistant\""));
        assert!(EXTENSION_SOURCE.contains("pi.on(\"tool_call\""));
        assert!(EXTENSION_SOURCE.contains("event.toolName === \"ask\""));
        assert!(EXTENSION_SOURCE.contains("setStatus(\"waiting\")"));
    }

    #[test]
    fn test_extension_registers_agent_before_agent_events() {
        let registration = EXTENSION_SOURCE
            .find("pi.on(\"session_start\"")
            .expect("session_start handler");
        let agent_start = EXTENSION_SOURCE
            .find("pi.on(\"agent_start\"")
            .expect("agent_start handler");

        assert!(registration < agent_start);
        assert!(
            EXTENSION_SOURCE
                .contains("await pi.exec(\"workmux\", [\"register-agent\"]).catch(() => {});")
        );
    }

    #[test]
    fn test_extension_without_registration_needs_update() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("workmux-status.ts");
        let previous_source = EXTENSION_SOURCE
            .replace(
                "  pi.on(\"session_start\", async () => {\n    await pi.exec(\"workmux\", [\"register-agent\"]).catch(() => {});\n  });\n\n",
                "",
            );
        std::fs::write(&path, previous_source).unwrap();

        assert!(matches!(
            extension_file::check_installed(Some(&path), EXTENSION_SOURCE).unwrap(),
            StatusCheck::UpdateAvailable
        ));
    }

    #[test]
    fn test_omp_agent_dir_default() {
        let dir = omp_agent_dir_with_env(Path::new("/home/test"), |_| None);

        assert_eq!(dir, PathBuf::from("/home/test/.omp/agent"));
    }

    #[test]
    fn test_omp_agent_dir_respects_pi_config_dir() {
        let dir = omp_agent_dir_with_env(Path::new("/home/test"), |key| {
            (key == "PI_CONFIG_DIR").then(|| OsString::from("custom-omp"))
        });

        assert_eq!(dir, PathBuf::from("/home/test/custom-omp/agent"));
    }

    #[test]
    fn test_omp_agent_dir_respects_pi_coding_agent_dir() {
        let dir = omp_agent_dir_with_env(Path::new("/home/test"), |key| {
            (key == "PI_CODING_AGENT_DIR").then(|| OsString::from("/tmp/omp-agent"))
        });

        assert_eq!(dir, PathBuf::from("/tmp/omp-agent"));
    }

    #[test]
    fn test_uninstall_no_omp_extension_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_path = tmp.path().join("extensions/workmux-status.ts");
        let result = uninstall_at(ext_path).unwrap();
        assert!(result.contains("No omp extension found"));
    }

    #[test]
    fn test_uninstall_removes_omp_extension_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_dir = tmp.path().join("extensions");
        std::fs::create_dir_all(&ext_dir).unwrap();
        let ext_path = ext_dir.join("workmux-status.ts");
        std::fs::write(&ext_path, "// extension").unwrap();

        let result = uninstall_at(ext_path.clone()).unwrap();
        assert!(result.contains("Removed omp extension"));
        assert!(!ext_path.exists());
    }

    #[test]
    fn test_uninstall_omp_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_path = tmp.path().join("extensions/workmux-status.ts");
        let result1 = uninstall_at(ext_path.clone()).unwrap();
        assert!(result1.contains("No omp extension found"));
        let result2 = uninstall_at(ext_path).unwrap();
        assert!(result2.contains("No omp extension found"));
    }
}
