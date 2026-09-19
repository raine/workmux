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
use std::time::{Duration, Instant};

use crate::cmd::Cmd;
use crate::multiplexer::{create_backend, detect_backend};
use crate::shell::shell_quote;

use super::app::{HostIdentity, SidebarApp};
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
    let Some(host_identity) = app.host_identity().cloned() else {
        tracing::error!("sidebar-run exiting: host pane identity unavailable");
        return Ok(());
    };

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
    let mut needs_clear = false;
    let startup = std::time::Instant::now();
    let startup_grace = Duration::from_secs(3);
    let mut last_pane_check = LastPaneCheck::new(startup + startup_grace);
    let mut last_refresh = std::time::Instant::now();

    loop {
        // Render before blocking (redraws only when state changed)
        if needs_render {
            if needs_clear {
                terminal.clear()?;
                needs_clear = false;
            }
            terminal.draw(|f| render_sidebar(f, &mut app))?;
            needs_render = false;
        }

        // Time-dependent content supplies its own refresh interval. Static or
        // hidden sidebars block until a snapshot or input wakes them.
        let refresh_interval = app
            .host_window_active()
            .then(|| app.refresh_interval())
            .flatten();
        let resize_timeout = app
            .resize_deadline
            .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
        let timeout = match (resize_timeout, refresh_interval) {
            (Some(resize), Some(refresh)) => {
                resize.min(refresh.saturating_sub(last_refresh.elapsed()))
            }
            (Some(resize), None) => resize,
            (None, Some(refresh)) => refresh.saturating_sub(last_refresh.elapsed()),
            (None, None) => Duration::from_secs(3600),
        };

        // The startup recheck has one deadline, even if snapshots are deduplicated.
        let timeout = last_pane_check
            .timeout(Instant::now())
            .map_or(timeout, |grace| timeout.min(grace));

        let first_event = match rx.recv_timeout(timeout) {
            Ok(ev) => Some(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
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
                &mut last_pane_check,
                &mut needs_render,
                &mut needs_clear,
            );
        }

        // Drain all pending events to coalesce (avoids multiple redraws)
        while let Ok(ev) = rx.try_recv() {
            process_event(
                ev,
                &mut app,
                &snapshot_handle,
                &mut last_pane_check,
                &mut needs_render,
                &mut needs_clear,
            );
        }

        if last_pane_check.grace_expired(Instant::now())
            && last_pane_check.should_exit(app.host_identity(), sidebar_is_only_pane)
        {
            quit_for_last_pane(&mut app);
        }

        // Process any pending resize whose debounce has elapsed
        app.process_pending_resize(&startup, startup_grace);
        advance_refresh_if_due(
            &mut app,
            &mut last_refresh,
            refresh_interval,
            &mut needs_render,
        );

        if app.should_quit {
            tracing::info!(
                host_window = ?app.host_window_id(),
                quit_reason = app.quit_reason.as_deref().unwrap_or("unknown"),
                "sidebar-run quitting"
            );
            if app.quit_silent {
                schedule_pane_kill(&host_identity.pane_id);
            } else {
                shutdown_all_sidebars(&host_identity);
            }
            break;
        }
    }

    // _guard handles cleanup on drop (including the normal exit path)
    Ok(())
}

fn advance_refresh_if_due(
    app: &mut SidebarApp,
    last_refresh: &mut std::time::Instant,
    refresh_interval: Option<Duration>,
    needs_render: &mut bool,
) {
    let Some(refresh_interval) = refresh_interval else {
        *last_refresh = std::time::Instant::now();
        return;
    };
    if last_refresh.elapsed() >= refresh_interval {
        *last_refresh = std::time::Instant::now();
        app.tick();
        *needs_render = true;
    }
}

fn handle_resize_event(
    app: &mut SidebarApp,
    cols: u16,
    rows: u16,
    needs_render: &mut bool,
    needs_clear: &mut bool,
) {
    app.on_resize_event(cols, rows);
    *needs_render = true;
    *needs_clear = true;
}

fn pane_kill_command(pane_id: &str) -> String {
    format!(
        "sleep 0.05; tmux kill-pane -t {} 2>/dev/null || true",
        shell_quote(pane_id)
    )
}

fn schedule_pane_kill(pane_id: &str) {
    let cmd = pane_kill_command(pane_id);
    let _ = Cmd::new("tmux").args(&["run-shell", "-b", &cmd]).run();
}

fn sole_pane_is_sidebar(output: &str, pane_id: &str) -> bool {
    let mut panes = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    panes.next() == Some(pane_id) && panes.next().is_none()
}

fn sidebar_is_only_pane(window_id: &str, pane_id: &str) -> bool {
    Cmd::new("tmux")
        .args(&["list-panes", "-t", window_id, "-F", "#{pane_id}"])
        .run_and_capture_stdout()
        .is_ok_and(|output| sole_pane_is_sidebar(&output, pane_id))
}

