//! Remote nodes: SSH-reachable Linux hosts that run agents.
//!
//! The laptop drives a node over a [`transport::Transport`]. Shell probes
//! cover everything that must work before a node binary exists (platform
//! detection, installation, doctor checks); everything else goes through the
//! pinned node binary's `workmux node <verb> --json` entry point.

pub mod agents;
pub mod attach;
pub mod cache;
pub mod client;
pub mod credentials;
pub mod doctor;
pub mod git;
pub mod homecoming;
pub mod hooks;
pub mod install;
pub mod lifecycle;
pub mod node;
pub mod observe;
pub mod portable;
pub mod protocol;
pub mod transport;

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};

use crate::config::{Config, RemoteIsolation};
use crate::shell::shell_quote;

/// Default node root on the remote.
pub const DEFAULT_NODE_PATH: &str = "~/.workmux-node";

/// A validated `remotes.<name>` entry.
#[derive(Debug, Clone)]
pub struct RemoteSpec {
    pub name: String,
    pub host: String,
    /// Node root as written in config; a leading `~` is expanded on the remote.
    pub path: String,
    pub isolation: RemoteIsolation,
    /// Local binary to upload, with `~` expanded.
    pub binary: Option<PathBuf>,
}

impl RemoteSpec {
    /// Look up and validate a configured remote.
    pub fn resolve(config: &Config, name: &str) -> Result<Self> {
        let remote = config
            .remotes
            .get(name)
            .ok_or_else(|| unknown_remote_error(config, name))?;
        validate_name(name)?;
        validate_host(name, &remote.host)?;
        let path = remote
            .path
            .clone()
            .unwrap_or_else(|| DEFAULT_NODE_PATH.to_string());
        validate_path(name, &path)?;
        let binary = remote
            .binary
            .as_deref()
            .map(|binary| expand_local_tilde(name, binary))
            .transpose()?;
        Ok(Self {
            name: name.to_string(),
            host: remote.host.clone(),
            path,
            isolation: remote.isolation.unwrap_or_default(),
            binary,
        })
    }

    /// A node script: `$root` is the node root with `~` expanded, followed by
    /// `vars` as shell variables, then `body`.
    pub fn script(&self, vars: &[(&str, &str)], body: &str) -> String {
        let mut script = format!(
            "root={}\ncase $root in \"~\") root=$HOME ;; \"~/\"*) root=$HOME/${{root#\"~/\"}} ;; esac\n",
            shell_quote(&self.path)
        );
        for (name, value) in vars {
            script.push_str(&format!("{name}={}\n", shell_quote(value)));
        }
        script.push_str(body);
        script
    }
}

fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        bail!(
            "Invalid remote name '{name}': use letters, digits, '.', '_' and '-', \
             starting with a letter or digit"
        );
    }
    if name == "local" {
        bail!("Invalid remote name 'local': it is reserved for `placement: local`");
    }
    Ok(())
}

fn validate_host(name: &str, host: &str) -> Result<()> {
    if host.trim().is_empty() {
        bail!("remotes.{name}.host is empty");
    }
    if host.starts_with('-') {
        bail!("remotes.{name}.host must not start with '-': {host}");
    }
    if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("remotes.{name}.host must not contain whitespace: {host:?}");
    }
    Ok(())
}

fn validate_path(name: &str, path: &str) -> Result<()> {
    let valid = path == "~" || path.starts_with("~/") || path.starts_with('/');
    if !valid {
        bail!("remotes.{name}.path must be absolute or start with '~/': {path}");
    }
    if path.chars().any(char::is_control) {
        bail!("remotes.{name}.path must not contain control characters");
    }
    Ok(())
}

fn expand_local_tilde(name: &str, binary: &str) -> Result<PathBuf> {
    if binary == "~" || binary.starts_with("~/") {
        let home = home::home_dir()
            .ok_or_else(|| anyhow!("remotes.{name}.binary: cannot determine home directory"))?;
        return Ok(home.join(binary.trim_start_matches('~').trim_start_matches('/')));
    }
    Ok(PathBuf::from(binary))
}

fn configured_names(config: &Config) -> String {
    config
        .remotes
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

fn unknown_remote_error(config: &Config, name: &str) -> anyhow::Error {
    if config.remotes.is_empty() {
        anyhow!(
            "Unknown remote '{name}': no remotes are configured. Define remotes.{name} \
             in your global config (~/.config/workmux/config.yaml)"
        )
    } else {
        anyhow!(
            "Unknown remote '{name}' (configured: {})",
            configured_names(config)
        )
    }
}

/// Where a new agent runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    Local,
    Remote(String),
}

