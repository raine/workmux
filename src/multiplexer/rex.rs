//! Rex backend implementation for the Multiplexer trait.
//!
//! Rex (https://www.superlogical.com/rex) is driven entirely through its `rex`
//! CLI. Rex sessions map to tmux sessions, Rex windows (tabs) to tmux windows,
//! and Rex blocks to tmux panes. Pane IDs are Rex block IDs (`block:<uuid>`).
//!
//! Relies on the REX_SERVER, REX_SESSION and REX_BLOCK environment variables
//! that Rex sets in every terminal it starts.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cmd::Cmd;
use crate::config::SplitDirection;

use super::Multiplexer;
use super::types::*;
use super::util;

const SHELL_FLAVOR: &str = "com.superlogical.terminal.shell";

/// Lists every session and pane on the server in one `rex do` call.
///
/// A script only sees sessions it is attached to, so it attaches to each one.
/// Attaching subscribes the script's own client to the session's events and
/// returns the session's view; the subscription ends when the script exits.
/// Terminal blocks add their foreground process and title.
const SNAPSHOT_SCRIPT: &str = r#"
local terminal = "com.superlogical.terminal"
local sessions, panes = {}, {}
for _, s in ipairs(rex.call("session.list").sessions or {}) do
  sessions[#sessions + 1] = { session_id = s.session_id, label = s.label or "" }
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
          panes[#panes + 1] = pane
        end
      end
    end
  end
end
return { sessions = sessions, panes = panes }
"#;

/// Whether this process runs in a Rex terminal (`$REX_BLOCK` is set and not empty).
pub fn in_rex_terminal() -> bool {
    is_rex_block(std::env::var("REX_BLOCK").ok().as_deref())
}

fn is_rex_block(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

/// A session as reported by the snapshot script.
#[derive(Debug, Clone, Deserialize)]
struct RexSession {
    session_id: String,
    label: String,
}

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

    /// Arguments that address this pane's window in the server API.
    fn window_ref(&self) -> Value {
        json!({ "session_id": self.session_id, "window_id": self.window_id })
    }

    /// Arguments that address this pane in the server API.
    fn block_ref(&self) -> Value {
        json!({ "session_id": self.session_id, "block_id": self.block_id })
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

/// Everything the snapshot script reports.
#[derive(Debug, Default, Deserialize)]
struct RexSnapshot {
    #[serde(default, deserialize_with = "lua_list")]
    sessions: Vec<RexSession>,
    #[serde(default, deserialize_with = "lua_list")]
    panes: Vec<RexPane>,
}

/// A Lua list. Rex prints an empty Lua table as `{}`, not `[]`.
fn lua_list<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum LuaList<T> {
        List(Vec<T>),
        Empty(serde::de::IgnoredAny),
    }
    Ok(match LuaList::deserialize(deserializer)? {
        LuaList::List(items) => items,
        LuaList::Empty(_) => Vec::new(),
    })
}

impl RexSnapshot {
    fn parse(output: &str) -> Result<Self> {
        let trimmed = output.trim();
        if trimmed.is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(trimmed).context("Failed to parse the Rex pane list")
    }

    fn pane(&self, pane_id: &str) -> Option<&RexPane> {
        self.panes.iter().find(|p| p.block_id == pane_id)
    }

    fn require_pane(&self, pane_id: &str) -> Result<&RexPane> {
        self.pane(pane_id)
            .ok_or_else(|| anyhow!("Pane {} not found", pane_id))
    }

    /// The session with this label. Rex labels need not be unique, so the
    /// current session wins, then the first one in server order.
    fn session_id(&self, label: &str) -> Option<String> {
        self.session_id_from(label, RexBackend::current_session_id().as_deref())
    }

    fn session_id_from(&self, label: &str, current: Option<&str>) -> Option<String> {
        let matching: Vec<&RexSession> =
            self.sessions.iter().filter(|s| s.label == label).collect();
        matching
            .iter()
            .find(|s| Some(s.session_id.as_str()) == current)
            .or(matching.first())
            .map(|s| s.session_id.clone())
    }

    fn require_session_id(&self, label: &str) -> Result<String> {
        self.session_id(label)
            .ok_or_else(|| anyhow!("Session '{}' not found", label))
    }

    fn session_label(&self, session_id: &str) -> Option<&str> {
        self.sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .map(|s| s.label.as_str())
    }

    /// Tiled panes of the window with this label, preferring the current session.
    fn window_panes(&self, full_name: &str) -> Vec<&RexPane> {
        let named: Vec<&RexPane> = self
            .panes
            .iter()
            .filter(|p| !p.floating && p.window == full_name)
            .collect();
        match RexBackend::current_session_id() {
            Some(current) if named.iter().any(|p| p.session_id == current) => named
                .into_iter()
                .filter(|p| p.session_id == current)
                .collect(),
            _ => named,
        }
    }

    fn require_window(&self, full_name: &str) -> Result<&RexPane> {
        self.window_panes(full_name)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Window '{}' not found", full_name))
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

impl RexClient {
    fn is_app(&self) -> bool {
        self.info.as_ref().and_then(|i| i.kind.as_deref()) == Some("app")
    }

    fn has_session(&self, session_id: &str) -> bool {
        self.session_ids
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| id == session_id))
    }
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

    fn current_session_id() -> Option<String> {
        std::env::var("REX_SESSION")
            .ok()
            .filter(|id| !id.trim().is_empty())
    }

    fn is_current_session(session_id: &str) -> bool {
        Self::current_session_id().as_deref() == Some(session_id)
    }

    /// Call a server API method and return its JSON result.
    fn call_api(&self, method: &str, args: Value) -> Result<Value> {
        self.call_api_in(None, method, args)
    }

    /// Call a server API method scoped to a session. Methods without a
    /// `session_id` argument, like `block.close`, act on the CLI's current
    /// session unless one is given here.
    fn call_api_in(&self, session_id: Option<&str>, method: &str, args: Value) -> Result<Value> {
        let args = args.to_string();
        let mut cmd_args = vec!["api", "call"];
        if let Some(session_id) = session_id {
            cmd_args.extend(["-s", session_id]);
        }
        cmd_args.extend([method, args.as_str()]);
        let output = self
            .rex_cmd()
            .args(&cmd_args)
            .run_and_capture_stdout()
            .with_context(|| format!("Failed to call Rex API method {method}"))?;
        let trimmed = output.trim();
        if trimmed.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(trimmed)
            .with_context(|| format!("Failed to parse the result of Rex API method {method}"))
    }

    fn snapshot(&self) -> Result<RexSnapshot> {
        let output = self
            .rex_cmd()
            .args(&["do", "-e", SNAPSHOT_SCRIPT])
            .run_and_capture_stdout()
            .context("Failed to list Rex panes")?;
        RexSnapshot::parse(&output)
    }

    /// A one-block layout running a shell in `cwd`, or `command` through
    /// `sh -c` so quoting behaves like the other backends.
    fn shell_layout(cwd: &Path, command: Option<&str>) -> Value {
        let mut options = json!({ "cwd": cwd.to_string_lossy() });
        if let Some(command) = command {
            options["command"] = json!(["sh", "-c", command]);
            options["shell"] = json!("none");
        }
        json!({ "block": { "flavor": SHELL_FLAVOR, "options": options } })
    }

    fn first_block_id(result: &Value) -> Result<String> {
        result
            .pointer("/block_ids/0")
            .or_else(|| result.pointer("/initial_windows/0/block_ids/0"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("Rex response did not include a block ID"))
    }

    /// The Rex app client to ask for session switches.
    fn app_client_id(&self) -> Result<String> {
        let output = self
            .rex_cmd()
            .args(&["client", "ls", "--json"])
            .run_and_capture_stdout()
            .context("Failed to list Rex clients")?;
        let list: RexClientList =
            serde_json::from_str(&output).context("Failed to parse the Rex client list")?;
        let apps: Vec<&RexClient> = list.clients.iter().filter(|c| c.is_app()).collect();
        let current = Self::current_session_id();
        apps.iter()
            .find(|c| current.as_deref().is_some_and(|s| c.has_session(s)))
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
    ///
    /// Switching the visible session is a client action, which Rex only
    /// accepts from the CLI with Remote Control turned on. Without it the
    /// work is already done, so say where it is instead of failing.
    fn show_session(&self, session_id: &str, label: &str, window_id: Option<&str>) {
        let shown = self.app_client_id().and_then(|client| {
            let args = Self::select_session_args(session_id, window_id);
            let mut cmd_args = vec!["-C", client.as_str()];
            cmd_args.extend(args.iter().map(String::as_str));
            self.rex_cmd()
                .args(&cmd_args)
                .run()
                .context("Failed to switch the Rex app to the session")
                .map(|_| ())
        });
        if let Err(error) = shown {
            eprintln!(
                "workmux: open the Rex session '{label}' to see it \
                 (switching needs Remote Control on in Rex Settings: {error:#})"
            );
        }
    }

    fn shell_show_session_cmd(&self, session_id: &str, window_id: Option<&str>) -> Result<String> {
        let client = self.app_client_id()?;
        let args = Self::select_session_args(session_id, window_id).join(" ");
        Ok(format!("rex -C {client} {args} >/dev/null 2>&1"))
    }

    fn close_window_shell_cmd(pane: &RexPane) -> String {
        format!(
            "rex api call session.close_window '{}' >/dev/null 2>&1",
            pane.window_ref()
        )
    }

    fn focus_block(&self, pane: &RexPane) -> Result<()> {
        self.call_api("session.focus_block", pane.block_ref())
            .context("Failed to focus Rex pane")?;
        Ok(())
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
        let snapshot = self.snapshot()?;
        let target = snapshot.require_pane(target_pane_id)?;

        let mut args = json!({
            "session_id": target.session_id,
            "anchor_id": target.block_id,
            "direction": direction,
            "side": "after",
            "layout": Self::shell_layout(cwd, command),
        });
        // `percentage` is the new pane's share; Rex's ratio is the anchor's share
        if let Some(p) = percentage.filter(|p| (1..100).contains(p)) {
            args["ratio"] = json!(f64::from(100 - p) / 100.0);
        }
        let result = self
            .call_api("session.new_split", args)
            .context("Failed to split Rex pane")?;
        Self::first_block_id(&result)
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
        let snapshot = self.snapshot()?;
        let pane = snapshot.require_pane(pane_id)?;
        // `--` so text starting with "-" isn't read as a flag
        self.rex_cmd()
            .args(&[
                "send",
                "-s",
                &pane.session_id,
                "-b",
                &pane.block_id,
                "--",
                text,
            ])
            .run()
            .context("Failed to send text to pane")?;
        Ok(())
    }

    fn set_window_label(&self, pane: &RexPane, label: &str) -> Result<()> {
        let mut args = pane.window_ref();
        args["label"] = json!(label);
        self.call_api("session.set_window_label", args)
            .context("Failed to rename Rex window")?;
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
        std::env::var("REX_BLOCK")
            .ok()
            .filter(|id| is_rex_block(Some(id)))
    }

    fn active_pane_id(&self) -> Option<String> {
        let snapshot = self.snapshot().ok()?;
        let current = Self::current_session_id();
        snapshot
            .panes
            .iter()
            .filter(|p| p.active)
            .find(|p| current.as_ref().is_none_or(|s| &p.session_id == s))
            .map(|p| p.block_id.clone())
    }

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        let pane_id = self
            .current_pane_id()
            .ok_or_else(|| anyhow!("REX_BLOCK not set"))?;
        let path = self.snapshot()?.require_pane(&pane_id)?.cwd_path();
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
        let result = self
            .call_api(
                "session.new_window",
                json!({
                    "session_id": session_id,
                    "window_label": full_name,
                    "focus": false,
                    "layout": Self::shell_layout(params.cwd, None),
                }),
            )
            .context("Failed to create Rex window")?;
        Self::first_block_id(&result)
    }

    fn create_session(&self, params: CreateSessionParams) -> Result<String> {
        let full_name = util::prefixed(params.prefix, params.name);
        let mut window = json!({ "layout": Self::shell_layout(params.cwd, None) });
        if let Some(name) = params.initial_window_name {
            window["window_label"] = json!(name);
        }
        let result = self
            .call_api(
                "session.create",
                json!({ "label": full_name, "initial_windows": [window] }),
            )
            .context("Failed to create Rex session")?;
        Self::first_block_id(&result)
    }

    fn create_window_in_session(&self, params: CreateWindowInSessionParams) -> Result<String> {
        let session_id = self.snapshot()?.require_session_id(params.session_name)?;
        let mut args = json!({
            "session_id": session_id,
            "focus": false,
            "layout": Self::shell_layout(params.cwd, None),
        });
        if let Some(name) = params.name {
            args["window_label"] = json!(name);
        }
        let result = self
            .call_api("session.new_window", args)
            .context("Failed to create Rex window")?;
        Self::first_block_id(&result)
    }

    fn switch_to_session(&self, prefix: &str, name: &str) -> Result<()> {
        let full_name = util::prefixed(prefix, name);
        let session_id = self.snapshot()?.require_session_id(&full_name)?;
        self.show_session(&session_id, &full_name, None);
        Ok(())
    }

    fn session_exists(&self, full_name: &str) -> Result<bool> {
        Ok(self.snapshot()?.session_id(full_name).is_some())
    }

    fn kill_session(&self, full_name: &str) -> Result<()> {
        if let Some(session_id) = self.snapshot()?.session_id(full_name) {
            self.rex_cmd()
                .args(&["kill", &session_id])
                .run()
                .context("Failed to kill Rex session")?;
        }
        Ok(())
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        let snapshot = self.snapshot()?;
        if let Some(pane) = snapshot.window_panes(full_name).first() {
            self.call_api("session.close_window", pane.window_ref())
                .context("Failed to close Rex window")?;
        }
        Ok(())
    }

    fn rename_window(&self, old_full_name: &str, new_full_name: &str) -> Result<()> {
        let snapshot = self.snapshot()?;
        self.set_window_label(snapshot.require_window(old_full_name)?, new_full_name)
    }

    fn rename_window_at_pane(&self, pane_id: &str, new_name: &str) -> Result<()> {
        let snapshot = self.snapshot()?;
        self.set_window_label(snapshot.require_pane(pane_id)?, new_name)
    }

    fn rename_session(&self, old_full_name: &str, new_full_name: &str) -> Result<()> {
        let session_id = self.snapshot()?.require_session_id(old_full_name)?;
        self.call_api(
            "session.set_label",
            json!({ "session_id": session_id, "label": new_full_name }),
        )
        .context("Failed to rename Rex session")?;
        Ok(())
    }

    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        let snapshot = self.snapshot()?;
        let Some(pane) = snapshot.window_panes(full_name).first().copied() else {
            return Ok(());
        };
        let script = format!(
            "sleep {}; {}",
            delay.as_secs_f64(),
            Self::close_window_shell_cmd(pane)
        );
        util::run_detached_sh_c(&script)
    }

    fn schedule_session_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        let Some(session_id) = self.snapshot()?.session_id(full_name) else {
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
        let snapshot = self.snapshot()?;
        let pane = snapshot.require_window(full_name)?;
        Ok(format!(
            "rex api call session.focus_window '{}' >/dev/null 2>&1",
            pane.window_ref()
        ))
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        let snapshot = self.snapshot()?;
        Ok(Self::close_window_shell_cmd(
            snapshot.require_window(full_name)?,
        ))
    }

    fn shell_switch_session_cmd(&self, full_name: &str) -> Result<String> {
        let session_id = self.snapshot()?.require_session_id(full_name)?;
        self.shell_show_session_cmd(&session_id, None)
    }

    fn shell_kill_session_cmd(&self, full_name: &str) -> Result<String> {
        let session_id = self.snapshot()?.require_session_id(full_name)?;
        Ok(format!("rex kill {session_id} >/dev/null 2>&1"))
    }

    fn select_window(&self, prefix: &str, name: &str) -> Result<()> {
        let full_name = util::prefixed(prefix, name);
        let snapshot = self.snapshot()?;
        let pane = snapshot.require_window(&full_name)?;
        if !Self::is_current_session(&pane.session_id) {
            let label = snapshot.session_label(&pane.session_id).unwrap_or_default();
            self.show_session(&pane.session_id, label, Some(&pane.window_id));
        }
        self.call_api("session.focus_window", pane.window_ref())
            .context("Failed to focus Rex window")?;
        Ok(())
    }

    fn current_window_name(&self) -> Result<Option<String>> {
        let Some(pane_id) = self.current_pane_id() else {
            return Ok(None);
        };
        Ok(self.snapshot()?.pane(&pane_id).map(|p| p.window.clone()))
    }

    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        let current = Self::current_session_id();
        Ok(self
            .snapshot()?
            .panes
            .iter()
            .filter(|p| current.as_ref().is_none_or(|s| &p.session_id == s))
            .map(|p| p.window.clone())
            .collect())
    }

    fn get_window_names_in_session(&self, session_name: &str) -> Result<HashSet<String>> {
        Ok(self
            .snapshot()?
            .panes
            .iter()
            .filter(|p| p.session == session_name)
            .map(|p| p.window.clone())
            .collect())
    }

    fn get_all_windows_with_sessions(&self) -> Result<HashSet<(String, String)>> {
        Ok(self
            .snapshot()?
            .panes
            .iter()
            .map(|p| (p.window.clone(), p.session.clone()))
            .collect())
    }

    fn get_all_session_names(&self) -> Result<HashSet<String>> {
        Ok(self
            .snapshot()?
            .sessions
            .into_iter()
            .map(|s| s.label)
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
        self.focus_block(self.snapshot()?.require_pane(pane_id)?)
    }

    fn zoom_pane(&self, pane_id: &str) -> Result<()> {
        let snapshot = self.snapshot()?;
        let pane = snapshot.require_pane(pane_id)?;
        self.rex_cmd()
            .args(&["zoom", "-s", &pane.session_id, "-b", &pane.block_id, "--on"])
            .run()
            .context("Failed to zoom Rex pane")?;
        Ok(())
    }

    fn switch_to_pane(&self, pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
        let snapshot = self.snapshot()?;
        let pane = snapshot.require_pane(pane_id)?;
        if !Self::is_current_session(&pane.session_id) {
            let label = snapshot.session_label(&pane.session_id).unwrap_or_default();
            self.show_session(&pane.session_id, label, Some(&pane.window_id));
        }
        self.focus_block(pane)
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        let snapshot = self.snapshot()?;
        let pane = snapshot.require_pane(pane_id)?;
        self.call_api_in(
            Some(&pane.session_id),
            "block.close",
            json!({ "block_id": pane.block_id }),
        )
        .context("Failed to close Rex pane")?;
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
        let snapshot = self.snapshot().ok()?;
        let pane = snapshot.pane(pane_id)?;
        // --trim drops the blank rows under the last line of output
        let output = self
            .rex_cmd()
            .args(&[
                "capture",
                "-s",
                &pane.session_id,
                "-b",
                &pane.block_id,
                "--trim",
            ])
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
        let snapshot = self.snapshot()?;
        let pane = snapshot.require_pane(pane_id)?;
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
        self.snapshot()
            .ok()?
            .session_label(&current)
            .map(str::to_string)
    }

    // === State Reconciliation ===

    fn instance_id(&self) -> String {
        self.resolve_instance_id()
            .unwrap_or_else(|_| "default".to_string())
    }

    fn resolve_instance_id(&self) -> Result<String> {
        // REX_SERVER names the server socket, so every session on one server
        // shares an instance, matching tmux server scope.
        std::env::var("REX_SERVER")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("REX_SERVER is required to resolve the Rex instance"))
    }

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        Ok(self.snapshot()?.pane(pane_id).map(|p| p.live_pane_info().1))
    }

    fn get_all_window_names_all_sessions(&self) -> Result<HashSet<String>> {
        Ok(self
            .snapshot()?
            .panes
            .iter()
            .map(|p| p.window.clone())
            .collect())
    }

    fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
        Ok(self
            .snapshot()?
            .panes
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

    fn session(id: &str, label: &str) -> RexSession {
        RexSession {
            session_id: id.to_string(),
            label: label.to_string(),
        }
    }

    #[test]
    fn parses_an_empty_snapshot() {
        // Rex prints empty Lua tables as `{}`
        for output in ["", "{}", r#"{"panes":{},"sessions":{}}"#] {
            let snapshot = RexSnapshot::parse(output).unwrap();
            assert!(snapshot.sessions.is_empty());
            assert!(snapshot.panes.is_empty());
        }
    }

    #[test]
    fn parses_a_snapshot() {
        let output = r#"{"sessions":[{"session_id":"session:1","label":"work"},
            {"session_id":"session:2","label":"empty"}],
            "panes":[{"session_id":"session:1","session":"work","window_id":"window:1",
            "window":"wm-feature","window_index":1,"block_id":"block:1","floating":false,
            "active":true,"pid":42,"command":"claude","cwd":"/tmp","title":"Feature"}]}"#;
        let snapshot = RexSnapshot::parse(output).unwrap();
        assert_eq!(snapshot.panes.len(), 1);
        assert_eq!(snapshot.panes[0].block_id, "block:1");
        assert_eq!(snapshot.panes[0].cwd_path(), PathBuf::from("/tmp"));
        // A session without panes is still known
        assert_eq!(snapshot.session_id("empty").as_deref(), Some("session:2"));
    }

    #[test]
    fn duplicate_session_labels_prefer_the_current_session() {
        let snapshot = RexSnapshot {
            sessions: vec![session("session:1", "work"), session("session:2", "work")],
            panes: Vec::new(),
        };
        assert_eq!(
            snapshot
                .session_id_from("work", Some("session:2"))
                .as_deref(),
            Some("session:2")
        );
        // Outside that session, server order decides
        assert_eq!(
            snapshot
                .session_id_from("work", Some("session:9"))
                .as_deref(),
            Some("session:1")
        );
        assert_eq!(
            snapshot.session_id_from("work", None).as_deref(),
            Some("session:1")
        );
        assert!(snapshot.session_id_from("missing", None).is_none());
    }

    #[test]
    fn empty_rex_block_is_not_a_rex_terminal() {
        assert!(is_rex_block(Some("block:1")));
        assert!(!is_rex_block(Some("")));
        assert!(!is_rex_block(Some("  ")));
        assert!(!is_rex_block(None));
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
            RexBackend::first_block_id(&json!({"window_id": "window:1", "block_ids": ["block:2"]}))
                .unwrap(),
            "block:2"
        );
        assert_eq!(
            RexBackend::first_block_id(&json!({
                "session_id": "session:1",
                "initial_windows": [{"window_id": "window:1", "block_ids": ["block:3"]}]
            }))
            .unwrap(),
            "block:3"
        );
    }

    #[test]
    fn shell_layout_runs_commands_through_sh() {
        let layout = RexBackend::shell_layout(Path::new("/tmp"), Some("echo 'hi'"));
        let options = &layout["block"]["options"];
        assert_eq!(layout["block"]["flavor"], SHELL_FLAVOR);
        assert_eq!(options["cwd"], "/tmp");
        assert_eq!(options["command"], json!(["sh", "-c", "echo 'hi'"]));
        assert_eq!(options["shell"], "none");
    }

    #[test]
    fn select_session_args_include_the_window_when_given() {
        assert_eq!(
            RexBackend::select_session_args("session:1", Some("window:2")),
            vec![
                "do",
                "session.select",
                "session_id=session:1",
                "window_id=window:2"
            ]
        );
    }
}
