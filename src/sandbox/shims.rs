//! Host-exec shim creation for sandbox guests (Lima VMs and containers).
//!
//! Creates a directory of symlinks that intercept configured command names
//! and route them to `workmux host-exec`.

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

/// Commands that are always available as shims, regardless of
/// user `host_commands` config. Includes both host-exec commands (e.g., `afplay`)
/// and clipboard shims (`wl-paste`, `xclip`).
pub const BUILTIN_HOST_COMMANDS: &[&str] = &["afplay", "wl-paste", "xclip"];

/// Clipboard shim scripts: these translate Linux clipboard tool CLIs
/// into `workmux clipboard-read` calls.
const CLIPBOARD_SHIMS: &[(&str, &str)] = &[
    (
        "wl-paste",
        r#"#!/bin/sh
mime=""
list_types=0
while [ $# -gt 0 ]; do
  case "$1" in
    -l|--list-types) list_types=1; shift ;;
    -t|--type) [ $# -ge 2 ] || exit 1; mime="$2"; shift 2 ;;
    -n|--no-newline) shift ;;
    *) shift ;;
  esac
done
if [ "$list_types" -eq 1 ]; then
  printf '%s\n' image/png
  exit 0
fi
[ -n "$mime" ] || exit 1
exec workmux clipboard-read "$mime"
"#,
    ),
    (
        "xclip",
        r#"#!/bin/sh
mime=""
output=0
while [ $# -gt 0 ]; do
  case "$1" in
    -o) output=1; shift ;;
    -selection) shift; shift ;;
    -t) [ $# -ge 2 ] || exit 1; mime="$2"; shift 2 ;;
    -i) echo "workmux: xclip write not supported in sandbox" >&2; exit 1 ;;
    *) shift ;;
  esac
done
[ "$output" -eq 1 ] || { echo "workmux: xclip write not supported in sandbox" >&2; exit 1; }
[ -n "$mime" ] || exit 1
exec workmux clipboard-read "$mime"
"#,
    ),
];

/// Check if a command name has a custom clipboard shim script.
fn clipboard_shim_script(cmd: &str) -> Option<&'static str> {
    CLIPBOARD_SHIMS
        .iter()
        .find(|(name, _)| *name == cmd)
        .map(|(_, script)| *script)
}

/// Check if a command name is a clipboard shim (uses ClipboardRead RPC, not Exec).
pub fn is_clipboard_shim(cmd: &str) -> bool {
    CLIPBOARD_SHIMS.iter().any(|(name, _)| *name == cmd)
}

/// Validate a command name for use in host-exec.
///
/// Rejects names that could cause security issues:
/// - empty or whitespace-only
/// - longer than 64 characters
/// - doesn't start with an ASCII alphanumeric character
/// - contains characters outside `[A-Za-z0-9._-]`
/// - is `.` or `..` (directory traversal)
/// - is `_shim` (reserved dispatcher name)
pub fn validate_command_name(cmd: &str) -> bool {
    if cmd.is_empty() || cmd.len() > 64 {
        return false;
    }
    if !cmd.as_bytes()[0].is_ascii_alphanumeric() {
        return false;
    }
    if !cmd
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return false;
    }
    if matches!(cmd, "." | ".." | "_shim") {
        return false;
    }
    true
}

/// Merge built-in host commands with user-configured ones, deduplicating.
pub fn effective_host_commands(user_commands: &[String]) -> Vec<String> {
    let mut commands: Vec<String> = BUILTIN_HOST_COMMANDS
        .iter()
        .map(|s| s.to_string())
        .collect();
    for cmd in user_commands {
        if !commands.iter().any(|c| c == cmd) {
            commands.push(cmd.clone());
        }
    }
    commands
}

