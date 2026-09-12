use anyhow::{Context, Result, bail};

use crate::workflow::file_ops::{handle_file_operations, symlink_claude_local_md};
use crate::{config, git};

pub fn run(all: bool) -> Result<()> {
    let repo_root =
        git::get_main_worktree_root().context("Could not find the main git worktree")?;

    // Discover config nesting from CWD (e.g., "backend/" for monorepo configs).
    // We only need the rel_dir, not the config content, since the worktree's
    // checked-out branch may have an outdated .workmux.yaml.
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let cwd_rel_dir = config::find_project_config(&cwd)?
        .map(|loc| loc.rel_dir)
        .unwrap_or_default();

    // Load the actual config from the main worktree at the equivalent path.
    let main_config_start = if cwd_rel_dir.as_os_str().is_empty() {
        repo_root.clone()
    } else {
        repo_root.join(&cwd_rel_dir)
    };
    let (config, config_location) =
        config::Config::load_with_location_from(&main_config_start, None)?;

    // Source for file operations is always the main worktree.
    // For monorepo configs, use the equivalent subdirectory within the main worktree.
    let rel_dir = config_location
        .as_ref()
        .map(|loc| loc.rel_dir.clone())
        .unwrap_or_default();
    let file_ops_source = if rel_dir.as_os_str().is_empty() {
        repo_root.clone()
    } else {
        repo_root.join(&rel_dir)
    };

    let targets = if all {
        // Sync all worktrees (excluding main)
        let worktrees = git::list_worktrees().context("Failed to list worktrees")?;
        let mut paths = Vec::new();
        for (path, _branch) in worktrees {
            if path == repo_root {
                continue;
            }
            // For monorepo, target the equivalent subdirectory in each worktree
            let target = if rel_dir.as_os_str().is_empty() {
                path
            } else {
                path.join(&rel_dir)
            };
            paths.push(target);
        }
        if paths.is_empty() {
            bail!("No worktrees found (besides main)");
        }
        paths
    } else {
        // Resolve the worktree root (not CWD, which could be a subdirectory)
        let worktree_root = crate::vcs::detect::detect_backend_in(&cwd)
            .and_then(|backend| backend.get_repo_root_in(None))
            .context("Failed to determine current worktree root")?;

        if worktree_root == repo_root {
            bail!(
                "Current directory is the main worktree. \
                 Run this from inside a worktree, or use --all."
            );
        }

        // For monorepo, target the equivalent subdirectory
        let target = if rel_dir.as_os_str().is_empty() {
            worktree_root
        } else {
            worktree_root.join(&rel_dir)
        };

        vec![target]
    };

    for target in &targets {
        handle_file_operations(&file_ops_source, target, &config.files)
            .with_context(|| format!("Failed to sync files to {}", target.display()))?;
        symlink_claude_local_md(&repo_root, target)
            .with_context(|| format!("Failed to sync CLAUDE.local.md to {}", target.display()))?;

        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| target.display().to_string());
        println!("✓ Synced files to '{}'", name);
    }

    Ok(())
}
