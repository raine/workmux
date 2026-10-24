//! `status-hooks`: the workmux status hooks of the agents remote executions
//! run, in the directories those agents read on the node.
//!
//! Containers mount the node user's default agent directories, whatever the
//! node user's environment selects, plus OpenCode's config directory. With
//! `isolation: none` agents run as the node user, so their hooks live where
//! `workmux setup` would put them.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::agent_setup::{self, Agent, StatusCheck, opencode};
use crate::config::{RemoteIsolation, SandboxConfig};

/// Agents remote containers run, by sandbox agent name.
const CONTAINER_AGENTS: &[(&str, Agent)] = &[
    ("claude", Agent::Claude),
    ("codex", Agent::Codex),
    ("gemini", Agent::Gemini),
    ("opencode", Agent::OpenCode),
    ("pi", Agent::Pi),
    ("omp", Agent::Omp),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusHooksRequest {
    pub isolation: RemoteIsolation,
    /// Install missing and outdated hooks instead of only reporting them.
    pub install: bool,
    /// Image containers run OpenCode from, whose version decides the
    /// plugin layout.
    pub opencode_image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusHooksResponse {
    pub agents: Vec<AgentHooks>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHooks {
    pub agent: Agent,
    /// Directory the hooks live in, when the agent reads a fixed one.
    pub dir: Option<String>,
    pub state: HookState,
    /// Why the state is `unknown` or `error`.
    pub detail: Option<String>,
    /// This call installed or updated the hooks.
    pub changed: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HookState {
    Installed,
    Outdated,
    Missing,
    /// The node cannot tell which hooks the agent needs.
    Unknown,
    Error,
}

pub fn status_hooks(request: StatusHooksRequest) -> Result<StatusHooksResponse> {
    let agents = match request.isolation {
        RemoteIsolation::Container => container_dirs()
            .into_iter()
            .map(|(agent, dir)| match (agent, dir) {
                (_, None) => report(
                    agent,
                    None,
                    Err("cannot locate its directory".into()),
                    false,
                ),
                (Agent::OpenCode, Some(dir)) => match version_in(&request.opencode_image) {
                    Ok(version) => in_dir(agent, &dir, Some(&version), request.install),
                    Err(detail) => AgentHooks {
                        agent,
                        dir: Some(dir.display().to_string()),
                        state: HookState::Unknown,
                        detail: Some(detail),
                        changed: false,
                    },
                },
                (_, Some(dir)) => in_dir(agent, &dir, None, request.install),
            })
            .collect(),
        RemoteIsolation::None => agent_setup::check_all_for_setup()
            .into_iter()
            .map(|check| on_host(check.agent, check.status, request.install))
            .collect(),
    };
    Ok(StatusHooksResponse { agents })
}

/// Where each container agent's hooks live on the node: the directory its
/// container mounts.
fn container_dirs() -> Vec<(Agent, Option<PathBuf>)> {
    let sandbox = SandboxConfig::default();
    CONTAINER_AGENTS
        .iter()
        .map(|&(name, agent)| {
            let dir = match agent {
                Agent::OpenCode => agent_setup::opencode::opencode_config_dir(),
                _ => sandbox.resolved_agent_config_dir(name),
            };
            (agent, dir)
        })
        .collect()
}

/// What `opencode --version` prints in `image`. OpenCode's plugin layout
/// follows its major version, so its hooks wait until the image is on the
/// node.
fn version_in(image: &str) -> Result<String, String> {
    let present = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !present {
        return Err(format!(
            "its plugin layout depends on the OpenCode in {image}, which is not on the node yet; \
             run setup again once it is"
        ));
    }
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--pull=never",
            "--entrypoint",
            "opencode",
            image,
            "--version",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("cannot run docker: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`opencode --version` failed in {image}: {}",
            stderr.lines().next().unwrap_or("").trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Check, and with `install` install, `agent`'s hooks in `dir`; `version` is
/// what OpenCode's `--version` printed.
fn in_dir(agent: Agent, dir: &Path, version: Option<&str>, install: bool) -> AgentHooks {
    let check = || {
        match version {
            Some(version) => opencode::check_dir_for(dir, version),
            None => agent_setup::check_in(agent, dir),
        }
        .map_err(|err| format!("{err:#}"))
    };
    let mut status = check();
    let changed = install && needs_install(&status);
    if changed {
        status = match version {
            Some(version) => opencode::install_dir_for(dir, version),
            None => agent_setup::install_in(agent, dir),
        }
        .map_err(|err| format!("{err:#}"))
        .and_then(|_| check());
    }
    report(agent, Some(dir), status, changed)
}

fn on_host(agent: Agent, status: StatusCheck, install: bool) -> AgentHooks {
    let mut status = match status {
        StatusCheck::Error(err) => Err(err),
        status => Ok(status),
    };
    let changed = install && needs_install(&status);
    if changed {
        status = agent_setup::install(agent)
            .map_err(|err| format!("{err:#}"))
            .and_then(|_| {
                let check = agent_setup::check_all_for_setup()
                    .into_iter()
                    .find(|check| check.agent == agent);
                match check.map(|check| check.status) {
                    Some(StatusCheck::Error(err)) => Err(err),
                    Some(status) => Ok(status),
                    None => Ok(StatusCheck::NotInstalled),
                }
            });
    }
    report(agent, None, status, changed)
}

fn needs_install(status: &Result<StatusCheck, String>) -> bool {
    matches!(
        status,
        Ok(StatusCheck::NotInstalled | StatusCheck::UpdateAvailable)
    )
}

fn report(
    agent: Agent,
    dir: Option<&Path>,
    status: Result<StatusCheck, String>,
    changed: bool,
) -> AgentHooks {
    let (state, detail) = match status {
        Ok(StatusCheck::Installed) => (HookState::Installed, None),
        Ok(StatusCheck::UpdateAvailable) => (HookState::Outdated, None),
        Ok(StatusCheck::NotInstalled) => (HookState::Missing, None),
        Ok(StatusCheck::Error(err)) | Err(err) => (HookState::Error, Some(err)),
    };
    AgentHooks {
        agent,
        dir: dir.map(|dir| dir.display().to_string()),
        state,
        detail,
        changed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installing_in_a_directory_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let agents = [
            (Agent::Claude, None),
            (Agent::Codex, None),
            (Agent::Gemini, None),
            (Agent::Pi, None),
            (Agent::Omp, None),
            (Agent::OpenCode, Some("1.14.2\n")),
            (Agent::OpenCode, Some("opencode v2.0.1\n")),
        ];
        for (agent, version) in agents {
            let dir = tmp.path().join(format!("{agent:?}{version:?}"));
            let missing = in_dir(agent, &dir, version, false);
            assert_eq!(missing.state, HookState::Missing, "{agent:?}");
            assert!(!missing.changed);

            let installed = in_dir(agent, &dir, version, true);
            assert_eq!(
                installed.state,
                HookState::Installed,
                "{agent:?}: {installed:?}"
            );
            assert!(installed.changed);

            let again = in_dir(agent, &dir, version, true);
            assert_eq!(again.state, HookState::Installed, "{agent:?}");
            assert!(!again.changed, "{agent:?} reinstalled");
        }
    }

    #[test]
    fn installing_keeps_existing_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = tmp.path().join("settings.json");
        std::fs::write(&settings, r#"{"theme":"dark"}"#).unwrap();
        let installed = in_dir(Agent::Claude, tmp.path(), None, true);
        assert_eq!(installed.state, HookState::Installed);
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(value["theme"], "dark");
        assert!(value["hooks"].is_object());
    }

    #[test]
    fn container_dirs_cover_every_container_agent() {
        let names: Vec<&str> = CONTAINER_AGENTS.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, crate::sandbox::KNOWN_AGENTS);
        for (agent, dir) in container_dirs() {
            assert!(dir.is_some(), "{agent:?}");
        }
    }
}
