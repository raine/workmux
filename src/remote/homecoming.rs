//! Bringing a remote workspace's work to the laptop.
//!
//! `workmux merge <remote-handle>` and `workmux remote pull` export the
//! workspace on the node, fetch the snapshot and rebuild it as a local
//! worktree at the handle's usual path (see [`crate::remote::git`]). Merge
//! then runs the local merge on that copy; pull makes it the agent's new
//! home. The record's [`Homecoming`] stage makes each step resumable: once
//! the work lands, the stage only asks for the remote workspace to be
//! removed, so a retry never merges again. The node removes the workspace
//! only while it still matches the snapshot that was brought home.
//!
//! `workmux rebase` goes the other way: the laptop's base branch goes to the
//! node, which rebases the workspace onto it.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use super::RemoteSpec;
use super::cache::{self, Homecoming, WorkspaceRecord};
use super::client::{Node, error_code};
use super::git::{self, Snapshot};
use super::protocol::{
    self, CheckoutRequest, CheckoutResponse, ExportRequest, ExportResponse, RebaseRequest,
    RebaseResponse, RemoveRequest, RemoveResponse, code,
};
use crate::workflow::cleanup::{self as worktree_cleanup, CleanupOptions};
use crate::workflow::{self, CreateArgs, SetupOptions, WorkflowContext};

pub struct MergeOptions<'a> {
    pub into: Option<&'a str>,
    pub ignore_uncommitted: bool,
    pub rebase: bool,
    pub squash: bool,
    pub keep: bool,
    pub no_verify: bool,
    pub no_hooks: bool,
    pub notification: bool,
}

/// What became of a remote workspace whose work came home.
enum Removal {
    Removed,
    /// The workspace changed after the export and was kept.
    Diverged(String),
}

/// `workmux merge` of a remote workspace.
pub fn merge(
    context: &WorkflowContext,
    mut record: WorkspaceRecord,
    options: MergeOptions,
) -> Result<()> {
    let spec = RemoteSpec::resolve(&context.config, &record.remote)?;
    let mut resumed = None;
    match record.homecoming.clone() {
        Some(Homecoming::Merged { snapshot }) => {
            println!(
                "'{}' is already merged; removing its remote workspace",
                record.handle
            );
            let node = connect_for_cleanup(&spec, &record)?;
            return report_removal(&node, record, &snapshot, !options.no_hooks);
        }
        Some(Homecoming::Pulled { .. }) => bail!(
            "'{}' was pulled to {}; merge that worktree instead",
            record.handle,
            record.local_path.display()
        ),
        Some(Homecoming::Conflicted {
            snapshot,
            created_branch,
        }) if record.local_path.join(".git").exists() => {
            println!(
                "Resuming the merge of '{}' in {}",
                record.handle,
                record.local_path.display()
            );
            resumed = Some((snapshot, created_branch));
        }
        Some(
            Homecoming::Hydrating { created_branch, .. }
            | Homecoming::Conflicted { created_branch, .. },
        ) => discard_copy(context, &mut record, created_branch, false)?,
        None => {}
    }

    let (snapshot, created_branch, node) = match resumed {
        Some((snapshot, created_branch)) => {
            check_copy(&record, &options, true)?;
            (snapshot, created_branch, None)
        }
        None => {
            let node = Node::connect(&spec)?;
            finish_pending(&node, &record.ws_id);
            let export = export(&node, &mut record, false)?;
            let created_branch = hydrate(context, &node, &mut record, &export)?;
            let prepared = check_copy(&record, &options, false).and_then(|()| {
                provision_copy(context, &record, &export.snapshot, !options.no_hooks)
            });
            if let Err(err) = prepared {
                discard_copy(context, &mut record, created_branch, false)?;
                return Err(err);
            }
            (export.snapshot, created_branch, Some(node))
        }
    };

    let result = workflow::merge(
        &record.handle,
        options.into,
        options.ignore_uncommitted,
        options.rebase,
        options.squash,
        true,
        options.no_verify,
        options.no_hooks,
        options.notification,
        context,
    );
    let result = match result {
        Ok(result) => result,
        Err(err) => {
            if err.downcast_ref::<workflow::MergeConflict>().is_some() {
                record.homecoming = Some(Homecoming::Conflicted {
                    snapshot,
                    created_branch,
                });
                record.save()?;
                eprintln!(
                    "workmux: '{}' on '{}' is untouched; after resolving the conflict in {}, \
                     run `workmux merge {}` again",
                    record.handle,
                    record.remote,
                    record.local_path.display(),
                    record.handle
                );
            } else if node.is_some() {
                discard_copy(context, &mut record, created_branch, false)?;
            }
            return Err(err.context("Failed to merge worktree"));
        }
    };

    if result.had_staged_changes {
        println!("✓ Committed staged changes");
    }
    println!(
        "Merging '{}' into '{}'...",
        result.branch_merged, result.main_branch
    );
    println!("✓ Merged '{}'", result.branch_merged);

    if options.keep {
        let node = match node {
            Some(node) => Ok(node),
            None => Node::connect(&spec).inspect(|node| finish_pending(node, &record.ws_id)),
        };
        keep_branch_in_sync(context, node, &record, &snapshot);
        discard_copy(context, &mut record, created_branch, !options.no_hooks)?;
        println!("Remote workspace, agent and branch kept");
        return Ok(());
    }

    // The checkpoint that keeps a retry from merging again.
    record.homecoming = Some(Homecoming::Merged {
        snapshot: snapshot.clone(),
    });
    record.save()?;

    let local = worktree_cleanup::cleanup_headless(
        context,
        &record.branch,
        &record.handle,
        &record.local_path,
        CleanupOptions {
            force: true,
            keep_branch: false,
            no_hooks: options.no_hooks,
            show_hook_output: true,
        },
    );
    if let Err(err) = &local {
        eprintln!(
            "workmux: could not remove the local copy at {}: {err:#}",
            record.local_path.display()
        );
    }
    super::lifecycle::close_relay(context.mux.as_ref(), &record);

    let node = match node {
        Some(node) => node,
        None => connect_for_cleanup(&spec, &record)?,
    };
    report_removal(&node, record, &snapshot, !options.no_hooks)?;
    local.map(|_| ())
}

