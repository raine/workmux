//! What the daemon reads from the multiplexer each observation: the
//! sessions, windows and panes it shows, and which working agents have
//! stopped producing output.

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::multiplexer::{
    AgentPane, LivePaneInfo, Multiplexer, TmuxBackend,
    rex::{RexBackend, sidebar::RexSidebarObservation},
    zellij::{ZellijBackend, ZellijSidebarTab},
};

use super::super::host::SidebarHost;
use super::super::preferences::Preferences;
use super::super::store::tmux_preferences;

/// Result of one batched multiplexer query.
#[derive(Clone, Default)]
pub(super) struct MuxState {
    pub(super) live_panes: HashMap<String, LivePaneInfo>,
    pub(super) window_statuses: HashMap<String, Option<String>>,
    pub(super) active_windows: HashSet<(String, String)>,
    pub(super) pane_window_ids: HashMap<String, String>,
    pub(super) pane_window_indexes: HashMap<String, u32>,
    pub(super) active_pane_ids: HashSet<String>,
    pub(super) window_pane_counts: HashMap<String, usize>,
    pub(super) server_boot_id: Option<String>,
    pub(super) prefs: Preferences,
}

/// Query all sidebar-relevant tmux state in a single server observation.
fn query_tmux_state(tmux: &TmuxBackend) -> Result<MuxState> {
    let snapshot = tmux.sidebar_snapshot()?;
    let prefs = tmux_preferences(&snapshot);
    Ok(MuxState {
        live_panes: snapshot.live_panes,
        window_statuses: snapshot.window_statuses,
        active_windows: snapshot.active_windows,
        pane_window_ids: snapshot.pane_window_ids,
        pane_window_indexes: snapshot.pane_window_indexes,
        active_pane_ids: snapshot.active_pane_ids,
        window_pane_counts: snapshot.window_pane_counts,
        prefs,
        server_boot_id: snapshot.server_boot_id,
    })
}

/// Preferences that Zellij and Rex keep in the session state file.
fn file_preferences(instance_id: &str) -> Result<Preferences> {
    Ok(super::super::session_state::read(instance_id)?.prefs)
}

fn query_zellij_state(
    zellij: &ZellijBackend,
    instance_id: &str,
) -> Result<(MuxState, Vec<ZellijSidebarTab>)> {
    let observation = zellij.sidebar_observation()?;
    let snapshot = observation.snapshot;
    let mux_state = MuxState {
        live_panes: snapshot.live_panes,
        window_statuses: HashMap::new(),
        active_windows: snapshot.active_windows,
        pane_window_ids: snapshot.pane_window_ids,
        pane_window_indexes: snapshot.pane_window_indexes,
        active_pane_ids: snapshot.active_pane_ids,
        window_pane_counts: snapshot.window_pane_counts,
        prefs: file_preferences(instance_id)?,
        ..MuxState::default()
    };
    Ok((mux_state, observation.tabs))
}

fn query_rex_state(instance_id: &str) -> Result<(MuxState, RexSidebarObservation)> {
    let observation = RexBackend::new().sidebar_observation()?;
    let mux_state = MuxState {
        live_panes: observation.live_panes.clone(),
        window_statuses: HashMap::new(),
        active_windows: observation.active_windows.clone(),
        pane_window_ids: observation.pane_window_ids.clone(),
        pane_window_indexes: observation.pane_window_indexes.clone(),
        active_pane_ids: observation.active_pane_ids.clone(),
        window_pane_counts: observation.window_pane_counts.clone(),
        server_boot_id: observation.boot_id.clone(),
        prefs: file_preferences(instance_id)?,
    };
    Ok((mux_state, observation))
}

/// What a backend whose sidebars the daemon places needs besides `MuxState`.
pub(super) enum PlacementObservation {
    None,
    Zellij(Vec<ZellijSidebarTab>),
    Rex(Box<RexSidebarObservation>),
}

