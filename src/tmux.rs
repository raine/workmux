use anyhow::{Context, Result, anyhow};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, trace, warn};

use crate::cmd::Cmd;
use crate::config::{PaneConfig, SplitDirection};

/// Helper function to add prefix to window name
pub fn prefixed(prefix: &str, window_name: &str) -> String {
    format!("{}{}", prefix, window_name)
}

/// Get all tmux window names in a single call
pub fn get_all_window_names() -> Result<HashSet<String>> {
    // tmux list-windows may exit with error if no windows exist
    let windows = Cmd::new("tmux")
        .args(&["list-windows", "-F", "#{window_name}"])
        .run_and_capture_stdout()
        .unwrap_or_default(); // Return empty string if command fails

    Ok(windows.lines().map(String::from).collect())
}

/// Filter a list of window names, returning only those that still exist.
/// Used by the worker pool to track which windows are still active.
pub fn filter_active_windows(windows: &[String]) -> Result<Vec<String>> {
    let all_current = get_all_window_names()?;

    Ok(windows
        .iter()
        .filter(|w| all_current.contains(*w))
        .cloned()
        .collect())
}

/// Check if tmux server is running
pub fn is_running() -> Result<bool> {
    Cmd::new("tmux").arg("has-session").run_as_check()
}

/// Find the last window (by index) that starts with the given prefix.
/// Returns the window ID (e.g. @1) to be used as a target for inserting new windows.
/// Uses window IDs rather than names for stability.
pub fn find_last_window_with_prefix(prefix: &str) -> Result<Option<String>> {
    // tmux list-windows outputs in index order, so the last match is the highest index.
    let output = Cmd::new("tmux")
        .args(&["list-windows", "-F", "#{window_id} #{window_name}"])
        .run_and_capture_stdout()
        .unwrap_or_default();

    let mut last_match: Option<String> = None;

    for line in output.lines() {
        // Split on first space: "@id name..."
        if let Some((id, name)) = line.split_once(' ')
            && name.starts_with(prefix)
        {
            last_match = Some(id.to_string());
        }
    }

    Ok(last_match)
}

/// Find the last window (by index) that belongs to a specific base handle group.
/// This matches either the exact base name or numeric suffixes (e.g., `my-feature`, `my-feature-2`).
/// Used to insert duplicate windows immediately after their base window group.
///
/// Returns the window ID (e.g. @1) to be used as a target for inserting new windows.
pub fn find_last_window_with_base_handle(
    prefix: &str,
    base_handle: &str,
) -> Result<Option<String>> {
    let output = Cmd::new("tmux")
        .args(&["list-windows", "-F", "#{window_id} #{window_name}"])
        .run_and_capture_stdout()
        .unwrap_or_default();

    let full_base = prefixed(prefix, base_handle);
    let full_base_dash = format!("{}-", full_base);
    let mut last_match: Option<String> = None;

    for line in output.lines() {
        if let Some((id, name)) = line.split_once(' ') {
            let is_exact = name == full_base;
            let is_numeric_suffix = name.strip_prefix(&full_base_dash).is_some_and(|suffix| {
                !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
            });

            if is_exact || is_numeric_suffix {
                last_match = Some(id.to_string());
            }
        }
    }

    Ok(last_match)
}

/// Check if a tmux window with the given name exists
pub fn window_exists(prefix: &str, window_name: &str) -> Result<bool> {
    let prefixed_name = prefixed(prefix, window_name);
    window_exists_by_full_name(&prefixed_name)
}

/// Check if a window exists by its full name (including prefix)
pub fn window_exists_by_full_name(full_name: &str) -> Result<bool> {
    let windows = Cmd::new("tmux")
        .args(&["list-windows", "-F", "#{window_name}"])
        .run_and_capture_stdout();

    match windows {
        Ok(output) => Ok(output.lines().any(|line| line == full_name)),
        Err(_) => Ok(false), // If command fails, window doesn't exist
    }
}

/// Return the tmux window name for the current pane, if any
pub fn current_window_name() -> Result<Option<String>> {
    match Cmd::new("tmux")
        .args(&["display-message", "-p", "#{window_name}"])
        .run_and_capture_stdout()
    {
        Ok(name) => Ok(Some(name.trim().to_string())),
        Err(_) => Ok(None),
    }
}

/// Get the current foreground command for a pane
pub fn get_pane_current_command(pane_id: &str) -> Result<String> {
    let output = Cmd::new("tmux")
        .args(&[
            "display-message",
            "-p",
            "-t",
            pane_id,
            "#{pane_current_command}",
        ])
        .run_and_capture_stdout()
        .context("Failed to get pane current command")?;
    Ok(output.trim().to_string())
}

/// Get the working directory of the active pane in the current client's session.
/// This is useful when running inside a tmux popup, where `std::env::current_dir()`
/// returns the popup's directory rather than the underlying pane's directory.
pub fn get_client_active_pane_path() -> Result<PathBuf> {
    // Single shell command to get the active pane's path from the client's session
    let output = Cmd::new("sh")
        .args(&[
            "-c",
            "tmux display-message -p -t \"$(tmux display-message -p '#{client_session}')\" '#{pane_current_path}'",
        ])
        .run_and_capture_stdout()
        .context("Failed to get client active pane path")?;

    let path = output.trim();
    if path.is_empty() {
        return Err(anyhow!("Empty path returned from tmux"));
    }

    Ok(PathBuf::from(path))
}

/// Information about a specific pane running a workmux agent
#[derive(Debug, Clone)]
pub struct AgentPane {
    /// Tmux session name
    pub session: String,
    /// Window name (e.g., wm-feature-auth)
    pub window_name: String,
    /// Pane ID (e.g., %0)
    pub pane_id: String,
    /// Working directory path of the pane
    pub path: PathBuf,
    /// Pane title (set by Claude Code to show session summary)
    pub pane_title: Option<String>,
    /// Current status icon (if set)
    pub status: Option<String>,
    /// Unix timestamp when status was last set
    pub status_ts: Option<u64>,
}

/// Monitors agent panes to detect stalls and interrupts.
/// Tracks pane content hashes to identify when a "working" agent has stopped producing output.
pub struct AgentMonitor {
    /// Tracks pane content hashes for detecting stalled working agents
    content_hashes: HashMap<String, u64>,
    /// Set of pane IDs that have been detected as stalled (to avoid re-checking)
    stalled_panes: HashSet<String>,
}

impl AgentMonitor {
    pub fn new() -> Self {
        Self {
            content_hashes: HashMap::new(),
            stalled_panes: HashSet::new(),
        }
    }

    /// Check if a working agent pane is stalled (content unchanged).
    /// Returns true if the agent was detected as stalled.
    fn check_if_stalled(&mut self, pane_id: &str) -> bool {
        let Some(content) = capture_pane(pane_id, 50) else {
            return false;
        };

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        let current_hash = hasher.finish();

        let pane_key = pane_id.to_string();
        if let Some(&prev_hash) = self.content_hashes.get(&pane_key) {
            if prev_hash == current_hash {
                // Content unchanged - agent is stalled
                mark_agent_interrupted(pane_id);
                self.stalled_panes.insert(pane_key.clone());
                self.content_hashes.remove(&pane_key);
                return true;
            } else {
                // Content changed - agent is still working
                self.content_hashes.insert(pane_key, current_hash);
            }
        } else {
            // First time seeing this pane, start tracking
            self.content_hashes.insert(pane_key, current_hash);
        }
        false
    }

