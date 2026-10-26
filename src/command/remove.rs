use crate::multiplexer::{create_backend, detect_backend};
use crate::workflow::WorkflowContext;
use crate::{config, git, spinner, workflow};
use anyhow::{Context, Result, anyhow};
use std::io::{self, Write};
use std::path::PathBuf;

pub fn run(
    names: Vec<String>,
    gone: bool,
    all: bool,
    force: bool,
    keep_branch: bool,
) -> Result<()> {
    if crate::remote::node::operation::in_session() {
        if gone || all || keep_branch {
            anyhow::bail!(
                "remote rm does not support --gone, --all or --keep-branch; run it on the laptop"
            );
        }
        return crate::remote::node::operation::submit(
            &names,
            crate::remote::node::operation::Operation::Remove { force },
        );
    }
    if crate::sandbox::guest::is_sandbox_guest() {
        if gone || all || keep_branch || names.len() > 1 {
            anyhow::bail!(
                "a sandboxed agent can only remove its own workspace, without --gone, --all or --keep-branch"
            );
        }
        let name = super::resolve_name(names.first().map(String::as_str))?;
        return run_via_rpc(&name, force);
    }
    let config = config::Config::load(None)?;
    let confirm_unmerged = config.confirm_unmerged_removal.unwrap_or(true);

    if all {
        return run_all(force, keep_branch, confirm_unmerged);
    }

    if gone {
        return run_gone(force, keep_branch, confirm_unmerged);
    }

    run_specified(names, force, keep_branch, confirm_unmerged, config)
}

/// Ask the sandbox supervisor to remove this workspace, printing its output.
fn run_via_rpc(name: &str, force: bool) -> Result<()> {
    use crate::sandbox::rpc::{RpcClient, RpcRequest, RpcResponse};

    let mut client = RpcClient::from_env()?;
    client.send(&RpcRequest::Remove {
        name: name.to_string(),
        force,
    })?;
    loop {
        match client.recv()? {
            RpcResponse::Output { message } => {
                print!("{message}");
                io::stdout().flush().ok();
            }
            RpcResponse::Ok => return Ok(()),
            RpcResponse::Error { message } => anyhow::bail!("{message}"),
            _ => {}
        }
    }
}

/// Remove specific worktrees provided by user (or current if empty)
fn run_specified(
    names: Vec<String>,
    force: bool,
    keep_branch: bool,
    confirm_unmerged: bool,
    config: config::Config,
) -> Result<()> {
    // Normalize all inputs (handles "." and other special cases)
    let resolved_names: Vec<String> = if names.is_empty() {
        vec![super::resolve_name(None)?]
    } else {
        names
            .iter()
            .map(|n| super::resolve_name(Some(n)))
            .collect::<Result<Vec<_>>>()?
    };

    let mut local_names = Vec::new();
    let mut remote_failures = 0;
    let mux = create_backend(detect_backend());
    let current_window = mux.current_window_name()?;
    let mut removed = Vec::new();
    // Reaching a node can finish other pending cleanups. Resolve all targets
    // first so those records are not subsequently mistaken for local worktrees.
    let targets: Vec<_> = resolved_names
        .into_iter()
        .map(|name| {
            let record = crate::remote::cache::find_here(&name);
            (name, record)
        })
        .collect();
    let mut batch = crate::remote::lifecycle::RemovalBatch::new(
        targets
            .iter()
            .filter_map(|(_, record)| record.as_ref().map(|r| r.ws_id.clone())),
    );
    for (name, remote) in targets {
        let Some(record) = remote else {
            local_names.push(name);
            continue;
        };
        let options = crate::remote::lifecycle::RemoveOptions {
            force,
            keep_branch,
            run_hooks: true,
            confirm_unmerged,
            refuse_unmerged: false,
        };
        if let Err(err) = batch.remove(record.clone(), options) {
            eprintln!("✗ {err:#}");
            remote_failures += 1;
        } else if crate::remote::cache::find_here(&record.handle).is_none() {
            removed.push(record);
        }
    }
    let local = if local_names.is_empty() {
        Ok(())
    } else {
        remove_local(local_names, force, keep_branch, confirm_unmerged, config)
    };
    let result = local.and_then(|()| {
        if remote_failures > 0 {
            Err(anyhow!(
                "{remote_failures} remote workspace(s) could not be removed"
            ))
        } else {
            Ok(())
        }
    });
    // Closing the invoking window can terminate this process. Finish every
    // target and report failures before closing it, after all other windows.
    if removed
        .iter()
        .any(|record| current_window.as_deref() == Some(&record.window_name))
        && let Err(err) = &result
    {
        eprintln!("✗ {err:#}");
    }
    removed.sort_by_key(|record| current_window.as_deref() == Some(&record.window_name));
    for record in removed {
        crate::remote::lifecycle::close_relay(mux.as_ref(), &record);
    }
    result
}

