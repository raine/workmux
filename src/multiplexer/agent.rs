//! Agent profile system for extensible agent-specific behavior.
//!
//! This module defines the `AgentProfile` trait and built-in profiles for
//! known AI coding agents. Adding support for a new agent only requires
//! implementing this trait.

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{AgentEntry, AgentEnvValue, Config};

/// Describes agent-specific behaviors for command rewriting and status handling.
pub trait AgentProfile: Send + Sync {
    /// Canonical name used for matching (e.g., "claude", "gemini").
    fn name(&self) -> &'static str;

    /// Whether this agent needs special handling for ! prefix (delay after !).
    ///
    /// Claude Code requires a small delay after sending `!` for it to register
    /// as a bash command.
    fn needs_bang_delay(&self) -> bool {
        false
    }

    /// Whether prompts should be delivered as bracketed paste.
    fn uses_bracketed_paste_for_input(&self) -> bool {
        false
    }

    /// Whether this agent needs auto-status when launched with a prompt file.
    ///
    /// Agents with hooks that would normally set status need auto-status as a
    /// workaround when launched with injected prompts. This is a workaround for
    /// Claude Code's broken UserPromptSubmit hook:
    /// <https://github.com/anthropics/claude-code/issues/17284>
    fn needs_auto_status(&self) -> bool {
        false
    }

    /// CLI flag to skip interactive permission prompts when running in a sandbox.
    ///
    /// Returns `None` for agents that don't support this, or a flag string
    /// like `--dangerously-skip-permissions` for agents that do.
    fn skip_permissions_flag(&self) -> Option<&'static str> {
        None
    }

    /// Format the prompt injection argument for this agent.
    ///
    /// Returns the CLI fragment to append (e.g., `-- "$(cat PROMPT.md)"`).
    fn prompt_argument(&self, prompt_path: &str) -> String {
        format!("-- \"$(cat {})\"", prompt_path)
    }

    /// Subcommand to insert after the executable when launching.
    ///
    /// For agents like kiro-cli where the bare executable shows a menu
    /// rather than starting chat, this returns the subcommand needed
    /// (e.g., `"chat"` so that `kiro-cli` becomes `kiro-cli chat`).
    ///
    /// Skipped if the user already includes it in their config
    /// (e.g., `agent: "kiro-cli chat"`).
    fn default_subcommand(&self) -> Option<&'static str> {
        None
    }

    /// Default command for auto-naming branches with this agent's CLI.
    ///
    /// Returns a fast/cheap command string suitable for branch name generation,
    /// or `None` if this profile has no known auto-name command.
    fn auto_name_command(&self) -> Option<&'static str> {
        None
    }

    /// CLI flag to continue/resume the most recent conversation.
    ///
    /// Returns `None` for agents that don't support this, or a flag string
    /// like `--continue` or `--resume` for agents that do.
    fn continue_flag(&self) -> Option<&'static str> {
        None
    }
}

// === Built-in Profiles ===

pub struct ClaudeProfile;

impl AgentProfile for ClaudeProfile {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn needs_bang_delay(&self) -> bool {
        true
    }

    fn needs_auto_status(&self) -> bool {
        true
    }

    fn skip_permissions_flag(&self) -> Option<&'static str> {
        Some("--dangerously-skip-permissions")
    }

    fn auto_name_command(&self) -> Option<&'static str> {
        Some("claude --model haiku -p")
    }

    fn continue_flag(&self) -> Option<&'static str> {
        Some("--continue")
    }
}

pub struct GeminiProfile;

impl AgentProfile for GeminiProfile {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn skip_permissions_flag(&self) -> Option<&'static str> {
        Some("--yolo")
    }

    fn prompt_argument(&self, prompt_path: &str) -> String {
        format!("-i \"$(cat {})\"", prompt_path)
    }

    fn auto_name_command(&self) -> Option<&'static str> {
        Some("gemini -m gemini-2.5-flash-lite -p")
    }

    fn continue_flag(&self) -> Option<&'static str> {
        Some("--resume")
    }
}

pub struct AntigravityProfile;

