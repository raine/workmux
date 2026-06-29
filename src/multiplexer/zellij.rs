//! Zellij multiplexer backend.
//!
//! Limitations:
//! - No percentage-based pane size control (can resize with +/- but not set exact %)
//! - No window insertion order (tabs always append)
//! - No visual status indicator (set_status is a no-op)

use anyhow::{Context, Result, anyhow};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, warn};

use crate::cmd::Cmd;
use crate::config::SplitDirection;

use super::types::{CreateWindowParams, LivePaneInfo};
use super::{Multiplexer, util};

/// Zellij multiplexer backend.
pub struct ZellijBackend {
    _private: (),
}

/// Info about a pane from `zellij action list-panes --json --tab --command`
#[derive(Debug, serde::Deserialize)]
struct PaneInfo {
    id: u32,
    is_plugin: bool,
    is_focused: bool,
    terminal_command: Option<String>,
    /// Running command (more reliable than terminal_command, available with --command flag)
    #[serde(default)]
    pane_command: Option<String>,
    /// Pane's current working directory (available with --command flag)
    #[serde(default)]
    pane_cwd: Option<String>,
    /// Stable tab ID (available with --tab flag)
    #[serde(default)]
    tab_id: Option<u32>,
    #[serde(default)]
    tab_name: String,
    #[serde(default)]
    title: String,
}

/// Info about a tab from `zellij action list-tabs --json`
#[derive(Debug, serde::Deserialize)]
struct TabInfo {
    tab_id: u32, // Stable tab ID (available in zellij 0.44.0+)
    #[allow(dead_code)]
    position: u32, // Tab position (can change when tabs are reordered)
    name: String,
    #[allow(dead_code)]
    active: bool,
}

impl TabInfo {
    /// Get stable tab ID
    fn tab_id(&self) -> u32 {
        self.tab_id
    }
}

/// Parse a numeric pane ID from a "terminal_X" string.
fn parse_pane_id(pane_id: &str) -> Option<u32> {
    pane_id
        .strip_prefix("terminal_")
        .and_then(|s| s.parse().ok())
}

/// Extract the base command name from a full command path/string.
///
/// Takes an optional command string (e.g., "/usr/bin/bash --login"),
/// extracts the first word, then returns only the basename.
fn extract_base_command(pane_command: Option<&str>, terminal_command: Option<&str>) -> String {
    pane_command
        .or(terminal_command)
        .and_then(|cmd| cmd.split_whitespace().next())
        .unwrap_or("")
        .split('/')
        .next_back()
        .unwrap_or("")
        .to_string()
}

/// Parse the focused tab name from `zellij action current-tab-info` output.
///
/// Output format: "name: Tab #1\nid: 0\nposition: 0\n..."
fn parse_tab_name_from_output(output: &str) -> Option<String> {
    output
        .lines()
        .find(|l| l.starts_with("name: "))
        .map(|l| l["name: ".len()..].to_string())
}

fn zellij_new_pane_direction_args(direction: &SplitDirection) -> &'static [&'static str] {
    match direction {
        SplitDirection::Horizontal => &["--direction", "right"],
        SplitDirection::Vertical => &["--direction", "down"],
        SplitDirection::Stacked => &["--stacked"],
    }
}

impl Default for ZellijBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ZellijBackend {
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Check if inside a zellij session
    fn is_inside_session() -> bool {
        std::env::var("ZELLIJ").is_ok()
    }

    /// Get session name from environment
    fn session_name() -> Option<String> {
        std::env::var("ZELLIJ_SESSION_NAME").ok()
    }

    /// Get current pane ID from environment (format: terminal_1, plugin_2, etc.)
    fn pane_id_from_env() -> Option<String> {
        std::env::var("ZELLIJ_PANE_ID")
            .ok()
            .map(|id| format!("terminal_{}", id))
    }

    /// Get the name of the currently focused tab using `current-tab-info`.
    fn focused_tab_name() -> Option<String> {
        let output = Cmd::new("zellij")
            .args(&["action", "current-tab-info"])
            .run_and_capture_stdout()
            .ok()?;

        parse_tab_name_from_output(&output)
    }

    /// Query all panes using `zellij action list-panes --json --tab --command`
    ///
    /// The `--tab` flag includes `tab_id`, `tab_name`, `tab_position`.
    /// The `--command` flag includes `pane_command`, `pane_cwd`.
    fn list_panes() -> Result<Vec<PaneInfo>> {
        let output = Cmd::new("zellij")
            .args(&["action", "list-panes", "--json", "--tab", "--command"])
            .run_and_capture_stdout()
            .context("Failed to list panes")?;

        serde_json::from_str(&output).context("Failed to parse list-panes JSON output")
    }

