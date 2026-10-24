//! Agent logins on remotes: `workmux remote login` runs an agent's own
//! login flow on the node, with the credential directories remote
//! executions mount; `--import` copies the laptop's credential files once.

use std::net::TcpListener;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;

use super::RemoteSpec;
use super::client::Node;
use super::protocol::{self, CredentialFile, PutCredentialsRequest, PutCredentialsResponse};
use crate::config::{Config, RemoteIsolation};

/// How an agent logs in and where it keeps the result.
pub struct AgentAuth {
    pub agent: &'static str,
    /// Login command, before the user's extra arguments.
    pub login: &'static [&'static str],
    /// Loopback ports its browser logins call back on, with what uses each.
    pub callbacks: &'static [(u16, &'static str)],
    /// Printed before the login starts.
    pub hint: &'static str,
    /// Files in the agent's directory that hold its logins; empty when they
    /// live in a database with other state.
    pub files: &'static [&'static str],
}

pub const AGENTS: &[AgentAuth] = &[
    AgentAuth {
        agent: "claude",
        login: &["claude", "auth", "login"],
        callbacks: &[],
        hint: "Open the URL it prints on this machine, then paste the code back here.",
        files: &[".credentials.json"],
    },
    AgentAuth {
        agent: "codex",
        login: &["codex", "login"],
        callbacks: &[(1455, "ChatGPT login"), (1457, "ChatGPT login")],
        hint: "Open the URL it prints on this machine; the browser returns through a \
               forwarded port. Without a browser here, pass `-- --device-auth`.",
        files: &["auth.json"],
    },
    AgentAuth {
        agent: "gemini",
        login: &["gemini"],
        callbacks: &[],
        hint: "Choose \"Sign in with Google\", open the URL on this machine and paste \
               the code back. Leave with /quit once it is done.",
        files: &["oauth_creds.json", "google_accounts.json"],
    },
    AgentAuth {
        agent: "opencode",
        login: &["opencode", "auth", "login"],
        callbacks: &[
            (1455, "OpenAI browser login"),
            (1457, "OpenAI browser login"),
        ],
        hint: "Pick a provider; browser logins return through a forwarded port.",
        files: &["auth.json"],
    },
    AgentAuth {
        agent: "pi",
        login: &["pi"],
        callbacks: &[
            (53692, "anthropic"),
            (1455, "openai-codex"),
            (1456, "radius"),
        ],
        hint: "Type /login and pick a provider; browser logins return through a \
               forwarded port (or paste the redirect URL). Leave with /quit.",
        files: &["auth.json"],
    },
    AgentAuth {
        agent: "omp",
        login: &["omp", "login"],
        callbacks: &[
            (54545, "anthropic"),
            (1455, "openai-codex"),
            (8085, "google-gemini-cli"),
            (51121, "google-antigravity"),
        ],
        hint: "Pick a provider (or name one: `-- anthropic`); browser logins return \
               through a forwarded port.",
        files: &[],
    },
];

pub fn agent_auth(agent: &str) -> Result<&'static AgentAuth> {
    AGENTS
        .iter()
        .find(|auth| auth.agent == agent)
        .ok_or_else(|| {
            let known: Vec<&str> = AGENTS.iter().map(|auth| auth.agent).collect();
            anyhow!(
                "unknown agent '{agent}'; remote logins support {}",
                known.join(", ")
            )
        })
}

/// Ports to forward: those of a provider named in `args`, else all.
fn forward_ports(auth: &AgentAuth, args: &[String]) -> Vec<u16> {
    let named: Vec<u16> = auth
        .callbacks
        .iter()
        .filter(|(_, provider)| args.iter().any(|arg| arg == provider))
        .map(|(port, _)| *port)
        .collect();
    if named.is_empty() {
        auth.callbacks.iter().map(|(port, _)| *port).collect()
    } else {
        named
    }
}

