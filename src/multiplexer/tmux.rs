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
use super::{Multiplexer, PaneHandshake, agent, util};

/// tmux backend implementation.
///
/// This struct wraps all tmux-specific operations and implements the Multiplexer
/// trait to provide a unified interface with other backends.
#[derive(Debug, Default)]
pub struct TmuxBackend;

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

        let mut cmd = Cmd::new("tmux").args(&["new-window", "-d"]);

        // Insert after the target window if specified (keeps workmux windows grouped)
        if let Some(target) = params.after_window {
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

    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String> {
        let session = self.current_session().unwrap_or_default();
        let session_prefix = if session.is_empty() {
            String::new()
        } else {
            format!("{}:", session)
        };
        let target = format!("{}={}", session_prefix, full_name);
        let escaped = format!("'{}'", target.replace('\'', r#"'\''"#));
        Ok(format!("tmux select-window -t {} >/dev/null 2>&1", escaped))
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        let session = self.current_session().unwrap_or_default();
        let session_prefix = if session.is_empty() {
            String::new()
        } else {
            format!("{}:", session)
        };
        let target = format!("{}={}", session_prefix, full_name);
        let escaped = format!("'{}'", target.replace('\'', r#"'\''"#));
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

    fn current_window_name(&self) -> Result<Option<String>> {
        match self.tmux_query(&["display-message", "-p", "#{window_name}"]) {
            Ok(name) => Ok(Some(name.trim().to_string())),
            Err(_) => Ok(None),
        }
    }

    fn current_session(&self) -> Option<String> {
        self.tmux_query(&["display-message", "-p", "#{session_name}"])
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

    fn get_all_session_names(&self) -> Result<HashSet<String>> {
        let sessions = self
            .tmux_query(&["list-sessions", "-F", "#{session_name}"])
            .unwrap_or_default();
        Ok(sessions.lines().map(String::from).collect())
    }

    fn filter_active_windows(&self, windows: &[String]) -> Result<Vec<String>> {
        let all_current = self.get_all_window_names()?;

        Ok(windows
            .iter()
            .filter(|w| all_current.contains(*w))
            .cloned()
            .collect())
    }

    fn find_last_window_with_prefix(&self, prefix: &str) -> Result<Option<String>> {
        let output = self
            .tmux_query(&["list-windows", "-F", "#{window_id} #{window_name}"])
            .unwrap_or_default();

        let mut last_match: Option<String> = None;

        for line in output.lines() {
            if let Some((id, name)) = line.split_once(' ')
                && name.starts_with(prefix)
            {
                last_match = Some(id.to_string());
            }
        }

        Ok(last_match)
    }

    fn find_last_window_with_base_handle(
        &self,
        prefix: &str,
        base_handle: &str,
    ) -> Result<Option<String>> {
        let output = self
            .tmux_query(&["list-windows", "-F", "#{window_id} #{window_name}"])
            .unwrap_or_default();

        let full_base = util::prefixed(prefix, base_handle);
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

    fn wait_until_windows_closed(&self, full_window_names: &[String]) -> Result<()> {
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
            if !self.is_running()? {
                return Ok(());
            }

            let current_windows = self.get_all_window_names()?;

            let any_exists = targets
                .iter()
                .any(|target| current_windows.contains(target));

            if !any_exists {
                return Ok(());
            }

            thread::sleep(Duration::from_millis(500));
        }
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

    fn apply_window_color(&self, pane_id: &str, name: &str) -> Result<()> {
        let scheme = window_color_for_name(name);
        // Pane content: background and text color
        let pane_style = format!("bg={},fg={}", scheme.bg, scheme.fg);
        self.tmux_cmd(&[
            "set-option",
            "-w",
            "-t",
            pane_id,
            "window-style",
            &pane_style,
        ])?;
        self.tmux_cmd(&[
            "set-option",
            "-w",
            "-t",
            pane_id,
            "window-active-style",
            &pane_style,
        ])?;
        // Pane borders
        self.tmux_cmd(&[
            "set-option",
            "-w",
            "-t",
            pane_id,
            "pane-border-style",
            &format!("fg={}", scheme.border),
        ])?;
        self.tmux_cmd(&[
            "set-option",
            "-w",
            "-t",
            pane_id,
            "pane-active-border-style",
            &format!("fg={}", scheme.active_border),
        ])?;
        // Status bar: this window's tab in the bottom bar
        let tab_style = format!("bg={},fg={}", scheme.status_bg, scheme.status_fg);
        let active_tab_style = format!("bg={},fg={},bold", scheme.active_border, scheme.status_fg);
        self.tmux_cmd(&[
            "set-option",
            "-w",
            "-t",
            pane_id,
            "window-status-style",
            &tab_style,
        ])?;
        self.tmux_cmd(&[
            "set-option",
            "-w",
            "-t",
            pane_id,
            "window-status-current-style",
            &active_tab_style,
        ])?;
        // Status bar background: update the session-level status-style when this window is focused.
        // Uses hook index [99] to avoid conflicting with other pane-focus-in hooks.
        let status_bar_style = format!("bg={},fg={}", scheme.status_bg, scheme.status_fg);
        let hook_cmd = format!("set-option status-style '{}'", status_bar_style);
        let _ = self.tmux_cmd(&[
            "set-hook",
            "-w",
            "-t",
            pane_id,
            "pane-focus-in[99]",
            &hook_cmd,
        ]);
        // Apply immediately for the current window
        self.tmux_cmd(&[
            "set-option",
            "-t",
            pane_id,
            "status-style",
            &status_bar_style,
        ])?;
        Ok(())
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

    fn send_keys(&self, pane_id: &str, command: &str) -> Result<()> {
        self.tmux_cmd(&["send-keys", "-t", pane_id, "-l", command])?;
        self.tmux_cmd(&["send-keys", "-t", pane_id, "Enter"])
    }

    fn send_keys_to_agent(&self, pane_id: &str, command: &str, agent: Option<&str>) -> Result<()> {
        if agent::resolve_profile(agent).needs_bang_delay() && command.starts_with('!') {
            // Send ! first
            self.tmux_cmd(&["send-keys", "-t", pane_id, "-l", "!"])?;

            // Small delay to let Claude register the !
            thread::sleep(Duration::from_millis(50));

            // Send the rest of the command
            self.tmux_cmd(&["send-keys", "-t", pane_id, "-l", &command[1..]])?;

            // Send Enter
            self.tmux_cmd(&["send-keys", "-t", pane_id, "Enter"])
        } else {
            self.send_keys(pane_id, command)
        }
    }

    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        self.tmux_cmd(&["send-keys", "-t", pane_id, key])
    }

    fn paste_multiline(&self, pane_id: &str, content: &str) -> Result<()> {
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

        self.tmux_cmd(&["paste-buffer", "-t", pane_id, "-p", "-d"])?;

        // Small delay to let the application process the bracketed paste before sending Enter
        thread::sleep(Duration::from_millis(100));

        self.tmux_cmd(&["send-keys", "-t", pane_id, "Enter"])
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
        std::env::var("TMUX")
            .ok()
            .and_then(|tmux| tmux.split(',').next().map(String::from))
            .unwrap_or_else(|| "default".to_string())
    }

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        let format = "#{pane_id}\t#{pane_pid}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_title}\t#{session_name}\t#{window_name}";

        // Use display-message to query a specific pane
        let output = self.tmux_query(&["display-message", "-t", pane_id, "-p", format]);

        let output = match output {
            Ok(o) => o,
            Err(_) => return Ok(None), // Pane doesn't exist or error querying
        };

        let line = output.trim();
        if line.is_empty() {
            return Ok(None);
        }

        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 7 {
            return Ok(None);
        }

        Ok(Some(LivePaneInfo {
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
        }))
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
        use std::collections::HashMap;

        let format = "#{pane_id}\t#{pane_pid}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_title}\t#{session_name}\t#{window_name}";

        // Use list-panes -a to query ALL panes across all sessions at once
        let output = self.tmux_query(&["list-panes", "-a", "-F", format])?;

        let mut panes = HashMap::new();

        for line in output.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 7 {
                continue;
            }

            let pane_id = parts[0].to_string();
            panes.insert(
                pane_id,
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
                },
            );
        }

        Ok(panes)
    }
}

/// A complete color scheme for a tmux window: background, text, borders, and status bar.
struct WindowColorScheme {
    /// Subtle background tint
    bg: &'static str,
    /// Text color (light, complementary to background hue)
    fg: &'static str,
    /// Pane border color (muted accent)
    border: &'static str,
    /// Active pane border color (brighter accent)
    active_border: &'static str,
    /// Status bar tab background for this window
    status_bg: &'static str,
    /// Status bar tab text color for this window
    status_fg: &'static str,
}

/// Palette of 12 color schemes for per-window tinting.
/// Each scheme uses a tinted dark background with coordinated text, border, and status bar colors.
const WINDOW_COLOR_PALETTE: &[WindowColorScheme] = &[
    WindowColorScheme {
        bg: "#141430",
        fg: "#b8b8d0",
        border: "#3a3a5e",
        active_border: "#6868b0",
        status_bg: "#1e1e48",
        status_fg: "#c8c8e0",
    }, // blue
    WindowColorScheme {
        bg: "#143014",
        fg: "#b8d0b8",
        border: "#3a5e3a",
        active_border: "#68b068",
        status_bg: "#1e481e",
        status_fg: "#c8e0c8",
    }, // green
    WindowColorScheme {
        bg: "#301414",
        fg: "#d0b8b8",
        border: "#5e3a3a",
        active_border: "#b06868",
        status_bg: "#481e1e",
        status_fg: "#e0c8c8",
    }, // red
    WindowColorScheme {
        bg: "#143030",
        fg: "#b8d0d0",
        border: "#3a5e5e",
        active_border: "#68b0b0",
        status_bg: "#1e4848",
        status_fg: "#c8e0e0",
    }, // cyan
    WindowColorScheme {
        bg: "#303014",
        fg: "#d0d0b8",
        border: "#5e5e3a",
        active_border: "#b0b068",
        status_bg: "#48481e",
        status_fg: "#e0e0c8",
    }, // yellow
    WindowColorScheme {
        bg: "#301430",
        fg: "#d0b8d0",
        border: "#5e3a5e",
        active_border: "#b068b0",
        status_bg: "#481e48",
        status_fg: "#e0c8e0",
    }, // magenta
    WindowColorScheme {
        bg: "#141a28",
        fg: "#b8c0c8",
        border: "#3a4858",
        active_border: "#6878a0",
        status_bg: "#1e2840",
        status_fg: "#c8d0d8",
    }, // slate
    WindowColorScheme {
        bg: "#281a14",
        fg: "#c8c0b8",
        border: "#58483a",
        active_border: "#a07868",
        status_bg: "#40281e",
        status_fg: "#d8d0c8",
    }, // warm
    WindowColorScheme {
        bg: "#142818",
        fg: "#b8c8c0",
        border: "#3a5848",
        active_border: "#68a078",
        status_bg: "#1e4028",
        status_fg: "#c8d8d0",
    }, // forest
    WindowColorScheme {
        bg: "#1a1428",
        fg: "#c0b8c8",
        border: "#483a58",
        active_border: "#7868a0",
        status_bg: "#281e40",
        status_fg: "#d0c8d8",
    }, // purple
    WindowColorScheme {
        bg: "#282814",
        fg: "#c8c8b8",
        border: "#58583a",
        active_border: "#a0a068",
        status_bg: "#40401e",
        status_fg: "#d8d8c8",
    }, // olive
    WindowColorScheme {
        bg: "#141c24",
        fg: "#b8c0c5",
        border: "#3a4850",
        active_border: "#688090",
        status_bg: "#1e2c38",
        status_fg: "#c8d0d5",
    }, // steel
];

/// Derive a deterministic palette color scheme from a name string.
/// Uses FNV-1a hash for fast, well-distributed mapping.
fn window_color_for_name(name: &str) -> &'static WindowColorScheme {
    let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
    for byte in name.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    &WINDOW_COLOR_PALETTE[(hash as usize) % WINDOW_COLOR_PALETTE.len()]
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

    #[test]
    fn test_window_color_deterministic() {
        // Same name always produces the same color scheme
        let scheme1 = window_color_for_name("feature-auth");
        let scheme2 = window_color_for_name("feature-auth");
        assert_eq!(scheme1.bg, scheme2.bg);
        assert_eq!(scheme1.fg, scheme2.fg);
        assert_eq!(scheme1.border, scheme2.border);
        assert_eq!(scheme1.active_border, scheme2.active_border);
    }

    #[test]
    fn test_window_color_different_names() {
        // Different names should (very likely) produce different schemes
        let scheme1 = window_color_for_name("feature-auth");
        let scheme2 = window_color_for_name("fix-bug-123");
        let scheme3 = window_color_for_name("refactor-db");
        // At least 2 of 3 should differ
        assert!(scheme1.bg != scheme2.bg || scheme2.bg != scheme3.bg);
    }

    #[test]
    fn test_window_color_scheme_has_all_fields() {
        // Every name produces a scheme with non-empty hex color values
        for name in &["main", "develop", "feature-x", "hotfix-y", "a", ""] {
            let scheme = window_color_for_name(name);
            let fields = [
                ("bg", scheme.bg),
                ("fg", scheme.fg),
                ("border", scheme.border),
                ("active_border", scheme.active_border),
                ("status_bg", scheme.status_bg),
                ("status_fg", scheme.status_fg),
            ];
            for (field, value) in &fields {
                assert!(!value.is_empty(), "{} empty for '{}'", field, name);
                assert!(value.starts_with('#'), "{} not hex for '{}'", field, name);
            }
        }
    }
}
