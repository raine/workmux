use anyhow::{Result, bail};
use slug::slugify;

use crate::config::Config;

/// Derives the "handle" (worktree dir name + tmux window base name)
/// from the branch name, optional explicit override, and config.
///
/// The handle is always slugified to ensure filesystem/tmux compatibility.
///
/// Priority:
/// 1. Explicit name (--name flag) - bypasses all config (including prefix)
/// 2. Config-based derivation: worktree_naming strategy + worktree_prefix
/// 3. Branch name as-is (default fallback)
pub fn derive_handle(
    branch_name: &str,
    explicit_name: Option<&str>,
    config: &Config,
) -> Result<String> {
    let handle = if let Some(name) = explicit_name {
        derive_target_name(name)?
    } else {
        // Apply naming strategy
        let derived = config.worktree_naming.derive_name(branch_name);

        // Apply prefix if configured
        let with_prefix = if let Some(ref prefix) = config.worktree_prefix {
            format!("{}{}", prefix, derived)
        } else {
            derived
        };

        slugify(&with_prefix)
    };

    validate_handle(&handle)?;
    Ok(handle)
}

pub fn derive_target_name(name: &str) -> Result<String> {
    let handle = slugify(name);
    validate_handle(&handle)?;
    Ok(handle)
}

pub fn validate_parent_session(name: &str) -> Result<String> {
    if name.is_empty() {
        bail!("Parent session cannot be empty");
    }
    if name.contains(':') {
        bail!("Parent session cannot contain ':'");
    }
    if name.starts_with('$') || name.starts_with('=') {
        bail!("Parent session cannot start with '$' or '='");
    }
    if name.chars().any(char::is_control) {
        bail!("Parent session cannot contain control characters");
    }

    Ok(name.to_string())
}

