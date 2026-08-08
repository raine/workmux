//! Zellij multiplexer backend.
//!
//! Limitations:
//! - No percentage-based pane size control (can resize with +/- but not set exact %)
//! - No window insertion order (tabs always append)

use anyhow::{Context, Result, anyhow};
use serde::de::DeserializeOwned;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, warn};

use crate::cmd::Cmd;
use crate::config::SplitDirection;

use super::types::{CreateWindowParams, LivePaneInfo};
use super::{Multiplexer, util};

// Zero-width markers let workmux replace or remove only the status suffix it
// owns, without mistaking user-provided title text or custom icons for status.
const STATUS_MARKER_OPEN: char = '\u{2063}';
const STATUS_MARKER_CLOSE: char = '\u{2064}';

/// Zellij multiplexer backend.
pub struct ZellijBackend {
    session_name: Option<String>,
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

fn normalize_terminal_pane_id(raw: &str) -> Option<String> {
    raw.split_whitespace().find_map(|token| {
        if let Some(id) = parse_pane_id(token) {
            Some(format!("terminal_{}", id))
        } else {
            token
                .parse::<u32>()
                .ok()
                .map(|id| format!("terminal_{}", id))
        }
    })
}

fn is_terminal_pane_in_tab(pane: &PaneInfo, tab_id: Option<u32>) -> bool {
    !pane.is_plugin && tab_id.map(|id| pane.tab_id == Some(id)).unwrap_or(true)
}

fn terminal_pane_ids_in_tab(panes: &[PaneInfo], tab_id: Option<u32>) -> HashSet<u32> {
    panes
        .iter()
        .filter(|pane| is_terminal_pane_in_tab(pane, tab_id))
        .map(|pane| pane.id)
        .collect()
}

fn find_created_terminal_pane_id(
    before_ids: &HashSet<u32>,
    panes: &[PaneInfo],
    tab_id: Option<u32>,
    expected_command: Option<&str>,
) -> Option<String> {
    let candidates: Vec<_> = panes
        .iter()
        .filter(|pane| is_terminal_pane_in_tab(pane, tab_id))
        .filter(|pane| !before_ids.contains(&pane.id))
        .collect();

    if let Some(expected_command) = expected_command {
        let matching: Vec<_> = candidates
            .iter()
            .filter(|pane| {
                pane.terminal_command
                    .as_deref()
                    .into_iter()
                    .chain(pane.pane_command.as_deref())
                    .any(|command| command.contains(expected_command))
            })
            .collect();
        if let [pane] = matching.as_slice() {
            return Some(format!("terminal_{}", pane.id));
        }
    }

    match candidates.as_slice() {
        [pane] => Some(format!("terminal_{}", pane.id)),
        _ => None,
    }
}

fn pane_navigation(panes: &[PaneInfo], target_id: u32) -> Result<(u32, isize)> {
    let target = panes
        .iter()
        .find(|pane| pane.id == target_id && !pane.is_plugin)
        .ok_or_else(|| anyhow!("Target pane terminal_{} not found", target_id))?;
    let tab_id = target
        .tab_id
        .ok_or_else(|| anyhow!("Target pane terminal_{} has no tab ID", target_id))?;
    let tab_panes: Vec<_> = panes
        .iter()
        .filter(|pane| !pane.is_plugin && pane.tab_id == Some(tab_id))
        .collect();
    let current_idx = tab_panes
        .iter()
        .position(|pane| pane.is_focused)
        .ok_or_else(|| anyhow!("No focused pane found in tab {}", tab_id))?;
    let target_idx = tab_panes
        .iter()
        .position(|pane| pane.id == target_id)
        .expect("target pane is included in its tab");

    Ok((tab_id, target_idx as isize - current_idx as isize))
}

fn query_json_with_retry<T, F>(mut query: F, parse_context: &str, delay: Duration) -> Result<T>
where
    T: DeserializeOwned,
    F: FnMut() -> Result<String>,
{
    const ATTEMPTS: usize = 5;

    let mut last_error = None;
    for attempt in 0..ATTEMPTS {
        let output = query()?;
        match serde_json::from_str(&output) {
            Ok(value) => return Ok(value),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(delay);
        }
    }

    Err(last_error.expect("at least one JSON parse attempt"))
        .with_context(|| parse_context.to_string())
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

fn tab_name_without_status(name: &str) -> Option<&str> {
    if !name.ends_with(STATUS_MARKER_CLOSE) {
        return None;
    }

    let marker_start = name.rfind(STATUS_MARKER_OPEN)?;
    name[..marker_start].strip_suffix(' ')
}

fn canonical_tab_name(name: &str) -> &str {
    tab_name_without_status(name).unwrap_or(name)
}

fn tab_name_with_status(name: &str, icon: &str) -> String {
    let base = canonical_tab_name(name);
    let icon = crate::tmux_style::strip_tmux_styles(icon)
        .replace([STATUS_MARKER_OPEN, STATUS_MARKER_CLOSE], "");
    format!("{base} {STATUS_MARKER_OPEN}{icon}{STATUS_MARKER_CLOSE}")
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
        Self {
            session_name: std::env::var("ZELLIJ_SESSION_NAME").ok(),
        }
    }

    pub fn for_session(session_name: impl Into<String>) -> Self {
        Self {
            session_name: Some(session_name.into()),
        }
    }

    fn command(&self) -> Cmd<'_> {
        let command = Cmd::new("zellij");
        match self.session_name.as_deref() {
            Some(session_name) => command.args(&["--session", session_name]),
            None => command,
        }
    }

