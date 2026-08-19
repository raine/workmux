//! Backend-agnostic utility functions for multiplexer operations.
//!
//! These helpers are shared between tmux, WezTerm, and any future backends.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::agent::SelectedAgent;
use crate::config::Config;

use crate::cmd::Cmd;

use super::PaneHandshake;
use super::handshake::UnixPipeHandshake;
use super::types::LivePaneInfo;

/// Helper function to add prefix to window name.
///
/// Used by all backends to construct full window names from prefix and base name.
pub fn prefixed(prefix: &str, window_name: &str) -> String {
    format!("{}{}", prefix, window_name)
}

/// Check if a shell is POSIX-compatible (supports `$(...)` syntax).
///
/// Used to determine whether agent commands need to be wrapped in `sh -c '...'`
/// for shells like nushell or fish that don't support POSIX command substitution.
pub fn is_posix_shell(shell: &str) -> bool {
    let shell_name = Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("sh");
    matches!(shell_name, "bash" | "zsh" | "sh" | "dash" | "ksh" | "ash")
}

/// Return the last `lines` lines from terminal output.
pub fn tail_lines(output: &str, lines: u16) -> String {
    let all_lines: Vec<&str> = output.lines().collect();
    let start = all_lines.len().saturating_sub(lines as usize);
    all_lines[start..].join("\n")
}

/// Build a `LivePaneInfo` from shared pane fields.
pub fn build_live_pane_info(
    pid: Option<u32>,
    current_command: Option<String>,
    working_dir: PathBuf,
    title: &str,
    session: String,
    window: String,
) -> LivePaneInfo {
    LivePaneInfo {
        pid,
        current_command,
        working_dir,
        title: if title.is_empty() {
            None
        } else {
            Some(title.to_string())
        },
        session: Some(session),
        window: Some(window),
        session_id: None,
        window_id: None,
    }
}

/// Snapshot of live pane fields before conversion to `LivePaneInfo`.
pub struct LivePaneSnapshot {
    pub pane_id: String,
    pub pid: Option<u32>,
    pub current_command: Option<String>,
    pub working_dir: PathBuf,
    pub title: String,
    pub session: String,
    pub window: String,
}

impl LivePaneSnapshot {
    pub fn into_pair(self) -> (String, LivePaneInfo) {
        let pane_id = self.pane_id;
        let info = build_live_pane_info(
            self.pid,
            self.current_command,
            self.working_dir,
            &self.title,
            self.session,
            self.window,
        );
        (pane_id, info)
    }
}

/// Build a pane ID map from live pane snapshots.
pub fn live_pane_map<I>(snapshots: I) -> HashMap<String, LivePaneInfo>
where
    I: IntoIterator<Item = LivePaneSnapshot>,
{
    snapshots
        .into_iter()
        .map(LivePaneSnapshot::into_pair)
        .collect()
}

/// Resolve the default shell from `$SHELL`, falling back when unset.
pub fn default_shell(fallback: &str) -> Result<String> {
    std::env::var("SHELL").or_else(|_| Ok(fallback.to_string()))
}

/// Create a Unix pipe handshake for shell startup synchronization.
pub fn unix_pipe_handshake() -> Result<Box<dyn PaneHandshake>> {
    Ok(Box::new(UnixPipeHandshake::new()?))
}

/// Run a shell script detached via `nohup sh -c`.
pub fn run_detached_sh_c(script: &str) -> Result<()> {
    let bg_script = format!("nohup sh -c '{}' >/dev/null 2>&1 &", script);
    Cmd::new("sh").args(&["-c", &bg_script]).run().map(|_| ())
}

/// Resolve a pane's command: handle `<agent>` placeholder, auto-detect known
/// agents, and adjust for prompt injection.
///
/// Returns the final command to send to the pane, or None if no command should be sent.
/// This consolidates the duplicated command resolution logic from both backends' setup_panes.
/// Result of resolving a pane command.
pub struct ResolvedCommand {
    /// The command string to send to the pane.
    pub command: String,
    /// Whether the command was rewritten to inject a prompt (needs auto-status).
    pub prompt_injected: bool,
    /// Selected agent metadata for agent panes.
    pub selected_agent: Option<SelectedAgent>,
    /// Command before prompt injection, so `render_command` stays idempotent.
    base_command: String,
    prompt_argument: Option<String>,
    posix_shell: bool,
    use_agent_command: bool,
    apply_agent_prefix: bool,
}