/// `workmux remote pull`: make a local worktree the workspace's home. The
/// remote agent stops, its work becomes a local worktree with a window,
/// and the remote workspace goes.
pub fn pull(context: &WorkflowContext, mut record: WorkspaceRecord, run_hooks: bool) -> Result<()> {
    let spec = RemoteSpec::resolve(&context.config, &record.remote)?;
    match record.homecoming.clone() {
        Some(Homecoming::Pulled { snapshot }) => {
            open_local(context, &record)?;
            let node = connect_for_cleanup(&spec, &record)?;
            return report_pull(&node, record, &snapshot);
        }
        Some(Homecoming::Merged { .. }) => bail!(
            "'{}' is already merged; `workmux merge {}` finishes removing its remote workspace",
            record.handle,
            record.handle
        ),
        Some(Homecoming::Conflicted { .. }) if record.local_path.join(".git").exists() => bail!(
            "'{}' has a merge in progress in {}; finish it with `workmux merge {}`",
            record.handle,
            record.local_path.display(),
            record.handle
        ),
        Some(
            Homecoming::Hydrating { created_branch, .. }
            | Homecoming::Conflicted { created_branch, .. },
        ) => discard_copy(context, &mut record, created_branch, false)?,
        None => {}
    }

    let node = Node::connect(&spec)?;
    finish_pending(&node, &record.ws_id);
    let export = export(&node, &mut record, true)?;
    let stopped = format!(
        "its agent on '{}' is stopped; `workmux open {}` restarts it there",
        record.remote, record.handle
    );
    let created_branch = hydrate(context, &node, &mut record, &export)
        .with_context(|| format!("Failed to bring '{}' home ({stopped})", record.handle))?;
    if let Err(err) = provision_copy(context, &record, &export.snapshot, run_hooks) {
        discard_copy(context, &mut record, created_branch, false)?;
        return Err(err.context(format!("Failed to set up the local worktree ({stopped})")));
    }
    record.homecoming = Some(Homecoming::Pulled {
        snapshot: export.snapshot.clone(),
    });
    record.save()?;
    println!(
        "✓ Brought '{}' home from '{}'\n  Worktree: {}",
        record.handle,
        record.remote,
        record.local_path.display()
    );

    super::lifecycle::close_relay(context.mux.as_ref(), &record);
    if let Err(err) = open_local(context, &record) {
        eprintln!(
            "workmux: could not open a window for '{}': {err:#}",
            record.handle
        );
    }
    report_pull(&node, record, &export.snapshot)
}

