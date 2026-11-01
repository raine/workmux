//! Shared extension file lifecycle helpers for agent setup.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use super::StatusCheck;

/// Check whether an extension file contains the bundled source.
pub fn check_installed(path: Option<&Path>, source: &str) -> Result<StatusCheck> {
    let Some(path) = path else {
        return Ok(StatusCheck::NotInstalled);
    };

    if !path.exists() {
        return Ok(StatusCheck::NotInstalled);
    }

    let installed = fs::read_to_string(path)?;
    if normalize_documentation_link(&installed) == normalize_documentation_link(source) {
        Ok(StatusCheck::Installed)
    } else {
        Ok(StatusCheck::UpdateAvailable)
    }
}

/// Canonicalize the documentation link in the leading extension comment.
/// Executable code and other comments remain significant for update detection.
pub(super) fn normalize_documentation_link(source: &str) -> String {
    let Some((header, rest)) = source.split_once("*/") else {
        return source.to_string();
    };
    if !header.starts_with("/**\n") {
        return source.to_string();
    }

    let header = header.replace(
        "\n * See: https://workmux.raine.dev/guide/status-tracking\n",
        "\n * See: https://workmux.dev/guide/status-tracking\n",
    );
    format!("{header}*/{rest}")
}

/// Create parent directories and write an extension source file.
pub fn install_extension_file(
    path: &Path,
    source: &str,
    mkdir_context: &str,
    write_context: &str,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context(mkdir_context.to_string())?;
    }

    fs::write(path, source).context(write_context.to_string())?;
    Ok(())
}

/// Remove an extension file and clean up an empty parent directory.
pub fn remove_extension_file(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }

    fs::remove_file(path)?;
    if let Some(parent) = path.parent()
        && parent.read_dir().is_ok_and(|mut it| it.next().is_none())
    {
        let _ = fs::remove_dir(parent);
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_installed_requires_bundled_source() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workmux-status.ts");
        fs::write(&path, "old extension").unwrap();

        assert!(matches!(
            check_installed(Some(&path), "current extension").unwrap(),
            StatusCheck::UpdateAvailable
        ));

        fs::write(&path, "current extension").unwrap();

        assert!(matches!(
            check_installed(Some(&path), "current extension").unwrap(),
            StatusCheck::Installed
        ));
    }

    #[test]
    fn documentation_domain_does_not_require_an_update() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workmux-status.ts");
        for source in [
            include_str!("../../resources/pi/extensions/workmux-status.ts"),
            include_str!("../../resources/omp/extensions/workmux-status.ts"),
        ] {
            let installed = source.replace("https://workmux.dev/", "https://workmux.raine.dev/");
            assert_ne!(installed, source);
            fs::write(&path, &installed).unwrap();

            assert!(matches!(
                check_installed(Some(&path), source).unwrap(),
                StatusCheck::Installed
            ));
            assert_eq!(normalize_documentation_link(&installed), source);
            assert_eq!(fs::read_to_string(&path).unwrap(), installed);

            let modified = format!("{installed}\n// custom behavior\n");
            fs::write(&path, &modified).unwrap();
            assert!(matches!(
                check_installed(Some(&path), source).unwrap(),
                StatusCheck::UpdateAvailable
            ));
            assert_eq!(
                normalize_documentation_link(&modified),
                format!("{source}\n// custom behavior\n")
            );
        }
    }

    #[test]
    fn documentation_normalization_preserves_other_changes() {
        let source = include_str!("../../resources/pi/extensions/workmux-status.ts");
        let other_page = source.replace("/guide/status-tracking", "/guide/skills");
        assert_eq!(normalize_documentation_link(&other_page), other_page);

        let code =
            format!("{source}\nconst url = \"https://workmux.raine.dev/guide/status-tracking\";\n");
        assert_eq!(normalize_documentation_link(&code), code);
        let comment = "// See: https://workmux.raine.dev/guide/status-tracking\n";
        assert_eq!(normalize_documentation_link(comment), comment);
    }

    #[test]
    fn check_installed_handles_missing_path() {
        assert!(matches!(
            check_installed(None, "current extension").unwrap(),
            StatusCheck::NotInstalled
        ));
    }
}
