use crate::multiplexer::{create_backend, detect_backend};
use crate::vcs::VcsBackend;
use crate::workflow::WorkflowContext;
use crate::{config, git, spinner, workflow};
use anyhow::{Context, Result, anyhow};
use std::io::{self, Write};
use std::path::PathBuf;

/// Find a worktree/workspace by handle (directory name) or branch/bookmark
/// name, mirroring `git::find_worktree`'s handle-then-branch matching but
/// generically over any [`VcsBackend`].
///
/// A workspace with no branch/bookmark checked out (a detached git worktree,
/// or an anonymous jj workspace) is reported with the same `"(detached)"`
/// sentinel `git::list_worktrees` used, so callers that already special-case
/// that literal (e.g. [`BulkRemovalMode`]'s main-branch skip) keep working
/// unchanged.
fn find_worktree(vcs: &dyn VcsBackend, name: &str) -> Result<(PathBuf, String)> {
    let workspaces = vcs.list_workspaces_in(None)?;

    // First: try to match by handle (directory name)
    for entry in &workspaces {
        if entry.name.as_deref() == Some(name) {
            let branch = entry
                .branch_or_bookmark
                .clone()
                .unwrap_or_else(|| "(detached)".to_string());
            return Ok((entry.path.clone(), branch));
        }
    }

    // Fallback: try to match by branch/bookmark name
    for entry in workspaces {
        if entry.branch_or_bookmark.as_deref() == Some(name) {
            let branch = entry.branch_or_bookmark.unwrap_or_default();
            return Ok((entry.path, branch));
        }
    }

    Err(anyhow!(
        "Worktree '{}' not found among the repository's worktrees/workspaces",
        name
    ))
}

pub fn run(
    names: Vec<String>,
    gone: bool,
    all: bool,
    force: bool,
    keep_branch: bool,
) -> Result<()> {
    if all {
        return run_all(force, keep_branch);
    }

    if gone {
        return run_gone(force, keep_branch);
    }

    run_specified(names, force, keep_branch)
}

