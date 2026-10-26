//! Detached laptop workers for requests made in node panes.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::cache::{self, WorkspaceRecord};
use super::client::Node;
use super::node::operation::{Claim, Operation, Outcome, Update};
use super::{RemoteSpec, observe::Observation};
use crate::config::{Config, MergeStrategy};
use crate::multiplexer::{create_backend, detect_backend};
use crate::workflow::WorkflowContext;

pub fn dispatch(remote: &str, observation: &Observation) {
    if !observation.is_connected(cache::now()) {
        return;
    }
    let records = cache::all();
    for ws in &observation.workspaces {
        let Some(request) = &ws.operation else {
            continue;
        };
        if super::node::validate_id(&request.id, 'r', "request id").is_err() {
            continue;
        }
        let Some(record) = owned_record(&records, remote, &ws.ws_id) else {
            continue;
        };
        if let Err(err) = spawn(record, &request.id, request.sandboxed) {
            tracing::warn!("cannot start remote operation worker: {err}");
        }
    }
}

fn owned_record<'a>(
    records: &'a [WorkspaceRecord],
    remote: &str,
    ws_id: &str,
) -> Option<&'a WorkspaceRecord> {
    records
        .iter()
        .find(|record| record.remote == remote && record.ws_id == ws_id)
}

fn spawn(record: &WorkspaceRecord, id: &str, sandboxed: bool) -> Result<()> {
    let path = log_path(&record.remote, id)?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let output = open_log(&path)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("_remote-operation")
        .arg(&record.remote)
        .arg(&record.ws_id)
        .arg(id)
        .current_dir(&record.project_root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "false")
        .env("GIT_SEQUENCE_EDITOR", "false")
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output);
    if sandboxed {
        // Git hooks in the workspace's repository come from the agent.
        crate::sandbox::rpc::disable_git_hooks(&mut command);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Closing the relay must not terminate its lifecycle worker.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn log_path(remote: &str, id: &str) -> Result<std::path::PathBuf> {
    super::validate_name(remote)?;
    super::node::validate_id(id, 'r', "request id")?;
    Ok(cache::remote_dir(remote)?
        .join("operation-logs")
        .join(format!("{id}.log")))
}

fn open_log(path: &std::path::Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    // Observers may dispatch the same request more than once. Never truncate
    // the output of the worker that holds its receipt.
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .context("Failed to open remote operation log")
}

/// Read retained output without contacting the node or requiring a workspace.
pub fn logs(remote: &str, id: Option<&str>) -> Result<()> {
    super::validate_name(remote)?;
    if let Some(id) = id {
        let path = log_path(remote, id)?;
        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        std::io::copy(&mut file, &mut std::io::stdout().lock())?;
    } else {
        let dir = cache::remote_dir(remote)?.join("operation-logs");
        if !dir.exists() {
            println!("No remote operation logs for '{remote}'");
            return Ok(());
        }
        let mut paths = std::fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.sort();
        for path in paths {
            if path.extension().is_some_and(|extension| extension == "log") {
                println!("{}", path.display());
            }
        }
    }
    Ok(())
}

pub fn run(remote: &str, ws_id: &str, id: &str) -> Result<()> {
    super::node::validate_id(ws_id, 'w', "workspace id")?;
    super::node::validate_id(id, 'r', "request id")?;
    let records = cache::all();
    let record = owned_record(&records, remote, ws_id)
        .ok_or_else(|| anyhow::anyhow!("remote workspace no longer exists"))?
        .clone();
    std::env::set_current_dir(&record.project_root)?;
    let config = Config::load(None)?;
    let node = Node::connect(&RemoteSpec::resolve(&config, remote)?)?;
    // The node serializes claims with expiry and withdrawal. A stale snapshot,
    // observer restart or duplicate worker cannot run an operation twice.
    let claim: Claim = node.call(
        "operation",
        &Update {
            ws_id: ws_id.into(),
            id: id.into(),
            progress: None,
            outcome: None,
        },
        false,
    )?;
    // Stages are best effort: the operation must not depend on the requester.
    let progress = |stage: &str| {
        let _ = node.call::<_, Claim>(
            "operation",
            &Update {
                ws_id: ws_id.into(),
                id: id.into(),
                progress: Some(stage.into()),
                outcome: None,
            },
            false,
        );
    };
    progress(&format!(
        "Laptop started; follow with `workmux remote logs {remote} {id}`"
    ));
    let result = (|| -> Result<()> {
        let receipt_dir = cache::remote_dir(remote)?.join("operations");
        std::fs::create_dir_all(&receipt_dir)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(receipt_dir.join(id))
        {
            Ok(_) => {
                println!("Remote operation {id}: {remote}/{}", record.handle);
                if claim.sandboxed && !git_hooks_disabled() {
                    bail!("a sandboxed agent's request needs git hooks disabled");
                }
                execute(record, config, claim, &progress)
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow::anyhow!(
                "request was already acted on; submit a new request"
            )),
            Err(err) => Err(err.into()),
        }
    })();
    let outcome = match &result {
        Ok(()) => Outcome {
            success: true,
            message: "Laptop operation completed".into(),
        },
        Err(err) => {
            tracing::warn!("remote operation failed: {err:#}");
            eprintln!("Remote operation failed: {err:#}");
            Outcome {
                success: false,
                message: format!(
                    "{}; on the laptop, run `workmux remote logs {remote} {id}`",
                    refusal(err)
                ),
            }
        }
    };
    // Successful cleanup removes the record, so there is nobody left to notify.
    let _ = node.call::<_, Claim>(
        "operation",
        &Update {
            ws_id: ws_id.into(),
            id: id.into(),
            progress: None,
            outcome: Some(outcome),
        },
        false,
    );
    result
}

/// Whether this worker was started with `disable_git_hooks`.
fn git_hooks_disabled() -> bool {
    std::env::var_os("GIT_CONFIG_PARAMETERS").is_none()
        && std::env::var("GIT_CONFIG_COUNT").as_deref() == Ok("1")
        && std::env::var("GIT_CONFIG_KEY_0").as_deref() == Ok("core.hooksPath")
        && std::env::var("GIT_CONFIG_VALUE_0").as_deref() == Ok("/dev/null")
}

/// Hooks run outside any sandbox, so a sandboxed agent's request runs
/// none, as the laptop's own supervisor does for its merges.
fn execute(
    record: WorkspaceRecord,
    config: Config,
    claim: Claim,
    progress: &dyn Fn(&str),
) -> Result<()> {
    let mux = create_backend(detect_backend());
    let run_hooks = !claim.sandboxed;
    match claim.operation {
        Operation::Remove { force } => {
            let hooks = run_hooks
                && config
                    .pre_remove
                    .as_ref()
                    .is_some_and(|hooks| !hooks.is_empty());
            progress(if hooks {
                "Running pre_remove hooks and removing…"
            } else {
                "Removing…"
            });
            let mut batch = super::lifecycle::RemovalBatch::new([record.ws_id.clone()]);
            batch.remove(
                record.clone(),
                super::lifecycle::RemoveOptions {
                    force,
                    keep_branch: false,
                    run_hooks,
                    confirm_unmerged: false,
                    refuse_unmerged: true,
                },
            )?;
            super::lifecycle::close_relay(mux.as_ref(), &record);
            Ok(())
        }
        Operation::Merge {
            into,
            mut rebase,
            mut squash,
            ignore_uncommitted,
            keep,
            cleanup,
            no_verify,
            no_hooks,
            notification,
        } => {
            if (keep && cleanup) || (rebase && squash) {
                bail!("incompatible merge options");
            }
            if !rebase && !squash {
                rebase = config.merge_strategy == Some(MergeStrategy::Rebase);
                squash = config.merge_strategy == Some(MergeStrategy::Squash);
            }
            if squash {
                bail!("squash needs an interactive commit message; run merge on the laptop");
            }
            // Resolve against laptop-owned branches before passing a target to
            // merge or hooks. Revision expressions and options are not branches.
            let into = into.map(|name| resolve_into(&record, &name)).transpose()?;
            let keep = !cleanup && (keep || config.merge_keep.unwrap_or(false));
            let context = WorkflowContext::new(config, mux, None)?;
            super::homecoming::merge(
                &context,
                record,
                super::homecoming::MergeOptions {
                    into: into.as_deref(),
                    ignore_uncommitted,
                    rebase,
                    squash: false,
                    keep,
                    no_verify: no_verify || !run_hooks,
                    no_hooks: no_hooks || !run_hooks,
                    notification,
                    noninteractive: true,
                    progress: Some(progress),
                },
            )
        }
    }
}

fn resolve_into(record: &WorkspaceRecord, name: &str) -> Result<String> {
    let branches = super::git::run(
        &record.project_root,
        &["for-each-ref", "--format=%(refname:strip=2)", "refs/heads/"],
    )?;
    branches
        .lines()
        .find(|branch| *branch == name && !branch.starts_with('-'))
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("merge target must be an existing laptop branch"))
}

