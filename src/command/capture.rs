use anyhow::{Result, anyhow};
use console::strip_ansi_codes;

use crate::multiplexer::{create_backend, detect_backend};
use crate::workflow::{self, AgentTarget};

pub fn run(name: &str, lines: u16) -> Result<()> {
    let mux = create_backend(detect_backend());
    let output = match workflow::resolve_agent_target(name, mux.as_ref())? {
        AgentTarget::Remote { record, pane } => {
            let config = crate::config::Config::load(None).unwrap_or_default();
            crate::remote::agents::capture(&config, &record, pane, lines)?
        }
        AgentTarget::Local(agent) => mux
            .capture_pane(&agent.pane_id, lines)
            .ok_or_else(|| anyhow!("Failed to capture pane output"))?,
    };

    // Strip ANSI escape codes
    let stripped = strip_ansi_codes(&output);

    // Trim trailing blank lines and limit to requested line count.
    // tmux capture-pane may return more lines than requested (it captures
    // from -N to the bottom of the visible pane area).
    let trimmed: Vec<&str> = stripped
        .lines()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .skip_while(|l| l.trim().is_empty())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let start = trimmed.len().saturating_sub(lines as usize);
    for line in &trimmed[start..] {
        println!("{line}");
    }

    Ok(())
}
