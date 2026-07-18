//! Multiplexer abstraction layer for terminal multiplexer backends.
//!
//! This module provides a trait-based abstraction that allows workmux to work
//! with different terminal multiplexers (tmux, WezTerm) interchangeably.

pub mod agent;
pub mod conversation;
pub mod handle;
pub mod handshake;
pub mod kitty;
pub mod tmux;
pub mod types;
pub mod util;
pub mod wezterm;
pub mod zellij;

use anyhow::{Context, Result, anyhow};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub use handle::MuxHandle;
pub use handshake::PaneHandshake;
pub use tmux::TmuxBackend;
pub use types::*;

use crate::config::{Config, PaneConfig, SplitDirection};

pub const STATUS_TARGET_BACKEND_ENV: &str = "WORKMUX_STATUS_BACKEND";
pub const STATUS_TARGET_INSTANCE_ENV: &str = "WORKMUX_STATUS_INSTANCE";
pub const STATUS_TARGET_PANE_ENV: &str = "WORKMUX_STATUS_PANE_ID";

fn command_with_status_target(
    command: &str,
    backend: &str,
    instance: &str,
    pane_id: &str,
) -> String {
    format!(
        "env {}={} {}={} {}={} {}",
        STATUS_TARGET_BACKEND_ENV,
        agent::shell_quote(backend),
        STATUS_TARGET_INSTANCE_ENV,
        agent::shell_quote(instance),
        STATUS_TARGET_PANE_ENV,
        agent::shell_quote(pane_id),
        command
    )
}

/// Main trait for terminal multiplexer backends.
///
/// Implementations must be Send + Sync to allow sharing via Arc<dyn Multiplexer>.
pub trait Multiplexer: Send + Sync {
    /// Returns the name of this backend (e.g., "tmux", "wezterm")
    fn name(&self) -> &'static str;

    // === Server/Session ===

    /// Check if the multiplexer server is running
    fn is_running(&self) -> Result<bool>;

    fn current_window_id(&self) -> Result<Option<String>> {
        Ok(None)
    }

    fn rightmost_window_id(&self) -> Result<Option<String>> {
        Ok(None)
    }

    fn current_session_id(&self) -> Result<Option<String>> {
        Ok(None)
    }

    fn shell_close_window_by_id_guard_cmd(&self, id: &str) -> Result<String> {
        let _ = id;
        Err(anyhow!(
            "Closing windows by stable ID is not supported by the {} backend",
            self.name()
        ))
    }

    fn shell_close_session_by_id_guard_cmd(&self, id: &str) -> Result<String> {
        let _ = id;
        Err(anyhow!(
            "Closing sessions by stable ID is not supported by the {} backend",
            self.name()
        ))
    }

    /// Get the current pane ID from environment (TMUX_PANE or WEZTERM_PANE)
    fn current_pane_id(&self) -> Option<String>;

    /// Query the active pane ID directly from the multiplexer.
    /// More reliable than current_pane_id() in run-shell contexts (keybindings)
    /// where the env var may be stale or missing.
    fn active_pane_id(&self) -> Option<String>;

    /// Get the working directory of the active pane in the current client's session
    fn get_client_active_pane_path(&self) -> Result<PathBuf>;

    // === Window/Tab Management ===

    /// Create a new window/tab with the given parameters.
    /// Returns: Window identifier (pane ID for tmux/WezTerm, tab name for Zellij)
    fn create_window(&self, params: CreateWindowParams) -> Result<String>;

    /// Create a new session with the given parameters.
    /// Returns the initial pane ID of the new session.
    /// For backends that don't support sessions (e.g., WezTerm), this may create a workspace.
    fn create_session(&self, params: CreateSessionParams) -> Result<String>;

    /// Create a new window within an existing session.
    /// Returns the pane ID of the new window's initial pane.
    /// Only supported by backends with session support (tmux).
    fn create_window_in_session(&self, params: CreateWindowInSessionParams) -> Result<String> {
        let _ = params;
        Err(anyhow!(
            "Multi-window sessions are not supported by the {} backend",
            self.name()
        ))
    }

    /// Switch to a session by prefix and name.
    /// For tmux, this switches the client to the session.
    /// For WezTerm, this may switch to a workspace.
    fn switch_to_session(&self, prefix: &str, name: &str) -> Result<()>;

