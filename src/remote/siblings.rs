//! Workspaces that remote agents spawn on their node (see
//! `node::spawn`). The node runs them without the laptop; the laptop adopts
//! each one the next time it looks at the node: it saves a record and opens
//! the window, as `workmux add --remote` would have.
//!
//! The record is the only acknowledgement. A running sibling the laptop has
//! no record of is adopted again, and one it cannot adopt (unknown project,
//! name taken) keeps running on the node until `workmux remote gc`.

use std::collections::HashSet;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};

use super::cache::{self, ExecutionRecord, WorkspaceRecord, WorkspaceState};
use super::node::observe::{ObservedWorkspace, Sibling};
use super::observe::Observation;
use crate::config::Config;
use crate::multiplexer::{Multiplexer, create_backend, detect_backend};
use crate::workflow::WorkflowContext;

/// Seconds after its spawn within which a sibling's window takes focus, so
/// a laptop that wakes up later does not jump between windows.
const FOCUS_WITHIN: u64 = 60;

/// Adopt the running siblings in `observation` of `remote` and open the
/// windows of adopted ones that have none yet. Problems go to stderr in
/// `foreground` commands and to the log in observers.
pub fn adopt(remote: &str, observation: &Observation, foreground: bool) {
    if let Err(err) = adopt_all(remote, observation, foreground) {
        report(
            foreground,
            &format!("could not adopt remote agents on '{remote}': {err:#}"),
        );
    }
}

fn adopt_all(remote: &str, observation: &Observation, foreground: bool) -> Result<()> {
    let present: Vec<&str> = observation
        .workspaces
        .iter()
        .map(|ws| ws.ws_id.as_str())
        .collect();
    // A workspace whose record went was removed or forgotten on purpose.
    // `remove --force` can forget one the node still has; it stays
    // unadopted for as long as the node lists it, and `remote gc` removes
    // it.
    let seen = observation.last_seen.unwrap_or(0);
    cache::prune_forgotten(remote, &present, seen);
    let running: Vec<(&ObservedWorkspace, &Sibling)> = observation
        .workspaces
        .iter()
        .filter(|ws| cache::forgotten_at(remote, &ws.ws_id).is_none())
        .filter_map(|ws| Some((ws, ws.sibling.as_ref()?)))
        .collect();
    if running.is_empty() {
        return Ok(());
    }
    let unsettled = |records: &[WorkspaceRecord]| -> Vec<(&ObservedWorkspace, &Sibling)> {
        running
            .iter()
            .copied()
            .filter(|(ws, _)| {
                !records.iter().any(|record| {
                    record.remote == remote && record.ws_id == ws.ws_id && !record.window_pending
                })
            })
            .collect()
    };
    if unsettled(&cache::all()).is_empty() {
        return Ok(());
    }
    let _lock = lock(remote)?;
    let records = cache::all();
    for (ws, sibling) in unsettled(&records) {
        let key = format!("{remote}/{}", ws.ws_id);
        if refused().contains(&key) {
            continue;
        }
        let pending = records
            .iter()
            .find(|record| record.remote == remote && record.ws_id == ws.ws_id);
        // An ended execution has nothing to relay; `workmux open` restarts it.
        if pending.is_some() && ws.ended.is_some() {
            continue;
        }
        let result = match pending {
            Some(record) => context(&record.project_root)
                .and_then(|context| open(&context, record.clone(), sibling, observation, &records)),
            None => adopt_one(remote, ws, sibling, observation, &records, foreground),
        };
        if let Err(err) = result {
            refused().insert(key);
            report(
                foreground,
                &format!(
                    "not adopting remote agent '{}' on '{remote}': {err:#}",
                    sibling.identity.handle
                ),
            );
        }
    }
    Ok(())
}

fn adopt_one(
    remote: &str,
    ws: &ObservedWorkspace,
    sibling: &Sibling,
    observation: &Observation,
    records: &[WorkspaceRecord],
    foreground: bool,
) -> Result<()> {
    let identity = &sibling.identity;
    let Some(project_root) = project_root(&identity.project_id, records) else {
        bail!(
            "its repository has no other remote workspace on this machine; run `workmux open {}` \
             in the repository",
            identity.handle
        );
    };
    if !valid_pin(&sibling.pin) {
        bail!("the node reported an invalid binary name");
    }
    if let Some(record) = cache::claimed(&project_root, &identity.handle, &identity.branch) {
        bail!(
            "remote workspace '{}' on '{}' already uses that name or branch",
            record.handle,
            record.remote
        );
    }
    let context = context(&project_root)?;
    let window_name = crate::multiplexer::util::prefixed(&context.prefix, &identity.handle);
    if let Some(conflict) = super::lifecycle::local_conflict(
        &context,
        &identity.branch,
        &identity.handle,
        &window_name,
        true,
        "",
    )? {
        bail!("{conflict}");
    }
    let mut record = WorkspaceRecord::new(
        identity.ws_id.clone(),
        remote,
        identity.project_id.clone(),
        project_root,
        &identity.handle,
        &identity.branch,
        identity.base_sha.clone(),
        Some(sibling.origin.base_label.clone()),
        window_name,
        context.worktree_base_dir()?.join(&identity.handle),
    );
    record.remote_worktree = Some(sibling.worktree.clone());
    record.state = WorkspaceState::Ready;
    record.layout = sibling.origin.layout.clone();
    record.execution = Some(ExecutionRecord {
        ex_id: ws.ex_id.clone(),
        panes: ws.panes.iter().map(|pane| pane.index).collect(),
        node_bin: sibling.pin.clone(),
    });
    record.window_pending = true;
    record.save()?;
    report_info(
        foreground,
        &format!(
            "adopted remote agent '{}' on '{remote}', spawned by another agent there",
            identity.handle
        ),
    );
    if ws.ended.is_some() {
        return Ok(());
    }
    open(&context, record, sibling, observation, records)
}

