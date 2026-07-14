//! Herdr backend implementation for the Multiplexer trait.
//!
//! This module provides HerdrBackend, which drives herdr through its JSON CLI.
//! It holds no session state of its own: everything is read from the environment
//! or queried live. Like the WezTerm backend, it is CLI-driven and needs no
//! daemon socket.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cmd::Cmd;
use crate::config::SplitDirection;
use crate::shell::shell_quote;

use super::Multiplexer;
use super::types::*;
use super::util;

/// herdr's error code for a pane that no longer exists.
const PANE_NOT_FOUND: &str = "pane_not_found";

/// A decoded herdr JSON envelope.
///
/// herdr reports failures in-band: the process still exits 0 and the envelope
/// carries an `error` object instead of `result`. Treating a missing `result`
/// as an empty payload would turn every backend failure into "nothing there",
/// so the two cases are kept distinct.
#[derive(Debug, PartialEq)]
enum Envelope {
    Result(Value),
    Error { code: String, message: String },
}

fn parse_envelope(raw: &str) -> Result<Envelope> {
    let envelope: Value = serde_json::from_str(raw)?;

    if let Some(error) = envelope.get("error").filter(|e| !e.is_null()) {
        let code = error["code"].as_str().unwrap_or_default().to_string();
        let message = error["message"].as_str().unwrap_or_default().to_string();
        return Ok(Envelope::Error { code, message });
    }

    match envelope.get("result") {
        Some(result) if !result.is_null() => Ok(Envelope::Result(result.clone())),
        _ => Err(anyhow!("envelope has neither result nor error")),
    }
}

/// Build a `cd <cwd>` script, optionally chaining a command onto it.
///
/// The path is quoted because worktree paths routinely contain spaces.
fn cd_script(cwd: &Path, cmd: Option<&str>) -> String {
    let quoted = shell_quote(&cwd.to_string_lossy());
    match cmd {
        Some(command) => format!("cd {quoted} && {command}"),
        None => format!("cd {quoted}"),
    }
}

/// `create.rs` rejects session mode for non-tmux backends, so no caller reaches
/// these; herdr has workspaces, not sessions.
fn no_sessions<T>() -> Result<T> {
    Err(anyhow!("sessions not supported by the herdr backend"))
}

#[derive(Debug, Deserialize)]
struct HerdrWorkspace {
    workspace_id: String,
    label: String,
    #[serde(default)]
    focused: bool,
}

#[derive(Debug, Deserialize)]
struct HerdrPane {
    pane_id: String,
    workspace_id: String,
    cwd: String,
    foreground_cwd: Option<String>,
}

impl HerdrPane {
    fn working_dir(&self) -> PathBuf {
        let path = self.foreground_cwd.as_deref().unwrap_or(&self.cwd);
        PathBuf::from(path)
    }
}

/// Herdr backend. Invokes `herdr` (or `$HERDR_BIN_PATH`) for all operations.
#[derive(Debug)]
pub struct HerdrBackend {
    bin: String,
}