/// Validates that a handle is safe for filesystem and tmux use.
fn validate_handle(handle: &str) -> Result<()> {
    if handle.is_empty() {
        bail!("Handle cannot be empty");
    }

    // Slugify should have removed these, but double check for safety
    if handle.contains("..") || handle.starts_with('/') {
        bail!("Handle cannot contain path traversal");
    }

    if handle.chars().any(char::is_whitespace) {
        bail!("Handle cannot contain whitespace");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorktreeNaming;

    fn default_config() -> Config {
        Config::default()
    }

    fn config_with_basename() -> Config {
        Config {
            worktree_naming: WorktreeNaming::Basename,
            ..Config::default()
        }
    }

    fn config_with_prefix(prefix: &str) -> Config {
        Config {
            worktree_prefix: Some(prefix.to_string()),
            ..Config::default()
        }
    }

    fn config_with_basename_and_prefix(prefix: &str) -> Config {
        Config {
            worktree_naming: WorktreeNaming::Basename,
            worktree_prefix: Some(prefix.to_string()),
            ..Config::default()
        }
    }

    // === Explicit name tests (bypass all config) ===

    #[test]
    fn derive_handle_explicit_name() {
        let result =
            derive_handle("prj-4120/feature", Some("cool-feature"), &default_config()).unwrap();
        assert_eq!(result, "cool-feature");
    }

    #[test]
    fn derive_handle_explicit_name_with_spaces() {
        let result = derive_handle("branch", Some("My Cool Feature"), &default_config()).unwrap();
        assert_eq!(result, "my-cool-feature");
    }

    #[test]
    fn derive_handle_explicit_name_with_special_chars() {
        let result = derive_handle("branch", Some("Feature! @#$%"), &default_config()).unwrap();
        assert_eq!(result, "feature");
    }

    #[test]
    fn derive_handle_explicit_name_bypasses_prefix() {
        let result = derive_handle("branch", Some("custom"), &config_with_prefix("web-")).unwrap();
        assert_eq!(result, "custom"); // NOT web-custom
    }

    #[test]
    fn derive_handle_explicit_name_bypasses_basename() {
        let result = derive_handle("prj/feature", Some("custom"), &config_with_basename()).unwrap();
        assert_eq!(result, "custom"); // NOT feature
    }

    // === Default (full) strategy tests ===

    #[test]
    fn derive_handle_branch_name_slugified() {
        let result = derive_handle("prj-4120/create-new-tags", None, &default_config()).unwrap();
        assert_eq!(result, "prj-4120-create-new-tags");
    }

    #[test]
    fn derive_handle_simple_branch() {
        let result = derive_handle("main", None, &default_config()).unwrap();
        assert_eq!(result, "main");
    }

    #[test]
    fn derive_handle_nested_branch() {
        let result = derive_handle("feature/auth/oauth", None, &default_config()).unwrap();
        assert_eq!(result, "feature-auth-oauth");
    }

    // === Basename strategy tests ===

    #[test]
    fn derive_handle_basename_extracts_last_segment() {
        let result = derive_handle("prj-4120/feature", None, &config_with_basename()).unwrap();
        assert_eq!(result, "feature");
    }

    #[test]
    fn derive_handle_basename_handles_trailing_slash() {
        let result = derive_handle("prj-4120/feature/", None, &config_with_basename()).unwrap();
        assert_eq!(result, "feature");
    }

    #[test]
    fn derive_handle_basename_simple_branch_unchanged() {
        let result = derive_handle("main", None, &config_with_basename()).unwrap();
        assert_eq!(result, "main");
    }

    #[test]
    fn derive_handle_basename_multiple_segments() {
        let result = derive_handle("prj/sub/feature", None, &config_with_basename()).unwrap();
        assert_eq!(result, "feature");
    }

    // === Prefix tests ===

    #[test]
    fn derive_handle_prefix_applied() {
        let result = derive_handle("feature", None, &config_with_prefix("web-")).unwrap();
        assert_eq!(result, "web-feature");
    }

    #[test]
    fn derive_handle_prefix_with_slash_branch() {
        let result = derive_handle("prj/feature", None, &config_with_prefix("api-")).unwrap();
        assert_eq!(result, "api-prj-feature");
    }

    // === Combined basename + prefix tests ===

    #[test]
    fn derive_handle_basename_and_prefix() {
        let result = derive_handle(
            "prj-4120/feature",
            None,
            &config_with_basename_and_prefix("web-"),
        )
        .unwrap();
        assert_eq!(result, "web-feature");
    }

    #[test]
    fn derive_handle_basename_and_prefix_simple_branch() {
        let result =
            derive_handle("feature", None, &config_with_basename_and_prefix("api-")).unwrap();
        assert_eq!(result, "api-feature");
    }

    // === Error cases ===

    #[test]
    fn derive_handle_empty_explicit_name_fails() {
        let result = derive_handle("branch", Some(""), &default_config());
        assert!(result.is_err());
    }

    #[test]
    fn validate_parent_session_preserves_valid_name() {
        let result = validate_parent_session("WalkingMate").unwrap();
        assert_eq!(result, "WalkingMate");
    }

    #[test]
    fn validate_parent_session_rejects_empty_name() {
        assert!(validate_parent_session("").is_err());
    }

    #[test]
    fn validate_parent_session_rejects_tmux_target_syntax() {
        for name in ["parent:window", "$1", "=parent"] {
            assert!(validate_parent_session(name).is_err(), "accepted {name:?}");
        }
    }

    #[test]
    fn validate_parent_session_rejects_control_characters() {
        assert!(validate_parent_session("parent\nsession").is_err());
    }

    #[test]
    fn validate_handle_empty_fails() {
        let result = validate_handle("");
        assert!(result.is_err());
    }

    #[test]
    fn validate_handle_valid() {
        let result = validate_handle("my-feature");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_handle_with_numbers() {
        let result = validate_handle("feature-123");
        assert!(result.is_ok());
    }

    // === WorktreeNaming::derive_name tests ===

    #[test]
    fn worktree_naming_full_preserves_branch() {
        assert_eq!(
            WorktreeNaming::Full.derive_name("prj/feature"),
            "prj/feature"
        );
    }

    #[test]
    fn worktree_naming_basename_extracts_last() {
        assert_eq!(
            WorktreeNaming::Basename.derive_name("prj/feature"),
            "feature"
        );
    }

    #[test]
    fn worktree_naming_basename_handles_trailing_slash() {
        assert_eq!(
            WorktreeNaming::Basename.derive_name("prj/feature/"),
            "feature"
        );
    }

    #[test]
    fn worktree_naming_basename_simple_branch() {
        assert_eq!(WorktreeNaming::Basename.derive_name("main"), "main");
    }
}