fn remove_local(
    resolved_names: Vec<String>,
    force: bool,
    keep_branch: bool,
    confirm_unmerged: bool,
    config: config::Config,
) -> Result<()> {
    let mux = create_backend(detect_backend());
    let context = WorkflowContext::new(config, mux, None)?;

    // 2. Resolve all targets and validate they exist
    let mut candidates: Vec<(String, PathBuf, String)> = Vec::new();
    for name in resolved_names {
        let (worktree_path, branch_name) = match git::find_worktree(&name) {
            Ok(worktree) => worktree,
            Err(e) => {
                if let Some(path) = workflow::fallback_worktree_path(&name, &context)? {
                    (path, String::new())
                } else {
                    return Err(anyhow!(
                        "Worktree '{}' not found. Use 'workmux list' to see available worktrees.",
                        name
                    )
                    .context(e));
                }
            }
        };

        let handle = worktree_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                anyhow!(
                    "Could not derive handle from worktree path: {:?}",
                    worktree_path
                )
            })?
            .to_string();

        candidates.push((handle, worktree_path, branch_name));
    }

    // 3. If forced, skip all checks and remove
    if force {
        let mut failed: Vec<(String, String)> = Vec::new();

        for (handle, _, _) in candidates {
            if let Err(e) = remove_worktree(&handle, true, keep_branch) {
                failed.push((handle, format!("{e:#}")));
            }
        }

        if !failed.is_empty() {
            eprintln!("\nFailed to remove {} worktree(s):", failed.len());
            for (handle, error) in &failed {
                eprintln!("  - {}: {}", handle, error);
            }
            return Err(anyhow!("Some worktrees could not be removed"));
        }

        return Ok(());
    }

    // 4. Safety checks: categorize candidates
    let mut uncommitted: Vec<String> = Vec::new();
    let mut unmerged: Vec<(String, String, String)> = Vec::new(); // (handle, branch, base)
    let mut safe: Vec<String> = Vec::new();

    for (handle, path, branch) in candidates {
        // Check uncommitted (blocking)
        if path.exists()
            && !git::has_missing_admin_dir(&path)
            && git::has_uncommitted_changes(&path)?
        {
            uncommitted.push(handle);
            continue;
        }

        if branch.is_empty() && !keep_branch {
            return Err(anyhow!(
                "Worktree '{}' has broken Git metadata, so its branch cannot be determined. \
                Use --keep-branch to remove only the worktree directory.",
                handle
            ));
        }

        // Check unmerged (promptable), only if configured and we're deleting the branch
        if confirm_unmerged
            && !keep_branch
            && let Some(base) = is_unmerged(&branch)?
        {
            unmerged.push((handle, branch, base));
            continue;
        }

        safe.push(handle);
    }

    // 5. Handle blocking issues (uncommitted changes)
    if !uncommitted.is_empty() {
        eprintln!("The following worktrees have uncommitted changes:");
        for handle in &uncommitted {
            eprintln!("  - {}", handle);
        }
        return Err(anyhow!(
            "Cannot remove worktrees with uncommitted changes. Use --force to override."
        ));
    }

    // 6. Handle warnings (unmerged branches)
    if !unmerged.is_empty() {
        println!("The following branches have commits not merged into their base:");
        for (_, branch, base) in &unmerged {
            println!("  - {} (base: {})", branch, base);
        }
        println!("\nThis will delete the worktree, tmux window, and local branch.");
        print!("Are you sure you want to continue? [y/N] ");
        io::stdout().flush().context("Failed to flush stdout")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read input")?;

        if input.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return Ok(());
        }

        // Add unmerged candidates to safe list for processing
        for (handle, _, _) in unmerged {
            safe.push(handle);
        }
    }

    // 7. Execute removal
    for handle in safe {
        // force=true because we already checked/prompted
        remove_worktree(&handle, true, keep_branch)?;
    }

    Ok(())
}

/// Check if a branch has unmerged commits. Returns Some(base) if unmerged, None otherwise.
fn is_unmerged(branch: &str) -> Result<Option<String>> {
    let main_branch = git::get_default_branch().unwrap_or_else(|_| "main".to_string());

    let base = git::get_branch_base(branch)
        .ok()
        .unwrap_or_else(|| main_branch.clone());

    let base_commit = match git::get_merge_base(&base) {
        Ok(b) => b,
        Err(_) => {
            // If we can't determine base, try falling back to main
            match git::get_merge_base(&main_branch) {
                Ok(b) => b,
                Err(error) => {
                    return Err(error.context("Cannot establish whether the branch is merged"));
                }
            }
        }
    };

    let unmerged_branches = git::get_unmerged_branches(&base_commit)?;
    if unmerged_branches.contains(branch) {
        Ok(Some(base))
    } else {
        Ok(None)
    }
}