pub(super) fn query_mux_state(host: &SidebarHost) -> Result<(MuxState, PlacementObservation)> {
    match host {
        SidebarHost::Tmux(tmux) => Ok((
            query_tmux_state(tmux.backend())?,
            PlacementObservation::None,
        )),
        SidebarHost::Zellij { instance_id } => {
            let (state, tabs) =
                query_zellij_state(&ZellijBackend::for_session(instance_id), instance_id)?;
            Ok((state, PlacementObservation::Zellij(tabs)))
        }
        SidebarHost::Rex { instance_id } => {
            let (state, observation) = query_rex_state(instance_id)?;
            Ok((state, PlacementObservation::Rex(Box::new(observation))))
        }
    }
}

pub(super) fn include_unregistered_zellij_agents(
    agents: &mut Vec<AgentPane>,
    live_panes: &HashMap<String, LivePaneInfo>,
) {
    let mut known_panes: HashSet<String> =
        agents.iter().map(|agent| agent.pane_id.clone()).collect();
    for (pane_id, live) in live_panes {
        if known_panes.contains(pane_id.as_str()) {
            continue;
        }
        let Some(command) = live.current_command.as_deref() else {
            continue;
        };
        let Some(agent_kind) = crate::agent_identity::classify_agent_kind(Some(command), None)
        else {
            continue;
        };
        known_panes.insert(pane_id.clone());
        agents.push(AgentPane {
            session: live.session.clone().unwrap_or_default(),
            window_name: live.window.clone().unwrap_or_default(),
            pane_id: pane_id.clone(),
            window_id: live.window_id.clone().unwrap_or_default(),
            window_index: live.window_index,
            path: live.working_dir.clone(),
            pane_title: live.title.clone(),
            status: None,
            status_ts: None,
            activity_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: Some(command.to_string()),
            agent_kind: Some(agent_kind),
            prompt: None,
            remote: None,
        });
    }
}

/// Detects working agents that have stopped producing output.
///
/// # Behavior
/// - A working agent with no pane output and no RPC activity for >= timeout
///   is considered interrupted.
/// - New pane output or an RPC update clears interrupted state.
/// - After clearing, the agent gets a fresh timeout window before it can
///   be marked interrupted again.
/// - Interrupted agents show no icon and no timer in the sidebar.
/// - When an agent resumes, the timer resets to zero.
pub(super) struct InactivityTracker {
    /// pane_id -> (server lifecycle, pane PID)
    identities: HashMap<String, (Option<String>, u32)>,
    /// pane_id -> (content_hash, first_seen_at, updated_ts at recording time)
    entries: HashMap<String, (u64, Instant, u64)>,
    /// pane_id -> updated_ts at the time interruption was confirmed.
    /// Cleared when pane content or updated_ts changes.
    confirmed: HashMap<String, u64>,
    /// How long content must be unchanged before marking as interrupted.
    timeout: Duration,
}

impl InactivityTracker {
    pub(super) fn new(timeout: Duration) -> Self {
        Self {
            identities: HashMap::new(),
            entries: HashMap::new(),
            confirmed: HashMap::new(),
            timeout,
        }
    }

    pub(super) fn reconcile_identities(
        &mut self,
        live_panes: &HashMap<String, LivePaneInfo>,
        server_boot_id: Option<&str>,
    ) {
        let current: HashMap<String, (Option<String>, u32)> = live_panes
            .iter()
            .filter_map(|(pane_id, pane)| {
                pane.pid
                    .map(|pid| (pane_id.clone(), (server_boot_id.map(str::to_string), pid)))
            })
            .collect();
        for (pane_id, identity) in &current {
            if self
                .identities
                .get(pane_id)
                .is_some_and(|previous| previous != identity)
            {
                self.entries.remove(pane_id);
                self.confirmed.remove(pane_id);
            }
        }
        self.entries
            .retain(|pane_id, _| current.contains_key(pane_id));
        self.confirmed
            .retain(|pane_id, _| current.contains_key(pane_id));
        self.identities = current;
    }

