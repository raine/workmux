//! TUI event loop for the sidebar client.

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::cmd::Cmd;
use crate::multiplexer::{create_backend, detect_backend};
use crate::shell::shell_quote;

use super::app::SidebarApp;
use super::client;
use super::daemon_ctrl::ensure_daemon_running;
use super::panes::shutdown_all_sidebars;
use super::ui::render_sidebar;

/// Drop guard that restores terminal state on panic or early return.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

enum AppEvent {
    /// A new snapshot is available in the SnapshotHandle.
    SnapshotReady,
    /// A terminal input event (key press, resize, etc.).
    Input(Event),
}

/// Spawn a thread that reads terminal events and forwards them.
/// Must be called AFTER terminal raw mode is enabled.
fn spawn_input_thread(tx: mpsc::Sender<AppEvent>) {
    thread::spawn(move || {
        // event::read() blocks until input is available - zero CPU
        while let Ok(ev) = event::read() {
            if tx.send(AppEvent::Input(ev)).is_err() {
                break;
            }
        }
    });
}

/// Run the sidebar TUI (called by the hidden `_sidebar-run` command).
pub fn run_sidebar() -> Result<()> {
    let mux = create_backend(detect_backend());

    if !mux.is_running().unwrap_or(false) {
        tracing::info!("sidebar-run exiting: mux not running");
        return Ok(());
    }

    // Create app BEFORE entering raw mode: terminal_light::luma() queries
    // the terminal via stdin, which would race with the input reader thread.
    let mut app = SidebarApp::new_client(mux)?;

    // Ensure daemon is running (may have auto-exited or crashed)
    let sock_path = ensure_daemon_running()?;

    // Setup terminal (raw mode required before spawning input thread)
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // Channel for all events
    let (tx, rx) = mpsc::channel();

    // Snapshot receiver: overwrites latest, sends SnapshotReady wake via
    // a thin forwarding thread that converts () -> AppEvent::SnapshotReady
    let snapshot_handle = {
        let (wake_tx, wake_rx) = mpsc::sync_channel::<()>(1);
        let event_tx = tx.clone();
        thread::spawn(move || {
            for () in wake_rx {
                if event_tx.send(AppEvent::SnapshotReady).is_err() {
                    break;
                }
            }
        });
        client::connect(&sock_path, wake_tx)
    };

    // Input reader thread (terminal is already in raw mode)
    spawn_input_thread(tx);

    let mut needs_render = true;
    let startup = std::time::Instant::now();
    let startup_grace = Duration::from_secs(3);
    let tick_rate = Duration::from_millis(250);
    let mut last_spinner_tick = std::time::Instant::now();

    loop {
        // Render before blocking (redraws only when state changed)
        if needs_render {
            terminal.draw(|f| render_sidebar(f, &mut app))?;
            needs_render = false;
        }

        // Adaptive timeout: 250ms when active, block when hidden.
        // If a resize debounce is pending, wake early to process it.
        let timeout = if let Some(deadline) = app.resize_deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            remaining.min(tick_rate)
        } else if app.host_window_active() {
            tick_rate.saturating_sub(last_spinner_tick.elapsed())
        } else {
            // Block until a snapshot or input wakes us. Use a large timeout
            // since recv() without timeout would prevent clean shutdown if
            // all senders drop.
            Duration::from_secs(3600)
        };

        let first_event = match rx.recv_timeout(timeout) {
            Ok(ev) => Some(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                app.process_pending_resize(&startup, startup_grace);
                advance_spinner_if_due(
                    &mut app,
                    &mut last_spinner_tick,
                    tick_rate,
                    &mut needs_render,
                );
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::info!("sidebar-run exiting: event channel disconnected");
                break;
            }
        };

        // Process first event
        if let Some(ev) = first_event {
            process_event(
                ev,
                &mut app,
                &snapshot_handle,
                &startup,
                startup_grace,
                &mut needs_render,
            );
        }

        // Drain all pending events to coalesce (avoids multiple redraws)
        while let Ok(ev) = rx.try_recv() {
            process_event(
                ev,
                &mut app,
                &snapshot_handle,
                &startup,
                startup_grace,
                &mut needs_render,
            );
        }

        // Process any pending resize whose debounce has elapsed
        app.process_pending_resize(&startup, startup_grace);
        advance_spinner_if_due(
            &mut app,
            &mut last_spinner_tick,
            tick_rate,
            &mut needs_render,
        );

        if app.should_quit {
            tracing::info!(
                host_window = ?app.host_window_id(),
                quit_reason = app.quit_reason.as_deref().unwrap_or("unknown"),
                "sidebar-run quitting"
            );
            if app.quit_silent {
                schedule_current_pane_kill();
            } else {
                shutdown_all_sidebars();
            }
            break;
        }
    }

    // _guard handles cleanup on drop (including the normal exit path)
    Ok(())
}