fn port_is_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// `workmux remote login <name> <agent>`: the agent's login flow on the
/// node, attached to this terminal. Returns the exit status to use.
pub fn login(
    spec: &RemoteSpec,
    config: &Config,
    agent: &str,
    setup_token: bool,
    args: &[String],
) -> Result<i32> {
    let auth = agent_auth(agent)?;
    if setup_token && agent != "claude" {
        bail!("--setup-token is for claude");
    }
    let command: Vec<String> = if setup_token {
        vec!["claude".into(), "setup-token".into()]
    } else {
        auth.login.iter().map(|word| word.to_string()).collect()
    }
    .into_iter()
    .chain(args.iter().cloned())
    .collect();

    let node = Node::connect(spec)?;
    let mut forwards = Vec::new();
    for port in forward_ports(auth, args) {
        if port_is_free(port) {
            forwards.push(port);
        } else {
            let users: Vec<&str> = auth
                .callbacks
                .iter()
                .filter(|(p, _)| *p == port)
                .map(|(_, provider)| *provider)
                .collect();
            eprintln!(
                "workmux: port {port} is in use on this machine, so it is not forwarded; \
                 a {} would not return to '{}'. Free the port and retry, or use a \
                 login that does not call back.",
                users.join(" or "),
                spec.name
            );
        }
    }

    let mut flags = Vec::new();
    if matches!(spec.isolation, RemoteIsolation::Container) {
        flags.push("--image".to_string());
        flags.push(super::portable::remote_image(&config.sandbox, agent));
    }
    if setup_token {
        flags.push("--setup-token".into());
    }
    flags.push(agent.to_string());
    flags.push("--".into());
    flags.extend(command);

    if setup_token {
        println!(
            "Running `claude setup-token` on '{}'. When it prints the token, copy it; \
             workmux asks for it next.",
            spec.name
        );
    } else {
        println!(
            "Running the {agent} login on '{}'. {}",
            spec.name, auth.hint
        );
    }
    if !forwards.is_empty() {
        let list: Vec<String> = forwards.iter().map(u16::to_string).collect();
        println!(
            "Forwarding localhost port(s) {} to '{}'.",
            list.join(", "),
            spec.name
        );
    }
    if matches!(spec.isolation, RemoteIsolation::Container) {
        println!("If Ctrl-C does not leave it, press Ctrl-P Ctrl-Q.");
    }
    let status = node
        .login_command(&flags, &forwards)
        .status()
        .context("Failed to run ssh")?;
    let code = status.code().unwrap_or(1);
    if let Some(failure) = node.transport.terminal_failure(status.code(), &[]) {
        eprintln!("workmux: {failure}");
    }
    Ok(code)
}

/// Credential files found for an import.
struct Import {
    files: Vec<(&'static str, Vec<u8>)>,
    note: Option<String>,
}

/// Read `auth`'s credential files from `dir`, the laptop's agent directory.
fn collect_import(auth: &AgentAuth, dir: &Path, macos: bool, remote: &str) -> Result<Import> {
    let agent = auth.agent;
    let login_hint = format!("log in on the remote with `workmux remote login {remote} {agent}`");
    if auth.files.is_empty() {
        bail!(
            "{agent} keeps its logins in {}, a database that also holds its \
             settings and caches, so workmux does not copy it; {login_hint}",
            dir.join("agent.db").display()
        );
    }
    let mut files = Vec::new();
    for name in auth.files {
        let path = dir.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => files.push((*name, bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("Failed to read {}", path.display()));
            }
        }
    }
    let keychain = macos && agent == "claude";
    if files.is_empty() {
        if keychain {
            bail!(
                "on macOS Claude Code keeps its login in the Keychain, which workmux does \
                 not read, and {} has no credentials file from a sandboxed Claude; \
                 {login_hint}, or use `--setup-token` for a long-lived token",
                dir.display()
            );
        }
        bail!(
            "no {agent} credentials in {} (looked for {}); {login_hint}",
            dir.display(),
            auth.files.join(", ")
        );
    }
    let note = keychain.then(|| {
        format!(
            "Claude Code on macOS keeps its own login in the Keychain, which workmux \
             does not read; this imports the file a sandboxed Claude wrote in {}.",
            dir.display()
        )
    });
    Ok(Import { files, note })
}