/// Create a shim directory with a dispatcher script and command symlinks.
///
/// The directory is created under the VM's state dir (which is mounted
/// into the guest at ~/.workmux-state/). Returns the guest-visible path
/// to prepend to PATH.
///
/// Layout:
///   <state_dir>/shims/bin/_shim    (dispatcher script)
///   <state_dir>/shims/bin/just     -> _shim
///   <state_dir>/shims/bin/cargo    -> _shim
pub fn create_shim_directory(state_dir: &Path, commands: &[String]) -> Result<PathBuf> {
    let shim_bin = state_dir.join("shims/bin");
    fs::create_dir_all(&shim_bin)
        .with_context(|| format!("Failed to create shim dir: {}", shim_bin.display()))?;

    // Write dispatcher script
    let dispatcher = shim_bin.join("_shim");
    fs::write(
        &dispatcher,
        "#!/bin/sh\nexec workmux host-exec \"$(basename \"$0\")\" \"$@\"\n",
    )
    .context("Failed to write shim dispatcher")?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dispatcher, fs::Permissions::from_mode(0o755))?;
    }

    // Create shims for each command
    for cmd in commands {
        if !validate_command_name(cmd) {
            tracing::warn!(command = cmd, "skipping invalid host_command name");
            continue;
        }

        let link = shim_bin.join(cmd);
        // Atomic: create temp file/symlink and rename into place.
        // Safe under concurrent supervisors sharing the same VM.
        let tmp = shim_bin.join(format!(".{}.tmp", cmd));
        let _ = fs::remove_file(&tmp);

        if let Some(script) = clipboard_shim_script(cmd) {
            // Custom clipboard shim: write script file
            fs::write(&tmp, script)
                .with_context(|| format!("Failed to write clipboard shim for: {}", cmd))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
            }
        } else {
            // Standard host-exec symlink
            symlink("_shim", &tmp)
                .with_context(|| format!("Failed to create temp shim symlink for: {}", cmd))?;
        }

        fs::rename(&tmp, &link).with_context(|| format!("Failed to rename shim for: {}", cmd))?;
    }

    Ok(shim_bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_shim_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let commands = vec!["just".to_string(), "cargo".to_string(), "npm".to_string()];

        let shim_bin = create_shim_directory(tmp.path(), &commands).unwrap();

        // Dispatcher exists and is executable
        let dispatcher = shim_bin.join("_shim");
        assert!(dispatcher.exists());
        let content = std::fs::read_to_string(&dispatcher).unwrap();
        assert!(content.contains("workmux host-exec"));

        // Symlinks exist
        for cmd in &commands {
            let link = shim_bin.join(cmd);
            assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from("_shim"));
        }
    }

    #[test]
    fn test_create_shim_directory_skips_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let commands = vec!["valid".to_string(), "/bin/evil".to_string(), "".to_string()];

        let shim_bin = create_shim_directory(tmp.path(), &commands).unwrap();
        assert!(shim_bin.join("valid").exists());
        assert!(!shim_bin.join("bin/evil").exists());
    }

    #[test]
    fn test_effective_host_commands_includes_builtins() {
        let result = effective_host_commands(&[]);
        assert!(result.contains(&"afplay".to_string()));
        assert!(result.contains(&"wl-paste".to_string()));
        assert!(result.contains(&"xclip".to_string()));
    }

    #[test]
    fn test_effective_host_commands_merges_user() {
        let result = effective_host_commands(&["just".to_string(), "cargo".to_string()]);
        assert!(result.contains(&"afplay".to_string()));
        assert!(result.contains(&"wl-paste".to_string()));
        assert!(result.contains(&"xclip".to_string()));
        assert!(result.contains(&"just".to_string()));
        assert!(result.contains(&"cargo".to_string()));
    }

    #[test]
    fn test_effective_host_commands_deduplicates() {
        let result = effective_host_commands(&["afplay".to_string(), "just".to_string()]);
        let afplay_count = result.iter().filter(|c| *c == "afplay").count();
        assert_eq!(afplay_count, 1);
        assert!(result.contains(&"just".to_string()));
    }

    #[test]
    fn test_create_shim_directory_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let commands = vec!["just".to_string()];

        create_shim_directory(tmp.path(), &commands).unwrap();
        // Running again should not error
        create_shim_directory(tmp.path(), &commands).unwrap();

        assert!(tmp.path().join("shims/bin/just").exists());
    }

    #[test]
    fn test_validate_command_name_valid() {
        assert!(validate_command_name("just"));
        assert!(validate_command_name("cargo"));
        assert!(validate_command_name("npm"));
        assert!(validate_command_name("node-v20"));
        assert!(validate_command_name("my_tool"));
        assert!(validate_command_name("go1.21"));
        assert!(validate_command_name("afplay"));
    }

    #[test]
    fn test_validate_command_name_rejects_empty() {
        assert!(!validate_command_name(""));
    }

    #[test]
    fn test_validate_command_name_rejects_path_separators() {
        assert!(!validate_command_name("/bin/rm"));
        assert!(!validate_command_name("..\\cmd"));
        assert!(!validate_command_name("foo/bar"));
    }

    #[test]
    fn test_validate_command_name_rejects_shell_metacharacters() {
        assert!(!validate_command_name("cmd;rm -rf /"));
        assert!(!validate_command_name("cmd$(whoami)"));
        assert!(!validate_command_name("cmd`whoami`"));
        assert!(!validate_command_name("cmd&bg"));
        assert!(!validate_command_name("cmd|pipe"));
        assert!(!validate_command_name("cmd>out"));
        assert!(!validate_command_name("cmd<in"));
        assert!(!validate_command_name("cmd name"));
        assert!(!validate_command_name("cmd\ttab"));
        assert!(!validate_command_name("cmd\nnewline"));
    }

    #[test]
    fn test_validate_command_name_rejects_reserved() {
        assert!(!validate_command_name("."));
        assert!(!validate_command_name(".."));
        assert!(!validate_command_name("_shim"));
    }

    #[test]
    fn test_validate_command_name_rejects_leading_special() {
        assert!(!validate_command_name("-flag"));
        assert!(!validate_command_name("_underscore"));
        assert!(!validate_command_name(".dotfile"));
    }

    #[test]
    fn test_validate_command_name_rejects_too_long() {
        let long = "a".repeat(65);
        assert!(!validate_command_name(&long));
        let ok = "a".repeat(64);
        assert!(validate_command_name(&ok));
    }

    #[test]
    fn test_clipboard_shims_are_regular_files() {
        let tmp = tempfile::tempdir().unwrap();
        let commands = vec![
            "just".to_string(),
            "wl-paste".to_string(),
            "xclip".to_string(),
        ];

        let shim_bin = create_shim_directory(tmp.path(), &commands).unwrap();

        // wl-paste and xclip should be regular files, not symlinks
        for cmd in &["wl-paste", "xclip"] {
            let path = shim_bin.join(cmd);
            let meta = path.symlink_metadata().unwrap();
            assert!(
                meta.file_type().is_file(),
                "{} should be a regular file, not a symlink",
                cmd
            );
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(
                content.contains("workmux clipboard-read"),
                "{} shim should call workmux clipboard-read",
                cmd
            );
        }

        // just should still be a symlink
        let just_meta = shim_bin.join("just").symlink_metadata().unwrap();
        assert!(just_meta.file_type().is_symlink());
    }

    #[test]
    fn test_is_clipboard_shim() {
        assert!(is_clipboard_shim("wl-paste"));
        assert!(is_clipboard_shim("xclip"));
        assert!(!is_clipboard_shim("afplay"));
        assert!(!is_clipboard_shim("just"));
    }

    #[test]
    fn test_wl_paste_shim_content() {
        let script = clipboard_shim_script("wl-paste").unwrap();
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("-t|--type"));
        assert!(script.contains("--list-types"));
        assert!(script.contains("image/png"));
        assert!(script.contains("workmux clipboard-read"));
    }

    #[test]
    fn test_xclip_shim_content() {
        let script = clipboard_shim_script("xclip").unwrap();
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("-o) output=1"));
        assert!(script.contains("xclip write not supported"));
        assert!(script.contains("workmux clipboard-read"));
    }
}
