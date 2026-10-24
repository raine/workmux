//! The configuration a remote workspace runs with.
//!
//! The node never reads the remote user's config or the worktree's
//! `.workmux.yaml`; it runs the laptop's effective config, rebuilt here from
//! an allowlist. Fields that name laptop resources (paths, devices, VM
//! settings, host commands) are dropped and reported, so a new config field
//! stays on the laptop until it is deliberately made portable.

use std::path::Path;

use crate::config::{
    Config, ConfigLocation, ContainerConfig, RemoteIsolation, SandboxBackend, SandboxConfig,
    SandboxRuntime,
};

/// A config for the node and the laptop settings it leaves behind.
pub struct Portable {
    pub config: Config,
    pub location: Option<ConfigLocation>,
    /// One line per dropped or changed setting, naming it but never its value.
    pub dropped: Vec<String>,
}

/// Build the node config from the laptop's effective `config`.
///
/// `remote_worktree` is the workspace's worktree on the node; the location
/// of a project config is rebased onto it.
pub fn portable_config(
    config: &Config,
    location: Option<&ConfigLocation>,
    isolation: RemoteIsolation,
    remote_worktree: &Path,
) -> Portable {
    let mut dropped = Vec::new();
    let mut portable = Config {
        panes: config.panes.clone(),
        post_create: config.post_create.clone(),
        pre_remove: config.pre_remove.clone(),
        agent: config.agent.clone(),
        agents: config.agents.clone(),
        agent_type: config.agent_type.clone(),
        selected_agent: config.selected_agent.clone(),
        status_format: config.status_format,
        status_icons: config.status_icons.clone(),
        prompt_file_only: config.prompt_file_only,
        ..Config::default()
    };

    match config.hook_shell.as_deref() {
        Some([program, ..]) if Path::new(program).is_absolute() => dropped
            .push("hook_shell: an absolute laptop path; hooks use the default shell".to_string()),
        _ => portable.hook_shell = config.hook_shell.clone(),
    }
    if config
        .agent
        .as_deref()
        .is_some_and(|agent| agent.trim_start().starts_with('/'))
    {
        dropped
            .push("agent: an absolute laptop path is kept but may not exist on the remote".into());
    }

    portable.sandbox = match isolation {
        RemoteIsolation::None => SandboxConfig {
            enabled: Some(false),
            ..SandboxConfig::default()
        },
        RemoteIsolation::Container => portable_sandbox(&config.sandbox, &mut dropped),
    };

    let location = location.map(|location| {
        let config_dir = remote_worktree.join(&location.rel_dir);
        let file_name = location
            .config_path
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_else(|| ".workmux.yaml".into());
        ConfigLocation {
            config_path: config_dir.join(file_name),
            config_dir,
            rel_dir: location.rel_dir.clone(),
        }
    });

    Portable {
        config: portable,
        location,
        dropped,
    }
}