    /// Clean up tracking data for panes that are no longer active.
    fn cleanup_cache(&mut self, agents: &[AgentPane], working_icon: &str) {
        // Remove hashes for panes that are no longer working
        let working_pane_ids: HashSet<_> = agents
            .iter()
            .filter(|a| a.status.as_deref() == Some(working_icon))
            .map(|a| a.pane_id.as_str())
            .collect();
        self.content_hashes
            .retain(|k, _| working_pane_ids.contains(k.as_str()));

        // Remove stalled panes that are no longer in the agent list or no longer have working status
        let all_pane_ids: HashSet<_> = agents.iter().map(|a| a.pane_id.as_str()).collect();
        self.stalled_panes.retain(|pane_id| {
            all_pane_ids.contains(pane_id.as_str())
                && agents
                    .iter()
                    .find(|a| &a.pane_id == pane_id)
                    .map(|a| a.status.as_deref() == Some(working_icon))
                    .unwrap_or(false)
        });
    }

    /// Process agents to detect stalls. Modifies agent status in place for stalled agents.
    pub fn process_stalls(
        &mut self,
        mut agents: Vec<AgentPane>,
        working_icon: &str,
    ) -> Vec<AgentPane> {
        // Check each working agent for stalls
        for agent in &mut agents {
            if agent.status.as_deref() == Some(working_icon)
                && !self.stalled_panes.contains(&agent.pane_id)
                && self.check_if_stalled(&agent.pane_id)
            {
                // Clear status in the agent object to reflect the interrupt
                agent.status = None;
                // Timestamp was reset in mark_agent_interrupted, but we keep the old one
                // in the struct for this refresh cycle - it will be updated on next fetch
            }
        }

        // Clean up tracking data
        self.cleanup_cache(&agents, working_icon);

        agents
    }
}

impl Default for AgentMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a single line from tmux list-panes output into an AgentPane.
fn parse_agent_pane_line(line: &str) -> Option<AgentPane> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 9 {
        return None;
    }

    let status = if parts[5].is_empty() {
        None
    } else {
        Some(parts[5].to_string())
    };

    let pane_id = parts[2];
    let original_cmd = parts[7]; // @workmux_pane_command (stored when status set)
    let current_cmd = parts[8]; // pane_current_command (live)

    // Only include panes that are or were agents
    // Either: has a status set (active) OR has @workmux_pane_command set (was active)
    if status.is_none() && original_cmd.is_empty() {
        return None;
    }

    // If command changed, agent has exited - clear status and skip
    if !original_cmd.is_empty() && current_cmd != original_cmd {
        remove_agent_status(pane_id);
        return None;
    }

    let status_ts = if parts[6].is_empty() {
        None
    } else {
        parts[6].parse().ok()
    };

    let pane_title = if parts[4].is_empty() {
        None
    } else {
        Some(parts[4].to_string())
    };

    Some(AgentPane {
        session: parts[0].to_string(),
        window_name: parts[1].to_string(),
        pane_id: pane_id.to_string(),
        path: PathBuf::from(parts[3]),
        pane_title,
        status,
        status_ts,
    })
}

/// Fetch all panes across all sessions that have workmux pane status set.
/// This is used by the status dashboard to show all active agents.
///
/// Automatically removes panes from the list when the agent has exited.
/// This is detected by comparing the stored command (from when status was set)
/// with the current foreground command. If they differ, the agent has exited.
///
/// Also detects stalled "working" agents using the provided monitor.
pub fn get_all_agent_panes(
    working_icon: &str,
    monitor: &mut AgentMonitor,
) -> Result<Vec<AgentPane>> {
    // Format string to extract all needed info in one call
    // Using tab as delimiter since it's less likely to appear in paths/names
    let format = "#{session_name}\t#{window_name}\t#{pane_id}\t#{pane_current_path}\t#{pane_title}\t#{@workmux_pane_status}\t#{@workmux_pane_status_ts}\t#{@workmux_pane_command}\t#{pane_current_command}";

    let output = Cmd::new("tmux")
        .args(&["list-panes", "-a", "-F", format])
        .run_and_capture_stdout()
        .unwrap_or_default();

    // Parse all panes, filtering out exited agents
    let agents: Vec<AgentPane> = output.lines().filter_map(parse_agent_pane_line).collect();

    // Process stalls using the monitor
    let agents = monitor.process_stalls(agents, working_icon);

    Ok(agents)
}

/// Remove all workmux status tracking from a pane (when agent has exited).
/// Clears all pane-level status options including the command tracker.
/// Only clears pane-level options, not window-level, because:
/// 1. Multiple panes in a window may have different agents
/// 2. Window status uses "last write wins" - an active agent will re-set it
fn remove_agent_status(pane_id: &str) {
    let _ = Cmd::new("tmux")
        .args(&["set-option", "-up", "-t", pane_id, "@workmux_pane_status"])
        .run();
    let _ = Cmd::new("tmux")
        .args(&[
            "set-option",
            "-up",
            "-t",
            pane_id,
            "@workmux_pane_status_ts",
        ])
        .run();
    let _ = Cmd::new("tmux")
        .args(&["set-option", "-up", "-t", pane_id, "@workmux_pane_command"])
        .run();
}

/// Mark an agent as interrupted by clearing its status icon and resetting timestamps.
/// Used when an agent is detected as stalled (no pane content changes).
/// Keeps the pane visible in the dashboard and maintains exit detection capability.
/// Timestamps are reset to "now" so elapsed time starts from zero.
fn mark_agent_interrupted(pane_id: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let now_str = now.to_string();

    // Clear pane-level status icon
    let _ = Cmd::new("tmux")
        .args(&["set-option", "-up", "-t", pane_id, "@workmux_pane_status"])
        .run();

    // Reset pane-level timestamp to "now" (elapsed time starts from 0)
    let _ = Cmd::new("tmux")
        .args(&[
            "set-option",
            "-p",
            "-t",
            pane_id,
            "@workmux_pane_status_ts",
            &now_str,
        ])
        .run();

    // Clear window-level status icon
    let _ = Cmd::new("tmux")
        .args(&["set-option", "-uw", "-t", pane_id, "@workmux_status"])
        .run();

    // Reset window-level timestamp to "now"
    let _ = Cmd::new("tmux")
        .args(&[
            "set-option",
            "-w",
            "-t",
            pane_id,
            "@workmux_status_ts",
            &now_str,
        ])
        .run();

    // Note: @workmux_pane_command is intentionally NOT cleared
    // This keeps the pane visible in the dashboard after interrupt
}

/// Switch the tmux client to a specific pane
pub fn switch_to_pane(pane_id: &str) -> Result<()> {
    Cmd::new("tmux")
        .args(&["switch-client", "-t", pane_id])
        .run()
        .context("Failed to switch to pane")?;
    Ok(())
}

/// Try to switch to a pane, returning false if the pane doesn't exist
fn try_switch_to_pane(pane_id: &str) -> bool {
    Cmd::new("tmux")
        .args(&["switch-client", "-t", pane_id])
        .run()
        .is_ok()
}