    /// Check if a session exists by its full name.
    fn session_exists(&self, _full_name: &str) -> Result<bool> {
        Ok(false)
    }

    /// Kill a session by its full name (including prefix).
    fn kill_session(&self, _full_name: &str) -> Result<()> {
        Ok(())
    }

    /// Kill a window by its full name (including prefix)
    fn kill_window(&self, full_name: &str) -> Result<()>;

    fn kill_window_target(&self, target: &WindowTarget) -> Result<()> {
        self.kill_window(&target.full_name)
    }

    /// Rename a window from its full name to a new full name.
    ///
    /// Default implementation returns an error. Backends that support
    /// rename (tmux) override this.
    fn rename_window(&self, old_full_name: &str, new_full_name: &str) -> Result<()> {
        let _ = (old_full_name, new_full_name);
        Err(anyhow!(
            "Renaming windows is not supported by the {} backend",
            self.name()
        ))
    }

    /// Rename a session from its full name to a new full name.
    ///
    /// Default implementation returns an error. Backends that support
    /// rename (tmux) override this.
    fn rename_session(&self, old_full_name: &str, new_full_name: &str) -> Result<()> {
        let _ = (old_full_name, new_full_name);
        Err(anyhow!(
            "Renaming sessions is not supported by the {} backend",
            self.name()
        ))
    }

    /// Schedule a window to close after a delay
    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()>;

    fn schedule_window_target_close(&self, target: &WindowTarget, delay: Duration) -> Result<()> {
        self.schedule_window_close(&target.full_name, delay)
    }

    /// Schedule a session to close after a delay
    fn schedule_session_close(&self, full_name: &str, delay: Duration) -> Result<()>;

    /// Run a deferred script in the background (for cleanup operations).
    /// For tmux, this uses `run-shell`. For other backends, may use different mechanisms.
    fn run_deferred_script(&self, script: &str) -> Result<()>;

    /// Generate a shell command string to select/focus a window by full name.
    /// Used in deferred scripts that run asynchronously via `run_deferred_script`.
    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String>;

    /// Generate a shell command string to close/kill a window by full name.
    /// Used in deferred scripts that run asynchronously via `run_deferred_script`.
    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String>;

    fn shell_kill_window_target_cmd(&self, target: &WindowTarget) -> Result<String> {
        self.shell_kill_window_cmd(&target.full_name)
    }

    /// Generate a shell command string to switch to a session by full name.
    /// Used in deferred scripts that run asynchronously via `run_deferred_script`.
    fn shell_switch_session_cmd(&self, full_name: &str) -> Result<String>;

    /// Generate a shell command string to kill a session by full name.
    /// Used in deferred scripts that run asynchronously via `run_deferred_script`.
    fn shell_kill_session_cmd(&self, full_name: &str) -> Result<String>;

    /// Generate a shell command string to switch to the last/previous session.
    /// Used in deferred scripts before killing the current session so the client
    /// returns to the session the user was on previously.
    fn shell_switch_to_last_session_cmd(&self) -> Result<String> {
        Err(anyhow!(
            "shell_switch_to_last_session_cmd not supported by {} backend",
            self.name()
        ))
    }

    /// Select (focus) a window by prefix and name
    fn select_window(&self, prefix: &str, name: &str) -> Result<()>;

    fn select_window_target(&self, target: &WindowTarget) -> Result<()> {
        let _ = target;
        Err(anyhow!(
            "Selecting parent-qualified windows is not supported by the {} backend",
            self.name()
        ))
    }

    /// Check if a window exists by prefix and name
    fn window_exists(&self, prefix: &str, name: &str) -> Result<bool> {
        let full_name = util::prefixed(prefix, name);
        self.window_exists_by_full_name(&full_name)
    }

    /// Check if a window exists by its full name
    fn window_exists_by_full_name(&self, full_name: &str) -> Result<bool> {
        let names = self.get_all_window_names()?;
        Ok(names.contains(full_name))
    }

    fn window_target_exists(&self, target: &WindowTarget) -> Result<bool> {
        self.window_exists_by_full_name(&target.full_name)
    }

    /// Get the current window name, if running inside the multiplexer
    fn current_window_name(&self) -> Result<Option<String>>;

    /// Get all window names in the current session
    fn get_all_window_names(&self) -> Result<HashSet<String>>;

    fn get_window_names_in_session(&self, session_name: &str) -> Result<HashSet<String>> {
        let _ = session_name;
        self.get_all_window_names()
    }