/// Open the pulled worktree's window unless it is open.
fn open_local(context: &WorkflowContext, record: &WorkspaceRecord) -> Result<()> {
    let mut options = SetupOptions::new(false, false, true);
    options.mode = context.config.mode();
    let result = workflow::open(&record.handle, context, options, false, None, None, None)?;
    if !result.did_switch {
        println!("✓ Opened a window for '{}'", record.handle);
    }
    Ok(())
}

fn report_pull(node: &Node, mut record: WorkspaceRecord, snapshot: &Snapshot) -> Result<()> {
    match remove_remote(node, &mut record, snapshot, true) {
        Ok(Removal::Removed) => {
            println!("✓ Removed the remote workspace from '{}'", record.remote);
            Ok(())
        }
        Ok(Removal::Diverged(message)) => {
            eprintln!(
                "workmux: {message}. The local worktree has the work exported at {}; the \
                 remote workspace was kept (`workmux remove {}` deletes it)",
                short(&snapshot.head),
                record.handle
            );
            Ok(())
        }
        Err(err) => Err(pending(&record, err)),
    }
}

/// `workmux rebase` of a remote workspace: rebase its branch on the node
/// onto the laptop's base branch, as a local rebase would.
pub fn rebase(config: &crate::config::Config, mut record: WorkspaceRecord) -> Result<()> {
    match record.homecoming {
        Some(Homecoming::Merged { .. }) => bail!(
            "'{}' is already merged; `workmux merge {}` finishes removing its remote workspace",
            record.handle,
            record.handle
        ),
        Some(_) => bail!(
            "'{}' is being merged or pulled; `workmux merge {}` or `workmux remote pull {}` \
             finishes that first",
            record.handle,
            record.handle,
            record.handle
        ),
        None => {}
    }
    let base = record.base_branch();
    if base == record.branch {
        bail!("Cannot rebase branch '{}' onto itself.", record.branch);
    }
    let onto = git::run(
        &record.project_root,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/heads/{base}^{{commit}}"),
        ],
    )
    .map_err(|_| anyhow!("Base branch '{base}' does not exist"))?;
    let spec = RemoteSpec::resolve(config, &record.remote)?;
    let node = Node::connect(&spec)?;
    finish_pending(&node, &record.ws_id);
    let repo = match record.remote_repo.clone() {
        Some(repo) => repo,
        None => export(&node, &mut record, false)?.repo_path,
    };
    println!(
        "Rebasing '{}' onto '{base}' on '{}'...",
        record.branch, record.remote
    );
    let onto_ref = super::node::git::onto_ref(&record.ws_id);
    node.push_ref(&record.project_root, &repo, &onto, &onto_ref)?;
    let _: RebaseResponse = node
        .call(
            protocol::REBASE,
            &RebaseRequest {
                ws_id: record.ws_id.clone(),
                onto: onto.clone(),
                committer: committer(&record.project_root),
            },
            false,
        )
        .map_err(|err| super::lifecycle::explain(&node.spec, err))?;
    record.base_sha = onto;
    record.observed();
    record.save()?;
    println!("✓ Rebased '{}' onto '{base}'", record.branch);
    Ok(())
}

/// The identity git commits as in `repo`.
fn committer(repo: &Path) -> Option<protocol::Committer> {
    let ident = git::run(repo, &["var", "GIT_COMMITTER_IDENT"]).ok()?;
    let (name, rest) = ident.split_once(" <")?;
    let (email, _) = rest.split_once('>')?;
    Some(protocol::Committer {
        name: name.to_string(),
        email: email.to_string(),
    })
}

/// Remove the remote workspace of merged `record` and say how it went.
fn report_removal(
    node: &Node,
    mut record: WorkspaceRecord,
    snapshot: &Snapshot,
    run_hooks: bool,
) -> Result<()> {
    match remove_remote(node, &mut record, snapshot, run_hooks) {
        Ok(Removal::Removed) => {
            println!(
                "✓ Successfully merged '{}' and removed its remote workspace from '{}'",
                record.branch, record.remote
            );
            Ok(())
        }
        Ok(Removal::Diverged(message)) => {
            eprintln!(
                "workmux: {message}. The work exported at {} is merged; the workspace was \
                 kept. `workmux open {}` continues it, `workmux remove {}` deletes it",
                short(&snapshot.head),
                record.handle,
                record.handle
            );
            Ok(())
        }
        Err(err) => Err(pending(&record, err)),
    }
}