fn print_skipped_summary(label: &str, uncommitted: &[String], unmerged: &[String]) {
    if !uncommitted.is_empty() {
        println!(
            "\n{} {} worktree(s) with uncommitted changes:",
            label,
            uncommitted.len()
        );
        for branch in uncommitted {
            println!("  - {}", branch);
        }
    }
    if !unmerged.is_empty() {
        println!(
            "\n{} {} worktree(s) with unmerged commits:",
            label,
            unmerged.len()
        );
        for branch in unmerged {
            println!("  - {}", branch);
        }
    }
}

/// Print the list of worktrees to remove and optionally prompt for confirmation.
/// Returns `Ok(true)` if removal should proceed, `Ok(false)` if aborted.
fn prompt_removal_confirmation(
    to_remove: &[BulkRemovableWorktree],
    skipped_uncommitted: &[String],
    skipped_unmerged: &[String],
    header: &str,
    force: bool,
    emphasize_all: bool,
) -> Result<bool> {
    println!("{}", header);
    for worktree in to_remove {
        println!("  - {}", worktree.branch);
    }

    print_skipped_summary("Skipping", skipped_uncommitted, skipped_unmerged);

    if !force {
        let all_label = if emphasize_all { "ALL " } else { "" };
        print!(
            "\nAre you sure you want to remove {}{} worktree(s)? [y/N] ",
            all_label,
            to_remove.len(),
        );
        io::stdout().flush().context("Failed to flush stdout")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read user input")?;

        if input.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return Ok(false);
        }
    }

    Ok(true)
}

/// Report removal results: successful and failed removals.
fn report_removal_results(summary: &BulkRemovalSummary) {
    if summary.removed > 0 {
        println!("\n✓ Successfully removed {} worktree(s)", summary.removed);
    }

    if summary.scheduled > 0 {
        println!("\n✓ Scheduled removal of {} worktree(s)", summary.scheduled);
    }

    if !summary.failed.is_empty() {
        eprintln!("\nFailed to remove {} worktree(s):", summary.failed.len());
        for (branch, error) in &summary.failed {
            eprintln!("  - {}: {}", branch, error);
        }
    }
}

#[derive(Default)]
struct BulkRemovalSummary {
    removed: usize,
    scheduled: usize,
    failed: Vec<(String, String)>,
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum BulkSkipReason {
    Uncommitted,
    Unmerged,
}

struct BulkSkippedWorktree {
    branch: String,
    reason: BulkSkipReason,
}

struct BulkRemovableWorktree {
    branch: String,
    handle: String,
}

struct BulkRemovalPlan {
    to_remove: Vec<BulkRemovableWorktree>,
    skipped: Vec<BulkSkippedWorktree>,
}

enum BulkRemovalMode {
    All,
    Gone(std::collections::HashSet<String>),
}

impl BulkRemovalMode {
    fn confirmation_header(&self) -> &'static str {
        match self {
            BulkRemovalMode::All => "The following worktrees will be removed:",
            BulkRemovalMode::Gone(_) => {
                "The following worktrees have gone upstreams and will be removed:"
            }
        }
    }

    fn empty_scan_message(&self) -> &'static str {
        match self {
            BulkRemovalMode::All => "No worktrees to remove.",
            BulkRemovalMode::Gone(_) => "No worktrees with gone upstreams found.",
        }
    }

    fn no_removable_message(&self) -> &'static str {
        match self {
            BulkRemovalMode::All => "No removable worktrees found.",
            BulkRemovalMode::Gone(_) => "No worktrees to remove.",
        }
    }

    fn should_consider_branch(&self, branch: &str) -> bool {
        match self {
            BulkRemovalMode::All => true,
            BulkRemovalMode::Gone(gone_branches) => gone_branches.contains(branch),
        }
    }

    fn allow_unmerged_skip(&self) -> bool {
        matches!(self, BulkRemovalMode::All)
    }

    fn prompt_emphasize_all(&self) -> bool {
        matches!(self, BulkRemovalMode::All)
    }
}