// --- Done Panes Stack (for fast last-done cycling) ---

/// Tmux server variable storing the done panes stack (space-separated pane IDs, most recent last)
const DONE_STACK_VAR: &str = "@workmux_done_stack";

/// Get the list of done pane IDs from the tmux server variable.
/// Returns panes in order: most recent last.
fn get_done_stack() -> Vec<String> {
    let output = Cmd::new("tmux")
        .args(&["show-option", "-gqv", DONE_STACK_VAR])
        .run_and_capture_stdout()
        .unwrap_or_default();

    output.split_whitespace().map(String::from).collect()
}

/// Set the done panes stack in the tmux server variable.
fn set_done_stack(panes: &[String]) {
    let value = panes.join(" ");
    let _ = Cmd::new("tmux")
        .args(&["set-option", "-g", DONE_STACK_VAR, &value])
        .run();
}

/// Add a pane to the done stack. If already present, moves it to the end (most recent).
pub fn push_done_pane(pane_id: &str) {
    let mut stack = get_done_stack();
    stack.retain(|id| id != pane_id); // Remove if already present
    stack.push(pane_id.to_string()); // Add to end (most recent)
    set_done_stack(&stack);
}

/// Remove a pane from the done stack (when status changes away from done).
pub fn pop_done_pane(pane_id: &str) {
    let mut stack = get_done_stack();
    let original_len = stack.len();
    stack.retain(|id| id != pane_id);
    if stack.len() != original_len {
        set_done_stack(&stack);
    }
}

/// Switch to the agent pane that most recently completed its task, cycling through
/// completed agents on repeated invocations.
///
/// Uses a persistent stack stored in tmux server variable for fast lookups.
///
/// Behavior:
/// - If not currently on a done pane: switches to the most recently completed agent
/// - If already on a done pane: switches to the next oldest completed agent
/// - Wraps around to the most recent when reaching the oldest
/// - Automatically removes stale pane IDs that no longer exist
///
/// Returns `Ok(true)` if a pane was found and switched to, `Ok(false)` if no
/// completed agents were found.
pub fn switch_to_last_completed() -> Result<bool> {
    let mut stack = get_done_stack();

    if stack.is_empty() {
        return Ok(false);
    }

    // Get current pane to determine where we are in the cycle
    // Use tmux command instead of env var - TMUX_PANE is stale in run-shell contexts
    let current_pane = Cmd::new("tmux")
        .args(&["display-message", "-p", "#{pane_id}"])
        .run_and_capture_stdout()
        .ok()
        .map(|s| s.trim().to_string());

    // Stack is ordered oldest-first, most-recent-last
    // We want to cycle: most recent -> second most recent -> ... -> oldest -> wrap
    // So we iterate in reverse

    // Find current position (searching from the end)
    let current_idx = current_pane
        .as_ref()
        .and_then(|current| stack.iter().rposition(|id| id == current));

    // Try to find a valid pane to switch to, starting from the appropriate position
    let start_idx = match current_idx {
        Some(idx) if idx > 0 => idx - 1, // Next older (toward start of list)
        Some(_) => stack.len() - 1,      // At oldest, wrap to most recent (end)
        None => stack.len() - 1,         // Not on a done pane, start with most recent
    };

    // Try each pane in the stack, removing stale ones
    let mut removed_any = false;
    for i in 0..stack.len() {
        let idx = (start_idx + stack.len() - i) % stack.len();
        let pane_id = &stack[idx];

        // Try to switch - this atomically verifies existence and switches
        if try_switch_to_pane(pane_id) {
            // Clean up any stale panes we found
            if removed_any {
                stack.retain(|id| pane_exists(id));
                set_done_stack(&stack);
            }
            return Ok(true);
        } else {
            // Pane doesn't exist, mark for removal
            removed_any = true;
        }
    }

    // All panes were stale, clear the stack
    if removed_any {
        stack.retain(|id| pane_exists(id));
        set_done_stack(&stack);
    }

    Ok(false)
}

/// Check if a tmux pane exists
fn pane_exists(pane_id: &str) -> bool {
    Cmd::new("tmux")
        .args(&["display-message", "-t", pane_id, "-p", ""])
        .run()
        .is_ok()
}

/// Capture the last N lines of a pane's terminal output with ANSI colors.
/// Returns the captured text, or None if the pane doesn't exist.
pub fn capture_pane(pane_id: &str, lines: u16) -> Option<String> {
    // Capture from history to get scrollable content.
    // -e flag preserves ANSI escape sequences (colors)
    let start_line = format!("-{}", lines);
    let output = Cmd::new("tmux")
        .args(&[
            "capture-pane",
            "-p",        // Print to stdout
            "-e",        // Preserve ANSI escape sequences (colors)
            "-S",        // Start line
            &start_line, // N lines back in history
            "-t",
            pane_id, // Target pane
        ])
        .run_and_capture_stdout()
        .ok()?;

    Some(output)
}

