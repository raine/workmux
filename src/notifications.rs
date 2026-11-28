//! Desktop notifications: merge results and agent status changes.
//!
//! A status notification is scheduled when a status hook commits a
//! transition, and shown by a detached `_notify-status` worker once the
//! configured delay has passed and the agent still holds the same
//! transition. Status hooks never wait for the delay or the notification
//! backend.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::NotificationsConfig;
use crate::multiplexer::{AgentStatus, BackendType, create_backend_for_instance};
use crate::state::{AgentState, PaneKey, StateStore, StatusTransition};

/// Agent events that can show a desktop notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum DesktopEvent {
    /// The agent needs input.
    Waiting,
    /// The agent finished its turn.
    Done,
}

impl DesktopEvent {
    fn status(self) -> AgentStatus {
        match self {
            DesktopEvent::Waiting => AgentStatus::Waiting,
            DesktopEvent::Done => AgentStatus::Done,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            DesktopEvent::Waiting => "needs input",
            DesktopEvent::Done => "finished",
        }
    }
}

/// The event a committed transition represents. A first `done` without an
/// earlier status is a baseline (the agent may have finished long before
/// workmux saw it), while a first `waiting` still needs the user.
pub fn event_for(previous: Option<AgentStatus>, status: AgentStatus) -> Option<DesktopEvent> {
    use AgentStatus::{Done, Waiting, Working};
    match (previous, status) {
        (None | Some(Working | Done), Waiting) => Some(DesktopEvent::Waiting),
        (Some(Working | Waiting), Done) => Some(DesktopEvent::Done),
        _ => None,
    }
}

/// Schedule a desktop notification for `transition` if the configuration
/// asks for its event. Never fails the status update that produced it.
pub fn schedule_status(config: &NotificationsConfig, transition: &StatusTransition) {
    let Some(event) = event_for(transition.previous, transition.status) else {
        return;
    };
    if !config.desktop.contains(&event) {
        return;
    }
    if let Err(error) = spawn_worker(
        &transition.pane_key,
        &transition.token,
        event,
        config.delay(),
    ) {
        warn!(%error, "failed to schedule status notification");
    }
}

