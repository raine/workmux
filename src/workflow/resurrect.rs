use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use tracing::info;

use crate::agent_identity::AgentKind;
use crate::config::{self, MuxMode};
use crate::git;
use crate::multiplexer::{self, Multiplexer};
use crate::state::{AgentState, PaneKey, StateStore};
use crate::util::canon_or_self;

#[derive(Debug)]
pub enum ResurrectAction {
    Restore,
    SkipAlreadyOpen,
    SkipMain,
}

#[derive(Debug)]
pub struct ResurrectCandidate {
    pub handle: String,
    pub action: ResurrectAction,
    pub stale_pane_keys: Vec<PaneKey>,
    pub mode: MuxMode,
    pub agent: Option<String>,
}

pub struct ResurrectPlan {
    pub candidates: Vec<ResurrectCandidate>,
    pub unmatched_states: usize,
}

type ResurrectAgentChoice = (String, u64, u8);
type ResurrectHandleState = (MuxMode, Option<ResurrectAgentChoice>, Vec<PaneKey>);

fn resurrect_agent(agent: &AgentState) -> Option<(String, u8)> {
    if let Some(kind) = agent.agent_kind.as_deref()
        && AgentKind::from_str(kind).is_some()
    {
        return Some((kind.to_string(), 1));
    }

    let profile = multiplexer::agent::resolve_profile(Some(&agent.command));
    if profile.name() != "default" {
        return Some((profile.name().to_string(), 0));
    }

    None
}

fn update_selected_agent(selected: &mut Option<ResurrectAgentChoice>, agent: &AgentState) {
    let Some((command, source_rank)) = resurrect_agent(agent) else {
        return;
    };
    let restored_agent = (command, agent.updated_ts, source_rank);

    let should_replace = match selected.as_ref() {
        None => true,
        Some((selected_command, selected_ts, selected_rank)) => {
            restored_agent.1 > *selected_ts
                || (restored_agent.1 == *selected_ts
                    && (restored_agent.2 > *selected_rank
                        || (restored_agent.2 == *selected_rank
                            && restored_agent.0 < *selected_command)))
        }
    };

    if should_replace {
        *selected = Some(restored_agent);
    }
}

