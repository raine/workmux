//! `node login`, `node put-credentials` and `node credentials`: agent logins
//! in the node user's agent directories, the ones remote executions mount.

use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::Engine;

use super::secrets::{self, CLAUDE_TOKEN};
use super::{NodeRoot, refuse};
use crate::config::{ContainerConfig, SandboxBackend, SandboxConfig, SandboxRuntime};
use crate::remote::credentials::{AgentAuth, agent_auth};
use crate::remote::protocol::{
    AgentCredentials, CredentialsRequest, CredentialsResponse, PutCredentialsRequest,
    PutCredentialsResponse, code,
};

/// Gemini's `security.auth.selectedType` for a Google login.
const GEMINI_GOOGLE_AUTH: &str = "oauth-personal";

/// Set for every login: the user finishes it in the laptop's browser, and
/// gemini would otherwise open one on a node that has a display.
const NO_BROWSER: &str = "NO_BROWSER";

fn known(agent: &str) -> Result<&'static AgentAuth> {
    agent_auth(agent).map_err(|err| refuse(code::BAD_REQUEST, err.to_string()))
}

/// The agent's directory for the node user: where remote executions mount
/// it from.
fn agent_dir(agent: &str) -> Result<PathBuf> {
    SandboxConfig::default()
        .resolved_agent_config_dir(agent)
        .ok_or_else(|| anyhow!("cannot locate the {agent} directory on the node"))
}

/// Run `command` (the agent's login) on this terminal, in the agent's
/// container image when `image` is set, else on the host. With
/// `setup_token`, then read the token it printed and keep it node-wide.
/// Returns the login's exit status.
pub fn login(
    root: &NodeRoot,
    agent: &str,
    image: Option<&str>,
    setup_token: bool,
    command: &[String],
) -> Result<i32> {
    known(agent)?;
    let (program, args) = command
        .split_first()
        .ok_or_else(|| refuse(code::BAD_REQUEST, "missing login command"))?;
    // This process outlives the login to clean up after it: an interrupt is
    // the login's to handle, and a hangup or termination stops the login.
    let hangup = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(libc::SIGINT, Arc::new(AtomicBool::new(false)))
        .context("Failed to install a signal handler")?;
    for signal in [libc::SIGHUP, libc::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&hangup))
            .context("Failed to install a signal handler")?;
    }
    let status = match image {
        Some(image) => login_in_container(agent, image, command, &hangup)?,
        None => login_on_host(agent, program, args, &hangup)?,
    };
    if setup_token && !hangup.load(Ordering::Relaxed) {
        save_setup_token(root)?;
    }
    Ok(status)
}

