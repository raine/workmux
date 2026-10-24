//! `workmux node <verb> --json`: the entry point the laptop calls on a
//! remote node. Each verb reads one JSON object on stdin and writes one JSON
//! document on stdout; failures write an error envelope and exit 1.

use std::io::{IsTerminal, Read};

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use crate::remote::protocol::{ErrorEnvelope, Hello};

#[derive(Debug, Args)]
pub struct NodeArgs {
    #[command(subcommand)]
    pub verb: NodeVerb,
}

#[derive(Debug, Subcommand)]
pub enum NodeVerb {
    /// Report protocol version, workmux version, platform and capabilities
    Hello {
        /// Write JSON to stdout (required)
        #[arg(long, required = true)]
        json: bool,
    },
}

pub fn run(args: NodeArgs) -> Result<()> {
    let response = read_request().and_then(|_request| match args.verb {
        NodeVerb::Hello { .. } => Ok(serde_json::to_value(Hello::local())?),
    });
    match response {
        Ok(value) => {
            println!("{value}");
            Ok(())
        }
        Err(err) => {
            let envelope = ErrorEnvelope::new("bad_request", format!("{err:#}"));
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(1);
        }
    }
}

/// The request object from stdin; `{}` when stdin is a terminal or empty.
fn read_request() -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(Default::default());
    }
    let mut text = String::new();
    stdin.read_to_string(&mut text)?;
    if text.trim().is_empty() {
        return Ok(Default::default());
    }
    match serde_json::from_str(&text)? {
        serde_json::Value::Object(map) => Ok(map),
        _ => bail!("request must be a JSON object"),
    }
}