impl AgentProfile for AntigravityProfile {
    fn name(&self) -> &'static str {
        "agy"
    }

    fn needs_auto_status(&self) -> bool {
        true
    }

    fn skip_permissions_flag(&self) -> Option<&'static str> {
        Some("--dangerously-skip-permissions")
    }

    fn prompt_argument(&self, prompt_path: &str) -> String {
        format!("-i \"$(cat {})\"", prompt_path)
    }

    fn auto_name_command(&self) -> Option<&'static str> {
        Some(r#"sh -c 'agy -p "$(cat)"'"#)
    }

    fn continue_flag(&self) -> Option<&'static str> {
        Some("--continue")
    }
}

pub struct OpenCodeProfile;

impl AgentProfile for OpenCodeProfile {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn needs_auto_status(&self) -> bool {
        true
    }

    fn prompt_argument(&self, prompt_path: &str) -> String {
        format!("--prompt \"$(cat {})\"", prompt_path)
    }

    fn auto_name_command(&self) -> Option<&'static str> {
        Some("opencode run")
    }

    fn continue_flag(&self) -> Option<&'static str> {
        Some("--continue")
    }
}

pub struct CodexProfile;

impl AgentProfile for CodexProfile {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn uses_bracketed_paste_for_input(&self) -> bool {
        true
    }

    fn skip_permissions_flag(&self) -> Option<&'static str> {
        Some("--yolo")
    }

    fn auto_name_command(&self) -> Option<&'static str> {
        Some(r#"codex exec --config model_reasoning_effort="low" -m gpt-5.1-codex-mini"#)
    }

    fn continue_flag(&self) -> Option<&'static str> {
        Some("resume --last")
    }
}

pub struct KiroProfile;

impl AgentProfile for KiroProfile {
    fn name(&self) -> &'static str {
        "kiro-cli"
    }

    fn default_subcommand(&self) -> Option<&'static str> {
        Some("chat")
    }

    fn prompt_argument(&self, prompt_path: &str) -> String {
        format!("\"$(cat {})\"", prompt_path)
    }

    fn auto_name_command(&self) -> Option<&'static str> {
        Some("kiro-cli chat --no-interactive")
    }

    fn continue_flag(&self) -> Option<&'static str> {
        Some("--resume")
    }
}

pub struct VibeProfile;

impl AgentProfile for VibeProfile {
    fn name(&self) -> &'static str {
        "vibe"
    }

    fn skip_permissions_flag(&self) -> Option<&'static str> {
        Some("--agent auto-approve")
    }

    fn prompt_argument(&self, prompt_path: &str) -> String {
        format!("\"$(cat {})\"", prompt_path)
    }

    fn continue_flag(&self) -> Option<&'static str> {
        Some("--continue")
    }
}

pub struct GrokProfile;

impl AgentProfile for GrokProfile {
    fn name(&self) -> &'static str {
        "grok"
    }

    fn needs_auto_status(&self) -> bool {
        true
    }

    fn skip_permissions_flag(&self) -> Option<&'static str> {
        Some("--yolo")
    }

    fn prompt_argument(&self, prompt_path: &str) -> String {
        format!("\"$(cat {})\"", prompt_path)
    }

    fn continue_flag(&self) -> Option<&'static str> {
        Some("--continue")
    }
}

pub struct PiProfile;

impl AgentProfile for PiProfile {
    fn name(&self) -> &'static str {
        "pi"
    }

    fn needs_auto_status(&self) -> bool {
        true
    }

    fn prompt_argument(&self, prompt_path: &str) -> String {
        format!("\"$(cat {})\"", prompt_path)
    }

    fn auto_name_command(&self) -> Option<&'static str> {
        Some("pi -p")
    }

    fn continue_flag(&self) -> Option<&'static str> {
        Some("--continue")
    }
}

pub struct OmpProfile;