/// The message of the node's refusal behind `err`.
pub(super) fn node_message(err: &anyhow::Error) -> String {
    err.downcast_ref::<protocol::NodeError>()
        .map(|err| err.message.clone())
        .unwrap_or_else(|| format!("{err:#}"))
}

/// `workmux remove` of a workspace whose work came home: remove the remote
/// workspace if it still holds only that work.
pub(super) fn finish_cleanup(node: &Node, record: WorkspaceRecord) -> Result<()> {
    match record.homecoming.clone() {
        Some(Homecoming::Pulled { snapshot }) => report_pull(node, record, &snapshot),
        Some(Homecoming::Merged { snapshot }) => report_removal(node, record, &snapshot, true),
        _ => Ok(()),
    }
}

/// Connect to finish `record`'s pending cleanup, and the others on its
/// remote.
pub(super) fn connect_for_cleanup(spec: &RemoteSpec, record: &WorkspaceRecord) -> Result<Node> {
    let node = Node::connect(spec).map_err(|err| pending(record, err))?;
    finish_pending(&node, &record.ws_id);
    Ok(node)
}

/// The error of `record`'s cleanup left pending by `err`: one line with the
/// cause and what finishes it. The whole error goes to the debug log.
fn pending(record: &WorkspaceRecord, err: anyhow::Error) -> anyhow::Error {
    tracing::debug!(handle = %record.handle, "remote cleanup pending: {err:?}");
    let (done, retry) = match record.homecoming {
        Some(Homecoming::Pulled { .. }) => ("pulled", "remote pull"),
        _ => ("merged", "merge"),
    };
    let cause = err
        .chain()
        .find_map(|err| err.downcast_ref::<super::transport::SshFailure>())
        .map(ToString::to_string)
        .unwrap_or_else(|| node_message(&err));
    anyhow!(
        "{done}; remote cleanup pending on '{remote}' ({cause}). `workmux {retry} {handle}`, or \
         the next merge, remove, open, add, rebase or `remote pull` that reaches '{remote}', \
         finishes it",
        cause = cause.lines().next().unwrap_or_default(),
        remote = record.remote,
        handle = record.handle,
    )
}

/// Remove the remote workspace if it still matches `snapshot`. The record
/// goes once the node acknowledges; a diverged workspace stays, with its
/// record back to normal.
fn remove_remote(
    node: &Node,
    record: &mut WorkspaceRecord,
    snapshot: &Snapshot,
    run_hooks: bool,
) -> Result<Removal> {
    let reply: Result<RemoveResponse> = node.call(
        protocol::REMOVE_WORKSPACE,
        &RemoveRequest {
            ws_id: record.ws_id.clone(),
            keep_branch: false,
            run_hooks,
            expect: Some(snapshot.clone()),
        },
        true,
    );
    match reply {
        Ok(_) => {
            forget_fetched(record);
            record.delete()?;
            Ok(Removal::Removed)
        }
        Err(err) if error_code(&err) == Some(code::DIVERGED) => {
            let message = node_message(&err);
            forget_fetched(record);
            record.homecoming = None;
            record.save()?;
            Ok(Removal::Diverged(message))
        }
        Err(err) => Err(super::lifecycle::explain(&node.spec, err)),
    }
}

/// Finish the remote cleanups an earlier merge or pull left pending on
/// `node`'s remote, except for workspace `skip`. Quiet unless one ends.
pub fn finish_pending(node: &Node, skip: &str) {
    for mut record in cache::all() {
        if record.remote != node.spec.name || record.ws_id == skip {
            continue;
        }
        let Some(Homecoming::Merged { snapshot } | Homecoming::Pulled { snapshot }) =
            record.homecoming.clone()
        else {
            continue;
        };
        match remove_remote(node, &mut record, &snapshot, true) {
            Ok(Removal::Removed) => println!(
                "✓ Finished removing remote workspace '{}' from '{}'",
                record.handle, record.remote
            ),
            Ok(Removal::Diverged(message)) => eprintln!(
                "workmux: {message}; kept the workspace (`workmux open {}` or `workmux remove {}`)",
                record.handle, record.handle
            ),
            Err(err) => tracing::debug!(handle = %record.handle, "cleanup still pending: {err:#}"),
        }
    }
}

