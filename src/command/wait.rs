use std::collections::HashSet;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::config::Config;
use crate::git;
use crate::multiplexer::{AgentStatus, create_backend, detect_backend};
use crate::remote::{self, cache::WorkspaceRecord, node::observe::ObservedAgent};
use crate::state::StateStore;
use crate::util;
use crate::workflow;

/// Resolve a worktree name to its path, trying local git first then global agents.
///
/// Local resolution is preferred because it works even before an agent starts
/// (the worktree directory exists from `workmux add`). Global resolution requires
/// a running agent.
fn resolve_worktree_path(
    name: &str,
    mux: &dyn crate::multiplexer::Multiplexer,
) -> Result<std::path::PathBuf> {
    // Try local git resolution first (supports waiting for unstarted agents)
    if git::is_git_repo().unwrap_or(false) {
        match git::find_worktree(name) {
            Ok((path, _branch)) => return Ok(path),
            Err(e) if e.downcast_ref::<git::WorktreeNotFound>().is_some() => {}
            Err(e) => return Err(e),
        }
    }

    // Fall back to global agent resolution
    let (path, _agents) = workflow::resolve_worktree_agents(name, mux)?;
    Ok(path)
}

fn parse_status(s: &str) -> Result<AgentStatus> {
    match s {
        "working" => Ok(AgentStatus::Working),
        "waiting" => Ok(AgentStatus::Waiting),
        "done" => Ok(AgentStatus::Done),
        _ => Err(anyhow!(
            "Invalid status '{}'. Must be: working, waiting, done",
            s
        )),
    }
}