impl AgentProfile for OmpProfile {
    fn name(&self) -> &'static str {
        "omp"
    }

    fn needs_auto_status(&self) -> bool {
        true
    }

    fn prompt_argument(&self, prompt_path: &str) -> String {
        format!("\"$(cat {})\"", prompt_path)
    }

    fn auto_name_command(&self) -> Option<&'static str> {
        Some("omp -p")
    }

    fn continue_flag(&self) -> Option<&'static str> {
        Some("--continue")
    }
}

pub struct DefaultProfile;

impl AgentProfile for DefaultProfile {
    fn name(&self) -> &'static str {
        "default"
    }
}

// === Registry ===

static PROFILES: &[&dyn AgentProfile] = &[
    &ClaudeProfile,
    &GeminiProfile,
    &AntigravityProfile,
    &OpenCodeProfile,
    &CodexProfile,
    &PiProfile,
    &OmpProfile,
    &KiroProfile,
    &VibeProfile,
    &GrokProfile,
];

/// Check if a command matches a known agent profile.
///
/// Returns true for commands whose executable stem matches a built-in agent
/// (claude, gemini, codex, opencode). Used for auto-detecting agent panes
/// without requiring the `<agent>` placeholder.
pub fn is_known_agent(command: &str) -> bool {
    let stem = extract_executable_stem(command);
    PROFILES.iter().any(|p| p.name() == stem)
}

/// Resolve an agent command to its profile.
///
/// Returns `DefaultProfile` if no specific profile matches.
pub fn resolve_profile(agent_command: Option<&str>) -> &'static dyn AgentProfile {
    let Some(cmd) = agent_command else {
        return &DefaultProfile;
    };

    let stem = extract_executable_stem(cmd);

    PROFILES
        .iter()
        .find(|p| p.name() == stem)
        .copied()
        .unwrap_or(&DefaultProfile)
}

/// Resolve an agent command to its profile without doing any I/O.
///
/// Unlike [`resolve_profile`], this does not call `tmux show-environment` or
/// `which` to canonicalize bare command names, so it is safe to call from hot
/// render paths. The trade-off is that commands invoked via a custom symlink
/// or wrapper whose own filename does not match a known profile name will
/// resolve to `DefaultProfile`.
pub fn resolve_profile_for_display(agent_command: Option<&str>) -> &'static dyn AgentProfile {
    let Some(cmd) = agent_command else {
        return &DefaultProfile;
    };

    let token = find_executable_token(cmd);
    let stem = executable_stem(&token);

    PROFILES
        .iter()
        .find(|p| p.name() == stem)
        .copied()
        .unwrap_or(&DefaultProfile)
}

/// Resolve an agent profile with an optional type override.
///
/// First tries normal stem-based detection. If that yields `DefaultProfile`
/// and a type override is provided, uses the override to find the profile.
/// This allows opaque wrapper scripts to inherit agent-specific behavior.
pub fn resolve_profile_with_type(
    agent_command: Option<&str>,
    type_override: Option<&str>,
) -> &'static dyn AgentProfile {
    let profile = resolve_profile(agent_command);
    if profile.name() != "default" {
        return profile;
    }
    if let Some(type_name) = type_override
        && let Some(&p) = PROFILES.iter().find(|p| p.name() == type_name)
    {
        return p;
    }
    profile
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, AgentEnvValue>,
    pub env_args: Vec<String>,
    pub env_assignments: Vec<String>,
}

impl AgentCommand {
    pub fn parse(command: &str) -> Option<Self> {
        let parts = shlex::split(command)?;
        let mut iter = parts.into_iter();
        let first = iter.next()?;
        let env = BTreeMap::new();
        let mut env_args = Vec::new();
        let mut env_assignments = Vec::new();
        let program;

        if executable_stem(&first) == "env" {
            program = loop {
                let Some(token) = iter.next() else {
                    return Some(Self {
                        program: first,
                        args: env_args,
                        env,
                        env_args: Vec::new(),
                        env_assignments,
                    });
                };

                if env_flag_takes_value(&token) {
                    env_args.push(token);
                    if let Some(value) = iter.next() {
                        env_args.push(value);
                    }
                    continue;
                }
                if token.starts_with('-') {
                    env_args.push(token);
                    continue;
                }
                if env_assignment(&token).is_some() {
                    env_assignments.push(token);
                    continue;
                }
                break token;
            };
        } else {
            let mut token = first;
            loop {
                if env_assignment(&token).is_none() {
                    program = token;
                    break;
                }
                env_assignments.push(token);
                token = iter.next()?;
            }
        }

        Some(Self {
            program,
            args: iter.collect(),
            env,
            env_args,
            env_assignments,
        })
    }