/// Wait for `child`, calling `stop` once if `hangup` is raised meanwhile.
fn wait_or_stop(child: &mut Child, hangup: &AtomicBool, stop: impl Fn(&Child)) -> Result<i32> {
    let mut stopped = false;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code().unwrap_or(1));
        }
        if !stopped && hangup.load(Ordering::Relaxed) {
            stop(child);
            stopped = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn remove_container(name: &str) {
    let _ = Command::new("docker")
        .args(["rm", "-f", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn login_in_container(
    agent: &str,
    image: &str,
    command: &[String],
    hangup: &AtomicBool,
) -> Result<i32> {
    let sandbox = SandboxConfig {
        enabled: Some(true),
        backend: Some(SandboxBackend::Container),
        image: Some(image.to_string()),
        container: ContainerConfig {
            runtime: Some(SandboxRuntime::Docker),
            ..ContainerConfig::default()
        },
        ..SandboxConfig::default()
    };
    crate::sandbox::ensure_sandbox_config_dirs()?;
    crate::sandbox::ensure_image_ready(&sandbox, image)?;

    let name = format!("workmux-login-{}", crate::remote::cache::new_id('l'));
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    // `--init` keeps the login off PID 1, which ignores the Ctrl-C of a
    // program that installs no handler for it.
    let mut args: Vec<String> = ["run", "--rm", "-it", "--init", "--name", &name]
        .into_iter()
        .map(str::to_string)
        .collect();
    // Host networking puts the agent's callback server on the loopback the
    // laptop's ssh forwards reach.
    args.extend(["--network".into(), "host".into()]);
    args.extend(["--user".into(), format!("{uid}:{gid}")]);
    for var in ["TERM", "COLORTERM"] {
        if std::env::var_os(var).is_some() {
            args.extend(["--env".into(), var.into()]);
        }
    }
    crate::sandbox::push_agent_home(&mut args, &sandbox, agent, SandboxRuntime::Docker, None);
    args.extend(["--workdir".into(), "/tmp".into()]);
    args.extend([
        "--env".into(),
        "PATH=/tmp/.local/bin:/usr/local/bin:/usr/bin:/bin".into(),
        "--env".into(),
        format!("{NO_BROWSER}=true"),
    ]);
    args.push(image.to_string());
    args.extend(command.iter().cloned());

    let mut child = Command::new("docker")
        .args(&args)
        .spawn()
        .context("Failed to run docker")?;
    // A docker client whose terminal is gone keeps waiting for the
    // container, and a container outlives a client that died.
    let status = wait_or_stop(&mut child, hangup, |_| remove_container(&name));
    remove_container(&name);
    status
}

fn login_on_host(agent: &str, program: &str, args: &[String], hangup: &AtomicBool) -> Result<i32> {
    let env = super::login_env();
    let path = env
        .iter()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var("PATH").ok());
    let home = home::home_dir().ok_or_else(|| anyhow!("cannot locate the home directory"))?;
    let resolved = which::which_in(program, path, &home).map_err(|_| {
        anyhow!("{agent} is not installed on the remote ({program} is not on the login PATH)")
    })?;
    let mut child = Command::new(resolved)
        .args(args)
        .envs(env)
        .env(NO_BROWSER, "true")
        .current_dir(&home)
        .spawn()
        .with_context(|| format!("Failed to run {program}"))?;
    wait_or_stop(&mut child, hangup, |child| unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGHUP);
    })
}

/// Ask for the token `claude setup-token` printed and keep it node-wide.
fn save_setup_token(root: &NodeRoot) -> Result<()> {
    let mut stderr = std::io::stderr();
    let _ = write!(
        stderr,
        "\r\nPaste the token (hidden; Enter alone keeps the current one): "
    );
    let _ = stderr.flush();
    let line = if std::io::stdin().is_terminal() {
        console::Term::stderr().read_secure_line()?
    } else {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        line
    };
    let token = clean_token(&line).map_err(|err| refuse(code::BAD_REQUEST, err))?;
    match token {
        Some(token) => {
            secrets::set_node_secret(root, CLAUDE_TOKEN, &token)?;
            let _ = writeln!(
                stderr,
                "✓ Saved. Claude agents started on this remote from now on use it."
            );
        }
        None => {
            let _ = writeln!(stderr, "Kept the current token.");
        }
    }
    Ok(())
}

/// The token in a pasted line, without bracketed-paste markers; `None`
/// when the line is empty.
fn clean_token(line: &str) -> std::result::Result<Option<String>, &'static str> {
    let token = line.replace("\x1b[200~", "").replace("\x1b[201~", "");
    let token = token.trim();
    if token.is_empty() {
        return Ok(None);
    }
    if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("a token is one word without spaces or control characters");
    }
    Ok(Some(token.to_string()))
}

pub fn put(_root: &NodeRoot, request: PutCredentialsRequest) -> Result<PutCredentialsResponse> {
    let auth = known(&request.agent)?;
    let mut files = Vec::new();
    for file in &request.files {
        if !auth.files.contains(&file.name.as_str()) {
            return Err(refuse(
                code::BAD_REQUEST,
                format!("{} is not a {} credential file", file.name, auth.agent),
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&file.data)
            .map_err(|err| refuse(code::BAD_REQUEST, format!("{}: {err}", file.name)))?;
        files.push((&file.name, bytes));
    }
    let dir = agent_dir(auth.agent)?;
    create_private_dir(&dir)?;
    let mut written = Vec::new();
    for (name, bytes) in files {
        let path = dir.join(name);
        crate::util::write_atomic(&path, &bytes)?;
        written.push(path.to_string_lossy().into_owned());
    }
    let mut notes = Vec::new();
    if auth.agent == "gemini" {
        notes.extend(select_gemini_google_auth(&dir)?);
    }
    Ok(PutCredentialsResponse { written, notes })
}

fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("Failed to create {}", dir.display()))
}