    /// Check all working agents for inactivity. Returns the set of pane IDs
    /// that appear interrupted (content unchanged for longer than timeout).
    pub(super) fn check_with(
        &mut self,
        agents: &[crate::multiplexer::AgentPane],
        now: Instant,
        capture: impl Fn(&str) -> Option<String>,
    ) -> HashSet<String> {
        use std::hash::{Hash, Hasher};

        // Build lookup of working agents
        let working: HashMap<&str, &crate::multiplexer::AgentPane> = agents
            .iter()
            .filter(|a| a.status == Some(crate::multiplexer::AgentStatus::Working))
            .map(|a| (a.pane_id.as_str(), a))
            .collect();

        // Remove entries for agents no longer in Working status
        self.entries
            .retain(|id, _| working.contains_key(id.as_str()));
        self.confirmed
            .retain(|id, _| working.contains_key(id.as_str()));

        // Clear interrupted state if the agent's state was updated via RPC
        // (updated_ts changed since we confirmed the interruption).
        // Collect resumed pane IDs first, then clear their entries for a fresh
        // inactivity window.
        let resumed: Vec<String> = self
            .confirmed
            .iter()
            .filter(|(id, confirmed_ts)| {
                working
                    .get(id.as_str())
                    .is_some_and(|a| a.updated_ts.unwrap_or(0) > **confirmed_ts)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &resumed {
            if let Some(confirmed_ts) = self.confirmed.remove(id) {
                let updated_ts = working
                    .get(id.as_str())
                    .and_then(|a| a.updated_ts)
                    .unwrap_or(0);
                tracing::info!(
                    pane_id = %id,
                    confirmed_ts,
                    updated_ts,
                    "agent inactivity cleared"
                );
            }
            self.entries.remove(id);
        }

        for (pane_id, agent) in &working {
            let Some(raw) = capture(pane_id) else {
                continue;
            };

            // Strip ANSI escapes and normalize whitespace for stable hashing
            let stripped = console::strip_ansi_codes(&raw);
            let normalized = stripped.trim();

            let mut hasher = std::hash::DefaultHasher::new();
            normalized.hash(&mut hasher);
            let hash = hasher.finish();

            let current_rpc = agent.updated_ts.unwrap_or(0);

            match self.entries.get(*pane_id) {
                Some(&(prev_hash, first_seen, prev_rpc))
                    if prev_hash == hash && prev_rpc == current_rpc =>
                {
                    // Same content and same RPC state: check timeout
                    let idle_for = now.duration_since(first_seen);
                    if idle_for >= self.timeout
                        && self
                            .confirmed
                            .insert(pane_id.to_string(), current_rpc)
                            .is_none()
                    {
                        tracing::info!(
                            pane_id = %pane_id,
                            updated_ts = current_rpc,
                            idle_for_ms = idle_for.as_millis(),
                            timeout_ms = self.timeout.as_millis(),
                            "agent inactivity detected"
                        );
                    }
                }
                _ => {
                    // Content changed or RPC updated: reset inactivity window
                    self.confirmed.remove(*pane_id);
                    self.entries
                        .insert(pane_id.to_string(), (hash, now, current_rpc));
                }
            }
        }

        self.confirmed.keys().cloned().collect()
    }
}

/// Whether the agent's local pane shows its output as it happens. A remote
/// agent's does only while a connected relay draws it.
pub(super) fn shows_live_output(agent: &crate::multiplexer::AgentPane) -> bool {
    agent
        .remote
        .as_ref()
        .is_none_or(|remote| remote.attached && remote.connected)
}

/// Capture working panes, including interrupted ones, to detect resumed output.
pub(super) fn gather_captures(
    agents: &[crate::multiplexer::AgentPane],
    live_panes: &HashMap<String, LivePaneInfo>,
    mux: &dyn Multiplexer,
) -> HashMap<String, String> {
    agents
        .iter()
        .filter(|a| a.status == Some(crate::multiplexer::AgentStatus::Working))
        .filter(|a| shows_live_output(a))
        .filter_map(|a| {
            match live_panes.get(&a.pane_id) {
                Some(info) => mux.capture_pane_at(&a.pane_id, info, 5),
                None => mux.capture_pane(&a.pane_id, 5),
            }
            .map(|content| (a.pane_id.clone(), content))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{done_agent, working_agent};
    use super::*;

    use crate::multiplexer::AgentPane;
    use std::cell::RefCell;
    use std::path::PathBuf;

    #[test]
    fn pane_lifecycle_replacement_resets_inactivity_tracking() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();
        let pane = |pid| LivePaneInfo {
            pid: Some(pid),
            current_command: Some("agent".into()),
            working_dir: PathBuf::new(),
            title: None,
            session: Some("session".into()),
            window: Some("window".into()),
            session_id: Some("$1".into()),
            window_id: Some("@1".into()),
            window_index: None,
        };

        tracker.reconcile_identities(
            &HashMap::from([("%1".to_string(), pane(10))]),
            Some("server-a"),
        );
        tracker.check_with(&agents, t0, |_| Some("same".into()));
        assert!(
            tracker
                .check_with(&agents, t0 + Duration::from_secs(11), |_| Some(
                    "same".into()
                ))
                .contains("%1")
        );

        tracker.reconcile_identities(
            &HashMap::from([("%1".to_string(), pane(11))]),
            Some("server-a"),
        );
        assert!(
            tracker
                .check_with(&agents, t0 + Duration::from_secs(12), |_| Some(
                    "same".into()
                ))
                .is_empty()
        );
    }

    #[test]
    fn server_lifecycle_replacement_resets_interruption() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();
        let panes = HashMap::from([(
            "%1".to_string(),
            LivePaneInfo {
                pid: Some(10),
                current_command: Some("agent".into()),
                working_dir: PathBuf::new(),
                title: None,
                session: Some("session".into()),
                window: Some("window".into()),
                session_id: Some("$1".into()),
                window_id: Some("@1".into()),
                window_index: None,
            },
        )]);

        tracker.reconcile_identities(&panes, Some("server-a"));
        tracker.check_with(&agents, t0, |_| Some("same".into()));
        tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("same".into())
        });
        tracker.reconcile_identities(&panes, Some("server-b"));

        assert!(
            tracker
                .check_with(&agents, t0 + Duration::from_secs(12), |_| Some(
                    "same".into()
                ))
                .is_empty()
        );
    }

