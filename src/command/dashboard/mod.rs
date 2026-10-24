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
pub mod agent;
mod ansi;
mod app;
pub(crate) mod diff;
mod diff_ops;
mod keymap;
mod layout;
mod mouse;
mod scope;
mod settings;
mod sort;
pub mod spinner;
pub mod ui;
pub use app::DashboardTab;

use anyhow::Result;
use crossterm::{
    cursor::Show,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use std::io;
use std::sync::mpsc;
use std::time::Duration;

use crate::git;
use crate::github;
use crate::multiplexer::{create_backend, detect_backend};

use self::actions::apply_action;
use self::app::{App, AppEvent, ViewMode};
use self::diff_ops::DiffOps;
use self::keymap::{Context, action_for_key};
use self::mouse::handle_mouse_event;
use self::spinner::SPINNER_FRAME_COUNT;
use self::ui::ui;

/// Determine the current keymap context based on app state.
fn get_context(app: &App) -> Context {
    app.keymap_context()
}

fn enter_terminal(mut writer: impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
}

fn leave_terminal(mut writer: impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    )
}

struct TerminalSession {
    active: bool,
}

impl TerminalSession {
    fn start() -> Result<Self> {
        enable_raw_mode()?;
        let stdout = io::stdout();
        if let Err(error) = enter_terminal(stdout) {
            let _ = disable_raw_mode();
            let _ = leave_terminal(io::stdout());
            return Err(error.into());
        }
        Ok(Self { active: true })
    }

