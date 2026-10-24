//! Laptop side of the node verbs.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::RemoteSpec;
use super::install::{self, NodeStatus, Pin};
use super::protocol::{self, ErrorEnvelope, NodeError, first_line};
use super::transport::{Input, Output, Transport};
use crate::shell::shell_quote;

/// Bound for verbs that never run user hooks.
const QUICK_VERB_TIMEOUT: Duration = Duration::from_secs(120);

/// Exit status of a verb script when the pinned binary is absent.
const MISSING_STATUS: i32 = 100;

/// Host name in the push URL; the git-ssh helper ignores it.
const PUSH_HOST: &str = "workmux-node";

/// A connected node running the pinned binary.
pub struct Node {
    pub spec: RemoteSpec,
    pub transport: Transport,
    pub pin: Pin,
}

impl Node {
    /// Connect, installing the pinned binary when it is missing.
    pub fn connect(spec: &RemoteSpec) -> Result<Self> {
        let transport = Transport::open(spec)?;
        transport.connect()?;
        let pin = Pin::for_spec(spec)?;
        match install::node_status(&transport, spec, &pin)? {
            NodeStatus::Ready(hello) => {
                let missing: Vec<&str> = [
                    protocol::PROVISION_WORKSPACE,
                    protocol::START_EXECUTION,
                    protocol::REMOVE_WORKSPACE,
                ]
                .into_iter()
                .filter(|verb| !hello.capabilities.iter().any(|c| c == verb))
                .collect();
                if !missing.is_empty() {
                    bail!(
                        "the workmux {} on '{}' cannot run remote agents (missing {}). {}",
                        hello.version,
                        spec.name,
                        missing.join(", "),
                        pin.mismatch_hint(spec)
                    );
                }
            }
            NodeStatus::Mismatch(hello) => bail!(
                "{}. {}",
                protocol::mismatch_message(&hello, &pin.path(spec)),
                pin.mismatch_hint(spec)
            ),
            NodeStatus::Missing | NodeStatus::Broken(_) => install::setup(spec)?,
        }
        Ok(Self {
            spec: spec.clone(),
            transport,
            pin,
        })
    }

    /// Run `workmux node <verb> --json` with `request` on stdin. With
    /// `streaming`, the verb's stderr (hook output) is shown as it arrives
    /// and there is no time limit.
    pub fn call<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        verb: &str,
        request: &Req,
        streaming: bool,
    ) -> Result<Resp> {
        let missing = MISSING_STATUS.to_string();
        let script = self.spec.script(
            &[
                ("pin", &self.pin.name),
                ("verb", verb),
                ("missing", &missing),
            ],
            "bin=\"$root/bin/$pin\"\n\
             [ -x \"$bin\" ] || exit \"$missing\"\n\
             WORKMUX_NODE_ROOT=$root XDG_STATE_HOME=$root/state\n\
             export WORKMUX_NODE_ROOT XDG_STATE_HOME\n\
             exec \"$bin\" node \"$verb\" --json\n",
        );
        let input = Input::Bytes(serde_json::to_vec(request)?);
        let output = if streaming {
            self.transport.run_streaming(&script, input)?
        } else {
            self.transport
                .run(&script, input, Some(QUICK_VERB_TIMEOUT))?
        };
        parse_reply(&self.spec.name, verb, &output)
    }

    /// Push `sha` from the local repository to the workspace's base ref in
    /// the node's project repository.
    pub fn push_base(
        &self,
        project_root: &Path,
        repo_path: &str,
        sha: &str,
        ws_id: &str,
    ) -> Result<()> {
        let exe = std::env::current_exe().context("Failed to locate the workmux binary")?;
        let ssh_command = format!(
            "{} remote git-ssh {}",
            shell_quote(&exe.to_string_lossy()),
            shell_quote(&self.spec.name)
        );
        let url = format!("ssh://{PUSH_HOST}{repo_path}");
        let refspec = format!("{sha}:refs/workmux/base/{ws_id}");
        let status = crate::git::unattended_git(Some(project_root))?
            .args(["push", "--no-verify", "--quiet", &url, &refspec])
            .env("GIT_SSH_COMMAND", ssh_command)
            .env("GIT_SSH_VARIANT", "simple")
            .status()
            .context("Failed to run git push")?;
        if !status.success() {
            bail!("Failed to push the base commit to '{}'", self.spec.name);
        }
        Ok(())
    }

    /// Command that attaches the calling terminal to pane `pane` of
    /// execution `ex_id`; exits with the `node attach` status.
    pub fn attach_command(&self, ex_id: &str, pane: usize) -> std::process::Command {
        let pane = pane.to_string();
        let script = self.spec.script(
            &[("pin", &self.pin.name), ("ex", ex_id), ("pane", &pane)],
            "WORKMUX_NODE_ROOT=$root\nexport WORKMUX_NODE_ROOT\n\
             exec \"$root/bin/$pin\" node attach \"$ex\" \"$pane\"\n",
        );
        self.transport.terminal_command(&script, &[])
    }

    /// Command that runs `node login <args>` on the calling terminal,
    /// forwarding `ports` for the login's browser callbacks.
    pub fn login_command(&self, args: &[String], ports: &[u16]) -> std::process::Command {
        let args: Vec<String> = args.iter().map(|arg| shell_quote(arg)).collect();
        let script = self.spec.script(
            &[("pin", &self.pin.name)],
            &format!(
                "WORKMUX_NODE_ROOT=$root XDG_STATE_HOME=$root/state\n\
                 export WORKMUX_NODE_ROOT XDG_STATE_HOME\n\
                 exec \"$root/bin/$pin\" node login {}\n",
                args.join(" ")
            ),
        );
        self.transport.terminal_command(&script, ports)
    }
}

