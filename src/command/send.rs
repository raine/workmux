use std::io::{IsTerminal, Read};

use anyhow::{Result, anyhow};

use crate::config;
use crate::multiplexer::{create_backend, detect_backend};
use crate::workflow;

pub fn run(name: &str, text: Option<&str>, file: Option<&str>) -> Result<()> {
    let cfg = config::Config::load(None).unwrap_or_default();
    let mux = create_backend(detect_backend());
    let vcs = crate::vcs::detect_vcs()?;
    let (_path, agent) = workflow::resolve_worktree_agent(name, mux.as_ref(), vcs.as_ref())?;

    // Determine content: positional arg > --file > stdin
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

    // Strip trailing newline
    let content = content.trim_end_matches('\n');

    if content.is_empty() {
        return Err(anyhow!("No content to send"));
    }

    // Single-line: use send_keys_to_agent (handles Claude's ! prefix delay)
    // Multi-line: use paste_multiline (already sends Enter in both backends)
    if content.contains('\n') {
        mux.paste_multiline(&agent.pane_id, content)?;
    } else {
        mux.send_keys_to_agent(&agent.pane_id, content, cfg.agent.as_deref())?;
    }

    Ok(())
}
