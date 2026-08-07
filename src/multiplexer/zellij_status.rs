//! Animated agent status for Zellij tab titles.

mod socket;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cmd::Cmd;
use crate::state::PaneKey;
use socket::{SocketIdentity, ensure_session_socket, session_socket};

// Zero-width markers let workmux replace or remove only the status decoration
// it owns, without mistaking user-provided title text for agent status.
const STATUS_MARKER_OPEN: char = '\u{2063}';
const STATUS_MARKER_CLOSE: char = '\u{2064}';
const SPINNER_INTERVAL: Duration = Duration::from_millis(100);
const MAX_CONSECUTIVE_FAILURES: usize = 50;

pub(crate) const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Debug, Deserialize, Serialize)]
struct SpinnerState {
    token: String,
    pid: u32,
    tab_id: u32,
    base_name: String,
    socket_path: PathBuf,
    socket_identity: SocketIdentity,
}

#[derive(Debug, Deserialize)]
struct TabTitle {
    tab_id: u32,
    name: String,
}

struct StatusLock {
    _lock: Flock<File>,
}

pub(crate) fn tab_name_without_status(name: &str) -> Option<&str> {
    if name.starts_with(STATUS_MARKER_OPEN) {
        let marker_end = name.find(STATUS_MARKER_CLOSE)? + STATUS_MARKER_CLOSE.len_utf8();
        return name[marker_end..].strip_prefix(' ');
    }

    if name.ends_with(STATUS_MARKER_CLOSE) {
        let marker_start = name.rfind(STATUS_MARKER_OPEN)?;
        return name[..marker_start].strip_suffix(' ');
    }

    None
}

pub(crate) fn canonical_tab_name(name: &str) -> &str {
    tab_name_without_status(name).unwrap_or(name)
}

pub(crate) fn tab_name_with_status(name: &str, icon: &str) -> String {
    let base = canonical_tab_name(name);
    let icon = crate::tmux_style::strip_tmux_styles(icon)
        .replace([STATUS_MARKER_OPEN, STATUS_MARKER_CLOSE], "");
    format!("{base} {STATUS_MARKER_OPEN}{icon}{STATUS_MARKER_CLOSE}")
}

pub(crate) fn tab_name_with_spinner(name: &str, frame: char) -> String {
    let base = canonical_tab_name(name);
    format!("{STATUS_MARKER_OPEN}{frame}{STATUS_MARKER_CLOSE} {base}")
}

