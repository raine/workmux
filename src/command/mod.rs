pub mod add;
pub mod args;
pub mod capture;
pub mod changelog;
pub mod clipboard_read;
pub mod close;
pub mod config;
pub mod dashboard;
pub mod docs;
pub mod exec;
pub mod host_exec;
pub mod last_agent;
pub mod last_done;
pub mod list;
pub mod merge;
pub mod open;
pub mod path;
pub mod remove;
pub mod rename;
pub mod resurrect;
pub mod run;
pub mod sandbox;
pub mod sandbox_run;
pub mod send;
pub mod set_base;
pub mod set_window_status;
pub mod setup;
pub mod sidebar;
pub mod status;
pub mod sync_files;
pub mod update;
pub mod wait;

use anyhow::{Context, Result, anyhow};

use crate::{config::Config, workflow::SetupOptions};

/// Represents the different phases where hooks can be executed
pub enum HookPhase {
    PostCreate,
    PreMerge,
    PreRemove,
}

/// Announce that hooks are about to run, if applicable.
/// Returns true if the announcement was printed (hooks will run).
pub fn announce_hooks(config: &Config, options: Option<&SetupOptions>, phase: HookPhase) -> bool {
    match phase {
        HookPhase::PostCreate => {
            let should_run = options.is_some_and(|opts| opts.run_hooks)
                && config.post_create.as_ref().is_some_and(|v| !v.is_empty());

            if should_run {
                println!("Running setup commands...");
            }
            should_run
        }
        HookPhase::PreMerge => {
            let should_run = config.pre_merge.as_ref().is_some_and(|v| !v.is_empty());

            if should_run {
                println!("Running pre-merge commands...");
            }
            should_run
        }
        HookPhase::PreRemove => {
            let should_run = config.pre_remove.as_ref().is_some_and(|v| !v.is_empty());

            if should_run {
                println!("Running pre-remove commands...");
            }
            should_run
        }
    }
}

/// Resolve name from argument or current worktree directory.
///
/// When no argument is provided, extracts the worktree name from the current directory.
/// If the user is in a subdirectory of a worktree, provides a helpful error message.
pub fn resolve_name(arg: Option<&str>) -> Result<String> {
    match arg {
        Some(name) => Ok(name.to_string()),
        None => {
            let cwd = std::env::current_dir().context("Failed to get current directory")?;
            resolve_name_from_path(&cwd)
        }
    }
}

/// Internal function to resolve worktree name from a path.
/// Separated for testability.
///
/// Uses `Path::components()` for cross-platform compatibility.
/// If the path is inside a worktree (even a subdirectory), extracts the worktree name.
///
/// Iterates in reverse to find the *closest* `__worktrees` parent, handling nested
/// structures correctly (e.g., `/backup__worktrees/project__worktrees/feature/src/`).
fn resolve_name_from_path(path: &std::path::Path) -> Result<String> {
    let mut iter = path.components().rev();
    let mut child_name: Option<String> = iter
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .map(|s| s.to_string());

    for component in iter {
        let name = component.as_os_str().to_str().unwrap_or("");

        // Found the container? Return the child we just visited.
        if name.ends_with("__worktrees") {
            return child_name
                .ok_or_else(|| anyhow!("Found worktree container but no child directory"));
        }

        // Track this component as the potential worktree name
        child_name = Some(name.to_string());
    }

    // Fallback to original behavior: use current directory name
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Could not determine worktree name from current directory"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_resolve_name_with_explicit_arg() {
        assert_eq!(resolve_name(Some("my-feature")).unwrap(), "my-feature");
    }

    #[test]
    fn test_resolve_name_from_worktree_root() {
        let path: PathBuf = ["/", "home", "user", "project__worktrees", "my-feature"]
            .iter()
            .collect();
        assert_eq!(resolve_name_from_path(&path).unwrap(), "my-feature");
    }

    #[test]
    fn test_resolve_name_from_subdirectory() {
        // Should extract worktree name even from a subdirectory
        let path: PathBuf = [
            "/",
            "home",
            "user",
            "project__worktrees",
            "my-feature",
            "src",
            "components",
        ]
        .iter()
        .collect();
        assert_eq!(resolve_name_from_path(&path).unwrap(), "my-feature");
    }

    #[test]
    fn test_resolve_name_from_non_worktree_directory() {
        let path: PathBuf = ["/", "home", "user", "some-project"].iter().collect();
        assert_eq!(resolve_name_from_path(&path).unwrap(), "some-project");
    }

    #[test]
    fn test_resolve_name_from_deeply_nested_subdirectory() {
        // Should extract worktree name even from deeply nested subdirectory
        let path: PathBuf = [
            "/",
            "home",
            "user",
            "project__worktrees",
            "fix-bug",
            "src",
            "lib",
            "utils",
            "helpers",
        ]
        .iter()
        .collect();
        assert_eq!(resolve_name_from_path(&path).unwrap(), "fix-bug");
    }

    #[test]
    fn test_resolve_name_with_nested_worktrees_dirs() {
        // Edge case: nested __worktrees directories (e.g., backup scenario)
        // Should use the *closest* __worktrees parent, not the first one found
        let path: PathBuf = [
            "/",
            "backup__worktrees",
            "project__worktrees",
            "feature",
            "src",
        ]
        .iter()
        .collect();
        assert_eq!(resolve_name_from_path(&path).unwrap(), "feature");
    }
}
