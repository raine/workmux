//! Agent identity classification.
//!
//! Classifies an agent pane by combining tmux's `pane_current_command` with
//! the pane title. Some agents report a version string (Claude Code: "2.1.118"),
//! a truncated binary name (Codex: "codex-aarch64-a"), or run as a generic
//! interpreter (Gemini, Pi, Vibe). Stem-based profile resolution alone misses
//! these, so the result of `classify_agent_kind` is cached on `AgentState`
//! once it becomes non-None and reused by the sidebar render path.
//!
//! The canonical string form (e.g. "claude", "kiro-cli") matches the existing
//! `AgentProfile::name` so the sidebar can look up the corresponding profile.

use ratatui::style::Color;
use std::path::Path;

const GENERIC_INTERPRETERS: &[&str] = &["node", "python", "python3", "bun", "deno"];

/// Canonical set of agents the sidebar knows how to render.
///
/// Keeping per-variant metadata (icon, label) on this enum forces a compile
/// error in every consumer when a new variant is added, instead of silently
/// falling through to a generic default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Claude,
    Codex,
    OpenCode,
    Gemini,
    Antigravity,
    Pi,
    Omp,
    KiroCli,
    Vibe,
    Copilot,
    Grok,
}

impl AgentKind {
    /// Canonical string form. Matches `AgentProfile::name` and is the value
    /// persisted in `AgentState::agent_kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::OpenCode => "opencode",
            AgentKind::Gemini => "gemini",
            AgentKind::Antigravity => "agy",
            AgentKind::Pi => "pi",
            AgentKind::Omp => "omp",
            AgentKind::KiroCli => "kiro-cli",
            AgentKind::Vibe => "vibe",
            AgentKind::Copilot => "copilot",
            AgentKind::Grok => "grok",
        }
    }

    /// Parse the canonical string form. Round-trips with [`AgentKind::as_str`].
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(AgentKind::Claude),
            "codex" => Some(AgentKind::Codex),
            "opencode" => Some(AgentKind::OpenCode),
            "gemini" => Some(AgentKind::Gemini),
            "agy" => Some(AgentKind::Antigravity),
            "pi" => Some(AgentKind::Pi),
            "omp" => Some(AgentKind::Omp),
            "kiro-cli" => Some(AgentKind::KiroCli),
            "vibe" => Some(AgentKind::Vibe),
            "copilot" => Some(AgentKind::Copilot),
            "grok" => Some(AgentKind::Grok),
            _ => None,
        }
    }

    /// Default sidebar icon. Exhaustive match: adding a variant forces an
    /// update here.
    pub fn default_icon(self) -> &'static str {
        match self {
            AgentKind::Claude => "CC",
            AgentKind::Codex => "CX",
            AgentKind::OpenCode => "OC",
            AgentKind::Gemini => "G",
            AgentKind::Antigravity => "⋂",
            AgentKind::Pi => "π",
            AgentKind::Omp => "⌥",
            AgentKind::KiroCli => "K",
            AgentKind::Vibe => "V",
            AgentKind::Copilot => "CP",
            AgentKind::Grok => "GK",
        }
    }

    /// Default sidebar label. Exhaustive match: adding a variant forces an
    /// update here.
    pub fn default_label(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude",
            AgentKind::Codex => "Codex",
            AgentKind::OpenCode => "OpenCode",
            AgentKind::Gemini => "Gemini",
            AgentKind::Antigravity => "Antigravity",
            AgentKind::Pi => "Pi",
            AgentKind::Omp => "Oh My Pi",
            AgentKind::KiroCli => "Kiro",
            AgentKind::Vibe => "Vibe",
            AgentKind::Copilot => "Copilot",
            AgentKind::Grok => "Grok",
        }
    }

    /// Default sidebar icon foreground color. Brand-true mid-luminance hex
    /// values that read on both shipped light and dark palettes.
    ///
    /// Sources: Anthropic, OpenAI, Google, GitHub, Mistral brand sheets;
    /// Pi sampled from product UI. `None` means "fall through to
    /// `palette.text`".
    pub fn default_color(self) -> Option<Color> {
        Some(match self {
            AgentKind::Claude => Color::Rgb(0xd9, 0x77, 0x57),
            AgentKind::Codex => Color::Rgb(0x10, 0xa3, 0x7f),
            AgentKind::Gemini => Color::Rgb(0x07, 0x8e, 0xfa),
            AgentKind::Antigravity => Color::Rgb(0x89, 0x57, 0xe5),
            AgentKind::Copilot => Color::Rgb(0x89, 0x57, 0xe5),
            AgentKind::Vibe => Color::Rgb(0xff, 0x82, 0x05),
            AgentKind::Pi => Color::Rgb(0x96, 0xbb, 0xb5),
            AgentKind::Omp => Color::Rgb(0xe0, 0x57, 0x35),
            AgentKind::OpenCode => Color::Blue,
            AgentKind::KiroCli => return None,
            AgentKind::Grok => Color::Rgb(0x1d, 0x9b, 0xf0),
        })
    }
}

