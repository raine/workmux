//! `workmux remote gc <name>`: list what a remote holds that no workspace
//! accounts for, plus laptop records of workspaces the remote no longer
//! has, and remove them once confirmed.

use std::collections::HashSet;
use std::io::IsTerminal;

use anyhow::{Result, bail};

use super::RemoteSpec;
use super::cache::{self, WorkspaceRecord};
use super::client::Node;
use super::install::Pin;
use super::node::gc::{GcRequest, GcResponse};
use super::protocol;
use crate::config::Config;
use crate::multiplexer::{create_backend, detect_backend};
use crate::ui::confirm::{ConfirmDefault, confirm};

/// Records younger than this may belong to an `add --remote` in progress.
const GRACE_SECS: u64 = 10 * 60;

pub fn run(config: &Config, name: &str, dry_run: bool, force: bool) -> Result<()> {
    let spec = RemoteSpec::resolve(config, name)?;
    let node = Node::connect_reader(&spec, protocol::GC)?;
    super::observe::current(config, name);
    let records: Vec<WorkspaceRecord> = cache::all()
        .into_iter()
        .filter(|record| record.remote == name)
        .collect();
    let mut keep_pins = vec![Pin::for_spec(&spec)?.name];
    keep_pins.extend(
        records
            .iter()
            .filter_map(|record| record.execution.as_ref())
            .map(|execution| execution.node_bin.clone()),
    );
    let request = GcRequest {
        known: records.iter().map(|record| record.ws_id.clone()).collect(),
        keep_pins,
        apply: Vec::new(),
        force,
    };
    let listed: GcResponse = node.call(protocol::GC, &request, false)?;
    let on_node: HashSet<&str> = listed.workspaces.iter().map(String::as_str).collect();
    let now = cache::now();
    // A record bringing work home is finished by `homecoming`, not here.
    let stale: Vec<&WorkspaceRecord> = records
        .iter()
        .filter(|record| {
            record.homecoming.is_none()
                && !on_node.contains(record.ws_id.as_str())
                && now.saturating_sub(record.created_at) >= GRACE_SECS
        })
        .collect();

    for line in &listed.kept {
        println!("Keeping {line}");
    }
    if listed.findings.is_empty() && stale.is_empty() {
        println!("Nothing to clean up on '{name}'.");
        return Ok(());
    }
    println!("Unused on '{name}':");
    for finding in &listed.findings {
        let note = if finding.unsaved && !force {
            " (kept without --force)"
        } else {
            ""
        };
        println!("  {}{note}", finding.detail);
    }
    for record in &stale {
        println!(
            "  local record of '{}', which '{name}' no longer has",
            record.handle
        );
    }
    if dry_run {
        return Ok(());
    }
    if !force {
        if !std::io::stdin().is_terminal() {
            bail!("Nothing removed: pass --force to remove without asking, or --dry-run to list");
        }
        if !confirm("Remove these?", ConfirmDefault::No)? {
            println!("Nothing removed.");
            return Ok(());
        }
    }

    let apply: Vec<String> = listed
        .findings
        .iter()
        .filter(|finding| force || !finding.unsaved)
        .map(|finding| finding.id.clone())
        .collect();
    let applied: GcResponse = node.call(protocol::GC, &GcRequest { apply, ..request }, true)?;
    let mut removed = applied.removed.len();
    let mut failed: Vec<String> = applied
        .failed
        .iter()
        .map(|failure| format!("{}: {}", failure.id, failure.message))
        .collect();
    // A record whose workspace reached the node meanwhile (a resumed
    // `add --remote`) is kept.
    let on_node: HashSet<&str> = applied.workspaces.iter().map(String::as_str).collect();
    let mux = create_backend(detect_backend());
    for record in cache::all().iter().filter(|current| {
        current.remote == name
            && current.homecoming.is_none()
            && stale.iter().any(|record| record.ws_id == current.ws_id)
            && !on_node.contains(current.ws_id.as_str())
    }) {
        match super::lifecycle::forget_locally(mux.as_ref(), record) {
            Ok(()) => removed += 1,
            Err(err) => failed.push(format!("record {}: {err:#}", record.handle)),
        }
    }
    println!("✓ Removed {removed} item(s) from '{name}'");
    for failure in &failed {
        eprintln!("✗ {failure}");
    }
    if !failed.is_empty() {
        bail!("{} item(s) could not be removed", failed.len());
    }
    Ok(())
}
