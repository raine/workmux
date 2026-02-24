//! Polling-based status detection for agents without hooks support.
//!
//! This module provides a `StatusPoller` that periodically captures terminal
//! output from agent panes and detects their status via pattern matching.
//! Used for agents like Copilot CLI that don't have hook/plugin systems.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use tracing::{debug, trace, warn};

use crate::config::Config;
use crate::multiplexer::agent::resolve_profile;
use crate::multiplexer::{AgentStatus, Multiplexer};

/// Number of terminal lines to capture for pattern matching.
const CAPTURE_LINES: u16 = 30;

/// Number of consecutive polls with identical content before inferring Done.
/// With a 3-second poll interval this means ~6 seconds of idle before Done.
const STABLE_THRESHOLD: u32 = 2;

/// Per-pane tracking state for the poller.
#[derive(Debug)]
struct PollState {
    status: AgentStatus,
    content_hash: u64,
    /// How many consecutive polls returned the same content hash.
    stable_count: u32,
}

type StatusCache = HashMap<String, PollState>;

fn hash_content(s: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Polls agent panes for status changes using terminal output pattern matching.
///
/// The poller runs in a background thread and periodically:
/// 1. Queries all live panes from the multiplexer
/// 2. For panes running agents that need polling, captures terminal output
/// 3. Detects status using the agent profile's pattern matching
/// 4. Updates the status if it has changed
pub struct StatusPoller {
    /// Signal to stop the polling loop
    stop_signal: Arc<AtomicBool>,
    /// Handle to the polling thread
    thread_handle: Option<thread::JoinHandle<()>>,
}

impl StatusPoller {
    /// Start a new status poller with the given interval.
    ///
    /// The poller runs in a background thread and polls at the specified interval.
    /// Call `stop()` to gracefully shut down the poller.
    pub fn start(mux: Arc<dyn Multiplexer>, interval: Duration) -> Self {
        let stop_signal = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_signal.clone();

        let handle = thread::spawn(move || {
            poll_loop(mux, interval, stop_clone);
        });

        StatusPoller {
            stop_signal,
            thread_handle: Some(handle),
        }
    }

    /// Stop the poller gracefully.
    ///
    /// Signals the polling loop to stop and waits for it to finish.
    pub fn stop(mut self) {
        self.stop_signal.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }

    /// Check if the poller is still running.
    pub fn is_running(&self) -> bool {
        !self.stop_signal.load(Ordering::SeqCst)
    }
}

impl Drop for StatusPoller {
    fn drop(&mut self) {
        self.stop_signal.store(true, Ordering::SeqCst);
        // Note: We don't join here to avoid blocking in drop
    }
}

/// Main polling loop that runs in a background thread.
fn poll_loop(mux: Arc<dyn Multiplexer>, interval: Duration, stop_signal: Arc<AtomicBool>) {
    let mut status_cache: StatusCache = HashMap::new();
    let config = Config::load(None).unwrap_or_default();

    debug!(interval_ms = interval.as_millis(), "status poller started");

    while !stop_signal.load(Ordering::SeqCst) {
        if let Err(e) = poll_once(&*mux, &mut status_cache, &config) {
            warn!(error = %e, "status poll cycle failed");
        }

        // Sleep in small increments to allow faster shutdown
        let sleep_increment = Duration::from_millis(100);
        let mut remaining = interval;
        while remaining > Duration::ZERO && !stop_signal.load(Ordering::SeqCst) {
            let sleep_time = remaining.min(sleep_increment);
            thread::sleep(sleep_time);
            remaining = remaining.saturating_sub(sleep_time);
        }
    }

    debug!("status poller stopped");
}

/// Perform a single poll cycle across all live panes.
///
/// Uses a hybrid approach for status detection:
/// - **Waiting**: Pattern matching on recent terminal lines (permission prompts
///   are distinctive and should always be detected immediately).
/// - **Working vs Done**: Content-change detection. If the terminal output
///   changed since the last poll, the agent is actively producing output
///   (Working). If the content has been stable for multiple consecutive polls,
///   the agent is idle (Done). This avoids false positives from old spinner
///   chars and tool-name artifacts that linger in the visible terminal buffer.
fn poll_once(
    mux: &dyn Multiplexer,
    status_cache: &mut StatusCache,
    config: &Config,
) -> anyhow::Result<()> {

    // Get ALL live panes from the multiplexer
    let live_panes = mux.get_all_live_pane_info()?;

    debug!(pane_count = live_panes.len(), "polling live panes");

    for (pane_id, pane_info) in &live_panes {
        // Get the agent profile based on the running command
        let profile = resolve_profile(Some(&pane_info.current_command));

        // Skip panes that don't need polling (not a polling-based agent)
        if !profile.needs_polling() {
            trace!(
                pane_id,
                command = %pane_info.current_command,
                "skipping non-polling pane"
            );
            continue;
        }

        debug!(
            pane_id,
            command = %pane_info.current_command,
            agent = profile.name(),
            "found polling-based agent"
        );

        // Capture terminal content
        let Some(raw_content) = mux.capture_pane(pane_id, CAPTURE_LINES) else {
            debug!(pane_id, "failed to capture pane content");
            continue;
        };

        // Strip ANSI escape codes so pattern matching works regardless of
        // whether the multiplexer returns colored output (e.g. tmux -e flag).
        let content = strip_ansi(&raw_content);
        let content_hash = hash_content(&content);
        let prev = status_cache.get(pane_id.as_str());

        // --- Detect status ---

        // 1. Waiting: pattern-match on recent lines (highest priority).
        //    Permission prompts are distinctive and need immediate detection.
        let detected_status = if let Some(patterns) = profile.status_patterns() {
            let lines: Vec<&str> = content.lines().collect();
            let recent_start = lines.len().saturating_sub(5);
            let recent_text: String = lines[recent_start..].join("\n");

            if patterns.waiting.iter().any(|p| recent_text.contains(p)) {
                AgentStatus::Waiting
            } else if let Some(prev) = prev {
                // 2. Content-change detection for working/done.
                if content_hash != prev.content_hash {
                    AgentStatus::Working
                } else if prev.stable_count + 1 >= STABLE_THRESHOLD {
                    AgentStatus::Done
                } else {
                    // Below threshold — keep current status, bump counter below
                    prev.status
                }
            } else {
                // 3. First poll for this pane — use pattern matching as a
                //    one-shot fallback until we have a second sample to compare.
                profile.detect_status(&content).unwrap_or(AgentStatus::Done)
            }
        } else {
            continue;
        };

        // Update stable counter
        let stable_count = match prev {
            Some(p) if content_hash == p.content_hash => p.stable_count + 1,
            _ => 0,
        };

        // Check if status actually changed
        let status_changed = prev.map(|p| p.status) != Some(detected_status);

        // Always update the cache (hash + counter must stay current)
        status_cache.insert(
            pane_id.to_string(),
            PollState {
                status: detected_status,
                content_hash,
                stable_count,
            },
        );

        if !status_changed {
            trace!(pane_id, ?detected_status, "status unchanged");
            continue;
        }

        // Status changed - update UI and persist
        debug!(
            pane_id,
            agent = profile.name(),
            ?detected_status,
            "detected status change via polling"
        );

        let icon = match detected_status {
            AgentStatus::Working => config.status_icons.working(),
            AgentStatus::Waiting => config.status_icons.waiting(),
            AgentStatus::Done => config.status_icons.done(),
        };
        let auto_clear = matches!(detected_status, AgentStatus::Waiting | AgentStatus::Done);

        if let Err(e) = mux.set_status(pane_id, icon, auto_clear) {
            warn!(pane_id, error = %e, "failed to set status icon");
        }

        crate::state::persist_agent_update(mux, pane_id, Some(detected_status), None);
    }

    // Clean up cache entries for panes that no longer exist
    status_cache.retain(|pane_id, _| live_panes.contains_key(pane_id));

    Ok(())
}

/// Strip ANSI escape sequences from a string for plain-text pattern matching.
/// Also processes carriage returns (\r) so that overwritten spinner lines
/// reflect what is actually visible on screen rather than all states concatenated.
fn strip_ansi(s: &str) -> String {
    // First strip ANSI escape sequences
    let mut stripped = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI sequence: ESC [ ... final-byte (0x40–0x7E)
            if chars.peek() == Some(&'[') {
                chars.next();
                for ch in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&ch) {
                        break;
                    }
                }
            } else {
                // Other escape sequence: skip next char
                chars.next();
            }
        } else {
            stripped.push(c);
        }
    }

    // Process carriage returns: \r without \n overwrites the current line,
    // so keep only the content after the last \r on each line.
    stripped
        .lines()
        .map(|line| {
            // Split on \r and take the last non-empty segment (the visible content)
            line.split('\r')
                .filter(|s| !s.is_empty())
                .last()
                .unwrap_or("")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi_removes_csi_sequences() {
        assert_eq!(strip_ansi("\x1b[32mgreen\x1b[0m"), "green");
    }

    #[test]
    fn test_strip_ansi_handles_carriage_return() {
        assert_eq!(strip_ansi("old text\rnew text"), "new text");
    }

    #[test]
    fn test_hash_content_deterministic() {
        assert_eq!(hash_content("hello"), hash_content("hello"));
        assert_ne!(hash_content("hello"), hash_content("world"));
    }
}
