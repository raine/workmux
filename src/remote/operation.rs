//! Detached laptop workers for requests made in node panes.

use std::process::{Command, Stdio};

use anyhow::{Result, bail};

use super::cache::{self, WorkspaceRecord};
use super::client::Node;
use super::node::operation::{Operation, Outcome, Update};
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
        if let Err(err) = spawn(record, &request.id) {
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

fn spawn(record: &WorkspaceRecord, id: &str) -> Result<()> {
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
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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
    let operation: Operation = node.call(
        "operation",
        &Update {
            ws_id: ws_id.into(),
            id: id.into(),
            outcome: None,
        },
        false,
    )?;
    let result = (|| -> Result<()> {
        let receipt_dir = cache::remote_dir(remote)?.join("operations");
        std::fs::create_dir_all(&receipt_dir)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(receipt_dir.join(id))
        {
            Ok(_) => execute(record, config, operation),
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
            Outcome {
                success: false,
                message: refusal(err),
            }
        }
    };
    // Successful cleanup removes the record, so there is nobody left to notify.
    let _ = node.call::<_, Operation>(
        "operation",
        &Update {
            ws_id: ws_id.into(),
            id: id.into(),
            outcome: Some(outcome),
        },
        false,
    );
    result
}

fn execute(record: WorkspaceRecord, config: Config, operation: Operation) -> Result<()> {
    let mux = create_backend(detect_backend());
    match operation {
        Operation::Remove { force } => {
            let mut batch = super::lifecycle::RemovalBatch::new([record.ws_id.clone()]);
            batch.remove(
                record.clone(),
                super::lifecycle::RemoveOptions {
                    force,
                    keep_branch: false,
                    run_hooks: true,
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
                    no_verify,
                    no_hooks,
                    notification: false,
                    noninteractive: true,
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
    } else if text.contains("hook") {
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
