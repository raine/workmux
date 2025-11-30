use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Path, PathBuf};

use crate::{cmd, config, git, prompt::Prompt, tmux};
use tracing::{debug, info};

use fs_extra::dir as fs_dir;
use fs_extra::file as fs_file;

use super::types::CreateResult;

/// Sets up the tmux window, files, and hooks for a worktree.
/// This is the shared logic between `create` and `open`.
///
/// # Arguments
/// * `branch_name` - The git branch name (for logging/reference)
/// * `handle` - The display name used for tmux window naming
/// * `worktree_path` - Path to the worktree directory
/// * `config` - Configuration settings
/// * `options` - Setup options (hooks, file ops, etc.)
/// * `agent` - Optional agent override
pub fn setup_environment(
    branch_name: &str,
    handle: &str,
    worktree_path: &Path,
    config: &config::Config,
    options: &super::types::SetupOptions,
    agent: Option<&str>,
) -> Result<CreateResult> {
    debug!(
        branch = branch_name,
        handle = handle,
        path = %worktree_path.display(),
        run_hooks = options.run_hooks,
        run_file_ops = options.run_file_ops,
        "setup_environment:start"
    );
    let prefix = config.window_prefix();
    let repo_root = git::get_repo_root()?;

    // Perform file operations (copy and symlink) if requested
    if options.run_file_ops {
        handle_file_operations(&repo_root, worktree_path, &config.files)
            .context("Failed to perform file operations")?;
        debug!(
            branch = branch_name,
            "setup_environment:file operations applied"
        );
    }

    // Run post-create hooks before opening tmux so the new window appears "ready"
    let mut hooks_run = 0;
    if options.run_hooks
        && let Some(post_create) = &config.post_create
        && !post_create.is_empty()
    {
        hooks_run = post_create.len();
        let hook_env = [("WORKMUX_HANDLE", handle)];
        for (idx, command) in post_create.iter().enumerate() {
            info!(branch = branch_name, step = idx + 1, total = hooks_run, command = %command, "setup_environment:hook start");
            info!(command = %command, "Running post-create hook {}/{}", idx + 1, hooks_run);
            cmd::shell_command_with_env(command, worktree_path, &hook_env)
                .with_context(|| format!("Failed to run post-create command: '{}'", command))?;
            info!(branch = branch_name, step = idx + 1, total = hooks_run, command = %command, "setup_environment:hook complete");
        }
        info!(
            branch = branch_name,
            total = hooks_run,
            "setup_environment:hooks complete"
        );
    }

    // Create tmux window and get the initial pane's ID
    // Use handle for the window name (not branch_name)
    let initial_pane_id = tmux::create_window(
        prefix,
        handle,
        worktree_path,
        /* detached: */ !options.focus_window,
    )
    .context("Failed to create tmux window")?;
    info!(
        branch = branch_name,
        handle = handle,
        pane_id = %initial_pane_id,
        "setup_environment:tmux window created"
    );

    // Setup panes
    let panes = config.panes.as_deref().unwrap_or(&[]);
    let resolved_panes = resolve_pane_configuration(panes, agent);
    let pane_setup_result = tmux::setup_panes(
        &initial_pane_id,
        &resolved_panes,
        worktree_path,
        tmux::PaneSetupOptions {
            run_commands: options.run_pane_commands,
            prompt_file_path: options.prompt_file_path.as_deref(),
        },
        config,
        agent,
    )
    .context("Failed to setup panes")?;
    debug!(
        branch = branch_name,
        focus_id = %pane_setup_result.focus_pane_id,
        "setup_environment:panes configured"
    );

    // Focus the configured pane and optionally switch to the window
    if options.focus_window {
        tmux::select_pane(&pane_setup_result.focus_pane_id)?;
        // Use handle for window selection (not branch_name)
        tmux::select_window(prefix, handle)?;
    } else {
        // Background mode: do not steal focus from the current window.
        // We intentionally skip select_window to keep the user's current window.
    }

    Ok(CreateResult {
        worktree_path: worktree_path.to_path_buf(),
        branch_name: branch_name.to_string(),
        post_create_hooks_run: hooks_run,
        base_branch: None,
    })
}

