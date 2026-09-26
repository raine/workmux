//! Filesystem-based state storage for workmux agents.
//!
//! This module provides persistent state storage that works across all
//! terminal multiplexer backends (tmux, WezTerm, Zellij).

pub mod run;
pub mod store;
#[cfg(test)]
pub(crate) mod test_support;
mod types;

use std::time::{SystemTime, UNIX_EPOCH};

use tracing::warn;

use crate::agent_identity::classify_agent_kind;
use crate::multiplexer::{AgentStatus, Multiplexer};

pub use store::StateStore;
pub(crate) use store::{AgentStateCache, AgentStateSource, ResurrectionAgentState};
pub use types::{AgentState, LastDoneCycleState, PaneKey, RuntimeState};

/// Persist an agent state update to the StateStore.
///
/// For tmux, merges state from the same pane process and server lifecycle so
/// partial updates don't wipe other fields. Other backends merge by pane key:
/// - If `status` is Some, updates the agent's status. If None, preserves existing.
/// - If `title_override` is Some, uses it. If None, preserves existing stored title,
///   falling back to the live pane title.
/// - If `agent_session_id` is Some, uses it. If None, preserves the existing binding.
///
/// Logs warnings on failure without propagating errors (best-effort persistence).
pub fn persist_agent_update(
    mux: &dyn Multiplexer,
    pane_id: &str,
    status: Option<AgentStatus>,
    title_override: Option<String>,
    agent_session_id: Option<String>,
) {
    persist_agent_snapshot(mux, pane_id, status, title_override, agent_session_id, true);
}

/// Register a live agent pane without assigning it an activity status.
///
/// Registration snapshots only live pane data, preventing state from an
/// unrelated agent process in the same pane from leaking into this record. A
/// hook-provided session ID binds later pane-less events to this registration.
pub fn persist_agent_registration(
    mux: &dyn Multiplexer,
    pane_id: &str,
    agent_session_id: Option<String>,
) {
    persist_agent_snapshot(mux, pane_id, None, None, agent_session_id, false);
}

/// Clear an agent's persisted status without deleting its state record.
///
/// An explicit clear is not a status transition, so the pane identity and
/// metadata (PID, command, title, agent session, kind) are preserved. Only the
/// status fields are reset for the exact pane key, so other panes and other
/// multiplexer instances keep their stored status.
///
/// Logs warnings on failure without propagating errors (best-effort
/// persistence).
pub fn clear_agent_status(mux: &dyn Multiplexer, pane_id: &str) {
    let Ok(store) = StateStore::new() else {
        return;
    };
    let pane_key = PaneKey {
        backend: mux.name().to_string(),
        instance: mux.instance_id(),
        pane_id: pane_id.to_string(),
    };
    if let Err(error) = store.clear_agent_status(&pane_key) {
        warn!(%error, "failed to persist cleared agent status");
    }
}

fn persist_agent_snapshot(
    mux: &dyn Multiplexer,
    pane_id: &str,
    status: Option<AgentStatus>,
    title_override: Option<String>,
    agent_session_id: Option<String>,
    preserve_existing: bool,
) {
    let Ok(store) = StateStore::new() else {
        return;
    };
    if let Err(error) = store.with_agent_lock(|store| {
        persist_agent_snapshot_locked(
            store,
            mux,
            pane_id,
            status,
            title_override,
            agent_session_id,
            preserve_existing,
        );
        Ok(())
    }) {
        warn!(%error, "failed to lock agent state persistence");
    }
}

