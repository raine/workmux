//! `workmux remote doctor <name>`: check that a remote can run agents.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;

use super::RemoteSpec;
use super::client::Node;
use super::credentials;
use super::install::{self, NodeStatus, Pin};
use super::protocol::{self, HelloFailure};
use super::transport::{Input, Transport};
use crate::config::{Config, RemoteIsolation};

const MIN_TMUX: (u32, u32) = (3, 3);
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Prints one `key=value` line per fact; values are first lines only.
const FACTS_SCRIPT: &str = r#"fact() { printf '%s=%s\n' "$1" "$(printf '%s\n' "$2" | head -n 1)"; }
fact os "$(uname -s)"
fact arch "$(uname -m)"
fact user "$(id -un)"
fact hostname "$(uname -n)"
fact tmux "$(tmux -V 2>/dev/null)"
fact git "$(git --version 2>/dev/null)"
if command -v loginctl >/dev/null 2>&1; then
  fact linger "$(loginctl show-user "$(id -un)" --property=Linger --value 2>/dev/null)"
else
  fact linger unavailable
fi
if command -v docker >/dev/null 2>&1; then
  out=$(docker info --format '{{.ServerVersion}}' 2>&1 </dev/null)
  fact docker_status $?
  fact docker_out "$out"
fi
if command -v podman >/dev/null 2>&1; then
  out=$(podman info --format '{{.Version.Version}}' 2>&1 </dev/null)
  fact podman_status $?
  fact podman_out "$out"
fi
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Fail,
    Warn,
    Skip,
}

#[derive(Debug)]
pub struct Row {
    pub check: &'static str,
    pub level: Level,
    pub detail: String,
    pub fix: Option<String>,
}

impl Row {
    fn new(check: &'static str, level: Level, detail: impl Into<String>) -> Self {
        Self {
            check,
            level,
            detail: detail.into(),
            fix: None,
        }
    }

    fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

type Facts = BTreeMap<String, String>;

fn parse_facts(stdout: &str) -> Facts {
    stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.trim().to_string()))
        .collect()
}

fn fact<'a>(facts: &'a Facts, key: &str) -> &'a str {
    facts.get(key).map(String::as_str).unwrap_or("")
}

/// Inputs to [`evaluate`] that come from outside the facts probe.
pub struct Context<'a> {
    pub spec: &'a RemoteSpec,
    pub node: &'a NodeStatus,
    pub node_path: &'a str,
    pub mismatch_hint: &'a str,
    /// Command that drops the shared ssh connection, when there is one.
    pub disconnect_hint: Option<&'a str>,
}

/// Turn probe facts into check rows. Also returns the container runtime
/// binary the remote can use without sudo, if any.
pub fn evaluate(facts: &Facts, cx: &Context) -> (Vec<Row>, Option<&'static str>) {
    let name = &cx.spec.name;
    let user = fact(facts, "user");
    let mut rows = vec![Row::new(
        "ssh",
        Level::Ok,
        format!("{} as {user}@{}", cx.spec.host, fact(facts, "hostname")),
    )];

    let os = fact(facts, "os");
    rows.push(match install::release_platform(os, fact(facts, "arch")) {
        Ok(platform) if os == "Linux" => Row::new("arch", Level::Ok, platform),
        Ok(_) => Row::new(
            "arch",
            Level::Fail,
            format!("remote runs {os}; remote nodes require Linux"),
        ),
        Err(err) => Row::new("arch", Level::Fail, err.to_string()),
    });

    rows.push(match cx.node {
        NodeStatus::Ready(hello) => Row::new(
            "node",
            Level::Ok,
            format!(
                "workmux {}, protocol {} ({})",
                hello.version, hello.protocol, cx.node_path
            ),
        ),
        NodeStatus::Missing => Row::new(
            "node",
            Level::Fail,
            format!("{} is not installed", cx.node_path),
        )
        .fix(format!("workmux remote setup {name}")),
        NodeStatus::Mismatch(hello) => Row::new(
            "node",
            Level::Fail,
            protocol::mismatch_message(hello, cx.node_path),
        )
        .fix(cx.mismatch_hint),
        NodeStatus::Broken(failure) => {
            let detail = match failure {
                HelloFailure::PredatesNodes => "has no `workmux node` command".to_string(),
                HelloFailure::NotANode { detail } => detail.clone(),
            };
            Row::new(
                "node",
                Level::Fail,
                format!("{} does not answer: {detail}", cx.node_path),
            )
            .fix(format!("workmux remote setup {name}"))
        }
    });

    rows.push(match fact(facts, "git") {
        "" => Row::new("git", Level::Fail, "git not found")
            .fix("install git (e.g. sudo apt install git)"),
        version => Row::new("git", Level::Ok, version),
    });

    rows.push(tmux_row(fact(facts, "tmux")));

    rows.push(match fact(facts, "linger") {
        "yes" => Row::new("linger", Level::Ok, format!("enabled for {user}")),
        "no" => Row::new(
            "linger",
            Level::Fail,
            format!("disabled for {user}: agents stop when your last ssh session ends"),
        )
        .fix(format!("sudo loginctl enable-linger {user}")),
        "unavailable" => Row::new("linger", Level::Warn, "loginctl not found; cannot check")
            .fix("make sure user processes survive logout"),
        other => Row::new(
            "linger",
            Level::Warn,
            format!("unknown linger state {other:?}"),
        )
        .fix(format!("sudo loginctl enable-linger {user}")),
    });

    if cx.spec.isolation == RemoteIsolation::None {
        rows.push(Row::new("container", Level::Skip, "isolation: none"));
        return (rows, None);
    }
    let (row, runtime) = runtime_row(facts, cx);
    rows.push(row);
    (rows, runtime)
}

