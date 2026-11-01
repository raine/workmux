use std::io::{IsTerminal, Read};

use anyhow::{Result, anyhow};

use crate::config;
use crate::multiplexer::{create_backend, detect_backend};
use crate::workflow;

pub fn run(name: &str, text: Option<&str>, file: Option<&str>) -> Result<()> {
    let cfg = config::Config::load(None).unwrap_or_default();
    if let Some(record) = crate::remote::agents::find_target(name) {
        let content = read_content(text, file)?;
        return crate::remote::agents::send(&cfg, &record, &content);
    }
    let mux = create_backend(detect_backend());
    let (_path, agent) = workflow::resolve_worktree_agent(name, mux.as_ref())?;
    let content = read_content(text, file)?;

    let agent_identity = agent
        .agent_kind
        .as_deref()
        .or(cfg.agent.as_deref())
        .or(agent.agent_command.as_deref());
    mux.send_prompt_to_agent(&agent.pane_id, &content, agent_identity)?;

    Ok(())
}

/// The text to send: positional arg > --file > stdin, without its trailing
/// newlines.
fn read_content(text: Option<&str>, file: Option<&str>) -> Result<String> {
    let content = if let Some(t) = text {
        t.to_string()
    } else if let Some(f) = file {
        std::fs::read_to_string(f)?
    } else {
        // Guard: don't block on interactive TTY
        if std::io::stdin().is_terminal() {
            return Err(anyhow!(
                "No content to send. Provide text argument, --file, or pipe stdin"
            ));
        }
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };

    let content = content.trim_end_matches('\n');
    if content.is_empty() {
        return Err(anyhow!("No content to send"));
    }
    Ok(content.to_string())
}