fn persist_agent_snapshot_locked(
    store: &StateStore,
    mux: &dyn Multiplexer,
    pane_id: &str,
    status: Option<AgentStatus>,
    title_override: Option<String>,
    agent_session_id: Option<String>,
    preserve_existing: bool,
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

    let (live_pid, boot_id) = if mux.name() == "tmux" {
        let Some(live_pid) = live_info.pid else {
            warn!(%pane_id, "tmux pane PID unavailable, skipping state persist");
            return;
        };
        let boot_id = match mux.server_boot_id() {
            Ok(Some(boot_id)) => Some(boot_id),
            Ok(None) => {
                warn!(%pane_id, "tmux server identity unavailable, skipping state persist");
                return;
            }
            Err(error) => {
                warn!(%pane_id, %error, "failed to get tmux server identity, skipping state persist");
                return;
            }
        };
        (live_pid, boot_id)
    } else {
        (
            live_info.pid.unwrap_or(0),
            mux.server_boot_id().unwrap_or(None),
        )
    };

    if let Some(boot_id) = boot_id.as_deref()
        && let Err(error) = store.compact_context_locked(mux.name(), &mux.instance_id(), boot_id)
    {
        warn!(%error, "failed to compact historical agent state");
        return;
    }

    // tmux pane PIDs and server lifecycles form a stable process identity.
    // Other backends retain their established pane-key merge behavior.
    let existing = preserve_existing
        .then(|| {
            store.get_agent(&pane_key).ok().flatten().filter(|state| {
                mux.name() != "tmux"
                    || (state.pane_pid == live_pid
                        && state
                            .boot_id
                            .as_deref()
                            .zip(boot_id.as_deref())
                            .is_some_and(|(stored, current)| {
                                store::same_server_boot("tmux", stored, current)
                            }))
            })
        })
        .flatten();

    // Resolve status: explicit update wins, otherwise preserve existing
    let final_status = status.or(existing.as_ref().and_then(|e| e.status));

    let previous_status = existing.as_ref().and_then(|state| state.status);

    // Preserve existing status_ts if status hasn't changed (avoids resetting timer)
    let status_ts = match final_status {
        None => None,
        Some(_) if final_status == previous_status => {
            Some(existing.as_ref().and_then(|e| e.status_ts).unwrap_or(now))
        }
        Some(_) => Some(now),
    };

    let previous_activity_ts = existing.as_ref().and_then(AgentState::activity_ts);
    let activity_ts = resolve_activity_ts(
        preserve_existing,
        status,
        previous_status,
        previous_activity_ts,
        now,
    );

    // Capture existing agent_kind before `existing` is consumed below.
    let existing_agent_kind = existing.as_ref().and_then(|e| e.agent_kind.clone());
    let agent_session_id = agent_session_id.or_else(|| {
        existing
            .as_ref()
            .and_then(|state| state.agent_session_id.clone())
    });

    // Snapshot the live title for classification before the resolved
    // `pane_title` consumes `live_info.title`.
    let live_title_for_classify = live_info.title.clone();

    // Resolve title: explicit override wins, then existing stored title, then live
    let pane_title = title_override
        .or_else(|| existing.as_ref().and_then(|state| state.pane_title.clone()))
        .or(live_info.title);

    // Classify the agent kind once and lock it in. The classifier sees the
    // *live* title (not the merged `pane_title` above, which prefers the
    // stored value): a stale stored title would otherwise re-confirm the
    // previous identity even after the foreground command has changed.
    // Reconcile clears entries when it observes a foreground command change.
    // This path does not use the command as process identity because a status
    // hook can temporarily become the pane's foreground command.
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
        status_ts,
        activity_ts,
        pane_title,
        pane_pid: live_pid,
        command: live_info.current_command.unwrap_or_default(),
        updated_ts: now,
        window_name: live_info.window,
        session_name: live_info.session,
        boot_id,
        agent_kind,
        agent_session_id,
    };

    if let Err(error) = store.upsert_agent_locked(&state) {
        warn!(%error, "failed to persist agent state");
    }
}

fn resolve_activity_ts(
    preserve_existing: bool,
    explicit_status: Option<AgentStatus>,
    previous_status: Option<AgentStatus>,
    previous_activity_ts: Option<u64>,
    now: u64,
) -> Option<u64> {
    if !preserve_existing
        || previous_activity_ts.is_none()
        || (explicit_status.is_some() && explicit_status != previous_status)
    {
        Some(now)
    } else {
        previous_activity_ts
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
    fn registration_starts_activity_without_status() {
        assert_eq!(
            resolve_activity_ts(false, None, Some(AgentStatus::Done), Some(10), 20),
            Some(20)
        );
    }

    #[test]
    fn first_status_starts_new_activity() {
        assert_eq!(
            resolve_activity_ts(true, Some(AgentStatus::Working), None, Some(10), 20),
            Some(20)
        );
    }

    #[test]
    fn status_transition_starts_new_activity() {
        assert_eq!(
            resolve_activity_ts(
                true,
                Some(AgentStatus::Done),
                Some(AgentStatus::Working),
                Some(10),
                20,
            ),
            Some(20)
        );
    }

    #[test]
    fn repeated_status_preserves_activity() {
        assert_eq!(
            resolve_activity_ts(
                true,
                Some(AgentStatus::Working),
                Some(AgentStatus::Working),
                Some(10),
                20,
            ),
            Some(10)
        );
    }

    #[test]
    fn metadata_update_preserves_activity() {
        assert_eq!(
            resolve_activity_ts(true, None, None, Some(10), 20),
            Some(10)
        );
    }

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
