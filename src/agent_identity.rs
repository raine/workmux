//! Agent identity detection for display surfaces.

use std::path::Path;

/// Supported agent identities for compact UI display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
    OpenCode,
    Gemini,
    Copilot,
    Pi,
    KiroCli,
    Vibe,
    Unknown,
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::Gemini => "Gemini",
            Self::Copilot => "Copilot",
            Self::Pi => "Pi",
            Self::KiroCli => "Kiro",
            Self::Vibe => "Vibe",
            Self::Unknown => "Agent",
        }
    }

    pub fn default_icon(self) -> &'static str {
        match self {
            Self::Claude => "✳",
            Self::Codex => "CX",
            Self::OpenCode => "OC",
            Self::Gemini => "◆",
            Self::Copilot => "CP",
            Self::Pi => "π",
            Self::KiroCli => "K",
            Self::Vibe => "V",
            Self::Unknown => "?",
        }
    }
}

/// Classify an agent from the stored command string.
pub fn classify_agent_command(command: Option<&str>) -> Option<AgentKind> {
    let command = command?.trim();
    if command.is_empty() {
        return None;
    }

    let argv = shlex::split(command)
        .filter(|parts| !parts.is_empty())
        .unwrap_or_else(|| {
            command
                .split_whitespace()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        });

    let executable = argv
        .first()
        .map(|arg| executable_name(arg))
        .unwrap_or_default();
    let executable = normalize(&executable);
    let command_lower = command.to_ascii_lowercase();

    if matches_agent(&executable, &command_lower, &["claude", "claude-code"]) {
        Some(AgentKind::Claude)
    } else if matches_agent(&executable, &command_lower, &["codex"]) {
        Some(AgentKind::Codex)
    } else if matches_agent(&executable, &command_lower, &["opencode", "opencode-ai"]) {
        Some(AgentKind::OpenCode)
    } else if matches_agent(&executable, &command_lower, &["gemini"]) {
        Some(AgentKind::Gemini)
    } else if matches_agent(&executable, &command_lower, &["copilot", "github-copilot"]) {
        Some(AgentKind::Copilot)
    } else if executable == "pi" {
        Some(AgentKind::Pi)
    } else if matches_agent(&executable, &command_lower, &["kiro-cli", "kiro"]) {
        Some(AgentKind::KiroCli)
    } else if executable == "vibe" {
        Some(AgentKind::Vibe)
    } else {
        Some(AgentKind::Unknown)
    }
}

fn executable_name(arg: &str) -> String {
    let path = Path::new(arg);
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| arg.to_string())
}

fn normalize(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .replace('_', "-")
}

fn matches_agent(executable: &str, command: &str, names: &[&str]) -> bool {
    names
        .iter()
        .any(|name| executable == *name || command.contains(name))
}

#[cfg(test)]
mod tests {
    use super::{AgentKind, classify_agent_command};

    #[test]
    fn classifies_known_agent_commands() {
        assert_eq!(
            classify_agent_command(Some("claude --dangerously-skip-permissions")),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            classify_agent_command(Some("/opt/homebrew/bin/codex exec")),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            classify_agent_command(Some("opencode run")),
            Some(AgentKind::OpenCode)
        );
        assert_eq!(
            classify_agent_command(Some("gemini -m gemini-2.5-flash-lite")),
            Some(AgentKind::Gemini)
        );
        assert_eq!(
            classify_agent_command(Some("kiro-cli chat --no-interactive")),
            Some(AgentKind::KiroCli)
        );
        assert_eq!(classify_agent_command(Some("pi")), Some(AgentKind::Pi));
    }

    #[test]
    fn handles_empty_and_unknown_commands() {
        assert_eq!(classify_agent_command(None), None);
        assert_eq!(classify_agent_command(Some("   ")), None);
        assert_eq!(
            classify_agent_command(Some("custom-agent --flag")),
            Some(AgentKind::Unknown)
        );
    }
}
