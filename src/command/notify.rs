//! Desktop notification that focuses the most recently completed agent on click.
//!
//! Shows a system notification and blocks until the user interacts with it.
//! Clicking the notification switches to the most recently completed or waiting
//! agent (same logic as `workmux last-done`).
//!
//! On Linux this uses notify-rust's action callback via D-Bus. On macOS, where
//! notify-rust does not support click actions, it falls back to a plain
//! non-clickable notification (matching the existing merge notification).
//!
//! Intended to be spawned in the background by the pi status extension when an
//! agent finishes, so the user is alerted and can jump back with a click.

use anyhow::Result;
use tracing::debug;

/// Show a desktop notification. Blocks until the notification is clicked or
/// dismissed. On click, switches to the most recently completed agent.
pub fn run(body: Option<&str>) -> Result<()> {
    let message = body.unwrap_or("Agent finished");

    #[cfg(not(target_os = "macos"))]
    {
        use notify_rust::{Hint, Notification};

        let handle = Notification::new()
            .summary("workmux")
            .body(message)
            .timeout(notify_rust::Timeout::Never)
            .action("default", "Focus")
            .hint(Hint::Resident(true))
            .show()?;

        handle.wait_for_action(|action: &str| {
            debug!(action, "notification action");
            if action == "default" {
                if let Err(e) = crate::command::last_done::run() {
                    debug!(error = %e, "last-done after notification click failed");
                }
            }
        });
    }

    #[cfg(target_os = "macos")]
    {
        use mac_notification_sys::{Notification, set_application};
        if let Err(e) = set_application("com.apple.Terminal") {
            debug!(error = ?e, "Failed to set notification application");
        }
        if let Err(e) = Notification::default()
            .title("workmux")
            .message(message)
            .send()
        {
            debug!(error = ?e, "Failed to send notification");
        }
    }

    Ok(())
}
