//! Filesystem-based state storage for workmux agents.
//!
//! This module provides persistent state storage that works across all
//! terminal multiplexer backends (tmux, WezTerm, Zellij).

pub mod codex_status;
pub mod run;
pub mod store;
#[cfg(test)]
pub(crate) mod test_support;
mod types;

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tracing::warn;

use crate::agent_identity::classify_agent_kind;
use crate::multiplexer::{AgentStatus, Multiplexer};

pub use store::StateStore;
pub use types::{AgentState, LastDoneCycleState, PaneKey, RuntimeState};

pub(crate) fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, content).context("Failed to write temp file")?;
    fs::rename(&tmp, path).context("Failed to rename temp file")?;
    Ok(())
}

/// Persist an agent state update to the StateStore.
///
/// Merges with existing state so partial updates don't wipe other fields:
/// - If `status` is Some, updates the agent's status. If None, preserves existing.
/// - If `title_override` is Some, uses it. If None, preserves existing stored title,
///   falling back to the live pane title.
///
/// Logs warnings on failure without propagating errors (best-effort persistence).
pub fn persist_agent_update(
    mux: &dyn Multiplexer,
    pane_id: &str,
    status: Option<AgentStatus>,
    title_override: Option<String>,
) {
    let pane_key = PaneKey {
        backend: mux.name().to_string(),
        instance: mux.instance_id(),
        pane_id: pane_id.to_string(),
    };

    let live_info = match mux.get_live_pane_info(pane_id) {
        Ok(Some(info)) => info,
        Ok(None) => {
            warn!(%pane_id, "pane not found, skipping state persist");
            return;
        }
        Err(e) => {
            warn!(error = %e, "failed to get live pane info, skipping state persist");
            return;
        }
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Load existing state to merge with
    let existing = StateStore::new()
        .ok()
        .and_then(|store| store.get_agent(&pane_key).ok().flatten());

    // Resolve status: explicit update wins, otherwise preserve existing
    let final_status = status.or(existing.as_ref().and_then(|e| e.status));

    // Preserve existing status_ts if status hasn't changed (avoids resetting timer)
    let status_ts = if final_status == existing.as_ref().and_then(|e| e.status) {
        existing.as_ref().and_then(|e| e.status_ts).unwrap_or(now)
    } else {
        now
    };

    // Capture existing agent_kind before `existing` is consumed below.
    let existing_agent_kind = existing.as_ref().and_then(|e| e.agent_kind.clone());

    // Snapshot the live title for classification before the resolved
    // `pane_title` consumes `live_info.title`.
    let live_title_for_classify = live_info.title.clone();

    // Resolve title: explicit override wins, then existing stored title, then live
    let pane_title = title_override
        .or(existing.and_then(|e| e.pane_title))
        .or(live_info.title);

    // Get server boot ID for crash detection (best-effort)
    let boot_id = mux.server_boot_id().unwrap_or(None);

    // Classify the agent kind once and lock it in. The classifier sees the
    // *live* title (not the merged `pane_title` above, which prefers the
    // stored value): a stale stored title would otherwise re-confirm the
    // previous identity even after the foreground command has changed.
    // Pane reuse (Claude exits, another agent launches in the same pane) is
    // handled by reconcile in `state::store`, which deletes the stored
    // entry on `command` change before this path runs again.
    let agent_kind = merge_agent_kind(
        classify_agent_kind(
            live_info.current_command.as_deref(),
            live_title_for_classify.as_deref(),
        ),
        existing_agent_kind,
    );

    let state = AgentState {
        pane_key,
        workdir: live_info.working_dir,
        status: final_status,
        status_ts: Some(status_ts),
        pane_title,
        pane_pid: live_info.pid.unwrap_or(0),
        command: live_info.current_command.unwrap_or_default(),
        updated_ts: now,
        window_name: live_info.window,
        session_name: live_info.session,
        boot_id,
        agent_kind,
    };

    if let Ok(store) = StateStore::new()
        && let Err(e) = store.upsert_agent(&state)
    {
        warn!(error = %e, "failed to persist agent state");
    }
}

/// Merge a freshly classified agent kind with the previously cached one.
///
/// Locks in the first definitive answer: once `existing` is `Some(_)`, that
/// value is preserved. This guards against title drift (a non-agent process
/// printing a substring like "Vibe" or "◇" into the pane title and stealing
/// the cached identity). Pane reuse is handled separately by reconcile,
/// which removes the stored entry when `pane_current_command` changes.
fn merge_agent_kind(new: Option<String>, existing: Option<String>) -> Option<String> {
    existing.or(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_existing_when_new_is_none() {
        let merged = merge_agent_kind(None, Some("claude".into()));
        assert_eq!(merged, Some("claude".into()));
    }

    #[test]
    fn merge_preserves_existing_against_drift() {
        // Existing was correctly classified; a later tick whose title drifted
        // into another agent's fingerprint must not overwrite it.
        let merged = merge_agent_kind(Some("vibe".into()), Some("claude".into()));
        assert_eq!(merged, Some("claude".into()));
    }

    #[test]
    fn merge_returns_none_when_both_none() {
        assert_eq!(merge_agent_kind(None, None), None);
    }

    #[test]
    fn merge_classifies_when_existing_is_none() {
        let merged = merge_agent_kind(Some("claude".into()), None);
        assert_eq!(merged, Some("claude".into()));
    }
}