/// Create a new tmux window with the given name and working directory.
/// Returns the pane ID of the initial pane in the window.
///
/// If `after_window` is provided (e.g., a window ID like "@1"), the new window
/// will be inserted immediately after that window using `tmux new-window -a`.
/// This keeps workmux windows grouped together.
pub fn create_window(
    prefix: &str,
    window_name: &str,
    working_dir: &Path,
    detached: bool,
    after_window: Option<&str>,
) -> Result<String> {
    let prefixed_name = prefixed(prefix, window_name);
    let working_dir_str = working_dir
        .to_str()
        .ok_or_else(|| anyhow!("Working directory path contains non-UTF8 characters"))?;

    let mut cmd = Cmd::new("tmux").arg("new-window");
    if detached {
        cmd = cmd.arg("-d");
    }

    // Insert after the target window if specified (keeps workmux windows grouped)
    if let Some(target) = after_window {
        cmd = cmd.arg("-a").args(&["-t", target]);
    }

    // Use -P to print pane info, -F to format output to just the pane ID
    let pane_id = cmd
        .args(&[
            "-n",
            &prefixed_name,
            "-c",
            working_dir_str,
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .run_and_capture_stdout()
        .context("Failed to create tmux window and get pane ID")?;

    Ok(pane_id.trim().to_string())
}

/// Select a specific pane by its ID
pub fn select_pane(pane_id: &str) -> Result<()> {
    Cmd::new("tmux")
        .args(&["select-pane", "-t", pane_id])
        .run()
        .context("Failed to select pane")?;

    Ok(())
}

/// Select a specific window
pub fn select_window(prefix: &str, window_name: &str) -> Result<()> {
    let prefixed_name = prefixed(prefix, window_name);
    let target = format!("={}", prefixed_name);

    Cmd::new("tmux")
        .args(&["select-window", "-t", &target])
        .run()
        .context("Failed to select window")?;

    Ok(())
}

/// Kill a tmux window by its full name (including prefix)
pub fn kill_window_by_full_name(full_name: &str) -> Result<()> {
    let target = format!("={}", full_name);

    Cmd::new("tmux")
        .args(&["kill-window", "-t", &target])
        .run()
        .context("Failed to kill tmux window")?;

    Ok(())
}

/// Execute a shell script via tmux run-shell
pub fn run_shell(script: &str) -> Result<()> {
    Cmd::new("tmux")
        .args(&["run-shell", script])
        .run()
        .context("Failed to run shell command via tmux")?;
    Ok(())
}

/// Schedule a tmux window to be killed after a short delay. This is useful when
/// the current command is running inside the window that needs to close.
pub fn schedule_window_close_by_full_name(full_name: &str, delay: Duration) -> Result<()> {
    let delay_secs = format!("{:.3}", delay.as_secs_f64());
    // Shell-escape the target with = inside quotes to handle spaces in window names
    let target = format!("={}", full_name);
    let escaped_target = format!("'{}'", target.replace('\'', r#"'\''"#));
    let script = format!(
        "sleep {delay}; tmux kill-window -t {target} >/dev/null 2>&1",
        delay = delay_secs,
        target = escaped_target
    );

    run_shell(&script)
}

/// Get the default shell configured in tmux
fn get_default_shell() -> Result<String> {
    let output = Cmd::new("tmux")
        .args(&["show-option", "-gqv", "default-shell"])
        .run_and_capture_stdout()?;
    let shell = output.trim();
    if shell.is_empty() {
        Ok("/bin/bash".to_string())
    } else {
        Ok(shell.to_string())
    }
}

/// Check if a shell is POSIX-compatible (supports `$(...)` syntax)
fn is_posix_shell(shell: &str) -> bool {
    let shell_name = Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("sh");
    matches!(shell_name, "bash" | "zsh" | "sh" | "dash" | "ksh" | "ash")
}

/// Timeout for waiting for pane readiness (seconds)
const HANDSHAKE_TIMEOUT_SECS: u64 = 5;

/// Manages the tmux wait-for handshake protocol for pane synchronization.
///
/// This struct encapsulates the channel-based handshake mechanism that ensures
/// the shell is ready before sending commands. The handshake uses tmux's `wait-for`
/// feature with channel locking to synchronize between the process spawning the
/// pane and the shell that starts inside it.
///
/// # Protocol
/// 1. Lock a unique channel (on construction)
/// 2. Start the shell with a wrapper that unlocks the channel when ready
/// 3. Wait for the shell to signal readiness (wait blocks until unlock)
/// 4. Clean up the channel
struct PaneHandshake {
    channel: String,
}

impl PaneHandshake {
    /// Create a new handshake and lock the channel.
    ///
    /// The channel must be locked before spawning the pane to ensure we don't
    /// miss the signal even if the shell starts instantly.
    fn new() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let channel = format!("wm_ready_{}_{}", pid, nanos);

        // Lock the channel (ensures we don't miss the signal)
        Cmd::new("tmux")
            .args(&["wait-for", "-L", &channel])
            .run()
            .context("Failed to initialize wait channel")?;

        Ok(Self { channel })
    }

    /// Build a shell wrapper command that signals readiness.
    ///
    /// The wrapper briefly disables echo while signaling the channel, restores it,
    /// then exec's into the shell so the TTY starts in a normal state.
    ///
    /// We wrap in `sh -c "..."` with double quotes to ensure the command works when
    /// tmux's default-shell is a non-POSIX shell like nushell. Single-quote escaping
    /// (`'\''`) doesn't work reliably when nushell parses the command before passing
    /// it to sh.
    fn wrapper_command(&self, shell: &str) -> String {
        // Two-step escaping for the shell path:
        // 1. Escape for inner single-quoted context (exec '...')
        let inner_shell = shell.replace('\'', "'\\''");
        // 2. Escape for outer double-quoted context (sh -c "...")
        let escaped_shell = inner_shell
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('`', "\\`");
        format!(
            "sh -c \"stty -echo 2>/dev/null; tmux wait-for -U {}; stty echo 2>/dev/null; exec '{}' -l\"",
            self.channel, escaped_shell
        )
    }

    /// Wait for the shell to signal it is ready, then clean up.
    ///
    /// This method consumes the handshake to ensure cleanup happens exactly once.
    /// Uses a polling loop with timeout to prevent indefinite hangs if the pane
    /// fails to start.
    fn wait(self) -> Result<()> {
        debug!(channel = %self.channel, "tmux:handshake start");

        let mut child = std::process::Command::new("tmux")
            .args(["wait-for", "-L", &self.channel])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn tmux wait-for command")?;

        let start = Instant::now();
        let timeout = Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        // Cleanup: unlock the channel we just re-locked
                        Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run()
                            .context("Failed to cleanup wait channel")?;
                        debug!(channel = %self.channel, "tmux:handshake success");
                        return Ok(());
                    } else {
                        // Attempt cleanup even on failure
                        let _ = Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run();
                        warn!(channel = %self.channel, status = ?status.code(), "tmux:handshake failed (wait-for error)");
                        return Err(anyhow!(
                            "Pane handshake failed - tmux wait-for returned error"
                        ));
                    }
                }
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait(); // Ensure process is reaped

                        // Attempt cleanup
                        let _ = Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run();

                        warn!(
                            channel = %self.channel,
                            timeout_secs = HANDSHAKE_TIMEOUT_SECS,
                            "tmux:handshake timeout"
                        );
                        return Err(anyhow!(
                            "Pane handshake timed out after {}s - shell may have failed to start",
                            HANDSHAKE_TIMEOUT_SECS
                        ));
                    }
                    trace!(
                        channel = %self.channel,
                        elapsed_ms = start.elapsed().as_millis(),
                        "tmux:handshake waiting"
                    );
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = Cmd::new("tmux")
                        .args(&["wait-for", "-U", &self.channel])
                        .run();
                    warn!(channel = %self.channel, error = %e, "tmux:handshake error");
                    return Err(anyhow!("Error waiting for pane handshake: {}", e));
                }
            }
        }
    }
}

/// Split a pane and return the new pane's ID
pub fn split_pane_with_command(
    target_pane_id: &str,
    direction: &SplitDirection,
    working_dir: &Path,
    size: Option<u16>,
    percentage: Option<u8>,
    shell_command: Option<&str>,
) -> Result<String> {
    let split_arg = match direction {
        SplitDirection::Horizontal => "-h",
        SplitDirection::Vertical => "-v",
    };

    let working_dir_str = working_dir
        .to_str()
        .ok_or_else(|| anyhow!("Working directory path contains non-UTF8 characters"))?;

    let mut cmd = Cmd::new("tmux").args(&[
        "split-window",
        split_arg,
        "-t",
        target_pane_id,
        "-c",
        working_dir_str,
        "-P", // Print new pane info
        "-F", // Format to get just the ID
        "#{pane_id}",
    ]);

    let size_arg;
    if let Some(p) = percentage {
        size_arg = format!("{}%", p);
        cmd = cmd.args(&["-l", &size_arg]);
    } else if let Some(s) = size {
        size_arg = s.to_string();
        cmd = cmd.args(&["-l", &size_arg]);
    }

    if let Some(shell_cmd) = shell_command {
        cmd = cmd.arg(shell_cmd);
    }

    let new_pane_id = cmd
        .run_and_capture_stdout()
        .context("Failed to split pane")?;

    Ok(new_pane_id.trim().to_string())
}

