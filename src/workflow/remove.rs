use anyhow::{Result, anyhow};
use std::io::{self, Write};
use std::path::PathBuf;

use crate::git;
use crate::sandbox;
use tracing::{debug, info};

use super::cleanup;
use super::context::WorkflowContext;
use super::types::RemoveResult;

pub fn fallback_worktree_path(handle: &str, context: &WorkflowContext) -> Result<Option<PathBuf>> {
    let base_dir = if let Some(ref worktree_dir) = context.config.worktree_dir {
        crate::util::expand_worktree_dir(worktree_dir, &context.main_worktree_root)?
    } else {
        let project_name = context
            .main_worktree_root
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("Could not determine project name"))?;
        context
            .main_worktree_root
            .parent()
            .ok_or_else(|| anyhow!("Could not determine parent directory"))?
            .join(format!("{}__worktrees", project_name))
    };

    let path = base_dir.join(handle);
    let Some(admin_dir) = git::linked_worktree_admin_dir(&path) else {
        return Ok(None);
    };
    let expected_parent = context.git_common_dir.join("worktrees");
    Ok((!admin_dir.is_dir() && admin_dir.starts_with(expected_parent)).then_some(path))
}

/// Remove a worktree without merging
pub fn remove(
    handle: &str,
    force: bool,
    keep_branch: bool,
    context: &WorkflowContext,
) -> Result<RemoveResult> {
    remove_with_hook_output(handle, force, keep_branch, context, true)
}

/// Remove a worktree while keeping hook output away from the terminal.
pub fn remove_quiet(
    handle: &str,
    force: bool,
    keep_branch: bool,
    context: &WorkflowContext,
) -> Result<RemoveResult> {
    remove_with_hook_output(handle, force, keep_branch, context, false)
}