impl ResolvedCommand {
    pub fn render_command(&self) -> String {
        let mut command = if self.use_agent_command {
            self.renderable_command()
        } else {
            self.render_raw_command()
        };
        if let Some(prompt_argument) = &self.prompt_argument {
            command.push(' ');
            command.push_str(prompt_argument);
            command = if self.posix_shell {
                command
            } else {
                wrap_for_non_posix_shell(&command)
            };
            command.insert(0, ' ');
        }
        command
    }

    pub fn renderable_command(&self) -> String {
        self.selected_agent
            .as_ref()
            .map(SelectedAgent::shell_command)
            .unwrap_or_else(|| self.base_command.clone())
    }

    fn render_raw_command(&self) -> String {
        let Some(agent) = &self.selected_agent else {
            return self.base_command.clone();
        };
        if !self.apply_agent_prefix {
            return self.base_command.clone();
        }
        let mut prefix = agent.command.shell_string();
        if prefix == self.base_command || !self.base_command.starts_with(&agent.command.program) {
            return self.base_command.clone();
        }
        prefix.push_str(&self.base_command[agent.command.program.len()..]);
        prefix
    }
}

pub fn resolve_pane_command_with_config(
    pane_command: Option<&str>,
    run_commands: bool,
    prompt_file_path: Option<&Path>,
    working_dir: &Path,
    config: &Config,
    task_agent: Option<&str>,
    shell: &str,
) -> Option<ResolvedCommand> {
    let raw_command = pane_command?;
    if !run_commands {
        return None;
    }

    let default_agent = super::agent::resolve_selected_agent(config, task_agent);
    let mut selected_agent = None;
    let mut use_agent_command = false;
    let mut apply_agent_prefix = false;
    let command = if raw_command == "<agent>" {
        let agent = default_agent?;
        let command = agent.shell_command();
        selected_agent = Some(agent);
        use_agent_command = true;
        command
    } else if let Some((selector, extra_args)) = parse_agent_placeholder_with_args(raw_command) {
        let mut agent = match selector {
            Some(name) => super::agent::resolve_selected_agent(config, Some(name))?,
            None => default_agent?,
        };
        agent.command.append_args_fragment(extra_args);
        let command = agent.shell_command();
        selected_agent = Some(agent);
        use_agent_command = true;
        command
    } else if let Some(name) = raw_command
        .strip_prefix("<agent:")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        let agent = super::agent::resolve_selected_agent(config, Some(name))?;
        let command = agent.shell_command();
        selected_agent = Some(agent);
        use_agent_command = true;
        command
    } else if default_agent.as_ref().is_some_and(|agent| {
        crate::config::is_agent_command(raw_command, &agent.shell_command())
            || crate::config::is_agent_command(raw_command, agent.kind())
    }) {
        if let Some(agent) = default_agent.as_ref()
            && raw_command.starts_with(&agent.command.program)
            && (!agent.command.env.is_empty()
                || !agent.command.env_args.is_empty()
                || !agent.command.env_assignments.is_empty())
        {
            apply_agent_prefix = true;
        }
        selected_agent = default_agent;
        raw_command.to_string()
    } else if super::agent::is_known_agent(raw_command) {
        selected_agent = super::agent::SelectedAgent::from_raw(raw_command);
        raw_command.to_string()
    } else {
        raw_command.to_string()
    };

    let prompt_argument = selected_agent.as_ref().and_then(|agent| {
        prompt_file_path.map(|prompt_path| {
            let relative = prompt_path.strip_prefix(working_dir).unwrap_or(prompt_path);
            agent.profile.prompt_argument(&relative.to_string_lossy())
        })
    });
    let prompt_injected = prompt_argument.is_some();
    if let Some(agent) = selected_agent.as_mut()
        && let Some(subcmd) = agent.profile.default_subcommand()
    {
        agent.command.insert_default_subcommand(subcmd);
    }

    let mut resolved = ResolvedCommand {
        command: command.clone(),
        base_command: command,
        prompt_injected,
        selected_agent,
        prompt_argument,
        posix_shell: is_posix_shell(shell),
        use_agent_command,
        apply_agent_prefix,
    };
    resolved.command = resolved.render_command();
    Some(resolved)
}

