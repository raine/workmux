//! Remote workspaces from the laptop: creation, the local window that
//! relays them, re-opening, closing and removal.
//!
//! The node owns a workspace and its running panes. The laptop keeps a
//! record of it (see [`super::cache`]) and a window whose panes each run
//! `workmux remote attach`; closing that window only detaches.

use std::fs;
use std::io::IsTerminal;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;

use super::RemoteSpec;
use super::cache::{self, ExecutionRecord, LayoutPane, WorkspaceRecord, WorkspaceState};
use super::client::{Node, error_code};
use super::portable::Portable;
use super::protocol::{
    self, FileEntry, FileEntryKind, ProvisionPayload, ProvisionRequest, ProvisionResponse,
    RemoveRequest, RemoveResponse, StartRequest, StartResponse, WorkspaceIdentity, code,
};
use crate::cmd::Cmd;
use crate::config::{Config, ConfigLocation};
use crate::multiplexer::{CreateWindowParams, Multiplexer};
use crate::prompt::Prompt;
use crate::shell::shell_quote;
use crate::workflow::{SetupOptions, WorkflowContext};

/// Combined size of `files.copy` results and symlink sources sent per
/// workspace.
const TRANSFER_LIMIT: u64 = 64 * 1024 * 1024;

pub struct CreateRequest<'a> {
    pub branch: &'a str,
    pub handle: &'a str,
    pub base: Option<&'a str>,
    pub prompt: Option<&'a Prompt>,
    pub agent: Option<&'a str>,
    pub location: Option<&'a ConfigLocation>,
    pub options: &'a SetupOptions,
    pub prompt_file_only: bool,
}