fn remove_with_hook_output(
    handle: &str,
    force: bool,
    keep_branch: bool,
    context: &WorkflowContext,
    show_hook_output: bool,
) -> Result<RemoveResult> {
    info!(handle = handle, force, keep_branch, "remove:start");

    // Get worktree path and branch - this also validates that the worktree exists
    // Smart resolution: try handle first, then branch name
    let workdir = Some(context.execution_dir.as_path());
    let (worktree_path, branch_name) = match git::find_worktree_in(handle, workdir) {
        Ok(worktree) => worktree,
        Err(e) => {
            if let Some(path) = fallback_worktree_path(handle, context)? {
                (path, String::new())
            } else {
                return Err(anyhow!(
                    "Worktree '{}' not found. Use 'workmux list' to see available worktrees.",
                    handle
                )
                .context(e));
            }
        }
    };

    // Extract actual handle from worktree path (directory name)
    // User may have provided branch name (with slashes) but window names use handle (with dashes)
    let actual_handle = worktree_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            anyhow!(
                "Could not derive handle from worktree path: {}",
                worktree_path.display()
            )
        })?;

    debug!(handle = actual_handle, branch = branch_name, path = %worktree_path.display(), "remove:worktree resolved");

    // Capture metadata before cleanup removes it.
    let mode = git::get_worktree_mode_opt_in(actual_handle, workdir)
        .unwrap_or(crate::config::MuxMode::Window);
    let attachment = git::get_worktree_attachment_in(actual_handle, workdir);

    // Safety Check: Prevent deleting the main worktree itself, regardless of branch.
    if context.is_main_worktree(&worktree_path) {
        return Err(anyhow!(
            "Cannot remove branch '{}' because it is checked out in the main worktree at '{}'. \
            Switch the main worktree to a different branch first, or create a linked worktree for '{}'.",
            branch_name,
            context.main_worktree_root.display(),
            branch_name
        ));
    }

    if branch_name.is_empty() && !keep_branch {
        return Err(anyhow!(
            "Worktree '{}' has broken Git metadata, so its branch cannot be determined. \
            Use --keep-branch to remove only the worktree directory.",
            actual_handle
        ));
    }

    // Safety Check: Prevent deleting the main branch by name (secondary check)
    if branch_name == context.main_branch {
        return Err(anyhow!(
            "Cannot delete the main branch ('{}')",
            context.main_branch
        ));
    }

    if worktree_path.exists()
        && !git::has_missing_admin_dir(&worktree_path)
        && git::has_uncommitted_changes(&worktree_path)?
        && !force
    {
        return Err(anyhow!(
            "Worktree has uncommitted changes. Use --force to delete anyway."
        ));
    }

    // Note: Unmerged branch check removed - git branch -d/D handles this natively
    // The CLI provides a user-friendly confirmation prompt before calling this function

    // Stop any running containers for this worktree before killing the window.
    // This is necessary because tmux kill-window sends SIGHUP which doesn't allow
    // the supervisor's Drop handler to run. We try unconditionally since sandbox
    // may have been enabled via --sandbox flag even if disabled in config.
    sandbox::stop_containers_for_handle(actual_handle);

    info!(branch = %branch_name, keep_branch, "remove:cleanup start");
    let cleanup_result = cleanup::cleanup(
        context,
        &branch_name,
        actual_handle,
        &worktree_path,
        cleanup::CleanupOptions {
            force,
            keep_branch,
            no_hooks: false,
            show_hook_output,
        },
    )?;

    if attachment.manages_mux() {
        cleanup::navigate_to_target_and_close(
            context.mux.as_ref(),
            &context.prefix,
            &context.main_branch,
            actual_handle,
            &cleanup_result,
            mode,
            context.config.default_session(),
        )?;
    }

    let cleanup_scheduled = cleanup_result.deferred_cleanup.is_some();
    if cleanup_scheduled && show_hook_output {
        if keep_branch {
            println!(
                "✓ Scheduled removal of worktree '{}' (branch '{}' will be kept)",
                actual_handle, branch_name
            );
        } else {
            println!(
                "✓ Scheduled removal of worktree '{}' and branch '{}'",
                actual_handle, branch_name
            );
        }
        io::stdout().flush()?;
    }

    Ok(RemoveResult {
        branch_removed: branch_name.to_string(),
        cleanup_scheduled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::multiplexer::{BackendType, create_backend};
    use crate::test_support;

    #[test]
    fn remove_uses_context_repository_not_process_cwd() {
        const TEST_NAME: &str =
            "workflow::remove::tests::remove_uses_context_repository_not_process_cwd";
        if !test_support::is_isolated_child(TEST_NAME) {
            let temp = tempfile::tempdir().unwrap();
            let repo_a = temp.path().join("repo-a");
            let repo_b = temp.path().join("repo-b");
            let worktree_a = temp.path().join("worktree-a");
            let worktree_b = temp.path().join("worktree-b");
            std::fs::create_dir_all(&repo_a).unwrap();
            std::fs::create_dir_all(&repo_b).unwrap();
            test_support::init_repo(&repo_a);
            test_support::init_repo(&repo_b);

            for (repo, worktree) in [(&repo_a, &worktree_a), (&repo_b, &worktree_b)] {
                test_support::run_git(
                    repo,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        "shared",
                        worktree.to_str().unwrap(),
                    ],
                );
                test_support::run_git(
                    repo,
                    &[
                        "config",
                        "--local",
                        "workmux.worktree.shared.attachment",
                        "headless",
                    ],
                );
            }

            test_support::run_isolated_test(
                TEST_NAME,
                &repo_a,
                &[
                    ("WM_TEST_TEMP", temp.path()),
                    ("XDG_STATE_HOME", temp.path()),
                ],
            );
            return;
        }

        println!("{}", test_support::ISOLATED_TEST_CANARY);
        let temp = std::env::var_os("WM_TEST_TEMP").map(PathBuf::from).unwrap();
        let repo_a = temp.join("repo-a");
        let repo_b = temp.join("repo-b");
        let ctx = WorkflowContext::new_in(
            &repo_b,
            Config::default(),
            create_backend(BackendType::Tmux),
            None,
        )
        .unwrap();

        remove("shared", true, false, &ctx).unwrap();

        assert!(temp.join("worktree-a").exists());
        assert!(!temp.join("worktree-b").exists());
        assert!(git::branch_exists_in("shared", Some(&repo_a)).unwrap());
        assert!(!git::branch_exists_in("shared", Some(&repo_b)).unwrap());
    }
}
