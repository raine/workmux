//! Laptop side of the node verbs.

use std::io::IsTerminal;
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
            NodeStatus::Missing | NodeStatus::Broken(_) => {
                install::setup(spec)?;
                let config = crate::config::Config::load(None).unwrap_or_default();
                if let Err(err) = super::hooks::setup(spec, &config) {
                    eprintln!("warning: {err:#}");
                }
            }
        }
        Ok(Self {
            spec: spec.clone(),
            transport,
            pin,
        })
    }

    /// Connect to read from the node without installing anything: the
    /// laptop's pin when it is installed and serves `verb`, else the newest
    /// pin of a recorded execution on the remote that does, since an
    /// execution keeps the binary that started it.
    pub fn connect_reader(spec: &RemoteSpec, verb: &str) -> Result<Self> {
        let transport = Transport::open(spec)?;
        transport.connect()?;
        let mut candidates = vec![Pin::for_spec(spec)?.name];
        let mut records: Vec<_> = super::cache::all()
            .into_iter()
            .filter(|record| record.remote == spec.name)
            .collect();
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
        for record in records {
            if let Some(execution) = record.execution
                && !candidates.contains(&execution.node_bin)
            {
                candidates.push(execution.node_bin);
            }
        }
        for name in candidates {
            let pin = Pin::installed(name);
            if let NodeStatus::Ready(hello) = install::node_status(&transport, spec, &pin)?
                && hello.capabilities.iter().any(|c| c == verb)
            {
                return Ok(Self {
                    spec: spec.clone(),
                    transport,
                    pin,
                });
            }
        }
        bail!(
            "no workmux on '{}' supports {verb}; run `workmux remote setup {}`",
            spec.name,
            spec.name
        )
    }

    /// Script that runs `workmux node <verb> --json` with the pinned binary.
    pub fn verb_script(&self, verb: &str) -> String {
        let missing = MISSING_STATUS.to_string();
        self.spec.script(
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
        )
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
        let script = self.verb_script(verb);
        let input = Input::Bytes(serde_json::to_vec(request)?);
        let output = if streaming {
            self.transport.run_streaming(&script, input)?
        } else {
            self.transport
                .run(&script, input, Some(QUICK_VERB_TIMEOUT))?
        };
        parse_reply(&self.spec.name, verb, &output)
    }

    /// Send one compressed provisioning stream using disk-backed staging so
    /// file contents never accumulate in memory.
    pub fn provision(
        &self,
        request: &protocol::ProvisionRequest,
        streaming: bool,
    ) -> Result<protocol::ProvisionResponse> {
        let mut input = tempfile::NamedTempFile::new()?;
        let mut writer = super::transfer::encoder(input.as_file_mut(), request)?;
        if let Some(payload) = &request.payload {
            super::transfer::write_files(&mut writer, payload.copies.iter().chain(&payload.seeds))?;
        }
        writer.finish()?;
        let script = self.verb_script(protocol::PROVISION_WORKSPACE);
        let file = Input::File(input.path().to_path_buf());
        let output = if streaming {
            self.transport.run_streaming(&script, file)?
        } else {
            self.transport
                .run(&script, file, Some(QUICK_VERB_TIMEOUT))?
        };
        parse_reply(&self.spec.name, protocol::PROVISION_WORKSPACE, &output)
    }

    /// Push `sha` to the workspace base and the project's transfer ref.
    /// The latter advertises history after all workspace refs are removed.
    pub fn push_base(
        &self,
        project_root: &Path,
        repo_path: &str,
        sha: &str,
        ws_id: &str,
    ) -> Result<()> {
        self.push_refs(
            project_root,
            repo_path,
            sha,
            &[
                &super::node::base_ref(ws_id),
                super::node::PROJECT_TRANSFER_REF,
            ],
            std::io::stderr().is_terminal(),
        )
        .with_context(|| format!("Failed to push the base commit to '{}'", self.spec.name))
    }

    /// Push `sha` from the local repository to `refname` in the node's
    /// project repository at `repo_path`.
    pub fn push_ref(
        &self,
        project_root: &Path,
        repo_path: &str,
        sha: &str,
        refname: &str,
    ) -> Result<()> {
        self.push_refs(project_root, repo_path, sha, &[refname], false)
    }

    fn push_refs(
        &self,
        project_root: &Path,
        repo_path: &str,
        sha: &str,
        refnames: &[&str],
        progress: bool,
    ) -> Result<()> {
        let refspecs: Vec<_> = refnames
            .iter()
            .map(|name| format!("+{sha}:{name}"))
            .collect();
        // `--quiet` hides the ref summary, which names internal refs;
        // `--progress` still reports the transfer.
        let status = self
            .git_over_ssh(project_root)?
            .args(["push", "--no-verify", "--atomic", "--quiet"])
            .args(progress.then_some("--progress"))
            .arg(self.repo_url(repo_path))
            .args(&refspecs)
            .status()
            .context("Failed to run git push")?;
        if !status.success() {
            bail!("git push to '{}' failed", self.spec.name);
        }
        Ok(())
    }

    /// Fetch the commits `export` stored for `ws_id` into the local
    /// repository as `refs/workmux/remote/<ws_id>/{index,worktree}`, and
    /// check they hold `snapshot`.
    pub fn fetch_export(
        &self,
        project_root: &Path,
        repo_path: &str,
        ws_id: &str,
        snapshot: &super::git::Snapshot,
    ) -> Result<()> {
        let local = |part: &str| format!("refs/workmux/remote/{ws_id}/{part}");
        let refspecs = ["index", "worktree"].map(|part| {
            format!(
                "+{}:{}",
                super::node::git::export_ref(ws_id, part),
                local(part)
            )
        });
        let status = self
            .git_over_ssh(project_root)?
            .args([
                "-c",
                "transfer.fsckObjects=true",
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                &self.repo_url(repo_path),
            ])
            .args(&refspecs)
            .status()
            .context("Failed to run git fetch")?;
        if !status.success() {
            bail!(
                "Failed to fetch the work of '{ws_id}' from '{}'",
                self.spec.name
            );
        }
        for (part, tree) in [("index", &snapshot.index), ("worktree", &snapshot.worktree)] {
            let commit = super::git::run(project_root, &["rev-parse", "--verify", &local(part)])?;
            if super::git::tree_of(project_root, &commit)? != *tree
                || super::git::parent_of(project_root, &commit)? != snapshot.head
            {
                bail!(
                    "the work fetched from '{}' does not match its export; try again",
                    self.spec.name
                );
            }
        }
        Ok(())
    }

    fn repo_url(&self, repo_path: &str) -> String {
        format!("ssh://{PUSH_HOST}{repo_path}")
    }

    /// Hardened git in `project_root` whose ssh is the git-ssh helper.
    fn git_over_ssh(&self, project_root: &Path) -> Result<std::process::Command> {
        let exe = std::env::current_exe().context("Failed to locate the workmux binary")?;
        let ssh_command = format!(
            "{} remote git-ssh {}",
            shell_quote(&exe.to_string_lossy()),
            shell_quote(&self.spec.name)
        );
        let mut command = crate::git::unattended_git(Some(project_root))?;
        command
            .env("GIT_SSH_COMMAND", ssh_command)
            .env("GIT_SSH_VARIANT", "simple");
        Ok(command)
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

/// Serve a push into or a fetch from `$path` only when it is
/// `$root/projects/<id>/repo`.
const GIT_SSH_SCRIPT: &str = "\
root=${root%/}
rest=${path#\"$root/projects/\"}
id=${rest%/repo}
if [ \"$rest\" = \"$path\" ] || [ \"$id/repo\" != \"$rest\" ]; then
\techo \"workmux: refusing to serve $path\" >&2; exit 1
fi
case $id in \"\"|*/*|.*) echo \"workmux: refusing to serve $path\" >&2; exit 1 ;; esac
case $service in
receive-pack) exec git-receive-pack \"$path\" ;;
upload-pack) exec git -c core.fsmonitor=false -c core.hooksPath=/dev/null upload-pack \"$path\" ;;
esac
exit 1
";

/// `workmux remote git-ssh <name> <host> <command>`: the `GIT_SSH_COMMAND`
/// of [`Node::push_ref`] and [`Node::fetch_export`]. Serves only
/// `git-receive-pack` and `git-upload-pack` for a project repository under
/// the node root.
pub fn git_ssh(spec: &RemoteSpec, args: &[String]) -> Result<i32> {
    let command = args
        .last()
        .ok_or_else(|| anyhow!("git-ssh: missing command"))?;
    let (service, path) = served_command(command)?;
    let transport = Transport::open(spec)?;
    transport.exec_inherit(&spec.script(&[("service", service), ("path", &path)], GIT_SSH_SCRIPT))
}

/// The service and repository of a git ssh command line.
fn served_command(command: &str) -> Result<(&'static str, String)> {
    let (service, rest) = if let Some(rest) = command.strip_prefix("git-receive-pack ") {
        ("receive-pack", rest)
    } else if let Some(rest) = command.strip_prefix("git-upload-pack ") {
        ("upload-pack", rest)
    } else {
        bail!("git-ssh only serves pushes and fetches, not {command:?}");
    };
    let words = shlex::split(rest).ok_or_else(|| anyhow!("git-ssh: unparsable {command:?}"))?;
    match words.as_slice() {
        [path] if path.starts_with('/') && !path.split('/').any(|part| part == "..") => {
            Ok((service, path.clone()))
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
    fn git_ssh_accepts_only_pack_services_of_absolute_paths() {
        assert_eq!(
            served_command("git-receive-pack '/n/projects/p/repo'").unwrap(),
            ("receive-pack", "/n/projects/p/repo".to_string())
        );
        assert_eq!(
            served_command("git-upload-pack '/n/projects/p/repo'").unwrap(),
            ("upload-pack", "/n/projects/p/repo".to_string())
        );
        assert_eq!(
            served_command("git-receive-pack '/n/it'\\''s/repo'").unwrap(),
            ("receive-pack", "/n/it's/repo".to_string())
        );
        for bad in [
            "git-upload-archive '/n/projects/p/repo'",
            "git-receive-pack 'relative'",
            "git-upload-pack '/n/../etc'",
            "git-receive-pack '/a' '/b'",
        ] {
            assert!(served_command(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn git_ssh_script_confines_paths_to_project_repos() {
        let body = GIT_SSH_SCRIPT.replace("case $service in", "exit 0\ncase $service in");
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