fn collect_bulk_removal_plan(
    mode: &BulkRemovalMode,
    force: bool,
    keep_branch: bool,
    confirm_unmerged: bool,
) -> Result<BulkRemovalPlan> {
    let worktrees = git::list_worktrees()?;
    let main_branch = git::get_default_branch()?;
    let main_worktree_root = git::get_main_worktree_root()?;

    let mut plan = BulkRemovalPlan {
        to_remove: Vec::new(),
        skipped: Vec::new(),
    };

    for (path, branch) in worktrees {
        if branch == main_branch || branch == "(detached)" {
            continue;
        }

        if path == main_worktree_root {
            continue;
        }

        if !mode.should_consider_branch(&branch) {
            continue;
        }

        if !force && path.exists() && git::has_uncommitted_changes(&path)? {
            plan.skipped.push(BulkSkippedWorktree {
                branch,
                reason: BulkSkipReason::Uncommitted,
            });
            continue;
        }

        if confirm_unmerged && mode.allow_unmerged_skip() && !force && !keep_branch {
            let base = git::get_branch_base(&branch)
                .ok()
                .unwrap_or_else(|| main_branch.clone());
            let merge_base = git::get_merge_base(&base)
                .context("Cannot establish the merge base for bulk removal")?;
            let unmerged_branches = git::get_unmerged_branches(&merge_base)
                .context("Cannot establish merged branches for bulk removal")?;
            if unmerged_branches.contains(&branch) {
                plan.skipped.push(BulkSkippedWorktree {
                    branch,
                    reason: BulkSkipReason::Unmerged,
                });
                continue;
            }
        }

        let handle = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&branch)
            .to_string();

        plan.to_remove
            .push(BulkRemovableWorktree { branch, handle });
    }

    Ok(plan)
}

fn split_skipped_worktrees(skipped: &[BulkSkippedWorktree], reason: BulkSkipReason) -> Vec<String> {
    skipped
        .iter()
        .filter(|worktree| worktree.reason == reason)
        .map(|worktree| worktree.branch.clone())
        .collect()
}

fn execute_bulk_removals(
    to_remove: &[BulkRemovableWorktree],
    keep_branch: bool,
) -> BulkRemovalSummary {
    let mut summary = BulkRemovalSummary::default();

    for worktree in to_remove {
        match remove_worktree(&worktree.handle, true, keep_branch) {
            Ok(result) if result.cleanup_scheduled => summary.scheduled += 1,
            Ok(_) => summary.removed += 1,
            Err(error) => summary
                .failed
                .push((worktree.branch.clone(), format!("{error:#}"))),
        }
    }

    summary
}

fn run_bulk_removal(
    mode: BulkRemovalMode,
    force: bool,
    keep_branch: bool,
    confirm_unmerged: bool,
) -> Result<()> {
    let plan = collect_bulk_removal_plan(&mode, force, keep_branch, confirm_unmerged)?;

    let skipped_uncommitted = split_skipped_worktrees(&plan.skipped, BulkSkipReason::Uncommitted);
    let skipped_unmerged = split_skipped_worktrees(&plan.skipped, BulkSkipReason::Unmerged);

    if plan.to_remove.is_empty() && skipped_uncommitted.is_empty() && skipped_unmerged.is_empty() {
        println!("{}", mode.empty_scan_message());
        return Ok(());
    }

    if plan.to_remove.is_empty() {
        println!("{}", mode.no_removable_message());
        print_skipped_summary("Skipped", &skipped_uncommitted, &skipped_unmerged);
        println!("\nUse --force to remove these anyway.");
        return Ok(());
    }

    if !prompt_removal_confirmation(
        &plan.to_remove,
        &skipped_uncommitted,
        &skipped_unmerged,
        mode.confirmation_header(),
        force,
        mode.prompt_emphasize_all(),
    )? {
        return Ok(());
    }

    let summary = execute_bulk_removals(&plan.to_remove, keep_branch);
    report_removal_results(&summary);
    Ok(())
}

/// Remove all managed worktrees (except main)
fn run_all(force: bool, keep_branch: bool, confirm_unmerged: bool) -> Result<()> {
    run_bulk_removal(BulkRemovalMode::All, force, keep_branch, confirm_unmerged)
}

/// Remove worktrees whose upstream remote branch has been deleted
fn run_gone(force: bool, keep_branch: bool, confirm_unmerged: bool) -> Result<()> {
    // Fetch with prune to update remote-tracking refs
    spinner::with_spinner("Fetching from remote", git::fetch_prune)?;
    let gone_branches = git::get_gone_branches().unwrap_or_default();
    run_bulk_removal(
        BulkRemovalMode::Gone(gone_branches),
        force,
        keep_branch,
        confirm_unmerged,
    )
}

/// Execute the actual worktree removal
fn remove_worktree(
    handle: &str,
    force: bool,
    keep_branch: bool,
) -> Result<workflow::types::RemoveResult> {
    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let context = WorkflowContext::new(config, mux, None)?;

    super::announce_hooks(&context.config, None, super::HookPhase::PreRemove);

    let result = workflow::remove(handle, force, keep_branch, &context)
        .context("Failed to remove worktree")?;

    if !result.cleanup_scheduled {
        if keep_branch {
            println!(
                "✓ Removed worktree '{}' (branch '{}' kept)",
                handle, result.branch_removed
            );
        } else {
            println!(
                "✓ Removed worktree '{}' and branch '{}'",
                handle, result.branch_removed
            );
        }
        io::stdout().flush()?;
    }

    super::sidebar::request_refresh_for(context.mux.as_ref());

    Ok(result)
}