    fn shell_command(&self) -> String {
        match self.session_name.as_deref() {
            Some(session_name) => format!(
                "zellij --session {}",
                super::agent::shell_quote(session_name)
            ),
            None => "zellij".to_string(),
        }
    }

    /// Check if this backend identifies a zellij session
    fn is_inside_session(&self) -> bool {
        self.session_name.is_some() || std::env::var("ZELLIJ").is_ok()
    }

    /// Get session name from the bound backend or environment
    fn session_name(&self) -> Option<String> {
        self.session_name
            .clone()
            .or_else(|| std::env::var("ZELLIJ_SESSION_NAME").ok())
    }

    /// Get current pane ID from environment (format: terminal_1, plugin_2, etc.)
    fn pane_id_from_env() -> Option<String> {
        std::env::var("ZELLIJ_PANE_ID")
            .ok()
            .map(|id| format!("terminal_{}", id))
    }

    /// Get the name of the currently focused tab using `current-tab-info`.
    fn focused_tab_name(&self) -> Option<String> {
        let output = self
            .command()
            .args(&["action", "current-tab-info"])
            .run_and_capture_stdout()
            .ok()?;

        parse_tab_name_from_output(&output)
    }

    /// Query all panes using `zellij action list-panes --json --tab --command`
    ///
    /// The `--tab` flag includes `tab_id`, `tab_name`, `tab_position`.
    /// The `--command` flag includes `pane_command`, `pane_cwd`.
    fn list_panes(&self) -> Result<Vec<PaneInfo>> {
        query_json_with_retry(
            || {
                self.command()
                    .args(&["action", "list-panes", "--json", "--tab", "--command"])
                    .run_and_capture_stdout()
                    .context("Failed to list panes")
            },
            "Failed to parse list-panes JSON output",
            Duration::from_millis(50),
        )
    }