/// Create a workspace on `spec` and open its window. A record left in
/// `provisioning` by an interrupted call for the same handle is resumed.
pub fn create(
    context: &WorkflowContext,
    spec: &RemoteSpec,
    request: CreateRequest,
) -> Result<WorkspaceRecord> {
    let config = &context.config;
    if config.windows.is_some() {
        bail!("'windows' configuration needs session mode, which remote agents do not support");
    }
    if let Some(panes) = &config.panes {
        crate::config::validate_panes_config(panes)?;
    }
    context.ensure_mux_running()?;
    let mux = context.mux.as_ref();
    let project_root = crate::util::canon_or_self(&context.main_worktree_root);

    let resume = match cache::claimed(&project_root, request.handle, request.branch) {
        Some(record)
            if record.handle == request.handle
                && record.branch == request.branch
                && record.state == WorkspaceState::Provisioning
                && record.remote == spec.name =>
        {
            Some(record)
        }
        Some(record) if record.handle == request.handle => bail!(
            "'{}' already exists as a remote workspace on '{}'. Use `workmux open {}`",
            record.handle,
            record.remote,
            record.handle
        ),
        Some(record) => bail!(
            "Branch '{}' belongs to remote workspace '{}' on '{}'",
            record.branch,
            record.handle,
            record.remote
        ),
        None => None,
    };
    if resume.is_none()
        && crate::git::worktree_exists_in(request.branch, Some(&context.execution_dir))?
    {
        bail!(
            "A local worktree for branch '{}' already exists",
            request.branch
        );
    }
    if resume.is_none()
        && crate::git::find_worktree_in(request.handle, Some(&context.execution_dir)).is_ok()
    {
        bail!(
            "A local worktree named '{}' already exists.\n\
             Hint: use --name to pick another name.",
            request.handle
        );
    }
    let window_name = crate::multiplexer::util::prefixed(&context.prefix, request.handle);
    if mux.window_exists_by_full_name(&window_name)? {
        bail!(
            "A tmux window named '{window_name}' already exists.\n\
             Hint: use --name to pick another name."
        );
    }

    let prompt_text = request.prompt.map(Prompt::read_content).transpose()?;
    if prompt_text.is_some() && !request.prompt_file_only {
        let panes = crate::workflow::resolve_pane_configuration(
            config.panes.as_deref().unwrap_or_default(),
            config,
            request.agent,
        );
        let options = SetupOptions {
            prompt_file_path: Some(PathBuf::from("prompt")),
            ..request.options.clone()
        };
        crate::workflow::validate_prompt_consumption(&panes, request.agent, config, &options)?;
    }

    let mut record = match resume {
        Some(record) => {
            println!(
                "Resuming remote workspace '{}' on '{}'",
                record.handle, record.remote
            );
            record
        }
        None => {
            let (base_sha, base_label) = resolve_base(context, request.branch, request.base)?;
            let origin = crate::git::get_remote_url_in("origin", Some(&project_root)).ok();
            WorkspaceRecord::new(
                cache::new_id('w'),
                &spec.name,
                cache::project_id(&project_root, origin.as_deref()),
                project_root.clone(),
                request.handle,
                request.branch,
                base_sha,
                Some(base_label),
                window_name.clone(),
            )
        }
    };

    let node = Node::connect(spec)?;
    record.save()?;
    let (portable, provisioned) = (|| -> Result<(Portable, ProvisionResponse)> {
        let identity = WorkspaceIdentity {
            ws_id: record.ws_id.clone(),
            project_id: record.project_id.clone(),
            handle: record.handle.clone(),
            branch: record.branch.clone(),
            base_sha: record.base_sha.clone(),
        };
        let prepared: ProvisionResponse = node
            .call(
                protocol::PROVISION_WORKSPACE,
                &ProvisionRequest {
                    identity: identity.clone(),
                    isolation: spec.isolation,
                    payload: None,
                },
                false,
            )
            .map_err(|err| explain(spec, err))?;
        if !prepared.has_base {
            println!("Pushing {} to '{}'...", &record.base_sha[..12], spec.name);
            node.push_base(
                &project_root,
                &prepared.repo_path,
                &record.base_sha,
                &record.ws_id,
            )?;
        }

        let portable = super::portable::portable_config(
            config,
            request.location,
            spec.isolation,
            Path::new(&prepared.worktree_path),
        );
        for line in &portable.dropped {
            eprintln!("workmux: not applied on '{}': {line}", spec.name);
        }
        let files = if request.options.run_file_ops {
            collect_files(context, &spec.name)?
        } else {
            Files::default()
        };
        let payload = ProvisionPayload {
            config: crate::frozen_config::snapshot_document(
                &portable.config,
                portable.location.as_ref(),
            )?,
            agent: request.agent.map(str::to_string),
            prompt: prompt_text.clone(),
            prompt_file_only: request.prompt_file_only,
            copies: files.copies,
            seeds: files.seeds,
            symlink_patterns: files.patterns,
            ignored: files.ignored,
            run_file_ops: request.options.run_file_ops,
            run_hooks: request.options.run_hooks,
            git_user_name: git_config(&project_root, "user.name"),
            git_user_email: git_config(&project_root, "user.email"),
            secrets: portable.secrets.clone(),
        };
        let provisioned: ProvisionResponse = node
            .call(
                protocol::PROVISION_WORKSPACE,
                &ProvisionRequest {
                    identity,
                    isolation: spec.isolation,
                    payload: Some(payload),
                },
                true,
            )
            .map_err(|err| explain(spec, err))?;
        Ok((portable, provisioned))
    })()
    .with_context(|| {
        format!(
            "'{}' is partly set up on '{}'; run this `workmux add` again to resume it, \
             or `workmux remove {}`",
            record.handle, spec.name, record.handle
        )
    })?;
    record.remote_worktree = Some(provisioned.worktree_path.clone());
    record.state = WorkspaceState::Ready;
    record.layout = super::node::execution::pane_plan(&portable.config, request.agent)
        .into_iter()
        .map(|(index, pane)| LayoutPane { index, pane })
        .collect();
    record.observed();
    record.save()?;

    start_execution(
        &node,
        &mut record,
        prompt_text.is_some(),
        request.options.run_pane_commands,
    )
    .with_context(|| {
        format!(
            "'{}' is set up on '{}' but its agent did not start; run `workmux open {}` \
             to start it",
            record.handle, spec.name, record.handle
        )
    })?;

    open_window(context, &record, request.options.focus_window)?;
    Ok(record)
}

/// Start the workspace's execution, or rejoin the running one, and record
/// it. Returns whether a running execution was rejoined.
fn start_execution(
    node: &Node,
    record: &mut WorkspaceRecord,
    use_prompt: bool,
    run_commands: bool,
) -> Result<bool> {
    let started: StartResponse = node
        .call(
            protocol::START_EXECUTION,
            &StartRequest {
                ws_id: record.ws_id.clone(),
                ex_id: cache::new_id('x'),
                use_prompt,
                run_commands,
            },
            false,
        )
        .map_err(|err| explain(&node.spec, err))?;
    record.execution = Some(ExecutionRecord {
        ex_id: started.ex_id,
        panes: started.panes,
        node_bin: node.pin.name.clone(),
    });
    record.observed();
    record.save()?;
    Ok(started.reused)
}