/// Respawn a pane by its ID
pub fn respawn_pane(pane_id: &str, working_dir: &Path, shell_command: Option<&str>) -> Result<()> {
    let working_dir_str = working_dir
        .to_str()
        .ok_or_else(|| anyhow!("Working directory path contains non-UTF8 characters"))?;

    let mut cmd =
        Cmd::new("tmux").args(&["respawn-pane", "-t", pane_id, "-c", working_dir_str, "-k"]);

    if let Some(shell_cmd) = shell_command {
        cmd = cmd.arg(shell_cmd);
    }

    cmd.run().context("Failed to respawn pane")?;

    Ok(())
}

/// Send keys to a pane using tmux send-keys
///
/// This is shell-agnostic - it works with any shell (bash, zsh, fish, nushell, etc.)
/// by typing the command as if the user had typed it, then pressing Enter.
pub fn send_keys(pane_id: &str, command: &str) -> Result<()> {
    // Use -l for literal keys (avoids interpretation of special characters)
    // Then send Enter separately to execute the command
    Cmd::new("tmux")
        .args(&["send-keys", "-t", pane_id, "-l", command])
        .run()
        .context("Failed to send keys to pane")?;

    Cmd::new("tmux")
        .args(&["send-keys", "-t", pane_id, "Enter"])
        .run()
        .context("Failed to send Enter key to pane")?;

    Ok(())
}

/// Check if the given agent command is Claude (needs special handling for ! prefix)
fn is_claude_agent(agent: Option<&str>) -> bool {
    let Some(agent) = agent else {
        return false;
    };

    let (token, _) = crate::config::split_first_token(agent).unwrap_or((agent, ""));
    let resolved =
        crate::config::resolve_executable_path(token).unwrap_or_else(|| token.to_string());
    let stem = Path::new(&resolved)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    stem == "claude"
}

/// Send keys to a pane, with special handling for Claude's ! prefix.
///
/// Claude Code requires a small delay after the `!` prefix for it to register
/// as a bash command. When sending commands starting with `!` to Claude,
/// this function sends the `!` separately, waits briefly, then sends the rest.
pub fn send_keys_to_agent(pane_id: &str, command: &str, agent: Option<&str>) -> Result<()> {
    if is_claude_agent(agent) && command.starts_with('!') {
        // Send ! first
        Cmd::new("tmux")
            .args(&["send-keys", "-t", pane_id, "-l", "!"])
            .run()
            .context("Failed to send ! to pane")?;

        // Small delay to let Claude register the !
        thread::sleep(Duration::from_millis(50));

        // Send the rest of the command
        Cmd::new("tmux")
            .args(&["send-keys", "-t", pane_id, "-l", &command[1..]])
            .run()
            .context("Failed to send keys to pane")?;

        // Send Enter
        Cmd::new("tmux")
            .args(&["send-keys", "-t", pane_id, "Enter"])
            .run()
            .context("Failed to send Enter key to pane")?;

        Ok(())
    } else {
        send_keys(pane_id, command)
    }
}

/// Send a single key to a pane without pressing Enter.
/// Used for interactive input mode where each keystroke is forwarded.
pub fn send_key(pane_id: &str, key: &str) -> Result<()> {
    Cmd::new("tmux")
        .args(&["send-keys", "-t", pane_id, key])
        .run()
        .context("Failed to send key to pane")?;
    Ok(())
}

/// Paste multiline content into a pane using tmux buffer and bracketed paste.
/// This ensures newlines are treated as content, not as Enter keypresses.
/// After pasting, sends Enter to submit the content.
pub fn paste_multiline(pane_id: &str, content: &str) -> Result<()> {
    use std::io::Write;

    // Load content into a temporary tmux buffer via stdin
    let mut child = std::process::Command::new("tmux")
        .args(["load-buffer", "-"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn tmux load-buffer")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(content.as_bytes())
            .context("Failed to write to tmux buffer")?;
    }

    let status = child
        .wait()
        .context("Failed to wait for tmux load-buffer")?;
    if !status.success() {
        return Err(anyhow::anyhow!("tmux load-buffer failed"));
    }

    // Paste the buffer with bracketed paste (-p) and delete after (-d)
    Cmd::new("tmux")
        .args(&["paste-buffer", "-t", pane_id, "-p", "-d"])
        .run()
        .context("Failed to paste buffer to pane")?;

    // Send Enter to submit the pasted content
    Cmd::new("tmux")
        .args(&["send-keys", "-t", pane_id, "Enter"])
        .run()
        .context("Failed to send Enter after paste")?;

    Ok(())
}

/// Result of setting up panes
pub struct PaneSetupResult {
    /// The ID of the pane that should receive focus.
    pub focus_pane_id: String,
}

pub struct PaneSetupOptions<'a> {
    pub run_commands: bool,
    pub prompt_file_path: Option<&'a Path>,
}

/// Setup panes in a window according to configuration
pub fn setup_panes(
    initial_pane_id: &str,
    panes: &[PaneConfig],
    working_dir: &Path,
    pane_options: PaneSetupOptions<'_>,
    config: &crate::config::Config,
    task_agent: Option<&str>,
) -> Result<PaneSetupResult> {
    if panes.is_empty() {
        return Ok(PaneSetupResult {
            focus_pane_id: initial_pane_id.to_string(),
        });
    }

    let mut focus_pane_id: Option<String> = None;
    let mut pane_ids: Vec<String> = vec![initial_pane_id.to_string()];
    let effective_agent = task_agent.or(config.agent.as_deref());
    let shell = get_default_shell()?;

    // Handle the first pane (initial pane from window creation)
    if let Some(pane_config) = panes.first() {
        let command_to_run = if pane_config.command.as_deref() == Some("<agent>") {
            effective_agent.map(|agent_cmd| agent_cmd.to_string())
        } else {
            pane_config.command.clone()
        };

        let adjusted_command = if pane_options.run_commands {
            command_to_run.as_ref().map(|cmd| {
                adjust_command(
                    cmd,
                    pane_options.prompt_file_path,
                    working_dir,
                    effective_agent,
                    &shell,
                )
            })
        } else {
            None
        };

        if let Some(cmd_str) = adjusted_command.as_ref().map(|c| c.as_ref()) {
            // Use PaneHandshake to ensure shell is ready before sending keys
            let handshake = PaneHandshake::new()?;
            let wrapper = handshake.wrapper_command(&shell);

            respawn_pane(initial_pane_id, working_dir, Some(&wrapper))?;
            handshake.wait()?;
            send_keys(initial_pane_id, cmd_str)?;

            // Set "working" status if prompt was injected into a hook-supporting agent.
            // See: agent_needs_auto_status()
            if let Some(Cow::Owned(_)) = &adjusted_command
                && agent_needs_auto_status(effective_agent)
            {
                let _ = set_pane_working_status(initial_pane_id, config);
            }
        }
        if pane_config.focus {
            focus_pane_id = Some(initial_pane_id.to_string());
        }
    }

    // Create additional panes by splitting
    for pane_config in panes.iter().skip(1) {
        if let Some(ref direction) = pane_config.split {
            // Determine which pane to split based on logical index, then get its ID
            let target_pane_idx = pane_config.target.unwrap_or(pane_ids.len() - 1);
            let target_pane_id = pane_ids
                .get(target_pane_idx)
                .ok_or_else(|| anyhow!("Invalid target pane index: {}", target_pane_idx))?;

            let command_to_run = if pane_config.command.as_deref() == Some("<agent>") {
                effective_agent.map(|agent_cmd| agent_cmd.to_string())
            } else {
                pane_config.command.clone()
            };

            let adjusted_command = if pane_options.run_commands {
                command_to_run.as_ref().map(|cmd| {
                    adjust_command(
                        cmd,
                        pane_options.prompt_file_path,
                        working_dir,
                        effective_agent,
                        &shell,
                    )
                })
            } else {
                None
            };

            let new_pane_id = if let Some(cmd_str) = adjusted_command.as_ref().map(|c| c.as_ref()) {
                // Use PaneHandshake to ensure shell is ready before sending keys
                let handshake = PaneHandshake::new()?;
                let wrapper = handshake.wrapper_command(&shell);

                let pane_id = split_pane_with_command(
                    target_pane_id,
                    direction,
                    working_dir,
                    pane_config.size,
                    pane_config.percentage,
                    Some(&wrapper),
                )?;

                handshake.wait()?;
                send_keys(&pane_id, cmd_str)?;

                // Set "working" status if prompt was injected into a hook-supporting agent.
                // See: agent_needs_auto_status()
                if let Some(Cow::Owned(_)) = &adjusted_command
                    && agent_needs_auto_status(effective_agent)
                {
                    let _ = set_pane_working_status(&pane_id, config);
                }

                pane_id
            } else {
                split_pane_with_command(
                    target_pane_id,
                    direction,
                    working_dir,
                    pane_config.size,
                    pane_config.percentage,
                    None,
                )?
            };

            if pane_config.focus {
                focus_pane_id = Some(new_pane_id.clone());
            }
            pane_ids.push(new_pane_id);
        }
    }

    Ok(PaneSetupResult {
        // Default to the first pane if no focus is specified
        focus_pane_id: focus_pane_id.unwrap_or_else(|| initial_pane_id.to_string()),
    })
}