    fn restore(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let raw_result = disable_raw_mode();
        let screen_result = leave_terminal(io::stdout());
        self.active = raw_result.is_err() || screen_result.is_err();
        raw_result?;
        screen_result?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub fn run(
    cli_preview_size: Option<u8>,
    open_diff: bool,
    session_filter: bool,
    tab: Option<DashboardTab>,
) -> Result<()> {
    let mux = create_backend(detect_backend());

    // Check if multiplexer is running
    if !mux.is_running().unwrap_or(false) {
        println!("No {} server running.", mux.name());
        return Ok(());
    }

    // Setup terminal
    let mut terminal_session = TerminalSession::start()?;
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // Unified event channel: all background threads and the input thread send here
    let (event_tx, event_rx) = mpsc::channel::<AppEvent>();

    // Create app state before spawning the input thread to avoid a race condition
    // where stray terminal events (e.g. the Enter key used to launch the command)
    // get queued and processed before the app is ready.
    let mut app = App::new(mux, session_filter, event_tx.clone())?;

    // CLI preview size overrides config and saved preference if provided
    if let Some(size) = cli_preview_size {
        app.preview_size = size;
    }

    // CLI tab override: set initial active tab if specified
    if let Some(initial_tab) = tab {
        app.active_tab = initial_tab;
    }

    // Open diff view for current worktree if requested
    if open_diff && let Some(ref current_path) = app.current_worktree {
        // Find the agent matching the current worktree path
        if let Some(idx) = app.agents.iter().position(|a| &a.path == current_path) {
            app.table_state.select(Some(idx));
            app.load_diff(false); // WIP diff (uncommitted changes)
        }
    }

    // Discard any terminal events buffered during startup (e.g. the Enter used to
    // launch the command from the shell). Intentionally discards typeahead.
    while crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
        if crossterm::event::read().is_err() {
            break;
        }
    }

    // Dedicated input thread: reads crossterm events and forwards them.
    // Spawned after init + drain so stray keypresses can't trigger actions.
    let input_tx = event_tx;
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input_tx.send(AppEvent::Terminal(ev)).is_err() {
                break; // receiver dropped, app is shutting down
            }
        }
    });

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

        // Calculate timeout: wake for the earliest timer deadline
        let current_preview_interval = if app.input_mode {
            preview_refresh_interval_input
        } else {
            preview_refresh_interval_normal
        };
        let time_until_preview =
            current_preview_interval.saturating_sub(last_preview_refresh.elapsed());
        let time_until_tick = tick_rate.saturating_sub(last_tick.elapsed());
        let timeout = time_until_tick.min(time_until_preview);

        // Block until an event arrives OR the timeout fires
        match event_rx.recv_timeout(timeout) {
            Ok(event) => {
                handle_event(&mut app, event, &mut last_preview_refresh);

                // Drain any other pending events to coalesce bursts
                while let Ok(event) = event_rx.try_recv() {
                    handle_event(&mut app, event, &mut last_preview_refresh);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
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

    // Save GitHub status caches before exiting
    github::save_pr_cache(app.pr_statuses());
    github::save_check_cache(app.check_statuses());

    terminal_session.restore()?;

    Ok(())
}

/// Handle a single AppEvent, dispatching terminal input or applying background data.
fn handle_event(app: &mut App, event: AppEvent, last_preview_refresh: &mut std::time::Instant) {
    match event {
        AppEvent::Terminal(terminal_event) => {
            handle_terminal_event(app, terminal_event, last_preview_refresh);
        }
        bg_event => app.apply_event(bg_event),
    }
}

/// Handle a crossterm terminal event (key press, mouse scroll, etc.)
fn handle_terminal_event(
    app: &mut App,
    event: Event,
    last_preview_refresh: &mut std::time::Instant,
) {
    // Sweep in progress - block all input except Ctrl+C until complete
    if app.sweep_progress.is_some() {
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && key.code == crossterm::event::KeyCode::Char('c')
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            app.should_quit = true;
        }
        return;
    }

    // Mouse input is routed against the most recently rendered frame.
    if let Event::Mouse(mouse) = event {
        if handle_mouse_event(app, mouse) {
            *last_preview_refresh = std::time::Instant::now();
        }
        return;
    }

    if let Event::Paste(text) = &event {
        if get_context(app) == Context::DashboardInput {
            app.paste_text_to_selected(text);
            app.refresh_preview();
            *last_preview_refresh = std::time::Instant::now();
        }
        return;
    }

    // Handle key events
    let Event::Key(key) = event else { return };
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Help overlay handling - close on any key if open
    if app.show_help {
        app.show_help = false;
        return;
    }

    if app.pending_row_context.is_some() {
        let action = match key.code {
            crossterm::event::KeyCode::Esc | crossterm::event::KeyCode::Char('q') => {
                app.pending_row_context = None;
                return;
            }
            crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Char('j') => {
                if let Some(menu) = &mut app.pending_row_context {
                    menu.move_next();
                }
                return;
            }
            crossterm::event::KeyCode::Up | crossterm::event::KeyCode::Char('k') => {
                if let Some(menu) = &mut app.pending_row_context {
                    menu.move_previous();
                }
                return;
            }
            crossterm::event::KeyCode::Enter => app
                .pending_row_context
                .as_ref()
                .and_then(|menu| menu.selected_action()),
            crossterm::event::KeyCode::Char(character) => app
                .pending_row_context
                .as_ref()
                .and_then(|menu| menu.action_for_hint(&character.to_string())),
            _ => None,
        };
        if let Some(action) = action {
            app.pending_row_context = None;
            if apply_action(app, action) {
                *last_preview_refresh = std::time::Instant::now();
            }
        }
        return;
    }

    // Kill confirmation popup - y confirms, anything else cancels
    if app.pending_kill_pane_id.is_some() {
        if key.code == crossterm::event::KeyCode::Char('y') {
            app.confirm_kill();
        } else {
            app.pending_kill_pane_id = None;
        }
        return;
    }

    // Remove worktree confirmation modal
    if app.pending_remove.is_some() {
        match key.code {
            crossterm::event::KeyCode::Char('y') => app.confirm_remove(),
            crossterm::event::KeyCode::Char('k') => app.toggle_remove_keep_branch(),
            crossterm::event::KeyCode::Char('f') => app.arm_remove_force(),
            _ => app.pending_remove = None, // n, Esc, or any other key cancels
        }
        return;
    }

    // Base branch picker modal
    if app.pending_base_picker.is_some() {
        match key.code {
            crossterm::event::KeyCode::Char('j') | crossterm::event::KeyCode::Down => {
                app.base_picker_down()
            }
            crossterm::event::KeyCode::Char('k') | crossterm::event::KeyCode::Up => {
                app.base_picker_up()
            }
            crossterm::event::KeyCode::Enter => app.confirm_base_picker(),
            crossterm::event::KeyCode::Backspace => app.base_picker_filter_delete(),
            crossterm::event::KeyCode::Esc => app.pending_base_picker = None,
            crossterm::event::KeyCode::Char(c) => app.base_picker_filter_append(c),
            _ => {}
        }
        return;
    }

    // Project picker modal
    if app.pending_project_picker.is_some() {
        match key.code {
            crossterm::event::KeyCode::Char('j') | crossterm::event::KeyCode::Down => {
                app.project_picker_down()
            }
            crossterm::event::KeyCode::Char('k') | crossterm::event::KeyCode::Up => {
                app.project_picker_up()
            }
            crossterm::event::KeyCode::Enter => app.confirm_project_picker(),
            crossterm::event::KeyCode::Backspace => app.project_picker_filter_delete(),
            crossterm::event::KeyCode::Esc => app.pending_project_picker = None,
            crossterm::event::KeyCode::Char(c) => app.project_picker_filter_append(c),
            _ => {}
        }
        return;
    }

    // Command palette modal
    if app.pending_command_palette.is_some() {
        match key.code {
            crossterm::event::KeyCode::Esc => app.pending_command_palette = None,
            crossterm::event::KeyCode::Enter => {
                if let Some(palette) = app.pending_command_palette.take()
                    && let Some(action) = palette.selected_action()
                {
                    let refreshed = apply_action(app, action);
                    if refreshed {
                        *last_preview_refresh = std::time::Instant::now();
                    }
                }
            }
            crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Char('j')
                if !key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                if let Some(ref mut palette) = app.pending_command_palette {
                    palette.move_next();
                }
            }
            crossterm::event::KeyCode::Up | crossterm::event::KeyCode::Char('k')
                if !key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                if let Some(ref mut palette) = app.pending_command_palette {
                    palette.move_previous();
                }
            }
            crossterm::event::KeyCode::Char('n')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                if let Some(ref mut palette) = app.pending_command_palette {
                    palette.move_next();
                }
            }
            crossterm::event::KeyCode::Char('p')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                if let Some(ref mut palette) = app.pending_command_palette {
                    palette.move_previous();
                }
            }
            crossterm::event::KeyCode::Backspace => {
                if let Some(ref mut palette) = app.pending_command_palette {
                    palette.delete_char();
                }
            }
            crossterm::event::KeyCode::Char('w')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                if let Some(ref mut palette) = app.pending_command_palette {
                    palette.delete_word();
                }
            }
            crossterm::event::KeyCode::Char('u')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                if let Some(ref mut palette) = app.pending_command_palette {
                    palette.clear_filter();
                }
            }
            crossterm::event::KeyCode::Char(c) => {
                if let Some(ref mut palette) = app.pending_command_palette {
                    palette.push_filter_char(c);
                }
            }
            _ => {}
        }
        return;
    }

    // Add worktree modal
    if let Some(ref state) = app.pending_add_worktree {
        if state.editing_base {
            // Base branch editing mode
            match key.code {
                crossterm::event::KeyCode::Tab => app.add_worktree_base_tab_complete(),
                crossterm::event::KeyCode::Enter => app.add_worktree_toggle_base(),
                crossterm::event::KeyCode::Backspace => app.add_worktree_base_delete(),
                crossterm::event::KeyCode::Esc => app.add_worktree_toggle_base(),
                crossterm::event::KeyCode::Char('w')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    app.add_worktree_base_delete_word()
                }
                crossterm::event::KeyCode::Char('u')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    app.add_worktree_base_clear()
                }
                crossterm::event::KeyCode::Char('b')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    app.add_worktree_toggle_base()
                }
                crossterm::event::KeyCode::Char(c) => app.add_worktree_base_append(c),
                _ => {}
            }
        } else {
            // Main picker mode
            match key.code {
                crossterm::event::KeyCode::Down => app.add_worktree_down(),
                crossterm::event::KeyCode::Up => app.add_worktree_up(),
                crossterm::event::KeyCode::Tab => app.add_worktree_tab_complete(),
                crossterm::event::KeyCode::Enter => app.add_worktree_confirm_selection(),
                crossterm::event::KeyCode::Backspace => app.add_worktree_delete(),
                crossterm::event::KeyCode::Esc => app.pending_add_worktree = None,
                crossterm::event::KeyCode::Char('w')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    app.add_worktree_delete_word()
                }
                crossterm::event::KeyCode::Char('u')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    app.add_worktree_clear()
                }
                crossterm::event::KeyCode::Char('b')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    app.add_worktree_toggle_base()
                }
                crossterm::event::KeyCode::Char('p')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    app.add_worktree_toggle_pr_mode()
                }
                crossterm::event::KeyCode::Char(c) => app.add_worktree_append(c),
                _ => {}
            }
        }
        return;
    }

    // Sweep modal
    if app.pending_sweep.is_some() {
        match key.code {
            crossterm::event::KeyCode::Char(' ') => app.sweep_toggle(),
            crossterm::event::KeyCode::Char('j') | crossterm::event::KeyCode::Down => {
                app.sweep_down()
            }
            crossterm::event::KeyCode::Char('k') | crossterm::event::KeyCode::Up => app.sweep_up(),
            crossterm::event::KeyCode::Enter => app.confirm_sweep(),
            _ => app.pending_sweep = None, // Esc or any other key cancels
        }
        return;
    }

    // Get current context and map key to action
    let ctx = get_context(app);

    // Special case: EnterPatchMode only works in WIP diff view (not branch diff)
    if ctx == Context::DiffNormal
        && let ViewMode::Diff(ref diff) = app.view_mode
        && diff.is_branch_diff
        && let Some(actions::Action::EnterPatchMode) =
            action_for_key(ctx, key, app.config.dashboard.input_exit_key())
    {
        return;
    }

    if let Some(action) = action_for_key(ctx, key, app.config.dashboard.input_exit_key()) {
        let refreshed_preview = apply_action(app, action);
        if refreshed_preview {
            *last_preview_refresh = std::time::Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{enter_terminal, leave_terminal};

    #[test]
    fn terminal_control_sequences_enable_and_disable_mouse_capture() {
        let mut enter = Vec::new();
        enter_terminal(&mut enter).unwrap();
        let mut leave = Vec::new();
        leave_terminal(&mut leave).unwrap();

        let enter = String::from_utf8(enter).unwrap();
        let leave = String::from_utf8(leave).unwrap();
        assert!(enter.contains("\u{1b}[?1049h"));
        assert!(enter.contains("\u{1b}[?1000h"));
        assert!(enter.contains("\u{1b}[?2004h"));
        assert!(leave.contains("\u{1b}[?2004l"));
        assert!(leave.contains("\u{1b}[?1000l"));
        assert!(leave.contains("\u{1b}[?1049l"));
    }
}