/// Remote agents run in docker; podman is reported but not used.
fn runtime_row(facts: &Facts, cx: &Context) -> (Row, Option<&'static str>) {
    if fact(facts, "docker_status") == "0" {
        let version = fact(facts, "docker_out");
        return (
            Row::new("container", Level::Ok, format!("docker {version}")),
            Some("docker"),
        );
    }
    let user = fact(facts, "user");
    let name = &cx.spec.name;
    let row = if !facts.contains_key("docker_status") {
        let detail = if fact(facts, "podman_status") == "0" {
            format!(
                "podman {} works, but remote agents run in docker",
                fact(facts, "podman_out")
            )
        } else {
            "docker is not installed".to_string()
        };
        Row::new("container", Level::Fail, detail).fix(format!(
            "install docker, or set remotes.{name}.isolation: none"
        ))
    } else {
        let output = fact(facts, "docker_out");
        let row = Row::new("container", Level::Fail, format!("docker: {output}"));
        if output.contains("permission denied") {
            let reconnect = cx
                .disconnect_hint
                .map(|hint| format!(", then reconnect: {hint}"))
                .unwrap_or_default();
            row.fix(format!("sudo usermod -aG docker {user}{reconnect}"))
        } else {
            row.fix("start the docker service (e.g. sudo systemctl start docker)")
        }
    };
    (row, None)
}

fn tmux_row(version: &str) -> Row {
    if version.is_empty() {
        return Row::new("tmux", Level::Fail, "tmux not found").fix("install tmux 3.3 or newer");
    }
    match parse_tmux_version(version) {
        Some(found) if found >= MIN_TMUX => Row::new("tmux", Level::Ok, version),
        Some(_) => Row::new("tmux", Level::Fail, format!("{version} is older than 3.3"))
            .fix("install tmux 3.3 or newer"),
        None if version.contains("master") => Row::new("tmux", Level::Ok, version),
        None => Row::new(
            "tmux",
            Level::Warn,
            format!("cannot parse version {version:?}"),
        )
        .fix("make sure tmux is 3.3 or newer"),
    }
}