fn adjust_command<'a>(
    command: &'a str,
    prompt_file_path: Option<&Path>,
    working_dir: &Path,
    effective_agent: Option<&str>,
    shell: &str,
) -> Cow<'a, str> {
    if let Some(prompt_path) = prompt_file_path
        && let Some(rewritten) =
            rewrite_agent_command(command, prompt_path, working_dir, effective_agent, shell)
    {
        return Cow::Owned(rewritten);
    }
    Cow::Borrowed(command)
}

/// Rewrites an agent command to inject a prompt file's contents.
///
/// When a prompt file is provided (via --prompt-file or --prompt-editor), this function
/// modifies the agent command to automatically pass the prompt content. For example,
/// "claude" becomes "claude -- \"$(cat PROMPT.md)\"" for POSIX shells, or wrapped in
/// `sh -c '...'` for non-POSIX shells like nushell.
///
/// Only rewrites commands that match the configured agent. For instance, if the config
/// specifies "gemini" as the agent, a "claude" command won't be rewritten.
///
/// Special handling:
/// - gemini: Adds `-i` flag for interactive mode after the prompt
/// - Other agents (claude, codex, etc.): Just passes the prompt as first argument
///
/// For non-POSIX shells (nushell, fish, pwsh), the command is wrapped in `sh -c '...'`
/// to ensure the `$(cat ...)` command substitution works correctly.
///
/// The returned command is prefixed with a space to prevent it from being saved to
/// shell history (most shells ignore commands starting with a space).
///
/// Returns None if the command shouldn't be rewritten (empty, doesn't match configured agent, etc.)
fn rewrite_agent_command(
    command: &str,
    prompt_file: &Path,
    working_dir: &Path,
    effective_agent: Option<&str>,
    shell: &str,
) -> Option<String> {
    let agent_command = effective_agent?;
    let trimmed_command = command.trim();
    if trimmed_command.is_empty() {
        return None;
    }

    let (pane_token, pane_rest) = crate::config::split_first_token(trimmed_command)?;
    let (config_token, _) = crate::config::split_first_token(agent_command)?;

    let resolved_pane_path = crate::config::resolve_executable_path(pane_token)
        .unwrap_or_else(|| pane_token.to_string());
    let resolved_config_path = crate::config::resolve_executable_path(config_token)
        .unwrap_or_else(|| config_token.to_string());

    let pane_stem = Path::new(&resolved_pane_path).file_stem();
    let config_stem = Path::new(&resolved_config_path).file_stem();

    if pane_stem != config_stem {
        return None;
    }

    let relative = prompt_file.strip_prefix(working_dir).unwrap_or(prompt_file);
    let prompt_path = relative.to_string_lossy();
    let rest = pane_rest.trim_start();

    // Build the inner command step-by-step to ensure correct order:
    // [agent_command] [agent_options] [user_args] [prompt_argument]
    let mut inner_cmd = pane_token.to_string();

    // Add user-provided arguments from config (must come before the prompt)
    if !rest.is_empty() {
        inner_cmd.push(' ');
        inner_cmd.push_str(rest);
    }

    // Add the prompt argument (agent-specific handling)
    let pane_stem_str = pane_stem.and_then(|s| s.to_str());
    if pane_stem_str == Some("gemini") {
        // gemini uses -i flag with the prompt as its argument
        inner_cmd.push_str(&format!(" -i \"$(cat {})\"", prompt_path));
    } else if pane_stem_str == Some("opencode") {
        // opencode uses --prompt flag for interactive TUI with initial prompt
        inner_cmd.push_str(&format!(" --prompt \"$(cat {})\"", prompt_path));
    } else {
        // Other agents use -- separator
        inner_cmd.push_str(&format!(" -- \"$(cat {})\"", prompt_path));
    }

    // For POSIX shells (bash, zsh, sh, etc.), use the command directly.
    // For non-POSIX shells (nushell, fish, pwsh), wrap in sh -c '...' to ensure
    // $(cat ...) command substitution works.
    // Prefix with space to prevent shell history entry.
    if is_posix_shell(shell) {
        Some(format!(" {}", inner_cmd))
    } else {
        let escaped_inner = inner_cmd.replace('\'', "'\\''");
        Some(format!(" sh -c '{}'", escaped_inner))
    }
}

// --- Status Management ---

/// Checks if an agent supports hooks and needs auto-status when launched with a prompt.
/// Currently only Claude and opencode support hooks that would normally set the status.
///
/// This is a workaround for Claude Code's broken UserPromptSubmit hook:
/// https://github.com/anthropics/claude-code/issues/17284
fn agent_needs_auto_status(effective_agent: Option<&str>) -> bool {
    let Some(agent) = effective_agent else {
        return false;
    };

    let (token, _) = crate::config::split_first_token(agent).unwrap_or((agent, ""));
    let resolved =
        crate::config::resolve_executable_path(token).unwrap_or_else(|| token.to_string());
    let stem = Path::new(&resolved)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    matches!(stem, "claude" | "opencode")
}