/// Open the window of adopted `record` if a multiplexer runs; otherwise it
/// stays pending.
fn open(
    context: &WorkflowContext,
    mut record: WorkspaceRecord,
    sibling: &Sibling,
    observation: &Observation,
    records: &[WorkspaceRecord],
) -> Result<()> {
    let mux = context.mux.as_ref();
    if !mux.is_running().unwrap_or(false) {
        return Ok(());
    }
    if !mux.window_exists_by_full_name(&record.window_name)? {
        let spawned = observation.local_ts(sibling.origin.spawned_at);
        let focus =
            !sibling.origin.background && cache::now().saturating_sub(spawned) <= FOCUS_WITHIN;
        let parent = records
            .iter()
            .find(|parent| parent.remote == record.remote && parent.ws_id == sibling.origin.parent)
            .filter(|_| mux.name() == "tmux")
            .and_then(|parent| window_of(mux, &parent.window_name));
        match parent {
            // Next to the window of the agent that spawned it, in whichever
            // session has it; focus only moves within this process's session.
            Some(parent) => super::lifecycle::open_window_after(
                context,
                &record,
                focus && parent.here,
                Some(&parent.id),
            ),
            None => super::lifecycle::open_window(context, &record, focus),
        }?;
    }
    record.window_pending = false;
    record.save()
}

struct ParentWindow {
    id: String,
    /// In the session of this process.
    here: bool,
}

/// The window named `name`, preferring one in this process's session.
fn window_of(mux: &dyn Multiplexer, name: &str) -> Option<ParentWindow> {
    let current = mux
        .current_pane_id()
        .and_then(|pane| mux.get_live_pane_info(&pane).ok().flatten())
        .and_then(|pane| pane.session);
    let mut windows: Vec<(bool, String)> = mux
        .get_all_live_pane_info()
        .ok()?
        .into_values()
        .filter(|pane| pane.window.as_deref() == Some(name))
        .filter_map(|pane| {
            Some((
                current.is_some() && pane.session == current,
                pane.window_id?,
            ))
        })
        .collect();
    windows.sort();
    let (here, id) = windows.pop()?;
    Some(ParentWindow { id, here })
}

/// The repository of `project_id` on this machine: from the records, else
/// the current repository when it is that project.
fn project_root(project_id: &str, records: &[WorkspaceRecord]) -> Option<PathBuf> {
    if let Some(record) = records
        .iter()
        .find(|record| record.project_id == project_id)
    {
        return Some(record.project_root.clone());
    }
    let root = crate::util::canon_or_self(&crate::git::get_main_worktree_root().ok()?);
    (cache::project_id_of(&root) == project_id).then_some(root)
}

fn context(project_root: &std::path::Path) -> Result<WorkflowContext> {
    let (config, location) = Config::load_with_location_from(project_root, None)?;
    WorkflowContext::new_in(
        project_root,
        config,
        create_backend(detect_backend()),
        location,
    )
}

/// A node binary name as `install` makes them: `workmux-<version>` or
/// `workmux-dev-<hash>`.
fn valid_pin(name: &str) -> bool {
    name.strip_prefix("workmux-").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
    })
}

/// Siblings this process gave up on, so observers do not retry them on
/// every frame.
fn refused() -> std::sync::MutexGuard<'static, HashSet<String>> {
    static REFUSED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    REFUSED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

/// Serializes adoption across processes.
fn lock(remote: &str) -> Result<File> {
    let file = cache::lock_file(remote, "adopt.lock")?;
    file.lock().context("Failed to lock adopt.lock")?;
    Ok(file)
}

fn report(foreground: bool, message: &str) {
    if foreground {
        eprintln!("workmux: {message}");
    } else {
        tracing::warn!("{message}");
    }
}

fn report_info(foreground: bool, message: &str) {
    if foreground {
        eprintln!("workmux: {message}");
    } else {
        tracing::info!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_look_like_installed_binaries() {
        assert!(valid_pin("workmux-0.1.80"));
        assert!(valid_pin("workmux-dev-ba7816bf8f01"));
        assert!(!valid_pin("workmux-"));
        assert!(!valid_pin("workmux-x;rm -rf ~"));
        assert!(!valid_pin("../workmux-1"));
        assert!(!valid_pin("sh"));
    }
}
