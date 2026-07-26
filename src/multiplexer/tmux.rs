//! tmux backend implementation for the Multiplexer trait.
//!
//! This module provides TmuxBackend, which wraps all tmux-specific operations
//! and exposes them through the Multiplexer trait interface.

use anyhow::{Context, Result, anyhow};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::cmd::Cmd;
use crate::config::SplitDirection as ConfigSplitDirection;

use super::handshake::TmuxHandshake;
use super::types::*;
use super::{Multiplexer, PaneHandshake, util};

/// tmux backend implementation.
///
/// This struct wraps all tmux-specific operations and implements the Multiplexer
/// trait to provide a unified interface with other backends.
#[derive(Debug, Default)]
pub struct TmuxBackend;

const LIVE_PANE_FORMAT: &str = "#{pane_id}\t#{pane_pid}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_title}\t#{session_name}\t#{window_name}\t#{session_id}\t#{window_id}";

fn parse_live_pane_line(line: &str) -> Option<(String, LivePaneInfo)> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 7 {
        return None;
    }

    Some((
        parts[0].to_string(),
        LivePaneInfo {
            pid: parts[1].parse().ok(),
            current_command: Some(parts[2].to_string()),
            working_dir: PathBuf::from(parts[3]),
            title: if parts[4].is_empty() {
                None
            } else {
                Some(parts[4].to_string())
            },
            session: Some(parts[5].to_string()),
            window: Some(parts[6].to_string()),
            session_id: parts
                .get(7)
                .map(|value| value.to_string())
                .filter(|value| !value.is_empty()),
            window_id: parts
                .get(8)
                .map(|value| value.to_string())
                .filter(|value| !value.is_empty()),
        },
    ))
}

impl TmuxBackend {
    /// Create a new TmuxBackend instance.
    pub fn new() -> Self {
        Self
    }

    /// Run a tmux command, returning an error with context on failure.
    fn tmux_cmd(&self, args: &[&str]) -> Result<()> {
        Cmd::new("tmux")
            .args(args)
            .run()
            .with_context(|| format!("tmux command failed: {:?}", args))?;
        Ok(())
    }

    /// Run a tmux command and capture stdout.
    fn tmux_query(&self, args: &[&str]) -> Result<String> {
        Cmd::new("tmux")
            .args(args)
            .run_and_capture_stdout()
            .with_context(|| format!("tmux query failed: {:?}", args))
    }

    /// Get the default shell configured in tmux.
    fn get_default_shell_internal(&self) -> Result<String> {
        let output = self.tmux_query(&["show-option", "-gqv", "default-shell"])?;
        let shell = output.trim();
        if shell.is_empty() {
            Ok("/bin/bash".to_string())
        } else {
            Ok(shell.to_string())
        }
    }

    /// Execute a shell script via tmux run-shell.
    fn run_shell(&self, script: &str) -> Result<()> {
        self.tmux_cmd(&["run-shell", script])
    }

    fn window_target_arg(target: &WindowTarget) -> String {
        match target.parent_session() {
            Some(session) => format!("{}:={}", session, target.full_name),
            None => format!("={}", target.full_name),
        }
    }