fn portable_sandbox(sandbox: &SandboxConfig, dropped: &mut Vec<String>) -> SandboxConfig {
    let mut drop = |setting: &str, why: &str| dropped.push(format!("sandbox.{setting}: {why}"));
    let laptop_lima = sandbox.backend() == SandboxBackend::Lima;
    if laptop_lima {
        drop("backend", "lima runs on the laptop; the remote uses docker");
    }
    if sandbox.image.is_some() && laptop_lima {
        drop(
            "image",
            "a Lima image; the remote uses the default container image",
        );
    }
    if sandbox
        .container
        .runtime
        .is_some_and(|runtime| runtime != SandboxRuntime::Docker)
        && !laptop_lima
    {
        drop("container.runtime", "the remote always uses docker");
    }
    if sandbox.rpc_host.is_some() {
        drop("rpc_host", "the remote picks its own private address");
    }
    if sandbox.extra_mounts.as_ref().is_some_and(|m| !m.is_empty()) {
        drop("extra_mounts", "laptop paths");
    }
    if sandbox.agent_config_dir.is_some() {
        drop("agent_config_dir", "a laptop path");
    }
    if sandbox
        .host_commands
        .as_ref()
        .is_some_and(|c| !c.is_empty())
    {
        drop("host_commands", "host commands are off on remotes");
    }
    if sandbox.dangerously_allow_unsandboxed_host_exec == Some(true) {
        drop(
            "dangerously_allow_unsandboxed_host_exec",
            "host commands are off on remotes",
        );
    }
    if sandbox
        .env_passthrough
        .as_ref()
        .is_some_and(|e| !e.is_empty())
    {
        drop(
            "env_passthrough",
            "laptop environment values are not sent to remotes",
        );
    }
    let container = &sandbox.container;
    if container.devices.as_ref().is_some_and(|d| !d.is_empty()) {
        drop("container.devices", "laptop devices");
    }
    if container.oci_runtime.is_some() {
        drop("container.oci_runtime", "a laptop runtime");
    }

    SandboxConfig {
        enabled: Some(true),
        backend: Some(SandboxBackend::Container),
        target: sandbox.target.clone(),
        image: if laptop_lima {
            None
        } else {
            sandbox.image.clone()
        },
        env: sandbox.env.clone(),
        toolchain: sandbox.toolchain.clone(),
        network: sandbox.network.clone(),
        container: ContainerConfig {
            runtime: Some(SandboxRuntime::Docker),
            cpus: container.cpus,
            memory: container.memory.clone(),
            group_add: container.group_add.clone(),
            excluded_files: container.excluded_files.clone(),
            cap_add: container.cap_add.clone(),
            security_opt: container.security_opt.clone(),
            ..ContainerConfig::default()
        },
        ..SandboxConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExtraMount, PaneConfig};
    use std::path::PathBuf;

    fn laptop() -> Config {
        let mut config = Config {
            agent: Some("claude".into()),
            post_create: Some(vec!["make setup".into()]),
            panes: Some(vec![PaneConfig {
                command: Some("<agent>".into()),
                ..Default::default()
            }]),
            hook_shell: Some(vec!["/opt/homebrew/bin/fish".into(), "-c".into()]),
            main_branch: Some("trunk".into()),
            ..Config::default()
        };
        config.sandbox.enabled = Some(false);
        config.sandbox.image = Some("my/image:1".into());
        config.sandbox.rpc_host = Some("10.0.0.1".into());
        config.sandbox.env_passthrough = Some(vec!["GITHUB_TOKEN".into()]);
        config.sandbox.extra_mounts = Some(vec![ExtraMount::Path("/Users/me/data".into())]);
        config.sandbox.container.runtime = Some(SandboxRuntime::AppleContainer);
        config.sandbox.container.excluded_files = Some(vec![".env".into()]);
        config.sandbox.container.oci_runtime = Some("runsc".into());
        config
    }

    #[test]
    fn container_isolation_forces_docker_and_reports_drops() {
        let portable = portable_config(
            &laptop(),
            None,
            RemoteIsolation::Container,
            Path::new("/n/wt"),
        );
        let sandbox = &portable.config.sandbox;
        assert!(sandbox.is_enabled());
        assert_eq!(sandbox.backend(), SandboxBackend::Container);
        assert_eq!(sandbox.runtime(), SandboxRuntime::Docker);
        assert_eq!(sandbox.image.as_deref(), Some("my/image:1"));
        assert_eq!(
            sandbox.container.excluded_files.as_deref(),
            Some(&[".env".to_string()][..])
        );
        assert!(sandbox.rpc_host.is_none());
        assert!(sandbox.extra_mounts.is_none());
        assert!(sandbox.env_passthrough.is_none());
        assert!(sandbox.container.oci_runtime.is_none());
        assert!(portable.config.hook_shell.is_none());
        assert!(portable.config.main_branch.is_none());
        assert_eq!(portable.config.post_create, laptop().post_create);

        let report = portable.dropped.join("\n");
        for setting in [
            "sandbox.container.runtime",
            "sandbox.rpc_host",
            "sandbox.extra_mounts",
            "sandbox.env_passthrough",
            "sandbox.container.oci_runtime",
            "hook_shell",
        ] {
            assert!(report.contains(setting), "{setting} missing from {report}");
        }
        assert!(!report.contains("GITHUB_TOKEN"), "{report}");
        assert!(!report.contains("/Users/me"), "{report}");
    }

    #[test]
    fn lima_image_is_dropped() {
        let mut config = laptop();
        config.sandbox.backend = Some(SandboxBackend::Lima);
        config.sandbox.image = Some("file:///Users/me/base.qcow2".into());
        let portable = portable_config(
            &config,
            None,
            RemoteIsolation::Container,
            Path::new("/n/wt"),
        );
        assert!(portable.config.sandbox.image.is_none());
        assert_eq!(portable.config.sandbox.backend(), SandboxBackend::Container);
        let report = portable.dropped.join("\n");
        assert!(report.contains("sandbox.image"), "{report}");
        assert!(report.contains("sandbox.backend"), "{report}");
    }

    #[test]
    fn isolation_none_disables_the_sandbox_quietly() {
        let mut config = laptop();
        config.hook_shell = None;
        config.sandbox.enabled = Some(true);
        let portable = portable_config(&config, None, RemoteIsolation::None, Path::new("/n/wt"));
        assert!(!portable.config.sandbox.is_enabled());
        assert!(portable.dropped.is_empty(), "{:?}", portable.dropped);
    }

    #[test]
    fn location_is_rebased_onto_the_remote_worktree() {
        let location = ConfigLocation {
            config_path: PathBuf::from("/Users/me/repo/backend/.workmux.yaml"),
            config_dir: PathBuf::from("/Users/me/repo/backend"),
            rel_dir: PathBuf::from("backend"),
        };
        let portable = portable_config(
            &laptop(),
            Some(&location),
            RemoteIsolation::Container,
            Path::new("/n/wt"),
        );
        let location = portable.location.unwrap();
        assert_eq!(location.config_dir, PathBuf::from("/n/wt/backend"));
        assert_eq!(
            location.config_path,
            PathBuf::from("/n/wt/backend/.workmux.yaml")
        );
        assert_eq!(location.rel_dir, PathBuf::from("backend"));
    }
}
