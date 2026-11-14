//! Manual sidebar resizes on tmux.
//!
//! tmux lets the user drag a sidebar's border. A sidebar client notices the
//! resize, waits for it to settle, then publishes the new size and reflows the
//! sidebars in other windows. Zellij keeps a resized sidebar as is, and the Rex
//! daemon sizes sidebars itself, so only tmux sidebars track resizes.

use std::time::{Duration, Instant};

use crate::config::{Config, SidebarPosition};

use super::super::host::TmuxHost;
use super::super::width_exceeds_defensive_max;
use super::{
    SIDEBAR_APPLIED_SIZE_OPTION, effective_height_for, effective_width_for, reflow,
    reflow_all_sidebars_except, set_sidebar_height, set_sidebar_width,
};

/// How long a resize must stay still before the sidebar acts on it.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Tracks one sidebar pane's manual resizes.
pub(crate) struct TmuxResize {
    tmux: TmuxHost,
    /// Last known window width (for detecting manual pane resizes).
    last_window_width: Option<u16>,
    /// Last known window height (for detecting manual top bar resizes).
    last_window_height: Option<u16>,
    /// Pending resize columns to process after debounce.
    pending_cols: Option<u16>,
    /// Pending resize rows to process after debounce.
    pending_rows: Option<u16>,
    /// Deadline after which the pending resize is processed.
    deadline: Option<Instant>,
    /// Ignore the next resize event, which a reflow this sidebar requested
    /// causes.
    suppress_once: bool,
}

impl TmuxResize {
    pub(crate) fn new(tmux: TmuxHost) -> Self {
        // Seed the window size so the first resize event after startup grace
        // can be compared against a baseline.
        let last_window_width = query_u16(&tmux, "#{window_width}");
        let last_window_height = query_u16(&tmux, "#{window_height}");
        Self {
            tmux,
            last_window_width,
            last_window_height,
            pending_cols: None,
            pending_rows: None,
            deadline: None,
            suppress_once: false,
        }
    }