/// Make gemini use the imported Google login: without a selected auth type
/// it opens its auth dialog, and choosing Google there deletes the cached
/// credentials. Settings that select something else, or that are not plain
/// JSON, are left alone with a note.
fn select_gemini_google_auth(dir: &Path) -> Result<Option<String>> {
    let path = dir.join("settings.json");
    let mut settings = match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => {
                return Ok(Some(format!(
                    "{} is not plain JSON, so it was left alone; if gemini asks how to \
                     authenticate, choose \"Sign in with Google\" and log in again",
                    path.display()
                )));
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(err) => return Err(err).with_context(|| format!("Failed to read {}", path.display())),
    };
    let selected = settings
        .get("security")
        .and_then(|security| security.get("auth"))
        .and_then(|auth| auth.get("selectedType"))
        .and_then(|selected| selected.as_str())
        .map(str::to_string);
    match selected.as_deref() {
        Some(GEMINI_GOOGLE_AUTH) => Ok(None),
        Some(other) => Ok(Some(format!(
            "{} selects {other} authentication, so gemini will not use the imported \
             Google login until you switch to \"Sign in with Google\"",
            path.display()
        ))),
        None => {
            let auth = settings
                .entry("security")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .and_then(|security| {
                    security
                        .entry("auth")
                        .or_insert_with(|| serde_json::json!({}))
                        .as_object_mut()
                });
            let Some(auth) = auth else {
                return Ok(Some(format!(
                    "{} has an unexpected security section, so it was left alone",
                    path.display()
                )));
            };
            auth.insert("selectedType".into(), GEMINI_GOOGLE_AUTH.into());
            crate::util::write_atomic(&path, &serde_json::to_vec_pretty(&settings)?)?;
            Ok(Some(format!(
                "Set gemini's auth type to Google in {}",
                path.display()
            )))
        }
    }
}

pub fn check(root: &NodeRoot, request: CredentialsRequest) -> Result<CredentialsResponse> {
    let token = secrets::node_secrets(root)?.0.contains_key(CLAUDE_TOKEN);
    let mut agents = Vec::new();
    for agent in &request.agents {
        let auth = known(agent)?;
        let dir = agent_dir(agent)?;
        let mut found: Vec<String> = auth
            .files
            .iter()
            .filter(|name| holds_login(&dir.join(name)))
            .map(|name| dir.join(name).to_string_lossy().into_owned())
            .collect();
        if agent == "claude" && token {
            found.push("the setup token".into());
        }
        agents.push(AgentCredentials {
            agent: agent.clone(),
            found,
        });
    }
    Ok(CredentialsResponse { agents })
}

/// A credential file holds a login when it is a JSON object with at least
/// one entry; agents create empty ones (`{}`) before any login.
fn holds_login(path: &Path) -> bool {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.as_object().map(|map| !map.is_empty()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_tokens_lose_paste_markers_and_whitespace() {
        assert_eq!(
            clean_token("\x1b[200~sk-ant-oat01-abc\x1b[201~\r\n").unwrap(),
            Some("sk-ant-oat01-abc".into())
        );
        assert_eq!(clean_token("  \n").unwrap(), None);
        assert!(clean_token("two words").is_err());
        assert!(clean_token("a\x07b").is_err());
    }

    #[test]
    fn empty_stores_hold_no_login() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        assert!(!holds_login(&path));
        for (text, expected) in [
            ("{}", false),
            ("[]", false),
            ("garbage", false),
            ("{\"anthropic\":{}}", true),
        ] {
            fs::write(&path, text).unwrap();
            assert_eq!(holds_login(&path), expected, "{text}");
        }
    }

    #[test]
    fn gemini_google_auth_is_selected_only_when_unset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let read = || -> serde_json::Value {
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap()
        };

        assert!(select_gemini_google_auth(dir.path()).unwrap().is_some());
        assert_eq!(
            read()["security"]["auth"]["selectedType"],
            GEMINI_GOOGLE_AUTH
        );

        fs::write(
            &path,
            r#"{"ui":{"theme":"x"},"security":{"folderTrust":{}}}"#,
        )
        .unwrap();
        select_gemini_google_auth(dir.path()).unwrap();
        assert_eq!(
            read()["security"]["auth"]["selectedType"],
            GEMINI_GOOGLE_AUTH
        );
        assert_eq!(read()["ui"]["theme"], "x");
        assert!(read()["security"]["folderTrust"].is_object());
        assert!(select_gemini_google_auth(dir.path()).unwrap().is_none());

        let other = r#"{"security":{"auth":{"selectedType":"gemini-api-key"}}}"#;
        fs::write(&path, other).unwrap();
        let note = select_gemini_google_auth(dir.path()).unwrap().unwrap();
        assert!(note.contains("gemini-api-key"), "{note}");
        assert_eq!(fs::read_to_string(&path).unwrap(), other);

        let commented = "{ // mine\n}";
        fs::write(&path, commented).unwrap();
        assert!(select_gemini_google_auth(dir.path()).unwrap().is_some());
        assert_eq!(fs::read_to_string(&path).unwrap(), commented);
    }
}
