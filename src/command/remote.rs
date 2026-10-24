//! `workmux remote`: manage SSH remotes that run agents.

use std::thread;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use crate::config::Config;
use crate::multiplexer::{create_backend, detect_backend};
use crate::remote::RemoteSpec;
use crate::remote::install::{self, NodeStatus, Pin};
use crate::remote::transport::Transport;
use crate::remote::{Placement, client, credentials, doctor, observe, resolve_placement};
use crate::workflow::WorkflowContext;

#[derive(Debug, Args)]
pub struct RemoteArgs {
    #[command(subcommand)]
    pub command: Option<RemoteCommand>,
}

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Install this workmux's node binary and the agents' status hooks on a
    /// remote
    Setup {
        /// Remote name from `remotes:` in the global config
        name: String,
    },
    /// Check that a remote can run agents
    Doctor {
        /// Remote name from `remotes:` in the global config
        name: String,
    },
    /// Log an agent in on a remote, where its remote agents find the login
    Login {
        /// Remote name from `remotes:` in the global config
        name: String,
        /// claude, codex, gemini, opencode, pi or omp
        agent: String,
        /// Copy this machine's credential files for the agent instead
        #[arg(long, conflicts_with_all = ["setup_token", "args"])]
        import: bool,
        /// Claude: run `claude setup-token` and keep the token it prints
        /// for Claude agents on the remote
        #[arg(long)]
        setup_token: bool,
        /// Extra arguments for the agent's login command
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Bring a remote workspace home: stop its agent, make its work a local
    /// worktree with a window, and remove it from the remote
    Pull {
        /// Handle of the remote workspace
        handle: String,
        /// Skip post_create hooks for the local worktree
        #[arg(long)]
        no_hooks: bool,
    },
    /// List what a remote holds that no workspace uses, and remove it
    Gc {
        /// Remote name from `remotes:` in the global config
        name: String,
        /// Only list what would be removed
        #[arg(long)]
        dry_run: bool,
        /// Remove without asking, including workspaces whose work exists only
        /// on the remote
        #[arg(long, conflicts_with = "dry_run")]
        force: bool,
    },
    /// Relay one pane of a remote execution into this terminal
    #[command(hide = true)]
    Attach {
        name: String,
        ex_id: String,
        #[arg(long)]
        pane: usize,
    },
    /// `GIT_SSH_COMMAND` for pushing a base commit to a remote
    #[command(hide = true, name = "git-ssh")]
    GitSsh {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

pub fn run(args: RemoteArgs) -> Result<()> {
    let config = Config::load(None)?;
    match args.command {
        None => list(&config),
        Some(RemoteCommand::Setup { name }) => {
            let spec = RemoteSpec::resolve(&config, &name)?;
            install::setup(&spec)?;
            crate::remote::hooks::setup(&spec, &config)?;
            observe::current(&config, &name);
            Ok(())
        }
        Some(RemoteCommand::Doctor { name }) => {
            let spec = RemoteSpec::resolve(&config, &name)?;
            if !doctor::run(&spec, &config)? {
                bail!(
                    "remote '{name}' is not ready: fix the ✗ rows above, then run \
                     `workmux remote doctor {name}` again"
                );
            }
            observe::current(&config, &name);
            Ok(())
        }
        Some(RemoteCommand::Login {
            name,
            agent,
            import,
            setup_token,
            args,
        }) => {
            let spec = RemoteSpec::resolve(&config, &name)?;
            if import {
                credentials::import(&spec, &config, &agent)?;
            } else {
                let status = credentials::login(&spec, &config, &agent, setup_token, &args)?;
                if status != 0 {
                    std::process::exit(status);
                }
            }
            observe::current(&config, &name);
            Ok(())
        }
        Some(RemoteCommand::Pull { handle, no_hooks }) => pull(config, &handle, !no_hooks),
        Some(RemoteCommand::Gc {
            name,
            dry_run,
            force,
        }) => crate::remote::gc::run(&config, &name, dry_run, force),
        Some(RemoteCommand::Attach { name, ex_id, pane }) => {
            crate::remote::attach::run(&config, &name, &ex_id, pane)
        }
        Some(RemoteCommand::GitSsh { name, args }) => {
            let status = client::git_ssh(&RemoteSpec::resolve(&config, &name)?, &args)?;
            std::process::exit(status)
        }
    }
}

fn pull(config: Config, handle: &str, run_hooks: bool) -> Result<()> {
    use crate::remote::cache::{self, Homecoming};
    // A pulled workspace whose remote cleanup is pending is found too, so
    // pulling it again finishes that cleanup.
    let pulled = || {
        let root = crate::git::get_main_worktree_root().ok()?;
        cache::for_project(&root).into_iter().find(|record| {
            record.handle == handle && matches!(record.homecoming, Some(Homecoming::Pulled { .. }))
        })
    };
    let Some(record) = cache::find_here(handle).or_else(pulled) else {
        bail!("No remote workspace '{handle}' in this repository");
    };
    let mux = create_backend(detect_backend());
    let context = WorkflowContext::new(config, mux, None)?;
    crate::remote::homecoming::pull(&context, record, run_hooks)
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
                observe::current(config, name);
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