/// Sets the "working" status on a pane. Used when launching an agent with a prompt
/// to work around Claude Code's broken UserPromptSubmit hook.
///
/// Note: This intentionally does NOT enable exit detection. When called right after
/// `send_keys()`, the shell hasn't started the agent yet, so capturing the command
/// would get `zsh`/`bash` instead of `node`/`claude`.
fn set_pane_working_status(pane_id: &str, config: &crate::config::Config) -> Result<()> {
    let icon = config.status_icons.working();

    // Ensure the status format is applied so the icon shows up
    if config.status_format.unwrap_or(true) {
        let _ = ensure_status_format(pane_id);
    }

    set_status_options(pane_id, icon, false);
    Ok(())
}

/// Sets status options on a pane (both window-level and pane-level).
///
/// This is the shared implementation used by both `workmux set-window-status` and
/// the auto-status workaround when launching agents with prompts.
///
/// # Arguments
/// * `pane` - The tmux pane ID to set status on
/// * `icon` - The status icon to display
/// * `enable_exit_detection` - If true, captures current command for exit detection.
///   Set to false when the agent hasn't started yet (e.g., right after send_keys).
pub fn set_status_options(pane: &str, icon: &str, enable_exit_detection: bool) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let now_str = now.to_string();

    // 1. Set Window Option (for tmux status bar display)
    // "Last write wins" behavior for the window icon
    if let Err(e) = Cmd::new("tmux")
        .args(&["set-option", "-w", "-t", pane, "@workmux_status", icon])
        .run()
    {
        eprintln!("workmux: failed to set window status: {}", e);
    }
    let _ = Cmd::new("tmux")
        .args(&[
            "set-option",
            "-w",
            "-t",
            pane,
            "@workmux_status_ts",
            &now_str,
        ])
        .run();

    // 2. Set Pane Option (for dashboard tracking)
    // Use a DISTINCT key to avoid inheritance issues in list-panes
    if let Err(e) = Cmd::new("tmux")
        .args(&["set-option", "-p", "-t", pane, "@workmux_pane_status", icon])
        .run()
    {
        eprintln!("workmux: failed to set pane status: {}", e);
    }
    let _ = Cmd::new("tmux")
        .args(&[
            "set-option",
            "-p",
            "-t",
            pane,
            "@workmux_pane_status_ts",
            &now_str,
        ])
        .run();

    // 3. Store the current foreground command for agent exit detection
    // When the command changes (e.g., from "node" to "zsh"), we know the agent exited
    if enable_exit_detection {
        let current_cmd = get_pane_current_command(pane).unwrap_or_default();
        if !current_cmd.is_empty() {
            let _ = Cmd::new("tmux")
                .args(&[
                    "set-option",
                    "-p",
                    "-t",
                    pane,
                    "@workmux_pane_command",
                    &current_cmd,
                ])
                .run();
        }
    }
}

// --- Status Format Management ---

/// Format string to inject into tmux window-status-format.
/// Uses conditional: only shows space + icon when @workmux_status is set.
const WORKMUX_STATUS_FORMAT: &str = "#{?@workmux_status, #{@workmux_status},}";

/// Ensures the tmux window's status format includes workmux status.
/// Sets format per-window to avoid affecting non-workmux windows or other sessions.
/// Uses pane target to set on the correct window (not the focused one).
pub fn ensure_status_format(pane: &str) -> Result<()> {
    update_format_option(pane, "window-status-format")?;
    update_format_option(pane, "window-status-current-format")?;
    Ok(())
}

/// Updates a single tmux format option for the target window to include workmux status.
fn update_format_option(pane: &str, option: &str) -> Result<()> {
    // Read current format. Try window-level first, fall back to global.
    // Note: show-option -wv returns empty string (not error) when no window option exists.
    //
    // Uses run() instead of run_and_capture_stdout() because the latter calls .trim()
    // which strips meaningful whitespace from format strings (e.g., padding spaces in
    // tmux themes). We only strip trailing newlines from command output.
    let window_format = Cmd::new("tmux")
        .args(&["show-option", "-wv", "-t", pane, option])
        .run()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim_end_matches('\n').to_string())
        .filter(|s| !s.is_empty());

    let current = match window_format {
        Some(fmt) => fmt,
        None => Cmd::new("tmux")
            .args(&["show-option", "-gv", option])
            .run()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|s| s.trim_end_matches('\n').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "#I:#W#{?window_flags,#{window_flags}, }".to_string()),
    };

    if !current.contains("@workmux_status") {
        let new_format = inject_status_format(&current);
        // Set per-window to avoid affecting other windows/sessions
        Cmd::new("tmux")
            .args(&["set-option", "-w", "-t", pane, option, &new_format])
            .run()?;
    }
    Ok(())
}

/// Block execution until all specified windows (by full name including prefix) are closed.
pub fn wait_until_windows_closed(full_window_names: &[String]) -> Result<()> {
    if full_window_names.is_empty() {
        return Ok(());
    }

    let targets: HashSet<String> = full_window_names.iter().cloned().collect();

    if targets.len() == 1 {
        println!("Waiting for window '{}' to close...", full_window_names[0]);
    } else {
        println!("Waiting for {} windows to close...", targets.len());
    }

    loop {
        // If tmux server isn't running, windows are definitely gone
        if !is_running()? {
            return Ok(());
        }

        // Get current windows once per iteration
        let current_windows = get_all_window_names()?;

        // Check if any of our targets still exist
        // We continue waiting as long as ANY target exists
        let any_exists = targets
            .iter()
            .any(|target| current_windows.contains(target));

        if !any_exists {
            return Ok(());
        }

        // Standard polling interval
        thread::sleep(Duration::from_millis(500));
    }
}