    #[test]
    fn no_interruption_before_timeout() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // First check: records the hash
        let result = tracker.check_with(&agents, t0, |_| Some("hello".into()));
        assert!(result.is_empty());

        // 5s later, same content: not yet interrupted
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(5), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());
    }

    #[test]
    fn interruption_after_timeout() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // First check records hash
        tracker.check_with(&agents, t0, |_| Some("hello".into()));

        // 11s later, same content: interrupted
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });
        assert!(result.contains("%1"));
    }

    #[test]
    fn changing_content_resets_window() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // First check
        tracker.check_with(&agents, t0, |_| Some("hello".into()));

        // 8s later, content changes: resets the window
        tracker.check_with(&agents, t0 + Duration::from_secs(8), |_| {
            Some("world".into())
        });

        // 5s after the change (13s total): not interrupted (only 5s since reset)
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(13), |_| {
            Some("world".into())
        });
        assert!(result.is_empty());

        // 11s after the change (19s total): now interrupted
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(19), |_| {
            Some("world".into())
        });
        assert!(result.contains("%1"));
    }

    #[test]
    fn content_change_clears_interruption_and_resets_window() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Become interrupted
        tracker.check_with(&agents, t0, |_| Some("hello".into()));
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });
        assert!(result.contains("%1"));

        // Output resumes without a status update.
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(12), |_| {
            Some("new output".into())
        });
        assert!(result.is_empty());

        let result = tracker.check_with(&agents, t0 + Duration::from_secs(17), |_| {
            Some("new output".into())
        });
        assert!(result.is_empty());

        let result = tracker.check_with(&agents, t0 + Duration::from_secs(23), |_| {
            Some("new output".into())
        });
        assert!(result.contains("%1"));
    }

    #[test]
    fn clears_on_updated_ts_change() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Become interrupted
        tracker.check_with(&agents, t0, |_| Some("hello".into()));
        tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });

        // Agent sends new RPC (updated_ts changes): clears interrupted
        let resumed_agents = vec![working_agent("%1", 2)];
        let result = tracker.check_with(&resumed_agents, t0 + Duration::from_secs(12), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());
    }

    #[test]
    fn fresh_window_after_resume() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Become interrupted
        tracker.check_with(&agents, t0, |_| Some("hello".into()));
        tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });

        // Resume (updated_ts changes) at t=12s
        let resumed = vec![working_agent("%1", 2)];
        tracker.check_with(&resumed, t0 + Duration::from_secs(12), |_| {
            Some("hello".into())
        });

        // 5s after resume (t=17s): same content but not interrupted yet (fresh window)
        let result = tracker.check_with(&resumed, t0 + Duration::from_secs(17), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());

        // 11s after resume (t=23s): now interrupted again
        let result = tracker.check_with(&resumed, t0 + Duration::from_secs(23), |_| {
            Some("hello".into())
        });
        assert!(result.contains("%1"));
    }

    #[test]
    fn non_working_agents_ignored() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![done_agent("%1")];
        let t0 = Instant::now();

        tracker.check_with(&agents, t0, |_| Some("hello".into()));
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());
    }

    #[test]
    fn leaves_working_clears_tracking() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let working = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Become interrupted
        tracker.check_with(&working, t0, |_| Some("hello".into()));
        tracker.check_with(&working, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });

        // Agent transitions to Done
        let done = vec![done_agent("%1")];
        let result = tracker.check_with(&done, t0 + Duration::from_secs(12), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());

        // Comes back as Working: starts fresh
        let working_again = vec![working_agent("%1", 3)];
        let result = tracker.check_with(&working_again, t0 + Duration::from_secs(13), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty()); // just recorded, not yet timed out
    }

    #[test]
    fn capture_failure_skips_pane() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Capture fails: no entry recorded
        tracker.check_with(&agents, t0, |_| None);
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| None);
        assert!(result.is_empty());
    }

    #[test]
    fn multiple_agents_tracked_independently() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1), working_agent("%2", 1)];
        let t0 = Instant::now();

        let content = RefCell::new(HashMap::from([
            ("%1".to_string(), "static".to_string()),
            ("%2".to_string(), "changing".to_string()),
        ]));

        // First check
        tracker.check_with(&agents, t0, |id| content.borrow().get(id).cloned());

        // Change %2's content at 5s
        content
            .borrow_mut()
            .insert("%2".to_string(), "new output".into());
        tracker.check_with(&agents, t0 + Duration::from_secs(5), |id| {
            content.borrow().get(id).cloned()
        });

        // At 11s: %1 is interrupted (11s unchanged), %2 is not (only 6s since change)
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |id| {
            content.borrow().get(id).cloned()
        });
        assert_eq!(result, HashSet::from(["%1".to_string()]));
    }

    #[test]
    fn rpc_update_before_timeout_resets_window() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let t0 = Instant::now();

        // Start tracking
        tracker.check_with(&[working_agent("%1", 1)], t0, |_| Some("hello".into()));

        // Agent sends RPC at 5s (updated_ts changes) but content unchanged
        tracker.check_with(
            &[working_agent("%1", 2)],
            t0 + Duration::from_secs(5),
            |_| Some("hello".into()),
        );

        // At 11s: only 6s since RPC update, should NOT be interrupted
        let result = tracker.check_with(
            &[working_agent("%1", 2)],
            t0 + Duration::from_secs(11),
            |_| Some("hello".into()),
        );
        assert!(result.is_empty());

        // At 16s: 11s since RPC update, now interrupted
        let result = tracker.check_with(
            &[working_agent("%1", 2)],
            t0 + Duration::from_secs(16),
            |_| Some("hello".into()),
        );
        assert_eq!(result, HashSet::from(["%1".to_string()]));
    }

    #[test]
    fn interruption_at_exact_timeout() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        tracker.check_with(&agents, t0, |_| Some("hello".into()));
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(10), |_| {
            Some("hello".into())
        });
        assert_eq!(result, HashSet::from(["%1".to_string()]));
    }

    #[test]
    fn ansi_and_whitespace_normalized() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Plain text first
        tracker.check_with(&agents, t0, |_| Some("hello\n".into()));

        // Same text wrapped in ANSI codes + trailing whitespace: should hash the same
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("\x1b[31mhello\x1b[0m   ".into())
        });
        assert_eq!(result, HashSet::from(["%1".to_string()]));
    }

    #[test]
    fn capture_failure_does_not_create_baseline() {
        let mut tracker = InactivityTracker::new(Duration::from_secs(10));
        let agents = vec![working_agent("%1", 1)];
        let t0 = Instant::now();

        // Capture fails: no baseline recorded
        tracker.check_with(&agents, t0, |_| None);

        // Capture succeeds later: this is the first successful capture, not a timeout
        let result = tracker.check_with(&agents, t0 + Duration::from_secs(11), |_| {
            Some("hello".into())
        });
        assert!(result.is_empty());
    }

    #[test]
    fn idle_zellij_agent_is_visible_before_status_hook_registration() {
        let mut agents = Vec::new();
        let live_panes = HashMap::from([(
            "terminal_1".to_string(),
            LivePaneInfo {
                pid: None,
                current_command: Some("copilot --resume".to_string()),
                working_dir: PathBuf::from("/repo/worktree"),
                title: Some("Feature work - GitHub Copilot".to_string()),
                session: Some("zellij-session".to_string()),
                window: Some("feature".to_string()),
                session_id: None,
                window_id: Some("7".to_string()),
                window_index: Some(1),
            },
        )]);

        include_unregistered_zellij_agents(&mut agents, &live_panes);

        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].pane_id, "terminal_1");
        assert_eq!(agents[0].agent_kind.as_deref(), Some("copilot"));
        assert_eq!(agents[0].status, None);
    }

    #[test]
    fn registered_zellij_agent_is_not_duplicated() {
        let live = LivePaneInfo {
            pid: None,
            current_command: Some("copilot --resume".to_string()),
            working_dir: PathBuf::from("/repo/worktree"),
            title: Some("Feature work - GitHub Copilot".to_string()),
            session: Some("zellij-session".to_string()),
            window: Some("feature".to_string()),
            session_id: None,
            window_id: Some("7".to_string()),
            window_index: Some(1),
        };
        let mut agents = vec![AgentPane {
            session: "zellij-session".to_string(),
            window_name: "feature".to_string(),
            pane_id: "terminal_1".to_string(),
            window_id: "7".to_string(),
            window_index: Some(1),
            path: PathBuf::from("/repo/worktree"),
            pane_title: live.title.clone(),
            status: Some(crate::multiplexer::AgentStatus::Working),
            status_ts: Some(1),
            activity_ts: Some(1),
            updated_ts: Some(1),
            window_cmd: None,
            agent_command: Some("copilot".to_string()),
            agent_kind: Some("copilot".to_string()),
            prompt: None,
            remote: None,
        }];

        include_unregistered_zellij_agents(
            &mut agents,
            &HashMap::from([("terminal_1".to_string(), live)]),
        );

        assert_eq!(agents.len(), 1);
        assert_eq!(
            agents[0].status,
            Some(crate::multiplexer::AgentStatus::Working)
        );
    }
}
