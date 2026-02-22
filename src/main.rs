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
mod spinner;
mod state;
mod template;
mod util;
mod vcs;
mod workflow;

use anyhow::Result;
use tracing::{error, info};

fn main() -> Result<()> {
    logger::init()?;
    info!(args = ?std::env::args().collect::<Vec<_>>(), "workmux start");

    match cli::run() {
        Ok(result) => {
            info!("workmux finished successfully");
            Ok(result)
        }
        Err(err) => {
            error!(error = ?err, "workmux failed");
            Err(err)
        }
    }
}
