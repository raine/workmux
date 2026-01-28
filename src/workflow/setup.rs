use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::TmuxTarget;
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
/// * `after_window` - Optional window ID to insert after (for grouping duplicates)
pub fn setup_environment(
    branch_name: &str,
    handle: &str,
    worktree_path: &Path,
    config: &config::Config,
    options: &super::types::SetupOptions,
    agent: Option<&str>,
    after_window: Option<String>,
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
    // Use main worktree root for file operations since source files live there
    let repo_root = git::get_main_worktree_root()?;

    // Determine effective working directory (config-relative or worktree root)
    let effective_working_dir = options.working_dir.as_deref().unwrap_or(worktree_path);

    // Determine source root for file operations
    let file_ops_source = options.config_root.as_deref().unwrap_or(&repo_root);

    // Perform file operations (copy and symlink) if requested
    if options.run_file_ops {
        handle_file_operations(file_ops_source, effective_working_dir, &config.files)
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
        // Resolve absolute paths for environment variables.
        // canonicalize() ensures symlinks are resolved and paths are absolute.
        let abs_worktree_path = worktree_path
            .canonicalize()
            .unwrap_or_else(|_| worktree_path.to_path_buf());
        let abs_project_root = repo_root
            .canonicalize()
            .unwrap_or_else(|_| repo_root.clone());
        let abs_config_dir = effective_working_dir
            .canonicalize()
            .unwrap_or_else(|_| effective_working_dir.to_path_buf());
        let worktree_path_str = abs_worktree_path.to_string_lossy();
        let project_root_str = abs_project_root.to_string_lossy();
        let config_dir_str = abs_config_dir.to_string_lossy();
        let hook_env = [
            ("WORKMUX_HANDLE", handle),
            ("WM_HANDLE", handle),
            ("WM_WORKTREE_PATH", worktree_path_str.as_ref()),
            ("WM_PROJECT_ROOT", project_root_str.as_ref()),
            ("WM_CONFIG_DIR", config_dir_str.as_ref()),
        ];
        for (idx, command) in post_create.iter().enumerate() {
            info!(branch = branch_name, step = idx + 1, total = hooks_run, command = %command, "setup_environment:hook start");
            info!(command = %command, "Running post-create hook {}/{}", idx + 1, hooks_run);
            cmd::shell_command_with_env(command, effective_working_dir, &hook_env)
                .with_context(|| format!("Failed to run post-create command: '{}'", command))?;
            info!(branch = branch_name, step = idx + 1, total = hooks_run, command = %command, "setup_environment:hook complete");
        }
        info!(
            branch = branch_name,
            total = hooks_run,
            "setup_environment:hooks complete"
        );
    }

    // Create tmux window or session based on target mode
    let initial_pane_id = match options.target {
        TmuxTarget::Window => {
            // Find the last workmux-managed window to insert the new one after.
            // If after_window is provided (for duplicate windows), use that to group with base handle.
            // Otherwise, use prefix-based lookup to group workmux windows together.
            // If not found (or error), falls back to default append behavior.
            let last_wm_window =
                after_window.or_else(|| tmux::find_last_window_with_prefix(prefix).unwrap_or(None));

            // Create tmux window and get the initial pane's ID
            // Use handle for the window name (not branch_name)
            let pane_id = tmux::create_window(
                prefix,
                handle,
                effective_working_dir,
                /* detached: */ !options.focus_window,
                last_wm_window.as_deref(),
            )
            .context("Failed to create tmux window")?;
            info!(
                branch = branch_name,
                handle = handle,
                pane_id = %pane_id,
                "setup_environment:tmux window created"
            );
            pane_id
        }
        TmuxTarget::Session => {
            // Create tmux session and get the initial pane's ID
            let pane_id = tmux::create_session(
                prefix,
                handle,
                effective_working_dir,
                /* detached: */ !options.focus_window,
            )
            .context("Failed to create tmux session")?;
            info!(
                branch = branch_name,
                handle = handle,
                pane_id = %pane_id,
                "setup_environment:tmux session created"
            );
            pane_id
        }
    };

    // Setup panes
    let panes = config.panes.as_deref().unwrap_or(&[]);
    let resolved_panes = resolve_pane_configuration(panes, agent);

    // Validate that prompt will be consumed if one was provided
    if options.prompt_file_path.is_some() {
        validate_prompt_consumption(&resolved_panes, agent, config, options)?;
    }

    let pane_setup_result = tmux::setup_panes(
        &initial_pane_id,
        &resolved_panes,
        effective_working_dir,
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

    // Focus the configured pane and optionally switch to the window/session
    if options.focus_window {
        tmux::select_pane(&pane_setup_result.focus_pane_id)?;
        match options.target {
            TmuxTarget::Window => {
                // Use handle for window selection (not branch_name)
                tmux::select_window(prefix, handle)?;
            }
            TmuxTarget::Session => {
                // Switch to the new session
                tmux::switch_to_session(prefix, handle)?;
            }
        }
    } else {
        // Background mode: do not steal focus from the current window/session.
        // We intentionally skip select_window/switch_to_session to keep the user's current context.
    }

    Ok(CreateResult {
        worktree_path: worktree_path.to_path_buf(),
        branch_name: branch_name.to_string(),
        post_create_hooks_run: hooks_run,
        base_branch: None,
        did_switch: false,
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

    // Sanitize branch name: replace path separators with dashes to avoid
    // interpreting slashes as directory separators (e.g., "feature/foo" -> "feature-foo")
    let safe_branch_name = branch_name.replace(['/', '\\'], "-");

    // Write to temp directory instead of the worktree to avoid polluting git status
    let prompt_filename = format!("workmux-prompt-{}.md", safe_branch_name);
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

    // --- validate_prompt_consumption tests ---

    fn make_config_with_agent(agent: Option<&str>) -> config::Config {
        config::Config {
            agent: agent.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    fn make_options_with_prompt(run_pane_commands: bool) -> crate::workflow::types::SetupOptions {
        crate::workflow::types::SetupOptions {
            run_hooks: true,
            run_file_ops: true,
            run_pane_commands,
            prompt_file_path: Some(std::path::PathBuf::from("/tmp/prompt.md")),
            focus_window: true,
            working_dir: None,
            config_root: None,
            target: crate::config::TmuxTarget::default(),
        }
    }

    #[test]
    fn validate_prompt_errors_when_pane_commands_disabled() {
        let panes = vec![config::PaneConfig {
            command: Some("<agent>".to_string()),
            focus: true,
            split: None,
            size: None,
            percentage: None,
            target: None,
        }];
        let config = make_config_with_agent(Some("claude"));
        let options = make_options_with_prompt(false); // pane commands disabled

        let result = super::validate_prompt_consumption(&panes, None, &config, &options);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("pane commands are disabled")
        );
    }

    #[test]
    fn validate_prompt_errors_when_no_agent_configured() {
        let panes = vec![config::PaneConfig {
            command: Some("vim".to_string()),
            focus: true,
            split: None,
            size: None,
            percentage: None,
            target: None,
        }];
        let config = make_config_with_agent(None); // no agent
        let options = make_options_with_prompt(true);

        let result = super::validate_prompt_consumption(&panes, None, &config, &options);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no agent is configured")
        );
    }

    #[test]
    fn validate_prompt_errors_when_no_pane_runs_agent() {
        let panes = vec![
            config::PaneConfig {
                command: None, // shell
                focus: true,
                split: None,
                size: None,
                percentage: None,
                target: None,
            },
            config::PaneConfig {
                command: Some("clear".to_string()),
                focus: false,
                split: Some(config::SplitDirection::Horizontal),
                size: None,
                percentage: None,
                target: None,
            },
        ];
        let config = make_config_with_agent(Some("claude"));
        let options = make_options_with_prompt(true);

        let result = super::validate_prompt_consumption(&panes, None, &config, &options);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("no pane is configured to run the agent"));
        assert!(err_msg.contains("claude"));
    }

    #[test]
    fn validate_prompt_succeeds_with_agent_placeholder() {
        let panes = vec![config::PaneConfig {
            command: Some("<agent>".to_string()),
            focus: true,
            split: None,
            size: None,
            percentage: None,
            target: None,
        }];
        let config = make_config_with_agent(Some("claude"));
        let options = make_options_with_prompt(true);

        let result = super::validate_prompt_consumption(&panes, None, &config, &options);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_prompt_succeeds_with_matching_agent_command() {
        let panes = vec![config::PaneConfig {
            command: Some("claude".to_string()),
            focus: true,
            split: None,
            size: None,
            percentage: None,
            target: None,
        }];
        let config = make_config_with_agent(Some("claude"));
        let options = make_options_with_prompt(true);

        let result = super::validate_prompt_consumption(&panes, None, &config, &options);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_prompt_cli_agent_overrides_config() {
        let panes = vec![config::PaneConfig {
            command: Some("gemini".to_string()),
            focus: true,
            split: None,
            size: None,
            percentage: None,
            target: None,
        }];
        let config = make_config_with_agent(Some("claude")); // config says claude
        let options = make_options_with_prompt(true);

        // CLI agent is gemini, which matches the pane
        let result = super::validate_prompt_consumption(&panes, Some("gemini"), &config, &options);
        assert!(result.is_ok());

        // CLI agent is None, falls back to config (claude), which doesn't match
        let result = super::validate_prompt_consumption(&panes, None, &config, &options);
        assert!(result.is_err());
    }

    #[test]
    fn validate_prompt_succeeds_when_any_pane_matches() {
        let panes = vec![
            config::PaneConfig {
                command: Some("vim".to_string()), // doesn't match
                focus: false,
                split: None,
                size: None,
                percentage: None,
                target: None,
            },
            config::PaneConfig {
                command: Some("claude --verbose".to_string()), // matches
                focus: true,
                split: Some(config::SplitDirection::Horizontal),
                size: None,
                percentage: None,
                target: None,
            },
        ];
        let config = make_config_with_agent(Some("claude"));
        let options = make_options_with_prompt(true);

        let result = super::validate_prompt_consumption(&panes, None, &config, &options);
        assert!(result.is_ok());
    }

    #[test]
    fn write_prompt_file_sanitizes_branch_with_slashes() {
        use crate::prompt::Prompt;

        let branch_name = "feature/nested/add-login";
        let prompt = Prompt::Inline("test prompt content".to_string());

        let path =
            super::write_prompt_file(branch_name, &prompt).expect("Should create prompt file");

        // Verify filename does not contain slashes
        let filename = path.file_name().unwrap().to_str().unwrap();
        assert!(
            filename.contains("feature-nested-add-login"),
            "Expected sanitized branch name in filename, got: {}",
            filename
        );
        assert!(
            !filename.contains('/'),
            "Filename should not contain slashes"
        );

        // Verify content was written correctly
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "test prompt content");

        // Cleanup
        let _ = std::fs::remove_file(path);
    }
}

/// Validates that a prompt will actually be consumed by an agent pane.
///
/// This prevents the case where a user provides `-p "some prompt"` but no pane
/// is configured to run an agent that would receive it.
fn validate_prompt_consumption(
    panes: &[config::PaneConfig],
    cli_agent: Option<&str>,
    config: &config::Config,
    options: &super::types::SetupOptions,
) -> Result<()> {
    if !options.run_pane_commands {
        return Err(anyhow!(
            "Prompt provided (-p/-P/-e) but pane commands are disabled (--no-pane-cmds). \
             The prompt would be ignored."
        ));
    }

    let effective_agent = cli_agent.or(config.agent.as_deref());

    let Some(agent_cmd) = effective_agent else {
        return Err(anyhow!(
            "Prompt provided but no agent is configured to consume it. \
             Set 'agent' in config or use -a/--agent flag."
        ));
    };

    let consumes_prompt = panes.iter().any(|pane| {
        pane.command
            .as_deref()
            .map(|cmd| config::is_agent_command(cmd, agent_cmd))
            .unwrap_or(false)
    });

    if !consumes_prompt {
        let commands: Vec<_> = panes
            .iter()
            .map(|p| p.command.as_deref().unwrap_or("<shell>"))
            .collect();

        return Err(anyhow!(
            "Prompt provided, but no pane is configured to run the agent '{}'.\n\
             Resolved pane commands: {:?}\n\
             Ensure your panes config includes '<agent>' or runs the configured agent.",
            agent_cmd,
            commands
        ));
    }

    Ok(())
}