/// Resolve where a new agent runs from `--remote [NAME]`, `--local` and the
/// `placement` config.
///
/// Without any configured remotes, `placement` is ignored so a project config
/// that names a remote cannot change behaviour for users who have none.
pub fn resolve_placement(
    config: &Config,
    remote_flag: Option<Option<&str>>,
    local_flag: bool,
) -> Result<Placement> {
    if local_flag {
        return Ok(Placement::Local);
    }
    let placement = config
        .placement
        .as_deref()
        .map(str::trim)
        .filter(|placement| !placement.is_empty() && *placement != "local");

    match remote_flag {
        Some(Some(name)) => {
            if config.remotes.contains_key(name) {
                Ok(Placement::Remote(name.to_string()))
            } else {
                bail!(
                    "{}. If '{name}' is the branch name, put it before --remote or name \
                     the remote explicitly (--remote <remote> {name})",
                    unknown_remote_error(config, name)
                )
            }
        }
        Some(None) => {
            if let Some(name) = placement.filter(|name| config.remotes.contains_key(*name)) {
                return Ok(Placement::Remote(name.to_string()));
            }
            let mut names = config.remotes.keys();
            match (names.next(), names.next()) {
                (Some(only), None) => Ok(Placement::Remote(only.clone())),
                (None, _) => bail!(
                    "--remote: no remotes are configured. Define one under `remotes:` in \
                     your global config (~/.config/workmux/config.yaml)"
                ),
                _ => bail!(
                    "--remote needs a name when several remotes are configured ({}): \
                     use --remote <name>",
                    configured_names(config)
                ),
            }
        }
        None => match placement {
            None => Ok(Placement::Local),
            Some(name) if config.remotes.is_empty() => {
                tracing::debug!(placement = name, "placement ignored: no remotes configured");
                Ok(Placement::Local)
            }
            Some(name) if config.remotes.contains_key(name) => {
                Ok(Placement::Remote(name.to_string()))
            }
            Some(name) => bail!(
                "placement '{name}' does not name a configured remote (configured: {}). \
                 Fix `placement` in your config or pass --local",
                configured_names(config)
            ),
        },
    }
}

