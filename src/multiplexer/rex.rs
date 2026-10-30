//! Rex backend implementation for the Multiplexer trait.
//!
//! Rex (https://www.superlogical.com/rex) is driven entirely through its `rex`
//! CLI. Rex sessions map to tmux sessions, Rex windows (tabs) to tmux windows,
//! and Rex blocks to tmux panes. Pane IDs are Rex block IDs (`block:<uuid>`).
//!
//! Relies on the REX_SESSION and REX_BLOCK environment variables that Rex sets
//! in every terminal it starts.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cmd::Cmd;
use crate::config::SplitDirection;

use super::Multiplexer;
use super::types::*;
use super::util;

const SHELL_FLAVOR: &str = "com.superlogical.terminal.shell";

/// Lists every pane on the server in one `rex do` call.
///
/// A script only sees sessions it is attached to, so it attaches to each one;
/// attaching also returns the session's view. Terminal blocks add their
/// foreground process and title.
const SNAPSHOT_SCRIPT: &str = r#"
local terminal = "com.superlogical.terminal"
local out = {}
for _, s in ipairs(rex.call("session.list").sessions or {}) do
  local attached = rex.call("session.attach", { session_id = s.session_id })
  local view = type(attached) == "table" and attached.view
  if type(view) == "table" then
    for index, w in ipairs(view.windows or {}) do
      for _, layer in ipairs(w.layers or {}) do
        for _, b in ipairs(layer.blocks or {}) do
          local pane = {
            session_id = s.session_id, session = view.label or "",
            window_id = w.window_id, window = w.label or "", window_index = index,
            block_id = b.block_id, floating = layer.kind ~= "tiled",
            active = view.active_window_id == w.window_id and w.focused_block_id == b.block_id,
          }
          if b.creator_name == terminal then
            local target = { session_id = s.session_id, block_id = b.block_id }
            local ok, proc = pcall(rex.block.call, terminal, "process", target)
            if ok and type(proc) == "table" then
              local fg = proc.foreground or {}
              pane.pid = (proc.child or {}).pid
              pane.command = fg.name
              pane.cwd = fg.cwd
              if not pane.cwd then
                for _, a in ipairs(proc.ancestors or {}) do
                  if a.cwd then pane.cwd = a.cwd break end
                end
              end
            end
            local ok_title, t = pcall(rex.block.call, terminal, "title", target)
            if ok_title and type(t) == "table" then pane.title = t.title end
          end
          out[#out + 1] = pane
        end
      end
    end
  end
end
return out
"#;

/// One pane (block) as reported by the snapshot script.
#[derive(Debug, Clone, Deserialize)]
struct RexPane {
    session_id: String,
    session: String,
    window_id: String,
    window: String,
    window_index: u32,
    block_id: String,
    #[serde(default)]
    floating: bool,
    #[serde(default)]
    active: bool,
    pid: Option<u32>,
    command: Option<String>,
    cwd: Option<String>,
    title: Option<String>,
}

impl RexPane {
    fn cwd_path(&self) -> PathBuf {
        self.cwd.as_deref().map(PathBuf::from).unwrap_or_default()
    }

    /// Live pane info keyed by block ID, with Rex's stable session and window
    /// IDs and the window's position in the tab bar (1-based)
    fn live_pane_info(&self) -> (String, LivePaneInfo) {
        let (pane_id, mut info) = util::LivePaneSnapshot {
            pane_id: self.block_id.clone(),
            pid: self.pid,
            current_command: self.command.clone(),
            working_dir: self.cwd_path(),
            title: self.title.clone().unwrap_or_default(),
            session: self.session.clone(),
            window: self.window.clone(),
        }
        .into_pair();
        info.session_id = Some(self.session_id.clone());
        info.window_id = Some(self.window_id.clone());
        info.window_index = Some(self.window_index);
        (pane_id, info)
    }
}

/// A connected client as reported by `rex client ls --json`.
#[derive(Debug, Deserialize)]
struct RexClient {
    client_id: String,
    info: Option<RexClientInfo>,
    session_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RexClientInfo {
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RexClientList {
    clients: Vec<RexClient>,
}

/// Rex backend implementation.
#[derive(Debug, Default)]
pub struct RexBackend;

impl RexBackend {
    pub fn new() -> Self {
        Self
    }

    fn rex_cmd(&self) -> Cmd<'static> {
        Cmd::new("rex")
    }

    /// Call a server API method with JSON arguments.
    fn api(&self, method: &str, args: serde_json::Value) -> Result<String> {
        let args = args.to_string();
        self.rex_cmd()
            .args(&["api", "call", method, &args])
            .run_and_capture_stdout()
            .with_context(|| format!("rex api call {method} failed"))
    }

    /// Every pane on the server, in session, window, and layout order.
    fn list_panes(&self) -> Result<Vec<RexPane>> {
        let output = self
            .rex_cmd()
            .args(&["do", "-e", SNAPSHOT_SCRIPT])
            .run_and_capture_stdout()
            .context("Failed to list Rex panes")?;
        Self::parse_panes(&output)
    }

    fn parse_panes(output: &str) -> Result<Vec<RexPane>> {
        let trimmed = output.trim();
        // Lua's empty table is printed as an object, not an array
        if trimmed.is_empty() || trimmed == "{}" {
            return Ok(Vec::new());
        }
        serde_json::from_str(trimmed).context("Failed to parse Rex pane list")
    }

    fn current_session_id() -> Option<String> {
        std::env::var("REX_SESSION")
            .ok()
            .filter(|id| !id.is_empty())
    }

    fn find_pane<'a>(panes: &'a [RexPane], pane_id: &str) -> Option<&'a RexPane> {
        panes.iter().find(|p| p.block_id == pane_id)
    }

    /// Tiled panes of the window with this label, preferring the current session.
    fn window_panes<'a>(&self, panes: &'a [RexPane], full_name: &str) -> Vec<&'a RexPane> {
        let named: Vec<&RexPane> = panes
            .iter()
            .filter(|p| !p.floating && p.window == full_name)
            .collect();
        match Self::current_session_id() {
            Some(current) if named.iter().any(|p| p.session_id == current) => named
                .into_iter()
                .filter(|p| p.session_id == current)
                .collect(),
            _ => named,
        }
    }

    fn find_window<'a>(&self, panes: &'a [RexPane], full_name: &str) -> Result<&'a RexPane> {
        self.window_panes(panes, full_name)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Window '{}' not found", full_name))
    }

    fn find_session_id(panes: &[RexPane], full_name: &str) -> Option<String> {
        panes
            .iter()
            .find(|p| p.session == full_name)
            .map(|p| p.session_id.clone())
    }

    /// The Rex app client to ask for session switches.
    ///
    /// Switching the visible session is a client action, so it needs a
    /// connected app with Remote Control turned on.
    fn app_client_id(&self) -> Result<String> {
        let output = self
            .rex_cmd()
            .args(&["client", "ls", "--json"])
            .run_and_capture_stdout()
            .context("Failed to list Rex clients")?;
        let list: RexClientList =
            serde_json::from_str(&output).context("Failed to parse Rex client list")?;
        let apps: Vec<&RexClient> = list
            .clients
            .iter()
            .filter(|c| c.info.as_ref().and_then(|i| i.kind.as_deref()) == Some("app"))
            .collect();
        let current = Self::current_session_id();
        apps.iter()
            .find(|c| {
                current.as_ref().is_some_and(|s| {
                    c.session_ids
                        .as_ref()
                        .is_some_and(|ids| ids.iter().any(|id| id == s))
                })
            })
            .or(apps.first())
            .map(|c| c.client_id.clone())
            .ok_or_else(|| anyhow!("No Rex app is connected"))
    }

    fn select_session_args(session_id: &str, window_id: Option<&str>) -> Vec<String> {
        let mut args = vec![
            "do".to_string(),
            "session.select".to_string(),
            format!("session_id={session_id}"),
        ];
        if let Some(window_id) = window_id {
            args.push(format!("window_id={window_id}"));
        }
        args
    }

    /// Show a session (and optionally one of its windows) in the Rex app.
    fn show_session(&self, session_id: &str, window_id: Option<&str>) -> Result<()> {
        let client = self.app_client_id()?;
        let args = Self::select_session_args(session_id, window_id);
        let mut all = vec!["-C", client.as_str()];
        all.extend(args.iter().map(String::as_str));
        self.rex_cmd().args(&all).run().context(
            "Failed to switch the Rex app to the session (is Remote Control on in Rex Settings?)",
        )?;
        Ok(())
    }

    fn shell_show_session_cmd(&self, session_id: &str, window_id: Option<&str>) -> Result<String> {
        let client = self.app_client_id()?;
        let args = Self::select_session_args(session_id, window_id).join(" ");
        Ok(format!("rex -C {client} {args} >/dev/null 2>&1"))
    }

    fn focus_window(&self, pane: &RexPane) -> Result<()> {
        self.api(
            "session.focus_window",
            json!({ "session_id": pane.session_id, "window_id": pane.window_id }),
        )?;
        Ok(())
    }

    fn focus_block(&self, pane: &RexPane) -> Result<()> {
        self.api(
            "session.focus_block",
            json!({ "session_id": pane.session_id, "block_id": pane.block_id }),
        )?;
        Ok(())
    }

    fn close_window_api_cmd(pane: &RexPane) -> String {
        let args = json!({ "session_id": pane.session_id, "window_id": pane.window_id });
        format!("rex api call session.close_window '{args}' >/dev/null 2>&1")
    }

    /// Terminal options for a new block: a working directory and, optionally,
    /// a command run through `sh -c` so quoting behaves like the other backends.
    fn block_options(cwd: &Path, command: Option<&str>) -> serde_json::Value {
        let mut options = json!({ "cwd": cwd.to_string_lossy() });
        if let Some(command) = command {
            options["command"] = json!(["sh", "-c", command]);
            options["shell"] = json!("none");
        }
        options
    }

    fn first_block_id(output: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(output.trim()).context("Failed to parse Rex response")?;
        value
            .pointer("/block_ids/0")
            .or_else(|| value.pointer("/initial_windows/0/block_ids/0"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("Rex response did not include a block ID"))
    }

    fn split_pane_internal(
        &self,
        target_pane_id: &str,
        direction: &SplitDirection,
        cwd: &Path,
        percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        let direction = match direction {
            SplitDirection::Horizontal => "horizontal",
            SplitDirection::Vertical => "vertical",
            SplitDirection::Stacked => {
                return Err(anyhow!(
                    "split: stacked is only supported by the Zellij backend"
                ));
            }
        };
        let panes = self.list_panes()?;
        let target = Self::find_pane(&panes, target_pane_id)
            .ok_or_else(|| anyhow!("Pane {} not found", target_pane_id))?;

        let mut args = json!({
            "session_id": target.session_id,
            "anchor_id": target.block_id,
            "direction": direction,
            "side": "after",
            "layout": { "block": { "flavor": SHELL_FLAVOR, "options": Self::block_options(cwd, command) } },
        });
        // `percentage` is the new pane's share; Rex's ratio is the anchor's share
        if let Some(p) = percentage.filter(|p| (1..100).contains(p)) {
            args["ratio"] = json!(f64::from(100 - p) / 100.0);
        }
        let output = self.api("session.new_split", args)?;
        Self::first_block_id(&output)
    }

    /// Translate tmux key names (used across workmux) to Rex key syntax.
    fn rex_key(key: &str) -> String {
        match key {
            "Enter" => "enter".to_string(),
            "Escape" => "escape".to_string(),
            "BSpace" => "backspace".to_string(),
            "Tab" => "tab".to_string(),
            "Space" => "space".to_string(),
            "Up" => "up".to_string(),
            "Down" => "down".to_string(),
            "Left" => "left".to_string(),
            "Right" => "right".to_string(),
            "Home" => "home".to_string(),
            "End" => "end".to_string(),
            "PageUp" | "PPage" => "pageup".to_string(),
            "PageDown" | "NPage" => "pagedown".to_string(),
            "DC" => "delete".to_string(),
            other => match other.strip_prefix("C-") {
                Some(rest) => format!("ctrl+{}", rest.to_lowercase()),
                None => match other.strip_prefix("M-") {
                    Some(rest) => format!("alt+{}", rest.to_lowercase()),
                    None => other.to_string(),
                },
            },
        }
    }

    fn send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        let panes = self.list_panes()?;
        let pane = Self::find_pane(&panes, pane_id)
            .ok_or_else(|| anyhow!("Pane {} not found", pane_id))?;
        self.rex_cmd()
            .args(&["send", "-s", &pane.session_id, "-b", &pane.block_id, text])
            .run()
            .context("Failed to send text to pane")?;
        Ok(())
    }
}

