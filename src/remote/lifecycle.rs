//! Remote workspaces from the laptop: creation, the local window that
//! relays them, re-opening, closing and removal.
//!
//! The node owns a workspace and its running panes. The laptop keeps a
//! record of it (see [`super::cache`]) and a window whose panes each run
//! `workmux remote attach`; closing that window only detaches.

use std::fs;
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
use crate::config::{Config, ConfigLocation, MuxMode};
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
    /// The handle came from `--name`, so it is never suffixed.
    pub explicit_name: bool,
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
    let project_root = crate::util::canon_or_self(&context.main_worktree_root);

    let mut handle = request.handle.to_string();
    let resume = match cache::claimed(&project_root, &handle, request.branch) {
        Some(record)
            if record.branch == request.branch
                && record.state == WorkspaceState::Provisioning
                && record.remote == spec.name
                && (record.handle == handle
                    || (!request.explicit_name
                        && record.handle
                            == crate::workflow::project_suffixed_handle(context, &handle))) =>
        {
            Some(record)
        }
        Some(record) if record.handle == handle => bail!(
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
        None => {
            // Local add's rule for a window name another repository holds:
            // an implicit name gets this project's suffix.
            if !request.explicit_name
                && context.mux.window_exists(&context.prefix, &handle)?
                && !crate::git::worktree_exists_in(request.branch, Some(&context.execution_dir))?
            {
                (handle, _) =
                    crate::workflow::suffix_taken_handle(context, MuxMode::Window, &handle)?;
                if let Some(record) = cache::claimed(&project_root, &handle, request.branch) {
                    bail!(
                        "'{}' already exists as a remote workspace on '{}'. Use `workmux open \
                         {}`, or pick another name with --name",
                        record.handle,
                        record.remote,
                        record.handle
                    );
                }
            }
            None
        }
    };
    let window_name = match &resume {
        Some(record) => record.window_name.clone(),
        None => crate::multiplexer::util::prefixed(&context.prefix, &handle),
    };
    if let Some(conflict) = local_conflict(
        context,
        request.branch,
        &handle,
        &window_name,
        resume.is_none(),
        "\nHint: use --name to pick another name.",
    )? {
        bail!("{conflict}");
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
            WorkspaceRecord::new(
                cache::new_id('w'),
                &spec.name,
                cache::project_id_of(&project_root),
                project_root.clone(),
                &handle,
                request.branch,
                base_sha,
                Some(base_label),
                window_name.clone(),
                context.worktree_base_dir()?.join(&handle),
            )
        }
    };

    let node = Node::connect(spec)?;
    super::homecoming::finish_pending(&node, &record.ws_id);
    super::observe::current_on(&node);
    record.save()?;
    let (portable, provisioned) = (|| -> Result<(Portable, ProvisionResponse)> {
        let identity = WorkspaceIdentity {
            ws_id: record.ws_id.clone(),
            project_id: record.project_id.clone(),
            handle: record.handle.clone(),
            branch: record.branch.clone(),
            base_sha: record.base_sha.clone(),
        };
        let plan = if request.options.run_file_ops {
            plan_files(context)?
        } else {
            FilePlan::default()
        };
        let seed_dir = request
            .location
            .map(|location| location.rel_dir.as_path())
            .unwrap_or_else(|| Path::new(""));
        let seed_paths = plan.seed_paths(&context.config_source_dir, seed_dir)?;
        let prepared: ProvisionResponse = node
            .call(
                protocol::PROVISION_WORKSPACE,
                &ProvisionRequest {
                    identity: identity.clone(),
                    isolation: spec.isolation,
                    payload: None,
                    seed_paths,
                },
                false,
            )
            .map_err(|err| explain(spec, err))?;
        record.remote_repo = Some(prepared.repo_path.clone());
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
            collect_files(
                &context.config_source_dir,
                &plan,
                &spec.name,
                seed_dir,
                &prepared.cached_seeds,
            )?
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
            cached_seeds: files.cached_seeds,
            symlink_patterns: files.patterns,
            ignored: files.ignored,
            run_file_ops: request.options.run_file_ops,
            run_hooks: request.options.run_hooks,
            git_user_name: git_config(&project_root, "user.name"),
            git_user_email: git_config(&project_root, "user.email"),
            secrets: portable.secrets.clone(),
        };
        let mut provision_request = ProvisionRequest {
            identity,
            isolation: spec.isolation,
            payload: Some(payload),
            seed_paths: Vec::new(),
        };
        let result = node.call::<_, ProvisionResponse>(
            protocol::PROVISION_WORKSPACE,
            &provision_request,
            true,
        );
        let provisioned = match result {
            Err(err) if error_code(&err) == Some(code::MISSING_SEED) => {
                // The cache can be removed between prepare and provision. Retry
                // once with full seeds; completed provisioning steps are idempotent.
                let files =
                    collect_files(&context.config_source_dir, &plan, &spec.name, seed_dir, &[])?;
                let payload = provision_request.payload.as_mut().unwrap();
                payload.copies = files.copies;
                payload.seeds = files.seeds;
                payload.cached_seeds.clear();
                payload.ignored = files.ignored;
                node.call(protocol::PROVISION_WORKSPACE, &provision_request, true)
                    .map_err(|err| explain(spec, err))?
            }
            other => other.map_err(|err| explain(spec, err))?,
        };
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
    record.layout = super::node::execution::layout(&portable.config, request.agent);
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

/// Why a new remote workspace cannot take `branch`, `handle` and
/// `window_name` on this laptop: a local worktree uses them (checked with
/// `worktrees`) or a window has the name. `hint` follows the messages about
/// the handle.
pub(super) fn local_conflict(
    context: &WorkflowContext,
    branch: &str,
    handle: &str,
    window_name: &str,
    worktrees: bool,
    hint: &str,
) -> Result<Option<String>> {
    let dir = Some(context.execution_dir.as_path());
    if worktrees && crate::git::worktree_exists_in(branch, dir)? {
        return Ok(Some(format!(
            "A local worktree for branch '{branch}' already exists"
        )));
    }
    if worktrees && crate::git::find_worktree_in(handle, dir).is_ok() {
        return Ok(Some(format!(
            "A local worktree named '{handle}' already exists.{hint}"
        )));
    }
    if context.mux.window_exists_by_full_name(window_name)? {
        return Ok(Some(format!(
            "A tmux window named '{window_name}' already exists.{hint}"
        )));
    }
    Ok(None)
}

/// Start the workspace's execution, or rejoin the running one, and record
/// it. Returns whether a running execution was rejoined.
fn start_execution(
    node: &Node,
    record: &mut WorkspaceRecord,
    use_prompt: bool,
    run_commands: bool,
) -> Result<StartResponse> {
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
        ex_id: started.ex_id.clone(),
        panes: started.panes.clone(),
        node_bin: node.pin.name.clone(),
    });
    record.observed();
    record.save()?;
    Ok(started)
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
pub(super) fn explain(spec: &RemoteSpec, err: anyhow::Error) -> anyhow::Error {
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
    cached_seeds: Vec<String>,
    patterns: Vec<String>,
    ignored: Vec<String>,
}

