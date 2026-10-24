//! The agents' workmux status hooks on a node, without which remote agents
//! report no status: installed by `workmux remote setup`, reported by
//! `workmux remote doctor`.

use anyhow::{Result, bail};

use super::RemoteSpec;
use super::client::Node;
use super::doctor::{Level, Row};
use super::node::hooks::{AgentHooks, HookState, StatusHooksRequest, StatusHooksResponse};
use super::protocol;
use crate::config::Config;

fn query(spec: &RemoteSpec, config: &Config, install: bool) -> Result<Vec<AgentHooks>> {
    let node = Node::connect_reader(spec, protocol::STATUS_HOOKS)?;
    let request = StatusHooksRequest {
        isolation: spec.isolation,
        install,
        opencode_image: config.sandbox.resolved_image("opencode"),
    };
    let response: StatusHooksResponse = node.call(protocol::STATUS_HOOKS, &request, false)?;
    Ok(response.agents)
}

/// Install missing and outdated hooks on the node and print the outcome.
pub fn setup(spec: &RemoteSpec, config: &Config) -> Result<()> {
    let agents = query(spec, config, true)?;
    let name = &spec.name;
    if agents.is_empty() {
        println!("- {name}: no agent found for status hooks; run setup again after installing one");
        return Ok(());
    }
    let changed = names(&agents, |agent| agent.changed);
    let unchanged = names(&agents, |agent| {
        !agent.changed && agent.state == HookState::Installed
    });
    if !changed.is_empty() {
        println!("✓ {name}: installed status hooks for {changed}");
    }
    if !unchanged.is_empty() {
        println!("✓ {name}: status hooks are already installed for {unchanged}");
    }
    let mut failed = Vec::new();
    for agent in &agents {
        let detail = agent.detail.as_deref().unwrap_or("unknown");
        match agent.state {
            HookState::Installed => {}
            HookState::Unknown => {
                println!(
                    "- {name}: skipped {} status hooks: {detail}",
                    agent.agent.name()
                )
            }
            HookState::Outdated | HookState::Missing | HookState::Error => {
                eprintln!("✗ {name}: {} status hooks: {detail}", agent.agent.name());
                failed.push(agent.agent.name());
            }
        }
    }
    if !failed.is_empty() {
        bail!(
            "could not install status hooks for {} on {name}",
            failed.join(", ")
        );
    }
    Ok(())
}

/// `remote doctor` rows; `ready` is whether the pinned node binary answers.
pub fn doctor_rows(spec: &RemoteSpec, config: &Config, ready: bool) -> Vec<Row> {
    if !ready {
        return vec![Row::new(
            "hooks",
            Level::Skip,
            "the node binary is not ready",
        )];
    }
    match query(spec, config, false) {
        Ok(agents) => rows(&spec.name, &agents),
        Err(err) => vec![
            Row::new("hooks", Level::Warn, format!("{err:#}"))
                .fix(format!("workmux remote setup {}", spec.name)),
        ],
    }
}

fn rows(remote: &str, agents: &[AgentHooks]) -> Vec<Row> {
    let setup = format!("workmux remote setup {remote}");
    if agents.is_empty() {
        return vec![
            Row::new("hooks", Level::Skip, "no agent found on the node")
                .fix(format!("after installing one: {setup}")),
        ];
    }
    let mut rows = Vec::new();
    let installed = names(agents, |agent| agent.state == HookState::Installed);
    if !installed.is_empty() {
        rows.push(Row::new(
            "hooks",
            Level::Ok,
            format!("installed for {installed}"),
        ));
    }
    let missing = names(agents, |agent| agent.state == HookState::Missing);
    let outdated = names(agents, |agent| agent.state == HookState::Outdated);
    let stale: Vec<String> = [("missing", missing), ("outdated", outdated)]
        .into_iter()
        .filter(|(_, agents)| !agents.is_empty())
        .map(|(state, agents)| format!("{state} for {agents}"))
        .collect();
    if !stale.is_empty() {
        rows.push(
            Row::new(
                "hooks",
                Level::Warn,
                format!("{}: those agents report no status", stale.join("; ")),
            )
            .fix(setup.clone()),
        );
    }
    for agent in agents {
        let detail = agent.detail.as_deref().unwrap_or("unknown");
        match agent.state {
            HookState::Unknown => rows.push(Row::new(
                "hooks",
                Level::Skip,
                format!("{}: {detail}", agent.agent.name()),
            )),
            HookState::Error => rows.push(
                Row::new(
                    "hooks",
                    Level::Warn,
                    format!("{}: {detail}", agent.agent.name()),
                )
                .fix(setup.clone()),
            ),
            _ => {}
        }
    }
    rows
}

fn names(agents: &[AgentHooks], keep: impl Fn(&AgentHooks) -> bool) -> String {
    agents
        .iter()
        .filter(|agent| keep(agent))
        .map(|agent| agent.agent.name())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_setup::Agent;

    fn hooks(agent: Agent, state: HookState) -> AgentHooks {
        AgentHooks {
            agent,
            dir: None,
            state,
            detail: matches!(state, HookState::Unknown | HookState::Error).then(|| "why".into()),
            changed: false,
        }
    }

    #[test]
    fn doctor_groups_agents_by_state() {
        let agents = [
            hooks(Agent::Claude, HookState::Installed),
            hooks(Agent::Codex, HookState::Installed),
            hooks(Agent::Gemini, HookState::Missing),
            hooks(Agent::Pi, HookState::Outdated),
            hooks(Agent::OpenCode, HookState::Unknown),
            hooks(Agent::Omp, HookState::Error),
        ];
        let rows = rows("hz", &agents);
        let summary: Vec<_> = rows
            .iter()
            .map(|row| (row.level, row.detail.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                (Level::Ok, "installed for Claude Code, Codex"),
                (
                    Level::Warn,
                    "missing for Gemini CLI; outdated for pi: those agents report no status"
                ),
                (Level::Skip, "OpenCode: why"),
                (Level::Warn, "Oh My Pi: why"),
            ]
        );
        assert_eq!(rows[1].fix.as_deref(), Some("workmux remote setup hz"));
    }

    #[test]
    fn doctor_without_agents_skips() {
        let rows = rows("hz", &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, Level::Skip);
    }
}