    pub fn from_entry(name: &str, entry: &AgentEntry) -> Self {
        let mut command = entry
            .command
            .as_deref()
            .and_then(Self::parse)
            .unwrap_or_else(|| Self {
                program: entry.agent_type.clone().unwrap_or_else(|| name.to_string()),
                args: Vec::new(),
                env: BTreeMap::new(),
                env_args: Vec::new(),
                env_assignments: Vec::new(),
            });
        command.args.extend(entry.args.clone());
        command.env.extend(entry.env.clone());
        command
    }

    pub fn shell_string(&self) -> String {
        let mut parts = Vec::new();
        if !self.env_args.is_empty() || !self.env_assignments.is_empty() || !self.env.is_empty() {
            parts.push("env".to_string());
            parts.extend(self.env_args.iter().map(|arg| shell_quote(arg)));
            parts.extend(self.env_assignments.iter().cloned());
            for (key, value) in &self.env {
                parts.push(format!("{}={}", key, value.shell_value()));
            }
        }
        parts.push(shell_quote(&self.program));
        parts.extend(self.args.iter().map(|arg| shell_quote(arg)));
        parts.join(" ")
    }

    pub fn prepend_args_fragment(&mut self, fragment: &str) {
        if let Some(mut args) = shlex::split(fragment) {
            args.extend(std::mem::take(&mut self.args));
            self.args = args;
        }
    }

    pub fn append_args_fragment(&mut self, fragment: &str) {
        if let Some(args) = shlex::split(fragment) {
            self.args.extend(args);
        }
    }

    pub fn insert_default_subcommand(&mut self, subcmd: &str) {
        let needs_subcmd = match self.args.first() {
            None => true,
            Some(first) if first == subcmd => false,
            Some(first) if first.starts_with('-') => true,
            Some(_) => false,
        };
        if needs_subcmd {
            self.args.insert(0, subcmd.to_string());
        }
    }
}

pub fn shell_quote(s: &str) -> String {
    shlex::try_quote(s)
        .map(|quoted| quoted.into_owned())
        .unwrap_or_else(|_| "''".to_string())
}

#[derive(Clone)]
pub struct SelectedAgent {
    pub command: AgentCommand,
    pub profile: &'static dyn AgentProfile,
}

impl SelectedAgent {
    pub fn kind(&self) -> &'static str {
        self.profile.name()
    }

    pub fn shell_command(&self) -> String {
        self.command.shell_string()
    }

    pub fn from_raw(command: &str) -> Option<Self> {
        let command = AgentCommand::parse(command)?;
        let profile = resolve_profile(Some(&command.shell_string()));
        Some(Self { command, profile })
    }
}

pub fn resolve_selected_agent(config: &Config, selector: Option<&str>) -> Option<SelectedAgent> {
    let selector = selector
        .or(config.selected_agent.as_deref())
        .or(config.agent.as_deref())?;

    if let Some(entry) = config.agents.get(selector) {
        let command = AgentCommand::from_entry(selector, entry);
        let profile =
            resolve_profile_with_type(Some(&command.shell_string()), entry.agent_type.as_deref());
        return Some(SelectedAgent { command, profile });
    }

    SelectedAgent::from_raw(selector)
}

/// Extract the executable stem from a command string, looking past
/// `env` wrappers and `VAR=value` assignments.
///
/// Examples:
/// - "claude --verbose" -> "claude"
/// - "/usr/bin/gemini" -> "gemini"
/// - "env -u FOO claude" -> "claude"
/// - "env VAR=value claude --flag" -> "claude"
fn extract_executable_stem(command: &str) -> String {
    let token = find_executable_token(command);

    // Resolve the path to handle symlinks and aliases
    let resolved =
        crate::config::resolve_executable_path(&token).unwrap_or_else(|| token.to_string());

    executable_stem(&resolved).to_string()
}