pub(crate) fn start_spinner(session: &str, tab_id: u32) -> Result<()> {
    let key = tab_key(session, tab_id);
    let dir = status_dir()?;
    let _lock = acquire_lock(&dir, &key)?;
    let (socket_path, socket_identity) = session_socket(session)?;
    let current_title = current_tab_title(session, tab_id)?
        .ok_or_else(|| anyhow::anyhow!("Zellij tab {tab_id} is unavailable"))?;
    let base_name = canonical_tab_name(&current_title);

    if read_state(&dir, &key)?.is_some_and(|state| {
        state.tab_id == tab_id
            && state.base_name == base_name
            && state.socket_path == socket_path
            && state.socket_identity == socket_identity
            && spinner_process_is_running(&state)
    }) {
        return Ok(());
    }

    delete_state(&dir, &key)?;
    ensure_session_socket(&socket_path, socket_identity)?;

    let token = animation_token();
    let exe = std::env::current_exe().context("Failed to resolve workmux executable")?;
    let child = Command::new(exe)
        .args([
            "_zellij-status-spinner",
            "--session",
            session,
            "--tab-id",
            &tab_id.to_string(),
            "--token",
            &token,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to start Zellij status spinner")?;

    let state = SpinnerState {
        token,
        pid: child.id(),
        tab_id,
        base_name: base_name.to_string(),
        socket_path,
        socket_identity,
    };
    write_state(&dir, &key, &state)?;

    if let Err(error) = rename_status_tab_checked(
        session,
        tab_id,
        &tab_name_with_spinner(base_name, SPINNER_FRAMES[0]),
        &state.socket_path,
        state.socket_identity,
    ) {
        let _ = delete_state(&dir, &key);
        return Err(error);
    }

    Ok(())
}

pub(crate) fn set_static_status(session: &str, tab_id: u32, icon: &str) -> Result<()> {
    let key = tab_key(session, tab_id);
    let dir = status_dir()?;
    let _lock = acquire_lock(&dir, &key)?;
    let Some((socket_path, socket_identity)) = operation_socket(session, &dir, &key)? else {
        return Ok(());
    };
    let current_title = current_tab_title(session, tab_id)?
        .ok_or_else(|| anyhow::anyhow!("Zellij tab {tab_id} is unavailable"))?;
    rename_status_tab_checked(
        session,
        tab_id,
        &tab_name_with_status(&current_title, icon),
        &socket_path,
        socket_identity,
    )?;
    delete_state(&dir, &key)
}

pub(crate) fn clear_status(session: &str, tab_id: u32) -> Result<()> {
    let key = tab_key(session, tab_id);
    let dir = status_dir()?;
    let _lock = acquire_lock(&dir, &key)?;
    let Some((socket_path, socket_identity)) = operation_socket(session, &dir, &key)? else {
        return Ok(());
    };
    let Some(current_title) = current_tab_title(session, tab_id)? else {
        delete_state(&dir, &key)?;
        return Ok(());
    };
    if let Some(base_name) = tab_name_without_status(&current_title) {
        ensure_session_socket(&socket_path, socket_identity)?;
        rename_tab(session, tab_id, base_name)?;
        ensure_session_socket(&socket_path, socket_identity)?;
    }
    delete_state(&dir, &key)
}

pub(crate) fn run_spinner(session: &str, tab_id: u32, token: &str) -> Result<()> {
    let key = tab_key(session, tab_id);
    let dir = status_dir()?;
    let mut frame = 1;
    let mut consecutive_failures = 0;
    let mut last_rendered_title: Option<String> = None;

    loop {
        thread::sleep(SPINNER_INTERVAL);
        let _lock = acquire_lock(&dir, &key)?;
        let Some(mut state) = read_state(&dir, &key)? else {
            return Ok(());
        };
        if state.token != token {
            return Ok(());
        }
        if ensure_session_socket(&state.socket_path, state.socket_identity).is_err() {
            delete_state(&dir, &key)?;
            return Ok(());
        }

        let current_title = match current_tab_title(session, state.tab_id) {
            Ok(Some(title)) => title,
            Ok(None) => {
                delete_state(&dir, &key)?;
                return Ok(());
            }
            Err(_) => {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    stop_failed_spinner(session, &dir, &key, &state)?;
                    return Ok(());
                }
                continue;
            }
        };
        let expected_title = last_rendered_title
            .get_or_insert_with(|| tab_name_with_spinner(&state.base_name, SPINNER_FRAMES[0]));
        let base_name = synchronized_base_name(&current_title, expected_title, &state.base_name);
        if base_name != state.base_name {
            state.base_name = base_name;
            write_state(&dir, &key, &state)?;
        }

        let title = tab_name_with_spinner(
            &state.base_name,
            SPINNER_FRAMES[frame % SPINNER_FRAMES.len()],
        );
        if rename_status_tab_checked(
            session,
            state.tab_id,
            &title,
            &state.socket_path,
            state.socket_identity,
        )
        .is_err()
        {
            if ensure_session_socket(&state.socket_path, state.socket_identity).is_err() {
                delete_state(&dir, &key)?;
                return Ok(());
            }
            consecutive_failures += 1;
            if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                stop_failed_spinner(session, &dir, &key, &state)?;
                return Ok(());
            }
            continue;
        }
        consecutive_failures = 0;
        last_rendered_title = Some(title);
        frame = (frame + 1) % SPINNER_FRAMES.len();
    }
}

fn stop_failed_spinner(
    session: &str,
    dir: &Path,
    key: &PaneKey,
    state: &SpinnerState,
) -> Result<()> {
    if ensure_session_socket(&state.socket_path, state.socket_identity).is_ok()
        && let Ok(Some(current_title)) = current_tab_title(session, state.tab_id)
        && let Some(base_name) = tab_name_without_status(&current_title)
    {
        let _ = rename_tab(session, state.tab_id, base_name);
    }
    delete_state(dir, key)
}

fn operation_socket(
    session: &str,
    dir: &Path,
    key: &PaneKey,
) -> Result<Option<(PathBuf, SocketIdentity)>> {
    if let Some(state) = read_state(dir, key)? {
        if ensure_session_socket(&state.socket_path, state.socket_identity).is_err() {
            delete_state(dir, key)?;
            return Ok(None);
        }
        return Ok(Some((state.socket_path, state.socket_identity)));
    }
    session_socket(session).map(Some)
}