    fn shell_escape(value: &str) -> String {
        format!("'{}'", value.replace('\'', r#"'\''"#))
    }

    /// Clear the window status display (status bar icon).
    fn clear_window_status_internal(&self, pane_id: &str) {
        let _ = self.tmux_cmd(&["set-option", "-uw", "-t", pane_id, "@workmux_status"]);
    }

    /// Updates a single tmux format option for the target window to include workmux status.
    fn update_format_option(&self, pane: &str, option: &str) -> Result<()> {
        // Read current format. Try window-level first, fall back to global.
        //
        // Uses run() instead of tmux_query()/run_and_capture_stdout() because the latter
        // calls .trim() which strips meaningful whitespace from format strings (e.g.,
        // padding spaces in tmux themes). We only strip trailing newlines from command output.
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
            self.tmux_cmd(&["set-option", "-w", "-t", pane, option, &new_format])?;
        }
        Ok(())
    }

    /// Internal split pane implementation.
    fn split_pane_internal(
        &self,
        target_pane_id: &str,
        direction: &ConfigSplitDirection,
        working_dir: &Path,
        size: Option<u16>,
        percentage: Option<u8>,
        shell_command: Option<&str>,
    ) -> Result<String> {
        let split_arg = match direction {
            ConfigSplitDirection::Horizontal => "-h",
            ConfigSplitDirection::Vertical => "-v",
            ConfigSplitDirection::Stacked => {
                return Err(anyhow!(
                    "split: stacked is only supported by the Zellij backend"
                ));
            }
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
            "-P",
            "-F",
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

        // Wrap in sh -c "..." to ensure POSIX evaluation even when tmux's
        // default-shell is a non-POSIX shell like nushell.
        let wrapped;
        if let Some(script) = shell_command {
            wrapped = format!("sh -c \"{}\"", util::escape_for_double_quotes(script));
            cmd = cmd.arg(&wrapped);
        }

        let new_pane_id = cmd
            .run_and_capture_stdout()
            .context("Failed to split pane")?;

        Ok(new_pane_id.trim().to_string())
    }

    fn shell_window_command(&self, command: &str, full_name: &str) -> Result<String> {
        let session = self.current_session().unwrap_or_default();
        let session_prefix = if session.is_empty() {
            String::new()
        } else {
            format!("{}:", session)
        };
        let target = format!("{}={}", session_prefix, full_name);
        let escaped = Self::shell_escape(&target);
        Ok(format!("tmux {command} -t {escaped} >/dev/null 2>&1"))
    }
}

impl Multiplexer for TmuxBackend {
    fn name(&self) -> &'static str {
        "tmux"
    }

    // === Server/Session ===

    fn is_running(&self) -> Result<bool> {
        Cmd::new("tmux").arg("has-session").run_as_check()
    }

    fn current_pane_id(&self) -> Option<String> {
        std::env::var("TMUX_PANE").ok()
    }

    fn active_pane_id(&self) -> Option<String> {
        self.tmux_query(&["display-message", "-p", "#{pane_id}"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
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

    // === Window/Tab Management ===

    fn create_window(&self, params: CreateWindowParams) -> Result<String> {
        let prefixed_name = util::prefixed(params.prefix, params.name);
        let working_dir_str = params
            .cwd
            .to_str()
            .ok_or_else(|| anyhow!("Working directory path contains non-UTF8 characters"))?;

        let mut cmd = Cmd::new("tmux").args(&["new-window", "-d", "-a"]);

        // With no explicit target, tmux inserts after the current window.
        if let Some(target) = params.after_window {
            cmd = cmd.args(&["-t", target]);
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

    fn create_session(&self, params: CreateSessionParams) -> Result<String> {
        let prefixed_name = util::prefixed(params.prefix, params.name);
        let working_dir_str = params
            .cwd
            .to_str()
            .ok_or_else(|| anyhow!("Working directory path contains non-UTF8 characters"))?;

        // Create a new detached session with the specified name and working directory
        // -d: detached (don't switch to it yet)
        // -s: session name
        // -c: start directory
        // -P -F: print the pane ID of the initial window
        let mut cmd = Cmd::new("tmux").args(&[
            "new-session",
            "-d",
            "-s",
            &prefixed_name,
            "-c",
            working_dir_str,
        ]);

        // Optionally name the initial window
        if let Some(window_name) = params.initial_window_name {
            cmd = cmd.args(&["-n", window_name]);
        }

        let pane_id = cmd
            .args(&["-P", "-F", "#{pane_id}"])
            .run_and_capture_stdout()
            .context("Failed to create tmux session and get pane ID")?;

        let pane_id = pane_id.trim().to_string();

        // Disable automatic window renaming for named windows so the name stays
        if params.initial_window_name.is_some() {
            let _ = self.tmux_cmd(&[
                "set-window-option",
                "-w",
                "-t",
                &pane_id,
                "automatic-rename",
                "off",
            ]);
        }

        Ok(pane_id)
    }

    fn create_window_in_session(&self, params: CreateWindowInSessionParams) -> Result<String> {
        let working_dir_str = params
            .cwd
            .to_str()
            .ok_or_else(|| anyhow!("Working directory path contains non-UTF8 characters"))?;

        // Target the specific session with trailing colon (creates window at next index)
        let target = format!("{}:", params.session_name);

        let mut cmd =
            Cmd::new("tmux").args(&["new-window", "-d", "-t", &target, "-c", working_dir_str]);

        // Optionally name the window
        if let Some(window_name) = params.name {
            cmd = cmd.args(&["-n", window_name]);
        }

        let pane_id = cmd
            .args(&["-P", "-F", "#{pane_id}"])
            .run_and_capture_stdout()
            .context("Failed to create window in session")?;

        let pane_id = pane_id.trim().to_string();

        // Disable automatic window renaming for named windows
        if params.name.is_some() {
            let _ = self.tmux_cmd(&[
                "set-window-option",
                "-w",
                "-t",
                &pane_id,
                "automatic-rename",
                "off",
            ]);
        }

        Ok(pane_id)
    }

    fn switch_to_session(&self, prefix: &str, name: &str) -> Result<()> {
        let prefixed_name = util::prefixed(prefix, name);
        self.tmux_cmd(&["switch-client", "-t", &prefixed_name])
    }

    fn session_exists(&self, full_name: &str) -> Result<bool> {
        // has-session returns 0 if session exists, 1 if not
        Cmd::new("tmux")
            .args(&["has-session", "-t", full_name])
            .run_as_check()
    }

    fn kill_session(&self, full_name: &str) -> Result<()> {
        self.tmux_cmd(&["kill-session", "-t", full_name])
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        let target = format!("={}", full_name);
        self.tmux_cmd(&["kill-window", "-t", &target])
    }

    fn kill_window_target(&self, target: &WindowTarget) -> Result<()> {
        let target_arg = Self::window_target_arg(target);
        self.tmux_cmd(&["kill-window", "-t", &target_arg])
    }

    fn rename_window(&self, old_full_name: &str, new_full_name: &str) -> Result<()> {
        // `=` prefix forces exact-name match so we don't hit similarly-named windows.
        let target = format!("={}", old_full_name);
        self.tmux_cmd(&["rename-window", "-t", &target, new_full_name])
    }

    fn rename_session(&self, old_full_name: &str, new_full_name: &str) -> Result<()> {
        // `=` prefix forces exact-name match so we don't hit similarly-named sessions.
        let target = format!("={}", old_full_name);
        self.tmux_cmd(&["rename-session", "-t", &target, new_full_name])
    }

    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        let delay_secs = format!("{:.3}", delay.as_secs_f64());
        let target = format!("={}", full_name);
        let escaped_target = format!("'{}'", target.replace('\'', r#"'\''"#));
        let script = format!(
            "sleep {delay}; tmux kill-window -t {target} >/dev/null 2>&1",
            delay = delay_secs,
            target = escaped_target
        );

        self.run_shell(&script)
    }

    fn schedule_window_target_close(&self, target: &WindowTarget, delay: Duration) -> Result<()> {
        let delay_secs = format!("{:.3}", delay.as_secs_f64());
        let target_arg = Self::window_target_arg(target);
        let escaped_target = Self::shell_escape(&target_arg);
        let script = format!(
            "sleep {delay}; tmux kill-window -t {target} >/dev/null 2>&1",
            delay = delay_secs,
            target = escaped_target
        );

        self.run_shell(&script)
    }

    fn schedule_session_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        let delay_secs = format!("{:.3}", delay.as_secs_f64());
        let escaped_name = format!("'{}'", full_name.replace('\'', r#"'\''"#));
        let script = format!(
            "sleep {delay}; tmux kill-session -t {name} >/dev/null 2>&1",
            delay = delay_secs,
            name = escaped_name
        );

        self.run_shell(&script)
    }

    fn run_deferred_script(&self, script: &str) -> Result<()> {
        self.run_shell(script)
    }

    fn current_window_id(&self) -> Result<Option<String>> {
        let Some(pane_id) = self.current_pane_id() else {
            return Ok(None);
        };
        match self.tmux_query(&["display-message", "-p", "-t", &pane_id, "#{window_id}"]) {
            Ok(id) => Ok(Some(id.trim().to_string()).filter(|s| !s.is_empty())),
            Err(_) => Ok(None),
        }
    }

    fn rightmost_window_id(&self) -> Result<Option<String>> {
        let Some(pane_id) = self.current_pane_id() else {
            return Ok(None);
        };
        let session_id =
            match self.tmux_query(&["display-message", "-p", "-t", &pane_id, "#{session_id}"]) {
                Ok(id) => id.trim().to_string(),
                Err(_) => return Ok(None),
            };
        if session_id.is_empty() {
            return Ok(None);
        }

        match self.tmux_query(&["list-windows", "-t", &session_id, "-F", "#{window_id}"]) {
            Ok(ids) => Ok(ids
                .lines()
                .filter_map(|id| {
                    let id = id.trim();
                    (!id.is_empty()).then(|| id.to_string())
                })
                .last()),
            Err(_) => Ok(None),
        }
    }

    fn current_session_id(&self) -> Result<Option<String>> {
        let Some(pane_id) = self.current_pane_id() else {
            return Ok(None);
        };
        match self.tmux_query(&["display-message", "-p", "-t", &pane_id, "#{session_id}"]) {
            Ok(id) => Ok(Some(id.trim().to_string()).filter(|s| !s.is_empty())),
            Err(_) => Ok(None),
        }
    }

    fn shell_close_window_by_id_guard_cmd(&self, id: &str) -> Result<String> {
        let escaped = Self::shell_escape(id);
        Ok(format!(
            "tmux display-message -p -t {target} '#{{window_id}}' >/dev/null 2>&1 && tmux kill-window -t {target} >/dev/null 2>&1 || true",
            target = escaped
        ))
    }

    fn shell_close_session_by_id_guard_cmd(&self, id: &str) -> Result<String> {
        let escaped = Self::shell_escape(id);
        Ok(format!(
            "tmux has-session -t {target} >/dev/null 2>&1 && tmux kill-session -t {target} >/dev/null 2>&1 || true",
            target = escaped
        ))
    }

    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String> {
        self.shell_window_command("select-window", full_name)
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        self.shell_window_command("kill-window", full_name)
    }

    fn shell_kill_window_target_cmd(&self, target: &WindowTarget) -> Result<String> {
        let target_arg = Self::window_target_arg(target);
        let escaped = Self::shell_escape(&target_arg);
        Ok(format!("tmux kill-window -t {} >/dev/null 2>&1", escaped))
    }

    fn shell_switch_session_cmd(&self, full_name: &str) -> Result<String> {
        let escaped = format!("'{}'", full_name.replace('\'', r#"'\''"#));
        Ok(format!("tmux switch-client -t {} >/dev/null 2>&1", escaped))
    }

    fn shell_kill_session_cmd(&self, full_name: &str) -> Result<String> {
        let escaped = format!("'{}'", full_name.replace('\'', r#"'\''"#));
        Ok(format!("tmux kill-session -t {} >/dev/null 2>&1", escaped))
    }

    fn shell_switch_to_last_session_cmd(&self) -> Result<String> {
        Ok("tmux switch-client -l >/dev/null 2>&1".to_string())
    }

    fn select_window(&self, prefix: &str, name: &str) -> Result<()> {
        let prefixed_name = util::prefixed(prefix, name);
        let target = format!("={}", prefixed_name);
        self.tmux_cmd(&["select-window", "-t", &target])
    }

    fn select_window_target(&self, target: &WindowTarget) -> Result<()> {
        let target_arg = Self::window_target_arg(target);
        self.tmux_cmd(&["switch-client", "-t", &target_arg])
            .or_else(|_| self.tmux_cmd(&["select-window", "-t", &target_arg]))
    }

    fn window_exists(&self, prefix: &str, name: &str) -> Result<bool> {
        let prefixed_name = util::prefixed(prefix, name);
        self.window_exists_by_full_name(&prefixed_name)
    }

    fn window_exists_by_full_name(&self, full_name: &str) -> Result<bool> {
        match self.tmux_query(&["list-windows", "-F", "#{window_name}"]) {
            Ok(output) => Ok(output.lines().any(|line| line == full_name)),
            Err(_) => Ok(false),
        }
    }

    fn window_target_exists(&self, target: &WindowTarget) -> Result<bool> {
        let windows = match target.parent_session() {
            Some(session) => self.get_window_names_in_session(session)?,
            None => self.get_all_window_names()?,
        };
        Ok(windows.contains(&target.full_name))
    }

    fn current_window_name(&self) -> Result<Option<String>> {
        let Some(pane_id) = self.current_pane_id() else {
            return Ok(None);
        };
        match self.tmux_query(&["display-message", "-p", "-t", &pane_id, "#{window_name}"]) {
            Ok(name) => Ok(Some(name.trim().to_string()).filter(|s| !s.is_empty())),
            Err(_) => Ok(None),
        }
    }

    fn current_session(&self) -> Option<String> {
        let pane_id = self.current_pane_id()?;
        self.tmux_query(&["display-message", "-p", "-t", &pane_id, "#{session_name}"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        let windows = self
            .tmux_query(&["list-windows", "-F", "#{window_name}"])
            .unwrap_or_default();
        Ok(windows.lines().map(String::from).collect())
    }

    fn get_window_names_in_session(&self, session_name: &str) -> Result<HashSet<String>> {
        let target = format!("{}:", session_name);
        let windows = self
            .tmux_query(&["list-windows", "-t", &target, "-F", "#{window_name}"])
            .unwrap_or_default();
        Ok(windows.lines().map(String::from).collect())
    }

    fn get_all_windows_with_sessions(&self) -> Result<HashSet<(String, String)>> {
        let windows = self
            .tmux_query(&[
                "list-windows",
                "-a",
                "-F",
                "#{window_name}\t#{session_name}",
            ])
            .unwrap_or_default();
        Ok(windows
            .lines()
            .filter_map(|line| {
                let (window, session) = line.split_once('\t')?;
                Some((window.to_string(), session.to_string()))
            })
            .collect())
    }

    fn get_all_session_names(&self) -> Result<HashSet<String>> {
        let sessions = self
            .tmux_query(&["list-sessions", "-F", "#{session_name}"])
            .unwrap_or_default();
        Ok(sessions.lines().map(String::from).collect())
    }

    fn wait_until_session_closed(&self, full_session_name: &str) -> Result<()> {
        println!("Waiting for session '{}' to close...", full_session_name);

        loop {
            if !self.is_running()? {
                return Ok(());
            }

            if !self.session_exists(full_session_name)? {
                return Ok(());
            }

            thread::sleep(Duration::from_millis(500));
        }
    }

    // === Pane Management ===

    fn select_pane(&self, pane_id: &str) -> Result<()> {
        self.tmux_cmd(&["select-pane", "-t", pane_id])
    }

    fn zoom_pane(&self, pane_id: &str) -> Result<()> {
        self.tmux_cmd(&["resize-pane", "-Z", "-t", pane_id])
    }

    fn switch_to_pane(&self, pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
        self.tmux_cmd(&["switch-client", "-t", pane_id])
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        self.tmux_cmd(&["kill-pane", "-t", pane_id])
    }

    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        let working_dir_str = cwd
            .to_str()
            .ok_or_else(|| anyhow!("Working directory path contains non-UTF8 characters"))?;

        let mut command =
            Cmd::new("tmux").args(&["respawn-pane", "-t", pane_id, "-c", working_dir_str, "-k"]);

        // Wrap in sh -c "..." to ensure POSIX evaluation even when tmux's
        // default-shell is a non-POSIX shell like nushell.
        let wrapped;
        if let Some(script) = cmd {
            wrapped = format!("sh -c \"{}\"", util::escape_for_double_quotes(script));
            command = command.arg(&wrapped);
        }

        command.run().context("Failed to respawn pane")?;

        // tmux respawn-pane keeps the same pane_id
        Ok(pane_id.to_string())
    }

    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String> {
        let start_line = format!("-{}", lines);
        self.tmux_query(&["capture-pane", "-p", "-e", "-S", &start_line, "-t", pane_id])
            .ok()
    }

    // === Text I/O ===

    fn send_text_fragment(&self, pane_id: &str, text: &str) -> Result<()> {
        self.tmux_cmd(&["send-keys", "-t", pane_id, "-l", text])
    }

    fn send_enter(&self, pane_id: &str) -> Result<()> {
        self.tmux_cmd(&["send-keys", "-t", pane_id, "Enter"])
    }

    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        self.tmux_cmd(&["send-keys", "-t", pane_id, key])
    }

    fn paste_text(&self, pane_id: &str, content: &str) -> Result<()> {
        use std::io::Write;

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

        self.tmux_cmd(&["paste-buffer", "-t", pane_id, "-p", "-d"])
    }

    // === Shell ===

    fn get_default_shell(&self) -> Result<String> {
        self.get_default_shell_internal()
    }

    fn create_handshake(&self) -> Result<Box<dyn PaneHandshake>> {
        Ok(Box::new(TmuxHandshake::new()?))
    }

    // === Status ===

    fn set_status(&self, pane_id: &str, icon: &str, auto_clear_on_focus: bool) -> Result<()> {
        // Window-level option for tmux status bar display (shared across panes in a window).
        if let Err(e) = self.tmux_cmd(&["set-option", "-w", "-t", pane_id, "@workmux_status", icon])
        {
            eprintln!("workmux: failed to set window status: {}", e);
        }

        // Pane-level option for per-agent sidebar tracking. Unlike the window option,
        // this is unique per pane so the sidebar can track individual agent statuses
        // even when multiple agents share a window.
        let _ = self.tmux_cmd(&[
            "set-option",
            "-p",
            "-t",
            pane_id,
            "@workmux_pane_status",
            icon,
        ]);

        // Set up hook to auto-clear status when a pane receives focus.
        // Used for "waiting" and "done" statuses so they clear once the user sees them.
        if auto_clear_on_focus {
            // The pane-focus-in hook fires in the context of the focused pane, so
            // `set-option -up` targets that specific pane's option. This makes
            // auto-clear work per-agent even with multiple agents in one window.
            let hook_cmd = format!(
                "set-option -up @workmux_pane_status ; if-shell -F \"#{{==:#{{@workmux_status}},{}}}\" \"set-option -uw @workmux_status\"",
                icon
            );
            let _ = self.tmux_cmd(&["set-hook", "-w", "-t", pane_id, "pane-focus-in", &hook_cmd]);
        }

        Ok(())
    }

    fn clear_status(&self, pane_id: &str) -> Result<()> {
        self.clear_window_status_internal(pane_id);
        let _ = self.tmux_cmd(&["set-option", "-up", "-t", pane_id, "@workmux_pane_status"]);
        Ok(())
    }

    fn ensure_status_format(&self, pane_id: &str) -> Result<()> {
        self.update_format_option(pane_id, "window-status-format")?;
        self.update_format_option(pane_id, "window-status-current-format")?;
        Ok(())
    }

    fn split_pane(
        &self,
        target_pane_id: &str,
        direction: &crate::config::SplitDirection,
        cwd: &Path,
        size: Option<u16>,
        percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        self.split_pane_internal(target_pane_id, direction, cwd, size, percentage, command)
    }

    // === State Reconciliation ===

    fn instance_id(&self) -> String {
        // TMUX env var format: /path/to/socket,pid,session_index
        // We use only the socket path, which identifies the tmux server.
        // All sessions on the same server share one socket, so instance_id
        // is per-server, not per-session.
        if let Some(socket_path) = std::env::var("TMUX")
            .ok()
            .and_then(|tmux| tmux.split(',').next().map(String::from))
            .filter(|socket_path| !socket_path.is_empty())
        {
            return socket_path;
        }

        self.tmux_query(&["display-message", "-p", "#{socket_path}"])
            .ok()
            .map(|socket_path| socket_path.trim().to_string())
            .filter(|socket_path| !socket_path.is_empty())
            .unwrap_or_else(|| "default".to_string())
    }

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        // Use display-message to query a specific pane
        let output = self.tmux_query(&["display-message", "-t", pane_id, "-p", LIVE_PANE_FORMAT]);

        let output = match output {
            Ok(o) => o,
            Err(_) => return Ok(None), // Pane doesn't exist or error querying
        };

        Ok(parse_live_pane_line(output.trim()).map(|(_, info)| info))
    }

    fn server_boot_id(&self) -> Result<Option<String>> {
        // #{start_time} is the Unix timestamp when the tmux server started.
        // Stable across the server's lifetime, changes on restart.
        self.tmux_query(&["display-message", "-p", "#{start_time}"])
            .map(|s| {
                let trimmed = s.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            })
            .or_else(|_| Ok(None))
    }

    fn get_all_live_pane_info(&self) -> Result<std::collections::HashMap<String, LivePaneInfo>> {
        // Use list-panes -a to query ALL panes across all sessions at once
        let output = self.tmux_query(&["list-panes", "-a", "-F", LIVE_PANE_FORMAT])?;

        Ok(output.lines().filter_map(parse_live_pane_line).collect())
    }
}
/// Format string to inject into tmux window-status-format.
const WORKMUX_STATUS_FORMAT: &str = "#{?@workmux_status, #{@workmux_status},}";

/// Injects workmux status format into an existing format string.
fn inject_status_format(format: &str) -> String {
    let patterns = ["#{window_flags", "#{?window_flags", "#{F}"];
    let insert_pos = patterns.iter().filter_map(|p| format.find(p)).min();

    if let Some(pos) = insert_pos {
        let (before, after) = format.split_at(pos);
        format!("{}{}{}", before, WORKMUX_STATUS_FORMAT, after)
    } else {
        format!("{}{}", format, WORKMUX_STATUS_FORMAT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inject_status_format_standard() {
        let input = "#I:#W#{?window_flags,#{window_flags}, }";
        let result = inject_status_format(input);
        assert_eq!(
            result,
            "#I:#W#{?@workmux_status, #{@workmux_status},}#{?window_flags,#{window_flags}, }"
        );
    }

    #[test]
    fn test_inject_status_format_short_flags() {
        let input = "#I:#W#{F}";
        let result = inject_status_format(input);
        assert_eq!(result, "#I:#W#{?@workmux_status, #{@workmux_status},}#{F}");
    }

    #[test]
    fn test_inject_status_format_no_flags() {
        let input = "#I:#W";
        let result = inject_status_format(input);
        assert_eq!(result, "#I:#W#{?@workmux_status, #{@workmux_status},}");
    }

    #[test]
    fn test_inject_status_format_complex() {
        let input = "#[fg=blue]#I#[default] #{?window_flags,#{window_flags},}";
        let result = inject_status_format(input);
        assert_eq!(
            result,
            "#[fg=blue]#I#[default] #{?@workmux_status, #{@workmux_status},}#{?window_flags,#{window_flags},}"
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