    /// When the pending resize is due, if one is pending.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn clear_pending(&mut self) {
        self.pending_cols = None;
        self.pending_rows = None;
        self.deadline = None;
    }

    /// Record a resize event for debounced processing.
    pub(crate) fn on_resize_event(&mut self, position: SidebarPosition, cols: u16, rows: u16) {
        if self.suppress_once {
            self.suppress_once = false;
            self.clear_pending();
            return;
        }

        match position {
            SidebarPosition::Left => {
                let window_w = self.window_width();
                if self.last_window_width.is_some_and(|prev| prev != window_w) {
                    self.last_window_width = Some(window_w);
                    self.clear_pending();
                    return;
                }
                self.pending_cols = Some(cols);
            }
            SidebarPosition::Top => {
                let window_h = self.window_height();
                if self.last_window_height.is_some_and(|prev| prev != window_h) {
                    self.last_window_height = Some(window_h);
                    self.clear_pending();
                    return;
                }
                self.pending_rows = Some(rows);
            }
        }

        self.deadline = Some(Instant::now() + RESIZE_DEBOUNCE);
    }

    /// Process any pending resize after the debounce period has elapsed.
    /// Skips detection during the startup grace period.
    pub(crate) fn process_pending(
        &mut self,
        position: SidebarPosition,
        window_id: Option<&str>,
        startup: &Instant,
        startup_grace: Duration,
    ) {
        if startup.elapsed() < startup_grace {
            // Suppress detection during startup to avoid false positives from
            // initial pane creation layout divergence.
            self.clear_pending();
            return;
        }

        let Some(deadline) = self.deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }

        let config = Config::load(None).unwrap_or_default();
        let tmux = self.tmux.clone();
        match position {
            SidebarPosition::Left => {
                let Some(pane_width) = self.pending_cols else {
                    self.deadline = None;
                    return;
                };
                let window_w = self.window_width();
                let prev_window_w = self.last_window_width.replace(window_w);
                self.clear_pending();
                if prev_window_w != Some(window_w) {
                    return;
                }
                let actual_width = self.pane_extent("#{pane_width}").unwrap_or(pane_width);
                let expected = effective_width_for(&tmux, &config, window_w);
                if actual_width == expected {
                    return;
                }
                if self.was_applied_by_reflow(actual_width) {
                    // A reflow raced with a newer synced width. Catch up
                    // locally without publishing a size the user never chose.
                    if let Some(wid) = window_id {
                        let _ = reflow(&tmux, Some(wid));
                    }
                } else if width_exceeds_defensive_max(actual_width) {
                    if let Some(wid) = window_id {
                        set_sidebar_width(&tmux, expected);
                        self.suppress_once = true;
                        let _ = reflow(&tmux, Some(wid));
                    }
                } else if config.sidebar.width.is_none() {
                    set_sidebar_width(&tmux, actual_width);
                    self.record_applied_size(actual_width);
                    if let Some(wid) = window_id {
                        reflow_all_sidebars_except(&tmux, wid);
                    }
                } else if let Some(wid) = window_id {
                    self.suppress_once = true;
                    let _ = reflow(&tmux, Some(wid));
                }
            }
            SidebarPosition::Top => {
                let Some(pane_height) = self.pending_rows else {
                    self.deadline = None;
                    return;
                };
                let window_h = self.window_height();
                let prev_window_h = self.last_window_height.replace(window_h);
                self.clear_pending();
                if prev_window_h != Some(window_h) {
                    return;
                }
                let actual_height = self.pane_extent("#{pane_height}").unwrap_or(pane_height);
                let expected = effective_height_for(&tmux, &config, window_h);
                if actual_height == expected {
                    return;
                }
                if self.was_applied_by_reflow(actual_height) {
                    if let Some(wid) = window_id {
                        let _ = reflow(&tmux, Some(wid));
                    }
                } else if config.sidebar.height.is_none() {
                    set_sidebar_height(&tmux, actual_height);
                    self.record_applied_size(actual_height);
                    if let Some(wid) = window_id {
                        reflow_all_sidebars_except(&tmux, wid);
                    }
                } else if let Some(wid) = window_id {
                    self.suppress_once = true;
                    let _ = reflow(&tmux, Some(wid));
                }
            }
        }
    }

    fn window_width(&self) -> u16 {
        query_u16(&self.tmux, "#{window_width}").unwrap_or(0)
    }

    fn window_height(&self) -> u16 {
        query_u16(&self.tmux, "#{window_height}").unwrap_or(0)
    }

    /// The sidebar pane's actual extent. Used to verify the size after a
    /// manual resize, since crossterm's SIGWINCH-derived size may differ from
    /// what tmux reports.
    fn pane_extent(&self, format: &str) -> Option<u16> {
        query_u16(&self.tmux, format).filter(|&extent| extent > 0)
    }

    /// Whether the sidebar's current extent is the one a layout reflow gave it.
    ///
    /// Every sidebar sees the resizes that propagate a new size to it. Treating
    /// those as user resizes would make each sidebar publish its own size and
    /// reflow the others, which keeps sidebars resizing each other indefinitely
    /// when their checks overlap.
    fn was_applied_by_reflow(&self, extent: u16) -> bool {
        let format = format!("#{{{}}}", SIDEBAR_APPLIED_SIZE_OPTION);
        query_u16(&self.tmux, &format) == Some(extent)
    }

    /// Record a user-chosen extent as applied, so a later resize back to the
    /// previous size still counts as a user resize.
    fn record_applied_size(&self, extent: u16) {
        let pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
        if pane_id.is_empty() {
            return;
        }
        let _ = self
            .tmux
            .cmd()
            .args(&[
                "set-option",
                "-p",
                "-t",
                &pane_id,
                SIDEBAR_APPLIED_SIZE_OPTION,
                &extent.to_string(),
            ])
            .run();
    }
}

/// Expand a format for this sidebar's pane.
fn query_format(tmux: &TmuxHost, format: &str) -> Option<String> {
    let pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
    let mut args = vec!["display-message", "-p"];
    if !pane_id.is_empty() {
        args.extend_from_slice(&["-t", &pane_id]);
    }
    args.push(format);
    tmux.cmd()
        .args(&args)
        .run_and_capture_stdout()
        .ok()
        .map(|s| s.trim().to_string())
}

fn query_u16(tmux: &TmuxHost, format: &str) -> Option<u16> {
    query_format(tmux, format).and_then(|s| s.parse().ok())
}