/// Classify an agent pane using its foreground command and pane title.
///
/// Returns the canonical profile name (e.g. "claude") or `None` if no rule
/// matches. Callers cache the first non-None result to avoid re-classifying
/// on every tick.
pub fn classify_agent_kind(command: Option<&str>, pane_title: Option<&str>) -> Option<String> {
    classify_agent_kind_enum(command, pane_title).map(|k| k.as_str().to_string())
}

fn classify_agent_kind_enum(command: Option<&str>, pane_title: Option<&str>) -> Option<AgentKind> {
    let raw = command.unwrap_or("").trim();
    let stem = command_stem(raw);

    if let Some(kind) = classify_by_command(raw, &stem) {
        return Some(kind);
    }

    if is_generic_interpreter(&stem)
        && let Some(kind) = classify_by_title(pane_title.unwrap_or(""))
    {
        return Some(kind);
    }

    None
}

fn classify_by_command(raw: &str, stem: &str) -> Option<AgentKind> {
    if stem.is_empty() {
        return None;
    }

    if is_version_string(stem) || is_version_string(raw) {
        return Some(AgentKind::Claude);
    }

    if stem == "codex" || stem.starts_with("codex-") {
        return Some(AgentKind::Codex);
    }

    AgentKind::from_str(stem)
}

fn classify_by_title(title: &str) -> Option<AgentKind> {
    if title.contains("Claude Code") {
        return Some(AgentKind::Claude);
    }
    if title.contains("opencode") {
        return Some(AgentKind::OpenCode);
    }
    if title.contains("Gemini") || title.contains('\u{25C7}') {
        return Some(AgentKind::Gemini);
    }
    if title.contains('\u{03C0}') {
        return Some(AgentKind::Pi);
    }
    if title.contains("Oh My Pi") || contains_omp_ascii_case_insensitive(title) {
        return Some(AgentKind::Omp);
    }
    if title.contains("Vibe") {
        return Some(AgentKind::Vibe);
    }
    None
}

fn contains_omp_ascii_case_insensitive(title: &str) -> bool {
    title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token.eq_ignore_ascii_case("omp"))
}

fn is_generic_interpreter(stem: &str) -> bool {
    let lower = stem.to_ascii_lowercase();
    GENERIC_INTERPRETERS.iter().any(|i| *i == lower)
}

fn is_version_string(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut has_dot = false;
    let mut prev_dot = true;
    for c in s.chars() {
        if c == '.' {
            if prev_dot {
                return false;
            }
            has_dot = true;
            prev_dot = true;
        } else if c.is_ascii_digit() {
            prev_dot = false;
        } else {
            return false;
        }
    }
    has_dot && !prev_dot
}