    fn get_all_windows_with_sessions(&self) -> Result<HashSet<(String, String)>> {
        let session = self.current_session().unwrap_or_default();
        Ok(self
            .get_all_window_names()?
            .into_iter()
            .map(|window| (window, session.clone()))
            .collect())
    }

    /// Get all session names
    fn get_all_session_names(&self) -> Result<HashSet<String>> {
        Ok(HashSet::new())
    }

    /// Filter a list of window names, returning only those that still exist
    fn filter_active_windows(&self, windows: &[String]) -> Result<Vec<String>> {
        let all_current = self.get_all_window_names()?;

        Ok(windows
            .iter()
            .filter(|w| all_current.contains(*w))
            .cloned()
            .collect())
    }

    /// Wait until all specified windows are closed
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

    /// Wait until the specified session is closed
    fn wait_until_session_closed(&self, full_session_name: &str) -> Result<()>;

    // === Pane Management ===

    /// Select (focus) a pane by ID
    fn select_pane(&self, pane_id: &str) -> Result<()>;

    /// Zoom (fullscreen) a pane by ID.
    /// Only supported by tmux. Other backends silently ignore this.
    fn zoom_pane(&self, _pane_id: &str) -> Result<()> {
        Ok(())
    }

    /// Switch to a pane (may also switch windows/tabs as needed).
    ///
    /// `window_hint` provides the window/tab name for backends that need it
    /// (e.g., Zellij can't look up a pane by ID alone). Backends that can
    /// switch by pane ID directly (tmux, WezTerm) ignore this parameter.
    fn switch_to_pane(&self, pane_id: &str, window_hint: Option<&str>) -> Result<()>;

    /// Whether jumping to a pane should exit the dashboard.
    /// Defaults to true. Override to return false to keep the dashboard open after jumping.
    fn should_exit_on_jump(&self) -> bool {
        true
    }

    /// Kill a pane by its ID. If this is the last pane in a window/tab,
    /// the window closes automatically.
    fn kill_pane(&self, pane_id: &str) -> Result<()>;

    /// Respawn a pane with optional command. Returns the (possibly new) pane ID.
    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String>;

    /// Set a user-facing display name for a pane.
    ///
    /// Backends that do not support stable pane names can ignore this.
    fn set_pane_name(&self, _pane_id: &str, _name: &str) -> Result<()> {
        Ok(())
    }

    /// Capture the content of a pane
    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String>;

    /// Whether this backend supports preview capture efficiently.
    /// Defaults to true. Override to return false for backends where preview capture
    /// requires expensive operations (process spawning, temp files).
    fn supports_preview(&self) -> bool {
        true
    }

    // === Text I/O ===

    /// Send a text fragment to a pane without submitting it.
    fn send_text_fragment(&self, pane_id: &str, text: &str) -> Result<()>;

    /// Submit the current pane input (Enter).
    fn send_enter(&self, pane_id: &str) -> Result<()>;

    /// Send keys (command + Enter) to a pane
    fn send_keys(&self, pane_id: &str, command: &str) -> Result<()> {
        self.send_text_fragment(pane_id, command)?;
        self.send_enter(pane_id)
    }

    /// Whether this backend requires focusing a pane before sending input to it.
    /// Defaults to false. Backends like Zellij that can't target unfocused panes
    /// override this to return true.
    fn requires_focus_for_input(&self) -> bool {
        false
    }

    /// Send keys to an agent pane, with special handling for Claude's ! prefix
    fn send_keys_to_agent(&self, pane_id: &str, command: &str, agent: Option<&str>) -> Result<()> {
        if agent::resolve_profile(agent).needs_bang_delay() && command.starts_with('!') {
            self.send_text_fragment(pane_id, "!")?;
            thread::sleep(Duration::from_millis(50));
            self.send_text_fragment(pane_id, &command[1..])?;
            self.send_enter(pane_id)
        } else {
            self.send_keys(pane_id, command)
        }
    }

    /// Send a single key to a pane
    fn send_key(&self, pane_id: &str, key: &str) -> Result<()>;

    /// Paste text to a pane.
    fn paste_text(&self, pane_id: &str, content: &str) -> Result<()>;

    /// Paste multiline content to a pane (using bracketed paste)
    fn paste_multiline(&self, pane_id: &str, content: &str) -> Result<()> {
        self.paste_text(pane_id, content)?;
        thread::sleep(Duration::from_millis(100));
        self.send_enter(pane_id)
    }

