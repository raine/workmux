use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};

use crate::multiplexer::{AgentPane, AgentStatus, Multiplexer, create_backend, detect_backend};
use crate::state::{PaneKey, StateStore};
use crate::util;

pub fn run(older_than_secs: u64, force: bool) -> Result<()> {
    let mux = create_backend(detect_backend());
    let store = StateStore::new()?;
    let agents = store.load_reconciled_agents(mux.as_ref())?;
    let now = now_secs();
    let old_agents = old_agents(&agents, now, older_than_secs);

    if old_agents.is_empty() {
        println!(
            "No agents older than {}",
            util::format_elapsed_secs(older_than_secs)
        );
        return Ok(());
    }

    let backend = mux.name().to_string();
    let instance = mux.instance_id();
    let mut failures = Vec::new();

    for agent in &old_agents {
        let age = last_activity_ts(agent)
            .map(|ts| util::format_elapsed_secs(now.saturating_sub(ts)))
            .unwrap_or_else(|| "unknown".to_string());
        let worktree = worktree_name(agent);
        let status = status_label(agent.status);
        let title = agent.pane_title.as_deref().unwrap_or("-");

        if force {
            match exit_agent(mux.as_ref(), agent) {
                Ok(()) => {
                    let key = PaneKey {
                        backend: backend.clone(),
                        instance: instance.clone(),
                        pane_id: agent.pane_id.clone(),
                    };
                    match store.delete_agent(&key) {
                        Ok(()) => println!(
                            "Exited {} in {} ({}, {}, {})",
                            agent.pane_id, worktree, age, status, title
                        ),
                        Err(error) => {
                            println!(
                                "Exited {} in {} but failed to remove state: {}",
                                agent.pane_id, worktree, error
                            );
                            failures.push(agent.pane_id.clone());
                        }
                    }
                }
                Err(error) => {
                    println!(
                        "Failed to exit {} in {}: {}",
                        agent.pane_id, worktree, error
                    );
                    failures.push(agent.pane_id.clone());
                }
            }
        } else {
            println!(
                "Would exit {} in {} ({}, {}, {})",
                agent.pane_id, worktree, age, status, title
            );
        }
    }

    if !force {
        println!("Run with -f/--force to exit these agents.");
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("failed to exit {} agent(s)", failures.len()))
    }
}

fn exit_agent(mux: &dyn Multiplexer, agent: &AgentPane) -> Result<()> {
    let original_command = mux
        .get_live_pane_info(&agent.pane_id)?
        .and_then(|live| live.current_command);

    mux.send_key(&agent.pane_id, "C-c")?;
    if wait_for_agent_exit(mux, &agent.pane_id, original_command.as_deref())? {
        return Ok(());
    }

    mux.send_key(&agent.pane_id, "C-d")?;
    if wait_for_agent_exit(mux, &agent.pane_id, original_command.as_deref())? {
        return Ok(());
    }

    Err(anyhow!("agent did not exit after C-c and C-d"))
}

fn wait_for_agent_exit(
    mux: &dyn Multiplexer,
    pane_id: &str,
    original_command: Option<&str>,
) -> Result<bool> {
    for _ in 0..20 {
        thread::sleep(Duration::from_millis(100));
        let Some(live) = mux.get_live_pane_info(pane_id)? else {
            return Ok(true);
        };
        if live.current_command.as_deref() != original_command {
            return Ok(true);
        }
    }
    Ok(false)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn last_activity_ts(agent: &AgentPane) -> Option<u64> {
    agent.updated_ts.or(agent.status_ts)
}

fn old_agents(agents: &[AgentPane], now: u64, older_than_secs: u64) -> Vec<&AgentPane> {
    agents
        .iter()
        .filter(|agent| {
            last_activity_ts(agent).is_some_and(|ts| now.saturating_sub(ts) >= older_than_secs)
        })
        .collect()
}

fn worktree_name(agent: &AgentPane) -> &str {
    agent
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
}

fn status_label(status: Option<AgentStatus>) -> &'static str {
    match status {
        Some(AgentStatus::Working) => "working",
        Some(AgentStatus::Waiting) => "waiting",
        Some(AgentStatus::Done) => "done",
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn agent(pane_id: &str, updated_ts: Option<u64>, status_ts: Option<u64>) -> AgentPane {
        AgentPane {
            session: "session".to_string(),
            window_name: "window".to_string(),
            pane_id: pane_id.to_string(),
            window_id: String::new(),
            window_index: None,
            path: PathBuf::from("/repo/worktree"),
            pane_title: None,
            status: None,
            status_ts,
            updated_ts,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
        }
    }

    #[test]
    fn old_agents_selects_agents_at_or_above_threshold() {
        let agents = vec![
            agent("%1", Some(100), None),
            agent("%2", Some(200), None),
            agent("%3", Some(201), None),
        ];

        let selected: Vec<&str> = old_agents(&agents, 300, 100)
            .into_iter()
            .map(|agent| agent.pane_id.as_str())
            .collect();

        assert_eq!(selected, vec!["%1", "%2"]);
    }

    #[test]
    fn old_agents_falls_back_to_status_ts() {
        let agents = vec![agent("%1", None, Some(100)), agent("%2", None, Some(201))];

        let selected: Vec<&str> = old_agents(&agents, 300, 100)
            .into_iter()
            .map(|agent| agent.pane_id.as_str())
            .collect();

        assert_eq!(selected, vec!["%1"]);
    }

    #[test]
    fn old_agents_prefers_updated_ts_over_status_ts() {
        let agents = vec![agent("%1", Some(250), Some(100))];

        assert!(old_agents(&agents, 300, 100).is_empty());
    }

    #[test]
    fn old_agents_ignores_agents_without_timestamps() {
        let agents = vec![agent("%1", None, None)];

        assert!(old_agents(&agents, 300, 100).is_empty());
    }

    #[test]
    fn old_agents_handles_future_timestamps() {
        let agents = vec![agent("%1", Some(400), None)];

        assert!(old_agents(&agents, 300, 100).is_empty());
    }
}
