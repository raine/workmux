use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

use crate::multiplexer::{AgentPane, Multiplexer};
use crate::state::StateStore;
use crate::util::canon_or_self;
use crate::vcs::Vcs;

/// Resolve a worktree name to its agent panes.
///
/// 1. Finds the workspace path via VCS
/// 2. Loads reconciled agent state
/// 3. Matches agents by comparing canonical workdir paths
///
/// Returns the workspace path and matching agent panes (may be empty if no agent is running).
pub fn resolve_worktree_agents(
    name: &str,
    mux: &dyn Multiplexer,
    vcs: &dyn Vcs,
) -> Result<(PathBuf, Vec<AgentPane>)> {
    let (worktree_path, _branch) = vcs.find_workspace(name)?;
    let canon_wt_path = canon_or_self(&worktree_path);

    let agent_panes = StateStore::new().and_then(|store| store.load_reconciled_agents(mux))?;

    let matching: Vec<AgentPane> = agent_panes
        .into_iter()
        .filter(|a| {
            let canon_agent_path = canon_or_self(&a.path);
            canon_agent_path == canon_wt_path || canon_agent_path.starts_with(&canon_wt_path)
        })
        .collect();

    Ok((worktree_path, matching))
}

/// Resolve a worktree name to exactly one agent pane (the first/primary).
///
/// Returns an error if no agent is running in the worktree.
pub fn resolve_worktree_agent(name: &str, mux: &dyn Multiplexer, vcs: &dyn Vcs) -> Result<(PathBuf, AgentPane)> {
    let (path, agents) = resolve_worktree_agents(name, mux, vcs)?;
    let agent = agents
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("No agent running in worktree '{}'", name))?;
    Ok((path, agent))
}

/// Match agents to a worktree path from a pre-loaded agent list.
///
/// Used by `status` and `wait` commands that load agents once and match
/// multiple worktrees, avoiding repeated calls to `load_reconciled_agents`.
pub fn match_agents_to_worktree<'a>(
    agents: &'a [AgentPane],
    worktree_path: &Path,
) -> Vec<&'a AgentPane> {
    let canon_wt = canon_or_self(worktree_path);
    agents
        .iter()
        .filter(|a| {
            let canon_agent = canon_or_self(&a.path);
            canon_agent == canon_wt || canon_agent.starts_with(&canon_wt)
        })
        .collect()
}