fn rename_status_tab_checked(
    session: &str,
    tab_id: u32,
    title: &str,
    socket_path: &Path,
    socket_identity: SocketIdentity,
) -> Result<()> {
    ensure_session_socket(socket_path, socket_identity)?;
    rename_tab(session, tab_id, title)?;
    if let Err(error) = ensure_session_socket(socket_path, socket_identity) {
        rollback_rendered_status(session, tab_id, title);
        return Err(error);
    }
    Ok(())
}

fn rollback_rendered_status(session: &str, tab_id: u32, rendered_title: &str) {
    let Ok(Some(current_title)) = current_tab_title(session, tab_id) else {
        return;
    };
    if current_title != rendered_title {
        return;
    }
    if let Some(base_name) = tab_name_without_status(&current_title) {
        let _ = rename_tab(session, tab_id, base_name);
    }
}

fn tab_key(session: &str, tab_id: u32) -> PaneKey {
    PaneKey {
        backend: "zellij".to_string(),
        instance: session.to_string(),
        pane_id: format!("tab_{tab_id}"),
    }
}

fn status_dir() -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?
        .join("runtime")
        .join("zellij-status"))
}

fn state_path(dir: &Path, key: &PaneKey) -> PathBuf {
    dir.join(key.to_filename())
}

fn lock_path(dir: &Path, key: &PaneKey) -> PathBuf {
    dir.join(format!("{}.lock", key.to_filename()))
}

fn acquire_lock(dir: &Path, key: &PaneKey) -> Result<StatusLock> {
    fs::create_dir_all(dir)?;
    let path = lock_path(dir, key);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("Failed to open Zellij status lock: {}", path.display()))?;
    let lock = Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_file, errno)| errno)
        .with_context(|| format!("Failed to acquire Zellij status lock: {}", path.display()))?;
    Ok(StatusLock { _lock: lock })
}

fn read_state(dir: &Path, key: &PaneKey) -> Result<Option<SpinnerState>> {
    let path = state_path(dir, key);
    match fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(state) => Ok(Some(state)),
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "corrupted Zellij status state, deleting");
                let _ = fs::remove_file(path);
                Ok(None)
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Failed to read Zellij status state"),
    }
}

fn write_state(dir: &Path, key: &PaneKey, state: &SpinnerState) -> Result<()> {
    fs::create_dir_all(dir)?;
    let content = serde_json::to_vec(state)?;
    crate::state::write_atomic(&state_path(dir, key), &content)
}

fn delete_state(dir: &Path, key: &PaneKey) -> Result<()> {
    match fs::remove_file(state_path(dir, key)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Failed to delete Zellij status state"),
    }
}

fn spinner_process_is_running(state: &SpinnerState) -> bool {
    let output = Command::new("ps")
        .args(["-p", &state.pid.to_string(), "-o", "command="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return false;
    };
    output.status.success()
        && is_expected_spinner_command(&String::from_utf8_lossy(&output.stdout), &state.token)
}

fn is_expected_spinner_command(command: &str, token: &str) -> bool {
    command.contains("_zellij-status-spinner") && command.contains(token)
}

fn synchronized_base_name(current_title: &str, expected_title: &str, base_name: &str) -> String {
    if current_title == expected_title {
        base_name.to_string()
    } else {
        canonical_tab_name(current_title).to_string()
    }
}

fn animation_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn rename_tab(session: &str, tab_id: u32, title: &str) -> Result<()> {
    Cmd::new("zellij")
        .args(&[
            "--session",
            session,
            "action",
            "rename-tab-by-id",
            &tab_id.to_string(),
            title,
        ])
        .run()
        .with_context(|| format!("Failed to rename Zellij tab {tab_id}"))?;
    Ok(())
}

fn current_tab_title(session: &str, tab_id: u32) -> Result<Option<String>> {
    let output = Cmd::new("zellij")
        .args(&["--session", session, "action", "list-tabs", "--json"])
        .run_and_capture_stdout()
        .context("Failed to list Zellij tabs while animating status")?;
    let tabs: Vec<TabTitle> = serde_json::from_str(&output)
        .context("Failed to parse Zellij tabs while animating status")?;
    Ok(tabs
        .into_iter()
        .find(|tab| tab.tab_id == tab_id)
        .map(|tab| tab.name))
}