/// Build a plan of what to restore based on stale agent state files.
///
/// Loads raw (non-reconciled) agent states and cross-references them against
/// existing git worktrees and live multiplexer state to determine which
/// worktrees need restoration.
pub fn plan(store: &StateStore, mux: &dyn Multiplexer) -> Result<ResurrectPlan> {
    let all_agents = store.list_all_agents()?;
    let backend = mux.name();
    let instance = mux.instance_id();

    info!(
        total_state_files = all_agents.len(),
        backend, instance, "resurrect:plan loading agent state files"
    );

    // Filter to current backend/instance
    let relevant: Vec<_> = all_agents
        .into_iter()
        .filter(|a| a.pane_key.backend == backend && a.pane_key.instance == instance)
        .collect();

    info!(
        relevant_count = relevant.len(),
        "resurrect:plan filtered to current backend/instance"
    );

    // Get worktrees for current repo
    let worktrees = git::list_worktrees()?;
    let main_root = git::get_main_worktree_root()?;
    let canon_main = canon_or_self(&main_root);

    // Build canonical worktree map: (canon_path, handle)
    let wt_map: Vec<(PathBuf, String)> = worktrees
        .iter()
        .map(|(path, _branch)| {
            let handle = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            (canon_or_self(path), handle)
        })
        .collect();

    // Get live mux state for skip detection
    let mux_windows = mux.get_all_window_names()?;
    let mux_sessions = mux.get_all_session_names()?;
    let config = config::Config::load(None)?;
    let prefix = config.window_prefix();

    // Use config default mode as fallback for worktrees with no stored mode,
    // matching the resolution logic in workflow::open
    let default_mode = config.mode();

    // Group agent states by matched worktree handle
    let mut by_handle: HashMap<String, ResurrectHandleState> = HashMap::new();
    let mut unmatched_states = 0usize;

    for agent in relevant {
        let canon_agent = canon_or_self(&agent.workdir);

        // Find matching worktree using descendant path matching
        // (agent workdir may be a subdirectory of the worktree root)
        let matched = wt_map
            .iter()
            .find(|(canon_wt, _)| canon_agent == *canon_wt || canon_agent.starts_with(canon_wt));

        match matched {
            Some((_canon_wt, handle)) => {
                info!(
                    pane_id = %agent.pane_key.pane_id,
                    workdir = %agent.workdir.display(),
                    handle,
                    boot_id = ?agent.boot_id,
                    status = ?agent.status,
                    "resurrect:plan matched agent to worktree"
                );
                let mode = git::get_worktree_mode_opt(handle).unwrap_or(default_mode);
                let entry = by_handle
                    .entry(handle.clone())
                    .or_insert_with(|| (mode, None, Vec::new()));
                update_selected_agent(&mut entry.1, &agent);
                entry.2.push(agent.pane_key);
            }
            None => {
                info!(
                    pane_id = %agent.pane_key.pane_id,
                    workdir = %agent.workdir.display(),
                    "resurrect:plan no matching worktree (other project or removed)"
                );
                unmatched_states += 1;
            }
        }
    }

    // Determine action per handle
    let mut candidates = Vec::new();
    for (handle, (mode, agent, pane_keys)) in by_handle {
        let canon_wt = wt_map
            .iter()
            .find(|(_, h)| *h == handle)
            .map(|(p, _)| p.clone())
            .unwrap_or_default();

        let action = if canon_wt == canon_main {
            ResurrectAction::SkipMain
        } else {
            let target_name = if mode == MuxMode::Session {
                git::get_worktree_target_session(&handle).unwrap_or_else(|| handle.clone())
            } else {
                git::get_worktree_target_window(&handle).unwrap_or_else(|| handle.clone())
            };
            let prefixed = crate::multiplexer::util::prefixed(prefix, &target_name);
            let is_open = if mode == MuxMode::Session {
                mux_sessions.contains(&prefixed)
            } else if mux.supports_window_ownership() {
                match git::get_worktree_window_token(&handle) {
                    Some(token) => !mux
                        .resolve_owned_window_targets(
                            &token,
                            &prefixed,
                            git::get_worktree_window_session(&handle).as_deref(),
                            &canon_wt,
                        )?
                        .is_empty(),
                    None => mux_windows.contains(&prefixed),
                }
            } else {
                mux_windows.contains(&prefixed)
            };
            if is_open {
                ResurrectAction::SkipAlreadyOpen
            } else {
                ResurrectAction::Restore
            }
        };

        let agent = agent.map(|(command, _updated_ts, _source_rank)| command);

        info!(
            handle,
            action = ?action,
            mode = ?mode,
            agent = agent.as_deref(),
            pane_count = pane_keys.len(),
            "resurrect:plan determined action for handle"
        );

        candidates.push(ResurrectCandidate {
            handle,
            action,
            stale_pane_keys: pane_keys,
            mode,
            agent,
        });
    }

    // Sort by handle for deterministic output
    candidates.sort_by(|a, b| a.handle.cmp(&b.handle));

    Ok(ResurrectPlan {
        candidates,
        unmatched_states,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent_state(command: &str, agent_kind: Option<&str>, updated_ts: u64) -> AgentState {
        AgentState {
            pane_key: PaneKey {
                backend: "tmux".to_string(),
                instance: "default".to_string(),
                pane_id: format!("%{updated_ts}"),
            },
            workdir: PathBuf::from("/repo/worktree"),
            status: None,
            status_ts: None,
            pane_title: None,
            pane_pid: 12345,
            command: command.to_string(),
            updated_ts,
            window_name: None,
            session_name: None,
            boot_id: None,
            agent_kind: agent_kind.map(|kind| kind.to_string()),
            agent_session_id: None,
        }
    }

    #[test]
    fn resurrect_agent_prefers_valid_agent_kind() {
        let state = test_agent_state("node", Some("codex"), 1);

        assert_eq!(resurrect_agent(&state), Some(("codex".to_string(), 1)));
    }

    #[test]
    fn resurrect_agent_uses_known_foreground_command() {
        let state = test_agent_state("codex --yolo", None, 1);

        assert_eq!(resurrect_agent(&state), Some(("codex".to_string(), 0)));
    }

    #[test]
    fn resurrect_agent_ignores_unknown_foreground_command() {
        let state = test_agent_state("node", None, 1);

        assert_eq!(resurrect_agent(&state), None);
    }

    #[test]
    fn update_selected_agent_keeps_newest_valid_agent() {
        let older = test_agent_state("claude", None, 10);
        let newer_invalid = test_agent_state("node", None, 30);
        let newer = test_agent_state("codex", None, 20);
        let mut selected = None;

        update_selected_agent(&mut selected, &older);
        update_selected_agent(&mut selected, &newer_invalid);
        update_selected_agent(&mut selected, &newer);

        assert_eq!(selected, Some(("codex".to_string(), 20, 0)));
    }

    #[test]
    fn update_selected_agent_breaks_ties_deterministically() {
        let command_state = test_agent_state("codex", None, 10);
        let kind_state = test_agent_state("node", Some("claude"), 10);
        let mut selected = None;

        update_selected_agent(&mut selected, &command_state);
        update_selected_agent(&mut selected, &kind_state);

        assert_eq!(selected, Some(("claude".to_string(), 10, 1)));
    }
}
