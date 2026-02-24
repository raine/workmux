//! Agent profile system for extensible agent-specific behavior.
//!
//! This module defines the `AgentProfile` trait and built-in profiles for
//! known AI coding agents. Adding support for a new agent only requires
//! implementing this trait.

use std::path::Path;

use crate::multiplexer::types::AgentStatus;

/// Terminal output patterns for detecting agent status via polling.
///
/// Used by agents that don't support hooks (like Copilot CLI) to infer
/// status by matching patterns in the terminal output.
#[derive(Debug, Clone)]
pub struct StatusPatterns {
    /// Patterns indicating the agent is waiting for user input/permission.
    /// Examples: "[Y/n]", "Allow?", permission prompts
    pub waiting: Vec<&'static str>,

    /// Patterns indicating the agent is actively working.
    /// Examples: spinner characters, "Thinking...", "Running tool"
    pub working: Vec<&'static str>,

    /// Patterns indicating the agent has finished (shell prompt visible).
    /// Examples: "$ ", "❯ ", "% " at end of output
    pub done: Vec<&'static str>,
}

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

    /// Whether this agent requires polling-based status detection.
    ///
    /// Returns true for agents without hooks support (like Copilot CLI).
    /// When true, the status poller will periodically capture terminal output
    /// and match against `status_patterns()` to infer agent status.
    fn needs_polling(&self) -> bool {
        false
    }

    /// Terminal patterns for polling-based status detection.
    ///
    /// Returns `None` for agents with hooks support (they don't need polling).
    /// Returns `Some(StatusPatterns)` for agents that need pattern matching.
    fn status_patterns(&self) -> Option<StatusPatterns> {
        None
    }

    /// Detect agent status from terminal output using pattern matching.
    ///
    /// Analyzes the captured terminal content and returns the detected status.
    /// Returns `None` if no patterns match (status unknown).
    ///
    /// The detection priority is: waiting > done > working
    /// Done is checked before working because spinner chars linger in scrollback
    /// even after the agent finishes, while a shell prompt on the last line is
    /// a reliable signal that the agent has exited.
    ///
    /// Waiting and working patterns are checked only against the last few lines
    /// of output (near the cursor) to avoid false positives from old artifacts
    /// lingering in scrollback — e.g. spinner chars or tool names from a
    /// previous task that are still visible higher in the capture buffer.
    fn detect_status(&self, terminal_content: &str) -> Option<AgentStatus> {
        let patterns = self.status_patterns()?;
        let lines: Vec<&str> = terminal_content.lines().collect();

        // Build a string from only the last few lines for recency-scoped checks.
        // Spinners and tool indicators appear at/near the bottom when active;
        // 5 lines is enough to capture them without matching old scrollback.
        let recent_start = lines.len().saturating_sub(5);
        let recent_text: String = lines[recent_start..].join("\n");

        // Check waiting patterns first (highest priority, recent lines only)
        for pattern in &patterns.waiting {
            if recent_text.contains(pattern) {
                return Some(AgentStatus::Waiting);
            }
        }

        // Check done patterns (shell prompt on last line) before working.
        // A shell prompt as the last non-empty line reliably means the agent exited,
        // even if spinner chars are still visible higher in the scrollback.
        // We require the prompt to have no trailing content (just cursor/spaces)
        // to avoid matching Copilot's own tool output like "❯ Edit src/file.rs".
        if let Some(last_line) = lines.iter().rev().find(|l| !l.trim().is_empty()) {
            for pattern in &patterns.done {
                if last_line.starts_with(pattern) && last_line.trim_end() == pattern.trim_end() {
                    return Some(AgentStatus::Done);
                }
            }
        }

        // Check working patterns (active processing, recent lines only)
        for pattern in &patterns.working {
            if recent_text.contains(pattern) {
                return Some(AgentStatus::Working);
            }
        }

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
}

pub struct CodexProfile;

impl AgentProfile for CodexProfile {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn skip_permissions_flag(&self) -> Option<&'static str> {
        Some("--yolo")
    }
}

pub struct CopilotProfile;