fn advance_spinner_if_due(
    app: &mut SidebarApp,
    last_spinner_tick: &mut std::time::Instant,
    tick_rate: Duration,
    needs_render: &mut bool,
) {
    if !app.host_window_active() {
        *last_spinner_tick = std::time::Instant::now();
        return;
    }
    if last_spinner_tick.elapsed() >= tick_rate {
        *last_spinner_tick = std::time::Instant::now();
        app.tick();
        *needs_render = true;
    }
}

fn schedule_current_pane_kill() {
    let pane = Cmd::new("tmux")
        .args(&["display-message", "-p", "#{pane_id}"])
        .run_and_capture_stdout()
        .unwrap_or_default()
        .trim()
        .to_string();
    if pane.is_empty() {
        return;
    }

    let cmd = format!(
        "sleep 0.05; tmux kill-pane -t {} 2>/dev/null",
        shell_quote(&pane)
    );
    let _ = Cmd::new("tmux").args(&["run-shell", "-b", &cmd]).run();
}

fn process_event(
    event: AppEvent,
    app: &mut SidebarApp,
    snapshot_handle: &client::SnapshotHandle,
    startup: &std::time::Instant,
    startup_grace: Duration,
    needs_render: &mut bool,
) {
    match event {
        AppEvent::SnapshotReady => {
            if let Some(snapshot) = snapshot_handle.take() {
                // Check last-pane using snapshot data (with startup grace period)
                if startup.elapsed() > startup_grace
                    && let Some(wid) = app.host_window_id()
                    && snapshot.window_pane_counts.get(wid).copied().unwrap_or(2) <= 1
                {
                    app.quit_reason = Some(format!("last-pane: window {} has <= 1 pane", wid));
                    app.quit_silent = true;
                    app.should_quit = true;
                }
                app.apply_snapshot(snapshot);
                *needs_render = true;
            }
        }
        AppEvent::Input(Event::Key(key)) if key.kind == KeyEventKind::Press => {
            match (key.code, key.modifiers) {
                (KeyCode::Char('q'), _)
                | (KeyCode::Esc, _)
                | (KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL) => {
                    app.quit_reason = Some("user keypress".to_string());
                    app.should_quit = true;
                }
                (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.next(),
                (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.previous(),
                (KeyCode::Enter, _) => app.jump_to_selected(),
                (KeyCode::Char('G'), _) => app.select_last(),
                (KeyCode::Char('g'), _) => app.select_first(),
                (KeyCode::Char('v'), _) => app.toggle_layout_mode(),
                (KeyCode::Char('z'), _) => app.toggle_sleeping(),
                (KeyCode::Char('f'), _) => app.toggle_filter_mode(),
                _ => {}
            }
            *needs_render = true;
        }
        AppEvent::Input(Event::Mouse(mouse)) => {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(idx) = app.hit_test(mouse.column, mouse.row) {
                        app.select_index(idx);
                        app.jump_to_selected();
                    }
                }
                MouseEventKind::ScrollUp => {
                    app.scroll_up();
                }
                MouseEventKind::ScrollDown => {
                    app.scroll_down();
                }
                _ => {}
            }
            *needs_render = true;
        }
        AppEvent::Input(Event::Resize(cols, rows)) => {
            app.on_resize_event(cols, rows);
            *needs_render = true;
        }
        AppEvent::Input(_) => {}
    }
}
