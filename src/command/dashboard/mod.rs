//! Dashboard TUI for monitoring and managing workmux agents.
//!
//! This module provides an interactive terminal UI that displays:
//! - All running agent panes across tmux sessions
//! - Git status for each worktree
//! - Agent status (working/waiting/done) with elapsed time
//! - Live preview of selected agent's terminal output
//!
//! # Module Structure
//!
//! - `app`: Application state and business logic
//! - `actions`: Action enum and dispatcher for all dashboard actions
//! - `agent`: Pure helper functions for agent data extraction
//! - `ansi`: ANSI escape sequence parsing and stripping
//! - `diff`: Diff domain types and helper functions
//! - `keymap`: Key-to-action mapping per context with help text
//! - `settings`: Tmux-persisted dashboard settings
//! - `sort`: Sort mode enum and tmux persistence
//! - `spinner`: Spinner animation constants
//! - `ui/`: TUI rendering modules
//!   - `dashboard`: Table, preview, and footer
//!   - `diff`: Normal diff, patch mode, file list
//!   - `format`: Git status formatting
//!   - `help`: Help overlay

mod actions;
mod agent;
mod ansi;
mod app;
mod diff;
mod diff_ops;
mod keymap;
mod settings;
mod sort;
mod spinner;
mod ui;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use std::io;
use std::time::Duration;

use crate::git;
use crate::github;
use crate::multiplexer::{create_backend, detect_backend};

use self::actions::apply_action;
use self::app::{App, ViewMode};
use self::diff_ops::DiffOps;
use self::keymap::{Context, action_for_key};
use self::spinner::SPINNER_FRAME_COUNT;
use self::ui::ui;

/// Determine the current keymap context based on app state.
fn get_context(app: &App) -> Context {
    match &app.view_mode {
        ViewMode::Dashboard => {
            if app.add_form.is_some() {
                Context::DashboardAdd
            } else if app.input_mode {
                Context::DashboardInput
            } else {
                Context::DashboardNormal
            }
        }
        ViewMode::Diff(diff) => {
            if diff.patch_mode {
                if diff.comment_input.is_some() {
                    Context::Comment
                } else {
                    Context::Patch
                }
            } else {
                Context::DiffNormal
            }
        }
    }
}

/// Handle mouse events for diff view scrolling.
fn handle_mouse_event(app: &mut App, kind: MouseEventKind) {
    if let ViewMode::Diff(ref mut diff_view) = app.view_mode {
        let total_lines = if diff_view.patch_mode {
            diff_view
                .hunks
                .get(diff_view.current_hunk)
                .map(|h| h.parsed_lines.len())
                .unwrap_or(0)
        } else {
            diff_view.line_count
        };

        match kind {
            MouseEventKind::ScrollUp => {
                diff_view.scroll = diff_view.scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                let max_scroll = total_lines.saturating_sub(diff_view.viewport_height as usize);
                diff_view.scroll = (diff_view.scroll + 3).min(max_scroll);
            }
            _ => {}
        }
    }
}

pub fn run(cli_preview_size: Option<u8>, open_diff: bool) -> Result<()> {
    let mux = create_backend(detect_backend());

    // Check if multiplexer is running
    if !mux.is_running().unwrap_or(false) {
        println!("No {} server running.", mux.name());
        return Ok(());
    }

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // Create app state
    let mut app = App::new(mux)?;

    // CLI preview size overrides config/tmux if provided
    if let Some(size) = cli_preview_size {
        app.preview_size = size;
    }

    // Open diff view for current worktree if requested
    if open_diff && let Some(ref current_path) = app.current_worktree {
        // Find the agent matching the current worktree path
        if let Some(idx) = app.agents.iter().position(|a| &a.path == current_path) {
            app.table_state.select(Some(idx));
            app.load_diff(false); // WIP diff (uncommitted changes)
        }
    }

    // Main loop
    let tick_rate = Duration::from_millis(250);
    let mut last_tick = std::time::Instant::now();
    let refresh_interval = Duration::from_secs(2);
    let mut last_refresh = std::time::Instant::now();
    // Preview refreshes more frequently than the agent list
    // Use a faster refresh rate when in input mode for responsive typing feedback
    let preview_refresh_interval_normal = Duration::from_millis(500);
    let preview_refresh_interval_input = Duration::from_millis(100);
    let mut last_preview_refresh = std::time::Instant::now();

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        // Calculate timeout to respect the next scheduled preview refresh
        let current_preview_interval = if app.input_mode {
            preview_refresh_interval_input
        } else {
            preview_refresh_interval_normal
        };
        let time_until_preview =
            current_preview_interval.saturating_sub(last_preview_refresh.elapsed());
        let time_until_tick = tick_rate.saturating_sub(last_tick.elapsed());
        let timeout = time_until_tick.min(time_until_preview);

        if event::poll(timeout)? {
            let event = event::read()?;

            // Handle mouse scroll events in diff view
            if let Event::Mouse(mouse) = &event {
                handle_mouse_event(&mut app, mouse.kind);
                continue;
            }

            // Handle key events
            let Event::Key(key) = event else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            // Help overlay handling - close on any key if open
            if app.show_help {
                app.show_help = false;
                continue;
            }

            // Get current context and map key to action
            let ctx = get_context(&app);

            // Special case: EnterPatchMode only works in WIP diff view (not branch diff)
            if ctx == Context::DiffNormal
                && let ViewMode::Diff(ref diff) = app.view_mode
                && diff.is_branch_diff
            {
                // Skip patch mode action for branch diffs
                if let Some(actions::Action::EnterPatchMode) = action_for_key(ctx, key) {
                    continue;
                }
            }

            if let Some(action) = action_for_key(ctx, key) {
                let refreshed_preview = apply_action(&mut app, action);
                if refreshed_preview {
                    last_preview_refresh = std::time::Instant::now();
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = std::time::Instant::now();
            // Advance spinner animation frame (wrap at frame count to avoid skip artifact)
            app.spinner_frame = (app.spinner_frame + 1) % SPINNER_FRAME_COUNT;
        }

        // Auto-refresh agent list every 2 seconds
        if last_refresh.elapsed() >= refresh_interval {
            app.refresh();
            last_refresh = std::time::Instant::now();
        }

        // Auto-refresh preview more frequently for live updates
        // Uses faster refresh rate in input mode (set at top of loop)
        if app.mux.supports_preview() && last_preview_refresh.elapsed() >= current_preview_interval
        {
            app.refresh_preview();
            last_preview_refresh = std::time::Instant::now();
        }

        if app.should_quit || app.should_jump {
            break;
        }
    }

    // Save git status cache before exiting
    git::save_status_cache(&app.git_statuses);

    // Save PR status cache before exiting
    github::save_pr_cache(app.pr_statuses());

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