impl Multiplexer for RexBackend {
    fn name(&self) -> &'static str {
        "rex"
    }

    // === Server/Session ===

    fn is_running(&self) -> Result<bool> {
        self.rex_cmd()
            .args(&["--autostart=false", "api", "call", "server.status", "{}"])
            .run_as_check()
    }

    fn current_pane_id(&self) -> Option<String> {
        std::env::var("REX_BLOCK").ok().filter(|id| !id.is_empty())
    }

    fn active_pane_id(&self) -> Option<String> {
        let panes = self.list_panes().ok()?;
        let current = Self::current_session_id();
        panes
            .iter()
            .filter(|p| p.active)
            .find(|p| current.as_ref().is_none_or(|s| &p.session_id == s))
            .map(|p| p.block_id.clone())
    }

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        let pane_id = self
            .current_pane_id()
            .ok_or_else(|| anyhow!("REX_BLOCK not set"))?;
        let panes = self.list_panes()?;
        let path = Self::find_pane(&panes, &pane_id)
            .ok_or_else(|| anyhow!("Current pane {} not found", pane_id))?
            .cwd_path();
        if path.as_os_str().is_empty() {
            return Err(anyhow!(
                "Rex did not report a directory for pane {}",
                pane_id
            ));
        }
        Ok(path)
    }

    // === Window/Tab Management ===

    fn create_window(&self, params: CreateWindowParams) -> Result<String> {
        let full_name = util::prefixed(params.prefix, params.name);
        let session_id = Self::current_session_id()
            .ok_or_else(|| anyhow!("REX_SESSION not set; run workmux inside a Rex terminal"))?;
        // Rex appends new windows at the end; params.after_window is ignored
        let output = self
            .api(
                "session.new_window",
                json!({
                    "session_id": session_id,
                    "window_label": full_name,
                    "focus": false,
                    "layout": { "block": { "flavor": SHELL_FLAVOR, "options": Self::block_options(params.cwd, None) } },
                }),
            )
            .context("Failed to create Rex window")?;
        Self::first_block_id(&output)
    }

    fn create_session(&self, params: CreateSessionParams) -> Result<String> {
        let full_name = util::prefixed(params.prefix, params.name);
        let mut window = json!({
            "layout": { "block": { "flavor": SHELL_FLAVOR, "options": Self::block_options(params.cwd, None) } },
        });
        if let Some(name) = params.initial_window_name {
            window["window_label"] = json!(name);
        }
        let output = self
            .api(
                "session.create",
                json!({ "label": full_name, "initial_windows": [window] }),
            )
            .context("Failed to create Rex session")?;
        Self::first_block_id(&output)
    }

    fn create_window_in_session(&self, params: CreateWindowInSessionParams) -> Result<String> {
        let panes = self.list_panes()?;
        let session_id = Self::find_session_id(&panes, params.session_name)
            .ok_or_else(|| anyhow!("Session '{}' not found", params.session_name))?;
        let mut args = json!({
            "session_id": session_id,
            "focus": false,
            "layout": { "block": { "flavor": SHELL_FLAVOR, "options": Self::block_options(params.cwd, None) } },
        });
        if let Some(name) = params.name {
            args["window_label"] = json!(name);
        }
        let output = self
            .api("session.new_window", args)
            .context("Failed to create Rex window")?;
        Self::first_block_id(&output)
    }

    fn switch_to_session(&self, prefix: &str, name: &str) -> Result<()> {
        let full_name = util::prefixed(prefix, name);
        let panes = self.list_panes()?;
        let session_id = Self::find_session_id(&panes, &full_name)
            .ok_or_else(|| anyhow!("Session '{}' not found", full_name))?;
        self.show_session(&session_id, None)
    }

    fn session_exists(&self, full_name: &str) -> Result<bool> {
        Ok(Self::find_session_id(&self.list_panes()?, full_name).is_some())
    }

    fn kill_session(&self, full_name: &str) -> Result<()> {
        if let Some(session_id) = Self::find_session_id(&self.list_panes()?, full_name) {
            self.rex_cmd()
                .args(&["kill", &session_id])
                .run()
                .context("Failed to kill Rex session")?;
        }
        Ok(())
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        let panes = self.list_panes()?;
        if let Some(pane) = self.window_panes(&panes, full_name).first() {
            self.api(
                "session.close_window",
                json!({ "session_id": pane.session_id, "window_id": pane.window_id }),
            )?;
        }
        Ok(())
    }

    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        let panes = self.list_panes()?;
        let Some(pane) = self.window_panes(&panes, full_name).first().copied() else {
            return Ok(());
        };
        let script = format!(
            "sleep {}; {}",
            delay.as_secs_f64(),
            Self::close_window_api_cmd(pane)
        );
        util::run_detached_sh_c(&script)
    }

    fn schedule_session_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        let Some(session_id) = Self::find_session_id(&self.list_panes()?, full_name) else {
            return Ok(());
        };
        let script = format!(
            "sleep {}; rex kill {} >/dev/null 2>&1",
            delay.as_secs_f64(),
            session_id
        );
        util::run_detached_sh_c(&script)
    }

    fn run_deferred_script(&self, script: &str) -> Result<()> {
        util::run_detached_sh_c(script)
    }

    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String> {
        let panes = self.list_panes()?;
        let pane = self.find_window(&panes, full_name)?;
        let args = json!({ "session_id": pane.session_id, "window_id": pane.window_id });
        Ok(format!(
            "rex api call session.focus_window '{args}' >/dev/null 2>&1"
        ))
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        let panes = self.list_panes()?;
        let pane = self.find_window(&panes, full_name)?;
        Ok(Self::close_window_api_cmd(pane))
    }

    fn shell_switch_session_cmd(&self, full_name: &str) -> Result<String> {
        let panes = self.list_panes()?;
        let session_id = Self::find_session_id(&panes, full_name)
            .ok_or_else(|| anyhow!("Session '{}' not found", full_name))?;
        self.shell_show_session_cmd(&session_id, None)
    }

    fn shell_kill_session_cmd(&self, full_name: &str) -> Result<String> {
        let panes = self.list_panes()?;
        let session_id = Self::find_session_id(&panes, full_name)
            .ok_or_else(|| anyhow!("Session '{}' not found", full_name))?;
        Ok(format!("rex kill {session_id} >/dev/null 2>&1"))
    }

    fn select_window(&self, prefix: &str, name: &str) -> Result<()> {
        let full_name = util::prefixed(prefix, name);
        let panes = self.list_panes()?;
        let pane = self.find_window(&panes, &full_name)?;
        if Self::current_session_id().as_deref() != Some(pane.session_id.as_str()) {
            return self.show_session(&pane.session_id, Some(&pane.window_id));
        }
        self.focus_window(pane)
    }

    fn current_window_name(&self) -> Result<Option<String>> {
        let Some(pane_id) = self.current_pane_id() else {
            return Ok(None);
        };
        let panes = self.list_panes()?;
        Ok(Self::find_pane(&panes, &pane_id).map(|p| p.window.clone()))
    }

    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        let panes = self.list_panes()?;
        let current = Self::current_session_id();
        Ok(panes
            .iter()
            .filter(|p| current.as_ref().is_none_or(|s| &p.session_id == s))
            .map(|p| p.window.clone())
            .collect())
    }

    fn get_window_names_in_session(&self, session_name: &str) -> Result<HashSet<String>> {
        Ok(self
            .list_panes()?
            .iter()
            .filter(|p| p.session == session_name)
            .map(|p| p.window.clone())
            .collect())
    }

    fn get_all_windows_with_sessions(&self) -> Result<HashSet<(String, String)>> {
        Ok(self
            .list_panes()?
            .iter()
            .map(|p| (p.window.clone(), p.session.clone()))
            .collect())
    }

    fn get_all_session_names(&self) -> Result<HashSet<String>> {
        Ok(self
            .list_panes()?
            .iter()
            .map(|p| p.session.clone())
            .collect())
    }

    fn wait_until_session_closed(&self, full_session_name: &str) -> Result<()> {
        while self.session_exists(full_session_name)? {
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    }

    // === Pane Management ===

    fn select_pane(&self, pane_id: &str) -> Result<()> {
        let panes = self.list_panes()?;
        let pane = Self::find_pane(&panes, pane_id)
            .ok_or_else(|| anyhow!("Pane {} not found", pane_id))?;
        self.focus_block(pane)
    }

    fn switch_to_pane(&self, pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
        let panes = self.list_panes()?;
        let pane = Self::find_pane(&panes, pane_id)
            .ok_or_else(|| anyhow!("Pane {} not found", pane_id))?;
        if Self::current_session_id().as_deref() != Some(pane.session_id.as_str()) {
            self.show_session(&pane.session_id, Some(&pane.window_id))?;
        }
        self.focus_block(pane)
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        self.api("block.close", json!({ "block_id": pane_id }))?;
        Ok(())
    }

    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        // Rex can't restart a block in place: split beside it, then close it.
        // The new block takes the old one's whole space once it is gone.
        let new_pane_id =
            self.split_pane_internal(pane_id, &SplitDirection::Horizontal, cwd, None, cmd)?;
        self.kill_pane(pane_id)?;
        Ok(new_pane_id)
    }

    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String> {
        let panes = self.list_panes().ok()?;
        let pane = Self::find_pane(&panes, pane_id)?;
        let output = self
            .rex_cmd()
            .args(&["capture", "-s", &pane.session_id, "-b", &pane.block_id])
            .run_and_capture_stdout()
            .ok()?;
        Some(util::tail_lines(&output, lines))
    }

    // === Text I/O ===

    fn send_text_fragment(&self, pane_id: &str, text: &str) -> Result<()> {
        self.send_text(pane_id, text)
    }

    fn send_enter(&self, pane_id: &str) -> Result<()> {
        self.send_key(pane_id, "Enter")
    }

    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        let panes = self.list_panes()?;
        let pane = Self::find_pane(&panes, pane_id)
            .ok_or_else(|| anyhow!("Pane {} not found", pane_id))?;
        let key = Self::rex_key(key);
        self.rex_cmd()
            .args(&[
                "send-key",
                "-s",
                &pane.session_id,
                "-b",
                &pane.block_id,
                &key,
            ])
            .run()
            .context("Failed to send key to pane")?;
        Ok(())
    }

    fn paste_text(&self, pane_id: &str, content: &str) -> Result<()> {
        // Bracketed paste, so shells and agents treat it as one paste
        self.send_text(pane_id, &format!("\x1b[200~{content}\x1b[201~"))
    }

    // === Status ===

    fn set_status(&self, _pane_id: &str, _icon: &str, _auto_clear_on_focus: bool) -> Result<()> {
        // Agent state lives in the StateStore. Rex shows agent status itself
        // from the program status the agents report, so no UI update is needed.
        Ok(())
    }

    fn clear_status(&self, _pane_id: &str) -> Result<()> {
        Ok(())
    }

    fn ensure_status_format(&self, _pane_id: &str) -> Result<()> {
        Ok(())
    }

    // === Multi-Session Support ===

    fn current_session(&self) -> Option<String> {
        let current = Self::current_session_id()?;
        self.list_panes()
            .ok()?
            .into_iter()
            .find(|p| p.session_id == current)
            .map(|p| p.session)
    }

    // === State Reconciliation ===

    fn instance_id(&self) -> String {
        // REX_SERVER names the server socket, so every session on one server
        // shares an instance, matching tmux server scope.
        std::env::var("REX_SERVER")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "default".to_string())
    }

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        let panes = self.list_panes()?;
        Ok(Self::find_pane(&panes, pane_id).map(|p| p.live_pane_info().1))
    }

    fn get_all_window_names_all_sessions(&self) -> Result<HashSet<String>> {
        Ok(self
            .list_panes()?
            .iter()
            .map(|p| p.window.clone())
            .collect())
    }

    fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
        Ok(self
            .list_panes()?
            .iter()
            .map(RexPane::live_pane_info)
            .collect())
    }

    fn split_pane(
        &self,
        target_pane_id: &str,
        direction: &SplitDirection,
        cwd: &Path,
        _size: Option<u16>,
        percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        self.split_pane_internal(target_pane_id, direction, cwd, percentage, command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_empty_snapshot() {
        assert!(RexBackend::parse_panes("{}").unwrap().is_empty());
        assert!(RexBackend::parse_panes("").unwrap().is_empty());
        assert!(RexBackend::parse_panes("[]").unwrap().is_empty());
    }

    #[test]
    fn parses_a_snapshot() {
        let output = r#"[{"session_id":"session:1","session":"work","window_id":"window:1",
            "window":"wm-feature","window_index":1,"block_id":"block:1","floating":false,
            "active":true,"pid":42,"command":"claude","cwd":"/tmp","title":"Feature"}]"#;
        let panes = RexBackend::parse_panes(output).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].block_id, "block:1");
        assert_eq!(panes[0].cwd_path(), PathBuf::from("/tmp"));
        assert_eq!(panes[0].window_index, 1);
    }

    #[test]
    fn translates_tmux_key_names() {
        assert_eq!(RexBackend::rex_key("Enter"), "enter");
        assert_eq!(RexBackend::rex_key("BSpace"), "backspace");
        assert_eq!(RexBackend::rex_key("C-c"), "ctrl+c");
        assert_eq!(RexBackend::rex_key("M-x"), "alt+x");
        assert_eq!(RexBackend::rex_key("q"), "q");
    }

    #[test]
    fn reads_block_ids_from_window_and_session_responses() {
        assert_eq!(
            RexBackend::first_block_id(r#"{"window_id":"window:1","block_ids":["block:2"]}"#)
                .unwrap(),
            "block:2"
        );
        assert_eq!(
            RexBackend::first_block_id(
                r#"{"session_id":"session:1","initial_windows":[{"window_id":"window:1","block_ids":["block:3"]}]}"#
            )
            .unwrap(),
            "block:3"
        );
    }

    #[test]
    fn block_options_run_commands_through_sh() {
        let options = RexBackend::block_options(Path::new("/tmp"), Some("echo 'hi'"));
        assert_eq!(options["cwd"], "/tmp");
        assert_eq!(options["command"], json!(["sh", "-c", "echo 'hi'"]));
        assert_eq!(options["shell"], "none");
    }
}