/// Fail when `placement` sends new agents to a remote, for creation paths
/// that only make local agents.
pub fn reject_remote_placement(config: &Config) -> Result<()> {
    match resolve_placement(config, None, false)? {
        Placement::Local => Ok(()),
        Placement::Remote(name) => bail!(
            "the dashboard creates local agents only, and `placement` selects remote \
             '{name}'; use `workmux add --remote {name}` or `workmux add --local`"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemoteConfig;

    fn remote(host: &str) -> RemoteConfig {
        RemoteConfig {
            host: host.to_string(),
            path: None,
            isolation: None,
            binary: None,
        }
    }

    fn config_with(remotes: &[&str], placement: Option<&str>) -> Config {
        Config {
            remotes: remotes
                .iter()
                .map(|name| (name.to_string(), remote("example")))
                .collect(),
            placement: placement.map(str::to_string),
            ..Config::default()
        }
    }

    #[test]
    fn no_flags_and_no_placement_is_local() {
        let config = config_with(&["hetzner"], None);
        assert_eq!(
            resolve_placement(&config, None, false).unwrap(),
            Placement::Local
        );
    }

    #[test]
    fn placement_local_is_local() {
        let config = config_with(&["hetzner"], Some("local"));
        assert_eq!(
            resolve_placement(&config, None, false).unwrap(),
            Placement::Local
        );
    }

    #[test]
    fn placement_naming_remote_selects_it() {
        let config = config_with(&["hetzner", "box"], Some("box"));
        assert_eq!(
            resolve_placement(&config, None, false).unwrap(),
            Placement::Remote("box".into())
        );
    }

    #[test]
    fn placement_is_ignored_without_remotes() {
        let config = config_with(&[], Some("hetzner"));
        assert_eq!(
            resolve_placement(&config, None, false).unwrap(),
            Placement::Local
        );
    }

    #[test]
    fn unknown_placement_with_remotes_fails() {
        let config = config_with(&["hetzner"], Some("gpu"));
        let err = resolve_placement(&config, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("placement 'gpu'"), "{err}");
        assert!(err.contains("--local"), "{err}");
    }

    #[test]
    fn local_flag_overrides_remote_placement() {
        let config = config_with(&["hetzner"], Some("hetzner"));
        assert_eq!(
            resolve_placement(&config, None, true).unwrap(),
            Placement::Local
        );
    }

    #[test]
    fn local_flag_overrides_unknown_placement() {
        let config = config_with(&["hetzner"], Some("gpu"));
        assert_eq!(
            resolve_placement(&config, None, true).unwrap(),
            Placement::Local
        );
    }

    #[test]
    fn named_remote_flag() {
        let config = config_with(&["hetzner", "box"], None);
        assert_eq!(
            resolve_placement(&config, Some(Some("box")), false).unwrap(),
            Placement::Remote("box".into())
        );
    }

    #[test]
    fn named_remote_flag_unknown_mentions_branch_order() {
        let config = config_with(&["hetzner"], None);
        let err = format!(
            "{:#}",
            resolve_placement(&config, Some(Some("feature-x")), false).unwrap_err()
        );
        assert!(err.contains("Unknown remote 'feature-x'"), "{err}");
        assert!(err.contains("configured: hetzner"), "{err}");
        assert!(err.contains("branch name"), "{err}");
    }

    #[test]
    fn bare_remote_flag_uses_placement() {
        let config = config_with(&["hetzner", "box"], Some("box"));
        assert_eq!(
            resolve_placement(&config, Some(None), false).unwrap(),
            Placement::Remote("box".into())
        );
    }

    #[test]
    fn bare_remote_flag_uses_only_remote() {
        let config = config_with(&["hetzner"], None);
        assert_eq!(
            resolve_placement(&config, Some(None), false).unwrap(),
            Placement::Remote("hetzner".into())
        );
    }

    #[test]
    fn bare_remote_flag_is_ambiguous_with_several() {
        let config = config_with(&["hetzner", "box"], None);
        let err = resolve_placement(&config, Some(None), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("box, hetzner"), "{err}");
    }

    #[test]
    fn bare_remote_flag_without_remotes_fails() {
        let config = config_with(&[], None);
        let err = resolve_placement(&config, Some(None), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no remotes are configured"), "{err}");
    }

    #[test]
    fn reject_remote_placement_names_remote_and_fix() {
        let config = config_with(&["hetzner"], Some("hetzner"));
        let err = reject_remote_placement(&config).unwrap_err().to_string();
        assert!(err.contains("workmux add --remote hetzner"), "{err}");
        assert!(reject_remote_placement(&config_with(&["hetzner"], None)).is_ok());
    }

    #[test]
    fn spec_defaults() {
        let config = config_with(&["hetzner"], None);
        let spec = RemoteSpec::resolve(&config, "hetzner").unwrap();
        assert_eq!(spec.host, "example");
        assert_eq!(spec.path, DEFAULT_NODE_PATH);
        assert_eq!(spec.isolation, RemoteIsolation::Container);
        assert!(spec.binary.is_none());
    }

    fn spec_error(name: &str, remote: RemoteConfig) -> String {
        let config = Config {
            remotes: [(name.to_string(), remote)].into_iter().collect(),
            ..Config::default()
        };
        RemoteSpec::resolve(&config, name).unwrap_err().to_string()
    }

    #[test]
    fn spec_rejects_bad_names() {
        assert!(spec_error("-x", remote("h")).contains("Invalid remote name"));
        assert!(spec_error("a b", remote("h")).contains("Invalid remote name"));
        assert!(spec_error("local", remote("h")).contains("reserved"));
    }

    #[test]
    fn spec_rejects_option_like_or_empty_hosts() {
        assert!(spec_error("r", remote("-oProxyCommand=x")).contains("must not start"));
        assert!(spec_error("r", remote(" ")).contains("empty"));
        assert!(spec_error("r", remote("a b")).contains("whitespace"));
    }

    #[test]
    fn spec_rejects_relative_paths() {
        let mut bad = remote("h");
        bad.path = Some("node".into());
        assert!(spec_error("r", bad).contains("absolute"));
    }

    #[test]
    fn spec_accepts_home_and_absolute_paths() {
        for path in ["~", "~/n", "/srv/node"] {
            let mut ok = remote("h");
            ok.path = Some(path.into());
            let config = Config {
                remotes: [("r".to_string(), ok)].into_iter().collect(),
                ..Config::default()
            };
            assert_eq!(RemoteSpec::resolve(&config, "r").unwrap().path, path);
        }
    }

    #[test]
    fn script_expands_tilde_and_quotes_vars() {
        let mut spec = RemoteSpec::resolve(&config_with(&["r"], None), "r").unwrap();
        spec.path = "~/node dir".into();
        let script = spec.script(&[("pin", "it's")], "printf '%s|%s\\n' \"$root\" \"$pin\"");
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .env("HOME", "/home/u")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "/home/u/node dir|it's\n"
        );

        spec.path = "/srv/node".into();
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(spec.script(&[], "printf %s \"$root\""))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "/srv/node");
    }

    #[test]
    fn spec_expands_binary_tilde_locally() {
        let mut with_binary = remote("h");
        with_binary.binary = Some("~/bin/workmux".into());
        let config = Config {
            remotes: [("r".to_string(), with_binary)].into_iter().collect(),
            ..Config::default()
        };
        let spec = RemoteSpec::resolve(&config, "r").unwrap();
        assert_eq!(
            spec.binary.unwrap(),
            home::home_dir().unwrap().join("bin/workmux")
        );
    }
}