fn command_stem(command: &str) -> String {
    let token = command.split_whitespace().next().unwrap_or("");
    Path::new(token)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(token)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(cmd: &str, title: &str) -> Option<String> {
        classify_agent_kind(Some(cmd), Some(title))
    }

    #[test]
    fn version_string_matches_claude() {
        assert_eq!(classify("2.1.118", ""), Some("claude".into()));
        assert_eq!(classify("2.1.111", "✳ task"), Some("claude".into()));
        assert_eq!(classify("3.0.0.1", ""), Some("claude".into()));
    }

    #[test]
    fn codex_truncated_binary_matches() {
        assert_eq!(classify("codex-aarch64-a", ""), Some("codex".into()));
        assert_eq!(classify("codex", ""), Some("codex".into()));
    }

    #[test]
    fn opencode_exact_command() {
        assert_eq!(classify("opencode", "⠹ opencode"), Some("opencode".into()));
    }

    #[test]
    fn kiro_and_copilot_match() {
        assert_eq!(classify("kiro-cli", ""), Some("kiro-cli".into()));
        assert_eq!(classify("copilot", ""), Some("copilot".into()));
    }

    #[test]
    fn grok_command_matches() {
        assert_eq!(classify("grok", ""), Some("grok".into()));
        assert_eq!(classify("/usr/local/bin/grok", ""), Some("grok".into()));
        assert_eq!(classify("grok", "anything"), Some("grok".into()));
        assert_eq!(classify("grok-build", ""), None);
        assert_eq!(classify("grokster", ""), None);
    }

    #[test]
    fn direct_stem_matches_for_known_binaries() {
        assert_eq!(classify("claude", ""), Some("claude".into()));
        assert_eq!(classify("gemini", ""), Some("gemini".into()));
        assert_eq!(classify("agy", ""), Some("agy".into()));
        assert_eq!(classify("pi", ""), Some("pi".into()));
        assert_eq!(classify("omp", ""), Some("omp".into()));
        assert_eq!(classify("vibe", ""), Some("vibe".into()));
        assert_eq!(classify("grok", ""), Some("grok".into()));
    }

    #[test]
    fn absolute_path_is_normalized() {
        assert_eq!(classify("/usr/local/bin/claude", ""), Some("claude".into()));
        assert_eq!(classify("/usr/bin/agy", ""), Some("agy".into()));
        assert_eq!(classify("/opt/codex-aarch64-a", ""), Some("codex".into()));
    }

    #[test]
    fn node_with_claude_title() {
        assert_eq!(
            classify("node", "Claude Code 2.1.0 - foo"),
            Some("claude".into())
        );
    }

    #[test]
    fn node_with_gemini_title() {
        assert_eq!(
            classify("node", "\u{25C7}  Ready (sidebar-templates)"),
            Some("gemini".into())
        );
        assert_eq!(classify("node", "Gemini - working"), Some("gemini".into()));
    }

    #[test]
    fn node_with_pi_title() {
        assert_eq!(
            classify("node", "\u{03C0} - sidebar-templates"),
            Some("pi".into())
        );
    }

    #[test]
    fn generic_interpreter_with_omp_title() {
        assert_eq!(classify("bun", "omp"), Some("omp".into()));
        assert_eq!(classify("bun", "OMP"), Some("omp".into()));
        assert_eq!(classify("bun", "agent: omp"), Some("omp".into()));
        assert_eq!(classify("python3", "Oh My Pi"), Some("omp".into()));
    }

    #[test]
    fn generic_interpreter_with_omp_substring_title_does_not_match() {
        assert_eq!(classify("bun", "component-tests"), None);
        assert_eq!(classify("node", "compile server"), None);
        assert_eq!(classify("python3", "company dashboard"), None);
        assert_eq!(classify("python3", "completion worker"), None);
    }

    #[test]
    fn python_with_vibe_title() {
        assert_eq!(classify("Python", "Vibe"), Some("vibe".into()));
        assert_eq!(classify("python3", "Vibe agent"), Some("vibe".into()));
    }

    #[test]
    fn opencode_via_node_title() {
        assert_eq!(
            classify("node", "⠹ opencode session"),
            Some("opencode".into())
        );
    }

    #[test]
    fn empty_command_returns_none() {
        assert_eq!(classify_agent_kind(None, None), None);
        assert_eq!(classify("", ""), None);
        assert_eq!(classify("", "Vibe"), None);
    }

    #[test]
    fn unknown_command_returns_none() {
        assert_eq!(classify("zsh", ""), None);
        assert_eq!(classify("vim", "some title"), None);
        // Bare prefix collisions with "codex" must not match.
        assert_eq!(classify("codexploitation", ""), None);
        assert_eq!(classify("codex2", ""), None);
    }

    #[test]
    fn generic_interpreter_no_matching_title_returns_none() {
        assert_eq!(classify("node", "random title"), None);
        assert_eq!(classify("Python", "no match"), None);
    }

    #[test]
    fn version_string_negative_cases() {
        assert!(!is_version_string(""));
        assert!(!is_version_string("2"));
        assert!(!is_version_string("2."));
        assert!(!is_version_string(".2"));
        assert!(!is_version_string("2..1"));
        assert!(!is_version_string("2.1a"));
        assert!(is_version_string("2.1"));
        assert!(is_version_string("2.1.118"));
    }

    /// Every variant has a non-empty icon and label; the classifier produces
    /// a string that round-trips back to the same variant. Catches forgetting
    /// to fill in metadata or string form when adding a variant.
    #[test]
    fn every_variant_has_metadata_and_round_trips() {
        let all = [
            AgentKind::Claude,
            AgentKind::Codex,
            AgentKind::OpenCode,
            AgentKind::Gemini,
            AgentKind::Pi,
            AgentKind::Omp,
            AgentKind::KiroCli,
            AgentKind::Vibe,
            AgentKind::Copilot,
            AgentKind::Grok,
        ];
        for kind in all {
            assert!(!kind.default_icon().is_empty(), "{:?} icon empty", kind);
            assert!(!kind.default_label().is_empty(), "{:?} label empty", kind);
            assert_eq!(AgentKind::from_str(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn from_str_rejects_unknown() {
        assert_eq!(AgentKind::from_str(""), None);
        assert_eq!(AgentKind::from_str("not-a-profile"), None);
        assert_eq!(AgentKind::from_str("Claude"), None); // case-sensitive
    }
}