pub(super) fn export(
    node: &Node,
    record: &mut WorkspaceRecord,
    stop: bool,
) -> Result<ExportResponse> {
    let export: ExportResponse = node
        .call(
            protocol::EXPORT,
            &ExportRequest {
                ws_id: record.ws_id.clone(),
                stop,
            },
            false,
        )
        .map_err(|err| super::lifecycle::explain(&node.spec, err))?;
    record.remote_repo = Some(export.repo_path.clone());
    record.observed();
    Ok(export)
}

/// Rebuild `export` as a headless local worktree at the handle's path, the
/// local branch at its head. Records the `Hydrating` stage before anything
/// is created. Returns whether the local branch was created.
fn hydrate(
    context: &WorkflowContext,
    node: &Node,
    record: &mut WorkspaceRecord,
    export: &ExportResponse,
) -> Result<bool> {
    let project_root = record.project_root.clone();
    let path = context.worktree_base_dir()?.join(&record.handle);
    if path.join(".git").exists() {
        bail!(
            "A local worktree already exists at {}; move it away to bring '{}' home",
            path.display(),
            record.handle
        );
    }
    let snapshot = &export.snapshot;
    node.fetch_export(&project_root, &export.repo_path, &record.ws_id, snapshot)?;

    let local_ref = format!("refs/heads/{}", record.branch);
    let tip = git::run(&project_root, &["rev-parse", "--verify", "-q", &local_ref]).ok();
    if let Some(tip) = &tip
        && *tip != snapshot.head
    {
        let ancestor = crate::git::unattended_git(Some(&project_root))?
            .args(["merge-base", "--is-ancestor", tip, &snapshot.head])
            .status()
            .context("Failed to run git merge-base")?
            .success();
        if !ancestor {
            bail!(
                "Local branch '{}' has commits that '{}' on '{}' lacks; rename or delete it, \
                 then try again",
                record.branch,
                record.handle,
                record.remote
            );
        }
    }
    if crate::git::worktree_exists_in(&record.branch, Some(&context.execution_dir))? {
        bail!(
            "Branch '{}' is checked out in a local worktree; remove that worktree first",
            record.branch
        );
    }

    record.local_path = path.clone();
    record.homecoming = Some(Homecoming::Hydrating {
        snapshot: snapshot.clone(),
        created_branch: tip.is_none(),
    });
    record.save()?;

    git::run(
        &project_root,
        &[
            "update-ref",
            &local_ref,
            &snapshot.head,
            tip.as_deref().unwrap_or(""),
        ],
    )?;
    workflow::create_headless(
        context,
        CreateArgs {
            branch_name: &record.branch,
            handle: &record.handle,
            base_branch: None,
            remote_branch: None,
            checkout_ref: None,
            prompt: None,
            options: SetupOptions::new(false, false, false),
            mode_override: None,
            agent: None,
            is_explicit_name: true,
            prompt_file_only: false,
            fork_source: None,
        },
        false,
    )
    .context("Failed to create the local worktree")?;
    // A branch that existed before the workspace keeps the base it has.
    if let Some(base) = record.base_label_branch() {
        let _lock = crate::git::GitConfigLock::acquire(&context.git_common_dir)?;
        crate::git::set_branch_base_in(&record.branch, base, Some(&context.execution_dir))?;
    }
    apply(&path, snapshot)?;
    Ok(tip.is_none())
}

/// Make the files and index of the copy at `path` those of `snapshot`,
/// whose trees [`Node::fetch_export`] brought over.
fn apply(path: &Path, snapshot: &Snapshot) -> Result<()> {
    git::run(path, &["read-tree", "--reset", "-u", &snapshot.worktree])?;
    git::run(path, &["read-tree", &snapshot.index])?;
    let _ = crate::git::pinned_git(path)?
        .args(["update-index", "-q", "--refresh"])
        .output();
    Ok(())
}

/// The usual setup of a new worktree, then the export again, so the node's
/// files win over anything the setup wrote.
fn provision_copy(
    context: &WorkflowContext,
    record: &WorkspaceRecord,
    snapshot: &Snapshot,
    run_hooks: bool,
) -> Result<()> {
    let copy = &record.local_path;
    let subdir = copy.join(&context.config_rel_dir);
    let options = SetupOptions {
        working_dir: (!context.config_rel_dir.as_os_str().is_empty() && subdir.exists())
            .then_some(subdir),
        config_root: Some(context.config_source_dir.clone()),
        ..SetupOptions::new(run_hooks, true, false)
    };
    workflow::provision_environment(
        &record.branch,
        &record.handle,
        copy,
        &context.main_worktree_root,
        &context.config,
        &options,
        crate::cmd::ShellOutput::Inherit,
    )?;
    apply(copy, snapshot)
}