impl Default for HerdrBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl HerdrBackend {
    pub fn new() -> Self {
        // Deferred cleanup scripts embed this path, so it must resolve whether
        // or not herdr is on `PATH`.
        Self {
            bin: std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into()),
        }
    }

    fn cmd(&self) -> Cmd<'_> {
        Cmd::new(&self.bin)
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        self.cmd()
            .args(args)
            .run_and_capture_stdout()
            .with_context(|| format!("herdr {}", args.join(" ")))
    }

    /// Run a herdr command and decode its JSON envelope.
    fn run_envelope(&self, args: &[&str]) -> Result<Envelope> {
        let raw = self.run(args)?;
        parse_envelope(&raw).with_context(|| format!("parse JSON from: herdr {}", args.join(" ")))
    }

    /// Run a herdr command and return its `result` payload, failing on an error
    /// envelope. Callers that need to act on a specific error code use
    /// `run_envelope` instead.
    fn run_json(&self, args: &[&str]) -> Result<Value> {
        match self.run_envelope(args)? {
            Envelope::Result(result) => Ok(result),
            Envelope::Error { code, message } => {
                Err(anyhow!("herdr {}: {message} ({code})", args.join(" ")))
            }
        }
    }

    fn list_workspaces(&self) -> Result<Vec<HerdrWorkspace>> {
        let result = self.run_json(&["workspace", "list"])?;
        let workspaces: Vec<HerdrWorkspace> =
            serde_json::from_value(result["workspaces"].clone()).context("parse workspace list")?;
        Ok(workspaces)
    }

    fn list_panes(&self) -> Result<Vec<HerdrPane>> {
        let result = self.run_json(&["pane", "list"])?;
        let panes: Vec<HerdrPane> =
            serde_json::from_value(result["panes"].clone()).context("parse pane list")?;
        Ok(panes)
    }

    fn workspace_id_for_label(&self, label: &str) -> Result<String> {
        self.list_workspaces()?
            .into_iter()
            .find(|w| w.label == label)
            .map(|w| w.workspace_id)
            .ok_or_else(|| anyhow!("no workspace with label '{label}'"))
    }

    fn workspace_label_for_id(&self, workspace_id: &str) -> Result<String> {
        self.list_workspaces()?
            .into_iter()
            .find(|w| w.workspace_id == workspace_id)
            .map(|w| w.label)
            .ok_or_else(|| anyhow!("no workspace with id '{workspace_id}'"))
    }

    fn workspace_id_for_pane(&self, pane_id: &str) -> Option<String> {
        // pane_id format is "wN:pM", so the workspace_id is the part before the
        // separator. Without a separator the id is not one we can attribute, and
        // guessing would target an unrelated workspace.
        pane_id
            .split_once(':')
            .map(|(workspace_id, _)| workspace_id.to_string())
    }

    // `worktree create`/`worktree open` resolve their source workspace from
    // herdr's ambient/focused-client state when `--workspace` is omitted.
    // That state drifts as workspaces open/close during a session and can
    // point at a linked worktree, which herdr refuses as a source
    // ("New and open worktree actions start from the repo parent
    // workspace"). HERDR_PANE_ID always names the actual calling pane, so
    // deriving `--workspace` from it sidesteps ambient drift entirely.
    fn own_workspace_id(&self) -> Option<String> {
        std::env::var("HERDR_PANE_ID")
            .ok()
            .and_then(|pane_id| self.workspace_id_for_pane(&pane_id))
    }

    /// Shared by `worktree create` and `worktree open`: both answer in the same
    /// envelope shape.
    ///
    /// Appends `--workspace` so herdr resolves the source from the calling pane
    /// rather than from ambient focus.
    fn worktree_workspace(&self, args: &[&str]) -> Result<Option<(String, String)>> {
        let mut args = args.to_vec();
        let source_workspace = self.own_workspace_id();
        if let Some(ref workspace_id) = source_workspace {
            args.extend_from_slice(&["--workspace", workspace_id]);
        }

        let result = self.run_json(&args)?;
        let field = |path: &str| {
            result
                .pointer(path)
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or_else(|| anyhow!("herdr {}: missing result{path}", args.join(" ")))
        };
        Ok(Some((
            field("/workspace/workspace_id")?,
            field("/root_pane/pane_id")?,
        )))
    }

    /// Attach a workspace to a worktree already on disk.
    ///
    /// Separate from `worktree create`, which always runs `git worktree add` and
    /// so fails with "already exists" on a path that is already checked out.
    fn open_worktree_workspace(
        &self,
        path: &Path,
        label: &str,
    ) -> Result<Option<(String, String)>> {
        let path_str = path.to_string_lossy();
        self.worktree_workspace(&[
            "worktree",
            "open",
            "--path",
            path_str.as_ref(),
            "--label",
            label,
            "--no-focus",
        ])
    }

    /// herdr has no send-and-submit primitive.
    fn send_line(&self, pane_id: &str, text: &str) -> Result<()> {
        self.run(&["pane", "send-text", pane_id, text])?;
        self.run(&["pane", "send-keys", pane_id, "Enter"])?;
        Ok(())
    }
}