    /// Query all tabs using `zellij action list-tabs --json`
    fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        let mut tabs: Vec<TabInfo> = query_json_with_retry(
            || {
                self.command()
                    .args(&["action", "list-tabs", "--json"])
                    .run_and_capture_stdout()
                    .context("Failed to list tabs")
            },
            "Failed to parse list-tabs JSON output",
            Duration::from_millis(50),
        )?;
        for tab in &mut tabs {
            tab.name = canonical_tab_name(&tab.name).to_string();
        }
        Ok(tabs)
    }

    /// Get focused pane ID from list-panes output
    ///
    /// Returns the focused pane in the currently active tab.
    fn focused_pane_id(&self) -> Result<u32> {
        let panes = self.list_panes()?;
        let focused_tab = self.focused_tab_name();

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
    fn get_tab_id_by_name(&self, name: &str) -> Result<Option<u32>> {
        let tabs = self.list_tabs()?;
        Ok(tabs
            .into_iter()
            .find(|t| t.name == name)
            .map(|t| t.tab_id()))
    }

    fn find_tab_by_name(&self, full_name: &str) -> Result<TabInfo> {
        self.list_tabs()?
            .into_iter()
            .find(|t| t.name == full_name)
            .ok_or_else(|| anyhow!("Window '{}' not found", full_name))
    }

    fn go_to_tab_by_id(&self, tab_id: u32) -> Result<()> {
        self.command()
            .args(&["action", "go-to-tab-by-id", &tab_id.to_string()])
            .run()
            .with_context(|| format!("Failed to switch to zellij tab {}", tab_id))?;
        Ok(())
    }

    fn tab_id_for_pane(&self, pane_id: &str) -> Result<Option<u32>> {
        let numeric_id =
            parse_pane_id(pane_id).ok_or_else(|| anyhow!("Invalid pane_id: {}", pane_id))?;
        Ok(self
            .list_panes()?
            .into_iter()
            .find(|p| p.id == numeric_id && !p.is_plugin)
            .and_then(|p| p.tab_id))
    }

    fn tab_for_pane(&self, pane_id: &str) -> Result<Option<(u32, String)>> {
        let numeric_id =
            parse_pane_id(pane_id).ok_or_else(|| anyhow!("Invalid pane_id: {}", pane_id))?;
        Ok(self
            .list_panes()?
            .into_iter()
            .find(|pane| pane.id == numeric_id && !pane.is_plugin)
            .and_then(|pane| pane.tab_id.map(|tab_id| (tab_id, pane.tab_name))))
    }

    fn rename_tab_by_id(&self, tab_id: u32, name: &str) -> Result<()> {
        self.command()
            .args(&["action", "rename-tab-by-id", &tab_id.to_string(), name])
            .run()
            .with_context(|| format!("Failed to rename zellij tab {}", tab_id))?;
        Ok(())
    }

    fn build_live_pane_info(&self, pane: &PaneInfo) -> LivePaneInfo {
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
            session: self.session_name(),
            window: Some(canonical_tab_name(&pane.tab_name).to_string()).filter(|t| !t.is_empty()),
            session_id: None,
            window_id: None,
        }
    }

    fn focus_pane_by_id(&self, pane_id: &str) -> Result<()> {
        self.command()
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
        if self.is_inside_session() {
            return Ok(true);
        }
        // Try a simple command to check if zellij is accessible
        self.command()
            .args(&["action", "dump-screen", "/dev/null"])
            .run_as_check()
    }

    fn current_pane_id(&self) -> Option<String> {
        // Fast path: Try environment variable first
        Self::pane_id_from_env()
    }

    fn active_pane_id(&self) -> Option<String> {
        // Reliable path: Query focused pane ID
        self.focused_pane_id()
            .ok()
            .map(|id| format!("terminal_{}", id))
    }

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        // Zellij doesn't expose this via CLI
        // Fall back to current directory
        std::env::current_dir().context("Failed to get current directory")
    }

    fn instance_id(&self) -> String {
        self.session_name().unwrap_or_else(|| "default".to_string())
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
        let tab = self.find_tab_by_name(full_name)?;
        Ok(format!(
            "{} action go-to-tab-by-id {} >/dev/null 2>&1",
            self.shell_command(),
            tab.tab_id()
        ))
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        let tab = self.find_tab_by_name(full_name)?;
        Ok(format!(
            "{} action close-tab-by-id {} >/dev/null 2>&1",
            self.shell_command(),
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
        let tab_id_str = self
            .command()
            .args(&["action", "new-tab", "--name", &full_name, "--cwd", cwd_str])
            .run_and_capture_stdout()
            .with_context(|| format!("Failed to create zellij tab '{}'", full_name))?;

        let tab_id: u32 = tab_id_str
            .trim()
            .parse()
            .with_context(|| format!("Invalid tab ID from new-tab: '{}'", tab_id_str.trim()))?;

        self.go_to_tab_by_id(tab_id)?;

        // Find the initial pane in the new tab by tab_id
        let panes = self.list_panes()?;
        let pane = panes
            .iter()
            .find(|p| !p.is_plugin && p.tab_id == Some(tab_id))
            .ok_or_else(|| anyhow!("No terminal pane found in new tab {}", tab_id))?;

        Ok(format!("terminal_{}", pane.id))
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        // Try to find the tab by name and close it by ID (zellij PR #4695)
        let tabs = self.list_tabs()?;
        if let Some(tab) = tabs.iter().find(|t| t.name == full_name) {
            let tab_id = tab.tab_id().to_string();
            self.command()
                .args(&["action", "close-tab-by-id", &tab_id])
                .run()
                .context("Failed to close zellij tab by ID")?;
        } else {
            // Fallback to old method if tab not found
            warn!("Tab '{}' not found, using fallback close method", full_name);
            self.command()
                .args(&["action", "go-to-tab-name", full_name])
                .run()
                .context("Failed to switch to tab for closing")?;

            self.command()
                .args(&["action", "close-tab"])
                .run()
                .context("Failed to close zellij tab")?;
        }
        Ok(())
    }

    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        // Try to find the tab ID for more reliable closing (zellij PR #4695)
        let tabs = self.list_tabs()?;
        let tab_id = tabs
            .iter()
            .find(|t| t.name == full_name)
            .map(|t| t.tab_id().to_string());

        let delay_secs = delay.as_secs();

        let zellij = self.shell_command();
        let cmd = if let Some(id) = tab_id {
            // Use ID-based close (no need to focus the tab first)
            format!(
                "sleep {} && {} action close-tab-by-id {}",
                delay_secs, zellij, id
            )
        } else {
            // Fallback to name-based close
            format!(
                "sleep {} && {} action go-to-tab-name '{}' && {} action close-tab",
                delay_secs,
                zellij,
                full_name.replace('\'', "'\\''"),
                zellij
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
        let tabs = self.list_tabs()?;
        if let Some(tab) = tabs.iter().find(|t| t.name == full_name) {
            let tab_id = tab.tab_id().to_string();
            self.command()
                .args(&["action", "go-to-tab-by-id", &tab_id])
                .run()
                .context("Failed to select zellij tab by ID")?;
        } else {
            // Fallback to old method
            warn!(
                "Tab '{}' not found, using fallback select method",
                full_name
            );
            self.command()
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
        if !self.is_inside_session() {
            return Ok(false);
        }

        let tabs = self.list_tabs()?;
        Ok(tabs.iter().any(|t| t.name == full_name))
    }

    fn current_window_name(&self) -> Result<Option<String>> {
        Ok(self
            .focused_tab_name()
            .map(|name| canonical_tab_name(&name).to_string()))
    }

    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        if !self.is_inside_session() {
            return Ok(HashSet::new());
        }

        // Use list_tabs() for richer metadata and better efficiency
        let tabs = self.list_tabs()?;
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
        if self.focus_pane_by_id(pane_id).is_ok() {
            return Ok(());
        }

        let target_id =
            parse_pane_id(pane_id).ok_or_else(|| anyhow!("Invalid pane_id: {}", pane_id))?;
        let panes = self.list_panes()?;
        let (tab_id, steps) = pane_navigation(&panes, target_id)?;
        self.go_to_tab_by_id(tab_id)?;

        let (action, count) = if steps < 0 {
            ("focus-previous-pane", steps.unsigned_abs())
        } else {
            ("focus-next-pane", steps as usize)
        };
        debug!(pane_id, tab_id, steps, "Navigating to focused pane");
        for _ in 0..count {
            self.command()
                .args(&["action", action])
                .run()
                .with_context(|| format!("Failed to navigate to pane {}", pane_id))?;
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
        let tabs = self.list_tabs()?;
        if let Some(tab) = tabs.iter().find(|t| t.name == tab_name) {
            let tab_id = tab.tab_id().to_string();
            self.command()
                .args(&["action", "go-to-tab-by-id", &tab_id])
                .run()
                .with_context(|| format!("Failed to switch to tab '{}' by ID", tab_name))?;
        } else {
            // Fallback to name-based switch
            self.command()
                .args(&["action", "go-to-tab-name", tab_name])
                .run()
                .with_context(|| format!("Failed to switch to tab '{}'", tab_name))?;
        }

        Ok(())
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        let numeric_id =
            parse_pane_id(pane_id).ok_or_else(|| anyhow!("Invalid pane_id format: {}", pane_id))?;
        let panes = self
            .list_panes()
            .context("Failed to list panes in kill_pane")?;
        let tab_id = panes
            .iter()
            .find(|p| p.id == numeric_id && !p.is_plugin)
            .and_then(|p| p.tab_id)
            .ok_or_else(|| anyhow!("Pane {} not found or tab_id unavailable", pane_id))?;
        self.command()
            .args(&["action", "close-tab-by-id", &tab_id.to_string()])
            .run()?;
        Ok(())
    }

    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        debug!(pane_id, "respawn_pane: starting");

        // Verify the pane exists - if list-panes returns it, it's ready for --pane-id targeting
        let panes = self
            .list_panes()
            .context("Failed to list panes in respawn_pane")?;
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

        self.command()
            .args(&["action", "write-chars", "--pane-id", pane_id, &combined])
            .run()?;
        self.command()
            .args(&["action", "write", "--pane-id", pane_id, "13"])
            .run()?;

        debug!(pane_id, "respawn_pane: completed");
        Ok(pane_id.to_string())
    }

    fn set_pane_name(&self, pane_id: &str, name: &str) -> Result<()> {
        self.command()
            .args(&["action", "rename-pane", "--pane-id", pane_id, name])
            .run()
            .with_context(|| format!("Failed to rename zellij pane '{}'", pane_id))?;
        Ok(())
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

        if self
            .command()
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
        self.command()
            .args(&["action", "write-chars", "--pane-id", pane_id, text])
            .run()
            .context("Failed to send text to pane")
            .map(|_| ())
    }

    fn send_enter(&self, pane_id: &str) -> Result<()> {
        self.command()
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
                self.command()
                    .args(&["action", "write-chars", "--pane-id", pane_id, key])
                    .run()
                    .context("Failed to send key")?;
                return Ok(());
            }
        };

        self.command()
            .args(&["action", "write", "--pane-id", pane_id, code])
            .run()
            .context("Failed to send key")?;
        Ok(())
    }

    fn paste_text(&self, pane_id: &str, content: &str) -> Result<()> {
        self.command()
            .args(&["action", "write-chars", "--pane-id", pane_id, content])
            .run()?;
        Ok(())
    }

    fn paste_multiline(&self, pane_id: &str, content: &str) -> Result<()> {
        // Send line by line with pane targeting
        for line in content.lines() {
            self.command()
                .args(&["action", "write-chars", "--pane-id", pane_id, line])
                .run()?;
            self.command()
                .args(&["action", "write", "--pane-id", pane_id, "13"])
                .run()?;
        }
        Ok(())
    }

    fn clear_pane(&self, pane_id: &str) -> Result<()> {
        // Clear the pane to hide handshake setup commands
        // Try with --pane-id first, fall back to focused pane if not supported
        let result = self
            .command()
            .args(&["action", "clear", "--pane-id", pane_id])
            .run();

        if result.is_err() {
            // Fallback for older zellij versions without --pane-id support for clear
            self.command()
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

    fn set_status(&self, pane_id: &str, icon: &str, _auto_clear_on_focus: bool) -> Result<()> {
        let Some((tab_id, current_name)) = self.tab_for_pane(pane_id)? else {
            warn!(pane_id, "Cannot display status: zellij pane has no tab");
            return Ok(());
        };
        let status_name = tab_name_with_status(&current_name, icon);
        self.rename_tab_by_id(tab_id, &status_name)?;
        Ok(())
    }

    fn clear_status(&self, pane_id: &str) -> Result<()> {
        let Some((tab_id, current_name)) = self.tab_for_pane(pane_id)? else {
            return Ok(());
        };
        if let Some(base_name) = tab_name_without_status(&current_name) {
            self.rename_tab_by_id(tab_id, base_name)?;
        }
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
        let target_tab_id = self.tab_id_for_pane(target_pane_id)?;
        if let Some(tab_id) = target_tab_id {
            self.go_to_tab_by_id(tab_id)?;
        }

        if let Err(err) = self.focus_pane_by_id(target_pane_id) {
            debug!(
                target_pane_id,
                error = %err,
                "split_pane: failed to focus target pane, falling back to zellij's current focus"
            );
        }

        let panes_before = self
            .list_panes()
            .context("Failed to list zellij panes before split")?;
        let before_ids = terminal_pane_ids_in_tab(&panes_before, target_tab_id);

        let cwd_str = cwd
            .to_str()
            .ok_or_else(|| anyhow!("Path contains non-UTF8 characters"))?;

        let mut cmd = self
            .command()
            .args(&["action", "new-pane"])
            .args(zellij_new_pane_direction_args(direction))
            .args(&["--cwd", cwd_str]);

        // Pass command inline via -- syntax (runs as `sh -c 'script'`)
        if let Some(script) = command {
            cmd = cmd.args(&["--", "sh", "-c", script]);
        }

        // new-pane normally returns pane ID on stdout (e.g., "terminal_5").
        // Zellij 0.44.3 can return empty stdout when a command is supplied
        // after `--`, so fall back to detecting the new terminal pane.
        let pane_stdout = cmd
            .run_and_capture_stdout()
            .context("Failed to split pane")?;

        if let Some(pane_id) = normalize_terminal_pane_id(&pane_stdout) {
            return Ok(pane_id);
        }

        debug!(
            stdout = %pane_stdout,
            "split_pane: new-pane did not return a terminal pane id; checking list-panes"
        );

        for _ in 0..5 {
            let panes_after = self
                .list_panes()
                .context("Failed to list zellij panes after split")?;
            if let Some(pane_id) =
                find_created_terminal_pane_id(&before_ids, &panes_after, target_tab_id, command)
            {
                return Ok(pane_id);
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        Err(anyhow!(
            "Zellij new-pane did not return a pane ID and no new terminal pane appeared"
        ))
    }

    // === State Reconciliation ===

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        let panes = self.list_panes()?;

        // Extract numeric ID from "terminal_X"
        let numeric_id: u32 =
            parse_pane_id(pane_id).ok_or_else(|| anyhow!("Invalid pane_id: {}", pane_id))?;

        // Find pane by ID
        let pane = match panes.iter().find(|p| p.id == numeric_id && !p.is_plugin) {
            Some(p) => p,
            None => return Ok(None), // Pane doesn't exist
        };

        Ok(Some(self.build_live_pane_info(pane)))
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
        let panes = self.list_panes()?;

        for pane in panes {
            // Skip plugin panes, only include terminal panes
            if pane.is_plugin {
                continue;
            }

            let pane_id = format!("terminal_{}", pane.id);
            result.insert(pane_id, self.build_live_pane_info(&pane));
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pane(id: u32, is_plugin: bool, tab_id: Option<u32>) -> PaneInfo {
        PaneInfo {
            id,
            is_plugin,
            is_focused: false,
            terminal_command: None,
            pane_command: None,
            pane_cwd: None,
            tab_id,
            tab_name: String::new(),
            title: String::new(),
        }
    }

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

    #[test]
    fn bound_session_is_the_backend_instance() {
        let backend = ZellijBackend::for_session("dev session");
        assert_eq!(backend.instance_id(), "dev session");
        assert_eq!(backend.shell_command(), "zellij --session 'dev session'");
        assert!(backend.is_inside_session());
    }

    // === normalize_terminal_pane_id ===

    #[test]
    fn normalize_terminal_pane_id_accepts_terminal_id() {
        assert_eq!(
            normalize_terminal_pane_id("terminal_42\n"),
            Some("terminal_42".to_string())
        );
    }

    #[test]
    fn normalize_terminal_pane_id_accepts_bare_numeric_id() {
        assert_eq!(
            normalize_terminal_pane_id("42"),
            Some("terminal_42".to_string())
        );
    }

    #[test]
    fn normalize_terminal_pane_id_rejects_empty_or_plugin_ids() {
        assert_eq!(normalize_terminal_pane_id(""), None);
        assert_eq!(normalize_terminal_pane_id("plugin_42"), None);
    }

    #[test]
    fn pane_navigation_uses_target_tab_focus() {
        let mut other_tab_focus = test_pane(1, false, Some(3));
        other_tab_focus.is_focused = true;
        let mut target_tab_focus = test_pane(10, false, Some(7));
        target_tab_focus.is_focused = true;
        let target = test_pane(11, false, Some(7));

        assert_eq!(
            pane_navigation(&[other_tab_focus, target_tab_focus, target], 11).unwrap(),
            (7, 1)
        );
    }

    // === find_created_terminal_pane_id ===

    #[test]
    fn find_created_terminal_pane_id_finds_new_terminal_in_target_tab() {
        let before_ids = HashSet::from([1, 2]);
        let panes = vec![
            test_pane(1, false, Some(7)),
            test_pane(2, false, Some(7)),
            test_pane(3, false, Some(7)),
            test_pane(4, false, Some(8)),
            test_pane(5, true, Some(7)),
        ];

        assert_eq!(
            find_created_terminal_pane_id(&before_ids, &panes, Some(7), None),
            Some("terminal_3".to_string())
        );
    }

    #[test]
    fn find_created_terminal_pane_id_matches_command_with_higher_id_intruder() {
        let before_ids = HashSet::from([1]);
        let mut created = test_pane(2, false, Some(7));
        created.terminal_command =
            Some("sh -c echo ready > /tmp/workmux_pipe_1; exec '/bin/sh' -l".to_string());
        let mut intruder = test_pane(3, false, Some(7));
        intruder.terminal_command = Some("sh -c exec unrelated-command".to_string());

        assert_eq!(
            find_created_terminal_pane_id(
                &before_ids,
                &[created, intruder],
                Some(7),
                Some("echo ready > /tmp/workmux_pipe_1; exec '/bin/sh' -l"),
            ),
            Some("terminal_2".to_string())
        );
    }

    #[test]
    fn find_created_terminal_pane_id_rejects_ambiguous_new_panes() {
        let before_ids = HashSet::from([1]);
        let panes = vec![test_pane(2, false, Some(7)), test_pane(3, false, Some(7))];

        assert_eq!(
            find_created_terminal_pane_id(&before_ids, &panes, Some(7), None),
            None
        );
    }

    #[test]
    fn find_created_terminal_pane_id_returns_none_without_new_terminal() {
        let before_ids = HashSet::from([1, 2]);
        let panes = vec![
            test_pane(1, false, Some(7)),
            test_pane(2, false, Some(7)),
            test_pane(3, true, Some(7)),
        ];

        assert_eq!(
            find_created_terminal_pane_id(&before_ids, &panes, Some(7), None),
            None
        );
    }

    #[test]
    fn query_json_with_retry_accepts_eventual_json() {
        let mut outputs = ["", "[]"].into_iter();
        let parsed: Vec<PaneInfo> = query_json_with_retry(
            || Ok(outputs.next().unwrap().to_string()),
            "parse panes",
            Duration::ZERO,
        )
        .unwrap();

        assert!(parsed.is_empty());
    }

    #[test]
    fn query_json_with_retry_reports_final_parse_error() {
        let error = query_json_with_retry::<Vec<PaneInfo>, _>(
            || Ok(String::new()),
            "parse panes",
            Duration::ZERO,
        )
        .unwrap_err();

        assert!(error.to_string().contains("parse panes"));
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

    #[test]
    fn status_tab_name_is_replaced_without_accumulating_icons() {
        let working = tab_name_with_status("wm-feature", "🤖");
        let waiting = tab_name_with_status(&working, "💬");

        assert_eq!(tab_name_without_status(&waiting), Some("wm-feature"));
        assert!(waiting.contains("💬"));
        assert!(!waiting.contains("🤖"));
    }

    #[test]
    fn status_tab_name_is_hidden_from_canonical_identity() {
        let status_name = tab_name_with_status("wm-feature", "🤖");

        assert_eq!(canonical_tab_name(&status_name), "wm-feature");
        assert_eq!(canonical_tab_name("wm-feature ✅"), "wm-feature ✅");
    }

    #[test]
    fn status_tab_name_strips_tmux_styles() {
        let name = tab_name_with_status("wm-feature", "#[fg=#a6e3a1]✅#[fg=default]");

        assert!(name.contains("✅"));
        assert!(!name.contains("#["));
        assert_eq!(tab_name_without_status(&name), Some("wm-feature"));
    }

    #[test]
    fn unmarked_tab_name_is_not_owned_by_workmux_status() {
        assert_eq!(tab_name_without_status("wm-feature ✅"), None);
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