/// Injects workmux status format into an existing format string.
/// Inserts before window_flags if present, otherwise appends to end.
fn inject_status_format(format: &str) -> String {
    // Match common window_flags patterns:
    // - #{window_flags} or #{window_flags,...}
    // - #{?window_flags,...} (conditional)
    // - #{F} (short alias for window_flags)
    let patterns = ["#{window_flags", "#{?window_flags", "#{F}"];

    let insert_pos = patterns.iter().filter_map(|p| format.find(p)).min(); // Find earliest occurrence

    if let Some(pos) = insert_pos {
        // Insert before window_flags
        let (before, after) = format.split_at(pos);
        format!("{}{}{}", before, WORKMUX_STATUS_FORMAT, after)
    } else {
        // Append to end
        format!("{}{}", format, WORKMUX_STATUS_FORMAT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // --- is_posix_shell tests ---

    #[test]
    fn test_is_posix_shell_bash() {
        assert!(is_posix_shell("/bin/bash"));
        assert!(is_posix_shell("/usr/bin/bash"));
    }

    #[test]
    fn test_is_posix_shell_zsh() {
        assert!(is_posix_shell("/bin/zsh"));
        assert!(is_posix_shell("/usr/local/bin/zsh"));
    }

    #[test]
    fn test_is_posix_shell_sh() {
        assert!(is_posix_shell("/bin/sh"));
    }

    #[test]
    fn test_is_posix_shell_nushell() {
        assert!(!is_posix_shell("/opt/homebrew/bin/nu"));
        assert!(!is_posix_shell("/usr/bin/nu"));
    }

    #[test]
    fn test_is_posix_shell_fish() {
        assert!(!is_posix_shell("/usr/bin/fish"));
        assert!(!is_posix_shell("/opt/homebrew/bin/fish"));
    }

    // --- rewrite_agent_command tests for POSIX shells ---

    #[test]
    fn test_rewrite_claude_command_posix() {
        let prompt_file = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");

        let result = rewrite_agent_command(
            "claude",
            &prompt_file,
            &working_dir,
            Some("claude"),
            "/bin/zsh",
        );
        // POSIX shell: no wrapper, prefixed with space to prevent history
        assert_eq!(result, Some(" claude -- \"$(cat PROMPT.md)\"".to_string()));
    }

    #[test]
    fn test_rewrite_gemini_command_posix() {
        let prompt_file = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");

        let result = rewrite_agent_command(
            "gemini",
            &prompt_file,
            &working_dir,
            Some("gemini"),
            "/bin/bash",
        );
        assert_eq!(result, Some(" gemini -i \"$(cat PROMPT.md)\"".to_string()));
    }

    #[test]
    fn test_rewrite_opencode_command_posix() {
        let prompt_file = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");

        let result = rewrite_agent_command(
            "opencode",
            &prompt_file,
            &working_dir,
            Some("opencode"),
            "/bin/zsh",
        );
        assert_eq!(
            result,
            Some(" opencode --prompt \"$(cat PROMPT.md)\"".to_string())
        );
    }

    #[test]
    fn test_rewrite_command_with_args_posix() {
        let prompt_file = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");

        let result = rewrite_agent_command(
            "claude --verbose",
            &prompt_file,
            &working_dir,
            Some("claude"),
            "/bin/bash",
        );
        assert_eq!(
            result,
            Some(" claude --verbose -- \"$(cat PROMPT.md)\"".to_string())
        );
    }

    // --- rewrite_agent_command tests for non-POSIX shells (nushell, fish) ---

    #[test]
    fn test_rewrite_claude_command_nushell() {
        let prompt_file = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");

        let result = rewrite_agent_command(
            "claude",
            &prompt_file,
            &working_dir,
            Some("claude"),
            "/opt/homebrew/bin/nu",
        );
        // Non-POSIX shell: wrap in sh -c, prefixed with space
        assert_eq!(
            result,
            Some(" sh -c 'claude -- \"$(cat PROMPT.md)\"'".to_string())
        );
    }

    #[test]
    fn test_rewrite_gemini_command_fish() {
        let prompt_file = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");

        let result = rewrite_agent_command(
            "gemini",
            &prompt_file,
            &working_dir,
            Some("gemini"),
            "/usr/bin/fish",
        );
        assert_eq!(
            result,
            Some(" sh -c 'gemini -i \"$(cat PROMPT.md)\"'".to_string())
        );
    }

    #[test]
    fn test_rewrite_command_escapes_single_quotes_nushell() {
        // Test that single quotes in agent paths are properly escaped for non-POSIX shells
        let prompt_file = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");

        let result = rewrite_agent_command(
            "/path/with'quote/claude",
            &prompt_file,
            &working_dir,
            Some("/path/with'quote/claude"),
            "/opt/homebrew/bin/nu",
        );
        assert_eq!(
            result,
            Some(" sh -c '/path/with'\\''quote/claude -- \"$(cat PROMPT.md)\"'".to_string())
        );
    }

    // --- Other rewrite_agent_command tests ---

    #[test]
    fn test_rewrite_mismatched_agent() {
        let prompt_file = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");

        // Command is for claude but agent is gemini
        let result = rewrite_agent_command(
            "claude",
            &prompt_file,
            &working_dir,
            Some("gemini"),
            "/bin/zsh",
        );
        assert_eq!(result, None);
    }

    #[test]
    fn test_rewrite_empty_command() {
        let prompt_file = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");

        let result =
            rewrite_agent_command("", &prompt_file, &working_dir, Some("claude"), "/bin/zsh");
        assert_eq!(result, None);
    }

    #[test]
    fn test_rewrite_command_with_path_posix() {
        let prompt_file = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");

        let result = rewrite_agent_command(
            "/usr/local/bin/claude",
            &prompt_file,
            &working_dir,
            Some("/usr/local/bin/claude"),
            "/bin/zsh",
        );
        assert_eq!(
            result,
            Some(" /usr/local/bin/claude -- \"$(cat PROMPT.md)\"".to_string())
        );
    }

    #[test]
    fn test_rewrite_unknown_agent_posix() {
        let prompt_file = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");

        let result = rewrite_agent_command(
            "unknown-agent",
            &prompt_file,
            &working_dir,
            Some("unknown-agent"),
            "/bin/bash",
        );
        assert_eq!(
            result,
            Some(" unknown-agent -- \"$(cat PROMPT.md)\"".to_string())
        );
    }

    // --- inject_status_format tests ---

    #[test]
    fn test_inject_status_format_standard() {
        // Standard default format with conditional window_flags
        let input = "#I:#W#{?window_flags,#{window_flags}, }";
        let result = inject_status_format(input);
        assert_eq!(
            result,
            "#I:#W#{?@workmux_status, #{@workmux_status},}#{?window_flags,#{window_flags}, }"
        );
    }

    #[test]
    fn test_inject_status_format_short_flags() {
        // Short format with #{F}
        let input = "#I:#W#{F}";
        let result = inject_status_format(input);
        assert_eq!(result, "#I:#W#{?@workmux_status, #{@workmux_status},}#{F}");
    }

    #[test]
    fn test_inject_status_format_no_flags() {
        // Format without window_flags - append to end
        let input = "#I:#W";
        let result = inject_status_format(input);
        assert_eq!(result, "#I:#W#{?@workmux_status, #{@workmux_status},}");
    }

    #[test]
    fn test_inject_status_format_complex() {
        // Complex format with styling
        let input = "#[fg=blue]#I#[default] #{?window_flags,#{window_flags},}";
        let result = inject_status_format(input);
        assert_eq!(
            result,
            "#[fg=blue]#I#[default] #{?@workmux_status, #{@workmux_status},}#{?window_flags,#{window_flags},}"
        );
    }

    #[test]
    fn test_inject_status_format_bare_window_flags() {
        // Bare #{window_flags} without conditional
        let input = "#I:#W#{window_flags}";
        let result = inject_status_format(input);
        assert_eq!(
            result,
            "#I:#W#{?@workmux_status, #{@workmux_status},}#{window_flags}"
        );
    }

    #[test]
    fn test_inject_status_format_preserves_whitespace() {
        // Leading and trailing spaces from tmux themes must be preserved
        let input = " #I:#W#{window_flags} ";
        let result = inject_status_format(input);
        assert_eq!(
            result,
            " #I:#W#{?@workmux_status, #{@workmux_status},}#{window_flags} "
        );
    }

    #[test]
    fn test_trim_end_newlines_preserves_spaces() {
        // Simulates processing tmux show-option output: trailing newlines are
        // stripped but meaningful whitespace (padding spaces) is kept intact.
        let raw_output = " #I:#W#{window_flags} \n";
        let processed = raw_output.trim_end_matches('\n').to_string();
        assert_eq!(processed, " #I:#W#{window_flags} ");

        let result = inject_status_format(&processed);
        assert_eq!(
            result,
            " #I:#W#{?@workmux_status, #{@workmux_status},}#{window_flags} "
        );
    }
}
