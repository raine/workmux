//! Preview pane management for the dashboard.

use std::time::Duration;

use super::super::settings::save_preview_size;
use super::App;
use super::types::AppEvent;

/// Number of lines to capture from the agent's terminal for preview (scrollable history)
pub const PREVIEW_LINES: u16 = 200;

/// Remote previews are fetched at most this often.
const REMOTE_PREVIEW_INTERVAL: Duration = Duration::from_secs(1);
/// A remote preview fetch that has not answered by then no longer holds
/// back the next one.
const REMOTE_PREVIEW_TIMEOUT: Duration = Duration::from_secs(15);

impl App {
    /// Update the preview for the currently selected agent.
    /// Only fetches if the selection has changed or preview is stale.
    pub fn update_preview(&mut self) {
        if !self.mux.supports_preview() {
            return;
        }
        let current_pane_id = self
            .table_state
            .selected()
            .and_then(|idx| self.agents.get(idx))
            .map(|agent| agent.pane_id.clone());

        // Only fetch if selection changed
        if current_pane_id != self.preview_pane_id {
            self.preview_pane_id = current_pane_id.clone();
            self.preview = current_pane_id
                .as_ref()
                .and_then(|pane_id| self.capture_preview(pane_id));
            // Reset scroll position when selection changes
            self.preview_scroll = None;
        }
    }

    /// Force refresh the preview (used on periodic refresh)
    pub fn refresh_preview(&mut self) {
        if !self.mux.supports_preview() {
            return;
        }
        self.preview = self
            .preview_pane_id
            .clone()
            .and_then(|pane_id| self.capture_preview(&pane_id));
    }

    /// The pane's output: captured locally, or for a remote agent no local
    /// pane relays, the last capture its node returned while a new one is
    /// fetched.
    fn capture_preview(&mut self, pane_id: &str) -> Option<String> {
        let detached = self
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id && crate::remote::agents::is_detached(agent))
            .and_then(|agent| agent.remote.clone());
        let Some(remote) = detached else {
            return self.mux.capture_pane(pane_id, PREVIEW_LINES);
        };
        let busy = self.remote_preview_fetch.is_some_and(|(started, running)| {
            started.elapsed()
                < if running {
                    REMOTE_PREVIEW_TIMEOUT
                } else {
                    REMOTE_PREVIEW_INTERVAL
                }
        });
        let placeholder = format!("(fetching from {})", remote.at.remote);
        if !busy {
            self.remote_preview_fetch = Some((std::time::Instant::now(), true));
            let tx = self.event_tx.clone();
            let config = self.config.clone();
            let pane_id = pane_id.to_string();
            std::thread::spawn(move || {
                let capture = crate::remote::agents::capture_from(
                    &config,
                    &remote.at.remote,
                    &remote.at.ws_id,
                    Some(remote.at.pane),
                    PREVIEW_LINES,
                )
                .map_err(|err| format!("{err:#}"));
                let _ = tx.send(AppEvent::RemotePreview(pane_id, capture));
            });
        }
        Some(
            self.remote_previews
                .get(pane_id)
                .cloned()
                .unwrap_or(placeholder),
        )
    }

    pub fn scroll_preview_lines(&mut self, delta: isize) {
        let max_scroll = self.preview_line_count.saturating_sub(self.preview_height);
        let current = self.preview_scroll.unwrap_or(max_scroll);
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs() as u16)
        } else {
            current.saturating_add(delta as u16).min(max_scroll)
        };
        self.preview_scroll = (next < max_scroll).then_some(next);
    }

    /// Scroll preview up (toward older content). Returns the amount to scroll by.
    pub fn scroll_preview_up(&mut self, visible_height: u16, total_lines: u16) {
        let max_scroll = total_lines.saturating_sub(visible_height);
        let current = self.preview_scroll.unwrap_or(max_scroll);
        let half_page = visible_height / 2;
        self.preview_scroll = Some(current.saturating_sub(half_page));
    }

    /// Scroll preview down (toward newer content).
    pub fn scroll_preview_down(&mut self, visible_height: u16, total_lines: u16) {
        let max_scroll = total_lines.saturating_sub(visible_height);
        let current = self.preview_scroll.unwrap_or(max_scroll);
        let half_page = visible_height / 2;
        let new_scroll = (current + half_page).min(max_scroll);
        // If at or past max, return to auto-scroll mode
        if new_scroll >= max_scroll {
            self.preview_scroll = None;
        } else {
            self.preview_scroll = Some(new_scroll);
        }
    }

    /// Increase preview size by 10% (max 90%)
    pub fn increase_preview_size(&mut self) {
        self.preview_size = (self.preview_size + 10).min(90);
        save_preview_size(self.preview_size);
    }

    /// Decrease preview size by 10% (min 10%)
    pub fn decrease_preview_size(&mut self) {
        self.preview_size = self.preview_size.saturating_sub(10).max(10);
        save_preview_size(self.preview_size);
    }
}
