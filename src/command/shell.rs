use anyhow::{Result, anyhow, bail};

use crate::config::Config;
use crate::multiplexer::{create_backend, detect_backend};
use crate::remote::cache::{self, WorkspaceRecord};

/// `workmux shell [name]`: a shell in a remote workspace. Without a name,
/// the workspace whose window this runs in.
pub fn run(name: Option<&str>, host: bool) -> Result<()> {
    if crate::remote::node::operation::in_session() {
        bail!("this pane is already on the server, in the workspace's worktree");
    }
    let record = match name {
        Some(name) => named(name)?,
        None => current_window().ok_or_else(|| {
            anyhow!("not in the window of a remote workspace; name one: `workmux shell <handle>`")
        })?,
    };
    let config = Config::load(None)?;
    crate::remote::shell::run(&config, &record, host)
}

fn named(name: &str) -> Result<WorkspaceRecord> {
    if let Some(record) = crate::remote::agents::find_target(name) {
        return Ok(record);
    }
    if crate::git::find_worktree(name).is_ok() {
        bail!(
            "'{name}' is a local worktree; `workmux shell` opens shells on remote servers. \
             Use `cd \"$(workmux path {name})\"`"
        );
    }
    bail!("no remote workspace named '{name}'; `workmux list` shows this repository's workspaces")
}

/// The remote workspace with a relay pane in the current window, such as
/// the window a fresh split was made in.
fn current_window() -> Option<WorkspaceRecord> {
    let records = cache::all();
    if records.is_empty() {
        return None;
    }
    let mux = create_backend(detect_backend());
    let panes = mux.get_all_live_pane_info().ok()?;
    let window = panes.get(&mux.current_pane_id()?)?.window_id.clone()?;
    let instance = mux.instance_id();
    let agents = crate::state::StateStore::new()
        .ok()?
        .list_all_agents()
        .ok()?;
    let relayed: Vec<_> = agents
        .into_iter()
        .filter(|agent| {
            agent.pane_key.backend == mux.name()
                && agent.pane_key.instance == instance
                && panes
                    .get(&agent.pane_key.pane_id)
                    .is_some_and(|pane| pane.window_id.as_ref() == Some(&window))
        })
        .filter_map(|agent| agent.remote)
        .collect();
    records.into_iter().find(|record| {
        relayed
            .iter()
            .any(|at| at.remote == record.remote && at.ws_id == record.ws_id)
    })
}