pub fn run(
    worktree_names: &[String],
    target_status: &str,
    timeout_secs: Option<u64>,
    any: bool,
) -> Result<()> {
    let target = parse_status(target_status)?;
    let mux = create_backend(detect_backend());
    let start = Instant::now();

    // Remote workspaces are watched through their node; the rest resolve
    // to paths upfront (local git first, then global agents).
    let mut remote_targets: Vec<(String, WorkspaceRecord)> = Vec::new();
    let mut worktree_paths = Vec::new();
    for name in worktree_names {
        match remote::agents::find_target(name) {
            Some(record) => remote_targets.push((name.clone(), record)),
            None => worktree_paths.push((name.clone(), resolve_worktree_path(name, mux.as_ref())?)),
        }
    }
    let config = if remote_targets.is_empty() {
        Config::default()
    } else {
        Config::load(None).unwrap_or_default()
    };
    let mut remotes: Vec<String> = remote_targets
        .iter()
        .map(|(_, record)| record.remote.clone())
        .collect();
    remotes.sort();
    remotes.dedup();
    remote::observe::spawn(&config, &remotes);

    let mut reached: HashSet<String> = HashSet::new();
    let mut seen_agent: HashSet<String> = HashSet::new();

    loop {
        // Check timeout
        if let Some(timeout) = timeout_secs
            && start.elapsed() > Duration::from_secs(timeout)
        {
            let remaining: Vec<_> = worktree_names
                .iter()
                .filter(|n| !reached.contains(n.as_str()))
                .collect();
            eprintln!(
                "Timeout waiting for: {}",
                remaining
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            std::process::exit(1);
        }

        for (name, record) in &remote_targets {
            if reached.contains(name) {
                continue;
            }
            match remote_progress(record, target) {
                RemoteProgress::Unknown => {}
                RemoteProgress::Running => {
                    seen_agent.insert(name.clone());
                }
                RemoteProgress::Reached => {
                    seen_agent.insert(name.clone());
                    let elapsed = util::format_elapsed_duration(start.elapsed());
                    eprintln!("{}: {} ({})", name, target_status, elapsed);
                    reached.insert(name.clone());
                    if any {
                        return Ok(());
                    }
                }
                RemoteProgress::Removed => {
                    let elapsed = util::format_elapsed_duration(start.elapsed());
                    eprintln!("{}: removed ({})", name, elapsed);
                    reached.insert(name.clone());
                    if any {
                        return Ok(());
                    }
                }
                RemoteProgress::Gone if seen_agent.contains(name) => {
                    eprintln!("{}: agent exited unexpectedly", name);
                    std::process::exit(3);
                }
                RemoteProgress::Gone => {}
            }
        }
        if reached.len() == worktree_names.len() {
            return Ok(());
        }
        if worktree_paths.is_empty() {
            thread::sleep(Duration::from_secs(2));
            continue;
        }

        // Load current agent state
        let agent_panes =
            StateStore::new().and_then(|store| store.load_reconciled_agents(mux.as_ref()))?;

        for (name, wt_path) in &worktree_paths {
            if reached.contains(name) {
                continue;
            }

            let matching = workflow::match_agents_to_worktree(&agent_panes, wt_path);

            if !matching.is_empty() {
                seen_agent.insert(name.clone());

                // Check if ANY agent in this worktree has reached the target status
                let has_target = matching.iter().any(|a| a.status == Some(target));
                if has_target {
                    let elapsed = util::format_elapsed_duration(start.elapsed());
                    eprintln!("{}: {} ({})", name, target_status, elapsed);
                    reached.insert(name.clone());

                    if any {
                        return Ok(());
                    }
                }
            } else if seen_agent.contains(name) {
                // Agent was previously running but disappeared
                // Check if worktree still exists - if not, it was merged (success)
                if !wt_path.exists() {
                    let elapsed = util::format_elapsed_duration(start.elapsed());
                    eprintln!("{}: merged ({})", name, elapsed);
                    reached.insert(name.clone());

                    if any {
                        return Ok(());
                    }
                } else {
                    // Worktree exists but agent gone - crashed/exited unexpectedly
                    eprintln!("{}: agent exited unexpectedly", name);
                    std::process::exit(3);
                }
            }
            // If we haven't seen an agent yet and it's been > 10s, still wait --
            // the agent may not have started yet. The timeout flag handles the
            // overall deadline.
        }

        // Check if all have reached target
        if reached.len() == worktree_names.len() {
            return Ok(());
        }

        thread::sleep(Duration::from_secs(2));
    }
}

/// Where a remote workspace's agents stand, from the latest observation.
enum RemoteProgress {
    /// The remote is not observed right now; keep waiting.
    Unknown,
    Running,
    Reached,
    /// No agent runs in the workspace.
    Gone,
    /// The workspace was removed.
    Removed,
}

fn remote_progress(record: &WorkspaceRecord, target: AgentStatus) -> RemoteProgress {
    let current = remote::cache::all()
        .into_iter()
        .find(|r| r.remote == record.remote && r.ws_id == record.ws_id && !r.cleanup_pending());
    let Some(current) = current else {
        return RemoteProgress::Removed;
    };
    let Some(execution) = &current.execution else {
        return RemoteProgress::Gone;
    };
    let Some(observation) = remote::observe::load_connected(&current.remote) else {
        return RemoteProgress::Unknown;
    };
    let Some(workspace) = observation.execution(&current.ws_id, &execution.ex_id) else {
        return RemoteProgress::Unknown;
    };
    // After a send, only the agent sent to counts, and until the fence
    // lapses only the turn the send started.
    let sent = remote::cache::sent_turn(&current.remote, &current.ws_id)
        .filter(|turn| turn.ex_id == execution.ex_id);
    let fence = sent
        .as_ref()
        .filter(|turn| !turn.lapsed(remote::cache::now()));
    let agents: Vec<_> = workspace
        .panes
        .iter()
        .filter(|pane| sent.as_ref().is_none_or(|turn| turn.pane == pane.index))
        .filter_map(|pane| pane.agent.as_ref())
        .collect();
    let reached = |agent: &ObservedAgent| {
        agent.status == Some(target) && fence.is_none_or(|turn| turn.admits(agent))
    };
    if agents.iter().any(|agent| reached(agent)) {
        RemoteProgress::Reached
    } else if agents.is_empty() {
        RemoteProgress::Gone
    } else {
        RemoteProgress::Running
    }
}