/// Remove specific worktrees provided by user (or current if empty)
fn run_specified(names: Vec<String>, force: bool, keep_branch: bool) -> Result<()> {
    // Normalize all inputs (handles "." and other special cases)
    let resolved_names: Vec<String> = if names.is_empty() {
        vec![super::resolve_name(None)?]
    } else {
        names
            .iter()
            .map(|n| super::resolve_name(Some(n)))
            .collect::<Result<Vec<_>>>()?
    };

    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let context = WorkflowContext::new(config, mux, None)?;

    // 2. Resolve all targets and validate they exist
    let mut candidates: Vec<(String, PathBuf, String)> = Vec::new();
    for name in resolved_names {
        let (worktree_path, branch_name) = match find_worktree(context.vcs.as_ref(), &name) {
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
        // Check uncommitted (blocking).
        //
        // `has_missing_admin_dir` has no VcsBackend equivalent, but it only
        // ever returns true for a linked git worktree whose `.git` pointer
        // file targets a missing admin dir (the broken-worktree case that
        // `workflow::fallback_worktree_path` detects above). It reads a
        // `.git` file that a jj workspace doesn't have, so it degrades to a
        // harmless `false` for jj rather than needing a backend guard.
        if path.exists()
            && !git::has_missing_admin_dir(&path)
            && context.vcs.get_status(&path, None)?.is_dirty
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

        // Check unmerged (promptable), only if we're deleting the branch
        if !keep_branch && let Some(base) = is_unmerged(context.vcs.as_ref(), &branch)? {
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
fn is_unmerged(vcs: &dyn VcsBackend, branch: &str) -> Result<Option<String>> {
    let main_branch = vcs
        .get_default_branch_in(None)
        .unwrap_or_else(|_| "main".to_string());

    let base = vcs
        .meta()
        .get_branch_base(branch, None)
        .unwrap_or_else(|| main_branch.clone());

    let base_commit = match vcs.get_merge_base_in(None, &base) {
        Ok(b) => b,
        Err(_) => {
            // If we can't determine base, try falling back to main
            match vcs.get_merge_base_in(None, &main_branch) {
                Ok(b) => b,
                Err(error) => {
                    return Err(error.context("Cannot establish whether the branch is merged"));
                }
            }
        }
    };

    // `get_unmerged_branches` (computing which local branches have commits
    // not reachable from `base_commit`) has no VcsBackend equivalent and no
    // jj analog in v1. This is a read-only, advisory pre-removal
    // confirmation (it only decides whether to show a "are you sure"
    // prompt) rather than a write, and the actual branch/bookmark deletion
    // goes through `VcsBackend::delete_branch_in` regardless of its result
    // — so, unlike Task 6's jj-unsafe *writes*, skipping it for jj degrades
    // to "no unmerged-commit warning" rather than silent data loss.
    if vcs.name() != "git" {
        return Ok(None);
    }

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
    context: &WorkflowContext,
    mode: &BulkRemovalMode,
    force: bool,
    keep_branch: bool,
) -> Result<BulkRemovalPlan> {
    let vcs = context.vcs.as_ref();
    let worktrees = vcs.list_workspaces_in(None)?;
    let main_branch = vcs.get_default_branch_in(None)?;
    let main_worktree_root = vcs.get_main_worktree_root_in(None)?;

    let mut plan = BulkRemovalPlan {
        to_remove: Vec::new(),
        skipped: Vec::new(),
    };

    for entry in worktrees {
        let path = entry.path;
        let branch = entry
            .branch_or_bookmark
            .unwrap_or_else(|| "(detached)".to_string());

        if branch == main_branch || branch == "(detached)" {
            continue;
        }

        if path == main_worktree_root {
            continue;
        }

        if !mode.should_consider_branch(&branch) {
            continue;
        }

        if !force && path.exists() && vcs.get_status(&path, None)?.is_dirty {
            plan.skipped.push(BulkSkippedWorktree {
                branch,
                reason: BulkSkipReason::Uncommitted,
            });
            continue;
        }

        // `get_unmerged_branches` has no VcsBackend equivalent / jj analog
        // (see `is_unmerged`'s doc comment for the full reasoning): skip the
        // unmerged-commit skip-check for non-git backends rather than
        // computing a base/merge-base that would go unused.
        if mode.allow_unmerged_skip() && !force && !keep_branch && vcs.name() == "git" {
            let base = vcs
                .meta()
                .get_branch_base(&branch, None)
                .unwrap_or_else(|| main_branch.clone());
            let merge_base = vcs
                .get_merge_base_in(None, &base)
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
    context: &WorkflowContext,
    mode: BulkRemovalMode,
    force: bool,
    keep_branch: bool,
) -> Result<()> {
    let plan = collect_bulk_removal_plan(context, &mode, force, keep_branch)?;

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
fn run_all(force: bool, keep_branch: bool) -> Result<()> {
    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let context = WorkflowContext::new(config, mux, None)?;
    run_bulk_removal(&context, BulkRemovalMode::All, force, keep_branch)
}

/// Remove worktrees whose upstream remote branch has been deleted
fn run_gone(force: bool, keep_branch: bool) -> Result<()> {
    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let context = WorkflowContext::new(config, mux, None)?;

    // Fetch with prune to update remote-tracking refs. No jj analog in v1:
    // `fetch_prune_in`/`get_gone_branches_in` no-op (empty/success) for
    // non-git backends, so bulk "--gone" removal degrades to "nothing is
    // gone" for jj rather than erroring.
    spinner::with_spinner("Fetching from remote", || context.vcs.fetch_prune_in(None))?;
    let gone_branches: std::collections::HashSet<String> = context
        .vcs
        .get_gone_branches_in(&context.git_common_dir)
        .unwrap_or_default()
        .into_iter()
        .collect();
    run_bulk_removal(
        &context,
        BulkRemovalMode::Gone(gone_branches),
        force,
        keep_branch,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use crate::vcs::GitBackend;
    use std::path::PathBuf;

    /// Characterization test: the new `VcsBackend`-generic `find_worktree`
    /// helper must resolve a git worktree by handle and by branch name
    /// exactly like the pre-migration `git::find_worktree_in` free function
    /// it replaces at this call site.
    #[test]
    fn find_worktree_matches_git_free_function_by_handle_and_branch() {
        const TEST_NAME: &str =
            "command::remove::tests::find_worktree_matches_git_free_function_by_handle_and_branch";
        if !test_support::is_isolated_child(TEST_NAME) {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            test_support::init_repo(&repo);

            let worktree_path = temp.path().join("feature-handle");
            test_support::run_git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feature",
                    worktree_path.to_str().unwrap(),
                ],
            );

            test_support::run_isolated_test(TEST_NAME, &repo, &[("WM_TEST_TEMP", temp.path())]);
            return;
        }

        println!("{}", test_support::ISOLATED_TEST_CANARY);
        let temp = std::env::var_os("WM_TEST_TEMP").map(PathBuf::from).unwrap();
        let worktree_path = temp.join("feature-handle");

        let backend = GitBackend;

        let expected_by_handle = git::find_worktree_in("feature-handle", None).unwrap();
        let actual_by_handle = find_worktree(&backend, "feature-handle").unwrap();
        assert_eq!(actual_by_handle, expected_by_handle);
        assert_eq!(actual_by_handle.0, worktree_path.canonicalize().unwrap());
        assert_eq!(actual_by_handle.1, "feature");

        let expected_by_branch = git::find_worktree_in("feature", None).unwrap();
        let actual_by_branch = find_worktree(&backend, "feature").unwrap();
        assert_eq!(actual_by_branch, expected_by_branch);

        let err = find_worktree(&backend, "does-not-exist").unwrap_err();
        assert!(err.to_string().contains("does-not-exist"));
    }

    /// Characterization test: `is_unmerged` must classify an unmerged
    /// feature branch and a fully-merged branch the same way the
    /// pre-migration code (bare `git::get_branch_base` /
    /// `git::get_merge_base` / `git::get_unmerged_branches` calls) did.
    #[test]
    fn is_unmerged_matches_git_semantics_for_merged_and_unmerged_branches() {
        const TEST_NAME: &str =
            "command::remove::tests::is_unmerged_matches_git_semantics_for_merged_and_unmerged_branches";
        if !test_support::is_isolated_child(TEST_NAME) {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            test_support::init_repo(&repo);

            // "merged-branch" points at the same commit as main: no
            // unmerged commits.
            test_support::run_git(&repo, &["branch", "merged-branch"]);

            // "feature" has a commit on top of main that main doesn't have.
            test_support::run_git(&repo, &["checkout", "-b", "feature"]);
            std::fs::write(repo.join("feature.txt"), "feature work\n").unwrap();
            test_support::run_git(&repo, &["add", "feature.txt"]);
            test_support::run_git(&repo, &["commit", "-m", "feature work"]);
            test_support::run_git(&repo, &["checkout", "main"]);

            test_support::run_isolated_test(TEST_NAME, &repo, &[]);
            return;
        }

        println!("{}", test_support::ISOLATED_TEST_CANARY);

        let backend = GitBackend;

        assert_eq!(
            is_unmerged(&backend, "merged-branch").unwrap(),
            None,
            "a branch with no commits ahead of its base must not be reported as unmerged"
        );

        let unmerged = is_unmerged(&backend, "feature").unwrap();
        assert_eq!(
            unmerged,
            Some("main".to_string()),
            "a branch with commits not reachable from its base must be reported unmerged, \
             with the base branch it was compared against"
        );
    }
}