fn parse_reply<Resp: DeserializeOwned>(name: &str, verb: &str, output: &Output) -> Result<Resp> {
    let stdout = output.stdout_text();
    let reply = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if output.success() {
        return serde_json::from_str(reply).with_context(|| {
            format!("remote '{name}' sent an unexpected reply to {verb}: {reply}")
        });
    }
    if let Ok(envelope) = serde_json::from_str::<ErrorEnvelope>(reply) {
        return Err(NodeError {
            code: envelope.error.code,
            message: envelope.error.message,
        }
        .into());
    }
    if output.status == Some(MISSING_STATUS) {
        bail!("the workmux node binary is missing on '{name}'; run `workmux remote setup {name}`");
    }
    let detail = first_line(&String::from_utf8_lossy(&output.stderr))
        .map(str::to_string)
        .unwrap_or_else(|| format!("exit status {:?}", output.status));
    Err(anyhow!("remote '{name}': {verb} failed: {detail}"))
}

/// The error code a node attached to `err`, if any.
pub fn error_code(err: &anyhow::Error) -> Option<&str> {
    err.downcast_ref::<NodeError>().map(|err| err.code.as_str())
}

/// Serve a push into `$path` only when it is `$root/projects/<id>/repo`.
const GIT_SSH_SCRIPT: &str = "\
root=${root%/}
rest=${path#\"$root/projects/\"}
id=${rest%/repo}
if [ \"$rest\" = \"$path\" ] || [ \"$id/repo\" != \"$rest\" ]; then
\techo \"workmux: refusing to serve $path\" >&2; exit 1
fi
case $id in \"\"|*/*|.*) echo \"workmux: refusing to serve $path\" >&2; exit 1 ;; esac
exec git-receive-pack \"$path\"
";

/// `workmux remote git-ssh <name> <host> <command>`: the `GIT_SSH_COMMAND`
/// of [`Node::push_base`]. Serves only `git-receive-pack` for a project
/// repository under the node root.
pub fn git_ssh(spec: &RemoteSpec, args: &[String]) -> Result<i32> {
    let command = args
        .last()
        .ok_or_else(|| anyhow!("git-ssh: missing command"))?;
    let path = receive_pack_path(command)?;
    let transport = Transport::open(spec)?;
    transport.exec_inherit(&spec.script(&[("path", &path)], GIT_SSH_SCRIPT))
}

fn receive_pack_path(command: &str) -> Result<String> {
    let rest = command
        .strip_prefix("git-receive-pack ")
        .ok_or_else(|| anyhow!("git-ssh only serves pushes, not {command:?}"))?;
    let words = shlex::split(rest).ok_or_else(|| anyhow!("git-ssh: unparsable {command:?}"))?;
    match words.as_slice() {
        [path] if path.starts_with('/') && !path.split('/').any(|part| part == "..") => {
            Ok(path.clone())
        }
        _ => bail!("git-ssh: unexpected repository in {command:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(status: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: Some(status),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn replies_and_error_envelopes() {
        let ok: protocol::RemoveResponse =
            parse_reply("r", "v", &output(0, "{\"already_removed\":true}\n", "")).unwrap();
        assert!(ok.already_removed);

        let envelope = serde_json::to_string(&ErrorEnvelope::new("busy", "later")).unwrap();
        let err = parse_reply::<protocol::RemoveResponse>("r", "v", &output(1, &envelope, ""))
            .unwrap_err();
        assert_eq!(error_code(&err), Some("busy"));
        assert_eq!(error_code(&err.context("outer")), Some("busy"));

        let err = parse_reply::<protocol::RemoveResponse>("r", "v", &output(100, "", ""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("remote setup r"), "{err}");
    }

    #[test]
    fn git_ssh_accepts_only_receive_pack_of_absolute_paths() {
        assert_eq!(
            receive_pack_path("git-receive-pack '/n/projects/p/repo'").unwrap(),
            "/n/projects/p/repo"
        );
        assert_eq!(
            receive_pack_path("git-receive-pack '/n/it'\\''s/repo'").unwrap(),
            "/n/it's/repo"
        );
        for bad in [
            "git-upload-pack '/n/projects/p/repo'",
            "git-receive-pack 'relative'",
            "git-receive-pack '/n/../etc'",
            "git-receive-pack '/a' '/b'",
        ] {
            assert!(receive_pack_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn git_ssh_script_confines_paths_to_project_repos() {
        let body = GIT_SSH_SCRIPT.replace("exec git-receive-pack \"$path\"", "exit 0");
        let serves = |path: &str| {
            let script = format!("root=/n/\npath='{path}'\n{body}");
            std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(serves("/n/projects/p-1/repo"));
        assert!(!serves("/n/projects/p/x/repo"));
        assert!(!serves("/n/projects/repo"));
        assert!(!serves("/n/projects/.p/repo"));
        assert!(!serves("/other/projects/p/repo"));
    }
}
