//! Polling-based status detection command for agents without hooks.
//!
//! This command runs a background poller that periodically captures terminal
//! output from agent panes and detects their status via pattern matching.

use std::time::Duration;

use anyhow::Result;

use crate::multiplexer::{create_backend, detect_backend};
use crate::state::StatusPoller;

/// Default polling interval in seconds.
const DEFAULT_INTERVAL_SECS: u64 = 3;

/// Run the status poller in the foreground.
///
/// This is useful for:
/// - Testing pattern detection
/// - Running as a background daemon
/// - Debugging status detection issues
pub fn run(interval_secs: Option<u64>) -> Result<()> {
    let mux = create_backend(detect_backend());

    // Check if multiplexer is running
    if !mux.is_running().unwrap_or(false) {
        println!("No {} server running.", mux.name());
        return Ok(());
    }

    let interval = Duration::from_secs(interval_secs.unwrap_or(DEFAULT_INTERVAL_SECS));

    println!(
        "Starting status poller (interval: {}s). Press Ctrl+C to stop.",
        interval.as_secs()
    );

    let poller = StatusPoller::start(mux, interval);

    // Wait for Ctrl+C
    ctrlc::set_handler(move || {
        println!("\nStopping poller...");
        std::process::exit(0);
    })?;

    // Keep main thread alive
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if !poller.is_running() {
            break;
        }
    }

    Ok(())
}