/// The checks the local merge makes before it starts, which it skips when
/// told to keep the worktree. `resumed`: the copy is the user's, from an
/// earlier conflict.
fn check_copy(record: &WorkspaceRecord, options: &MergeOptions, resumed: bool) -> Result<()> {
    let copy = &record.local_path;
    if let Some(reason) = git::unexportable(copy, &record.branch)? {
        bail!(
            "{reason} in {}; finish it there, then run `workmux merge {}` again",
            copy.display(),
            record.handle
        );
    }
    if options.keep || options.ignore_uncommitted {
        return Ok(());
    }
    let mut issues = Vec::new();
    if crate::git::has_unstaged_changes(copy)? {
        issues.push("unstaged changes");
    }
    if crate::git::has_untracked_files(copy)? {
        issues.push("untracked files (will be lost)");
    }
    if issues.is_empty() {
        return Ok(());
    }
    if resumed {
        bail!(
            "Worktree for '{}' has {}. Please stage or stash them, or use --ignore-uncommitted.",
            record.branch,
            issues.join(" and ")
        );
    }
    bail!(
        "Remote workspace '{}' on '{}' has {}. Please stage or stash them there \
         (`workmux open {}`), or use --ignore-uncommitted.",
        record.handle,
        record.remote,
        issues.join(" and "),
        record.handle
    )
}

/// After a `--keep` merge, move the remote branch to the merged local one
/// (a staged-change commit, a rebase), as a kept local worktree would be.
fn keep_branch_in_sync(
    context: &WorkflowContext,
    node: Result<Node>,
    record: &WorkspaceRecord,
    snapshot: &Snapshot,
) {
    let local_ref = format!("refs/heads/{}", record.branch);
    let Ok(tip) = git::run(
        &context.main_worktree_root,
        &["rev-parse", "--verify", &local_ref],
    ) else {
        return;
    };
    if tip == snapshot.head {
        return;
    }
    let moved = node.and_then(|node| {
        let repo = record
            .remote_repo
            .as_deref()
            .ok_or_else(|| anyhow!("the node repository is unknown"))?;
        node.push_ref(
            &record.project_root,
            repo,
            &tip,
            &super::node::git::kept_ref(&record.ws_id),
        )?;
        let _: CheckoutResponse = node.call(
            protocol::CHECKOUT,
            &CheckoutRequest {
                ws_id: record.ws_id.clone(),
                head: tip.clone(),
                expect: snapshot.clone(),
            },
            false,
        )?;
        Ok(())
    });
    match moved {
        Ok(()) => println!(
            "✓ Moved '{}' on '{}' to the merged commits",
            record.branch, record.remote
        ),
        Err(err) => eprintln!(
            "workmux: left '{}' on '{}' as it was: {err:#}",
            record.branch, record.remote
        ),
    }
}

/// Remove the local copy (and the local branch if hydrating created it)
/// and clear the stage.
fn discard_copy(
    context: &WorkflowContext,
    record: &mut WorkspaceRecord,
    created_branch: bool,
    run_hooks: bool,
) -> Result<()> {
    let path = record.local_path.clone();
    if path.join(".git").exists() {
        worktree_cleanup::cleanup_headless(
            context,
            &record.branch,
            &record.handle,
            &path,
            CleanupOptions {
                force: true,
                keep_branch: !created_branch,
                no_hooks: !run_hooks,
                show_hook_output: run_hooks,
            },
        )
        .with_context(|| format!("Failed to remove the local copy at {}", path.display()))?;
    } else if created_branch
        && !crate::git::worktree_exists_in(&record.branch, Some(&context.execution_dir))?
    {
        let _ = crate::git::unattended_git(Some(&record.project_root))?
            .args(["branch", "-D", &record.branch])
            .output();
    }
    forget_fetched(record);
    record.homecoming = None;
    record.save()
}

/// Drop the refs a fetch left in the local repository.
pub(super) fn forget_fetched(record: &WorkspaceRecord) {
    for part in ["index", "worktree"] {
        let name = format!("refs/workmux/remote/{}/{part}", record.ws_id);
        let _ = git::run(&record.project_root, &["update-ref", "-d", &name]);
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}