    /// Clear the pane screen. Default is no-op; backends override if needed.
    fn clear_pane(&self, _pane_id: &str) -> Result<()> {
        Ok(())
    }

    // === Shell ===

    /// Fallback shell when `$SHELL` is unset.
    fn default_shell_fallback(&self) -> &'static str {
        "/bin/bash"
    }

    /// Get the default shell for new panes
    fn get_default_shell(&self) -> Result<String> {
        util::default_shell(self.default_shell_fallback())
    }

    /// Create a handshake mechanism for synchronizing shell startup
    fn create_handshake(&self) -> Result<Box<dyn PaneHandshake>> {
        util::unix_pipe_handshake()
    }

    // === Status ===

    /// Set status icon for a pane.
    ///
    /// If `auto_clear_on_focus` is true, the status will be automatically cleared
    /// when the window receives focus (used for "waiting" and "done" statuses).
    fn set_status(&self, pane_id: &str, icon: &str, auto_clear_on_focus: bool) -> Result<()>;

    /// Clear status from a pane
    fn clear_status(&self, pane_id: &str) -> Result<()>;

    /// Ensure the status format is configured (for backends that need it)
    fn ensure_status_format(&self, pane_id: &str) -> Result<()>;

    // === Pane Setup ===

    /// Split a pane, returning the new pane ID.
    /// Returns: Pane identifier (accurate for tmux/WezTerm, tab name for Zellij)
    fn split_pane(
        &self,
        target_pane_id: &str,
        direction: &SplitDirection,
        cwd: &Path,
        size: Option<u16>,
        percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String>;

    /// Setup panes in a window according to configuration.
    ///
    /// Default implementation handles the full orchestration: command resolution,
    /// handshake-based shell synchronization, command injection, and auto-status.
    /// Backends only need to implement `respawn_pane`, `split_pane`, and other
    /// primitive trait methods.
    fn setup_panes(
        &self,
        initial_pane_id: &str,
        panes: &[PaneConfig],
        working_dir: &Path,
        options: PaneSetupOptions<'_>,
        config: &Config,
        task_agent: Option<&str>,
    ) -> Result<PaneSetupResult> {
        if panes.is_empty() {
            return Ok(PaneSetupResult {
                focus_pane_id: initial_pane_id.to_string(),
                zoom_pane_id: None,
            });
        }

        let mut focus_pane_id: Option<String> = None;
        let mut zoom_pane_id: Option<String> = None;
        let mut pane_ids: Vec<String> = vec![initial_pane_id.to_string()];
        let shell = self.get_default_shell()?;

        for (i, pane_config) in panes.iter().enumerate() {
            let is_first = i == 0;

            // Skip non-first panes that have no split direction
            if !is_first && pane_config.split.is_none() {
                continue;
            }

            // Resolve command: handle <agent> placeholders and prompt injection
            let adjusted_command = util::resolve_pane_command_with_config(
                pane_config.command.as_deref(),
                options.run_commands,
                options.prompt_file_path,
                working_dir,
                config,
                task_agent,
                &shell,
            );

            let pane_id = if let Some(mut resolved) = adjusted_command {
                let is_agent_pane = resolved.selected_agent.is_some();

                // Spawn with handshake so we can send the command after shell is ready
                let handshake = self.create_handshake()?;
                let script = handshake.script_content(&shell);

                let spawned_id = if is_first {
                    self.respawn_pane(&pane_ids[0], working_dir, Some(&script))?
                } else {
                    let direction = pane_config.split.as_ref().unwrap();
                    let target_idx = pane_config.target.unwrap_or(pane_ids.len() - 1);
                    let target = pane_ids
                        .get(target_idx)
                        .ok_or_else(|| anyhow!("Invalid target pane index: {}", target_idx))?;
                    self.split_pane(
                        target,
                        direction,
                        working_dir,
                        pane_config.size,
                        pane_config.percentage,
                        Some(&script),
                    )?
                };

                handshake.wait()?;

                // Inject resume/continue arguments for agent panes when requested
                if let Some(selected_agent) = resolved.selected_agent.as_mut() {
                    match &options.resume_mode {
                        crate::multiplexer::types::ResumeMode::Continue => {
                            let profile = selected_agent.profile;
                            if let Some(flag) = profile.continue_flag() {
                                selected_agent.command.append_args_fragment(flag);
                            } else {
                                tracing::warn!(
                                    agent = profile.name(),
                                    "agent does not support --continue, flag ignored"
                                );
                            }
                        }
                        crate::multiplexer::types::ResumeMode::ForkSession(session_id) => {
                            let agent_name = selected_agent.kind();
                            if let Some(forker) =
                                crate::multiplexer::conversation::resolve_forker(agent_name)
                            {
                                let resume_args = forker.resume_args(session_id);
                                selected_agent.command.args.extend(resume_args);
                            } else {
                                tracing::warn!(
                                    agent = agent_name,
                                    "agent does not support forking, resume flag ignored"
                                );
                            }
                        }
                        crate::multiplexer::types::ResumeMode::None => {}
                    }
                }

                // Apply sandbox wrapping if enabled for this pane type
                let final_command = if config.sandbox.is_enabled() {
                    let should_wrap = match config.sandbox.target() {
                        crate::config::SandboxTarget::All => true,
                        crate::config::SandboxTarget::Agent => is_agent_pane,
                    };
                    if should_wrap {
                        // Use worktree_root for mounting, working_dir for cwd
                        let wt_root = options.worktree_root.unwrap_or(working_dir);

                        // Inject skip-permissions flag for agent panes only
                        // (sandbox provides the security boundary, so permission
                        // prompts are unnecessary and break autonomous workflow)
                        let command_to_wrap =
                            if let Some(selected_agent) = resolved.selected_agent.as_mut() {
                                if let Some(flag) = selected_agent.profile.skip_permissions_flag() {
                                    selected_agent.command.prepend_args_fragment(flag);
                                }
                                resolved.render_command()
                            } else {
                                resolved.command.clone()
                            };

                        // Choose backend based on config
                        let wrap_result = match config.sandbox.backend() {
                            crate::config::SandboxBackend::Container => {
                                crate::sandbox::wrap_for_container(
                                    &command_to_wrap,
                                    &config.sandbox,
                                    wt_root,
                                    working_dir,
                                )
                            }
                            crate::config::SandboxBackend::Lima => {
                                let vm_name = options.lima_vm_name.ok_or_else(|| {
                                    anyhow!(
                                        "Lima VM name missing despite sandbox wrap request. \
                                         This is a bug in workmux."
                                    )
                                })?;
                                crate::sandbox::wrap_for_lima(
                                    &command_to_wrap,
                                    config,
                                    vm_name,
                                    working_dir,
                                )
                            }
                        };

                        // Fail closed: if sandbox is enabled but wrapping fails, don't fall back to unsandboxed
                        match wrap_result {
                            Ok(wrapped) => wrapped,
                            Err(e) => {
                                return Err(anyhow!(
                                    "Sandbox is enabled but failed to wrap command: {}. \
                                     To disable sandbox, set 'sandbox.enabled: false' in config.",
                                    e
                                ));
                            }
                        }
                    } else {
                        resolved.render_command()
                    }
                } else {
                    resolved.render_command()
                };

                let final_command = if is_agent_pane && self.name() == "zellij" {
                    command_with_status_target(
                        &final_command,
                        self.name(),
                        &self.instance_id(),
                        &spawned_id,
                    )
                } else {
                    final_command
                };

                let _ = self.clear_pane(&spawned_id);
                self.send_keys(&spawned_id, &final_command)?;

                // Set working status for agent panes with injected prompts
                if resolved.prompt_injected
                    && resolved
                        .selected_agent
                        .as_ref()
                        .is_some_and(|agent| agent.profile.needs_auto_status())
                {
                    let icon = config.status_icons.working();
                    if config.status_format.unwrap_or(true) {
                        let _ = self.ensure_status_format(&spawned_id);
                    }
                    let _ = self.set_status(&spawned_id, icon, false);
                }

                spawned_id
            } else if is_first {
                // No command for first pane - keep as-is
                pane_ids[0].clone()
            } else {
                // No command - just split
                let direction = pane_config.split.as_ref().unwrap();
                let target_idx = pane_config.target.unwrap_or(pane_ids.len() - 1);
                let target = pane_ids
                    .get(target_idx)
                    .ok_or_else(|| anyhow!("Invalid target pane index: {}", target_idx))?;
                self.split_pane(
                    target,
                    direction,
                    working_dir,
                    pane_config.size,
                    pane_config.percentage,
                    None,
                )?
            };

            if is_first {
                pane_ids[0] = pane_id.clone();
            } else {
                pane_ids.push(pane_id.clone());
            }

            if let Some(name) = pane_config.name.as_deref() {
                self.set_pane_name(&pane_id, name)
                    .with_context(|| format!("Failed to set pane name '{}'", name))?;
            }

            if pane_config.zoom || pane_config.focus {
                focus_pane_id = Some(pane_id.clone());
            }

            if pane_config.zoom {
                zoom_pane_id = Some(pane_id);
            }
        }

        Ok(PaneSetupResult {
            focus_pane_id: focus_pane_id.unwrap_or_else(|| pane_ids[0].clone()),
            zoom_pane_id,
        })
    }

    // === Multi-Session/Workspace Support ===

    /// Get the current session/workspace name, if determinable.
    ///
    /// Returns None if not running inside the multiplexer.
    /// For tmux, this is the session name. For WezTerm, this is the workspace name.
    #[allow(dead_code)] // Reserved for future multi-session features
    fn current_session(&self) -> Option<String> {
        None // Default: can't determine
    }

    /// Get all window names across ALL sessions/workspaces.
    ///
    /// Default implementation returns same as get_all_window_names() (single session).
    /// WezTerm overrides this to return windows from all workspaces.
    #[allow(dead_code)] // Reserved for future multi-session features
    fn get_all_window_names_all_sessions(&self) -> Result<HashSet<String>> {
        self.get_all_window_names()
    }

    // === State Reconciliation ===

    /// Get the backend instance identifier (socket path, mux ID, etc.).
    ///
    /// This is used to create unique state file paths when multiple instances
    /// of the same backend are running (e.g., multiple tmux servers).
    ///
    /// For tmux: socket path or "default" for standard socket
    /// For WezTerm: mux domain ID or workspace name
    fn instance_id(&self) -> String;

    /// Get live pane info including PID and current command.
    ///
    /// Returns None if pane does not exist. Used during state reconciliation
    /// to validate stored state against actual pane state.
    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>>;

    /// Get live pane info for all panes at once (batched query).
    ///
    /// Returns a HashMap from pane_id to LivePaneInfo. This is more efficient
    /// than calling get_live_pane_info repeatedly when validating many panes.
    fn get_all_live_pane_info(&self) -> Result<std::collections::HashMap<String, LivePaneInfo>>;

    /// Get the server's boot identifier for crash detection.
    ///
    /// Returns a stable identifier that changes when the multiplexer server restarts.
    /// Used to distinguish intentional pane closes from server crashes: if a pane's
    /// stored boot_id differs from the current one, the server restarted.
    ///
    /// Default returns None (no crash detection for this backend).
    fn server_boot_id(&self) -> Result<Option<String>> {
        Ok(None)
    }

    /// Validate if an agent is still alive and should be kept in the dashboard.
    ///
    /// Called when a pane is not found in the batched `get_all_live_pane_info()` result.
    /// Backends can implement custom validation logic (e.g., Zellij checks pane existence
    /// and command matching). Default implementation queries the pane individually.
    fn validate_agent_alive(&self, state: &crate::state::AgentState) -> Result<bool> {
        let live_pane = self.get_live_pane_info(&state.pane_key.pane_id)?;

        match live_pane {
            None => Ok(false), // Pane no longer exists
            Some(ref live) if live.pid.is_some_and(|pid| pid != state.pane_pid) => Ok(false), // PID mismatch
            Some(ref live)
                if live
                    .current_command
                    .as_ref()
                    .is_some_and(|cmd| *cmd != state.command) =>
            {
                Ok(false) // Command changed
            }
            Some(_) => Ok(true), // Valid
        }
    }
}

