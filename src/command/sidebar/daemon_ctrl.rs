//! Daemon lifecycle management: spawn, kill, signal, health checks.

use anyhow::{Result, anyhow};
use std::path::PathBuf;
use std::time::Duration;

use crate::cmd::Cmd;
use crate::multiplexer::{Multiplexer, TmuxBackend, create_backend, detect_backend};

use super::daemon;

/// Ensure the daemon is running, spawning it if needed. Returns the socket path.
pub(super) fn ensure_daemon_running() -> Result<PathBuf> {
    let mux = create_backend(detect_backend());
    let instance_id = mux.instance_id();
    let sock_path = daemon::socket_path(&instance_id);

    if std::os::unix::net::UnixStream::connect(&sock_path).is_ok() {
        return Ok(sock_path);
    }

    // Stale socket from a crashed daemon
    let _ = std::fs::remove_file(&sock_path);
    spawn_daemon()?;
    if !wait_for_socket(&instance_id, Duration::from_secs(2)) {
        return Err(anyhow!("Sidebar daemon failed to start"));
    }
    Ok(sock_path)
}

/// Spawn the sidebar daemon as a detached background process.
fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("_sidebar-daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// Wait for the daemon's Unix socket to appear.
fn wait_for_socket(instance_id: &str, timeout: Duration) -> bool {
    let path = daemon::socket_path(instance_id);
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Read the daemon PID from the tmux global option.
fn daemon_pid(mux: &dyn Multiplexer) -> Option<String> {
    if mux.name() == "tmux" {
        return Cmd::new("tmux")
            .args(&["show-option", "-gqv", "@workmux_sidebar_daemon_pid"])
            .run_and_capture_stdout()
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }

    super::session_state::read(&mux.instance_id())
        .ok()
        .and_then(|state| state.daemon_pid)
        .map(|pid| pid.to_string())
}

/// Kill the sidebar daemon (sends SIGTERM, cleans up tmux option).
pub(super) fn kill_daemon() {
    let mux = create_backend(detect_backend());
    if let Some(pid) = daemon_pid(mux.as_ref()) {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .stderr(std::process::Stdio::null())
            .status();
    }
    if mux.name() == "tmux" {
        let _ = Cmd::new("tmux")
            .args(&["set-option", "-gu", "@workmux_sidebar_daemon_pid"])
            .run();
    } else {
        let _ = super::session_state::update(&mux.instance_id(), |state| {
            state.daemon_pid = None;
        });
    }
}

fn signal_pid(pid: &str) {
    let _ = std::process::Command::new("kill")
        .args(["-USR1", pid])
        .stderr(std::process::Stdio::null())
        .status();
}

pub(super) fn signal_daemon_for(mux: &dyn Multiplexer) {
    if mux.name() == "tmux" {
        let tmux = TmuxBackend::for_socket(&mux.instance_id());
        if let Ok(Some(pid)) = tmux.global_option("@workmux_sidebar_daemon_pid") {
            signal_pid(&pid);
        }
    } else if let Some(pid) = daemon_pid(mux) {
        signal_pid(&pid);
    }
}

/// Signal the daemon to do an immediate refresh, bypassing tmux hook latency.
pub(super) fn signal_daemon() {
    let mux = create_backend(detect_backend());
    signal_daemon_for(mux.as_ref());
}
