use anyhow::Result;
use console::style;
use std::io::{self, IsTerminal};

use crate::agent_setup::{self, Agent, StatusCheck};
use crate::skills;
use crate::ui::confirm::{self, ConfirmDefault};

pub fn run(hooks_only: bool, skills_only: bool) -> Result<()> {
    if !io::stdin().is_terminal() {
        anyhow::bail!("workmux setup requires an interactive terminal");
    }

    // If neither flag is set, do both
    let do_hooks = !skills_only || hooks_only;
    let do_skills = !hooks_only || skills_only;

    let checks = if do_hooks {
        agent_setup::check_all_for_setup()
    } else {
        agent_setup::check_all()
    };

    if checks.is_empty() {
        println!(
            "No agents detected. Install an agent CLI (Claude Code, OpenCode) to get started."
        );
        return Ok(());
    }

    if do_hooks {
        run_hooks_setup(&checks)?;
    }

    if do_skills {
        if do_hooks {
            println!();
        }
        run_skills_setup(&checks)?;
    }

    Ok(())
}

fn run_hooks_setup(checks: &[agent_setup::AgentCheck]) -> Result<()> {
    println!();
    println!("  {}", style("Status Tracking").bold().cyan());
    println!();

    let mut any_needed = false;

    for check in checks {
        let status_str = match &check.status {
            StatusCheck::Installed => format!("{}", style("configured").green()),
            StatusCheck::UpdateAvailable => {
                any_needed = true;
                format!("{}", style("update available").yellow())
            }
            StatusCheck::NotInstalled => {
                any_needed = true;
                format!("{}", style("not configured").yellow())
            }
            StatusCheck::Error(e) => {
                any_needed = true;
                format!("{} ({})", style("error").red(), e)
            }
        };

        println!(
            "  {} {} ({}): {}",
            style("•").dim(),
            check.agent.name(),
            style(check.reason).dim(),
            status_str
        );
    }
    println!();

    for check in checks {
        if matches!(check.status, StatusCheck::UpdateAvailable) {
            agent_setup::print_update_diff(check.agent);
        }
    }

    if !any_needed {
        println!(
            "  {}",
            style("All agents have status tracking configured.").green()
        );
        return Ok(());
    }

    let needs_setup: Vec<_> = checks
        .iter()
        .filter(|c| {
            matches!(
                c.status,
                StatusCheck::NotInstalled | StatusCheck::UpdateAvailable | StatusCheck::Error(_)
            )
        })
        .collect();

    agent_setup::print_description("");
    println!();

    if confirm::confirm(
        "Install or update status tracking hooks?",
        ConfirmDefault::Yes,
    )? {
        let mut any_failed = false;
        for check in &needs_setup {
            match agent_setup::install(check.agent) {
                Ok(msg) => println!("  {} {}", style("✓").green(), msg),
                Err(e) => {
                    println!("  {} {}: {}", style("✗").red(), check.agent.name(), e);
                    any_failed = true;
                }
            }
        }
        println!();
        if any_failed {
            anyhow::bail!("Some hook installations failed");
        }
    }

    Ok(())
}

fn run_skills_setup(checks: &[agent_setup::AgentCheck]) -> Result<()> {
    println!("  {}", style("Skills").bold().cyan());
    println!();

    let skill_agents: Vec<Agent> = checks
        .iter()
        .map(|c| c.agent)
        .filter(|a| skills::skills_dir(*a).is_some())
        .collect();

    if skill_agents.is_empty() {
        println!("  No agents with skill support detected.");
        return Ok(());
    }

    let skill_names: Vec<_> = skills::BUNDLED_SKILLS.iter().map(|s| s.name).collect();
    println!("  Skills: {}", style(skill_names.join(", ")).dim());
    for agent in &skill_agents {
        if let Some(dir) = skills::skills_dir(*agent) {
            println!(
                "  {} {} -> {}",
                style("•").dim(),
                agent.name(),
                style(dir.display()).dim()
            );
        }
    }
    println!();
    println!(
        "  Learn more: {}",
        style("https://workmux.raine.dev/guide/skills").dim()
    );
    println!();

    if confirm::confirm("Install bundled skills?", ConfirmDefault::Yes)? {
        let mut any_failed = false;
        for agent in &skill_agents {
            match skills::install_skills(*agent) {
                Ok(msg) => println!("  {}", msg),
                Err(e) => {
                    println!("  {} {}: {}", style("✗").red(), agent.name(), e);
                    any_failed = true;
                }
            }
        }
        println!();
        if any_failed {
            anyhow::bail!("Some skill installations failed");
        }
    }

    Ok(())
}
