use crate::multiplexer::{create_backend, detect_backend};
use crate::workflow::WorkflowContext;
use crate::{config, spinner, vcs, workflow};
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

    let vcs = vcs::detect_vcs()?;

    // 2. Resolve all targets and validate they exist
    let mut candidates: Vec<(String, PathBuf, String)> = Vec::new();
    for name in resolved_names {
        let (worktree_path, branch_name) = vcs.find_workspace(&name)
            .with_context(|| format!("No workspace found with name '{}'", name))?;

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
                failed.push((handle, e.to_string()));
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
        if path.exists() && vcs.has_uncommitted_changes(&path).unwrap_or(false) {
            uncommitted.push(handle);
            continue;
        }

        // Check unmerged (promptable), only if we're deleting the branch
        if !keep_branch && let Some(base) = is_unmerged(vcs.as_ref(), &branch)? {
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
fn is_unmerged(vcs: &dyn vcs::Vcs, branch: &str) -> Result<Option<String>> {
    let main_branch = vcs.get_default_branch().unwrap_or_else(|_| "main".to_string());

    let base = vcs.get_branch_base(branch)
        .ok()
        .unwrap_or_else(|| main_branch.clone());

    let base_commit = match vcs.get_merge_base(&base) {
        Ok(b) => b,
        Err(_) => {
            // If we can't determine base, try falling back to main
            match vcs.get_merge_base(&main_branch) {
                Ok(b) => b,
                Err(_) => return Ok(None), // Can't determine, assume safe
            }
        }
    };

    let unmerged_branches = vcs.get_unmerged_branches(&base_commit)?;
    if unmerged_branches.contains(branch) {
        Ok(Some(base))
    } else {
        Ok(None)
    }
}

/// Remove all managed worktrees (except main)
fn run_all(force: bool, keep_branch: bool) -> Result<()> {
    let vcs = vcs::detect_vcs()?;
    let worktrees = vcs.list_workspaces()?;
    let main_branch = vcs.get_default_branch()?;
    let main_worktree_root = vcs.get_main_workspace_root()?;

    let mut to_remove: Vec<(PathBuf, String, String)> = Vec::new();
    let mut skipped_uncommitted: Vec<String> = Vec::new();
    let mut skipped_unmerged: Vec<String> = Vec::new();

    for (path, branch) in worktrees {
        // Skip main branch/worktree and detached HEAD
        if branch == main_branch || branch == "(detached)" {
            continue;
        }

        // Skip the main worktree itself (safety check)
        if path == main_worktree_root {
            continue;
        }

        // Check for uncommitted changes
        if !force && path.exists() && vcs.has_uncommitted_changes(&path).unwrap_or(false) {
            skipped_uncommitted.push(branch);
            continue;
        }

        // Check for unmerged commits (only when deleting the branch)
        if !force && !keep_branch {
            let base = vcs.get_branch_base(&branch)
                .ok()
                .unwrap_or_else(|| main_branch.clone());
            if let Ok(merge_base) = vcs.get_merge_base(&base)
                && let Ok(unmerged_branches) = vcs.get_unmerged_branches(&merge_base)
                && unmerged_branches.contains(&branch)
            {
                skipped_unmerged.push(branch);
                continue;
            }
        }

        let handle = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&branch)
            .to_string();

        to_remove.push((path, branch, handle));
    }

    if to_remove.is_empty() && skipped_uncommitted.is_empty() && skipped_unmerged.is_empty() {
        println!("No worktrees to remove.");
        return Ok(());
    }

    if to_remove.is_empty() {
        println!("No removable worktrees found.");
        if !skipped_uncommitted.is_empty() {
            println!(
                "\nSkipped {} worktree(s) with uncommitted changes:",
                skipped_uncommitted.len()
            );
            for branch in &skipped_uncommitted {
                println!("  - {}", branch);
            }
        }
        if !skipped_unmerged.is_empty() {
            println!(
                "\nSkipped {} worktree(s) with unmerged commits:",
                skipped_unmerged.len()
            );
            for branch in &skipped_unmerged {
                println!("  - {}", branch);
            }
        }
        println!("\nUse --force to remove these anyway.");
        return Ok(());
    }

    // Show what will be removed
    println!("The following worktrees will be removed:");
    for (_, branch, _) in &to_remove {
        println!("  - {}", branch);
    }

    if !skipped_uncommitted.is_empty() {
        println!(
            "\nSkipping {} worktree(s) with uncommitted changes:",
            skipped_uncommitted.len()
        );
        for branch in &skipped_uncommitted {
            println!("  - {}", branch);
        }
    }

    if !skipped_unmerged.is_empty() {
        println!(
            "\nSkipping {} worktree(s) with unmerged commits:",
            skipped_unmerged.len()
        );
        for branch in &skipped_unmerged {
            println!("  - {}", branch);
        }
    }

    // Confirm with user unless --force
    if !force {
        print!(
            "\nAre you sure you want to remove ALL {} worktree(s)? [y/N] ",
            to_remove.len()
        );
        io::stdout().flush().context("Failed to flush stdout")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read user input")?;

        if input.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Execute removal
    let mut success_count = 0;
    let mut failed: Vec<(String, String)> = Vec::new();

    for (_, branch, handle) in to_remove {
        match remove_worktree(&handle, true, keep_branch) {
            Ok(()) => success_count += 1,
            Err(e) => failed.push((branch, e.to_string())),
        }
    }

    // Report results
    if success_count > 0 {
        println!("\n✓ Successfully removed {} worktree(s)", success_count);
    }

    if !failed.is_empty() {
        eprintln!("\nFailed to remove {} worktree(s):", failed.len());
        for (branch, error) in &failed {
            eprintln!("  - {}: {}", branch, error);
        }
    }

    Ok(())
}

/// Remove worktrees whose upstream remote branch has been deleted
fn run_gone(force: bool, keep_branch: bool) -> Result<()> {
    let vcs = vcs::detect_vcs()?;

    // Fetch with prune to update remote-tracking refs
    let vcs_clone = vcs.clone();
    spinner::with_spinner("Fetching from remote", move || vcs_clone.fetch_prune())?;

    let worktrees = vcs.list_workspaces()?;
    let main_branch = vcs.get_default_branch()?;
    let main_worktree_root = vcs.get_main_workspace_root()?;

    let gone_branches = vcs.get_gone_branches().unwrap_or_default();

    // Find worktrees whose upstream is gone
    let mut to_remove: Vec<(PathBuf, String, String)> = Vec::new();
    let mut skipped_uncommitted: Vec<String> = Vec::new();

    for (path, branch) in worktrees {
        // Skip main branch/worktree and detached HEAD
        if branch == main_branch || branch == "(detached)" {
            continue;
        }

        // Skip the main worktree itself
        if path == main_worktree_root {
            continue;
        }

        // Check if upstream is gone
        if !gone_branches.contains(&branch) {
            continue;
        }

        // Check for uncommitted changes
        if !force && path.exists() && vcs.has_uncommitted_changes(&path).unwrap_or(false) {
            skipped_uncommitted.push(branch);
            continue;
        }

        let handle = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&branch)
            .to_string();

        to_remove.push((path, branch, handle));
    }

    if to_remove.is_empty() && skipped_uncommitted.is_empty() {
        println!("No worktrees with gone upstreams found.");
        return Ok(());
    }

    if to_remove.is_empty() {
        println!("No worktrees to remove.");
        if !skipped_uncommitted.is_empty() {
            println!(
                "\nSkipped {} worktree(s) with uncommitted changes:",
                skipped_uncommitted.len()
            );
            for branch in &skipped_uncommitted {
                println!("  - {}", branch);
            }
            println!("\nUse --force to remove these anyway.");
        }
        return Ok(());
    }

    // Show what will be removed
    println!("The following worktrees have gone upstreams and will be removed:");
    for (_, branch, _) in &to_remove {
        println!("  - {}", branch);
    }

    if !skipped_uncommitted.is_empty() {
        println!(
            "\nSkipping {} worktree(s) with uncommitted changes:",
            skipped_uncommitted.len()
        );
        for branch in &skipped_uncommitted {
            println!("  - {}", branch);
        }
    }

    // Confirm with user unless --force
    if !force {
        print!(
            "\nAre you sure you want to remove {} worktree(s)? [y/N] ",
            to_remove.len()
        );
        io::stdout().flush().context("Failed to flush stdout")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read user input")?;

        if input.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Execute removal
    let mut success_count = 0;
    let mut failed: Vec<(String, String)> = Vec::new();

    for (_, branch, handle) in to_remove {
        match remove_worktree(&handle, true, keep_branch) {
            Ok(()) => success_count += 1,
            Err(e) => failed.push((branch, e.to_string())),
        }
    }

    // Report results
    if success_count > 0 {
        println!("\n✓ Successfully removed {} worktree(s)", success_count);
    }

    if !failed.is_empty() {
        eprintln!("\nFailed to remove {} worktree(s):", failed.len());
        for (branch, error) in &failed {
            eprintln!("  - {}: {}", branch, error);
        }
    }

    Ok(())
}

/// Execute the actual worktree removal
fn remove_worktree(handle: &str, force: bool, keep_branch: bool) -> Result<()> {
    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let context = WorkflowContext::new(config, mux, None)?;

    super::announce_hooks(&context.config, None, super::HookPhase::PreRemove);

    let result = workflow::remove(handle, force, keep_branch, &context)
        .context("Failed to remove worktree")?;

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

    Ok(())
}