/// Never send arbitrary git output, hook output or laptop paths to the node.
fn refusal(err: &anyhow::Error) -> String {
    let text = format!("{err:#}").to_lowercase();
    let message = if text.contains("not merged into") {
        "has commits not merged into its base; run `workmux rm --force`"
    } else if text.contains("uncommitted")
        || text.contains("unstaged")
        || text.contains("untracked")
    {
        "has uncommitted changes; commit them or explicitly use --force (rm) / --ignore-uncommitted (merge)"
    } else if text.contains("interactive") {
        "this merge needs interaction; commit staged changes first or run merge on the laptop"
    } else if text.contains("conflict") {
        "merge conflict; resolve it on the laptop, then run workmux merge there"
    } else if text.contains("hook") || text.contains("post-create") {
        "a laptop hook failed; inspect its output on the laptop"
    } else if text.contains("existing laptop branch") {
        "merge target must be an existing laptop branch"
    } else {
        "laptop operation failed; inspect it on the laptop with workmux"
    };
    message.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn log_paths_reject_untrusted_components() {
        assert!(log_path("../escape", "r123456789abc").is_err());
        assert!(log_path("trusted", "../escape").is_err());
        assert!(
            log_path("trusted", "r123456789abc")
                .unwrap()
                .ends_with("remote/trusted/operation-logs/r123456789abc.log")
        );
    }

    #[test]
    fn duplicate_workers_preserve_private_output() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.log");
        writeln!(open_log(&path).unwrap(), "first").unwrap();
        writeln!(open_log(&path).unwrap(), "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(open_log(&link).is_err());
        }
    }

    #[test]
    fn identity_is_scoped_to_the_reporting_remote() {
        let record = WorkspaceRecord::new(
            "w123456789abc".into(),
            "trusted",
            "project".into(),
            "/laptop/project".into(),
            "mine",
            "mine",
            "0".repeat(40),
            None,
            "wm-mine".into(),
            "/laptop/mine".into(),
        );
        let records = [record];
        assert!(owned_record(&records, "trusted", "w123456789abc").is_some());
        assert!(owned_record(&records, "other", "w123456789abc").is_none());
        assert!(owned_record(&records, "trusted", "mine").is_none());
        assert!(owned_record(&records, "trusted", "/laptop/mine").is_none());
        assert!(owned_record(&records, "trusted", "w000000000000").is_none());
    }

    #[test]
    fn refusal_does_not_disclose_paths() {
        let err = anyhow::anyhow!("hook failed in /private/laptop/secret: token");
        assert_eq!(
            refusal(&err),
            "a laptop hook failed; inspect its output on the laptop"
        );
        assert!(!refusal(&anyhow::anyhow!("/private/laptop/secret")).contains("secret"));
    }
}