/// Find the real executable token in a command string, skipping past
/// `env` wrappers and `VAR=value` assignments.
pub(crate) fn find_executable_token(command: &str) -> String {
    AgentCommand::parse(command)
        .map(|command| command.program)
        .unwrap_or_default()
}

fn executable_stem(program: &str) -> &str {
    Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
}

fn env_flag_takes_value(token: &str) -> bool {
    matches!(token, "-u" | "-S" | "-P" | "--unset")
}

fn env_assignment(token: &str) -> Option<(&str, &str)> {
    let (key, value) = token.split_once('=')?;
    if key.is_empty()
        || token.starts_with('-')
        || token.starts_with('/')
        || !key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    Some((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    // === Profile behavior tests ===

    #[test]
    fn test_claude_profile() {
        let profile = ClaudeProfile;
        assert_eq!(profile.name(), "claude");
        assert!(profile.needs_bang_delay());
        assert!(profile.needs_auto_status());
        assert_eq!(
            profile.prompt_argument("PROMPT.md"),
            "-- \"$(cat PROMPT.md)\""
        );
        assert_eq!(
            profile.skip_permissions_flag(),
            Some("--dangerously-skip-permissions")
        );
        assert_eq!(profile.auto_name_command(), Some("claude --model haiku -p"));
        assert_eq!(profile.continue_flag(), Some("--continue"));
    }

    #[test]
    fn test_gemini_profile() {
        let profile = GeminiProfile;
        assert_eq!(profile.name(), "gemini");
        assert!(!profile.needs_bang_delay());
        assert!(!profile.needs_auto_status());
        assert_eq!(
            profile.prompt_argument("PROMPT.md"),
            "-i \"$(cat PROMPT.md)\""
        );
        assert_eq!(profile.skip_permissions_flag(), Some("--yolo"));
        assert_eq!(
            profile.auto_name_command(),
            Some("gemini -m gemini-2.5-flash-lite -p")
        );
        assert_eq!(profile.continue_flag(), Some("--resume"));
    }

    #[test]
    fn test_antigravity_profile() {
        let profile = AntigravityProfile;
        assert_eq!(profile.name(), "agy");
        assert!(!profile.needs_bang_delay());
        assert!(profile.needs_auto_status());
        assert_eq!(
            profile.prompt_argument("PROMPT.md"),
            "-i \"$(cat PROMPT.md)\""
        );
        assert_eq!(
            profile.skip_permissions_flag(),
            Some("--dangerously-skip-permissions")
        );
        assert_eq!(
            profile.auto_name_command(),
            Some(r#"sh -c 'agy -p "$(cat)"'"#)
        );
        assert_eq!(profile.continue_flag(), Some("--continue"));
    }

    #[test]
    fn test_opencode_profile() {
        let profile = OpenCodeProfile;
        assert_eq!(profile.name(), "opencode");
        assert!(!profile.needs_bang_delay());
        assert!(profile.needs_auto_status());
        assert_eq!(
            profile.prompt_argument("PROMPT.md"),
            "--prompt \"$(cat PROMPT.md)\""
        );
        assert_eq!(profile.auto_name_command(), Some("opencode run"));
        assert_eq!(profile.continue_flag(), Some("--continue"));
    }

    #[test]
    fn test_codex_profile() {
        let profile = CodexProfile;
        assert_eq!(profile.name(), "codex");
        assert!(!profile.needs_bang_delay());
        assert!(profile.uses_bracketed_paste_for_input());
        assert!(!profile.needs_auto_status());
        assert_eq!(
            profile.prompt_argument("PROMPT.md"),
            "-- \"$(cat PROMPT.md)\""
        );
        assert_eq!(profile.skip_permissions_flag(), Some("--yolo"));
        assert_eq!(
            profile.auto_name_command(),
            Some(r#"codex exec --config model_reasoning_effort="low" -m gpt-5.1-codex-mini"#)
        );
        assert_eq!(profile.continue_flag(), Some("resume --last"));
    }

    #[test]
    fn test_kiro_profile() {
        let profile = KiroProfile;
        assert_eq!(profile.name(), "kiro-cli");
        assert!(!profile.needs_bang_delay());
        assert!(!profile.needs_auto_status());
        assert_eq!(profile.default_subcommand(), Some("chat"));
        assert_eq!(profile.prompt_argument("PROMPT.md"), "\"$(cat PROMPT.md)\"");
        assert_eq!(profile.skip_permissions_flag(), None);
        assert_eq!(
            profile.auto_name_command(),
            Some("kiro-cli chat --no-interactive")
        );
        assert_eq!(profile.continue_flag(), Some("--resume"));
    }

    #[test]
    fn test_vibe_profile() {
        let profile = VibeProfile;
        assert_eq!(profile.name(), "vibe");
        assert!(!profile.needs_bang_delay());
        assert!(!profile.needs_auto_status());
        assert_eq!(profile.prompt_argument("PROMPT.md"), "\"$(cat PROMPT.md)\"");
        assert_eq!(
            profile.skip_permissions_flag(),
            Some("--agent auto-approve")
        );
        assert_eq!(profile.auto_name_command(), None);
        assert_eq!(profile.continue_flag(), Some("--continue"));
    }

    #[test]
    fn test_grok_profile() {
        let profile = GrokProfile;
        assert_eq!(profile.name(), "grok");
        assert!(!profile.needs_bang_delay());
        assert!(profile.needs_auto_status());
        assert_eq!(profile.prompt_argument("PROMPT.md"), "\"$(cat PROMPT.md)\"");
        assert_eq!(profile.skip_permissions_flag(), Some("--yolo"));
        assert_eq!(profile.auto_name_command(), None);
        assert_eq!(profile.continue_flag(), Some("--continue"));
    }

    #[test]
    fn test_pi_profile() {
        let profile = PiProfile;
        assert_eq!(profile.name(), "pi");
        assert!(!profile.needs_bang_delay());
        assert!(profile.needs_auto_status());
        assert_eq!(profile.prompt_argument("PROMPT.md"), "\"$(cat PROMPT.md)\"");
        assert_eq!(profile.skip_permissions_flag(), None);
        assert_eq!(profile.auto_name_command(), Some("pi -p"));
        assert_eq!(profile.continue_flag(), Some("--continue"));
    }

    #[test]
    fn test_omp_profile() {
        let profile = OmpProfile;
        assert_eq!(profile.name(), "omp");
        assert!(!profile.needs_bang_delay());
        assert!(profile.needs_auto_status());
        assert_eq!(profile.prompt_argument("PROMPT.md"), "\"$(cat PROMPT.md)\"");
        assert_eq!(profile.skip_permissions_flag(), None);
        assert_eq!(profile.auto_name_command(), Some("omp -p"));
        assert_eq!(profile.continue_flag(), Some("--continue"));
    }

    #[test]
    fn test_default_profile() {
        let profile = DefaultProfile;
        assert_eq!(profile.name(), "default");
        assert!(!profile.needs_bang_delay());
        assert!(!profile.needs_auto_status());
        assert_eq!(
            profile.prompt_argument("PROMPT.md"),
            "-- \"$(cat PROMPT.md)\""
        );
        assert_eq!(profile.auto_name_command(), None);
        assert_eq!(profile.continue_flag(), None);
    }

    // === resolve_profile tests ===

    #[test]
    fn test_resolve_profile_none() {
        let profile = resolve_profile(None);
        assert_eq!(profile.name(), "default");
    }

    #[test]
    fn test_resolve_profile_claude() {
        let profile = resolve_profile(Some("claude"));
        assert_eq!(profile.name(), "claude");
    }

    #[test]
    fn test_resolve_profile_claude_with_args() {
        let profile = resolve_profile(Some("claude --verbose"));
        assert_eq!(profile.name(), "claude");
    }

    #[test]
    fn test_resolve_profile_gemini() {
        let profile = resolve_profile(Some("gemini"));
        assert_eq!(profile.name(), "gemini");
    }

    #[test]
    fn test_resolve_profile_agy() {
        let profile = resolve_profile(Some("agy"));
        assert_eq!(profile.name(), "agy");
    }

    #[test]
    fn test_resolve_profile_opencode() {
        let profile = resolve_profile(Some("opencode"));
        assert_eq!(profile.name(), "opencode");
    }

    #[test]
    fn test_resolve_profile_pi() {
        let profile = resolve_profile(Some("pi"));
        assert_eq!(profile.name(), "pi");
    }

    #[test]
    fn test_resolve_profile_omp() {
        let profile = resolve_profile(Some("omp"));
        assert_eq!(profile.name(), "omp");
    }

    #[test]
    fn test_resolve_profile_codex() {
        let profile = resolve_profile(Some("codex"));
        assert_eq!(profile.name(), "codex");
    }

    #[test]
    fn test_resolve_profile_kiro() {
        let profile = resolve_profile(Some("kiro-cli"));
        assert_eq!(profile.name(), "kiro-cli");
    }

    #[test]
    fn test_resolve_profile_kiro_with_subcommand() {
        let profile = resolve_profile(Some("kiro-cli chat"));
        assert_eq!(profile.name(), "kiro-cli");
    }

    #[test]
    fn test_resolve_profile_vibe() {
        let profile = resolve_profile(Some("vibe"));
        assert_eq!(profile.name(), "vibe");
    }

    #[test]
    fn test_resolve_profile_unknown() {
        let profile = resolve_profile(Some("unknown-agent"));
        assert_eq!(profile.name(), "default");
    }

    // === is_known_agent tests ===

    #[test]
    fn test_is_known_agent_bare_names() {
        assert!(is_known_agent("claude"));
        assert!(is_known_agent("gemini"));
        assert!(is_known_agent("agy"));
        assert!(is_known_agent("codex"));
        assert!(is_known_agent("opencode"));
        assert!(is_known_agent("pi"));
        assert!(is_known_agent("omp"));
        assert!(is_known_agent("kiro-cli"));
        assert!(is_known_agent("vibe"));
    }

    #[test]
    fn test_is_known_agent_with_args() {
        assert!(is_known_agent("claude --dangerously-skip-permissions"));
        assert!(is_known_agent("codex --yolo"));
        assert!(is_known_agent("gemini -i foo"));
        assert!(is_known_agent("agy --continue"));
    }

    #[test]
    fn test_is_known_agent_unknown() {
        assert!(!is_known_agent("vim"));
        assert!(!is_known_agent("npm run dev"));
        assert!(!is_known_agent("clear"));
        assert!(!is_known_agent("unknown-agent"));
    }

    // === find_executable_token tests ===

    #[test]
    fn test_find_executable_token_simple() {
        assert_eq!(find_executable_token("claude"), "claude");
        assert_eq!(find_executable_token("claude --verbose"), "claude");
        assert_eq!(find_executable_token("/usr/bin/gemini"), "/usr/bin/gemini");
    }

    #[test]
    fn test_find_executable_token_env_wrapper() {
        assert_eq!(find_executable_token("env claude"), "claude");
        assert_eq!(
            find_executable_token("env -u CLAUDE_CODE_USE_BEDROCK claude"),
            "claude"
        );
        assert_eq!(
            find_executable_token("env -u FOO -u BAR claude --flag"),
            "claude"
        );
        assert_eq!(find_executable_token("env FOO=bar claude"), "claude");
        assert_eq!(find_executable_token("env -u FOO BAR=baz claude"), "claude");
    }

    #[test]
    fn test_find_executable_token_env_assignments() {
        assert_eq!(find_executable_token("FOO=bar claude"), "claude");
        assert_eq!(
            find_executable_token("FOO=bar BAR=baz codex --yolo"),
            "codex"
        );
    }

    #[test]
    fn test_find_executable_token_empty() {
        assert_eq!(find_executable_token(""), "");
    }

    #[test]
    fn test_find_executable_token_env_only() {
        // env with no real executable falls back to "env"
        assert_eq!(find_executable_token("env -u FOO"), "env");
    }

    // === env-wrapped resolve_profile tests ===

    #[test]
    fn test_resolve_profile_env_wrapped_claude() {
        let profile = resolve_profile(Some("env -u FOO claude"));
        assert_eq!(profile.name(), "claude");
    }

    #[test]
    fn test_resolve_profile_env_wrapped_with_assignments() {
        let profile = resolve_profile(Some(
            "env -u CLAUDE_CODE_USE_BEDROCK -u AWS_REGION AWS_PROFILE=prod claude",
        ));
        assert_eq!(profile.name(), "claude");
    }

    #[test]
    fn test_resolve_profile_leading_assignments() {
        let profile = resolve_profile(Some("FOO=bar claude --verbose"));
        assert_eq!(profile.name(), "claude");
    }

    // === env-wrapped is_known_agent tests ===

    #[test]
    fn test_is_known_agent_env_wrapped() {
        assert!(is_known_agent("env -u FOO claude"));
        assert!(is_known_agent("env FOO=bar codex --yolo"));
        assert!(is_known_agent("FOO=bar gemini -i foo"));
        assert!(is_known_agent("env FOO=bar agy -i foo"));
        assert!(is_known_agent("env FOO=bar omp --continue"));
    }

    #[test]
    fn test_is_known_agent_env_wrapped_unknown() {
        assert!(!is_known_agent("env -u FOO vim"));
        assert!(!is_known_agent("env FOO=bar npm run dev"));
    }

    // === resolve_profile_with_type tests ===

    #[test]
    fn test_type_override_for_wrapper_script() {
        // Wrapper script stem doesn't match any profile
        let profile = resolve_profile_with_type(Some("/path/to/smart-picker"), Some("claude"));
        assert_eq!(profile.name(), "claude");
    }

    #[test]
    fn test_type_override_ignored_when_stem_matches() {
        // codex stem matches CodexProfile, type override should be ignored
        let profile = resolve_profile_with_type(Some("codex --yolo"), Some("gemini"));
        assert_eq!(profile.name(), "codex");
    }

    #[test]
    fn test_type_override_none() {
        let profile = resolve_profile_with_type(Some("/path/to/wrapper"), None);
        assert_eq!(profile.name(), "default");
    }

    #[test]
    fn test_type_override_invalid() {
        let profile = resolve_profile_with_type(Some("/path/to/wrapper"), Some("nonexistent"));
        assert_eq!(profile.name(), "default");
    }

    #[test]
    fn test_agent_command_renders_from_env() {
        let command = AgentCommand {
            program: "claude".to_string(),
            args: vec!["--dangerously-skip-permissions".to_string()],
            env: BTreeMap::from([
                (
                    "ANTHROPIC_AUTH_TOKEN".to_string(),
                    AgentEnvValue::FromEnv {
                        from_env: "DEEPSEEK_API_KEY".to_string(),
                    },
                ),
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    AgentEnvValue::Literal("https://api.deepseek.com/anthropic".to_string()),
                ),
            ]),
            env_args: Vec::new(),
            env_assignments: Vec::new(),
        };

        assert_eq!(
            command.shell_string(),
            "env ANTHROPIC_AUTH_TOKEN=\"$DEEPSEEK_API_KEY\" ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn test_agent_command_preserves_shell_env_assignment_expansion() {
        let command = AgentCommand::parse("env CLAUDE_CONFIG_DIR=~/.claude-personal claude")
            .expect("command parses");
        assert_eq!(
            command.shell_string(),
            "env CLAUDE_CONFIG_DIR=~/.claude-personal claude"
        );

        let command = AgentCommand::parse("ANTHROPIC_AUTH_TOKEN=$ANTHROPIC_AUTH_TOKEN claude")
            .expect("command parses");
        assert_eq!(
            command.shell_string(),
            "env ANTHROPIC_AUTH_TOKEN=$ANTHROPIC_AUTH_TOKEN claude"
        );
    }
}
