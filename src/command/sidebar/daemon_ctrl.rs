//! Daemon lifecycle management: spawn, kill, signal, health checks.

use anyhow::{Result, anyhow};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::multiplexer::{Multiplexer, TmuxBackend, create_backend, detect_backend};

use super::daemon;
use super::store::SidebarStore;

/// Ensure the daemon is running, spawning it if needed. Returns the socket path.
pub(super) fn ensure_daemon_running() -> Result<PathBuf> {
    let mux = create_backend(detect_backend());
    let instance_id = mux.instance_id();
    let sock_path = daemon::socket_path(&instance_id);

    if UnixStream::connect(&sock_path).is_ok() {
        return Ok(sock_path);
    }

    let _spawn = super::session_state::lock_spawn(&instance_id)?;
    // Another caller may have started the daemon while this one waited
    if UnixStream::connect(&sock_path).is_ok() {
        return Ok(sock_path);
    }

    // Stale socket from a crashed daemon
    let _ = std::fs::remove_file(&sock_path);
    spawn_daemon()?;
    if !wait_for_daemon(&sock_path, Duration::from_secs(4)) {
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

/// Wait until the daemon accepts connections on its socket.
fn wait_for_daemon(path: &Path, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if UnixStream::connect(path).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Kill the sidebar daemon (sends SIGTERM, clears its recorded PID).
pub(super) fn kill_daemon() {
    let mux = create_backend(detect_backend());
    let store = SidebarStore::for_mux(mux.as_ref());
    if let Some(pid) = store.daemon_pid() {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .stderr(std::process::Stdio::null())
            .status();
    }
    store.clear_daemon_pid();
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
    } else if let Some(pid) = SidebarStore::for_mux(mux).daemon_pid() {
        signal_pid(&pid);
    }
}

/// Signal the daemon to do an immediate refresh, bypassing tmux hook latency.
pub(super) fn signal_daemon() {
    let mux = create_backend(detect_backend());
    signal_daemon_for(mux.as_ref());
}
