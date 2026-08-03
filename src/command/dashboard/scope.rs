//! Scope filter for the dashboard agent list.

use crate::multiplexer::AgentPane;
use crate::state::StateStore;

/// Whether the dashboard shows all agents or only those in the current session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScopeMode {
    /// Show all agents across all sessions
    #[default]
    All,
    /// Show only agents in the current session
    Session,
}

impl ScopeMode {
    /// Toggle between All and Session
    pub fn toggle(self) -> Self {
        match self {
            ScopeMode::All => ScopeMode::Session,
            ScopeMode::Session => ScopeMode::All,
        }
    }

    /// Get the display label for the scope mode
    pub fn label(&self) -> &'static str {
        match self {
            ScopeMode::All => "all",
            ScopeMode::Session => "session",
        }
    }

    /// Convert to string for storage.
    fn as_str(&self) -> &'static str {
        match self {
            ScopeMode::All => "all",
            ScopeMode::Session => "session",
        }
    }

    /// Parse from storage string.
    fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "session" => ScopeMode::Session,
            _ => ScopeMode::All,
        }
    }

    /// Retain agents visible within this scope.
    pub fn retain_agents(&self, agents: &mut Vec<AgentPane>, session: Option<&str>) {
        if *self == ScopeMode::Session {
            agents.retain(|agent| session.is_some_and(|session| agent.session == session));
        }
    }

    /// Load scope mode from StateStore.
    pub fn load() -> Self {
        StateStore::new()
            .ok()
            .and_then(|store| store.load_settings().ok())
            .and_then(|s| s.dashboard_scope)
            .map(|s| Self::from_str(&s))
            .unwrap_or_default()
    }

    /// Save scope mode to StateStore.
    pub fn save(&self) {
        if let Ok(store) = StateStore::new()
            && let Ok(mut settings) = store.load_settings()
        {
            settings.dashboard_scope = Some(self.as_str().to_string());
            let _ = store.save_settings(&settings);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn agent(pane_id: &str, session: &str, path: &str, status_ts: Option<u64>) -> AgentPane {
        AgentPane {
            session: session.to_string(),
            window_name: format!("wm-{pane_id}"),
            pane_id: pane_id.to_string(),
            window_id: String::new(),
            window_index: None,
            path: PathBuf::from(path),
            pane_title: None,
            status: None,
            status_ts,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
        }
    }

    #[test]
    fn test_toggle() {
        assert_eq!(ScopeMode::All.toggle(), ScopeMode::Session);
        assert_eq!(ScopeMode::Session.toggle(), ScopeMode::All);
    }

    #[test]
    fn test_labels() {
        assert_eq!(ScopeMode::All.label(), "all");
        assert_eq!(ScopeMode::Session.label(), "session");
    }

    #[test]
    fn test_roundtrip_strings() {
        for mode in [ScopeMode::All, ScopeMode::Session] {
            assert_eq!(ScopeMode::from_str(mode.as_str()), mode);
        }
    }

    #[test]
    fn test_from_str_defaults_to_all() {
        assert_eq!(ScopeMode::from_str(""), ScopeMode::All);
        assert_eq!(ScopeMode::from_str("unknown"), ScopeMode::All);
        assert_eq!(ScopeMode::from_str("project"), ScopeMode::All);
    }

    #[test]
    fn test_from_str_case_insensitive() {
        assert_eq!(ScopeMode::from_str("Session"), ScopeMode::Session);
        assert_eq!(ScopeMode::from_str("SESSION"), ScopeMode::Session);
    }

    #[test]
    fn session_scope_uses_session_identity_only() {
        let mut agents = vec![
            agent(
                "%1",
                "claude-codex-proxy",
                "/code/claude-codex-proxy",
                Some(1),
            ),
            agent(
                "%2",
                "claude-codex-proxy",
                "/code/claude-codex-proxy__worktrees/feature",
                None,
            ),
            agent("%3", "workmux", "/code/workmux", None),
            agent("%4", "raine", "/Users/raine", None),
            agent("%5", "aven", "/code/aven", None),
            agent("%6", "WalkingMate", "/code/WalkingMate", None),
        ];

        ScopeMode::Session.retain_agents(&mut agents, Some("claude-codex-proxy"));

        assert_eq!(
            agents
                .iter()
                .map(|agent| agent.pane_id.as_str())
                .collect::<Vec<_>>(),
            vec!["%1", "%2"]
        );
    }

    #[test]
    fn session_scope_without_session_identity_shows_no_agents() {
        let mut agents = vec![
            agent("%1", "workmux", "/code/workmux", None),
            agent("%2", "aven", "/code/aven", None),
        ];

        ScopeMode::Session.retain_agents(&mut agents, None);

        assert!(agents.is_empty());
    }

    #[test]
    fn all_scope_ignores_session_identity() {
        let mut agents = vec![
            agent("%1", "workmux", "/code/workmux", None),
            agent("%2", "aven", "/code/aven", None),
        ];

        ScopeMode::All.retain_agents(&mut agents, None);

        assert_eq!(agents.len(), 2);
    }

    #[test]
    fn scope_cycle_restores_all_agents() {
        let all_agents = vec![
            agent("%1", "claude-codex-proxy", "/code/proxy", None),
            agent("%2", "workmux", "/code/workmux", None),
        ];
        let mut mode = ScopeMode::All.toggle();
        let mut visible = all_agents.clone();
        mode.retain_agents(&mut visible, Some("claude-codex-proxy"));
        assert_eq!(visible.len(), 1);

        mode = mode.toggle();
        visible = all_agents;
        mode.retain_agents(&mut visible, Some("claude-codex-proxy"));
        assert_eq!(visible.len(), 2);
    }
}
