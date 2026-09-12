//! Unified handle for multiplexer targets (windows or sessions).
//!
//! Centralizes all mode-dependent dispatch so callers don't need
//! `if mode == Session { ... } else { ... }` branches.

use anyhow::Result;
use std::time::Duration;

use crate::config::MuxMode;

use super::Multiplexer;
use super::types::WindowTarget;
use super::util;

/// Returns "window" or "session" for a given mode.
pub fn mode_label(mode: MuxMode) -> &'static str {
    match mode {
        MuxMode::Window => "window",
        MuxMode::Session => "session",
    }
}

/// A unified handle for a multiplexer target (window or session).
///
/// Wraps a reference to the backend, the mode, prefix, and handle name,
/// then dispatches to the correct window or session methods.
pub struct MuxHandle<'a> {
    mux: &'a dyn Multiplexer,
    mode: MuxMode,
    prefix: &'a str,
    name: &'a str,
}

impl<'a> MuxHandle<'a> {
    pub fn new(mux: &'a dyn Multiplexer, mode: MuxMode, prefix: &'a str, name: &'a str) -> Self {
        Self {
            mux,
            mode,
            prefix,
            name,
        }
    }

    /// Returns "window" or "session".
    pub fn kind(&self) -> &'static str {
        mode_label(self.mode)
    }

    pub fn is_session(&self) -> bool {
        self.mode == MuxMode::Session
    }

    /// The prefixed name (e.g., "wm-feature-auth").
    pub fn full_name(&self) -> String {
        util::prefixed(self.prefix, self.name)
    }

    /// Check if the target exists.
    pub fn exists(&self) -> Result<bool> {
        let full = self.full_name();
        match self.mode {
            MuxMode::Session => self.mux.session_exists(&full),
            MuxMode::Window => self.mux.window_exists(self.prefix, self.name),
        }
    }

    /// Check if a target exists by its full name (including prefix).
    /// Useful when the full name was obtained from current_name() or similar.
    pub fn exists_full(mux: &dyn Multiplexer, mode: MuxMode, full_name: &str) -> Result<bool> {
        match mode {
            MuxMode::Session => mux.session_exists(full_name),
            MuxMode::Window => mux.window_exists_by_full_name(full_name),
        }
    }

    /// Activate (focus/switch to) the target.
    pub fn select(&self) -> Result<()> {
        match self.mode {
            MuxMode::Session => self.mux.switch_to_session(self.prefix, self.name),
            MuxMode::Window => self.mux.select_window(self.prefix, self.name),
        }
    }

    /// Kill a target by its full name.
    pub fn kill_full(mux: &dyn Multiplexer, mode: MuxMode, full_name: &str) -> Result<()> {
        match mode {
            MuxMode::Session => mux.kill_session(full_name),
            MuxMode::Window => mux.kill_window(full_name),
        }
    }

    pub fn kill_window_target(mux: &dyn Multiplexer, target: &WindowTarget) -> Result<()> {
        mux.kill_window_target(target)
    }

    /// Schedule a target to close after a delay, by full name.
    pub fn schedule_close_full(
        mux: &dyn Multiplexer,
        mode: MuxMode,
        full_name: &str,
        delay: Duration,
        session_destination: Option<&str>,
    ) -> Result<()> {
        match mode {
            MuxMode::Session => {
                mux.schedule_session_close_to(full_name, session_destination, delay)
            }
            MuxMode::Window => mux.schedule_window_close(full_name, delay),
        }
    }

    pub fn schedule_window_target_close(
        mux: &dyn Multiplexer,
        target: &WindowTarget,
        delay: Duration,
    ) -> Result<()> {
        mux.schedule_window_target_close(target, delay)
    }

    /// Get the current target name (session name or window name).
    pub fn current_name(&self) -> Result<Option<String>> {
        match self.mode {
            MuxMode::Session => Ok(self.mux.current_session()),
            MuxMode::Window => self.mux.current_window_name(),
        }
    }

    /// Generate a shell command to kill a target by full name (for deferred scripts).
    pub fn shell_kill_cmd_full(
        mux: &dyn Multiplexer,
        mode: MuxMode,
        full_name: &str,
    ) -> Result<String> {
        match mode {
            MuxMode::Session => mux.shell_kill_session_cmd(full_name),
            MuxMode::Window => mux.shell_kill_window_cmd(full_name),
        }
    }

    pub fn shell_kill_window_target_cmd(
        mux: &dyn Multiplexer,
        target: &WindowTarget,
    ) -> Result<String> {
        mux.shell_kill_window_target_cmd(target)
    }

    /// Generate a shell command to select a target by full name (for deferred scripts).
    pub fn shell_select_cmd_full(
        mux: &dyn Multiplexer,
        mode: MuxMode,
        full_name: &str,
    ) -> Result<String> {
        match mode {
            MuxMode::Session => mux.shell_switch_session_cmd(full_name),
            MuxMode::Window => mux.shell_select_window_cmd(full_name),
        }
    }
}