struct LastPaneCheck {
    grace_deadline: Option<Instant>,
    pane_count: Option<usize>,
}

impl LastPaneCheck {
    fn new(grace_deadline: Instant) -> Self {
        Self {
            grace_deadline: Some(grace_deadline),
            pane_count: None,
        }
    }

    fn timeout(&self, now: Instant) -> Option<Duration> {
        self.grace_deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Consume the startup deadline once, including when input wakes the loop.
    fn grace_expired(&mut self, now: Instant) -> bool {
        if self.grace_deadline.is_some_and(|deadline| now >= deadline) {
            self.grace_deadline = None;
            true
        } else {
            false
        }
    }

    fn should_exit(
        &self,
        identity: Option<&HostIdentity>,
        verify_live_panes: impl FnOnce(&str, &str) -> bool,
    ) -> bool {
        if self.grace_deadline.is_some() || self.pane_count.is_none_or(|count| count > 1) {
            return false;
        }
        let Some(identity) = identity else {
            return false;
        };
        verify_live_panes(&identity.window_id, &identity.pane_id)
    }
}

fn quit_for_last_pane(app: &mut SidebarApp) {
    let window_id = app.host_window_id().unwrap_or("unknown");
    app.quit_reason = Some(format!(
        "last-pane: sidebar is sole pane in window {}",
        window_id
    ));
    app.quit_silent = true;
    app.should_quit = true;
}

fn process_event(
    event: AppEvent,
    app: &mut SidebarApp,
    snapshot_handle: &client::SnapshotHandle,
    last_pane_check: &mut LastPaneCheck,
    needs_render: &mut bool,
    needs_clear: &mut bool,
) {
    match event {
        AppEvent::SnapshotReady => {
            if let Some(snapshot) = snapshot_handle.take() {
                last_pane_check.pane_count = app
                    .host_window_id()
                    .and_then(|window_id| snapshot.window_pane_counts.get(window_id))
                    .copied();
                if last_pane_check.should_exit(app.host_identity(), sidebar_is_only_pane) {
                    quit_for_last_pane(app);
                }
                app.apply_snapshot(snapshot);
                *needs_render = true;
            }
        }
        AppEvent::Input(Event::Key(key)) if key.kind == KeyEventKind::Press => {
            handle_key_press(app, key.code, key.modifiers);
            *needs_render = true;
        }
        AppEvent::Input(Event::Mouse(_)) if app.pending_exit => {}
        AppEvent::Input(Event::Mouse(mouse)) => {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(group) = app.hit_test_toggle(mouse.column, mouse.row) {
                        app.toggle_group(&group);
                    } else if let Some(idx) = app.hit_test(mouse.column, mouse.row) {
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
            handle_resize_event(app, cols, rows, needs_render, needs_clear);
        }
        AppEvent::Input(_) => {}
    }
}

fn handle_key_press(
    app: &mut SidebarApp,
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) {
    if app.pending_exit {
        if code == KeyCode::Char('y') {
            app.quit_reason = Some("confirmed user exit".to_string());
            app.should_quit = true;
        } else {
            app.pending_exit = false;
        }
        return;
    }

    // The overlay covers the list, so the next key dismisses it rather than
    // acting on rows the user cannot see.
    if app.show_help {
        app.show_help = false;
        return;
    }

    match (code, modifiers) {
        (KeyCode::Char('q'), _)
        | (KeyCode::Esc, _)
        | (KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL) => {
            app.pending_exit = true;
        }
        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.next(),
        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.previous(),
        (KeyCode::Enter, _) => {
            if app.selected_toggle().is_some() {
                app.toggle_selected_group();
            } else {
                app.jump_to_selected();
            }
        }
        (KeyCode::Char('h'), _) | (KeyCode::Left, _) => app.set_selected_group_expanded(false),
        (KeyCode::Char('l'), _) | (KeyCode::Right, _) => app.set_selected_group_expanded(true),
        (KeyCode::Char('G'), _) => app.select_last(),
        (KeyCode::Char('g'), _) => app.select_first(),
        (KeyCode::Char('v'), _) => app.toggle_layout_mode(),
        (KeyCode::Char('z'), _) => app.toggle_sleeping(),
        (KeyCode::Char('f'), _) => app.toggle_filter_mode(),
        (KeyCode::Char('t'), _) => app.toggle_grouping(),
        (KeyCode::Char('s'), _) => app.toggle_selected_group(),
        (KeyCode::Char('S'), _) => app.toggle_all_groups(),
        (KeyCode::Char('?'), _) => {
            app.show_help = true;
            app.dismiss_hint();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::sidebar::app::TemplateError;
    use crossterm::event::KeyModifiers;

    fn test_app() -> SidebarApp {
        SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        })
    }

    fn test_identity() -> HostIdentity {
        HostIdentity {
            session_name: "main".to_string(),
            session_id: "$1".to_string(),
            window_id: "@42".to_string(),
            pane_id: "%12".to_string(),
        }
    }

    #[test]
    fn startup_recheck_exits_without_another_snapshot() {
        let identity = test_identity();
        let startup = Instant::now();
        let grace = Duration::from_secs(3);
        let mut check = LastPaneCheck::new(startup + grace);
        check.pane_count = Some(1);

        assert_eq!(check.timeout(startup), Some(grace));
        assert!(!check.grace_expired(startup + grace - Duration::from_nanos(1)));
        assert!(!check.should_exit(Some(&identity), |_, _| {
            panic!("startup grace must skip live verification")
        }));

        assert_eq!(check.timeout(startup + grace), Some(Duration::ZERO));
        assert!(check.grace_expired(startup + grace));
        assert!(check.should_exit(Some(&identity), |window, pane| {
            window == "@42" && pane == "%12"
        }));
        assert_eq!(check.timeout(startup + grace), None);
        assert!(!check.grace_expired(startup + grace + Duration::from_secs(1)));
    }

    #[test]
    fn startup_recheck_live_verifies_stale_snapshot_only_once() {
        let identity = test_identity();
        let deadline = Instant::now();
        let mut check = LastPaneCheck::new(deadline);
        check.pane_count = Some(1);
        let mut live_checks = 0;

        for now in [deadline, deadline + Duration::from_secs(60)] {
            if check.grace_expired(now) {
                assert!(!check.should_exit(Some(&identity), |_, _| {
                    live_checks += 1;
                    false
                }));
            }
        }
        assert_eq!(live_checks, 1);
        assert_eq!(check.timeout(deadline), None);
    }

    #[test]
    fn startup_recheck_uses_latest_snapshot_count() {
        let identity = test_identity();
        let deadline = Instant::now();
        let mut check = LastPaneCheck::new(deadline);
        check.pane_count = Some(1);
        check.pane_count = Some(2);
        assert!(check.grace_expired(deadline));
        assert!(!check.should_exit(Some(&identity), |_, _| {
            panic!("content pane created during grace must skip live verification")
        }));
        assert_eq!(check.timeout(deadline), None);

        // Snapshot-driven checks remain available after the one-shot deadline.
        check.pane_count = Some(1);
        assert!(check.should_exit(Some(&identity), |_, _| true));
    }

    #[test]
    fn last_pane_exit_requires_snapshot_and_live_confirmation() {
        let identity = test_identity();
        let deadline = Instant::now();
        let mut check = LastPaneCheck::new(deadline);
        assert!(check.grace_expired(deadline));

        for count in [None, Some(2)] {
            check.pane_count = count;
            assert!(!check.should_exit(Some(&identity), |_, _| {
                panic!("missing count or multiple panes must skip live verification")
            }));
        }
        check.pane_count = Some(1);
        assert!(!check.should_exit(None, |_, _| {
            panic!("missing identity must skip live verification")
        }));
        assert!(!check.should_exit(Some(&identity), |_, _| false));
        assert!(check.should_exit(Some(&identity), |window, pane| {
            window == "@42" && pane == "%12"
        }));
    }

    #[test]
    fn pane_kill_command_targets_captured_sidebar_pane() {
        assert_eq!(
            pane_kill_command("%12"),
            "sleep 0.05; tmux kill-pane -t '%12' 2>/dev/null || true"
        );
    }

    #[test]
    fn sole_pane_confirmation_requires_sidebar_identity() {
        assert!(sole_pane_is_sidebar("%12\n", "%12"));
        assert!(!sole_pane_is_sidebar("%12\n%13\n", "%12"));
        assert!(!sole_pane_is_sidebar("%13\n", "%12"));
        assert!(!sole_pane_is_sidebar("", "%12"));
    }

    #[test]
    fn resize_requests_full_redraw() {
        let mut app = test_app();
        let mut needs_render = false;
        let mut needs_clear = false;

        handle_resize_event(&mut app, 120, 3, &mut needs_render, &mut needs_clear);

        assert!(needs_render);
        assert!(needs_clear);
    }

    #[test]
    fn q_q_does_not_quit_sidebar() {
        let mut app = test_app();

        handle_key_press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.pending_exit);
        assert!(!app.should_quit);

        handle_key_press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.pending_exit);
        assert!(!app.should_quit);
    }

    #[test]
    fn any_key_closes_the_help_overlay_without_acting() {
        let mut app = test_app();

        handle_key_press(&mut app, KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(app.show_help);

        // 'q' would normally ask to quit, but it is spent closing the overlay.
        handle_key_press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.show_help);
        assert!(!app.pending_exit);
    }

    #[test]
    fn y_confirms_pending_exit() {
        let mut app = test_app();

        handle_key_press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        handle_key_press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);

        assert!(app.should_quit);
        assert_eq!(app.quit_reason.as_deref(), Some("confirmed user exit"));
    }
}