/// `workmux remote login <name> <agent> --import`.
pub fn import(spec: &RemoteSpec, config: &Config, agent: &str) -> Result<()> {
    let auth = agent_auth(agent)?;
    let dir = config
        .sandbox
        .resolved_agent_config_dir(agent)
        .ok_or_else(|| anyhow!("cannot locate the {agent} directory on this machine"))?;
    let import = collect_import(auth, &dir, cfg!(target_os = "macos"), &spec.name)?;
    if let Some(note) = &import.note {
        println!("{note}");
    }
    let names: Vec<&str> = import.files.iter().map(|(name, _)| *name).collect();
    println!(
        "Copying {} from {} to '{}'. This is a one-time copy: agents rotate their \
         refresh token as they use it, so once either machine refreshes, the other \
         copy can stop working. Run this again, or `workmux remote login {} {agent}`, \
         when the remote one does.",
        names.join(", "),
        dir.display(),
        spec.name,
        spec.name
    );
    let request = PutCredentialsRequest {
        agent: agent.to_string(),
        files: import
            .files
            .into_iter()
            .map(|(name, bytes)| CredentialFile {
                name: name.to_string(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
            .collect(),
    };
    let node = Node::connect(spec)?;
    let response: PutCredentialsResponse = node.call(protocol::PUT_CREDENTIALS, &request, false)?;
    for path in &response.written {
        println!("✓ Wrote {path} on '{}'", spec.name);
    }
    for note in &response.notes {
        println!("{note}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_agent_has_a_login() {
        for agent in crate::sandbox::KNOWN_AGENTS {
            assert!(agent_auth(agent).is_ok(), "{agent}");
        }
        let err = agent_auth("cursor").err().unwrap().to_string();
        assert!(err.contains("claude, codex"), "{err}");
    }

    #[test]
    fn a_named_provider_narrows_the_forwards() {
        let omp = agent_auth("omp").unwrap();
        assert_eq!(forward_ports(omp, &[]), [54545, 1455, 8085, 51121]);
        assert_eq!(forward_ports(omp, &["anthropic".into()]), [54545]);
        let codex = agent_auth("codex").unwrap();
        assert_eq!(
            forward_ports(codex, &["--device-auth".into()]),
            [1455, 1457]
        );
        assert!(forward_ports(agent_auth("claude").unwrap(), &[]).is_empty());
    }

    #[test]
    fn busy_ports_are_detected() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_free(port));
    }

    #[test]
    fn import_reads_only_the_agents_credential_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("oauth_creds.json"), "{\"a\":1}").unwrap();
        std::fs::write(dir.path().join("settings.json"), "{}").unwrap();
        let import = collect_import(agent_auth("gemini").unwrap(), dir.path(), false, "r").unwrap();
        let names: Vec<&str> = import.files.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["oauth_creds.json"]);
        assert!(import.note.is_none());

        let err = collect_import(agent_auth("codex").unwrap(), dir.path(), false, "r")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("workmux remote login r codex"), "{err}");
    }

    #[test]
    fn claude_on_macos_explains_the_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let claude = agent_auth("claude").unwrap();
        let err = collect_import(claude, dir.path(), true, "r")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("Keychain"), "{err}");
        assert!(err.contains("--setup-token"), "{err}");

        std::fs::write(dir.path().join(".credentials.json"), "{}").unwrap();
        let import = collect_import(claude, dir.path(), true, "r").unwrap();
        assert!(import.note.unwrap().contains("Keychain"));
        assert!(
            collect_import(claude, dir.path(), false, "r")
                .unwrap()
                .note
                .is_none()
        );
    }

    #[test]
    fn omp_import_points_to_login() {
        let dir = tempfile::tempdir().unwrap();
        let err = collect_import(agent_auth("omp").unwrap(), dir.path(), false, "r")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("agent.db"), "{err}");
        assert!(err.contains("workmux remote login r omp"), "{err}");
    }
}