impl Multiplexer for HerdrBackend {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn supports_atomic_worktree_workspace(&self) -> bool {
        true
    }

    // === Server ===

    fn is_running(&self) -> Result<bool> {
        if std::env::var("HERDR_PANE_ID").is_ok() {
            return Ok(true);
        }
        Ok(self.run_json(&["workspace", "list"]).is_ok())
    }

    // === Pane ID from environment ===

    fn current_pane_id(&self) -> Option<String> {
        std::env::var("HERDR_PANE_ID").ok()
    }

    fn active_pane_id(&self) -> Option<String> {
        let result = self.run_json(&["pane", "current"]).ok()?;
        result["pane"]["pane_id"].as_str().map(String::from)
    }

    // === Active pane path ===

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        let result = self.run_json(&["pane", "current"])?;
        let cwd = result["pane"]["foreground_cwd"]
            .as_str()
            .or_else(|| result["pane"]["cwd"].as_str())
            .ok_or_else(|| anyhow!("pane current: missing cwd"))?;
        Ok(PathBuf::from(cwd))
    }

    // === Window / workspace management ===

    fn create_worktree_and_workspace(
        &self,
        branch: &str,
        base: Option<&str>,
        path: &Path,
        label: &str,
    ) -> Result<Option<(String, String)>> {
        let path_str = path.to_string_lossy();
        let mut args = vec![
            "worktree",
            "create",
            "--branch",
            branch,
            "--path",
            path_str.as_ref(),
            "--label",
            label,
            "--no-focus",
        ];
        if let Some(base_ref) = base {
            args.extend_from_slice(&["--base", base_ref]);
        }
        self.worktree_workspace(&args)
    }

    fn remove_worktree_and_workspace(&self, workspace_id: &str) -> Result<bool> {
        self.run(&["worktree", "remove", "--workspace", workspace_id])?;
        Ok(true)
    }

    fn shell_remove_worktree_and_workspace_cmd(
        &self,
        workspace_id: &str,
    ) -> Result<Option<String>> {
        Ok(Some(format!(
            "{} worktree remove --workspace {}",
            shell_quote(&self.bin),
            shell_quote(workspace_id)
        )))
    }

    fn ensure_worktree_workspace(
        &self,
        path: &Path,
        label: &str,
        branch: &str,
    ) -> Result<Option<(String, String)>> {
        if self.workspace_id_for_label(label).is_ok() {
            return Ok(None);
        }
        if path.exists() {
            self.open_worktree_workspace(path, label)
        } else {
            // The worktree was removed outside workmux, so the directory has to
            // be recreated alongside the workspace.
            self.create_worktree_and_workspace(branch, None, path, label)
        }
    }

    fn create_window(&self, params: CreateWindowParams) -> Result<String> {
        let label = util::prefixed(params.prefix, params.name);
        let cwd = params.cwd.to_string_lossy();
        let result = self.run_json(&[
            "workspace",
            "create",
            "--cwd",
            &cwd,
            "--label",
            &label,
            "--no-focus",
        ])?;
        let pane_id = result["root_pane"]["pane_id"]
            .as_str()
            .ok_or_else(|| anyhow!("workspace create: missing root_pane.pane_id"))?;
        Ok(pane_id.to_string())
    }

    fn create_session(&self, _params: CreateSessionParams) -> Result<String> {
        no_sessions()
    }

    fn switch_to_session(&self, _prefix: &str, _name: &str) -> Result<()> {
        no_sessions()
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        let workspace_id = self.workspace_id_for_label(full_name)?;
        self.run(&["workspace", "close", &workspace_id])?;
        Ok(())
    }

    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        let workspace_id = self.workspace_id_for_label(full_name)?;
        let secs = delay.as_secs();
        let script = format!("sleep {secs}; {} workspace close {workspace_id}", self.bin);
        util::run_detached_sh_c(&script)
    }

    fn schedule_session_close(&self, _full_name: &str, _delay: Duration) -> Result<()> {
        no_sessions()
    }

    fn run_deferred_script(&self, script: &str) -> Result<()> {
        util::run_detached_sh_c(script)
    }

    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String> {
        let workspace_id = self.workspace_id_for_label(full_name)?;
        Ok(format!("{} workspace focus {workspace_id}", self.bin))
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        let workspace_id = self.workspace_id_for_label(full_name)?;
        Ok(format!("{} workspace close {workspace_id}", self.bin))
    }

    fn shell_switch_session_cmd(&self, _full_name: &str) -> Result<String> {
        no_sessions()
    }

    fn shell_kill_session_cmd(&self, _full_name: &str) -> Result<String> {
        no_sessions()
    }

    fn select_window(&self, prefix: &str, name: &str) -> Result<()> {
        let label = util::prefixed(prefix, name);
        let workspace_id = self.workspace_id_for_label(&label)?;
        self.run(&["workspace", "focus", &workspace_id])?;
        Ok(())
    }

    fn current_window_name(&self) -> Result<Option<String>> {
        let workspaces = self.list_workspaces()?;
        Ok(workspaces.into_iter().find(|w| w.focused).map(|w| w.label))
    }

    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        Ok(self
            .list_workspaces()?
            .into_iter()
            .map(|w| w.label)
            .collect())
    }

    fn wait_until_session_closed(&self, _full_session_name: &str) -> Result<()> {
        no_sessions()
    }

    // === Pane management ===

    fn select_pane(&self, pane_id: &str) -> Result<()> {
        self.run(&["pane", "zoom", pane_id, "--on"])?;
        Ok(())
    }

    fn switch_to_pane(&self, pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
        if let Some(workspace_id) = self.workspace_id_for_pane(pane_id) {
            let _ = self.run(&["workspace", "focus", &workspace_id]);
        }
        self.run(&["pane", "zoom", pane_id, "--on"])?;
        Ok(())
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        self.run(&["pane", "close", pane_id])?;
        Ok(())
    }

    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        // herdr exposes no respawn primitive, so like the Zellij backend this
        // reuses the pane rather than replacing its process: cd into the target
        // directory and run the command there. The sole caller, `setup_panes`,
        // invokes this on a freshly created pane sitting at a shell prompt, so
        // there is no existing process to displace.
        self.send_line(pane_id, &cd_script(cwd, cmd))?;
        Ok(pane_id.to_string())
    }

    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String> {
        let lines_str = lines.to_string();
        self.run(&[
            "pane",
            "read",
            pane_id,
            "--source",
            "recent-unwrapped",
            "--lines",
            &lines_str,
        ])
        .ok()
    }

    // === Text I/O ===

    fn send_text_fragment(&self, pane_id: &str, text: &str) -> Result<()> {
        self.run(&["pane", "send-text", pane_id, text])?;
        Ok(())
    }

    fn send_enter(&self, pane_id: &str) -> Result<()> {
        self.run(&["pane", "send-keys", pane_id, "Enter"])?;
        Ok(())
    }

    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        self.run(&["pane", "send-keys", pane_id, key])?;
        Ok(())
    }

    fn paste_text(&self, pane_id: &str, content: &str) -> Result<()> {
        // herdr send-text handles newlines correctly; no paste-mode distinction needed
        self.run(&["pane", "send-text", pane_id, content])?;
        Ok(())
    }

    // === Status ===

    fn set_status(&self, pane_id: &str, icon: &str, _auto_clear_on_focus: bool) -> Result<()> {
        self.run(&["pane", "rename", pane_id, icon])?;
        Ok(())
    }

    fn clear_status(&self, pane_id: &str) -> Result<()> {
        self.run(&["pane", "rename", pane_id, "--clear"])?;
        Ok(())
    }

    fn ensure_status_format(&self, _pane_id: &str) -> Result<()> {
        // herdr renders pane status itself; no format string to configure.
        Ok(())
    }

    // === Pane splitting ===

    fn split_pane(
        &self,
        target_pane_id: &str,
        direction: &SplitDirection,
        cwd: &Path,
        _size: Option<u16>,
        percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        let direction_arg = match direction {
            SplitDirection::Horizontal => "right",
            SplitDirection::Vertical => "down",
            SplitDirection::Stacked => {
                return Err(anyhow!(
                    "split: stacked is only supported by the Zellij backend"
                ));
            }
        };
        let cwd_str = cwd.to_string_lossy();

        let mut args = vec![
            "pane",
            "split",
            target_pane_id,
            "--direction",
            direction_arg,
            "--cwd",
            cwd_str.as_ref(),
        ];

        let ratio_str;
        if let Some(pct) = percentage {
            ratio_str = format!("{:.2}", pct as f64 / 100.0);
            args.push("--ratio");
            args.push(&ratio_str);
        }

        let result = self.run_json(&args)?;
        let new_pane_id = result["pane"]["pane_id"]
            .as_str()
            .ok_or_else(|| anyhow!("pane split: missing result.pane.pane_id"))?
            .to_string();

        if let Some(cmd) = command {
            self.send_line(&new_pane_id, &cd_script(cwd, Some(cmd)))?;
        }

        Ok(new_pane_id)
    }

    // === Instance identity ===

    fn instance_id(&self) -> String {
        std::env::var("HERDR_SOCKET_PATH").unwrap_or_else(|_| "herdr-default".into())
    }

    fn resolve_instance_id(&self) -> Result<String> {
        std::env::var("HERDR_SOCKET_PATH")
            .ok()
            .filter(|instance| !instance.trim().is_empty())
            .ok_or_else(|| anyhow!("HERDR_SOCKET_PATH is required to resolve the herdr instance"))
    }

    // === State reconciliation ===

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        // Only a positively reported missing pane counts as gone. Any other
        // failure has to propagate, otherwise a transient herdr error would read
        // as "this agent vanished" and reconciliation would drop live agents.
        let result = match self
            .run_envelope(&["pane", "get", pane_id])
            .with_context(|| format!("failed to query herdr pane {pane_id}"))?
        {
            Envelope::Result(result) => result,
            Envelope::Error { code, .. } if code == PANE_NOT_FOUND => return Ok(None),
            Envelope::Error { code, message } => {
                return Err(anyhow!("herdr pane get {pane_id}: {message} ({code})"));
            }
        };

        let pane = &result["pane"];
        if pane.is_null() {
            return Ok(None);
        }

        let cwd = pane["foreground_cwd"]
            .as_str()
            .or_else(|| pane["cwd"].as_str())
            .unwrap_or("/");
        let working_dir = PathBuf::from(cwd);

        let workspace_id = pane["workspace_id"].as_str().unwrap_or("").to_string();
        let label = self
            .workspace_label_for_id(&workspace_id)
            .unwrap_or_default();

        Ok(Some(util::build_live_pane_info(
            None, // herdr doesn't expose shell PID
            None, // herdr doesn't expose foreground command name
            working_dir,
            "", // no pane title concept in herdr
            workspace_id,
            label,
        )))
    }

    fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
        let panes = self.list_panes()?;
        let workspaces: HashMap<String, String> = self
            .list_workspaces()?
            .into_iter()
            .map(|w| (w.workspace_id, w.label))
            .collect();

        let snapshots = panes.into_iter().map(|p| {
            let label = workspaces.get(&p.workspace_id).cloned().unwrap_or_default();
            let working_dir = p.working_dir();
            util::LivePaneSnapshot {
                pane_id: p.pane_id,
                pid: None,
                current_command: None,
                working_dir,
                title: String::new(),
                session: p.workspace_id,
                window: label,
            }
        });

        Ok(util::live_pane_map(snapshots))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Envelopes below are verbatim herdr 0.7.1 CLI output.

    #[test]
    fn parses_result_envelope() {
        let raw =
            r#"{"id":"cli:pane:get","result":{"pane":{"pane_id":"w2:p1"},"type":"pane_info"}}"#;
        let envelope = parse_envelope(raw).unwrap();
        match envelope {
            Envelope::Result(result) => assert_eq!(result["pane"]["pane_id"], "w2:p1"),
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn parses_error_envelope() {
        // herdr exits 0 for this, so the error is only visible in the payload.
        let raw = r#"{"error":{"code":"pane_not_found","message":"pane w99:p99 not found"},"id":"cli:pane:get"}"#;
        assert_eq!(
            parse_envelope(raw).unwrap(),
            Envelope::Error {
                code: "pane_not_found".to_string(),
                message: "pane w99:p99 not found".to_string(),
            }
        );
    }

    #[test]
    fn envelope_without_result_or_error_is_rejected() {
        // Must not silently decode as an empty result.
        let err = parse_envelope(r#"{"id":"cli:pane:get"}"#).unwrap_err();
        assert!(err.to_string().contains("neither result nor error"));
    }

    #[test]
    fn null_result_is_rejected() {
        let err = parse_envelope(r#"{"id":"cli:pane:get","result":null}"#).unwrap_err();
        assert!(err.to_string().contains("neither result nor error"));
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(parse_envelope("not json").is_err());
    }

    #[test]
    fn workspace_id_for_pane_splits_on_separator() {
        let backend = HerdrBackend {
            bin: "herdr".to_string(),
        };
        assert_eq!(
            backend.workspace_id_for_pane("w2Z:p1"),
            Some("w2Z".to_string())
        );
    }

    #[test]
    fn workspace_id_for_pane_rejects_id_without_separator() {
        // Returning the whole string here would target an unrelated workspace.
        let backend = HerdrBackend {
            bin: "herdr".to_string(),
        };
        assert_eq!(backend.workspace_id_for_pane("nonsense"), None);
    }

    #[test]
    fn cd_script_quotes_paths_with_spaces() {
        let script = cd_script(Path::new("/tmp/my worktree"), Some("claude"));
        assert_eq!(script, "cd '/tmp/my worktree' && claude");
    }

    #[test]
    fn cd_script_leaves_plain_paths_unquoted() {
        let script = cd_script(Path::new("/tmp/wt"), Some("claude"));
        assert_eq!(script, "cd /tmp/wt && claude");
    }

    #[test]
    fn cd_script_without_command_still_changes_directory() {
        assert_eq!(cd_script(Path::new("/tmp/wt"), None), "cd /tmp/wt");
    }

    #[test]
    fn shell_remove_cmd_uses_configured_binary() {
        let backend = HerdrBackend {
            bin: "/opt/herdr/bin/herdr".to_string(),
        };

        let cmd = backend
            .shell_remove_worktree_and_workspace_cmd("ws-abc123")
            .unwrap()
            .unwrap();

        assert_eq!(
            cmd,
            "/opt/herdr/bin/herdr worktree remove --workspace ws-abc123"
        );
    }

    #[test]
    fn shell_remove_cmd_quotes_binary_with_spaces() {
        let backend = HerdrBackend {
            bin: "/Applications/My Tools/herdr".to_string(),
        };

        let cmd = backend
            .shell_remove_worktree_and_workspace_cmd("ws-abc123")
            .unwrap()
            .unwrap();

        assert!(
            cmd.starts_with("'/Applications/My Tools/herdr' worktree remove"),
            "binary path with spaces should be quoted: {cmd}"
        );
    }
}