/// Detect which backend to use based on environment.
///
/// Checks `$WORKMUX_BACKEND` first for an explicit override, then auto-detects
/// from multiplexer environment variables. Session-specific variables (set only
/// when inside the multiplexer) are checked before ambient variables (inherited
/// from the parent terminal):
///
/// 1. `$WORKMUX_BACKEND` set → use that backend
/// 2. `$TMUX` set → tmux
/// 3. `$WEZTERM_PANE` set → WezTerm
/// 4. `$ZELLIJ`, `$ZELLIJ_PANE_ID`, or `$ZELLIJ_SESSION_NAME` set → Zellij
/// 5. `$KITTY_WINDOW_ID` set → Kitty
/// 6. None → defaults to tmux (for backward compatibility)
///
/// This ordering ensures that running tmux inside kitty (or wezterm) correctly
/// selects the innermost multiplexer.
pub fn detect_backend() -> BackendType {
    if let Ok(val) = std::env::var("WORKMUX_BACKEND") {
        match val.parse() {
            Ok(bt) => return bt,
            Err(_) => {
                eprintln!(
                    "workmux: invalid WORKMUX_BACKEND={val:?}, expected tmux|wezterm|kitty|zellij"
                );
            }
        }
    }

    resolve_backend(
        std::env::var("TMUX").is_ok(),
        std::env::var("WEZTERM_PANE").is_ok(),
        std::env::var("ZELLIJ").is_ok()
            || std::env::var("ZELLIJ_PANE_ID").is_ok()
            || std::env::var("ZELLIJ_SESSION_NAME").is_ok(),
        std::env::var("KITTY_WINDOW_ID").is_ok(),
    )
}