impl AgentProfile for CopilotProfile {
    fn name(&self) -> &'static str {
        "copilot"
    }

    fn needs_polling(&self) -> bool {
        true
    }

    fn skip_permissions_flag(&self) -> Option<&'static str> {
        Some("--allow-all-tools")
    }

    fn prompt_argument(&self, prompt_path: &str) -> String {
        format!("-p \"$(cat {})\"", prompt_path)
    }

    fn status_patterns(&self) -> Option<StatusPatterns> {
        Some(StatusPatterns {
            // Permission/confirmation prompts
            waiting: vec![
                "[Y/n]",
                "[y/N]",
                "(y/n)",
                "Allow?",
                "Confirm?",
                "Continue?",
                "Proceed?",
                "? (Y/n)",
                "? (y/N)",
                "Do you want",
            ],
            // Active processing indicators
            working: vec![
                // Braille spinner characters
                "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
                // Copilot CLI circular spinner (three states)
                "◎", "◉", "∙",
                // Copilot CLI tool names (format: "● Edit src/file.rs")
                "● Edit ",
                "● Read ",
                "● Bash ",
                "● Write ",
                "● Search ",
                "● Grep ",
                "● Glob ",
                "● Run ",
                "● List ",
            ],
            // Shell prompt patterns (agent exited)
            done: vec![
                "❯ ", "➜ ", "$ ", "% ", // Basic prompt ("> " omitted: conflicts with Copilot tool output lines)
            ],
        })
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
    &CopilotProfile,
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

/// Extract the executable stem from a command string.
///
/// Examples:
/// - "claude --verbose" -> "claude"
/// - "/usr/bin/gemini" -> "gemini"
fn extract_executable_stem(command: &str) -> String {
    let (token, _) = crate::config::split_first_token(command).unwrap_or((command, ""));

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
    fn test_resolve_profile_codex() {
        let profile = resolve_profile(Some("codex"));
        assert_eq!(profile.name(), "codex");
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

    // === CopilotProfile tests ===

    #[test]
    fn test_copilot_profile() {
        let profile = CopilotProfile;
        assert_eq!(profile.name(), "copilot");
        assert!(!profile.needs_bang_delay());
        assert!(!profile.needs_auto_status());
        assert!(profile.needs_polling());
        assert_eq!(
            profile.prompt_argument("PROMPT.md"),
            "-p \"$(cat PROMPT.md)\""
        );
        assert_eq!(profile.skip_permissions_flag(), Some("--allow-all-tools"));
        assert!(profile.status_patterns().is_some());
    }

    #[test]
    fn test_resolve_profile_copilot() {
        let profile = resolve_profile(Some("copilot"));
        assert_eq!(profile.name(), "copilot");
    }

    #[test]
    fn test_is_known_agent_copilot() {
        assert!(is_known_agent("copilot"));
        assert!(is_known_agent("copilot --allow-all-tools"));
    }

    // === Status detection tests ===

    #[test]
    fn test_detect_status_waiting() {
        let profile = CopilotProfile;

        // Permission prompts
        assert_eq!(
            profile.detect_status("Allow file write? [Y/n]"),
            Some(AgentStatus::Waiting)
        );
        assert_eq!(
            profile.detect_status("Continue? (y/n)"),
            Some(AgentStatus::Waiting)
        );
        // Copilot CLI numbered choice menu
        assert_eq!(
            profile.detect_status("Do you want to proceed?\n> 1. Yes\n> 2. No"),
            Some(AgentStatus::Waiting)
        );
    }

    #[test]
    fn test_detect_status_working() {
        let profile = CopilotProfile;

        // Braille spinner character
        assert_eq!(
            profile.detect_status("⠋ Processing request..."),
            Some(AgentStatus::Working)
        );
        // Copilot CLI circular spinner states
        assert_eq!(
            profile.detect_status("◎ Working..."),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            profile.detect_status("◉ Working..."),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            profile.detect_status("∙ Working..."),
            Some(AgentStatus::Working)
        );
        // Status message (generic keywords removed — too broad)
        // Tool execution
        assert_eq!(
            profile.detect_status("● Run tests\n◎ running"),
            Some(AgentStatus::Working)
        );
    }

    #[test]
    fn test_detect_status_done() {
        let profile = CopilotProfile;

        // Shell prompts as the last non-empty line (starts_with check)
        assert_eq!(
            profile.detect_status("Task completed.\n❯ "),
            Some(AgentStatus::Done)
        );
        assert_eq!(
            profile.detect_status("Done!\n$ "),
            Some(AgentStatus::Done)
        );
        // Prompt with trailing command should NOT match done (it's tool output)
        // and working should be detected from the tool name in the content
        assert_eq!(
            profile.detect_status("● Edit src/file.rs\n◎ continuing"),
            Some(AgentStatus::Working)
        );
    }

    #[test]
    fn test_detect_status_waiting_priority_over_working() {
        let profile = CopilotProfile;

        // If both waiting and working patterns present, waiting wins
        let content = "⠋ Processing...\nAllow? [Y/n]";
        assert_eq!(profile.detect_status(content), Some(AgentStatus::Waiting));
    }

    #[test]
    fn test_detect_status_no_match() {
        let profile = CopilotProfile;

        // No recognizable patterns
        assert_eq!(profile.detect_status("Some random text"), None);
        // "> " only matches done when it STARTS the last non-empty line
        assert_eq!(
            profile.detect_status("Here is the change:\nsome text > with > in it"),
            None
        );
        // spinner in scrollback but shell prompt on last line → done wins
        assert_eq!(
            profile.detect_status("◎ Reading file\n❯ "),
            Some(AgentStatus::Done)
        );
    }

    #[test]
    fn test_detect_status_ignores_working_artifacts_in_scrollback() {
        let profile = CopilotProfile;

        // Spinner and tool names from a previous task are still in scrollback
        // (more than 5 lines above the bottom), but the agent is now idle.
        // The last lines are plain result text — should NOT match Working.
        let content = "● Edit src/main.rs\n\
                        ◎ applying changes\n\
                        ⠋ thinking\n\
                        line4\n\
                        line5\n\
                        line6\n\
                        Changes applied successfully.\n\
                        All tests pass.\n\
                        Summary of changes:";
        assert_eq!(profile.detect_status(content), None);
    }

    #[test]
    fn test_default_profile_no_polling() {
        let profile = DefaultProfile;
        assert!(!profile.needs_polling());
        assert!(profile.status_patterns().is_none());
        assert!(profile.detect_status("anything").is_none());
    }

    #[test]
    fn test_claude_profile_no_polling() {
        let profile = ClaudeProfile;
        assert!(!profile.needs_polling());
        assert!(profile.status_patterns().is_none());
    }
}