/// The commit a new remote branch starts from and how it was named: the
/// tip of an existing local branch, else `base`, else the current branch.
fn resolve_base(
    context: &WorkflowContext,
    branch: &str,
    base: Option<&str>,
) -> Result<(String, String)> {
    let dir = &context.execution_dir;
    let commit = |name: &str| -> Result<String> {
        Cmd::new("git")
            .workdir(dir)
            .args(&["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")])
            .run_and_capture_stdout()
            .with_context(|| format!("'{name}' is not a commit"))
    };
    if let Ok(sha) = commit(&format!("refs/heads/{branch}")) {
        return Ok((sha, branch.to_string()));
    }
    let label = match base.filter(|base| !base.trim().is_empty()) {
        Some(base) => base.to_string(),
        None => {
            let current = crate::git::get_current_branch_in(dir)
                .context("Failed to determine the current branch to use as the base")?;
            let current = current.trim().to_string();
            if current.is_empty() {
                bail!(
                    "Cannot determine current branch (detached HEAD). \
                     Use --base to explicitly specify the starting point."
                );
            }
            current
        }
    };
    Ok((commit(&label)?, label))
}

fn git_config(dir: &Path, key: &str) -> Option<String> {
    Cmd::new("git")
        .workdir(dir)
        .args(&["config", "--get", key])
        .run_and_capture_stdout()
        .ok()
        .filter(|value| !value.is_empty())
}

/// Add what the user can do about a node refusal.
fn explain(spec: &RemoteSpec, err: anyhow::Error) -> anyhow::Error {
    match error_code(&err) {
        Some(code::UNSUPPORTED_RUNTIME) => err.context(format!(
            "Install docker on '{}' (and add your user to the docker group), or set \
             remotes.{}.isolation: none",
            spec.name, spec.name
        )),
        Some(code::BUSY) => err.context(format!(
            "Another workmux command is working on this workspace on '{}'",
            spec.name
        )),
        _ => err.context(format!("remote '{}'", spec.name)),
    }
}

#[derive(Default)]
struct Files {
    copies: Vec<FileEntry>,
    seeds: Vec<FileEntry>,
    patterns: Vec<String>,
    ignored: Vec<String>,
}

/// `files.copy` results and `files.symlink` sources from the config
/// directory, within [`TRANSFER_LIMIT`]. A source that does not fit is left
/// out whole and reported.
fn collect_files(context: &WorkflowContext, remote: &str) -> Result<Files> {
    use crate::workflow::file_ops::{FileOperationKind, resolve_file_operations};

    let source = &context.config_source_dir;
    let files_config = &context.config.files;
    let mut files = Files {
        patterns: files_config.symlink.clone().unwrap_or_default(),
        ..Files::default()
    };
    let mut sources: Vec<(PathBuf, FileOperationKind)> =
        resolve_file_operations(source, Path::new("/"), files_config)?
            .into_iter()
            .map(|operation| (operation.source, operation.kind))
            .collect();
    let claude_local = source.join("CLAUDE.local.md");
    if claude_local.is_file()
        && crate::git::is_path_ignored(source, "CLAUDE.local.md")
        && !sources.iter().any(|(path, _)| path == &claude_local)
    {
        sources.push((claude_local, FileOperationKind::Symlink));
        files.patterns.push("CLAUDE.local.md".to_string());
    }

    let mut budget = TRANSFER_LIMIT;
    for (path, kind) in sources {
        let relative = path.strip_prefix(source)?.to_path_buf();
        let size = tree_size(&path, true)?;
        let setting = match kind {
            FileOperationKind::Copy => "files.copy",
            FileOperationKind::Symlink => "files.symlink",
        };
        if size > budget {
            eprintln!(
                "workmux: {setting}: not sending {} ({:.1} MB) to '{remote}': over the {} MiB \
                 limit per workspace",
                relative.display(),
                size as f64 / 1_000_000.0,
                TRANSFER_LIMIT >> 20
            );
            continue;
        }
        budget -= size;
        let entries = match kind {
            FileOperationKind::Copy => &mut files.copies,
            FileOperationKind::Symlink => &mut files.seeds,
        };
        collect_tree(&path, &relative, true, entries)?;
        let relative = relative.to_string_lossy().into_owned();
        if crate::git::is_path_ignored(source, &relative) {
            files.ignored.push(relative);
        }
    }
    Ok(files)
}

fn tree_size(path: &Path, follow: bool) -> Result<u64> {
    let meta = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    }
    .with_context(|| format!("Failed to read {}", path.display()))?;
    if meta.is_dir() {
        let mut total = 0;
        for entry in fs::read_dir(path)? {
            total += tree_size(&entry?.path(), false)?;
        }
        Ok(total)
    } else if meta.is_file() {
        Ok(meta.len())
    } else {
        Ok(0)
    }
}