    /// Query all tabs using `zellij action list-tabs --json`
    fn list_tabs() -> Result<Vec<TabInfo>> {
        let output = Cmd::new("zellij")
            .args(&["action", "list-tabs", "--json"])
            .run_and_capture_stdout()
            .context("Failed to list tabs")?;

        serde_json::from_str(&output).context("Failed to parse list-tabs JSON output")
    }

    /// Get focused pane ID from list-panes output
    ///
    /// Returns the focused pane in the currently active tab.
    fn focused_pane_id() -> Result<u32> {
        let panes = Self::list_panes()?;
        let focused_tab = Self::focused_tab_name();

        // Filter by focused tab if we know which tab is focused
        if let Some(tab_name) = focused_tab {
            panes
                .iter()
                .find(|p| p.is_focused && !p.is_plugin && p.tab_name == tab_name)
                .map(|p| p.id)
                .ok_or_else(|| anyhow!("No focused terminal pane found in tab '{}'", tab_name))
        } else {
            // Fallback: just find any focused terminal pane
            panes
                .iter()
                .find(|p| p.is_focused && !p.is_plugin)
                .map(|p| p.id)
                .ok_or_else(|| anyhow!("No focused terminal pane found"))
        }
    }

    /// Get tab ID by tab name (for future use)
    #[allow(dead_code)]
    fn get_tab_id_by_name(name: &str) -> Result<Option<u32>> {
        let tabs = Self::list_tabs()?;
        Ok(tabs
            .into_iter()
            .find(|t| t.name == name)
            .map(|t| t.tab_id()))
    }

    fn find_tab_by_name(full_name: &str) -> Result<TabInfo> {
        Self::list_tabs()?
            .into_iter()
            .find(|t| t.name == full_name)
            .ok_or_else(|| anyhow!("Window '{}' not found", full_name))
    }

    fn go_to_tab_by_id(tab_id: u32) -> Result<()> {
        Cmd::new("zellij")
            .args(&["action", "go-to-tab-by-id", &tab_id.to_string()])
            .run()
            .with_context(|| format!("Failed to switch to zellij tab {}", tab_id))?;
        Ok(())
    }

    fn tab_id_for_pane(pane_id: &str) -> Result<Option<u32>> {
        let numeric_id =
            parse_pane_id(pane_id).ok_or_else(|| anyhow!("Invalid pane_id: {}", pane_id))?;
        Ok(Self::list_panes()?
            .into_iter()
            .find(|p| p.id == numeric_id && !p.is_plugin)
            .and_then(|p| p.tab_id))
    }

    fn build_live_pane_info(pane: &PaneInfo) -> LivePaneInfo {
        let current_command = extract_base_command(
            pane.pane_command.as_deref(),
            pane.terminal_command.as_deref(),
        );
        let current_command = if current_command.is_empty() {
            None
        } else {
            Some(current_command)
        };

        let working_dir = pane
            .pane_cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        LivePaneInfo {
            pid: None,
            current_command,
            working_dir,
            title: Some(pane.title.clone()).filter(|t| !t.is_empty()),
            session: Self::session_name(),
            window: Some(pane.tab_name.clone()).filter(|t| !t.is_empty()),
            session_id: None,
            window_id: None,
        }
    }

    fn focus_pane_by_id(pane_id: &str) -> Result<()> {
        Cmd::new("zellij")
            .args(&["action", "focus-pane-id", pane_id])
            .run()
            .with_context(|| format!("Failed to focus zellij pane '{}'", pane_id))?;
        Ok(())
    }
}