/// Pure auto-detection logic, separated for testability.
fn resolve_backend(tmux: bool, wezterm: bool, zellij: bool, kitty: bool) -> BackendType {
    if tmux {
        return BackendType::Tmux;
    }

    if wezterm {
        return BackendType::WezTerm;
    }

    if zellij {
        return BackendType::Zellij;
    }

    if kitty {
        return BackendType::Kitty;
    }

    BackendType::Tmux
}

/// Create a backend instance based on the backend type.
pub fn create_backend(backend_type: BackendType) -> Arc<dyn Multiplexer> {
    match backend_type {
        BackendType::Tmux => Arc::new(TmuxBackend::new()),
        BackendType::WezTerm => Arc::new(wezterm::WezTermBackend::new()),
        BackendType::Kitty => Arc::new(kitty::KittyBackend::new()),
        BackendType::Zellij => Arc::new(zellij::ZellijBackend::new()),
    }
}

pub fn create_backend_for_instance(
    backend_type: BackendType,
    instance: &str,
) -> Arc<dyn Multiplexer> {
    match backend_type {
        BackendType::Zellij => Arc::new(zellij::ZellijBackend::for_session(instance)),
        _ => create_backend(backend_type),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_target_command_quotes_identity_values() {
        assert_eq!(
            command_with_status_target(
                "claude --continue",
                "zellij",
                "session with spaces",
                "terminal_7",
            ),
            "env WORKMUX_STATUS_BACKEND=zellij WORKMUX_STATUS_INSTANCE='session with spaces' \
             WORKMUX_STATUS_PANE_ID=terminal_7 claude --continue"
        );
    }

    #[test]
    fn no_env_defaults_to_tmux() {
        assert_eq!(
            resolve_backend(false, false, false, false),
            BackendType::Tmux
        );
    }

    #[test]
    fn tmux_only() {
        assert_eq!(
            resolve_backend(true, false, false, false),
            BackendType::Tmux
        );
    }

    #[test]
    fn wezterm_only() {
        assert_eq!(
            resolve_backend(false, true, false, false),
            BackendType::WezTerm
        );
    }

    #[test]
    fn zellij_only() {
        assert_eq!(
            resolve_backend(false, false, true, false),
            BackendType::Zellij
        );
    }

    #[test]
    fn kitty_only() {
        assert_eq!(
            resolve_backend(false, false, false, true),
            BackendType::Kitty
        );
    }

    #[test]
    fn tmux_inside_kitty() {
        assert_eq!(resolve_backend(true, false, false, true), BackendType::Tmux);
    }

    #[test]
    fn tmux_inside_wezterm() {
        assert_eq!(resolve_backend(true, true, false, false), BackendType::Tmux);
    }

    #[test]
    fn tmux_inside_zellij() {
        assert_eq!(resolve_backend(true, false, true, false), BackendType::Tmux);
    }

    #[test]
    fn wezterm_inside_kitty() {
        assert_eq!(
            resolve_backend(false, true, false, true),
            BackendType::WezTerm
        );
    }

    #[test]
    fn zellij_inside_kitty() {
        assert_eq!(
            resolve_backend(false, false, true, true),
            BackendType::Zellij
        );
    }

    #[test]
    fn all_env_vars_set() {
        assert_eq!(resolve_backend(true, true, true, true), BackendType::Tmux);
    }
}
