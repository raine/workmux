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
mod workflow;

use anyhow::Result;
use tracing::{error, info};

#[cfg(all(debug_assertions, feature = "dev-hotpatch"))]
fn connect_hotpatch_runtime() {
    dioxus_devtools::connect_at(hotpatch_endpoint(), |msg| {
        if let dioxus_devtools::DevserverMsg::HotReload(hot_reload_msg) = msg {
            if let Some(jumptable) = hot_reload_msg.jump_table {
                if hot_reload_msg.for_pid == Some(std::process::id()) {
                    unsafe { dioxus_devtools::subsecond::apply_patch(jumptable).unwrap() };
                }
            }
        }
    });
}

#[cfg(not(all(debug_assertions, feature = "dev-hotpatch")))]
fn connect_hotpatch_runtime() {}

#[cfg(all(debug_assertions, feature = "dev-hotpatch"))]
fn hotpatch_endpoint() -> String {
    let ip = std::env::var("DIOXUS_DEVSERVER_IP").ok();
    let port = std::env::var("DIOXUS_DEVSERVER_PORT").ok();
    hotpatch_endpoint_from_env(ip.as_deref(), port.as_deref())
}

#[cfg(any(test, all(debug_assertions, feature = "dev-hotpatch")))]
fn hotpatch_endpoint_from_env(ip: Option<&str>, port: Option<&str>) -> String {
    format!(
        "ws://{}:{}/_dioxus",
        ip.unwrap_or("127.0.0.1"),
        port.unwrap_or("8080")
    )
}

fn main() -> Result<()> {
    logger::init()?;
    connect_hotpatch_runtime();
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

#[cfg(test)]
mod tests {
    use super::hotpatch_endpoint_from_env;

    #[test]
    fn hotpatch_endpoint_defaults_to_localhost() {
        let endpoint = hotpatch_endpoint_from_env(None, None);
        assert_eq!(endpoint, "ws://127.0.0.1:8080/_dioxus");
    }

    #[test]
    fn hotpatch_endpoint_uses_provided_env_values() {
        let endpoint = hotpatch_endpoint_from_env(Some("10.1.2.3"), Some("9091"));
        assert_eq!(endpoint, "ws://10.1.2.3:9091/_dioxus");
    }
}
