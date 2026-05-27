mod agent_display;
mod agent_identity;
mod agent_setup;
mod claude;
mod cli;
mod cmd;
mod command;
mod config;
mod git;
mod github;
mod llm;
mod logger;
mod markdown;
mod multiplexer;
mod naming;
mod nerdfont;
mod prompt;
mod sandbox;
mod shell;
mod skills;
mod spinner;
mod state;
mod template;
#[cfg(test)]
mod test_support;
mod tips;
mod tmux_style;
mod ui;
mod util;
mod workflow;
mod xdg;

use anyhow::Result;
use tracing::{error, info};

fn main() -> Result<()> {
    logger::init()?;
    let context = LogContext::current();
    info!(
        args = ?std::env::args().collect::<Vec<_>>(),
        cwd = ?context.cwd,
        tmux_pane = ?context.tmux_pane,
        "workmux start"
    );

    match cli::run() {
        Ok(result) => {
            info!(
                cwd = ?context.cwd,
                tmux_pane = ?context.tmux_pane,
                "workmux finished successfully"
            );
            Ok(result)
        }
        Err(err) => {
            error!(error = ?err, "workmux failed");
            Err(err)
        }
    }
}

struct LogContext {
    cwd: Option<std::path::PathBuf>,
    tmux_pane: Option<String>,
}

impl LogContext {
    fn current() -> Self {
        Self {
            cwd: std::env::current_dir().ok(),
            tmux_pane: std::env::var("TMUX_PANE").ok(),
        }
    }
}
