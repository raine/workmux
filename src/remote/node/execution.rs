//! `node start-execution` and the node's tmux server.
//!
//! Each configured pane of a workspace runs in its own session of a tmux
//! server the node owns, so the laptop can relay every pane into a pane of
//! its own window. The server's config makes it transparent: no prefix or
//! status line, and mouse, clipboard, titles and focus pass through to the
//! laptop's terminal.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use super::{
    Execution, ExecutionState, NODE_ROOT_ENV, NodeRoot, PaneProgress, refuse, secrets,
    stop_containers_mounting, validate_id,
};
use crate::config::{Config, PaneConfig, RemoteIsolation};
use crate::multiplexer::types::ResumeMode;
use crate::multiplexer::{Multiplexer, PaneSetupOptions, tmux::TmuxBackend};
use crate::remote::protocol::{Secrets, StartRequest, StartResponse, code, session_name};
use crate::shell::shell_quote;

/// Longest socket path tmux can bind on Linux and macOS, with a margin.
const MAX_SOCKET_PATH: usize = 100;

const TMUX_CONF: &str = "\
# Written by workmux. Sessions on this server are relayed into panes of a
# local tmux window, so this server stays out of the way.
set -g prefix None
set -g prefix2 None
set -g status off
set -g mouse on
set -g set-clipboard on
set -g allow-passthrough on
set -g focus-events on
set -g set-titles on
# A kitty graphics query an agent sends to probe the terminal arrives as an
# APC sequence, which tmux takes as the pane title; it is not relayed.
set -g set-titles-string '#{?#{m/r:^G[a-zA-Z]=[^;]*;,#{pane_title}},,#{pane_title}}'
set -g window-size latest
set -g history-limit 50000
set -s escape-time 0
set -s extended-keys on
set -sa terminal-features '*:clipboard:title:focus:RGB'
";

/// Socket of the node tmux server: `run/tmux.sock` under the root, or a
/// private per-user directory in `/tmp` when that path is too long to bind.
pub fn tmux_socket(root: &NodeRoot) -> Result<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let preferred_dir = root.path.join("run");
    let preferred = preferred_dir.join("tmux.sock");
    if preferred.as_os_str().len() <= MAX_SOCKET_PATH {
        crate::remote::transport::ensure_private_dir(&preferred_dir, uid)?;
        return Ok(preferred);
    }
    let digest = Sha256::digest(root.path.as_os_str().as_encoded_bytes());
    let hash: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let dir = PathBuf::from(format!("/tmp/workmux-node-{uid}"));
    crate::remote::transport::ensure_private_dir(&dir, uid)?;
    Ok(dir.join(format!("{hash}.sock")))
}

pub fn tmux(socket: &Path) -> Command {
    let mut cmd = Command::new("tmux");
    cmd.arg("-S").arg(socket);
    cmd
}