/// Entries for `path`, following it but preserving symlinks below it the
/// way local copies do; sockets and other special files are skipped.
fn collect_tree(
    path: &Path,
    relative: &Path,
    follow: bool,
    entries: &mut Vec<FileEntry>,
) -> Result<()> {
    let meta = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    }
    .with_context(|| format!("Failed to read {}", path.display()))?;
    let name = relative.to_string_lossy().into_owned();
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        entries.push(FileEntry {
            path: name,
            kind: FileEntryKind::Symlink {
                target: target.to_string_lossy().into_owned(),
            },
        });
    } else if meta.is_dir() {
        entries.push(FileEntry {
            path: name,
            kind: FileEntryKind::Dir,
        });
        let mut children: Vec<_> = fs::read_dir(path)?.collect::<std::io::Result<_>>()?;
        children.sort_by_key(|child| child.file_name());
        for child in children {
            collect_tree(
                &child.path(),
                &relative.join(child.file_name()),
                false,
                entries,
            )?;
        }
    } else if meta.is_file() {
        let data = fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
        entries.push(FileEntry {
            path: name,
            kind: FileEntryKind::File {
                mode: meta.permissions().mode(),
                data: base64::engine::general_purpose::STANDARD.encode(data),
            },
        });
    }
    Ok(())
}

/// Command a window pane runs to relay pane `index` of `ex_id`.
fn attach_command_line(remote: &str, ex_id: &str, index: usize) -> Result<String> {
    let exe = std::env::current_exe().context("Failed to locate the workmux binary")?;
    let loopback = if std::env::var_os(super::transport::LOOPBACK_ENV).is_some_and(|v| v == "1") {
        format!("env {}=1 ", super::transport::LOOPBACK_ENV)
    } else {
        String::new()
    };
    Ok(format!(
        "{loopback}{} remote attach {} {ex_id} --pane {index}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(remote)
    ))
}

/// Create the window for `record`'s execution: one pane per remote session,
/// split, sized, named and focused as configured.
pub fn open_window(context: &WorkflowContext, record: &WorkspaceRecord, focus: bool) -> Result<()> {
    let mux = context.mux.as_ref();
    let execution = record
        .execution
        .as_ref()
        .ok_or_else(|| anyhow!("remote workspace '{}' has no execution", record.handle))?;
    let layout: Vec<&LayoutPane> = record
        .layout
        .iter()
        .filter(|pane| execution.panes.contains(&pane.index))
        .collect();

    let cwd = &context.main_worktree_root;
    let placement = mux.resolve_window_placement_target(context.config.window_placement())?;
    let first = mux
        .create_window(CreateWindowParams {
            prefix: &context.prefix,
            name: &record.handle,
            cwd,
            after_window: placement.as_deref(),
        })
        .context("Failed to create window")?;

    let mut pane_ids: Vec<String> = Vec::new();
    let mut focus_pane = None;
    let mut zoom_pane = None;
    for (n, planned) in layout.iter().enumerate() {
        let command = attach_command_line(&record.remote, &execution.ex_id, planned.index)?;
        let pane = &planned.pane;
        let pane_id = if n == 0 {
            mux.respawn_pane(&first, cwd, Some(&command))?
        } else {
            let direction = pane
                .split
                .as_ref()
                .unwrap_or(&crate::config::SplitDirection::Horizontal);
            let target_index = pane.target.unwrap_or(pane_ids.len() - 1);
            let target = pane_ids
                .get(target_index)
                .ok_or_else(|| anyhow!("Invalid target pane index: {target_index}"))?;
            mux.split_pane(
                target,
                direction,
                cwd,
                pane.size,
                pane.percentage,
                Some(&command),
            )?
        };
        if let Some(name) = pane.name.as_deref() {
            let _ = mux.set_pane_name(&pane_id, name);
        }
        if pane.focus || pane.zoom {
            focus_pane = Some(pane_id.clone());
        }
        if pane.zoom {
            zoom_pane = Some(pane_id.clone());
        }
        pane_ids.push(pane_id);
    }

    if focus {
        mux.select_pane(focus_pane.as_deref().unwrap_or(&pane_ids[0]))?;
        mux.select_window(&context.prefix, &record.handle)?;
    }
    if let Some(zoom) = zoom_pane {
        mux.zoom_pane(&zoom)?;
    }
    Ok(())
}