/// Parse `tmux -V` output such as `tmux 3.4`, `tmux 3.3a` or `tmux next-3.5`.
fn parse_tmux_version(output: &str) -> Option<(u32, u32)> {
    let version = output.trim().strip_prefix("tmux ")?;
    let version = version.strip_prefix("next-").unwrap_or(version);
    let (major, rest) = version.split_once('.')?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Image the remote would run for the configured agent.
fn agent_image(config: &Config) -> (String, &'static str) {
    let agent = crate::multiplexer::agent::resolve_profile_with_type(
        config.agent.as_deref(),
        config.agent_type.as_deref(),
    )
    .name();
    let source = if config.sandbox.image.is_some() {
        "sandbox.image"
    } else {
        "the default workmux image"
    };
    (config.sandbox.resolved_image(agent), source)
}

fn image_row(transport: &Transport, runtime: &str, image: &str, source: &str) -> Row {
    let script = format!(
        "{runtime} pull -q -- {} </dev/null\n",
        crate::shell::shell_quote(image)
    );
    match transport.run(&script, Input::Null, None) {
        Ok(output) if output.success() => Row::new("image", Level::Ok, format!("pulled {image}")),
        Ok(output) => Row::new(
            "image",
            Level::Fail,
            format!("cannot pull {image} ({source}): {}", output.stderr_line()),
        )
        .fix(if source == "sandbox.image" {
            "push the image to a registry the remote can reach, or build it on the remote"
        } else {
            "check the remote's network access to ghcr.io"
        }),
        Err(err) => Row::new("image", Level::Fail, format!("{err:#}")),
    }
}

fn print_row(row: &Row) {
    let mark = match row.level {
        Level::Ok => "✓",
        Level::Fail => "✗",
        Level::Warn => "!",
        Level::Skip => "-",
    };
    println!("  {mark} {:<10} {}", row.check, row.detail);
    if let Some(fix) = &row.fix {
        println!("    {:<10} fix: {fix}", "");
    }
}

/// Run every check, print the report, and return whether all checks passed.
pub fn run(spec: &RemoteSpec, config: &Config) -> Result<bool> {
    println!("Remote {} ({})", spec.name, spec.host);
    let transport = Transport::open(spec)?;
    if let Err(err) = transport.connect() {
        print_row(&Row::new("ssh", Level::Fail, format!("{err:#}")));
        return Ok(false);
    }
    let output = transport.run(FACTS_SCRIPT, Input::Null, Some(PROBE_TIMEOUT))?;
    let facts = parse_facts(&output.stdout_text());

    let pin = Pin::for_spec(spec)?;
    let node = install::node_status(&transport, spec, &pin)?;
    let node_path = pin.path(spec);
    let mismatch_hint = pin.mismatch_hint(spec);
    let disconnect_hint = transport.disconnect_hint();
    let cx = Context {
        spec,
        node: &node,
        node_path: &node_path,
        mismatch_hint: &mismatch_hint,
        disconnect_hint: disconnect_hint.as_deref(),
    };
    let (mut rows, runtime) = evaluate(&facts, &cx);
    for row in &rows {
        print_row(row);
    }
    if let Some(runtime) = runtime {
        let (image, source) = agent_image(config);
        let row = image_row(&transport, runtime, &image, source);
        print_row(&row);
        rows.push(row);
    } else if spec.isolation == RemoteIsolation::Container {
        let row = Row::new("image", Level::Skip, "no usable container runtime");
        print_row(&row);
        rows.push(row);
    }
    let agents = configured_agents(config);
    if !agents.is_empty() {
        let login_rows = if matches!(node, NodeStatus::Ready(_)) {
            let node = Node {
                spec: spec.clone(),
                transport,
                pin,
            };
            let request = protocol::CredentialsRequest {
                agents: agents.iter().map(|agent| agent.to_string()).collect(),
            };
            match node.call(protocol::CREDENTIALS, &request, false) {
                Ok(response) => credential_rows(&spec.name, &response),
                Err(err) => vec![Row::new("logins", Level::Warn, format!("{err:#}"))],
            }
        } else {
            vec![Row::new(
                "logins",
                Level::Skip,
                "the node binary is not ready",
            )]
        };
        for row in login_rows {
            print_row(&row);
            rows.push(row);
        }
    }
    Ok(rows.iter().all(|row| row.level != Level::Fail))
}

/// Agents the config can start: the default agent and the kinds of the
/// `agents:` entries, those remote logins know.
fn configured_agents(config: &Config) -> Vec<&'static str> {
    let default = crate::multiplexer::agent::resolve_selected_agent(config, None);
    let entries = config
        .agents
        .keys()
        .filter_map(|name| crate::multiplexer::agent::resolve_selected_agent(config, Some(name)));
    let mut agents: Vec<&'static str> = Vec::new();
    for agent in default.into_iter().chain(entries) {
        let kind = agent.kind();
        if credentials::agent_auth(kind).is_ok() && !agents.contains(&kind) {
            agents.push(kind);
        }
    }
    agents
}