pub fn session_alive(socket: &Path, name: &str) -> bool {
    tmux(socket)
        .args(["has-session", "-t", &format!("={name}")])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Sessions on the node tmux server; none when no server runs.
pub fn sessions(socket: &Path) -> HashSet<String> {
    tmux(socket)
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Kill the sessions of execution `ex_id`, live or not.
pub fn kill_execution(root: &NodeRoot, socket: &Path, ex_id: &str) {
    let prefix = format!("{ex_id}-");
    for name in sessions(socket) {
        if name.starts_with(&prefix) {
            let _ = tmux(socket)
                .args(["kill-session", "-t", &format!("={name}")])
                .output();
        }
    }
    AgentMarker::sweep(root, socket);
}

/// The panes of a workspace that get a session, with their configured
/// index: the agent is placed as `setup_panes` would, a pane without a split
/// direction after the first is skipped, and no panes means one shell.
pub fn pane_plan(config: &Config, agent: Option<&str>) -> Vec<(usize, PaneConfig)> {
    let configured = config.panes.clone().unwrap_or_default();
    let resolved = crate::workflow::resolve_pane_configuration(&configured, config, agent);
    if resolved.is_empty() {
        return vec![(0, PaneConfig::default())];
    }
    resolved
        .into_iter()
        .enumerate()
        .filter(|(index, pane)| *index == 0 || pane.split.is_some())
        .collect()
}

/// `run/agents/<pane>` under a node root: present, holding the time it was
/// written, while a supervisor's container runtime runs in that pane of the
/// node tmux server and reads its input. Removed when the runtime exits.
pub struct AgentMarker(Option<PathBuf>);

impl AgentMarker {
    fn path(root: &NodeRoot, pane_id: &str) -> Option<PathBuf> {
        let number = pane_id.strip_prefix('%')?;
        number
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| root.path.join("run").join("agents").join(number))
    }

    /// Write the marker of `pane_id`; failures only lose the marker.
    pub fn write(root: &NodeRoot, pane_id: &str) -> Self {
        let Some(path) = Self::path(root, pane_id) else {
            return Self(None);
        };
        let written = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                crate::util::write_atomic(&path, crate::remote::cache::now().to_string().as_bytes())
            });
        match written {
            Ok(()) => Self(Some(path)),
            Err(err) => {
                tracing::warn!("failed to write {}: {err:#}", path.display());
                Self(None)
            }
        }
    }

    /// When the marker of `pane_id` was written (node clock), if it exists.
    pub fn read(root: &NodeRoot, pane_id: &str) -> Option<u64> {
        fs::read_to_string(Self::path(root, pane_id)?)
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Remove the markers of panes gone from the node tmux server: a
    /// supervisor killed with its session leaves its marker behind.
    pub fn sweep(root: &NodeRoot, socket: &Path) {
        let dir = root.path.join("run").join("agents");
        // Listed before the panes, so a marker written meanwhile is for a
        // pane the listing has.
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        let markers: Vec<_> = entries.flatten().map(|entry| entry.file_name()).collect();
        let live: HashSet<String> = tmux(socket)
            .args(["list-panes", "-a", "-F", "#{pane_id}"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|id| id.strip_prefix('%'))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        for name in markers {
            if !name.to_str().is_some_and(|number| live.contains(number)) {
                let _ = fs::remove_file(dir.join(name));
            }
        }
    }
}

impl Drop for AgentMarker {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

/// Index of the first pane of [`pane_plan`] that runs the agent.
pub fn agent_pane(config: &Config, agent: Option<&str>, working_dir: &Path) -> Option<usize> {
    pane_plan(config, agent)
        .into_iter()
        .find_map(|(index, pane)| {
            crate::multiplexer::util::resolve_pane_command_with_config(
                pane.command.as_deref(),
                true,
                None,
                working_dir,
                config,
                agent,
                "sh",
            )
            .is_some_and(|resolved| resolved.selected_agent.is_some())
            .then_some(index)
        })
}

/// [`pane_plan`] as the laptop records it to lay out the window.
pub fn layout(config: &Config, agent: Option<&str>) -> Vec<crate::remote::cache::LayoutPane> {
    pane_plan(config, agent)
        .into_iter()
        .map(|(index, pane)| crate::remote::cache::LayoutPane { index, pane })
        .collect()
}

pub fn start(root: &NodeRoot, request: StartRequest) -> Result<StartResponse> {
    validate_id(&request.ws_id, 'w', "workspace id")?;
    validate_id(&request.ex_id, 'x', "execution id")?;
    let _lock = root.lock_workspace(&request.ws_id)?;
    let mut workspace = root.load(&request.ws_id)?.ok_or_else(|| {
        refuse(
            code::NOT_FOUND,
            format!("workspace {} does not exist on the node", request.ws_id),
        )
    })?;
    if !workspace.progress.ready {
        return Err(refuse(
            code::FAILED,
            format!("workspace {} is not provisioned yet", request.ws_id),
        ));
    }
    let socket = tmux_socket(root)?;

    let boot_id = super::boot_id();
    let resume = match &workspace.execution {
        Some(execution) if execution.state == ExecutionState::Running => {
            let live = execution
                .panes
                .iter()
                .any(|pane| session_alive(&socket, &session_name(&execution.ex_id, pane.index)));
            if live {
                return Ok(StartResponse {
                    ex_id: execution.ex_id.clone(),
                    panes: execution.panes.iter().map(|pane| pane.index).collect(),
                    reused: true,
                    after_reboot: false,
                    continued: false,
                });
            }
            false
        }
        Some(execution) => execution.ex_id == request.ex_id,
        None => false,
    };
    let pin = std::env::current_exe().context("Failed to locate the node binary")?;
    if !resume {
        let mut continues = false;
        if let Some(previous) = workspace.execution.take() {
            continues = previous.ended_by_reboot(boot_id.as_deref());
            kill_execution(root, &socket, &previous.ex_id);
        }
        if workspace.isolation == RemoteIsolation::Container {
            stop_containers_mounting(&workspace.worktree_path)?;
        }
        workspace.execution = Some(Execution {
            ex_id: request.ex_id.clone(),
            state: ExecutionState::Starting,
            panes: Vec::new(),
            boot_id: boot_id.clone(),
            pin: pin
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            continues,
            agent_kind: None,
        });
        root.save(&workspace)?;
    }
    // An agent the host rebooted under picks up its conversation again.
    let resume_mode = match &workspace.execution {
        Some(execution) if execution.continues => ResumeMode::Continue,
        _ => ResumeMode::None,
    };

    let config_path = root.config_path(&request.ws_id);
    let (config, _) = crate::frozen_config::load(&config_path)?;
    let agent_kind = workspace.agent_kind(&config);
    workspace
        .execution
        .as_mut()
        .expect("execution recorded")
        .agent_kind = agent_kind.map(str::to_string);
    let plan = pane_plan(&config, workspace.agent.as_deref());
    let conf = root.path.join("tmux.conf");
    if fs::read_to_string(&conf).ok().as_deref() != Some(TMUX_CONF) {
        crate::util::write_atomic(&conf, TMUX_CONF.as_bytes())?;
    }

    super::trust::ensure_for_execution(root, &workspace, &config);

    let supervisor = format!(
        "{} sandbox run --node-root {} --node-workspace {}",
        shell_quote(&pin.to_string_lossy()),
        shell_quote(&root.path.to_string_lossy()),
        request.ws_id
    );
    // Without a container, agent hooks and commands typed in any pane run
    // the node's `workmux`, which must use the node store.
    let status_bin = match workspace.isolation {
        RemoteIsolation::None => Some(write_status_wrapper(root, &pin)?),
        RemoteIsolation::Container => None,
    };
    let prompt = workspace
        .prompt_path
        .clone()
        .filter(|_| request.use_prompt && !workspace.prompt_file_only);
    let working_dir = workspace.working_dir().to_path_buf();
    let worktree = workspace.worktree_path.clone();
    let backend = TmuxBackend::for_socket(&socket.to_string_lossy());
    // Containers get the workspace's secrets from their supervisor; panes
    // without one get them in their session's environment.
    let session_secrets = match workspace.isolation {
        RemoteIsolation::None => secrets::for_execution(root, &request.ws_id, agent_kind)?,
        RemoteIsolation::Container => Default::default(),
    };

    for (index, pane) in &plan {
        let execution = workspace.execution.as_ref().expect("execution recorded");
        let name = session_name(&execution.ex_id, *index);
        let dispatched = execution
            .panes
            .iter()
            .any(|recorded| recorded.index == *index);
        if dispatched && session_alive(&socket, &name) {
            continue;
        }
        let _ = tmux(&socket)
            .args(["kill-session", "-t", &format!("={name}")])
            .output();
        let spawn_env = (workspace.isolation == RemoteIsolation::None)
            .then_some((root.path.as_path(), request.ws_id.as_str()));
        let pane_id = new_session(&socket, &conf, &name, &working_dir, &config_path, spawn_env)?;
        if !session_secrets.0.is_empty() {
            set_session_env(root, &socket, &name, &session_secrets)?;
        }
        let single = PaneConfig {
            split: None,
            target: None,
            size: None,
            percentage: None,
            ..pane.clone()
        };
        backend
            .setup_panes(
                &pane_id,
                &[single],
                &working_dir,
                PaneSetupOptions {
                    run_commands: request.run_commands,
                    prompt_file_path: prompt.as_deref(),
                    worktree_root: Some(&worktree),
                    lima_vm_name: None,
                    resume_mode: resume_mode.clone(),
                    sandbox_supervisor: Some(&supervisor),
                    path_prefix: status_bin.as_deref(),
                },
                &config,
                workspace.agent.as_deref(),
            )
            .with_context(|| format!("Failed to start pane {index}"))?;
        let execution = workspace.execution.as_mut().expect("execution recorded");
        execution.panes.retain(|recorded| recorded.index != *index);
        execution.panes.push(PaneProgress { index: *index });
        root.save(&workspace)?;
    }

    let continue_flag = crate::multiplexer::agent::resolve_profile(agent_kind)
        .continue_flag()
        .is_some();
    let execution = workspace.execution.as_mut().expect("execution recorded");
    execution.state = ExecutionState::Running;
    execution.panes.sort_by_key(|pane| pane.index);
    let response = StartResponse {
        ex_id: execution.ex_id.clone(),
        panes: execution.panes.iter().map(|pane| pane.index).collect(),
        reused: false,
        after_reboot: execution.continues,
        continued: execution.continues && request.run_commands && continue_flag,
    };
    root.save(&workspace)?;
    Ok(response)
}

/// Write `run/bin/workmux`, which runs `pin` against the node store, and
/// return its directory.
fn write_status_wrapper(root: &NodeRoot, pin: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = root.path.join("run").join("bin");
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let script = format!(
        "#!/bin/sh\n# Written by workmux.\nexport XDG_STATE_HOME={}\nexec {} \"$@\"\n",
        shell_quote(&root.path.join("state").to_string_lossy()),
        shell_quote(&pin.to_string_lossy())
    );
    let path = dir.join("workmux");
    if fs::read_to_string(&path).ok().as_deref() != Some(script.as_str()) {
        crate::util::write_atomic(&path, script.as_bytes())?;
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("Failed to make {} executable", path.display()))?;
    Ok(dir)
}

/// Create a detached session and return its pane. The variables that steer
/// node verbs are kept out of the server's global environment; the session
/// gets its workspace config explicitly, and with `spawn_env` the node root
/// and workspace its agent spawns siblings from.
fn new_session(
    socket: &Path,
    conf: &Path,
    name: &str,
    cwd: &Path,
    config_path: &Path,
    spawn_env: Option<(&Path, &str)>,
) -> Result<String> {
    let mut cmd = tmux(socket);
    cmd.arg("-f")
        .arg(conf)
        // Use tmux's default size until a viewer attaches. An oversized
        // initial screen can push startup output into history when reflowed
        // to the viewer's narrower terminal.
        .args(["new-session", "-d", "-P", "-F", "#{pane_id}"])
        .args(["-s", name, "-c"])
        .arg(cwd)
        .arg("-e")
        .arg(format!(
            "{}={}",
            crate::frozen_config::FROZEN_CONFIG_ENV,
            config_path.display()
        ))
        .env_remove(crate::frozen_config::FROZEN_CONFIG_ENV)
        .env_remove(super::spawn::SESSION_ROOT_ENV)
        .env_remove(super::spawn::SESSION_WORKSPACE_ENV)
        .env_remove(NODE_ROOT_ENV)
        .env_remove("XDG_STATE_HOME")
        .env_remove("TMUX");
    if let Some((root, ws_id)) = spawn_env {
        cmd.arg("-e")
            .arg(format!(
                "{}={}",
                super::spawn::SESSION_ROOT_ENV,
                root.display()
            ))
            .arg("-e")
            .arg(format!("{}={ws_id}", super::spawn::SESSION_WORKSPACE_ENV));
    }
    let output = cmd.output().context("Failed to run tmux")?;
    if !output.status.success() {
        return Err(anyhow!(
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Set `secrets` in a session's environment through a private file, so no
/// value appears in argv; panes spawned afterwards inherit them.
fn set_session_env(root: &NodeRoot, socket: &Path, name: &str, secrets: &Secrets) -> Result<()> {
    let file = secrets::tmux_file(root, name, secrets)?;
    let output = tmux(socket)
        .arg("source-file")
        .arg(&*file)
        .output()
        .context("Failed to run tmux")?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to set the environment of session {name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SplitDirection;

    #[test]
    fn fresh_server_uses_the_default_terminal_size() {
        if which::which("tmux").is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("tmux.sock");
        let conf = dir.path().join("tmux.conf");
        fs::write(&conf, TMUX_CONF).unwrap();
        let result = new_session(
            &socket,
            &conf,
            "first",
            dir.path(),
            &dir.path().join("config.json"),
            None,
        );
        let dimensions = tmux(&socket)
            .args([
                "display-message",
                "-p",
                "-t",
                "=first:",
                "#{pane_width}x#{pane_height}",
            ])
            .output();
        let _ = tmux(&socket).arg("kill-server").output();

        result.unwrap();
        let dimensions = dimensions.unwrap();
        assert!(dimensions.status.success());
        assert_eq!(String::from_utf8_lossy(&dimensions.stdout).trim(), "80x24");
    }

    #[test]
    fn plan_skips_unsplit_panes_and_defaults_to_one_shell() {
        let config = Config::default();
        assert_eq!(pane_plan(&config, None).len(), 1);

        let config = Config {
            panes: Some(vec![
                PaneConfig {
                    command: Some("<agent>".into()),
                    ..Default::default()
                },
                PaneConfig::default(),
                PaneConfig {
                    split: Some(SplitDirection::Horizontal),
                    ..Default::default()
                },
            ]),
            ..Config::default()
        };
        let indices: Vec<usize> = pane_plan(&config, None).iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, [0, 2]);
    }

    #[test]
    fn an_agent_marker_lasts_while_it_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let root = NodeRoot {
            path: dir.path().to_path_buf(),
        };
        let marker = AgentMarker::write(&root, "%12");
        assert!(AgentMarker::read(&root, "%12").is_some_and(|at| at > 0));
        assert_eq!(AgentMarker::read(&root, "%13"), None);
        drop(marker);
        assert_eq!(AgentMarker::read(&root, "%12"), None);
        // Only tmux pane ids name a file.
        drop(AgentMarker::write(&root, "../x"));
        assert!(!dir.path().join("run/x").exists());
        // Without a tmux server no pane is live.
        std::mem::forget(AgentMarker::write(&root, "%14"));
        AgentMarker::sweep(&root, &dir.path().join("no-server.sock"));
        assert_eq!(AgentMarker::read(&root, "%14"), None);
    }

    #[test]
    fn the_agent_pane_is_the_first_that_runs_the_agent() {
        let dir = std::env::temp_dir();
        let pane = |command: &str, split| PaneConfig {
            command: Some(command.into()),
            split,
            ..Default::default()
        };
        let config = Config {
            agent: Some("codex".into()),
            panes: Some(vec![
                pane("bash", None),
                pane("<agent>", Some(SplitDirection::Horizontal)),
                pane("<agent>", Some(SplitDirection::Vertical)),
            ]),
            ..Config::default()
        };
        assert_eq!(agent_pane(&config, None, &dir), Some(1));
        let config = Config {
            panes: Some(vec![pane("htop", None)]),
            ..config
        };
        assert_eq!(agent_pane(&config, None, &dir), None);
    }

    #[test]
    fn long_roots_fall_back_to_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let long = dir.path().join("x".repeat(120));
        fs::create_dir_all(&long).unwrap();
        let socket = tmux_socket(&NodeRoot::at(long).unwrap()).unwrap();
        let text = socket.to_string_lossy();
        assert!(text.starts_with("/tmp/workmux-node-"), "{text}");
        assert!(socket.as_os_str().len() <= MAX_SOCKET_PATH);
    }
}
