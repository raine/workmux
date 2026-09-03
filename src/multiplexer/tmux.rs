//! tmux backend implementation for the Multiplexer trait.
//!
//! This module provides TmuxBackend, which wraps all tmux-specific operations
//! and exposes them through the Multiplexer trait interface.

use anyhow::{Context, Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::cmd::Cmd;
use crate::config::{SplitDirection as ConfigSplitDirection, WindowPlacement};

use super::handshake::TmuxHandshake;
use super::types::*;
use super::{Multiplexer, PaneHandshake, util};

/// tmux backend implementation.
///
/// This struct wraps all tmux-specific operations and implements the Multiplexer
/// trait to provide a unified interface with other backends.
#[derive(Debug, Default)]
pub struct TmuxBackend {
    socket_path: Option<String>,
}

const LIVE_PANE_RECORD_SEPARATOR: char = '\x1e';
const LIVE_PANE_FIELD_SEPARATOR: char = '\x1f';
const LIVE_PANE_ESCAPED_RECORD_SEPARATOR: &str = "\\036";
const LIVE_PANE_ESCAPED_FIELD_SEPARATOR: &str = "\\037";
const LIVE_PANE_FORMAT: &str = "\x1e#{pane_id}\x1f#{pane_pid}\x1f#{pane_current_command}\x1f#{pane_current_path}\x1f#{pane_title}\x1f#{session_name}\x1f#{window_name}\x1f#{session_id}\x1f#{window_id}";
const WINDOW_OWNERSHIP_FORMAT: &str = "\x1e#{window_id}\x1f#{window_name}\x1f#{session_name}\x1f#{@workmux_token}\x1f#{pane_current_path}";
const SIDEBAR_STATE_FORMAT: &str = "\x1e#{pane_id}\x1f#{pane_pid}\x1f#{pane_current_command}\x1f#{pane_current_path}\x1f#{pane_title}\x1f#{session_name}\x1f#{window_name}\x1f#{session_id}\x1f#{window_id}\x1f#{@workmux_pane_status}\x1f#{window_active}\x1f#{session_attached}\x1f#{pane_active}\x1f#{window_index}\x1f#{start_time}\x1f#{@workmux_sidebar_position}\x1f#{@workmux_sidebar_layout}\x1f#{@workmux_sidebar_filter}\x1f#{@workmux_sleeping_panes}";

/// One tmux server observation containing every input needed by a daemon tick.
#[derive(Debug)]
pub(crate) struct TmuxSidebarSnapshot {
    pub live_panes: HashMap<String, LivePaneInfo>,
    pub window_statuses: HashMap<String, Option<String>>,
    pub active_windows: HashSet<(String, String)>,
    pub pane_window_ids: HashMap<String, String>,
    pub pane_window_indexes: HashMap<String, u32>,
    pub active_pane_ids: HashSet<String>,
    pub window_pane_counts: HashMap<String, usize>,
    pub server_boot_id: Option<String>,
    pub position: Option<String>,
    pub layout: Option<String>,
    pub filter: Option<String>,
    pub sleeping_panes: Option<String>,
}

fn live_pane_fields(line: &str) -> Vec<&str> {
    if line.contains(LIVE_PANE_FIELD_SEPARATOR) {
        line.split(LIVE_PANE_FIELD_SEPARATOR).collect()
    } else if line.contains(LIVE_PANE_ESCAPED_FIELD_SEPARATOR) {
        line.split(LIVE_PANE_ESCAPED_FIELD_SEPARATOR).collect()
    } else {
        line.split('\t').collect()
    }
}

fn parse_live_pane_line(line: &str) -> Option<(String, LivePaneInfo)> {
    let line = line
        .strip_prefix(LIVE_PANE_RECORD_SEPARATOR)
        .or_else(|| line.strip_prefix(LIVE_PANE_ESCAPED_RECORD_SEPARATOR))
        .unwrap_or(line);
    let parts = live_pane_fields(line);
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

fn live_pane_records(output: &str) -> Vec<&str> {
    if output.contains(LIVE_PANE_RECORD_SEPARATOR) {
        output
            .split(LIVE_PANE_RECORD_SEPARATOR)
            .filter(|record| !record.is_empty())
            .map(|record| record.strip_suffix('\n').unwrap_or(record))
            .collect()
    } else if output.contains(LIVE_PANE_ESCAPED_RECORD_SEPARATOR) {
        output
            .split(LIVE_PANE_ESCAPED_RECORD_SEPARATOR)
            .filter(|record| !record.is_empty())
            .map(|record| record.strip_suffix('\n').unwrap_or(record))
            .collect()
    } else {
        output.lines().collect()
    }
}

fn parse_live_pane_snapshot_lossy(output: &str) -> HashMap<String, LivePaneInfo> {
    live_pane_records(output)
        .into_iter()
        .filter_map(parse_live_pane_line)
        .collect()
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn parse_sidebar_snapshot(output: &str) -> Result<TmuxSidebarSnapshot> {
    let mut snapshot = TmuxSidebarSnapshot {
        live_panes: HashMap::new(),
        window_statuses: HashMap::new(),
        active_windows: HashSet::new(),
        pane_window_ids: HashMap::new(),
        pane_window_indexes: HashMap::new(),
        active_pane_ids: HashSet::new(),
        window_pane_counts: HashMap::new(),
        server_boot_id: None,
        position: None,
        layout: None,
        filter: None,
        sleeping_panes: None,
    };

    for record in live_pane_records(output) {
        let fields = live_pane_fields(
            record
                .strip_prefix(LIVE_PANE_RECORD_SEPARATOR)
                .or_else(|| record.strip_prefix(LIVE_PANE_ESCAPED_RECORD_SEPARATOR))
                .unwrap_or(record),
        );
        if fields.len() != 19 || fields[0].is_empty() {
            return Err(anyhow!("tmux returned malformed sidebar state: {record:?}"));
        }

        let pane_id = fields[0].to_string();
        let session = fields[5].to_string();
        let window_id = fields[8].to_string();
        let status = nonempty(fields[9]);
        snapshot.live_panes.insert(
            pane_id.clone(),
            LivePaneInfo {
                pid: fields[1].parse().ok(),
                current_command: Some(fields[2].to_string()),
                working_dir: PathBuf::from(fields[3]),
                title: nonempty(fields[4]),
                session: Some(session.clone()),
                window: Some(fields[6].to_string()),
                session_id: nonempty(fields[7]),
                window_id: nonempty(fields[8]),
            },
        );
        snapshot.window_statuses.insert(pane_id.clone(), status);
        snapshot
            .pane_window_ids
            .insert(pane_id.clone(), window_id.clone());
        if let Ok(index) = fields[13].parse() {
            snapshot.pane_window_indexes.insert(pane_id.clone(), index);
        }
        *snapshot
            .window_pane_counts
            .entry(window_id.clone())
            .or_default() += 1;
        if fields[10] == "1" && fields[11] == "1" {
            snapshot.active_windows.insert((session, window_id));
        }
        if fields[12] == "1" {
            snapshot.active_pane_ids.insert(pane_id);
        }

        if snapshot.server_boot_id.is_none() {
            snapshot.server_boot_id = nonempty(fields[14]);
            snapshot.position = nonempty(fields[15]);
            snapshot.layout = nonempty(fields[16]);
            snapshot.filter = nonempty(fields[17]);
            snapshot.sleeping_panes = nonempty(fields[18]);
        }
    }

    Ok(snapshot)
}

fn parse_window_ownership_record(line: &str) -> Option<WindowOwnershipRecord> {
    let line = line
        .strip_prefix(LIVE_PANE_RECORD_SEPARATOR)
        .or_else(|| line.strip_prefix(LIVE_PANE_ESCAPED_RECORD_SEPARATOR))
        .unwrap_or(line);
    let parts = live_pane_fields(line);
    if parts.len() != 5 || parts[0].is_empty() {
        return None;
    }

    Some(WindowOwnershipRecord {
        window_id: parts[0].to_string(),
        window_name: parts[1].to_string(),
        session_name: parts[2].to_string(),
        token: (!parts[3].is_empty()).then(|| parts[3].to_string()),
        pane_path: PathBuf::from(parts[4]),
    })
}

fn parse_window_ownership_records(output: &str) -> Result<Vec<WindowOwnershipRecord>> {
    live_pane_records(output)
        .into_iter()
        .map(|line| {
            parse_window_ownership_record(line)
                .ok_or_else(|| anyhow!("tmux returned malformed window ownership data: {line:?}"))
        })
        .collect()
}

fn parse_live_pane_line_strict(line: &str) -> Result<(String, LivePaneInfo)> {
    let parts = live_pane_fields(line);
    if parts.len() != 9 || parts[0].is_empty() {
        return Err(anyhow!(
            "tmux returned malformed pane information: {line:?}"
        ));
    }
    let pid = parts[1]
        .parse()
        .map_err(|_| anyhow!("tmux returned malformed pane PID: {:?}", parts[1]))?;
    let (pane_id, mut info) = parse_live_pane_line(line)
        .ok_or_else(|| anyhow!("tmux returned malformed pane information: {line:?}"))?;
    info.pid = Some(pid);
    Ok((pane_id, info))
}

fn parse_live_pane_snapshot(output: &str) -> Result<HashMap<String, LivePaneInfo>> {
    let mut panes = HashMap::new();
    for line in live_pane_records(output) {
        let (pane_id, info) = parse_live_pane_line_strict(line)?;
        if panes.insert(pane_id.clone(), info).is_some() {
            return Err(anyhow!("tmux returned duplicate pane ID: {pane_id}"));
        }
    }
    Ok(panes)
}

fn classify_live_pane_query<F>(
    pane_id: &str,
    query_result: Result<String>,
    list_pane_ids: F,
) -> Result<Option<LivePaneInfo>>
where
    F: FnOnce() -> Result<String>,
{
    let query_error = match query_result {
        Ok(output) => {
            if let Some((_, info)) =
                parse_live_pane_line(output.trim()).filter(|(queried_id, _)| queried_id == pane_id)
            {
                return Ok(Some(info));
            }
            anyhow!("tmux returned malformed live pane information for {pane_id}")
        }
        Err(query_error) => query_error,
    };

    let pane_ids = list_pane_ids()
        .with_context(|| format!("failed to confirm whether tmux pane {pane_id} exists"))?;
    let mut pane_is_present = false;
    for candidate in pane_ids.lines() {
        let valid_id = candidate.strip_prefix('%').is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        });
        if !valid_id {
            return Err(anyhow!(
                "tmux returned malformed pane identifier while checking {pane_id}: {candidate:?}"
            ));
        }
        pane_is_present |= candidate == pane_id;
    }

    if pane_is_present {
        Err(query_error.context(format!(
            "tmux pane {pane_id} exists but its live information could not be queried"
        )))
    } else {
        Ok(None)
    }
}

fn select_session_for_cwd(
    live_panes: &HashMap<String, LivePaneInfo>,
    cwd: &Path,
) -> Option<String> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut best_score = 0;
    let mut sessions = HashSet::new();

    for pane in live_panes.values() {
        let Some(session_id) = pane.session_id.as_deref() else {
            continue;
        };
        let pane_cwd = pane
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| pane.working_dir.clone());
        if !cwd.starts_with(&pane_cwd) {
            continue;
        }

        let score = pane_cwd.components().count();
        match score.cmp(&best_score) {
            std::cmp::Ordering::Greater => {
                best_score = score;
                sessions.clear();
                sessions.insert(session_id);
            }
            std::cmp::Ordering::Equal => {
                sessions.insert(session_id);
            }
            std::cmp::Ordering::Less => {}
        }
    }

    if sessions.len() == 1 {
        sessions.into_iter().next().map(str::to_string)
    } else {
        None
    }
}