/// `workmux open` of a remote workspace: focus its window, or start (or
/// rejoin) its execution and build the window again.
pub fn open(context: &WorkflowContext, mut record: WorkspaceRecord, focus: bool) -> Result<()> {
    let mux = context.mux.as_ref();
    if mux.window_exists_by_full_name(&record.window_name)? {
        if focus {
            mux.select_window(&context.prefix, &record.handle)?;
        }
        println!("✓ Switched to remote workspace '{}'", record.handle);
        return Ok(());
    }
    if record.state != WorkspaceState::Ready {
        bail!(
            "remote workspace '{}' did not finish provisioning; run `workmux add --remote {} {}` \
             again to resume it, or `workmux remove {}`",
            record.handle,
            record.remote,
            record.branch,
            record.handle
        );
    }
    let config = Config::load(None)?;
    let spec = RemoteSpec::resolve(&config, &record.remote)?;
    let node = Node::connect(&spec)?;
    let rejoined = start_execution(&node, &mut record, false, true)?;
    open_window(context, &record, focus)?;
    if rejoined {
        println!(
            "✓ Attached to the running agent of '{}' on '{}'",
            record.handle, record.remote
        );
    } else {
        println!(
            "✓ Started remote workspace '{}' on '{}'",
            record.handle, record.remote
        );
    }
    Ok(())
}

/// `workmux close` of a remote workspace: the window goes, the remote
/// sessions keep running.
pub fn close(mux: &dyn Multiplexer, record: &WorkspaceRecord) -> Result<()> {
    if !mux.window_exists_by_full_name(&record.window_name)? {
        bail!("No open window for remote workspace '{}'", record.handle);
    }
    mux.kill_window(&record.window_name)
        .context("Failed to close window")?;
    println!(
        "✓ Closed the window of '{}'; its agent keeps running on '{}' (`workmux open {}` \
         to return)",
        record.handle, record.remote, record.handle
    );
    Ok(())
}

pub struct RemoveOptions {
    pub force: bool,
    pub keep_branch: bool,
    pub run_hooks: bool,
}

/// `workmux remove` of a remote workspace. Local state goes only after the
/// node acknowledges, except that `--force` forgets a workspace whose
/// remote is unconfigured or unreachable.
pub fn remove(
    mux: &dyn Multiplexer,
    record: WorkspaceRecord,
    options: RemoveOptions,
) -> Result<()> {
    let forget = |reason: anyhow::Error| -> Result<()> {
        forget_locally(mux, &record)?;
        eprintln!(
            "workmux: forgot remote workspace '{}' without contacting '{}' ({reason:#}); \
             its worktree and agent may remain there",
            record.handle, record.remote
        );
        Ok(())
    };
    let config = Config::load(None)?;
    let spec = match RemoteSpec::resolve(&config, &record.remote) {
        Ok(spec) => spec,
        Err(err) if options.force => return forget(err),
        Err(err) => {
            return Err(err.context(format!(
                "Cannot remove '{}'; pass --force to forget it locally",
                record.handle
            )));
        }
    };
    let node = match Node::connect(&spec) {
        Ok(node) => node,
        Err(err) if options.force => return forget(err),
        Err(err) => {
            return Err(err.context(format!(
                "Nothing was removed; pass --force to forget '{}' locally",
                record.handle
            )));
        }
    };

    let call = |force: bool| -> Result<RemoveResponse> {
        node.call(
            protocol::REMOVE_WORKSPACE,
            &RemoveRequest {
                ws_id: record.ws_id.clone(),
                force,
                keep_branch: options.keep_branch,
                run_hooks: options.run_hooks,
            },
            true,
        )
    };
    match call(options.force) {
        Ok(_) => {}
        Err(err)
            if error_code(&err) == Some(code::UNSAVED_WORK) && std::io::stdin().is_terminal() =>
        {
            eprintln!("{err}");
            let prompt = format!(
                "Remove '{}' on '{}' and discard that work?",
                record.handle, record.remote
            );
            if !crate::ui::confirm::confirm(&prompt, crate::ui::confirm::ConfirmDefault::No)? {
                bail!("Aborted; nothing was removed");
            }
            call(true).map_err(|err| explain(&spec, err))?;
        }
        Err(err) if error_code(&err) == Some(code::UNSAVED_WORK) => {
            return Err(err.context(format!(
                "Refusing to remove '{}'; pass --force to discard that work",
                record.handle
            )));
        }
        Err(err) => return Err(explain(&spec, err)),
    }
    forget_locally(mux, &record)?;
    println!(
        "✓ Removed remote workspace '{}' from '{}'",
        record.handle, record.remote
    );
    Ok(())
}

fn forget_locally(mux: &dyn Multiplexer, record: &WorkspaceRecord) -> Result<()> {
    if mux
        .window_exists_by_full_name(&record.window_name)
        .unwrap_or(false)
    {
        let _ = mux.kill_window(&record.window_name);
    }
    record.delete()
}