pub fn resolve_pane_configuration(
    original_panes: &[config::PaneConfig],
    agent: Option<&str>,
) -> Vec<config::PaneConfig> {
    let Some(agent_cmd) = agent else {
        return original_panes.to_vec();
    };

    if original_panes
        .iter()
        .any(|pane| pane.command.as_deref() == Some("<agent>"))
    {
        return original_panes.to_vec();
    }

    let mut panes = original_panes.to_vec();

    if let Some(focused) = panes.iter_mut().find(|pane| pane.focus) {
        focused.command = Some(agent_cmd.to_string());
        return panes;
    }

    if let Some(first) = panes.get_mut(0) {
        first.command = Some(agent_cmd.to_string());
        return panes;
    }

    vec![config::PaneConfig {
        command: Some(agent_cmd.to_string()),
        focus: true,
        split: None,
        size: None,
        percentage: None,
        target: None,
    }]
}

/// Performs copy and symlink operations from the repo root to the worktree
pub fn handle_file_operations(
    repo_root: &Path,
    worktree_path: &Path,
    file_config: &config::FileConfig,
) -> Result<()> {
    debug!(
        repo = %repo_root.display(),
        worktree = %worktree_path.display(),
        copy_patterns = file_config.copy.as_ref().map(|v| v.len()).unwrap_or(0),
        symlink_patterns = file_config.symlink.as_ref().map(|v| v.len()).unwrap_or(0),
        "file_operations:start"
    );

    let canon_repo_root = repo_root.canonicalize().with_context(|| {
        format!(
            "Failed to canonicalize repository root path: {:?}",
            repo_root
        )
    })?;

    let mut copy_count = 0;
    let mut symlink_count = 0;

    // Handle copies
    if let Some(copy_patterns) = &file_config.copy {
        for pattern in copy_patterns {
            let full_pattern = repo_root.join(pattern).to_string_lossy().to_string();
            for entry in glob::glob(&full_pattern)? {
                let source_path = entry?;

                // Validate that the resolved source path stays within the repository root
                let canon_source_path = source_path.canonicalize().with_context(|| {
                    format!("Failed to canonicalize source path: {:?}", source_path)
                })?;
                if !canon_source_path.starts_with(&canon_repo_root) {
                    return Err(anyhow!(
                        "Path traversal detected for copy pattern '{}'. The resolved path '{}' is outside the repository root.",
                        pattern,
                        source_path.display()
                    ));
                }

                let relative_path = source_path.strip_prefix(repo_root).with_context(|| {
                    format!(
                        "Path '{}' is outside the repository root '{}', which is not allowed.",
                        source_path.display(),
                        repo_root.display()
                    )
                })?;
                let dest_path = worktree_path.join(relative_path);

                if source_path.is_dir() {
                    // Create destination parent directory
                    if let Some(parent) = dest_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    // Use fs_extra::dir::copy which handles recursion and symlinks correctly
                    let mut dir_options = fs_dir::CopyOptions::new();
                    dir_options.overwrite = true;
                    dir_options.content_only = true;
                    fs::create_dir_all(&dest_path)?; // Ensure dest exists
                    fs_dir::copy(&source_path, &dest_path, &dir_options).with_context(|| {
                        format!(
                            "Failed to copy directory {:?} to {:?}",
                            source_path, dest_path
                        )
                    })?;
                } else {
                    // Copy single file
                    if let Some(parent) = dest_path.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!("Failed to create parent directory for {:?}", dest_path)
                        })?;
                    }
                    let mut options = fs_file::CopyOptions::new();
                    options.overwrite = true;
                    fs_file::copy(&source_path, &dest_path, &options).with_context(|| {
                        format!("Failed to copy file {:?} to {:?}", source_path, dest_path)
                    })?;
                }
                copy_count += 1;
            }
        }
    }

    // Handle symlinks
    if let Some(symlink_patterns) = &file_config.symlink {
        for pattern in symlink_patterns {
            let full_pattern = repo_root.join(pattern).to_string_lossy().to_string();
            for entry in glob::glob(&full_pattern)? {
                let source_path = entry?;

                // Validate that the resolved source path is within the repository root
                let canon_source_path = source_path.canonicalize().with_context(|| {
                    format!("Failed to canonicalize source path: {:?}", source_path)
                })?;
                if !canon_source_path.starts_with(&canon_repo_root) {
                    return Err(anyhow!(
                        "Path traversal detected for symlink pattern '{}'. The resolved path '{}' is outside the repository root.",
                        pattern,
                        source_path.display()
                    ));
                }

                let relative_path = source_path.strip_prefix(repo_root)?;
                let dest_path = worktree_path.join(relative_path);

                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("Failed to create parent directory for {:?}", dest_path)
                    })?;
                }

                // Critical: create a relative path for the symlink
                let dest_parent = dest_path.parent().ok_or_else(|| {
                    anyhow!(
                        "Could not determine parent directory for destination path: {:?}",
                        dest_path
                    )
                })?;

                let relative_source = pathdiff::diff_paths(&source_path, dest_parent)
                    .ok_or_else(|| anyhow!("Could not create relative path for symlink"))?;

                // Remove existing file/symlink at destination to avoid errors
                // IMPORTANT: Use symlink_metadata to avoid following symlinks
                if let Ok(metadata) = dest_path.symlink_metadata() {
                    if metadata.is_dir() {
                        fs::remove_dir_all(&dest_path).with_context(|| {
                            format!("Failed to remove existing directory at {:?}", &dest_path)
                        })?;
                    } else {
                        // Handles both files and symlinks
                        fs::remove_file(&dest_path).with_context(|| {
                            format!("Failed to remove existing file/symlink at {:?}", &dest_path)
                        })?;
                    }
                }

                #[cfg(unix)]
                std::os::unix::fs::symlink(&relative_source, &dest_path).with_context(|| {
                    format!(
                        "Failed to create symlink from {:?} to {:?}",
                        relative_source, dest_path
                    )
                })?;

                #[cfg(windows)]
                {
                    if source_path.is_dir() {
                        std::os::windows::fs::symlink_dir(&relative_source, &dest_path)
                    } else {
                        std::os::windows::fs::symlink_file(&relative_source, &dest_path)
                    }
                    .with_context(|| {
                        format!(
                            "Failed to create symlink from {:?} to {:?}",
                            relative_source, dest_path
                        )
                    })?;
                }
                symlink_count += 1;
            }
        }
    }

    if copy_count > 0 || symlink_count > 0 {
        info!(
            copied = copy_count,
            symlinked = symlink_count,
            "file_operations:completed"
        );
    }

    Ok(())
}

