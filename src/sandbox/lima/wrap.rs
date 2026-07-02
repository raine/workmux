//! Command wrapping for Lima backend.

use anyhow::Result;
use std::path::Path;

use crate::config::Config;

use crate::shell::shell_escape;

/// Wrap a command to run inside a Lima VM via the sandbox supervisor.
///
/// Generates a `workmux sandbox run` command that manages the VM lifecycle,
/// starts an RPC server, and executes the agent command inside the VM.
///
/// The supervisor handles:
/// - Ensuring the VM is running
/// - Starting the TCP RPC server
/// - Passing sandbox env vars (WM_SANDBOX_GUEST, WM_RPC_HOST, WM_RPC_PORT, WM_RPC_TOKEN)
/// - Setting the working directory via `limactl shell --workdir`
/// - Running the command via `limactl shell`
///
/// # Arguments
/// * `command` - The command string to run (may contain shell operators)
/// * `_config` - The workmux configuration (env passthrough handled by supervisor)
/// * `_vm_name` - The Lima VM instance name (supervisor resolves this itself)
/// * `working_dir` - Working directory inside the VM
pub fn wrap_for_lima(
    command: &str,
    _config: &Config,
    _vm_name: &str,
    working_dir: &Path,
) -> Result<String> {
    // Strip the leading history-prevention space before passing the command to
    // the sandbox supervisor.
    let command = command.strip_prefix(' ').unwrap_or(command);
    // Pass the command as a single quoted argument. The sandbox supervisor
    // (sandbox_run.rs) handles wrapping it in `sh -lc '...'` for limactl,
    // which is necessary because limactl/SSH flattens separate args.
    // Prefix with space to prevent shell history entry.
    Ok(format!(
        " workmux sandbox run '{}' -- '{}'",
        shell_escape(&working_dir.to_string_lossy()),
        shell_escape(command)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::lima::LimaInstanceInfo;

    #[test]
    fn test_check_vm_state_running() {
        // LimaInstanceInfo correctly categorizes states
        let info = LimaInstanceInfo {
            name: "test-vm".to_string(),
            status: "Running".to_string(),
            dir: None,
        };
        assert!(info.is_running());
    }

    #[test]
    fn test_check_vm_state_stopped() {
        let info = LimaInstanceInfo {
            name: "test-vm".to_string(),
            status: "Stopped".to_string(),
            dir: None,
        };
        assert!(!info.is_running());
    }

    #[test]
    fn test_wrap_generates_supervisor_command() {
        let config = Config::default();
        let result = wrap_for_lima(
            "claude",
            &config,
            "wm-abc12345",
            Path::new("/Users/test/project"),
        )
        .unwrap();

        assert!(result.starts_with(" workmux sandbox run"));
        assert!(result.contains("/Users/test/project"));
        // Command is passed as a single quoted arg (no sh -lc at this level)
        assert!(result.contains("-- 'claude'"));
    }

    #[test]
    fn test_wrap_strips_leading_space() {
        let config = Config::default();
        // Agent commands can start with a history-prevention space.
        let result = wrap_for_lima(
            " claude -- \"$(cat PROMPT.md)\"",
            &config,
            "wm-abc12345",
            Path::new("/tmp/wt"),
        )
        .unwrap();

        // Leading space should be stripped
        assert!(result.contains("-- 'claude -- \"$(cat PROMPT.md)\"'"));
    }

    #[test]
    fn test_wrap_with_spaces_in_path() {
        let config = Config::default();
        let result = wrap_for_lima(
            "claude",
            &config,
            "wm-abc12345",
            Path::new("/Users/test user/my project"),
        )
        .unwrap();

        assert!(result.contains("test user/my project"));
        // Path should be in single quotes
        assert!(result.contains("'/Users/test user/my project'"));
    }

    #[test]
    fn test_wrap_with_complex_command() {
        let config = Config::default();
        let result = wrap_for_lima(
            "claude --dangerously-skip-permissions -- \"$(cat .workmux/prompts/PROMPT.md)\"",
            &config,
            "wm-abc",
            Path::new("/tmp/wt"),
        )
        .unwrap();

        // Command passed as single quoted arg
        assert!(!result.contains("sh -lc"));
        assert!(result.contains("claude"));
        assert!(result.contains("dangerously-skip-permissions"));
    }

    #[test]
    fn test_wrap_escapes_single_quotes_in_command() {
        let config = Config::default();
        let result = wrap_for_lima(
            "echo 'hello world'",
            &config,
            "wm-abc",
            Path::new("/tmp/wt"),
        )
        .unwrap();

        // Single quotes in the command should be escaped
        assert!(result.contains("echo '\\''hello world'\\''"));
    }

    #[test]
    fn test_env_passthrough_escaping() {
        // Verify env var values with special characters are properly escaped
        let env_var = "MY_VAR";
        let val = "hello'world";
        let flag = format!(" --setenv {}='{}'", env_var, shell_escape(val));
        assert_eq!(flag, " --setenv MY_VAR='hello'\\''world'");
    }
}
