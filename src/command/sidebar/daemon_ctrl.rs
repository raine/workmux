//! Daemon lifecycle management: spawn, kill, signal, health checks.

use anyhow::{Result, anyhow};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::daemon;
use super::host::SidebarHost;

/// Ensure the daemon is running, spawning it if needed. Returns the socket path.
pub(super) fn ensure_daemon_running(host: &SidebarHost) -> Result<PathBuf> {
    let instance_id = host.instance_id();
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
    spawn_daemon(host)?;
    if !wait_for_daemon(&sock_path, Duration::from_secs(4)) {
        return Err(anyhow!("Sidebar daemon failed to start"));
    }
    Ok(sock_path)
}

/// Spawn the sidebar daemon as a detached background process.
fn spawn_daemon(host: &SidebarHost) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("_sidebar-daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    host.daemon_command(&mut command);
    command.spawn()?;
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
pub(super) fn kill_daemon(host: &SidebarHost) {
    let store = host.store();
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

/// Signal the daemon to do an immediate refresh, bypassing tmux hook latency.
pub(super) fn signal_daemon(host: &SidebarHost) {
    if let Some(pid) = host.store().daemon_pid() {
        signal_pid(&pid);
    }
}
