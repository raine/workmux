//! `workmux remote`: manage SSH remotes that run agents.

use std::thread;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use crate::config::Config;
use crate::remote::RemoteSpec;
use crate::remote::install::{self, NodeStatus, Pin};
use crate::remote::transport::Transport;
use crate::remote::{Placement, doctor, resolve_placement};

#[derive(Debug, Args)]
pub struct RemoteArgs {
    #[command(subcommand)]
    pub command: Option<RemoteCommand>,
}

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Install this workmux's node binary on a remote
    Setup {
        /// Remote name from `remotes:` in the global config
        name: String,
    },
    /// Check that a remote can run agents
    Doctor {
        /// Remote name from `remotes:` in the global config
        name: String,
    },
}

pub fn run(args: RemoteArgs) -> Result<()> {
    let config = Config::load(None)?;
    match args.command {
        None => list(&config),
        Some(RemoteCommand::Setup { name }) => {
            install::setup(&RemoteSpec::resolve(&config, &name)?)
        }
        Some(RemoteCommand::Doctor { name }) => {
            let spec = RemoteSpec::resolve(&config, &name)?;
            if !doctor::run(&spec, &config)? {
                bail!("remote {name} is not ready");
            }
            Ok(())
        }
    }
}

fn list(config: &Config) -> Result<()> {
    if config.remotes.is_empty() {
        println!(
            "No remotes configured. Add one under `remotes:` in your global config \
             (~/.config/workmux/config.yaml)."
        );
        return Ok(());
    }
    let default = match resolve_placement(config, None, false) {
        Ok(Placement::Remote(name)) => Some(name),
        _ => None,
    };
    let rows: Vec<(String, String, String)> = thread::scope(|scope| {
        let handles: Vec<_> = config
            .remotes
            .iter()
            .map(|(name, remote)| {
                scope.spawn(move || (name.clone(), remote.host.clone(), status(config, name)))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("remote status thread panicked"))
            .collect()
    });
    let name_width = rows
        .iter()
        .map(|row| row.0.len() + 10)
        .max()
        .unwrap_or(4)
        .max(4);
    let host_width = rows.iter().map(|row| row.1.len()).max().unwrap_or(4).max(4);
    println!("{:<name_width$}  {:<host_width$}  STATUS", "NAME", "HOST");
    for (name, host, status) in rows {
        let label = if default.as_deref() == Some(name.as_str()) {
            format!("{name} (default)")
        } else {
            name
        };
        println!("{label:<name_width$}  {host:<host_width$}  {status}");
    }
    Ok(())
}

fn status(config: &Config, name: &str) -> String {
    let check = || -> Result<String> {
        let spec = match RemoteSpec::resolve(config, name) {
            Ok(spec) => spec,
            Err(err) => return Ok(format!("invalid config: {err:#}")),
        };
        let transport = Transport::open(&spec)?;
        if let Err(err) = transport.connect() {
            return Ok(format!("unreachable: {err:#}"));
        }
        let pin = Pin::for_spec(&spec)?;
        Ok(match install::node_status(&transport, &spec, &pin)? {
            NodeStatus::Ready(hello) => {
                format!(
                    "ready (workmux {}, protocol {})",
                    hello.version, hello.protocol
                )
            }
            NodeStatus::Missing => {
                format!("reachable, not set up: run workmux remote setup {name}")
            }
            NodeStatus::Mismatch(hello) => format!(
                "protocol mismatch: node speaks {}, run workmux remote doctor {name}",
                hello.protocol
            ),
            NodeStatus::Broken(_) => format!("node binary broken: run workmux remote setup {name}"),
        })
    };
    check().unwrap_or_else(|err| format!("error: {err:#}"))
}
