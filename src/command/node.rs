//! `workmux node <verb> --json`: the entry point the laptop calls on a
//! remote node. Each verb reads one JSON object on stdin and writes one JSON
//! document on stdout; failures write an error envelope and exit 1.

use std::io::{IsTerminal, Read};

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::remote::node::{self, NodeRoot, hooks, observe};
use crate::remote::protocol::{ErrorEnvelope, Hello, NodeError, code};

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
    /// Prepare a workspace's repository, then create and provision it
    ProvisionWorkspace {
        /// Read JSON from stdin and write JSON to stdout (required)
        #[arg(long, required = true)]
        json: bool,
    },
    /// Start a workspace's panes in node tmux sessions
    StartExecution {
        /// Read JSON from stdin and write JSON to stdout (required)
        #[arg(long, required = true)]
        json: bool,
    },
    /// Stop a workspace and delete its worktree, branch and records
    RemoveWorkspace {
        /// Read JSON from stdin and write JSON to stdout (required)
        #[arg(long, required = true)]
        json: bool,
    },
    /// Attach this terminal to a pane session of an execution
    Attach { ex_id: String, pane: usize },
    /// Run an agent's login on this terminal, where executions find it
    Login {
        agent: String,
        /// Container image to log in from; without it, the login runs on the
        /// host (isolation: none)
        #[arg(long)]
        image: Option<String>,
        /// Then read the token `claude setup-token` printed
        #[arg(long)]
        setup_token: bool,
        /// Login command and arguments
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Report which agents have credentials on the node
    Credentials {
        /// Read JSON from stdin and write JSON to stdout (required)
        #[arg(long, required = true)]
        json: bool,
    },
    /// Write an agent's credential files into the node user's directories
    PutCredentials {
        /// Read JSON from stdin and write JSON to stdout (required)
        #[arg(long, required = true)]
        json: bool,
    },
    /// Report the agents of running executions, once or as a stream
    Watch {
        /// Read JSON from stdin and write JSON to stdout (required)
        #[arg(long, required = true)]
        json: bool,
    },
    /// Type text into an agent pane
    Send {
        /// Read JSON from stdin and write JSON to stdout (required)
        #[arg(long, required = true)]
        json: bool,
    },
    /// Return the recent output of an agent pane
    Capture {
        /// Read JSON from stdin and write JSON to stdout (required)
        #[arg(long, required = true)]
        json: bool,
    },
    /// Check or install the agents' workmux status hooks on the node
    StatusHooks {
        /// Read JSON from stdin and write JSON to stdout (required)
        #[arg(long, required = true)]
        json: bool,
    },
}

pub fn run(args: NodeArgs) -> Result<()> {
    match &args.verb {
        NodeVerb::Attach { ex_id, pane } => {
            let status = node::attach::attach(&NodeRoot::from_env()?, ex_id, *pane)?;
            std::process::exit(status);
        }
        NodeVerb::Login {
            agent,
            image,
            setup_token,
            command,
        } => {
            let status = node::credentials::login(
                &NodeRoot::from_env()?,
                agent,
                image.as_deref(),
                *setup_token,
                command,
            )?;
            std::process::exit(status);
        }
        _ => {}
    }
    let response = read_request().and_then(|request| match args.verb {
        NodeVerb::Hello { .. } => Ok(serde_json::to_value(Hello::local())?),
        NodeVerb::ProvisionWorkspace { .. } => serve(request, |root, request| {
            node::provision::provision(&root, request)
        }),
        NodeVerb::StartExecution { .. } => serve(request, |root, request| {
            node::execution::start(&root, request)
        }),
        NodeVerb::RemoveWorkspace { .. } => serve(request, |root, request| {
            node::remove::remove(&root, request)
        }),
        NodeVerb::Credentials { .. } => serve(request, |root, request| {
            node::credentials::check(&root, request)
        }),
        NodeVerb::PutCredentials { .. } => serve(request, |root, request| {
            node::credentials::put(&root, request)
        }),
        NodeVerb::Watch { .. } => serve(request, |root, request: observe::WatchRequest| {
            if request.stream {
                observe::stream(&root)?;
                std::process::exit(0);
            }
            observe::snapshot(&root)
        }),
        NodeVerb::Send { .. } => serve(request, |root, request| observe::send(&root, request)),
        NodeVerb::Capture { .. } => {
            serve(request, |root, request| observe::capture(&root, request))
        }
        NodeVerb::StatusHooks { .. } => serve(request, |_, request| hooks::status_hooks(request)),
        NodeVerb::Attach { .. } | NodeVerb::Login { .. } => unreachable!("handled above"),
    });
    match response {
        Ok(value) => {
            println!("{value}");
            Ok(())
        }
        Err(err) => {
            let envelope = match err.downcast_ref::<NodeError>() {
                Some(refusal) => ErrorEnvelope::new(&refusal.code, refusal.message.clone()),
                None => ErrorEnvelope::new(code::FAILED, format!("{err:#}")),
            };
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(1);
        }
    }
}

/// Decode a typed request, run `verb` on the node root and encode its
/// response.
fn serve<Req: DeserializeOwned, Resp: Serialize>(
    request: serde_json::Map<String, serde_json::Value>,
    verb: impl FnOnce(NodeRoot, Req) -> Result<Resp>,
) -> Result<serde_json::Value> {
    let request: Req = serde_json::from_value(serde_json::Value::Object(request))
        .map_err(|err| node::refuse(code::BAD_REQUEST, format!("invalid request: {err}")))?;
    let root =
        NodeRoot::from_env().map_err(|err| node::refuse(code::BAD_REQUEST, format!("{err:#}")))?;
    Ok(serde_json::to_value(verb(root, request)?)?)
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
        _ => Err(node::refuse(
            code::BAD_REQUEST,
            "request must be a JSON object",
        )),
    }
}