fn credential_rows(remote: &str, response: &protocol::CredentialsResponse) -> Vec<Row> {
    response
        .agents
        .iter()
        .filter_map(|agent| {
            let auth = credentials::agent_auth(&agent.agent).ok()?;
            let login = format!("workmux remote login {remote} {}", auth.agent);
            let row = if auth.files.is_empty() {
                Row::new(
                    auth.agent,
                    Level::Skip,
                    "cannot tell: its logins live in its database",
                )
                .fix(format!("if it asks you to log in: {login}"))
            } else if agent.found.is_empty() {
                let fix = if auth.agent == "claude" {
                    format!("{login} (or --import, or --setup-token)")
                } else {
                    format!("{login} (or --import)")
                };
                Row::new(
                    auth.agent,
                    Level::Warn,
                    "no login found (an API key in sandbox.env may still work)",
                )
                .fix(fix)
            } else {
                Row::new(auth.agent, Level::Ok, agent.found.join(", "))
            };
            Some(row)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::protocol::Hello;

    fn spec(isolation: RemoteIsolation) -> RemoteSpec {
        RemoteSpec {
            name: "hetzner".into(),
            host: "wm".into(),
            path: "~/.workmux-node".into(),
            isolation,
            binary: None,
        }
    }

    fn healthy_facts() -> Facts {
        parse_facts(
            "os=Linux\narch=aarch64\nuser=dev\nhostname=box\ntmux=tmux 3.4\n\
             git=git version 2.43.0\nlinger=yes\ndocker_status=0\ndocker_out=29.0.1\n",
        )
    }

    fn rows_for(
        facts: &Facts,
        isolation: RemoteIsolation,
        node: NodeStatus,
    ) -> (Vec<Row>, Option<&'static str>) {
        let spec = spec(isolation);
        let cx = Context {
            spec: &spec,
            node: &node,
            node_path: "~/.workmux-node/bin/workmux-1",
            mismatch_hint: "rebuild",
            disconnect_hint: Some("ssh -O exit -o ControlPath=/s wm"),
        };
        evaluate(facts, &cx)
    }

    fn row<'a>(rows: &'a [Row], check: &str) -> &'a Row {
        rows.iter().find(|row| row.check == check).unwrap()
    }

    #[test]
    fn healthy_remote_is_all_green() {
        let (rows, runtime) = rows_for(
            &healthy_facts(),
            RemoteIsolation::Container,
            NodeStatus::Ready(Hello::local()),
        );
        assert!(rows.iter().all(|row| row.level == Level::Ok), "{rows:?}");
        let checks: Vec<_> = rows.iter().map(|row| row.check).collect();
        assert_eq!(
            checks,
            ["ssh", "arch", "node", "git", "tmux", "linger", "container"]
        );
        assert_eq!(runtime, Some("docker"));
        assert_eq!(row(&rows, "arch").detail, "linux-arm64");
    }

    #[test]
    fn missing_node_and_mismatch_have_fixes() {
        let (rows, _) = rows_for(
            &healthy_facts(),
            RemoteIsolation::Container,
            NodeStatus::Missing,
        );
        let node = row(&rows, "node");
        assert_eq!(node.level, Level::Fail);
        assert_eq!(node.fix.as_deref(), Some("workmux remote setup hetzner"));

        let mut hello = Hello::local();
        hello.protocol = 999;
        let (rows, _) = rows_for(
            &healthy_facts(),
            RemoteIsolation::Container,
            NodeStatus::Mismatch(hello),
        );
        let node = row(&rows, "node");
        assert!(node.detail.contains("protocol 999"), "{node:?}");
        assert_eq!(node.fix.as_deref(), Some("rebuild"));
    }

    #[test]
    fn tmux_versions() {
        assert_eq!(parse_tmux_version("tmux 3.3a"), Some((3, 3)));
        assert_eq!(parse_tmux_version("tmux next-3.5"), Some((3, 5)));
        assert_eq!(parse_tmux_version("tmux 3.10"), Some((3, 10)));
        assert_eq!(parse_tmux_version("tmux master"), None);
        assert_eq!(tmux_row("tmux 3.3a").level, Level::Ok);
        assert_eq!(tmux_row("tmux next-3.5").level, Level::Ok);
        assert_eq!(tmux_row("tmux master").level, Level::Ok);
        assert_eq!(tmux_row("tmux 3.2a").level, Level::Fail);
        assert_eq!(tmux_row("").level, Level::Fail);
        assert_eq!(tmux_row("tmux weird").level, Level::Warn);
    }

    #[test]
    fn linger_states() {
        let mut facts = healthy_facts();
        facts.insert("linger".into(), "no".into());
        let (rows, _) = rows_for(
            &facts,
            RemoteIsolation::Container,
            NodeStatus::Ready(Hello::local()),
        );
        let linger = row(&rows, "linger");
        assert_eq!(linger.level, Level::Fail);
        assert_eq!(
            linger.fix.as_deref(),
            Some("sudo loginctl enable-linger dev")
        );

        facts.insert("linger".into(), "unavailable".into());
        let (rows, _) = rows_for(
            &facts,
            RemoteIsolation::Container,
            NodeStatus::Ready(Hello::local()),
        );
        assert_eq!(row(&rows, "linger").level, Level::Warn);
    }

    #[test]
    fn docker_permission_denied_suggests_group_and_reconnect() {
        let mut facts = healthy_facts();
        facts.insert("docker_status".into(), "1".into());
        facts.insert(
            "docker_out".into(),
            "permission denied while trying to connect to the Docker daemon socket".into(),
        );
        let (rows, runtime) = rows_for(
            &facts,
            RemoteIsolation::Container,
            NodeStatus::Ready(Hello::local()),
        );
        assert!(runtime.is_none());
        let fix = row(&rows, "container").fix.clone().unwrap();
        assert!(fix.contains("usermod -aG docker dev"), "{fix}");
        assert!(fix.contains("ssh -O exit"), "{fix}");
    }

    #[test]
    fn podman_alone_fails_with_a_docker_fix() {
        let mut facts = healthy_facts();
        facts.remove("docker_status");
        facts.remove("docker_out");
        facts.insert("podman_status".into(), "0".into());
        facts.insert("podman_out".into(), "5.0.0".into());
        let (rows, runtime) = rows_for(
            &facts,
            RemoteIsolation::Container,
            NodeStatus::Ready(Hello::local()),
        );
        assert_eq!(runtime, None);
        let container = row(&rows, "container");
        assert_eq!(container.level, Level::Fail);
        assert!(
            container.detail.contains("podman 5.0.0"),
            "{}",
            container.detail
        );
        assert!(container.fix.as_deref().unwrap().contains("install docker"));
    }

    #[test]
    fn no_runtime_suggests_install_or_isolation_none() {
        let mut facts = healthy_facts();
        facts.remove("docker_status");
        facts.remove("docker_out");
        let (rows, _) = rows_for(
            &facts,
            RemoteIsolation::Container,
            NodeStatus::Ready(Hello::local()),
        );
        let container = row(&rows, "container");
        assert_eq!(container.level, Level::Fail);
        assert!(
            container
                .fix
                .as_deref()
                .unwrap()
                .contains("isolation: none")
        );
    }

    #[test]
    fn isolation_none_skips_container() {
        let mut facts = healthy_facts();
        facts.remove("docker_status");
        let (rows, runtime) = rows_for(
            &facts,
            RemoteIsolation::None,
            NodeStatus::Ready(Hello::local()),
        );
        assert!(runtime.is_none());
        assert_eq!(row(&rows, "container").level, Level::Skip);
        assert!(rows.iter().all(|row| row.level != Level::Fail));
    }

    #[test]
    fn missing_tools_fail() {
        let mut facts = healthy_facts();
        facts.insert("git".into(), "".into());
        facts.insert("os".into(), "Darwin".into());
        let (rows, _) = rows_for(
            &facts,
            RemoteIsolation::Container,
            NodeStatus::Ready(Hello::local()),
        );
        assert_eq!(row(&rows, "git").level, Level::Fail);
        assert_eq!(row(&rows, "arch").level, Level::Fail);
    }

    #[test]
    fn facts_script_runs_locally() {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(FACTS_SCRIPT)
            .output()
            .unwrap();
        let facts = parse_facts(&String::from_utf8_lossy(&output.stdout));
        assert!(!fact(&facts, "os").is_empty());
        assert!(!fact(&facts, "user").is_empty());
        assert!(facts.contains_key("linger"));
    }

    #[test]
    fn configured_agents_are_the_default_and_entry_kinds() {
        let mut config = Config {
            agent: Some("claude".into()),
            ..Config::default()
        };
        assert_eq!(configured_agents(&config), ["claude"]);
        config.agents =
            serde_yaml::from_str("fast: claude --model haiku\nreview: codex\nlocal: ./my-agent\n")
                .unwrap();
        let mut agents = configured_agents(&config);
        agents.sort();
        assert_eq!(agents, ["claude", "codex"]);
    }

    #[test]
    fn login_rows_warn_with_the_login_command() {
        let response = protocol::CredentialsResponse {
            agents: vec![
                protocol::AgentCredentials {
                    agent: "claude".into(),
                    found: vec!["the setup token".into()],
                },
                protocol::AgentCredentials {
                    agent: "codex".into(),
                    found: vec![],
                },
                protocol::AgentCredentials {
                    agent: "omp".into(),
                    found: vec![],
                },
            ],
        };
        let rows = credential_rows("hetzner", &response);
        let levels: Vec<_> = rows.iter().map(|row| (row.check, row.level)).collect();
        assert_eq!(
            levels,
            [
                ("claude", Level::Ok),
                ("codex", Level::Warn),
                ("omp", Level::Skip)
            ]
        );
        let fix = rows[1].fix.as_deref().unwrap();
        assert!(
            fix.starts_with("workmux remote login hetzner codex"),
            "{fix}"
        );
    }
}