fn parse_agent_placeholder_with_args(raw_command: &str) -> Option<(Option<&str>, &str)> {
    let rest = raw_command.strip_prefix("<agent")?;
    let (selector, rest) = if let Some(rest) = rest.strip_prefix(':') {
        let (name, rest) = rest.split_once('>')?;
        (Some(name), rest)
    } else {
        let rest = rest.strip_prefix('>')?;
        (None, rest)
    };
    let extra_args = rest.trim_start();
    if extra_args.is_empty() {
        return None;
    }
    Some((selector, extra_args))
}

/// Escape a string for embedding inside a double-quoted shell context.
///
/// Escapes: backslash, double quote, dollar sign, backtick.
/// Does NOT add surrounding quotes - caller controls the quoting.
///
/// Example: `$HOME` -> `\$HOME`
pub fn escape_for_double_quotes(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

/// Escape a command to be safely embedded inside `sh -c "..."`.
///
/// This handles the two-step nesting complexity:
/// 1. Inner single-quoted context (for paths/args inside the command)
/// 2. Outer double-quoted context (for the sh -c wrapper)
///
/// Use when you need to pass a value that will be single-quoted inside
/// a double-quoted sh -c command.
///
/// Example: `/bin/user's shell` inside `sh -c "exec '/bin/user's shell'"`:
/// - Step 1: `'\''` escaping -> `/bin/user'\''s shell`
/// - Step 2: double-quote escaping -> `/bin/user'\''s shell` (no change here)
pub fn escape_for_sh_c_inner_single_quote(s: &str) -> String {
    let single_escaped = s.replace('\'', "'\\''");
    escape_for_double_quotes(&single_escaped)
}

/// Wrap a command in `sh -c '...'` for execution in non-POSIX shells.
///
/// Used when the default shell (nushell, fish, etc.) doesn't support
/// POSIX command substitution like `$(...)`.
pub fn wrap_for_non_posix_shell(command: &str) -> String {
    let escaped = command.replace('\'', "'\\''");
    format!("sh -c '{}'", escaped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // --- prefixed tests ---

    #[test]
    fn test_tail_lines() {
        assert_eq!(tail_lines("one\ntwo\nthree", 2), "two\nthree");
        assert_eq!(tail_lines("one\ntwo\nthree", 10), "one\ntwo\nthree");
        assert_eq!(tail_lines("", 5), "");
    }

    #[test]
    fn test_prefixed() {
        assert_eq!(prefixed("wm-", "feature"), "wm-feature");
        assert_eq!(prefixed("", "feature"), "feature");
        assert_eq!(prefixed("prefix-", ""), "prefix-");
    }

    // --- is_posix_shell tests ---

    #[test]
    fn test_is_posix_shell_bash() {
        assert!(is_posix_shell("/bin/bash"));
        assert!(is_posix_shell("/usr/bin/bash"));
    }

    #[test]
    fn test_is_posix_shell_zsh() {
        assert!(is_posix_shell("/bin/zsh"));
        assert!(is_posix_shell("/usr/local/bin/zsh"));
    }

    #[test]
    fn test_is_posix_shell_sh() {
        assert!(is_posix_shell("/bin/sh"));
    }

    #[test]
    fn test_is_posix_shell_nushell() {
        assert!(!is_posix_shell("/opt/homebrew/bin/nu"));
        assert!(!is_posix_shell("/usr/bin/nu"));
    }

    #[test]
    fn test_is_posix_shell_fish() {
        assert!(!is_posix_shell("/usr/bin/fish"));
        assert!(!is_posix_shell("/opt/homebrew/bin/fish"));
    }

    // --- escape_for_double_quotes tests ---

    #[test]
    fn test_escape_for_double_quotes_simple() {
        assert_eq!(escape_for_double_quotes("hello"), "hello");
        assert_eq!(escape_for_double_quotes("foo bar"), "foo bar");
    }

    #[test]
    fn test_escape_for_double_quotes_special_chars() {
        assert_eq!(escape_for_double_quotes("$HOME"), "\\$HOME");
        assert_eq!(escape_for_double_quotes("a\"b"), "a\\\"b");
        assert_eq!(escape_for_double_quotes("$(cmd)"), "\\$(cmd)");
        assert_eq!(escape_for_double_quotes("`cmd`"), "\\`cmd\\`");
    }

    #[test]
    fn test_escape_for_double_quotes_backslash() {
        assert_eq!(escape_for_double_quotes("a\\b"), "a\\\\b");
        assert_eq!(escape_for_double_quotes("\\$HOME"), "\\\\\\$HOME");
    }

    #[test]
    fn test_escape_for_double_quotes_combined() {
        // Test multiple special chars together
        assert_eq!(
            escape_for_double_quotes("echo \"$HOME\" `pwd`"),
            "echo \\\"\\$HOME\\\" \\`pwd\\`"
        );
    }

    // --- escape_for_sh_c_inner_single_quote tests ---

    #[test]
    fn test_escape_for_sh_c_inner_single_quote_simple() {
        assert_eq!(escape_for_sh_c_inner_single_quote("/bin/bash"), "/bin/bash");
    }

    #[test]
    fn test_escape_for_sh_c_inner_single_quote_with_single_quote() {
        // Shell path with single quote
        // Step 1: ' -> '\'' (single quote escaping)
        // Step 2: backslash in '\'' gets doubled for double-quote context -> '\\''
        assert_eq!(
            escape_for_sh_c_inner_single_quote("/bin/user's shell"),
            "/bin/user'\\\\''s shell"
        );
    }

    #[test]
    fn test_escape_for_sh_c_inner_single_quote_with_dollar() {
        // Dollar sign needs double-quote escaping
        assert_eq!(
            escape_for_sh_c_inner_single_quote("/path/$dir/shell"),
            "/path/\\$dir/shell"
        );
    }

    #[test]
    fn test_escape_for_sh_c_inner_single_quote_combined() {
        // Both single quote and dollar sign
        // Single quote becomes '\'' then backslash is doubled -> '\\''
        // Dollar sign becomes \$ (escaped for double quotes)
        assert_eq!(
            escape_for_sh_c_inner_single_quote("it's $HOME"),
            "it'\\\\''s \\$HOME"
        );
    }

    // --- wrap_for_non_posix_shell tests ---

    #[test]
    fn test_wrap_for_non_posix_shell_simple() {
        assert_eq!(wrap_for_non_posix_shell("echo hello"), "sh -c 'echo hello'");
    }

    #[test]
    fn test_wrap_for_non_posix_shell_with_single_quote() {
        assert_eq!(
            wrap_for_non_posix_shell("echo 'quoted'"),
            "sh -c 'echo '\\''quoted'\\'''"
        );
    }

    #[test]
    fn test_wrap_for_non_posix_shell_with_dollar() {
        // Dollar sign doesn't need escaping in single quotes
        assert_eq!(wrap_for_non_posix_shell("echo $HOME"), "sh -c 'echo $HOME'");
    }

    #[test]
    fn test_wrap_for_non_posix_shell_complex() {
        assert_eq!(
            wrap_for_non_posix_shell("claude -- \"$(cat PROMPT.md)\""),
            "sh -c 'claude -- \"$(cat PROMPT.md)\"'"
        );
    }

    // --- structured agent resolver tests ---

    fn config_with_agent(agent: &str) -> Config {
        Config {
            agent: Some(agent.to_string()),
            selected_agent: Some(agent.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_structured_pane_command_expands_agent_placeholder() {
        let config = config_with_agent("claude");
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, "claude");
        assert_eq!(resolved.selected_agent.unwrap().kind(), "claude");
    }

    #[test]
    fn resolve_structured_pane_command_uses_named_profile() {
        let mut config = config_with_agent("cc-sonnet");
        config.agents.insert(
            "cc-sonnet".to_string(),
            crate::config::AgentEntry {
                command: Some("claude".to_string()),
                agent_type: Some("claude".to_string()),
                args: vec!["--model".to_string(), "sonnet".to_string()],
                env: std::collections::BTreeMap::new(),
            },
        );
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, "claude --model sonnet");
        assert_eq!(resolved.selected_agent.unwrap().kind(), "claude");
    }

    #[test]
    fn resolve_structured_pane_command_supports_named_placeholder() {
        let mut config = config_with_agent("claude");
        config.agents.insert(
            "codex-mini".to_string(),
            crate::config::AgentEntry {
                command: Some("codex".to_string()),
                agent_type: Some("codex".to_string()),
                args: vec!["exec".to_string(), "-m".to_string(), "mini".to_string()],
                env: std::collections::BTreeMap::new(),
            },
        );
        let resolved = resolve_pane_command_with_config(
            Some("<agent:codex-mini>"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, "codex exec -m mini");
        assert_eq!(resolved.selected_agent.unwrap().kind(), "codex");
    }

    #[test]
    fn resolve_structured_pane_command_applies_env_and_prompt() {
        let prompt = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");
        let mut config = config_with_agent("cc-proxy");
        config.agents.insert(
            "cc-proxy".to_string(),
            crate::config::AgentEntry {
                command: Some("/bin/claude wrapper".to_string()),
                agent_type: Some("claude".to_string()),
                args: vec!["--verbose".to_string()],
                env: std::collections::BTreeMap::from([(
                    "ANTHROPIC_BASE_URL".to_string(),
                    crate::config::AgentEnvValue::Literal("http://localhost:18765".to_string()),
                )]),
            },
        );
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            Some(&prompt),
            &working_dir,
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(
            resolved.command,
            " env ANTHROPIC_BASE_URL=http://localhost:18765 /bin/claude wrapper --verbose -- \"$(cat PROMPT.md)\""
        );
        assert!(resolved.prompt_injected);
    }

    #[test]
    fn resolve_structured_pane_command_injects_prompt_once_for_literal_agent_name() {
        let prompt = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");
        let config = config_with_agent("claude");
        let resolved = resolve_pane_command_with_config(
            Some("claude"),
            true,
            Some(&prompt),
            &working_dir,
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, " claude -- \"$(cat PROMPT.md)\"");
        assert_eq!(resolved.render_command(), resolved.command);
    }

    #[test]
    fn resolve_structured_pane_command_wraps_non_posix_shell() {
        let prompt = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");
        let config = config_with_agent("claude");
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            Some(&prompt),
            &working_dir,
            &config,
            None,
            "/opt/homebrew/bin/nu",
        )
        .unwrap();
        assert_eq!(resolved.command, " sh -c 'claude -- \"$(cat PROMPT.md)\"'");
    }

    #[test]
    fn resolve_structured_pane_command_preserves_regular_command() {
        let config = config_with_agent("claude");
        let resolved = resolve_pane_command_with_config(
            Some("vim"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, "vim");
        assert!(resolved.selected_agent.is_none());
    }

    #[test]
    fn resolve_structured_pane_command_injects_antigravity_prompt() {
        let prompt = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");
        let config = config_with_agent("agy");
        let resolved = resolve_pane_command_with_config(
            Some("agy"),
            true,
            Some(&prompt),
            &working_dir,
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();

        assert!(resolved.prompt_injected);
        assert!(resolved.command.contains("-i"));
        assert!(resolved.command.contains("PROMPT.md"));
        assert_eq!(resolved.selected_agent.unwrap().kind(), "agy");
    }

    #[test]
    fn resolve_structured_pane_command_inserts_default_subcommand_before_flags() {
        let mut config = config_with_agent("kiro");
        config.agents.insert(
            "kiro".to_string(),
            crate::config::AgentEntry {
                command: Some("kiro-cli".to_string()),
                agent_type: Some("kiro-cli".to_string()),
                args: vec!["--verbose".to_string()],
                env: std::collections::BTreeMap::new(),
            },
        );
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, "kiro-cli chat --verbose");
    }

    #[test]
    fn resolve_structured_pane_command_preserves_existing_subcommand() {
        let config = config_with_agent("kiro-cli chat --model foo");
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, "kiro-cli chat --model foo");
    }

    #[test]
    fn resolve_structured_pane_command_preserves_other_subcommand() {
        let config = config_with_agent("kiro-cli login");
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, "kiro-cli login");
    }

    #[test]
    fn resolve_structured_pane_command_handles_env_wrapped_kiro() {
        let prompt = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");
        let mut config = config_with_agent("kiro-env");
        config.agents.insert(
            "kiro-env".to_string(),
            crate::config::AgentEntry {
                command: Some("env FOO=bar kiro-cli".to_string()),
                agent_type: Some("kiro-cli".to_string()),
                args: Vec::new(),
                env: std::collections::BTreeMap::new(),
            },
        );
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            Some(&prompt),
            &working_dir,
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(
            resolved.command,
            " env FOO=bar kiro-cli chat \"$(cat PROMPT.md)\""
        );
    }

    #[test]
    fn resolve_structured_pane_command_handles_quoted_env_values() {
        let prompt = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");
        let mut config = config_with_agent("cc-env");
        config.agents.insert(
            "cc-env".to_string(),
            crate::config::AgentEntry {
                command: Some("claude".to_string()),
                agent_type: Some("claude".to_string()),
                args: Vec::new(),
                env: std::collections::BTreeMap::from([(
                    "FOO".to_string(),
                    crate::config::AgentEnvValue::Literal("bar baz".to_string()),
                )]),
            },
        );
        let mut resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            Some(&prompt),
            &working_dir,
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        let agent = resolved.selected_agent.as_mut().unwrap();
        agent
            .command
            .prepend_args_fragment("--dangerously-skip-permissions");
        assert_eq!(
            resolved.render_command(),
            " env FOO='bar baz' claude --dangerously-skip-permissions -- \"$(cat PROMPT.md)\""
        );
    }

    #[test]
    fn resolve_structured_pane_command_appends_codex_resume_after_args() {
        let mut config = config_with_agent("codex-mini");
        config.agents.insert(
            "codex-mini".to_string(),
            crate::config::AgentEntry {
                command: Some("codex".to_string()),
                agent_type: Some("codex".to_string()),
                args: vec!["exec".to_string(), "-m".to_string(), "mini".to_string()],
                env: std::collections::BTreeMap::new(),
            },
        );
        let mut resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        let agent = resolved.selected_agent.as_mut().unwrap();
        agent.command.append_args_fragment("resume --last");
        assert_eq!(
            resolved.render_command(),
            "codex exec -m mini resume --last"
        );
    }

    #[test]
    fn resolve_pane_command_expands_agent_placeholder_with_args() {
        let config = config_with_agent("claude");
        let resolved = resolve_pane_command_with_config(
            Some("<agent> --model sonnet"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.render_command(), "claude --model sonnet");
    }

    #[test]
    fn resolve_pane_command_expands_named_agent_placeholder_with_args() {
        let mut config = config_with_agent("claude");
        config.agents.insert(
            "cc-work".to_string(),
            crate::config::AgentEntry {
                command: Some("claude".to_string()),
                agent_type: Some("claude".to_string()),
                args: vec!["--model".to_string(), "opus".to_string()],
                env: std::collections::BTreeMap::new(),
            },
        );
        let resolved = resolve_pane_command_with_config(
            Some("<agent:cc-work> --verbose"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.render_command(), "claude --model opus --verbose");
    }

    #[test]
    fn resolve_pane_command_preserves_shell_syntax_in_manual_agent_commands() {
        let config = config_with_agent("claude");
        let resolved = resolve_pane_command_with_config(
            Some("claude --model sonnet > out.txt"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.render_command(), "claude --model sonnet > out.txt");
    }

    #[test]
    fn resolve_pane_command_preserves_config_env_for_manual_agent_commands() {
        let mut config = config_with_agent("cc-env");
        config.agents.insert(
            "cc-env".to_string(),
            crate::config::AgentEntry {
                command: Some("claude".to_string()),
                agent_type: Some("claude".to_string()),
                args: Vec::new(),
                env: std::collections::BTreeMap::from([(
                    "API_KEY".to_string(),
                    crate::config::AgentEnvValue::Literal("secret".to_string()),
                )]),
            },
        );
        let resolved = resolve_pane_command_with_config(
            Some("claude --verbose"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(
            resolved.render_command(),
            "env API_KEY=secret claude --verbose"
        );
    }
}
