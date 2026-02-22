use std::collections::HashSet;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::multiplexer::{AgentStatus, create_backend, detect_backend};
use crate::vcs;
use crate::state::StateStore;
use crate::util;
use crate::workflow;

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
    let vcs = vcs::detect_vcs()?;
    let start = Instant::now();

    // Resolve worktree paths upfront
    let worktree_paths: Vec<_> = worktree_names
        .iter()
        .map(|name| {
            let (path, _branch) = vcs.find_workspace(name)?;
            Ok((name.clone(), path))
        })
        .collect::<Result<Vec<_>>>()?;

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
        if reached.len() == worktree_paths.len() {
            return Ok(());
        }

        thread::sleep(Duration::from_secs(2));
    }
}