fn spawn_worker(
    pane_key: &PaneKey,
    token: &str,
    event: DesktopEvent,
    delay: Duration,
) -> Result<()> {
    let event = match event {
        DesktopEvent::Waiting => "waiting",
        DesktopEvent::Done => "done",
    };
    let mut command = Command::new(std::env::current_exe().context("no workmux executable")?);
    command
        .arg("_notify-status")
        .args(["--backend", &pane_key.backend])
        .args(["--instance", &pane_key.instance])
        .args(["--pane", &pane_key.pane_id])
        .args(["--token", token])
        .args(["--event", event])
        .args(["--delay-ms", &delay.as_millis().to_string()])
        // Agents wait for a hook until its output streams close.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // The worker must outlive the hook and its process group.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .context("failed to start notification worker")?;
    // Long-lived callers (the sandbox supervisor) must not collect zombies.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Body of the `_notify-status` worker: wait out `delay`, then notify if
/// the agent in `pane_key` still holds the transition `token` with the
/// event's status.
pub fn run_status_worker(
    pane_key: PaneKey,
    token: &str,
    event: DesktopEvent,
    delay: Duration,
) -> Result<()> {
    std::thread::sleep(delay);
    let store = StateStore::new()?;
    let Some(state) = store.get_agent(&pane_key)? else {
        debug!(pane_id = %pane_key.pane_id, "agent gone before status notification");
        return Ok(());
    };
    if !still_current(&state, token, event) {
        debug!(pane_id = %pane_key.pane_id, "agent state changed before status notification");
        return Ok(());
    }
    if !pane_is_live(&state) {
        debug!(pane_id = %pane_key.pane_id, "agent pane closed before status notification");
        return Ok(());
    }
    show_notification(&status_message(state.window_name.as_deref(), event));
    Ok(())
}

fn still_current(state: &AgentState, token: &str, event: DesktopEvent) -> bool {
    state.status == Some(event.status()) && state.status_token.as_deref() == Some(token)
}

fn pane_is_live(state: &AgentState) -> bool {
    let Ok(backend) = state.pane_key.backend.parse::<BackendType>() else {
        return false;
    };
    let mux = create_backend_for_instance(backend, &state.pane_key.instance);
    match mux.get_live_pane_info(&state.pane_key.pane_id) {
        Ok(Some(live)) => live
            .pid
            .is_none_or(|pid| state.pane_pid == 0 || pid == state.pane_pid),
        // An unreachable backend says nothing about the pane.
        Err(_) => true,
        Ok(None) => false,
    }
}

/// Longest agent label shown, in characters, including the ellipsis.
const MAX_LABEL_CHARS: usize = 60;

fn status_message(window_name: Option<&str>, event: DesktopEvent) -> String {
    let label: String = window_name
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let label = label.trim();
    let label = if label.is_empty() {
        "Agent".to_string()
    } else if label.chars().count() > MAX_LABEL_CHARS {
        let mut short: String = label.chars().take(MAX_LABEL_CHARS - 1).collect();
        short.push('…');
        short
    } else {
        label.to_string()
    };
    format!("{label} {}", event.describe())
}

/// Environment variable naming a file that receives notifications instead
/// of the desktop, one message per line. Used by integration tests.
const TEST_LOG_ENV: &str = "WORKMUX_TEST_NOTIFICATION_LOG";

/// Show a desktop notification titled "workmux". Failures are logged only.
pub fn show_notification(message: &str) {
    if let Some(path) = std::env::var_os(TEST_LOG_ENV).map(PathBuf::from) {
        use std::io::Write;
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| writeln!(file, "{message}"));
        if let Err(error) = result {
            warn!(%error, "failed to record notification");
        }
        return;
    }

    #[cfg(target_os = "macos")]
    {
        use mac_notification_sys::{Notification, set_application};
        // Set application to Terminal to use its icon
        if let Err(e) = set_application("com.apple.Terminal") {
            debug!("Failed to set notification application: {:?}", e);
        }
        if let Err(e) = Notification::default()
            .title("workmux")
            .message(message)
            .send()
        {
            debug!("Failed to send notification: {:?}", e);
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Notification servers may interpret body markup.
        let body = message
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        if let Err(e) = notify_rust::Notification::new()
            .summary("workmux")
            .body(&body)
            .show()
        {
            debug!("Failed to send notification: {:?}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentStatus::{Done, Waiting, Working};

    #[test]
    fn first_waiting_alerts_but_first_done_is_a_baseline() {
        assert_eq!(event_for(None, Waiting), Some(DesktopEvent::Waiting));
        assert_eq!(event_for(None, Done), None);
        assert_eq!(event_for(None, Working), None);
    }

    #[test]
    fn transitions_into_waiting_and_done_alert() {
        assert_eq!(
            event_for(Some(Working), Waiting),
            Some(DesktopEvent::Waiting)
        );
        assert_eq!(event_for(Some(Done), Waiting), Some(DesktopEvent::Waiting));
        assert_eq!(event_for(Some(Working), Done), Some(DesktopEvent::Done));
        assert_eq!(event_for(Some(Waiting), Done), Some(DesktopEvent::Done));
    }

    #[test]
    fn working_never_alerts() {
        for previous in [None, Some(Working), Some(Waiting), Some(Done)] {
            assert_eq!(event_for(previous, Working), None);
        }
    }

    fn state(status: Option<AgentStatus>, token: Option<&str>) -> AgentState {
        serde_json::from_value(serde_json::json!({
            "pane_key": {"backend": "tmux", "instance": "default", "pane_id": "%1"},
            "workdir": "/repo",
            "status": status,
            "status_ts": 1,
            "pane_title": null,
            "pane_pid": 1,
            "command": "claude",
            "updated_ts": 1,
            "status_token": token,
        }))
        .unwrap()
    }

    #[test]
    fn worker_requires_same_status_and_token() {
        let event = DesktopEvent::Waiting;
        assert!(still_current(&state(Some(Waiting), Some("a")), "a", event));
        assert!(!still_current(&state(Some(Working), Some("a")), "a", event));
        assert!(!still_current(&state(Some(Waiting), Some("b")), "a", event));
        assert!(!still_current(&state(None, None), "a", event));
    }

    #[test]
    fn message_uses_bounded_plain_window_label() {
        assert_eq!(
            status_message(Some("wm-parser"), DesktopEvent::Waiting),
            "wm-parser needs input"
        );
        assert_eq!(status_message(None, DesktopEvent::Done), "Agent finished");
        assert_eq!(
            status_message(Some("a\u{1b}[31mb\n"), DesktopEvent::Done),
            "a[31mb finished"
        );
        let long = "x".repeat(100);
        let message = status_message(Some(&long), DesktopEvent::Done);
        assert_eq!(message.chars().count(), MAX_LABEL_CHARS + " finished".len());
        assert!(message.starts_with(&"x".repeat(MAX_LABEL_CHARS - 1)));
    }
}