pub fn write_prompt_file(branch_name: &str, prompt: &Prompt) -> Result<PathBuf> {
    let content = match prompt {
        Prompt::Inline(text) => text.clone(),
        Prompt::FromFile(path) => fs::read_to_string(path)
            .with_context(|| format!("Failed to read prompt file '{}'", path.display()))?,
    };

    // Write to temp directory instead of the worktree to avoid polluting git status
    let prompt_filename = format!("workmux-prompt-{}.md", branch_name);
    let prompt_path = std::env::temp_dir().join(prompt_filename);
    fs::write(&prompt_path, content)
        .with_context(|| format!("Failed to write prompt file '{}'", prompt_path.display()))?;
    Ok(prompt_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_pane_configuration_no_agent_returns_original() {
        let original_panes = vec![config::PaneConfig {
            command: Some("vim".to_string()),
            focus: true,
            split: None,
            size: None,
            percentage: None,
            target: None,
        }];

        let result = resolve_pane_configuration(&original_panes, None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].command, Some("vim".to_string()));
    }

    #[test]
    fn resolve_pane_configuration_agent_placeholder_returns_original() {
        let original_panes = vec![config::PaneConfig {
            command: Some("<agent>".to_string()),
            focus: true,
            split: None,
            size: None,
            percentage: None,
            target: None,
        }];

        let result = resolve_pane_configuration(&original_panes, Some("claude"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].command, Some("<agent>".to_string()));
    }

    #[test]
    fn resolve_pane_configuration_agent_sets_focused_pane() {
        let original_panes = vec![
            config::PaneConfig {
                command: Some("vim".to_string()),
                focus: false,
                split: None,
                size: None,
                percentage: None,
                target: None,
            },
            config::PaneConfig {
                command: Some("npm run dev".to_string()),
                focus: true,
                split: None,
                size: None,
                percentage: None,
                target: None,
            },
        ];

        let result = resolve_pane_configuration(&original_panes, Some("claude"));
        assert_eq!(result[0].command, Some("vim".to_string()));
        assert_eq!(result[1].command, Some("claude".to_string()));
    }

    #[test]
    fn resolve_pane_configuration_agent_sets_first_pane_when_no_focus() {
        let original_panes = vec![config::PaneConfig {
            command: Some("vim".to_string()),
            focus: false,
            split: None,
            size: None,
            percentage: None,
            target: None,
        }];

        let result = resolve_pane_configuration(&original_panes, Some("claude"));
        assert_eq!(result[0].command, Some("claude".to_string()));
    }

    #[test]
    fn resolve_pane_configuration_agent_creates_new_pane_when_empty() {
        let result = resolve_pane_configuration(&[], Some("claude"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].command, Some("claude".to_string()));
        assert!(result[0].focus);
    }
}
