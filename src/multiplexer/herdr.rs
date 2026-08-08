//! Herdr backend implementation for the `Multiplexer` contract.
//!
//! Herdr exposes its persistent server through the `herdr` CLI.  Commands below
//! deliberately target that socket API rather than driving the interactive UI.

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::SplitDirection;
use super::types::*;
use super::{Multiplexer, util};

#[derive(Debug, Default)]
pub struct HerdrBackend;

impl HerdrBackend {
    pub fn new() -> Self { Self }

    fn run(&self, args: &[String]) -> Result<String> {
        let output = Command::new("herdr").args(args).output()
            .with_context(|| format!("failed to execute herdr {}", args.join(" ")))?;
        if !output.status.success() {
            return Err(anyhow!("herdr {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim()));
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    fn json(&self, args: &[String]) -> Result<Value> {
        let output = self.run(args)?;
        serde_json::from_str(&output).with_context(|| format!("herdr returned non-JSON for {}", args.join(" ")))
    }

    fn id(value: &Value) -> Option<String> {
        for key in ["id", "pane_id", "tab_id", "workspace_id"] {
            if let Some(value) = value.get(key).and_then(Value::as_str) { return Some(value.to_string()); }
        }
        None
    }

    fn items(value: Value) -> Vec<Value> {
        value.as_array().cloned().or_else(|| value.get("items").and_then(Value::as_array).cloned())
            .or_else(|| value.get("panes").and_then(Value::as_array).cloned())
            .or_else(|| value.get("tabs").and_then(Value::as_array).cloned())
            .unwrap_or_default()
    }

    fn pane_list(&self) -> Result<Vec<Value>> { Ok(Self::items(self.json(&["pane".into(), "list".into()])?)) }
    fn tab_list(&self) -> Result<Vec<Value>> { Ok(Self::items(self.json(&["tab".into(), "list".into()])?)) }
    fn focused_workspace(&self) -> Result<String> {
        self.items(self.json(&["workspace".into(), "list".into()])?).into_iter()
            .find(|item| item.get("focused").and_then(Value::as_bool).unwrap_or(false))
            .and_then(|item| Self::id(&item))
            .ok_or_else(|| anyhow!("herdr has no focused workspace"))
    }
    fn focused_tab(&self) -> Result<String> {
        self.tab_list()?.into_iter().find(|item| item.get("focused").and_then(Value::as_bool).unwrap_or(false))
            .and_then(|item| Self::id(&item)).ok_or_else(|| anyhow!("herdr has no focused tab"))
    }
    fn pane_by_id(&self, pane_id: &str) -> Result<Option<Value>> {
        Ok(self.pane_list()?.into_iter().find(|pane| Self::id(pane).as_deref() == Some(pane_id)))
    }
    fn tab_by_name(&self, name: &str) -> Result<Option<Value>> {
        Ok(self.tab_list()?.into_iter().find(|tab| tab.get("label").or_else(|| tab.get("name")).and_then(Value::as_str) == Some(name)))
    }
    fn path_arg(path: &Path) -> Result<String> { path.to_str().map(str::to_owned).ok_or_else(|| anyhow!("path contains non-UTF-8 characters")) }
    fn sh_command(command: Option<&str>) -> Vec<String> {
        command.map(|value| vec!["sh".into(), "-c".into(), value.into()]).unwrap_or_default()
    }
}

impl Multiplexer for HerdrBackend {
    fn name(&self) -> &'static str { "herdr" }
    fn is_running(&self) -> Result<bool> { Ok(self.run(&["status".into(), "server".into()]).is_ok()) }
    fn current_pane_id(&self) -> Option<String> { std::env::var("HERDR_PANE_ID").ok() }
    fn active_pane_id(&self) -> Option<String> { self.pane_list().ok()?.into_iter().find(|p| p.get("focused").and_then(Value::as_bool).unwrap_or(false)).and_then(|p| Self::id(&p)) }
    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        let pane = self.active_pane_id().ok_or_else(|| anyhow!("no active herdr pane"))?;
        let value = self.pane_by_id(&pane)?.ok_or_else(|| anyhow!("herdr pane {pane} not found"))?;
        value.get("cwd").or_else(|| value.get("working_dir")).and_then(Value::as_str).map(PathBuf::from).ok_or_else(|| anyhow!("herdr did not report pane cwd"))
    }
    fn create_window(&self, params: CreateWindowParams) -> Result<String> {
        let workspace = self.focused_workspace()?;
        let name = util::prefixed(params.prefix, params.name);
        let cwd = Self::path_arg(params.cwd)?;
        let before = self.pane_list()?;
        let mut args = vec!["tab".into(), "create".into(), "--workspace".into(), workspace, "--cwd".into(), cwd, "--label".into(), name, "--no-focus".into()];
        self.json(&args)?;
        self.pane_list()?.into_iter().find_map(|pane| { let id = Self::id(&pane)?; (!before.iter().any(|old| Self::id(old).as_deref() == Some(&id))).then_some(id) }).ok_or_else(|| anyhow!("herdr did not return a pane for the new tab"))
    }
    fn create_session(&self, _params: CreateSessionParams) -> Result<String> { Err(anyhow!("session mode is not supported by herdr; use workspace/window mode")) }
    fn switch_to_session(&self, _prefix: &str, _name: &str) -> Result<()> { Err(anyhow!("session mode is not supported by herdr")) }
    fn kill_window(&self, full_name: &str) -> Result<()> { if let Some(tab) = self.tab_by_name(full_name)? { self.run(&["tab".into(), "close".into(), Self::id(&tab).ok_or_else(|| anyhow!("tab has no id"))?])?; } Ok(()) }
    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> { self.run_deferred_script(&format!("sleep {}; herdr tab close '{}'", delay.as_secs_f64(), full_name.replace('\'', "'\\''"))) }
    fn schedule_session_close(&self, _full_name: &str, _delay: Duration) -> Result<()> { Err(anyhow!("session mode is not supported by herdr")) }
    fn run_deferred_script(&self, script: &str) -> Result<()> { util::run_detached_sh_c(script) }
    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String> { let tab = self.tab_by_name(full_name)?.ok_or_else(|| anyhow!("tab not found"))?; Ok(format!("herdr tab focus {}", super::agent::shell_quote(&Self::id(&tab).unwrap()))) }
    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> { Ok(format!("herdr tab close {}", super::agent::shell_quote(full_name))) }
    fn shell_switch_session_cmd(&self, _full_name: &str) -> Result<String> { Err(anyhow!("session mode is not supported by herdr")) }
    fn shell_kill_session_cmd(&self, _full_name: &str) -> Result<String> { Err(anyhow!("session mode is not supported by herdr")) }
    fn select_window(&self, prefix: &str, name: &str) -> Result<()> { let name = util::prefixed(prefix, name); let tab = self.tab_by_name(&name)?.ok_or_else(|| anyhow!("tab '{name}' not found"))?; self.run(&["tab".into(), "focus".into(), Self::id(&tab).unwrap()])?; Ok(()) }
    fn current_window_name(&self) -> Result<Option<String>> { Ok(self.tab_list()?.into_iter().find(|t| t.get("focused").and_then(Value::as_bool).unwrap_or(false)).and_then(|t| t.get("label").or_else(|| t.get("name")).and_then(Value::as_str).map(str::to_owned))) }
    fn get_all_window_names(&self) -> Result<HashSet<String>> { Ok(self.tab_list()?.into_iter().filter_map(|t| t.get("label").or_else(|| t.get("name")).and_then(Value::as_str).map(str::to_owned)).collect()) }
    fn wait_until_session_closed(&self, _full_session_name: &str) -> Result<()> { Err(anyhow!("session mode is not supported by herdr")) }
    fn select_pane(&self, pane_id: &str) -> Result<()> { self.run(&["pane".into(), "send-keys".into(), pane_id.into(), "Escape".into()])?; Ok(()) }
    fn switch_to_pane(&self, pane_id: &str, _window_hint: Option<&str>) -> Result<()> { self.select_pane(pane_id) }
    fn kill_pane(&self, pane_id: &str) -> Result<()> { self.run(&["pane".into(), "close".into(), pane_id.into()])?; Ok(()) }
    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> { self.send_keys(pane_id, &format!("cd {}{}", super::agent::shell_quote(&Self::path_arg(cwd)?), cmd.map(|v| format!(" && {v}")).unwrap_or_default()))?; Ok(pane_id.into()) }
    fn set_pane_name(&self, pane_id: &str, name: &str) -> Result<()> { self.run(&["pane".into(), "rename".into(), pane_id.into(), name.into()])?; Ok(()) }
    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String> { self.run(&["pane".into(), "read".into(), pane_id.into(), "--source".into(), "recent".into(), "--lines".into(), lines.to_string(), "--format".into(), "text".into()]).ok() }
    fn send_text_fragment(&self, pane_id: &str, text: &str) -> Result<()> { self.run(&["pane".into(), "send-text".into(), pane_id.into(), text.into()])?; Ok(()) }
    fn send_enter(&self, pane_id: &str) -> Result<()> { self.run(&["pane".into(), "send-keys".into(), pane_id.into(), "Enter".into()])?; Ok(()) }
    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> { self.run(&["pane".into(), "send-keys".into(), pane_id.into(), key.into()])?; Ok(()) }
    fn paste_text(&self, pane_id: &str, content: &str) -> Result<()> { self.send_text_fragment(pane_id, content) }
    fn set_status(&self, pane_id: &str, icon: &str, _auto_clear: bool) -> Result<()> { self.run(&["pane".into(), "report-metadata".into(), pane_id.into(), "--source".into(), "workmux".into(), "--custom-status".into(), icon.into()])?; Ok(()) }
    fn clear_status(&self, pane_id: &str) -> Result<()> { self.run(&["pane".into(), "report-metadata".into(), pane_id.into(), "--source".into(), "workmux".into(), "--clear-custom-status".into()])?; Ok(()) }
    fn ensure_status_format(&self, _pane_id: &str) -> Result<()> { Ok(()) }
    fn split_pane(&self, target: &str, direction: &SplitDirection, cwd: &Path, _size: Option<u16>, _percentage: Option<u8>, command: Option<&str>) -> Result<String> {
        let direction = match direction { SplitDirection::Horizontal => "right", SplitDirection::Vertical => "down", SplitDirection::Stacked => return Err(anyhow!("herdr does not support stacked splits")) };
        let before: HashSet<_> = self.pane_list()?.into_iter().filter_map(|p| Self::id(&p)).collect();
        self.run(&["pane".into(), "split".into(), target.into(), "--direction".into(), direction.into(), "--cwd".into(), Self::path_arg(cwd)?, "--no-focus".into()])?;
        let pane = self.pane_list()?.into_iter().find_map(|p| { let id = Self::id(&p)?; (!before.contains(&id)).then_some(id) }).ok_or_else(|| anyhow!("herdr did not report new split pane"))?;
        if let Some(command) = command { self.run(&["pane".into(), "run".into(), pane.clone(), command.into()])?; }
        Ok(pane)
    }
    fn instance_id(&self) -> String { std::env::var("HERDR_SOCKET").unwrap_or_else(|_| "default".into()) }
    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> { Ok(self.pane_by_id(pane_id)?.map(|p| LivePaneInfo { pid: p.get("pid").and_then(Value::as_u64).map(|v| v as u32), current_command: p.get("command").and_then(Value::as_str).map(str::to_owned), working_dir: p.get("cwd").and_then(Value::as_str).map(PathBuf::from).unwrap_or_default(), title: p.get("label").and_then(Value::as_str).map(str::to_owned), session: p.get("workspace_id").and_then(Value::as_str).map(str::to_owned), window: p.get("tab_label").and_then(Value::as_str).map(str::to_owned), session_id: p.get("workspace_id").and_then(Value::as_str).map(str::to_owned), window_id: p.get("tab_id").and_then(Value::as_str).map(str::to_owned) })) }
    fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> { self.pane_list()?.into_iter().filter_map(|p| { let id = Self::id(&p)?; Some((id, LivePaneInfo { pid: p.get("pid").and_then(Value::as_u64).map(|v| v as u32), current_command: p.get("command").and_then(Value::as_str).map(str::to_owned), working_dir: p.get("cwd").and_then(Value::as_str).map(PathBuf::from).unwrap_or_default(), title: p.get("label").and_then(Value::as_str).map(str::to_owned), session: p.get("workspace_id").and_then(Value::as_str).map(str::to_owned), window: p.get("tab_label").and_then(Value::as_str).map(str::to_owned), session_id: p.get("workspace_id").and_then(Value::as_str).map(str::to_owned), window_id: p.get("tab_id").and_then(Value::as_str).map(str::to_owned) })) }).collect::<HashMap<_, _>>().pipe(Ok) }
}

trait Pipe: Sized { fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T { f(self) } }
impl<T> Pipe for T {}