#[derive(Default)]
struct FilePlan {
    sources: Vec<(PathBuf, crate::workflow::file_ops::FileOperationKind)>,
    patterns: Vec<String>,
}

impl FilePlan {
    fn seed_paths(&self, source: &Path, directory: &Path) -> Result<Vec<String>> {
        use crate::workflow::file_ops::FileOperationKind;
        self.sources
            .iter()
            .filter(|(_, kind)| *kind == FileOperationKind::Symlink)
            .map(|(path, _)| {
                Ok(directory
                    .join(path.strip_prefix(source)?)
                    .to_string_lossy()
                    .into_owned())
            })
            .collect()
    }
}

/// Resolve names without traversing or reading the source trees.
fn plan_files(context: &WorkflowContext) -> Result<FilePlan> {
    use crate::workflow::file_ops::{FileOperationKind, resolve_file_operations};
    let source = &context.config_source_dir;
    let files_config = &context.config.files;
    let mut plan = FilePlan {
        patterns: files_config.symlink.clone().unwrap_or_default(),
        ..FilePlan::default()
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
        plan.patterns.push("CLAUDE.local.md".to_string());
    }

    plan.sources = sources;
    Ok(plan)
}

/// Collect only missing seeds, leaving the budget for bytes actually sent.
fn collect_files(
    source: &Path,
    plan: &FilePlan,
    remote: &str,
    seed_dir: &Path,
    cached: &[String],
) -> Result<Files> {
    use crate::workflow::file_ops::FileOperationKind;
    let mut files = Files {
        patterns: plan.patterns.clone(),
        ..Files::default()
    };
    let mut budget = TRANSFER_LIMIT;
    for (path, kind) in &plan.sources {
        let relative = path.strip_prefix(source)?.to_path_buf();
        let name = relative.to_string_lossy().into_owned();
        if *kind == FileOperationKind::Symlink
            && cached.contains(&seed_dir.join(&relative).to_string_lossy().into_owned())
        {
            files.cached_seeds.push(name.clone());
            if crate::git::is_path_ignored(source, &name) {
                files.ignored.push(name);
            }
            continue;
        }
        let size = tree_size(path, true)?;
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
        collect_tree(path, &relative, true, entries)?;
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
    let placement = context
        .mux
        .resolve_window_placement_target(context.config.window_placement())?;
    open_window_after(context, record, focus, placement.as_deref())
}

/// [`open_window`], placed after the tmux window `after`.
pub(super) fn open_window_after(
    context: &WorkflowContext,
    record: &WorkspaceRecord,
    focus: bool,
    after: Option<&str>,
) -> Result<()> {
    let mux = context.mux.as_ref();
    let execution = record.execution.as_ref().ok_or_else(|| {
        anyhow!(
            "remote workspace '{}' on '{}' has no running agent; run `workmux open {}`",
            record.handle,
            record.remote,
            record.handle
        )
    })?;
    let layout: Vec<&LayoutPane> = record
        .layout
        .iter()
        .filter(|pane| execution.panes.contains(&pane.index))
        .collect();

    let cwd = &context.main_worktree_root;
    let first = mux
        .create_window(CreateWindowParams {
            prefix: &context.prefix,
            name: &record.handle,
            cwd,
            after_window: after,
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
/// rejoin) its execution and build the window again. A window whose
/// execution may have ended (the remote said so, or is not seen) is checked
/// with the node first, and replaced when the execution starts again. A
/// merged workspace only has its removal finished.
pub fn open(context: &WorkflowContext, mut record: WorkspaceRecord, focus: bool) -> Result<()> {
    if record.cleanup_pending() {
        let config = Config::load(None)?;
        let spec = RemoteSpec::resolve(&config, &record.remote)?;
        let node = super::homecoming::connect_for_cleanup(&spec, &record)?;
        return super::homecoming::finish_cleanup(&node, record);
    }
    let mux = context.mux.as_ref();
    let switch = |record: &WorkspaceRecord| -> Result<()> {
        if focus {
            mux.select_window(&context.prefix, &record.handle)?;
        }
        println!("✓ Switched to remote workspace '{}'", record.handle);
        Ok(())
    };
    let window = mux.window_exists_by_full_name(&record.window_name)?;
    let stale = super::observe::load_connected(&record.remote)
        .and_then(|observation| Some(observation.of(&record)?.ended.is_some()))
        .unwrap_or(window);
    if window && !stale {
        return switch(&record);
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
    let node = match Node::connect(&spec) {
        Ok(node) => node,
        Err(err) if window => {
            tracing::debug!(remote = %spec.name, "open: remote not reachable: {err:#}");
            return switch(&record);
        }
        Err(err) => {
            return Err(err.context(format!(
                "Cannot reach '{}' to open '{}'",
                spec.name, record.handle
            )));
        }
    };
    super::homecoming::finish_pending(&node, &record.ws_id);
    // Observing adopts siblings, which may open this window meanwhile.
    super::observe::current_on(&node);
    if !window && mux.window_exists_by_full_name(&record.window_name)? {
        return switch(&record);
    }
    let previous = record
        .execution
        .as_ref()
        .map(|execution| execution.ex_id.clone());
    let started = start_execution(&node, &mut record, false, true)?;
    if window {
        let current = record.execution.as_ref().map(|execution| &execution.ex_id);
        if started.reused && current == previous.as_ref() {
            return switch(&record);
        }
        mux.kill_window(&record.window_name)
            .context("Failed to close the window of the ended execution")?;
        forget_attachments(&record);
    }
    open_window(context, &record, focus)?;
    if record.window_pending {
        record.window_pending = false;
        record.save()?;
    }
    if started.reused {
        println!(
            "✓ Attached to the running agent of '{}' on '{}'",
            record.handle, record.remote
        );
    } else if started.continued {
        println!(
            "✓ Resumed '{}' on '{}' after the host rebooted; the agent continues its \
             last conversation",
            record.handle, record.remote
        );
    } else if started.after_reboot {
        println!(
            "✓ Restarted '{}' on '{}' after the host rebooted; its agent starts a new \
             conversation",
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
        bail!(
            "No open window for remote workspace '{}' on '{}'",
            record.handle,
            record.remote
        );
    }
    mux.kill_window(&record.window_name)
        .context("Failed to close window")?;
    forget_attachments(record);
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
    /// Ask before deleting commits the local base lacks.
    pub confirm_unmerged: bool,
}

/// `workmux remove` of a remote workspace, with the checks a local remove
/// makes: uncommitted work refuses and unmerged commits ask, unless
/// `--force`. `--keep-branch` brings the branch to the laptop. Local state
/// goes only after the node acknowledges, except that `--force` forgets a
/// workspace whose remote is unconfigured or unreachable.
pub fn remove(
    mux: &dyn Multiplexer,
    mut record: WorkspaceRecord,
    options: RemoveOptions,
) -> Result<()> {
    if record
        .homecoming
        .as_ref()
        .is_some_and(|stage| !stage.cleanup_pending())
    {
        if !options.force {
            bail!(
                "A merge of '{}' is in progress in {}; finish it with `workmux merge {}`, or \
                 pass --force to remove the remote workspace and keep that copy",
                record.handle,
                record.local_path.display(),
                record.handle
            );
        }
        record.homecoming = None;
    }
    let forget = |record: &WorkspaceRecord, reason: anyhow::Error| -> Result<()> {
        forget_locally(mux, record)?;
        eprintln!(
            "workmux: forgot remote workspace '{}' without contacting '{}' ({reason:#}); \
             its worktree and agent may remain there (`workmux remote gc {}` removes them)",
            record.handle, record.remote, record.remote
        );
        Ok(())
    };
    let config = Config::load(None)?;
    let spec = match RemoteSpec::resolve(&config, &record.remote) {
        Ok(spec) => spec,
        Err(err) if options.force => return forget(&record, err),
        Err(err) => {
            return Err(err.context(format!(
                "Cannot remove '{}'; pass --force to forget it locally",
                record.handle
            )));
        }
    };
    let node = match Node::connect(&spec) {
        Ok(node) => node,
        Err(err) if options.force => return forget(&record, err),
        Err(err) => {
            return Err(err.context(format!(
                "Cannot reach '{}' to remove '{}'; nothing was removed (pass --force to forget \
                 it locally)",
                record.remote, record.handle
            )));
        }
    };
    super::homecoming::finish_pending(&node, &record.ws_id);
    super::observe::current_on(&node);
    if record.cleanup_pending() {
        return super::homecoming::finish_cleanup(&node, record);
    }

    let mut expect = None;
    if !options.force || options.keep_branch {
        match check_removable(&node, &mut record, &options) {
            Ok(Some(snapshot)) => expect = Some(snapshot),
            Ok(None) => {}
            Err(err) if err.is::<Declined>() => {
                println!("Aborted.");
                return Ok(());
            }
            Err(err) if options.force => eprintln!("workmux: {err:#}"),
            Err(err) => return Err(err),
        }
    }
    if options.force {
        expect = None;
    }
    // Under `expect`, a kept branch is home exactly as the node has it, so
    // the node's copy goes; otherwise the node keeps it.
    let keep_on_node = options.keep_branch && expect.is_none();
    let reply: Result<RemoveResponse> = node.call(
        protocol::REMOVE_WORKSPACE,
        &RemoveRequest {
            ws_id: record.ws_id.clone(),
            keep_branch: keep_on_node,
            run_hooks: options.run_hooks,
            expect,
        },
        true,
    );
    match reply {
        Ok(_) => {}
        Err(err) if error_code(&err) == Some(code::DIVERGED) => {
            bail!(
                "{}; nothing was removed. Run `workmux remove {}` again",
                super::homecoming::node_message(&err),
                record.handle
            );
        }
        Err(err) => return Err(explain(&spec, err)),
    }
    if record.local_path.join(".git").exists() {
        println!(
            "The local copy at {} stays a local worktree (`workmux remove {}` removes it)",
            record.local_path.display(),
            record.handle
        );
    }
    super::homecoming::forget_fetched(&record);
    forget_locally(mux, &record)?;
    println!(
        "✓ Removed remote workspace '{}' from '{}'",
        record.handle, record.remote
    );
    Ok(())
}

/// The checks before removing `record`: refuses uncommitted work, asks
/// about commits its base lacks, and with `--keep-branch` brings the
/// branch to the laptop. Returns the snapshot the removal must still match,
/// or None when the node has no worktree left.
fn check_removable(
    node: &Node,
    record: &mut WorkspaceRecord,
    options: &RemoveOptions,
) -> Result<Option<super::git::Snapshot>> {
    let export = match super::homecoming::export(node, record, false) {
        Ok(export) => export,
        Err(err) if matches!(error_code(&err), Some(code::NOT_FOUND | code::NO_WORKTREE)) => {
            return Ok(None);
        }
        Err(err) if error_code(&err) == Some(code::CONFLICT) => bail!(
            "Cannot remove '{}': {}. Use --force to override.",
            record.handle,
            super::homecoming::node_message(&err)
        ),
        Err(err) => return Err(err),
    };
    let dirty = locally_unignored(record, &export.changes);
    // A cut-short listing may hide changes the laptop does not ignore.
    if !dirty.is_empty() || export.truncated {
        let mut listing: String = dirty.iter().map(|path| format!("\n  - {path}")).collect();
        if export.truncated {
            listing.push_str("\n  - ...");
        }
        bail!(
            "Remote workspace '{}' on '{}' has uncommitted changes:{listing}\nCannot remove it; \
             use --force to override.",
            record.handle,
            record.remote
        );
    }
    let head = &export.snapshot.head;
    if options.keep_branch {
        node.fetch_export(
            &record.project_root,
            &export.repo_path,
            &record.ws_id,
            &export.snapshot,
        )?;
        keep_branch_locally(record, head)?;
    } else if options.confirm_unmerged {
        let base = record.base_branch();
        if !is_ancestor(&record.project_root, head, &base) {
            println!("The following branches have commits not merged into their base:");
            println!("  - {} (base: {base})", record.branch);
            println!(
                "\nThis will delete the remote worktree on '{}', its agent and branch.",
                record.remote
            );
            let prompt = "Are you sure you want to continue?";
            if !crate::ui::confirm::confirm(prompt, crate::ui::confirm::ConfirmDefault::No)? {
                return Err(Declined.into());
            }
        }
    }
    Ok(Some(export.snapshot))
}

/// The user declined to remove a workspace whose commits its base lacks.
#[derive(Debug)]
struct Declined;

impl std::fmt::Display for Declined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("declined to remove unmerged commits")
    }
}

impl std::error::Error for Declined {}

/// `changes` without the paths the laptop's ignore rules (global excludes
/// included) leave out, as a local worktree's status would.
fn locally_unignored(record: &WorkspaceRecord, changes: &super::git::Changes) -> Vec<String> {
    let ignored = if changes.untracked.is_empty() {
        Default::default()
    } else {
        locally_ignored(&record.project_root, &changes.untracked).unwrap_or_else(|err| {
            tracing::debug!("git check-ignore failed: {err:#}");
            Default::default()
        })
    };
    let dirty: std::collections::BTreeSet<&String> = changes
        .staged
        .iter()
        .chain(&changes.unstaged)
        .chain(
            changes
                .untracked
                .iter()
                .filter(|path| !ignored.contains(*path)),
        )
        .collect();
    dirty.into_iter().cloned().collect()
}

fn locally_ignored(repo: &Path, paths: &[String]) -> Result<std::collections::HashSet<String>> {
    let mut child = crate::git::unattended_git(Some(repo))?
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("Failed to run git check-ignore")?;
    let input: Vec<u8> = paths
        .iter()
        .flat_map(|path| path.bytes().chain([0]))
        .collect();
    let mut stdin = child
        .stdin
        .take()
        .context("git check-ignore has no stdin")?;
    std::thread::spawn(move || std::io::Write::write_all(&mut stdin, &input));
    let output = child.wait_with_output()?;
    Ok(output
        .stdout
        .split(|b| *b == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

/// Whether commit `commit` exists here and is reachable from `rev`.
fn is_ancestor(repo: &Path, commit: &str, rev: &str) -> bool {
    crate::git::unattended_git(Some(repo))
        .and_then(|mut git| {
            Ok(git
                .args(["merge-base", "--is-ancestor", commit, rev])
                .stderr(std::process::Stdio::null())
                .status()?)
        })
        .is_ok_and(|status| status.success())
}

/// Point local branch `record.branch` at `head`, fetched from the node:
/// creates it, or fast-forwards it when no worktree has it checked out.
fn keep_branch_locally(record: &WorkspaceRecord, head: &str) -> Result<()> {
    let repo = &record.project_root;
    let local_ref = format!("refs/heads/{}", record.branch);
    let tip = super::git::run(repo, &["rev-parse", "--verify", "-q", &local_ref]).ok();
    match tip.as_deref() {
        Some(tip) if tip == head => {}
        Some(tip) if is_ancestor(repo, tip, head) => {
            if crate::git::worktree_exists_in(&record.branch, Some(repo))? {
                bail!(
                    "Branch '{}' is checked out locally and behind '{}' on '{}'; nothing was \
                     removed",
                    record.branch,
                    record.handle,
                    record.remote
                );
            }
            super::git::run(repo, &["update-ref", &local_ref, head, tip])?;
        }
        Some(_) => bail!(
            "Local branch '{}' has diverged from '{}' on '{}'; nothing was removed. Rename it, \
             or pass --force to keep only the remote branch",
            record.branch,
            record.handle,
            record.remote
        ),
        None => {
            super::git::run(repo, &["update-ref", &local_ref, head, ""])?;
        }
    }
    println!("✓ Kept branch '{}' locally", record.branch);
    Ok(())
}

pub(super) fn forget_locally(mux: &dyn Multiplexer, record: &WorkspaceRecord) -> Result<()> {
    close_relay(mux, record);
    record.delete()
}

/// Close the workspace's local window and forget its panes.
pub(super) fn close_relay(mux: &dyn Multiplexer, record: &WorkspaceRecord) {
    if mux
        .window_exists_by_full_name(&record.window_name)
        .unwrap_or(false)
    {
        let _ = mux.kill_window(&record.window_name);
    }
    forget_attachments(record);
}

/// Drop the state of the closed window's panes now, so a later server
/// restart cannot bring the window back through `resurrect`.
fn forget_attachments(record: &WorkspaceRecord) {
    let result = crate::state::StateStore::new()
        .and_then(|store| store.delete_remote_attachments(&record.remote, &record.ws_id));
    if let Err(err) = result {
        tracing::warn!(handle = %record.handle, "failed to forget attach panes: {err:#}");
    }
}

#[cfg(test)]
mod transfer_tests {
    use super::*;
    use crate::workflow::file_ops::FileOperationKind::{Copy, Symlink};

    #[test]
    fn cached_sources_are_not_read_and_copies_still_transfer() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("copy"), "new").unwrap();
        // An absent local tree proves cache hits do not stat or walk it.
        let plan = FilePlan {
            sources: vec![
                (dir.path().join("absent"), Symlink),
                (dir.path().join("copy"), Copy),
            ],
            patterns: vec!["absent".into()],
        };
        let cached = plan.seed_paths(dir.path(), Path::new("subdir")).unwrap();
        assert_eq!(cached, ["subdir/absent"]);
        let files = collect_files(dir.path(), &plan, "test", Path::new("subdir"), &cached).unwrap();
        assert!(files.seeds.is_empty());
        assert_eq!(files.cached_seeds, ["absent"]);
        assert_eq!(files.copies.len(), 1);
        // A response from an old node, or another config directory, is not a hit.
        assert!(collect_files(dir.path(), &plan, "test", Path::new(""), &cached).is_err());
        assert!(collect_files(dir.path(), &plan, "test", Path::new("subdir"), &[]).is_err());
    }

    #[test]
    #[ignore = "35 MiB transfer measurement; run explicitly with --nocapture"]
    fn measure_cold_and_warm_seed_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let block = vec![0x5a; 64 * 1024];
        let mut plan = FilePlan::default();
        for (name, count) in [("history", 80), ("tests/venv", 480)] {
            let path = dir.path().join(name);
            fs::create_dir_all(&path).unwrap();
            for index in 0..count {
                fs::write(path.join(format!("file-{index}")), &block).unwrap();
            }
            plan.sources.push((path, Symlink));
            plan.patterns.push(name.into());
        }
        let cached = plan.seed_paths(dir.path(), Path::new("")).unwrap();
        for (label, cache) in [("cold", &[][..]), ("warm", cached.as_slice())] {
            let start = std::time::Instant::now();
            let files = collect_files(dir.path(), &plan, "fixture", Path::new(""), cache).unwrap();
            #[derive(serde::Serialize)]
            struct Transfer<'a> {
                seeds: &'a [FileEntry],
                cached_seeds: &'a [String],
            }
            let payload = serde_json::to_vec(&Transfer {
                seeds: &files.seeds,
                cached_seeds: &files.cached_seeds,
            })
            .unwrap();
            eprintln!("{label}: {} bytes, {:?}", payload.len(), start.elapsed());
            if label == "warm" {
                assert!(payload.len() < 100);
            } else {
                assert!(payload.len() > 46_000_000);
            }
        }
    }
}
