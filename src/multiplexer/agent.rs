//! Agent profile system for extensible agent-specific behavior.
//!
//! This module defines the `AgentProfile` trait and built-in profiles for
//! known AI coding agents. Adding support for a new agent only requires
//! implementing this trait.

use std::path::Path;

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

pub struct PiProfile;

impl AgentProfile for PiProfile {
    fn name(&self) -> &'static str {
        "pi"
    }

    fn needs_auto_status(&self) -> bool {
        true
    }

    fn prompt_argument(&self, prompt_path: &str) -> String {
        format!("\"$(cat {})\"" , prompt_path)
    }

    fn auto_name_command(&self) -> Option<&'static str> {
        Some("pi -p")
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
    &OpenCodeProfile,
    &CodexProfile,
    &PiProfile,
    &KiroProfile,
    &VibeProfile,
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
        crate::config::resolve_executable_path(token).unwrap_or_else(|| token.to_string());

    // Extract stem from the resolved path
    Path::new(&resolved)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Find the real executable token in a command string, skipping past
/// `env` wrappers and `VAR=value` assignments.
///
/// Returns a reference into the original command string.
pub(crate) fn find_executable_token(command: &str) -> &str {
    let mut iter = command.split_whitespace();

    let first = match iter.next() {
        Some(t) => t,
        None => return "",
    };

    // Check if first token is a VAR=value assignment
    if is_env_assignment(first) {
        for token in iter {
            if is_env_assignment(token) {
                continue;
            }
            return token;
        }
        return first; // fallback
    }

    // Check if first token is `env`
    let first_stem = Path::new(first)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    if first_stem != "env" {
        return first; // not a wrapper
    }

    // Skip env's own flags and arguments
    let mut skip_next = false;
    for token in iter {
        if skip_next {
            skip_next = false;
            continue;
        }
        if token.starts_with('-') {
            // Flags that take a value argument
            if matches!(token, "-u" | "-S" | "-P" | "--unset") {
                skip_next = true;
            }
            continue;
        }
        if is_env_assignment(token) {
            continue;
        }
        return token; // found the real executable
    }

    first // fallback to "env" if nothing found
}

/// Check if a token looks like an environment variable assignment (VAR=value).
fn is_env_assignment(token: &str) -> bool {
    token.contains('=')
        && !token.starts_with('-')
        && !token.starts_with('/')
        && token
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
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
    fn test_pi_profile() {
        let profile = PiProfile;
        assert_eq!(profile.name(), "pi");
        assert!(!profile.needs_bang_delay());
        assert!(profile.needs_auto_status());
        assert_eq!(
            profile.prompt_argument("PROMPT.md"),
            "\"$(cat PROMPT.md)\""
        );
        assert_eq!(profile.skip_permissions_flag(), None);
        assert_eq!(profile.auto_name_command(), Some("pi -p"));
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
        assert!(is_known_agent("codex"));
        assert!(is_known_agent("opencode"));
        assert!(is_known_agent("pi"));
        assert!(is_known_agent("kiro-cli"));
        assert!(is_known_agent("vibe"));
    }

    #[test]
    fn test_is_known_agent_with_args() {
        assert!(is_known_agent("claude --dangerously-skip-permissions"));
        assert!(is_known_agent("codex --yolo"));
        assert!(is_known_agent("gemini -i foo"));
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
}