impl Multiplexer for ZellijBackend {
    fn name(&self) -> &'static str {
        "zellij"
    }

    fn supports_preview(&self) -> bool {
        false // Preview requires expensive process spawning
    }

    fn requires_focus_for_input(&self) -> bool {
        true // Zellij's write-chars with --pane-id works, but tab must be active
    }

    fn should_exit_on_jump(&self) -> bool {
        false // Dashboard runs in a persistent tab; keep it alive when switching to agent tabs
    }

    // === Server/Session ===

    fn is_running(&self) -> Result<bool> {
        if Self::is_inside_session() {
            return Ok(true);
        }
        // Try a simple command to check if zellij is accessible
        Cmd::new("zellij")
            .args(&["action", "dump-screen", "/dev/null"])
            .run_as_check()
    }

    fn current_pane_id(&self) -> Option<String> {
        // Fast path: Try environment variable first
        Self::pane_id_from_env()
    }

    fn active_pane_id(&self) -> Option<String> {
        // Reliable path: Query focused pane ID
        Self::focused_pane_id()
            .ok()
            .map(|id| format!("terminal_{}", id))
    }

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        // Zellij doesn't expose this via CLI
        // Fall back to current directory
        std::env::current_dir().context("Failed to get current directory")
    }

    fn instance_id(&self) -> String {
        Self::session_name().unwrap_or_else(|| "default".to_string())
    }

    // === Session Management (not supported in Zellij) ===

    fn create_session(&self, _params: super::types::CreateSessionParams) -> Result<String> {
        Err(anyhow!(
            "Session mode (--session) is not supported in Zellij. Use window mode instead."
        ))
    }

    fn switch_to_session(&self, _prefix: &str, _name: &str) -> Result<()> {
        Err(anyhow!(
            "Session mode is not supported in Zellij. Use window mode instead."
        ))
    }

    fn schedule_session_close(&self, _full_name: &str, _delay: Duration) -> Result<()> {
        Err(anyhow!(
            "Session mode is not supported in Zellij. Use window mode instead."
        ))
    }

    fn wait_until_session_closed(&self, _full_session_name: &str) -> Result<()> {
        Err(anyhow!(
            "Session mode is not supported in Zellij. Use window mode instead."
        ))
    }

    fn run_deferred_script(&self, script: &str) -> Result<()> {
        util::run_detached_sh_c(script)
    }

    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String> {
        let tab = Self::find_tab_by_name(full_name)?;
        Ok(format!(
            "zellij action go-to-tab-by-id {} >/dev/null 2>&1",
            tab.tab_id()
        ))
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        let tab = Self::find_tab_by_name(full_name)?;
        Ok(format!(
            "zellij action close-tab-by-id {} >/dev/null 2>&1",
            tab.tab_id()
        ))
    }

    fn shell_switch_session_cmd(&self, _full_name: &str) -> Result<String> {
        Err(anyhow!(
            "Session mode is not supported in Zellij. Use window mode instead."
        ))
    }

    fn shell_kill_session_cmd(&self, _full_name: &str) -> Result<String> {
        Err(anyhow!(
            "Session mode is not supported in Zellij. Use window mode instead."
        ))
    }

    // === Window/Tab Management ===

    /// Create a new tab in Zellij.
    /// Returns: Pane ID of the initial pane (e.g., "terminal_5")
    fn create_window(&self, params: CreateWindowParams) -> Result<String> {
        let full_name = format!("{}{}", params.prefix, params.name);
        let cwd_str = params
            .cwd
            .to_str()
            .ok_or_else(|| anyhow!("Path contains non-UTF8 characters"))?;

        if params.after_window.is_some() {
            debug!("Zellij does not support window insertion order - ignoring after_window");
        }

        // new-tab returns tab_id on stdout and auto-focuses the new tab
        let tab_id_str = Cmd::new("zellij")
            .args(&["action", "new-tab", "--name", &full_name, "--cwd", cwd_str])
            .run_and_capture_stdout()
            .with_context(|| format!("Failed to create zellij tab '{}'", full_name))?;

        let tab_id: u32 = tab_id_str
            .trim()
            .parse()
            .with_context(|| format!("Invalid tab ID from new-tab: '{}'", tab_id_str.trim()))?;

        Self::go_to_tab_by_id(tab_id)?;

        // Find the initial pane in the new tab by tab_id
        let panes = Self::list_panes()?;
        let pane = panes
            .iter()
            .find(|p| !p.is_plugin && p.tab_id == Some(tab_id))
            .ok_or_else(|| anyhow!("No terminal pane found in new tab {}", tab_id))?;

        Ok(format!("terminal_{}", pane.id))
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        // Try to find the tab by name and close it by ID (zellij PR #4695)
        let tabs = Self::list_tabs()?;
        if let Some(tab) = tabs.iter().find(|t| t.name == full_name) {
            let tab_id = tab.tab_id().to_string();
            Cmd::new("zellij")
                .args(&["action", "close-tab-by-id", &tab_id])
                .run()
                .context("Failed to close zellij tab by ID")?;
        } else {
            // Fallback to old method if tab not found
            warn!("Tab '{}' not found, using fallback close method", full_name);
            Cmd::new("zellij")
                .args(&["action", "go-to-tab-name", full_name])
                .run()
                .context("Failed to switch to tab for closing")?;

            Cmd::new("zellij")
                .args(&["action", "close-tab"])
                .run()
                .context("Failed to close zellij tab")?;
        }
        Ok(())
    }

    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        // Try to find the tab ID for more reliable closing (zellij PR #4695)
        let tabs = Self::list_tabs()?;
        let tab_id = tabs
            .iter()
            .find(|t| t.name == full_name)
            .map(|t| t.tab_id().to_string());

        let delay_secs = delay.as_secs();

        let cmd = if let Some(id) = tab_id {
            // Use ID-based close (no need to focus the tab first)
            format!(
                "sleep {} && zellij action close-tab-by-id {}",
                delay_secs, id
            )
        } else {
            // Fallback to name-based close
            format!(
                "sleep {} && zellij action go-to-tab-name '{}' && zellij action close-tab",
                delay_secs,
                full_name.replace('\'', "'\\''")
            )
        };

        std::process::Command::new("sh")
            .args(["-c", &cmd])
            .spawn()
            .context("Failed to spawn delayed close")?;

        Ok(())
    }

    fn select_window(&self, prefix: &str, name: &str) -> Result<()> {
        let full_name = format!("{}{}", prefix, name);

        // Try to find the tab by name and switch by ID (zellij PR #4695)
        let tabs = Self::list_tabs()?;
        if let Some(tab) = tabs.iter().find(|t| t.name == full_name) {
            let tab_id = tab.tab_id().to_string();
            Cmd::new("zellij")
                .args(&["action", "go-to-tab-by-id", &tab_id])
                .run()
                .context("Failed to select zellij tab by ID")?;
        } else {
            // Fallback to old method
            warn!(
                "Tab '{}' not found, using fallback select method",
                full_name
            );
            Cmd::new("zellij")
                .args(&["action", "go-to-tab-name", &full_name])
                .run()
                .context("Failed to select zellij tab")?;
        }
        Ok(())
    }

    fn window_exists(&self, prefix: &str, name: &str) -> Result<bool> {
        let full_name = format!("{}{}", prefix, name);
        self.window_exists_by_full_name(&full_name)
    }

    fn window_exists_by_full_name(&self, full_name: &str) -> Result<bool> {
        if !Self::is_inside_session() {
            return Ok(false);
        }

        let tabs = Self::list_tabs()?;
        Ok(tabs.iter().any(|t| t.name == full_name))
    }

    fn current_window_name(&self) -> Result<Option<String>> {
        Ok(Self::focused_tab_name())
    }

    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        if !Self::is_inside_session() {
            return Ok(HashSet::new());
        }

        // Use list_tabs() for richer metadata and better efficiency
        let tabs = Self::list_tabs()?;
        Ok(tabs.into_iter().map(|t| t.name).collect())
    }

    fn wait_until_windows_closed(&self, full_window_names: &[String]) -> Result<()> {
        use std::thread;

        loop {
            let active = self.get_all_window_names()?;
            if full_window_names.iter().all(|w| !active.contains(w)) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    // === Pane Management ===

    fn select_pane(&self, pane_id: &str) -> Result<()> {
        if Self::focus_pane_by_id(pane_id).is_ok() {
            return Ok(());
        }

        // Fallback for older Zellij builds without focus-pane-id: navigate with
        // focus-next-pane/focus-previous-pane in the current tab.

        // Extract numeric ID from pane_id
        let target_id: u32 =
            parse_pane_id(pane_id).ok_or_else(|| anyhow!("Invalid pane_id: {}", pane_id))?;

        // Get focused tab name to filter panes
        let focused_tab =
            Self::focused_tab_name().ok_or_else(|| anyhow!("Could not determine focused tab"))?;

        // Get all panes in the current tab
        let all_panes = Self::list_panes()?;
        let tab_panes: Vec<_> = all_panes
            .iter()
            .filter(|p| !p.is_plugin && p.tab_name == focused_tab)
            .collect();

        // Find current and target indices
        let current_idx = tab_panes
            .iter()
            .position(|p| p.is_focused)
            .ok_or_else(|| anyhow!("No focused pane found in current tab"))?;

        let target_idx = tab_panes
            .iter()
            .position(|p| p.id == target_id)
            .ok_or_else(|| anyhow!("Target pane {} not found in current tab", pane_id))?;

        if current_idx == target_idx {
            // Already focused
            return Ok(());
        }

        // Navigate to target pane
        if target_idx < current_idx {
            // Navigate backwards
            let steps = current_idx - target_idx;
            debug!(
                current_idx,
                target_idx, steps, "Navigating backwards to focused pane"
            );
            for _ in 0..steps {
                Cmd::new("zellij")
                    .args(&["action", "focus-previous-pane"])
                    .run()
                    .context("Failed to navigate to previous pane")?;
            }
        } else {
            // Navigate forwards
            let steps = target_idx - current_idx;
            debug!(
                current_idx,
                target_idx, steps, "Navigating forwards to focused pane"
            );
            for _ in 0..steps {
                Cmd::new("zellij")
                    .args(&["action", "focus-next-pane"])
                    .run()
                    .context("Failed to navigate to next pane")?;
            }
        }

        Ok(())
    }

    fn switch_to_pane(&self, pane_id: &str, window_hint: Option<&str>) -> Result<()> {
        // Zellij can't switch to arbitrary panes by ID, so switch to the containing tab.
        let tab_name = window_hint.ok_or_else(|| {
            anyhow!(
                "Zellij switch_to_pane requires window_hint (tab name) for pane '{}'",
                pane_id
            )
        })?;

        debug!(pane_id, tab_name, "switch_to_pane: switching to tab");

        // Try to switch by tab ID for more reliability
        let tabs = Self::list_tabs()?;
        if let Some(tab) = tabs.iter().find(|t| t.name == tab_name) {
            let tab_id = tab.tab_id().to_string();
            Cmd::new("zellij")
                .args(&["action", "go-to-tab-by-id", &tab_id])
                .run()
                .with_context(|| format!("Failed to switch to tab '{}' by ID", tab_name))?;
        } else {
            // Fallback to name-based switch
            Cmd::new("zellij")
                .args(&["action", "go-to-tab-name", tab_name])
                .run()
                .with_context(|| format!("Failed to switch to tab '{}'", tab_name))?;
        }

        Ok(())
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        let numeric_id =
            parse_pane_id(pane_id).ok_or_else(|| anyhow!("Invalid pane_id format: {}", pane_id))?;
        let panes = Self::list_panes().context("Failed to list panes in kill_pane")?;
        let tab_id = panes
            .iter()
            .find(|p| p.id == numeric_id && !p.is_plugin)
            .and_then(|p| p.tab_id)
            .ok_or_else(|| anyhow!("Pane {} not found or tab_id unavailable", pane_id))?;
        Cmd::new("zellij")
            .args(&["action", "close-tab-by-id", &tab_id.to_string()])
            .run()?;
        Ok(())
    }

    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        debug!(pane_id, "respawn_pane: starting");

        // Verify the pane exists - if list-panes returns it, it's ready for --pane-id targeting
        let panes = Self::list_panes().context("Failed to list panes in respawn_pane")?;
        let numeric_id: u32 =
            parse_pane_id(pane_id).ok_or_else(|| anyhow!("Invalid pane_id format: {}", pane_id))?;

        if !panes.iter().any(|p| p.id == numeric_id && !p.is_plugin) {
            return Err(anyhow!(
                "Pane {} not found. Available panes: {:?}",
                pane_id,
                panes
                    .iter()
                    .map(|p| format!("terminal_{}", p.id))
                    .collect::<Vec<_>>()
            ));
        }

        // Zellij doesn't have respawn-pane; send cd + command to the target pane
        let cwd_str = cwd
            .to_str()
            .ok_or_else(|| anyhow!("Path contains non-UTF8 characters"))?;

        // Combine cd + command into a single write-chars call to reduce subprocess spawns
        let combined = if let Some(command) = cmd {
            debug!(
                pane_id,
                command = command.chars().take(100).collect::<String>(),
                "respawn_pane: sending cd + command"
            );
            format!("cd '{}' && {}", cwd_str.replace('\'', "'\\''"), command)
        } else {
            debug!(pane_id, "respawn_pane: sending cd command");
            format!("cd '{}'", cwd_str.replace('\'', "'\\''"))
        };

        Cmd::new("zellij")
            .args(&["action", "write-chars", "--pane-id", pane_id, &combined])
            .run()?;
        Cmd::new("zellij")
            .args(&["action", "write", "--pane-id", pane_id, "13"])
            .run()?;

        debug!(pane_id, "respawn_pane: completed");
        Ok(pane_id.to_string())
    }

    fn capture_pane(&self, _pane_id: &str, _lines: u16) -> Option<String> {
        // Zellij limitation: dump-screen always captures the focused pane,
        // not the pane specified by pane_id. When the dashboard is focused,
        // it captures itself, creating a recursive loop. We detect this and
        // return None to prevent the recursion.

        // Use PID + thread ID + timestamp for thread-safe temp file naming
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let thread_id = std::thread::current().id();
        let temp_path = std::env::temp_dir().join(format!(
            "zellij_capture_{}_{:?}_{}",
            std::process::id(),
            thread_id,
            timestamp
        ));
        let temp_str = temp_path.to_string_lossy();

        if Cmd::new("zellij")
            .args(&["action", "dump-screen", &temp_str])
            .run()
            .is_ok()
        {
            if let Ok(content) = std::fs::read_to_string(&temp_path) {
                let _ = std::fs::remove_file(&temp_path);
                return Some(content);
            }
            let _ = std::fs::remove_file(&temp_path);
        }

        None
    }

    // === Text I/O ===

    fn send_text_fragment(&self, pane_id: &str, text: &str) -> Result<()> {
        Cmd::new("zellij")
            .args(&["action", "write-chars", "--pane-id", pane_id, text])
            .run()
            .context("Failed to send text to pane")
            .map(|_| ())
    }

    fn send_enter(&self, pane_id: &str) -> Result<()> {
        Cmd::new("zellij")
            .args(&["action", "write", "--pane-id", pane_id, "13"])
            .run()
            .context("Failed to send Enter")
            .map(|_| ())
    }

    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        // Map common key names to ASCII codes
        let code = match key {
            "Enter" => "13",
            "Escape" => "27",
            "Tab" => "9",
            _ => {
                // For single chars, use write-chars with pane targeting
                Cmd::new("zellij")
                    .args(&["action", "write-chars", "--pane-id", pane_id, key])
                    .run()
                    .context("Failed to send key")?;
                return Ok(());
            }
        };

        Cmd::new("zellij")
            .args(&["action", "write", "--pane-id", pane_id, code])
            .run()
            .context("Failed to send key")?;
        Ok(())
    }

    fn paste_text(&self, pane_id: &str, content: &str) -> Result<()> {
        Cmd::new("zellij")
            .args(&["action", "write-chars", "--pane-id", pane_id, content])
            .run()?;
        Ok(())
    }

    fn paste_multiline(&self, pane_id: &str, content: &str) -> Result<()> {
        // Send line by line with pane targeting
        for line in content.lines() {
            Cmd::new("zellij")
                .args(&["action", "write-chars", "--pane-id", pane_id, line])
                .run()?;
            Cmd::new("zellij")
                .args(&["action", "write", "--pane-id", pane_id, "13"])
                .run()?;
        }
        Ok(())
    }

    fn clear_pane(&self, pane_id: &str) -> Result<()> {
        // Clear the pane to hide handshake setup commands
        // Try with --pane-id first, fall back to focused pane if not supported
        let result = Cmd::new("zellij")
            .args(&["action", "clear", "--pane-id", pane_id])
            .run();

        if result.is_err() {
            // Fallback for older zellij versions without --pane-id support for clear
            Cmd::new("zellij")
                .args(&["action", "clear"])
                .run()
                .context("Failed to clear pane")?;
        }
        Ok(())
    }

    // === Shell ===

    fn default_shell_fallback(&self) -> &'static str {
        "/bin/sh"
    }

    // === Status ===

    fn set_status(&self, _pane_id: &str, _icon: &str, _auto_clear_on_focus: bool) -> Result<()> {
        // No-op: can't target specific panes, and rename-pane would hijack
        // the user's focused pane. Status is tracked in StateStore by tab name.
        Ok(())
    }

    fn clear_status(&self, _pane_id: &str) -> Result<()> {
        // No-op: status is managed by StateStore
        Ok(())
    }

    fn ensure_status_format(&self, _pane_id: &str) -> Result<()> {
        // No-op for zellij
        Ok(())
    }

    // === Pane Setup ===

    // Use default implementation from trait - no need for Zellij-specific workarounds
    // now that pane targeting is reliable with --pane-id (zellij PR #4691)

    /// Split a pane in Zellij.
    ///
    /// **Zellij CLI Limitations:**
    /// - `size`/`percentage` are ignored - all splits are 50/50.
    ///
    /// **Returns:** The pane ID from `new-pane` stdout (e.g., "terminal_5").
    fn split_pane(
        &self,
        target_pane_id: &str,
        direction: &SplitDirection,
        cwd: &Path,
        _size: Option<u16>,
        _percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        if let Some(tab_id) = Self::tab_id_for_pane(target_pane_id)? {
            Self::go_to_tab_by_id(tab_id)?;
        }

        if let Err(err) = Self::focus_pane_by_id(target_pane_id) {
            debug!(
                target_pane_id,
                error = %err,
                "split_pane: failed to focus target pane, falling back to zellij's current focus"
            );
        }

        let cwd_str = cwd
            .to_str()
            .ok_or_else(|| anyhow!("Path contains non-UTF8 characters"))?;

        let mut cmd = Cmd::new("zellij")
            .args(&["action", "new-pane"])
            .args(zellij_new_pane_direction_args(direction))
            .args(&["--cwd", cwd_str]);

        // Pass command inline via -- syntax (runs as `sh -c 'script'`)
        if let Some(script) = command {
            cmd = cmd.args(&["--", "sh", "-c", script]);
        }

        // new-pane returns pane ID on stdout (e.g., "terminal_5")
        let pane_id = cmd
            .run_and_capture_stdout()
            .context("Failed to split pane")?;

        Ok(pane_id.trim().to_string())
    }

    // === State Reconciliation ===

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        let panes = Self::list_panes()?;

        // Extract numeric ID from "terminal_X"
        let numeric_id: u32 =
            parse_pane_id(pane_id).ok_or_else(|| anyhow!("Invalid pane_id: {}", pane_id))?;

        // Find pane by ID
        let pane = match panes.iter().find(|p| p.id == numeric_id && !p.is_plugin) {
            Some(p) => p,
            None => return Ok(None), // Pane doesn't exist
        };

        Ok(Some(Self::build_live_pane_info(pane)))
    }

    fn validate_agent_alive(&self, state: &crate::state::AgentState) -> Result<bool> {
        // Check if pane exists
        let pane_info = self.get_live_pane_info(&state.pane_key.pane_id)?;
        let pane_info = match pane_info {
            Some(info) => info,
            None => return Ok(false), // Pane doesn't exist
        };

        // Secondary validation: Check if command matches stored command
        // This detects if the agent process was killed and replaced with something else
        if let Some(ref live_command) = pane_info.current_command
            && !state.command.is_empty()
            && !live_command.is_empty()
        {
            // Extract base command name for comparison
            let expected_base = state
                .command
                .split('/')
                .next_back()
                .unwrap_or(&state.command);
            let actual_base = live_command.split('/').next_back().unwrap_or(live_command);

            if expected_base != actual_base {
                debug!(
                    "Agent validation: command mismatch - expected '{}', got '{}'",
                    expected_base, actual_base
                );
                return Ok(false); // Different command running
            }
        }

        Ok(true) // Agent is valid
    }

    fn get_all_live_pane_info(&self) -> Result<std::collections::HashMap<String, LivePaneInfo>> {
        use std::collections::HashMap;

        let mut result = HashMap::new();

        // Use list-panes to get all panes (not just focused ones)
        let panes = Self::list_panes()?;

        for pane in panes {
            // Skip plugin panes, only include terminal panes
            if pane.is_plugin {
                continue;
            }

            let pane_id = format!("terminal_{}", pane.id);
            result.insert(pane_id, Self::build_live_pane_info(&pane));
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // === parse_pane_id ===

    #[test]
    fn parse_pane_id_valid() {
        assert_eq!(parse_pane_id("terminal_0"), Some(0));
        assert_eq!(parse_pane_id("terminal_1"), Some(1));
        assert_eq!(parse_pane_id("terminal_42"), Some(42));
        assert_eq!(parse_pane_id("terminal_999"), Some(999));
    }

    #[test]
    fn parse_pane_id_invalid_prefix() {
        assert_eq!(parse_pane_id("plugin_1"), None);
        assert_eq!(parse_pane_id("pane_1"), None);
        assert_eq!(parse_pane_id("1"), None);
        assert_eq!(parse_pane_id(""), None);
    }

    #[test]
    fn parse_pane_id_non_numeric() {
        assert_eq!(parse_pane_id("terminal_abc"), None);
        assert_eq!(parse_pane_id("terminal_"), None);
        assert_eq!(parse_pane_id("terminal_1.5"), None);
        assert_eq!(parse_pane_id("terminal_-1"), None);
    }

    // === extract_base_command ===

    #[test]
    fn extract_base_command_full_path() {
        assert_eq!(extract_base_command(Some("/usr/bin/bash"), None), "bash");
    }

    #[test]
    fn extract_base_command_with_args() {
        assert_eq!(
            extract_base_command(Some("/usr/bin/bash --login -i"), None),
            "bash"
        );
    }

    #[test]
    fn extract_base_command_bare_name() {
        assert_eq!(extract_base_command(Some("zsh"), None), "zsh");
    }

    #[test]
    fn extract_base_command_prefers_pane_command() {
        assert_eq!(extract_base_command(Some("fish"), Some("bash")), "fish");
    }

    #[test]
    fn extract_base_command_falls_back_to_terminal_command() {
        assert_eq!(extract_base_command(None, Some("/bin/zsh")), "zsh");
    }

    #[test]
    fn extract_base_command_both_none() {
        assert_eq!(extract_base_command(None, None), "");
    }

    #[test]
    fn extract_base_command_empty_strings() {
        assert_eq!(extract_base_command(Some(""), None), "");
    }

    // === parse_tab_name_from_output ===

    #[test]
    fn parse_tab_name_standard() {
        let output = "name: Tab #1\nid: 0\nposition: 0\n";
        assert_eq!(
            parse_tab_name_from_output(output),
            Some("Tab #1".to_string())
        );
    }

    #[test]
    fn parse_tab_name_custom_name() {
        let output = "name: my-worktree\nid: 3\nposition: 2\n";
        assert_eq!(
            parse_tab_name_from_output(output),
            Some("my-worktree".to_string())
        );
    }

    #[test]
    fn parse_tab_name_with_spaces() {
        let output = "name: My Project Tab\nid: 1\nposition: 0\n";
        assert_eq!(
            parse_tab_name_from_output(output),
            Some("My Project Tab".to_string())
        );
    }

    #[test]
    fn parse_tab_name_empty_output() {
        assert_eq!(parse_tab_name_from_output(""), None);
    }

    #[test]
    fn parse_tab_name_no_name_field() {
        let output = "id: 0\nposition: 0\n";
        assert_eq!(parse_tab_name_from_output(output), None);
    }

    #[test]
    fn parse_tab_name_name_field_in_middle() {
        let output = "id: 5\nname: middle-tab\nposition: 3\nactive: true\n";
        assert_eq!(
            parse_tab_name_from_output(output),
            Some("middle-tab".to_string())
        );
    }

    // === zellij_new_pane_direction_args ===

    #[test]
    fn new_pane_direction_args_horizontal() {
        assert_eq!(
            zellij_new_pane_direction_args(&SplitDirection::Horizontal),
            &["--direction", "right"]
        );
    }

    #[test]
    fn new_pane_direction_args_vertical() {
        assert_eq!(
            zellij_new_pane_direction_args(&SplitDirection::Vertical),
            &["--direction", "down"]
        );
    }

    #[test]
    fn new_pane_direction_args_stacked() {
        assert_eq!(
            zellij_new_pane_direction_args(&SplitDirection::Stacked),
            &["--stacked"]
        );
    }

    // === PaneInfo deserialization ===

    #[test]
    fn pane_info_deserialize_full() {
        let json = r#"{
            "id": 5,
            "is_plugin": false,
            "is_focused": true,
            "terminal_command": "/bin/bash",
            "pane_command": "/usr/bin/fish",
            "pane_cwd": "/home/user/project",
            "tab_id": 2,
            "tab_name": "my-tab",
            "title": "fish"
        }"#;

        let pane: PaneInfo = serde_json::from_str(json).unwrap();
        assert_eq!(pane.id, 5);
        assert!(!pane.is_plugin);
        assert!(pane.is_focused);
        assert_eq!(pane.terminal_command.as_deref(), Some("/bin/bash"));
        assert_eq!(pane.pane_command.as_deref(), Some("/usr/bin/fish"));
        assert_eq!(pane.pane_cwd.as_deref(), Some("/home/user/project"));
        assert_eq!(pane.tab_id, Some(2));
        assert_eq!(pane.tab_name, "my-tab");
        assert_eq!(pane.title, "fish");
    }

    #[test]
    fn pane_info_deserialize_minimal() {
        // Only required fields; optional fields use serde defaults
        let json = r#"{
            "id": 0,
            "is_plugin": true,
            "is_focused": false,
            "terminal_command": null
        }"#;

        let pane: PaneInfo = serde_json::from_str(json).unwrap();
        assert_eq!(pane.id, 0);
        assert!(pane.is_plugin);
        assert!(!pane.is_focused);
        assert!(pane.terminal_command.is_none());
        assert!(pane.pane_command.is_none());
        assert!(pane.pane_cwd.is_none());
        assert!(pane.tab_id.is_none());
        assert_eq!(pane.tab_name, "");
        assert_eq!(pane.title, "");
    }

    #[test]
    fn pane_info_deserialize_list() {
        let json = r#"[
            {"id": 1, "is_plugin": false, "is_focused": true, "terminal_command": "bash", "tab_name": "tab1"},
            {"id": 2, "is_plugin": true, "is_focused": false, "terminal_command": null, "tab_name": "tab1"}
        ]"#;

        let panes: Vec<PaneInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].id, 1);
        assert!(!panes[0].is_plugin);
        assert_eq!(panes[1].id, 2);
        assert!(panes[1].is_plugin);
    }

    // === TabInfo deserialization ===

    #[test]
    fn tab_info_deserialize() {
        let json = r#"{
            "tab_id": 3,
            "position": 1,
            "name": "workmux-feature",
            "active": true
        }"#;

        let tab: TabInfo = serde_json::from_str(json).unwrap();
        assert_eq!(tab.tab_id(), 3);
        assert_eq!(tab.position, 1);
        assert_eq!(tab.name, "workmux-feature");
        assert!(tab.active);
    }

    #[test]
    fn tab_info_deserialize_list() {
        let json = r#"[
            {"tab_id": 0, "position": 0, "name": "Tab #1", "active": true},
            {"tab_id": 1, "position": 1, "name": "my-feature", "active": false}
        ]"#;

        let tabs: Vec<TabInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].tab_id(), 0);
        assert_eq!(tabs[0].name, "Tab #1");
        assert!(tabs[0].active);
        assert_eq!(tabs[1].tab_id(), 1);
        assert_eq!(tabs[1].name, "my-feature");
        assert!(!tabs[1].active);
    }
}