impl TmuxBackend {
    /// Create a new TmuxBackend instance.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn for_socket(socket_path: &str) -> Self {
        Self {
            socket_path: (socket_path != "default").then(|| socket_path.to_string()),
        }
    }

    fn tmux_command(&self) -> Cmd<'_> {
        match self.socket_path.as_deref() {
            Some(socket_path) => Cmd::new("tmux").args(&["-S", socket_path]),
            None => Cmd::new("tmux"),
        }
    }

    fn shell_tmux_command(&self) -> String {
        self.socket_path.as_deref().map_or_else(
            || "tmux".to_string(),
            |socket_path| format!("tmux -S {}", Self::shell_escape(socket_path)),
        )
    }

    /// Query all tmux state consumed by one sidebar daemon refresh.
    pub(crate) fn sidebar_snapshot(&self) -> Result<TmuxSidebarSnapshot> {
        let output = self.tmux_query(&["list-panes", "-a", "-F", SIDEBAR_STATE_FORMAT])?;
        parse_sidebar_snapshot(&output)
    }

    /// Run a tmux command, returning an error with context on failure.
    fn tmux_cmd(&self, args: &[&str]) -> Result<()> {
        self.tmux_command()
            .args(args)
            .run()
            .with_context(|| format!("tmux command failed: {:?}", args))?;
        Ok(())
    }

    fn load_buffer(&self, content: &str) -> Result<()> {
        use std::io::Write;

        let mut command = std::process::Command::new("tmux");
        if let Some(socket_path) = self.socket_path.as_deref() {
            command.args(["-S", socket_path]);
        }
        let mut child = command
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
            return Err(anyhow!("tmux load-buffer failed"));
        }

        Ok(())
    }

    /// Run a tmux command and capture stdout.
    fn tmux_query(&self, args: &[&str]) -> Result<String> {
        self.tmux_command()
            .args(args)
            .run_and_capture_stdout()
            .with_context(|| format!("tmux query failed: {:?}", args))
    }

    fn sole_session_id(&self) -> Result<String> {
        let output = self.tmux_query(&["list-sessions", "-F", "#{session_id}"])?;
        let session_ids: Vec<_> = output.lines().filter(|id| !id.is_empty()).collect();

        match session_ids.as_slice() {
            [session_id] => Ok((*session_id).to_string()),
            [] => Err(anyhow!(
                "tmux has no sessions available for window placement"
            )),
            _ => Err(anyhow!(
                "Cannot determine the tmux session for window placement because TMUX_PANE is unset or does not identify a live pane, the working directory does not identify a unique session, and {} sessions exist. Pass --parent-session <name> to select one explicitly.",
                session_ids.len()
            )),
        }
    }

    fn active_window_id(&self, session_id: &str) -> Result<String> {
        let window_id = self
            .tmux_query(&[
                "display-message",
                "-p",
                "-t",
                &format!("{session_id}:"),
                "#{window_id}",
            ])
            .with_context(|| format!("Failed to resolve the active window in {session_id}"))?;
        let window_id = window_id.trim();
        if window_id.is_empty() {
            return Err(anyhow!(
                "tmux returned an empty window identifier for session {session_id}"
            ));
        }
        Ok(window_id.to_string())
    }

    fn invoking_window_id(&self) -> Result<String> {
        if let Some(pane_id) = self.current_pane_id().filter(|id| !id.is_empty())
            && let Ok(window_id) =
                self.tmux_query(&["display-message", "-p", "-t", &pane_id, "#{window_id}"])
            && !window_id.trim().is_empty()
        {
            return Ok(window_id.trim().to_string());
        }

        if let Ok(cwd) = std::env::current_dir()
            && let Ok(live_panes) = self.get_all_live_pane_info()
            && let Some(session_id) = select_session_for_cwd(&live_panes, &cwd)
        {
            return self.active_window_id(&session_id);
        }

        let session_id = self.sole_session_id()?;
        self.active_window_id(&session_id)
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
        let escaped_script = script.replace('#', "##");
        self.tmux_cmd(&["run-shell", "-b", &escaped_script])
    }

    fn window_target_arg(target: &WindowTarget) -> String {
        if let Some(window_id) = &target.window_id {
            return window_id.clone();
        }

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
        let window_format = self
            .tmux_command()
            .args(&["show-option", "-wv", "-t", pane, option])
            .run()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|s| s.trim_end_matches('\n').to_string())
            .filter(|s| !s.is_empty());

        let current = match window_format {
            Some(fmt) => fmt,
            None => self
                .tmux_command()
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

        let mut cmd = self.tmux_command().args(&[
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
        Ok(format!(
            "{} {command} -t {escaped} >/dev/null 2>&1",
            self.shell_tmux_command()
        ))
    }
}

impl Multiplexer for TmuxBackend {
    fn name(&self) -> &'static str {
        "tmux"
    }

    // === Server/Session ===

    fn is_running(&self) -> Result<bool> {
        self.tmux_command().arg("has-session").run_as_check()
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
        let tmux = self.shell_tmux_command();
        let script = format!(
            "{tmux} display-message -p -t \"$({tmux} display-message -p '#{{client_session}}')\" '#{{pane_current_path}}'"
        );
        let output = Cmd::new("sh")
            .args(&["-c", &script])
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

        let mut cmd = self.tmux_command().args(&["new-window", "-d", "-a"]);

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
        let mut cmd = self.tmux_command().args(&[
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
            self.tmux_command()
                .args(&["new-window", "-d", "-t", &target, "-c", working_dir_str]);

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

    fn supports_window_ownership(&self) -> bool {
        true
    }

    fn set_window_ownership(&self, pane_id: &str, token: &str, is_primary: bool) -> Result<()> {
        self.tmux_cmd(&[
            "set-window-option",
            "-q",
            "-t",
            pane_id,
            "@workmux_token",
            token,
        ])?;
        self.tmux_cmd(&[
            "set-window-option",
            "-q",
            "-t",
            pane_id,
            "@workmux_primary",
            if is_primary { "1" } else { "0" },
        ])
    }

    fn owned_window_targets(&self, token: &str) -> Result<Vec<OwnedWindowTarget>> {
        let output = self.tmux_query(&[
            "list-windows",
            "-a",
            "-F",
            "#{window_id}\t#{window_name}\t#{session_name}\t#{@workmux_token}\t#{@workmux_primary}",
        ])?;

        Ok(output
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let window_id = parts.next()?;
                let window_name = parts.next()?;
                let session_name = parts.next()?;
                let candidate_token = parts.next()?;
                let primary = parts.next().unwrap_or_default();
                (candidate_token == token).then(|| OwnedWindowTarget {
                    target: WindowTarget::with_id(
                        window_name.to_string(),
                        Some(session_name.to_string()),
                        window_id.to_string(),
                    ),
                    is_primary: primary == "1",
                })
            })
            .collect())
    }

    fn owned_window_tokens(&self) -> Result<HashSet<String>> {
        let output = self.tmux_query(&["list-windows", "-a", "-F", "#{@workmux_token}"])?;
        Ok(output
            .lines()
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn window_ownership_records(&self) -> Result<Vec<WindowOwnershipRecord>> {
        let output = self.tmux_query(&["list-panes", "-a", "-F", WINDOW_OWNERSHIP_FORMAT])?;
        parse_window_ownership_records(&output)
    }

    fn switch_to_session(&self, prefix: &str, name: &str) -> Result<()> {
        let prefixed_name = util::prefixed(prefix, name);
        self.tmux_cmd(&["switch-client", "-t", &prefixed_name])
    }

    fn session_exists(&self, full_name: &str) -> Result<bool> {
        // has-session returns 0 if session exists, 1 if not
        self.tmux_command()
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
                .next_back()),
            Err(_) => Ok(None),
        }
    }

    fn resolve_window_placement_target(
        &self,
        placement: WindowPlacement,
    ) -> Result<Option<String>> {
        let window_id = self.invoking_window_id()?;
        if placement == WindowPlacement::AfterCurrent {
            return Ok(Some(window_id));
        }

        let session_id = self
            .tmux_query(&["display-message", "-p", "-t", &window_id, "#{session_id}"])?
            .trim()
            .to_string();
        if session_id.is_empty() {
            return Err(anyhow!(
                "tmux returned an empty session identifier for window {window_id}"
            ));
        }

        let ids = self.tmux_query(&["list-windows", "-t", &session_id, "-F", "#{window_id}"])?;
        let rightmost = ids
            .lines()
            .filter_map(|id| {
                let id = id.trim();
                (!id.is_empty()).then(|| id.to_string())
            })
            .next_back()
            .ok_or_else(|| anyhow!("tmux session {session_id} has no windows"))?;
        Ok(Some(rightmost))
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
        let tmux = self.shell_tmux_command();
        let target = Self::shell_escape(id);
        Ok(format!(
            "{tmux} display-message -p -t {target} '#{{window_id}}' >/dev/null 2>&1 && {tmux} kill-window -t {target} >/dev/null 2>&1 || true"
        ))
    }

    fn shell_close_session_by_id_guard_cmd(&self, id: &str) -> Result<String> {
        let tmux = self.shell_tmux_command();
        let target = Self::shell_escape(id);
        Ok(format!(
            "{tmux} has-session -t {target} >/dev/null 2>&1 && {tmux} kill-session -t {target} >/dev/null 2>&1 || true"
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
        Ok(format!(
            "{} kill-window -t {} >/dev/null 2>&1",
            self.shell_tmux_command(),
            escaped
        ))
    }

    fn shell_switch_session_cmd(&self, full_name: &str) -> Result<String> {
        let escaped = Self::shell_escape(full_name);
        Ok(format!(
            "{} switch-client -t {} >/dev/null 2>&1",
            self.shell_tmux_command(),
            escaped
        ))
    }

    fn shell_kill_session_cmd(&self, full_name: &str) -> Result<String> {
        let escaped = Self::shell_escape(full_name);
        Ok(format!(
            "{} kill-session -t {} >/dev/null 2>&1",
            self.shell_tmux_command(),
            escaped
        ))
    }

    fn shell_switch_to_last_session_cmd(&self) -> Result<String> {
        Ok(format!(
            "{} switch-client -l >/dev/null 2>&1",
            self.shell_tmux_command()
        ))
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
        if let Some(window_id) = &target.window_id {
            let ids = self.tmux_query(&["list-windows", "-a", "-F", "#{window_id}"])?;
            return Ok(ids.lines().any(|candidate| candidate == window_id));
        }

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

    fn client_session(&self) -> Option<String> {
        self.tmux_query(&["display-message", "-p", "#{client_session}"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| self.current_session())
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
            self.tmux_command()
                .args(&["respawn-pane", "-t", pane_id, "-c", working_dir_str, "-k"]);

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
        self.load_buffer(content)?;
        self.tmux_cmd(&["paste-buffer", "-t", pane_id, "-p", "-d"])
    }

    fn paste_and_submit(&self, pane_id: &str, content: &str) -> Result<()> {
        self.load_buffer(content)?;
        self.tmux_cmd(&[
            "paste-buffer",
            "-t",
            pane_id,
            "-p",
            "-d",
            ";",
            "send-keys",
            "-t",
            pane_id,
            "Enter",
        ])
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
            // The hook body is constant, so it never carries the icon as tmux command
            // syntax. It compares the window status against a separate pane option
            // holding the icon that is awaiting acknowledgement. The pane-focus-in hook
            // fires in the context of the focused pane, so `set-option -up` targets that
            // specific pane's option, which makes auto-clear work per-agent even with
            // multiple agents in one window.
            let _ = self.tmux_cmd(&[
                "set-option",
                "-p",
                "-t",
                pane_id,
                "@workmux_ack_status",
                icon,
            ]);
            let hook_cmd = auto_clear_status_hook();
            let _ = self.tmux_cmd(&["set-hook", "-w", "-t", pane_id, "pane-focus-in", &hook_cmd]);

            // A status in the focused pane is already acknowledged. Background
            // panes retain the status until their pane-focus-in hook runs.
            let _ = self.tmux_cmd(&[
                "if-shell",
                "-F",
                "-t",
                pane_id,
                FOCUSED_PANE_FORMAT,
                &hook_cmd,
            ]);
        } else {
            // Statuses that are not acknowledged on focus must clear the option, so a
            // hook left installed by an earlier acknowledgeable status does not match.
            let _ = self.tmux_cmd(&["set-option", "-up", "-t", pane_id, "@workmux_ack_status"]);
        }

        Ok(())
    }

    fn clear_status(&self, pane_id: &str) -> Result<()> {
        self.clear_window_status_internal(pane_id);
        let _ = self.tmux_cmd(&["set-option", "-up", "-t", pane_id, "@workmux_pane_status"]);
        let _ = self.tmux_cmd(&["set-option", "-up", "-t", pane_id, "@workmux_ack_status"]);
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
        self.resolve_instance_id()
            .unwrap_or_else(|_| "default".to_string())
    }

    fn resolve_instance_id(&self) -> Result<String> {
        if let Some(socket_path) = self.socket_path.as_ref() {
            return Ok(socket_path.clone());
        }

        // TMUX env var format: /path/to/socket,pid,session_index
        // The socket path identifies the server shared by all of its sessions.
        if let Some(socket_path) = std::env::var("TMUX")
            .ok()
            .and_then(|tmux| tmux.split(',').next().map(String::from))
            .filter(|socket_path| !socket_path.is_empty())
        {
            return Ok(socket_path);
        }

        let socket_path = self
            .tmux_query(&["display-message", "-p", "#{socket_path}"])
            .context("failed to resolve tmux socket path")?;
        let socket_path = socket_path.trim();
        if socket_path.is_empty() {
            return Err(anyhow!("tmux returned an empty socket path"));
        }
        Ok(socket_path.to_string())
    }

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        let query_result =
            self.tmux_query(&["display-message", "-t", pane_id, "-p", LIVE_PANE_FORMAT]);

        classify_live_pane_query(pane_id, query_result, || {
            self.tmux_query(&["list-panes", "-a", "-F", "#{pane_id}"])
        })
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
    }

    fn get_all_live_pane_info(&self) -> Result<std::collections::HashMap<String, LivePaneInfo>> {
        // Use list-panes -a to query ALL panes across all sessions at once
        let output = self.tmux_query(&["list-panes", "-a", "-F", LIVE_PANE_FORMAT])?;

        Ok(parse_live_pane_snapshot_lossy(&output))
    }

    fn get_all_live_pane_info_strict(
        &self,
    ) -> Result<std::collections::HashMap<String, LivePaneInfo>> {
        let output = self.tmux_query(&["list-panes", "-a", "-F", LIVE_PANE_FORMAT])?;
        parse_live_pane_snapshot(&output)
    }
}
/// A pane is focused when it is selected in a visible, attached session.
const FOCUSED_PANE_FORMAT: &str = "#{&&:#{pane_active},#{&&:#{window_active},#{session_attached}}}";

/// Build the pane-focus hook that acknowledges a status and refreshes sidebar clients.
///
/// The body is a constant. The icon reaches tmux only as an option value, which tmux
/// does not re-expand as a command, so a hostile icon cannot become tmux command syntax.
fn auto_clear_status_hook() -> String {
    // The acknowledgement option is tested for emptiness rather than truth, because
    // tmux treats the literal "0" as false and that is a usable icon.
    "if-shell -F \"#{&&:#{!=:#{@workmux_ack_status},},#{==:#{@workmux_status},#{@workmux_ack_status}}}\" \"set-option -uw @workmux_status\" ; set-option -up @workmux_pane_status ; set-option -up @workmux_ack_status ; run-shell -b 'kill -USR1 $(tmux show-option -gqv @workmux_sidebar_daemon_pid) 2>/dev/null || true'".to_string()
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

    const LIVE_PANE_LINE: &str = "%7\t12345\tnode\t/repo\tWorking\tmain\twork\t$1\t@2";

    #[test]
    fn run_shell_preserves_literal_tmux_formats() {
        if which::which("tmux").is_err() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("tmux socket ' quoted");
        let marker = temp.path().join("format-job-marker");
        let output = temp.path().join("format-output");
        let socket_text = socket.to_string_lossy();
        Cmd::new("tmux")
            .args(&[
                "-S",
                &socket_text,
                "new-session",
                "-d",
                "-s",
                "literal-session",
            ])
            .run()
            .unwrap();
        let backend = TmuxBackend::for_socket(&socket_text);
        let literal = format!("#{{session_name}} #(touch {})", marker.display());
        let script = format!(
            "printf %s {} > {}",
            crate::shell::shell_quote(&literal),
            crate::shell::shell_quote(&output.to_string_lossy())
        );

        let run_result = backend.run_shell(&script);
        for _ in 0..100 {
            if output.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let output_result = std::fs::read_to_string(&output);
        let marker_exists = marker.exists();
        let _ = Cmd::new("tmux")
            .args(&["-S", &socket_text, "kill-server"])
            .run();

        run_result.unwrap();
        assert_eq!(output_result.unwrap(), literal);
        assert!(!marker_exists);
    }

    fn live_pane(path: &str, session_id: &str) -> LivePaneInfo {
        let line = format!("%7\t12345\tnode\t{path}\tWorking\tmain\twork\t{session_id}\t@2");
        parse_live_pane_line(&line).unwrap().1
    }

    #[test]
    fn sidebar_snapshot_parses_one_server_observation() {
        let output = "\x1e%7\x1f12345\x1fnode\x1f/repo\x1fAgent\x1fmain\x1fwork\x1f$1\x1f@2\x1f✓\x1f1\x1f1\x1f1\x1f4\x1f1700000000\x1ftop\x1fcompact\x1fsession\x1f%7 %8\n\x1e%8\x1f12346\x1fbash\x1f/repo\x1fShell\x1fmain\x1fwork\x1f$1\x1f@2\x1f\x1f1\x1f1\x1f0\x1f4\x1f1700000000\x1ftop\x1fcompact\x1fsession\x1f%7 %8\n";

        let snapshot = parse_sidebar_snapshot(output).unwrap();

        assert_eq!(snapshot.live_panes.len(), 2);
        assert_eq!(snapshot.live_panes["%7"].pid, Some(12345));
        assert_eq!(snapshot.window_statuses["%7"].as_deref(), Some("✓"));
        assert_eq!(snapshot.window_statuses["%8"], None);
        assert_eq!(snapshot.window_pane_counts["@2"], 2);
        assert_eq!(snapshot.pane_window_indexes["%7"], 4);
        assert!(
            snapshot
                .active_windows
                .contains(&("main".into(), "@2".into()))
        );
        assert!(snapshot.active_pane_ids.contains("%7"));
        assert!(!snapshot.active_pane_ids.contains("%8"));
        assert_eq!(snapshot.server_boot_id.as_deref(), Some("1700000000"));
        assert_eq!(snapshot.position.as_deref(), Some("top"));
        assert_eq!(snapshot.layout.as_deref(), Some("compact"));
        assert_eq!(snapshot.filter.as_deref(), Some("session"));
        assert_eq!(snapshot.sleeping_panes.as_deref(), Some("%7 %8"));
    }

    #[test]
    fn sidebar_snapshot_rejects_malformed_records() {
        let error = parse_sidebar_snapshot("\x1e%7\x1f12345\n").unwrap_err();

        assert!(error.to_string().contains("malformed sidebar state"));
    }

    #[test]
    fn configured_socket_identifies_tmux_instance() {
        let backend = TmuxBackend::for_socket("/tmp/tmux socket");

        assert_eq!(backend.instance_id(), "/tmp/tmux socket");
        assert_eq!(backend.shell_tmux_command(), "tmux -S '/tmp/tmux socket'");
    }

    #[test]
    fn window_ownership_snapshot_parses_records() {
        let output = "\x1e@2\x1fwm-work\x1fmain\x1ftoken-1\x1f/repo/work\n\x1e@3\x1fwm-other\x1fmain\x1f\x1f/repo/other\n";

        let records = parse_window_ownership_records(output).unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].window_id, "@2");
        assert_eq!(records[0].token.as_deref(), Some("token-1"));
        assert_eq!(records[0].pane_path, PathBuf::from("/repo/work"));
        assert_eq!(records[1].token, None);
    }

    #[test]
    fn window_ownership_snapshot_rejects_malformed_rows() {
        let error = parse_window_ownership_records("@2\twm-work").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("malformed window ownership data")
        );
    }

    #[test]
    fn live_pane_snapshot_rejects_malformed_rows() {
        let error = parse_live_pane_snapshot(&format!("{LIVE_PANE_LINE}\nmalformed")).unwrap_err();

        assert!(error.to_string().contains("malformed pane information"));
    }

    #[test]
    fn live_pane_snapshot_accepts_octal_escaped_separators() {
        let output = "\\036%7\\03712345\\037node\\037/repo/a\\037Working\\037main\\037work\\037$1\\037@2\n\\036%8\\03712346\\037bash\\037/repo/b\\037Shell\\037main\\037shell\\037$1\\037@3\n";

        let panes = parse_live_pane_snapshot(output).unwrap();

        assert_eq!(panes["%7"].working_dir, PathBuf::from("/repo/a"));
        assert_eq!(panes["%8"].window_id.as_deref(), Some("@3"));
    }

    #[test]
    fn live_pane_snapshot_preserves_newlines_inside_records() {
        let output =
            "\x1e%7\x1f12345\x1fnode\x1f/repo/a\nb\x1fWorking\x1fmain\x1fwork\x1f$1\x1f@2\n";

        let panes = parse_live_pane_snapshot(output).unwrap();

        assert_eq!(panes["%7"].working_dir, PathBuf::from("/repo/a\nb"));
    }

    #[test]
    fn live_pane_snapshot_preserves_tabs_inside_fields() {
        let output =
            "\x1e%7\x1f12345\x1fnode\x1f/repo/a\tb\x1fWorking\x1fmain\x1fwork\x1f$1\x1f@2\n";

        let panes = parse_live_pane_snapshot(output).unwrap();

        assert_eq!(panes["%7"].working_dir, PathBuf::from("/repo/a\tb"));
    }

    #[test]
    fn live_pane_snapshot_rejects_invalid_pid() {
        let error =
            parse_live_pane_snapshot("%7\tnot-a-pid\tnode\t/repo\tWorking\tmain\twork\t$1\t@2")
                .unwrap_err();

        assert!(error.to_string().contains("malformed pane PID"));
    }

    #[test]
    fn lossy_live_pane_snapshot_skips_malformed_rows() {
        let panes = parse_live_pane_snapshot_lossy(&format!("{LIVE_PANE_LINE}\nmalformed"));

        assert_eq!(panes.len(), 1);
        assert!(panes.contains_key("%7"));
    }

    #[test]
    fn live_pane_snapshot_accepts_empty_output() {
        assert!(parse_live_pane_snapshot("").unwrap().is_empty());
    }

    #[test]
    fn cwd_session_selection_accepts_multiple_panes_in_one_session() {
        let panes = HashMap::from([
            ("%1".to_string(), live_pane("/repo", "$1")),
            ("%2".to_string(), live_pane("/repo", "$1")),
        ]);

        assert_eq!(
            select_session_for_cwd(&panes, Path::new("/repo")),
            Some("$1".to_string())
        );
    }

    #[test]
    fn cwd_session_selection_prefers_closest_ancestor() {
        let panes = HashMap::from([
            ("%1".to_string(), live_pane("/repo", "$1")),
            ("%2".to_string(), live_pane("/repo/subdir", "$2")),
        ]);

        assert_eq!(
            select_session_for_cwd(&panes, Path::new("/repo/subdir/package")),
            Some("$2".to_string())
        );
    }

    #[test]
    fn cwd_session_selection_rejects_multiple_best_sessions() {
        let panes = HashMap::from([
            ("%1".to_string(), live_pane("/repo", "$1")),
            ("%2".to_string(), live_pane("/repo", "$2")),
        ]);

        assert_eq!(select_session_for_cwd(&panes, Path::new("/repo")), None);
    }

    #[test]
    fn window_target_prefers_stable_id() {
        let target = WindowTarget::with_id(
            "renamed".to_string(),
            Some("parent".to_string()),
            "@42".to_string(),
        );

        assert_eq!(TmuxBackend::window_target_arg(&target), "@42");
    }

    #[test]
    fn window_target_falls_back_to_parent_qualified_exact_name() {
        let target = WindowTarget::new("work".to_string(), Some("parent".to_string()));

        assert_eq!(TmuxBackend::window_target_arg(&target), "parent:=work");
    }

    #[test]
    fn live_pane_query_returns_parsed_pane() {
        let result = classify_live_pane_query("%7", Ok(LIVE_PANE_LINE.to_string()), || {
            panic!("pane listing should not run after a successful query")
        })
        .unwrap()
        .unwrap();

        assert_eq!(result.pid, Some(12345));
        assert_eq!(result.current_command.as_deref(), Some("node"));
        assert_eq!(result.working_dir, PathBuf::from("/repo"));
        assert_eq!(result.session.as_deref(), Some("main"));
        assert_eq!(result.window.as_deref(), Some("work"));
    }

    #[test]
    fn live_pane_query_returns_none_when_listing_confirms_absence() {
        let result = classify_live_pane_query("%7", Err(anyhow!("query failed")), || {
            Ok("%1\n%3".to_string())
        })
        .unwrap();

        assert!(result.is_none());
    }

    #[test]
    fn live_pane_query_preserves_error_when_listing_finds_pane() {
        let error = classify_live_pane_query("%7", Err(anyhow!("query failed")), || {
            Ok("%1\n%7".to_string())
        })
        .unwrap_err();

        assert!(error.to_string().contains("exists"));
    }

    #[test]
    fn live_pane_query_preserves_error_when_confirmation_fails() {
        let error = classify_live_pane_query("%7", Err(anyhow!("query failed")), || {
            Err(anyhow!("listing failed"))
        })
        .unwrap_err();

        assert!(error.to_string().contains("failed to confirm"));
    }

    #[test]
    fn live_pane_query_returns_none_when_empty_output_confirms_absence() {
        let result =
            classify_live_pane_query("%7", Ok(String::new()), || Ok("%1\n%3".to_string())).unwrap();

        assert!(result.is_none());
    }

    #[test]
    fn live_pane_query_preserves_malformed_error_when_listing_finds_pane() {
        let error = classify_live_pane_query("%7", Ok("unexpected output".to_string()), || {
            Ok("%1\n%7".to_string())
        })
        .unwrap_err();

        assert!(error.to_string().contains("exists"));
        assert!(format!("{error:#}").contains("malformed live pane information"));
    }

    #[test]
    fn live_pane_query_preserves_malformed_error_when_confirmation_fails() {
        let error =
            classify_live_pane_query("%7", Ok(String::new()), || Err(anyhow!("listing failed")))
                .unwrap_err();

        assert!(error.to_string().contains("failed to confirm"));
    }

    #[test]
    fn live_pane_query_rejects_malformed_confirmation_output() {
        let error = classify_live_pane_query("%7", Err(anyhow!("query failed")), || {
            Ok("not-a-pane-id".to_string())
        })
        .unwrap_err();

        assert!(error.to_string().contains("malformed pane identifier"));
    }

    #[test]
    fn auto_clear_status_hook_signals_daemon_after_clearing_status() {
        let hook = auto_clear_status_hook();
        let clear_window = hook.find("set-option -uw @workmux_status").unwrap();
        let clear_pane = hook.find("set-option -up @workmux_pane_status").unwrap();
        let clear_ack = hook.find("set-option -up @workmux_ack_status").unwrap();
        let signal_daemon = hook.find("kill -USR1").unwrap();

        assert!(hook.contains(
            "#{&&:#{!=:#{@workmux_ack_status},},#{==:#{@workmux_status},#{@workmux_ack_status}}}"
        ));
        assert!(clear_window < clear_pane);
        assert!(clear_pane < clear_ack);
        assert!(clear_ack < signal_daemon);
    }

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
