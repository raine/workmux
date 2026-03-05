use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::{MuxMode, WindowConfig};
use crate::multiplexer::{
    CreateSessionParams, CreateWindowInSessionParams, CreateWindowParams, Multiplexer,
    PaneSetupOptions,
};
use crate::{cmd, config, git, prompt::Prompt};
use tracing::{debug, info};

use super::types::CreateResult;

/// Sets up the terminal window, files, and hooks for a worktree.
/// This is the shared logic between `create` and `open`.
///
/// # Arguments
/// * `mux` - The terminal multiplexer backend
/// * `branch_name` - The git branch name (for logging/reference)
/// * `handle` - The display name used for window naming
/// * `worktree_path` - Path to the worktree directory
/// * `config` - Configuration settings
/// * `options` - Setup options (hooks, file ops, etc.)
/// * `agent` - Optional agent override
/// * `after_window` - Optional window ID to insert after (for grouping duplicates)
#[allow(clippy::too_many_arguments)]
pub fn setup_environment(
    mux: &dyn Multiplexer,
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

    // Auto-symlink CLAUDE.local.md from main worktree if it exists and is gitignored
    if options.run_file_ops {
        symlink_claude_local_md(&repo_root, effective_working_dir)
            .context("Failed to auto-symlink CLAUDE.local.md")?;
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

    // Build window plans: normalize windows/panes config into a list of window configs.
    // In window mode, we always use a single window from panes config.
    // In session mode, we can use multiple windows from windows config.
    let window_plans: Vec<WindowConfig> = if let Some(windows) = &config.windows {
        // windows config is session-mode only (validated at config load time)
        windows.clone()
    } else {
        // Legacy: wrap panes in a single window plan
        let panes = config.panes.clone();
        vec![WindowConfig { name: None, panes }]
    };

    // Flatten all panes across all windows for prechecks.
    // This ensures Lima pre-boot and prompt validation consider ALL panes.
    let all_panes: Vec<config::PaneConfig> = window_plans
        .iter()
        .flat_map(|w| w.panes.as_deref().unwrap_or(&[]).iter().cloned())
        .collect();
    let all_resolved_panes = resolve_pane_configuration(&all_panes, agent);

    // Validate that prompt will be consumed if one was provided
    if options.prompt_file_path.is_some() {
        validate_prompt_consumption(&all_resolved_panes, agent, config, options)?;
    }

    // Pre-boot Lima VM if needed BEFORE creating the tmux window.
    // This ensures the user sees VM boot progress in their terminal
    // and the window only appears once the VM is ready.
    let lima_vm_name = pre_boot_lima_vm(
        mux,
        config,
        &all_resolved_panes,
        effective_working_dir,
        worktree_path,
        options,
        agent,
    )?;

    let pane_setup_options = PaneSetupOptions {
        run_commands: options.run_pane_commands,
        prompt_file_path: options.prompt_file_path.as_deref(),
        worktree_root: Some(worktree_path),
        lima_vm_name: lima_vm_name.as_deref(),
    };

    // Track the focus pane across all windows
    let mut focus_pane_id: Option<String> = None;

    match options.mode {
        MuxMode::Window => {
            // Window mode: single window, use panes config (window_plans always has 1 entry)
            let panes = window_plans[0].panes.as_deref().unwrap_or(&[]);
            let resolved_panes = resolve_pane_configuration(panes, agent);

            let last_wm_window =
                after_window.or_else(|| mux.find_last_window_with_prefix(prefix).unwrap_or(None));

            let initial_pane_id = mux
                .create_window(CreateWindowParams {
                    prefix,
                    name: handle,
                    cwd: effective_working_dir,
                    after_window: last_wm_window.as_deref(),
                })
                .context("Failed to create window")?;
            info!(
                branch = branch_name,
                handle = handle,
                pane_id = %initial_pane_id,
                "setup_environment:window created"
            );

            let result = mux
                .setup_panes(
                    &initial_pane_id,
                    &resolved_panes,
                    effective_working_dir,
                    pane_setup_options,
                    config,
                    agent,
                )
                .context("Failed to setup panes")?;

            focus_pane_id = Some(result.focus_pane_id);
        }
        MuxMode::Session => {
            let session_full_name = crate::multiplexer::util::prefixed(prefix, handle);

            for (i, window_plan) in window_plans.iter().enumerate() {
                let panes = window_plan.panes.as_deref().unwrap_or(&[]);
                let resolved_panes = resolve_pane_configuration(panes, agent);

                let initial_pane_id = if i == 0 {
                    // First window: create the session
                    let pane_id = mux
                        .create_session(CreateSessionParams {
                            prefix,
                            name: handle,
                            cwd: effective_working_dir,
                            initial_window_name: window_plan.name.as_deref(),
                        })
                        .context("Failed to create session")?;
                    info!(
                        branch = branch_name,
                        handle = handle,
                        window = ?window_plan.name,
                        pane_id = %pane_id,
                        "setup_environment:session created (window 0)"
                    );
                    pane_id
                } else {
                    // Subsequent windows: create within the existing session
                    let pane_id = mux
                        .create_window_in_session(CreateWindowInSessionParams {
                            session_name: &session_full_name,
                            name: window_plan.name.as_deref(),
                            cwd: effective_working_dir,
                        })
                        .context("Failed to create window in session")?;
                    info!(
                        branch = branch_name,
                        handle = handle,
                        window = ?window_plan.name,
                        window_index = i,
                        pane_id = %pane_id,
                        "setup_environment:window created in session"
                    );
                    pane_id
                };

                let result = mux
                    .setup_panes(
                        &initial_pane_id,
                        &resolved_panes,
                        effective_working_dir,
                        pane_setup_options.clone(),
                        config,
                        agent,
                    )
                    .context("Failed to setup panes")?;

                // Track focus: last window with a focus: true pane wins.
                // If no pane has focus: true, use the first window's default.
                let has_explicit_focus = resolved_panes.iter().any(|p| p.focus);
                if i == 0 || has_explicit_focus {
                    focus_pane_id = Some(result.focus_pane_id);
                }
            }
        }
    }

    let focus_pane_id = focus_pane_id.expect("at least one window must be created");
    debug!(
        branch = branch_name,
        focus_id = %focus_pane_id,
        "setup_environment:panes configured"
    );

    // Focus the configured pane and optionally switch to the window/session.
    // select_pane automatically selects the containing window in tmux.
    if options.focus_window {
        mux.select_pane(&focus_pane_id)?;
        match options.mode {
            MuxMode::Window => {
                mux.select_window(prefix, handle)?;
            }
            MuxMode::Session => {
                mux.switch_to_session(prefix, handle)?;
            }
        }
    }

    Ok(CreateResult {
        worktree_path: worktree_path.to_path_buf(),
        branch_name: branch_name.to_string(),
        post_create_hooks_run: hooks_run,
        base_branch: None,
        did_switch: false,
        resolved_handle: handle.to_string(),
        mode: options.mode,
    })
}

/// Pre-boot a Lima VM if sandbox is enabled with the Lima backend and any
/// pane requires sandboxing. Must be called BEFORE creating the tmux window
/// so the user sees VM boot progress in their terminal.
///
/// Returns the VM name if booted, None otherwise.
#[allow(clippy::too_many_arguments)]
fn pre_boot_lima_vm(
    mux: &dyn crate::multiplexer::Multiplexer,
    config: &config::Config,
    panes: &[config::PaneConfig],
    working_dir: &Path,
    worktree_path: &Path,
    options: &super::types::SetupOptions,
    agent: Option<&str>,
) -> Result<Option<String>> {
    if !config.sandbox.is_enabled()
        || !matches!(
            config.sandbox.backend(),
            crate::config::SandboxBackend::Lima
        )
    {
        return Ok(None);
    }

    let effective_agent = agent.or(config.agent.as_deref());
    let shell = mux.get_default_shell()?;

    // Check if any pane will actually need Lima wrapping by resolving
    // commands the same way setup_panes does (respects run_commands flag).
    let any_pane_needs_lima = panes.iter().any(|pane_config| {
        let resolved = crate::multiplexer::util::resolve_pane_command(
            pane_config.command.as_deref(),
            options.run_pane_commands,
            options.prompt_file_path.as_deref(),
            working_dir,
            effective_agent,
            &shell,
        );
        if resolved.is_none() {
            return false;
        }
        let is_agent_pane = pane_config.command.as_deref().is_some_and(|cmd| {
            cmd == "<agent>"
                || crate::multiplexer::agent::is_known_agent(cmd)
                || effective_agent.is_some_and(|a| crate::config::is_agent_command(cmd, a))
        });
        match config.sandbox.target() {
            crate::config::SandboxTarget::All => true,
            crate::config::SandboxTarget::Agent => is_agent_pane,
        }
    });

    if !any_pane_needs_lima {
        return Ok(None);
    }

    info!("pre-booting Lima VM before window creation");
    let vm_name = crate::sandbox::ensure_lima_vm(config, worktree_path)?;
    Ok(Some(vm_name))
}

pub fn resolve_pane_configuration(
    original_panes: &[config::PaneConfig],
    agent: Option<&str>,
) -> Vec<config::PaneConfig> {
    let Some(agent_cmd) = agent else {
        return original_panes.to_vec();
    };

    if original_panes.iter().any(|pane| {
        pane.command
            .as_deref()
            .is_some_and(|cmd| cmd == "<agent>" || crate::multiplexer::agent::is_known_agent(cmd))
    }) {
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

/// Validates that a glob-matched source path is within the repository root.
///
/// Instead of canonicalizing (which resolves symlinks and would reject symlinks
/// inside the repo that point outside), we check the logical path:
/// 1. The path must start with repo_root (glob ensures this for non-traversal patterns)
/// 2. The relative portion must not contain ".." components, which could escape
///    the repo through symlink boundaries (e.g., `symlink_to_external/../secret`)
fn validate_path_within_repo(
    source_path: &Path,
    repo_root: &Path,
    op: &str,
    pattern: &str,
) -> Result<()> {
    let relative = source_path.strip_prefix(repo_root).map_err(|_| {
        anyhow!(
            "Path traversal detected for {} pattern '{}'. The path '{}' is outside the repository root.",
            op, pattern, source_path.display()
        )
    })?;

    if relative
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(anyhow!(
            "Path traversal detected for {} pattern '{}'. The path '{}' contains '..' components.",
            op,
            pattern,
            source_path.display()
        ));
    }

    Ok(())
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

    let mut copy_count = 0;
    let mut symlink_count = 0;

    // Handle copies
    if let Some(copy_patterns) = &file_config.copy {
        for pattern in copy_patterns {
            let full_pattern = repo_root.join(pattern).to_string_lossy().to_string();
            for entry in glob::glob(&full_pattern)? {
                let source_path = entry?;

                validate_path_within_repo(&source_path, repo_root, "copy", pattern)?;

                let relative_path = source_path.strip_prefix(repo_root)?;
                let dest_path = worktree_path.join(relative_path);

                if source_path.is_dir() {
                    // Recursively copy directory contents
                    copy_dir_recursive(&source_path, &dest_path).with_context(|| {
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
                    fs::copy(&source_path, &dest_path).with_context(|| {
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

                validate_path_within_repo(&source_path, repo_root, "symlink", pattern)?;

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

/// Write a prompt file for agent consumption.
///
/// When `working_dir` is provided, writes to `<working_dir>/.workmux/PROMPT-<branch>.md`
/// so the prompt is accessible inside container sandboxes. Also adds `.workmux/` to
/// `.git/info/exclude` to avoid polluting git status.
///
/// When `working_dir` is None, writes to a temp directory (legacy behavior for open command
/// which doesn't know the worktree path at prompt write time).
pub fn write_prompt_file(
    working_dir: Option<&Path>,
    branch_name: &str,
    prompt: &Prompt,
) -> Result<PathBuf> {
    let content = match prompt {
        Prompt::Inline(text) => text.clone(),
        Prompt::FromFile(path) => fs::read_to_string(path)
            .with_context(|| format!("Failed to read prompt file '{}'", path.display()))?,
    };

    // Sanitize branch name: replace path separators with dashes to avoid
    // interpreting slashes as directory separators (e.g., "feature/foo" -> "feature-foo")
    let safe_branch_name = branch_name.replace(['/', '\\', ':'], "-");

    let prompt_path = if let Some(dir) = working_dir {
        // Write to .workmux/ inside the worktree so it's accessible in container sandbox
        let workmux_dir = dir.join(".workmux");
        fs::create_dir_all(&workmux_dir).with_context(|| {
            format!("Failed to create .workmux directory in '{}'", dir.display())
        })?;

        // Add .workmux/ to git exclude to avoid polluting git status
        // In worktrees, .git is a file pointing to the real git dir, so we need to resolve it
        if let Some(exclude_path) = resolve_git_exclude_path(dir)
            && exclude_path.exists()
            && let Ok(content) = fs::read_to_string(&exclude_path)
            && !content.lines().any(|line| line.trim() == ".workmux/")
            && let Ok(mut file) = fs::OpenOptions::new().append(true).open(&exclude_path)
        {
            use std::io::Write;
            let _ = writeln!(file, "\n# workmux prompt files\n.workmux/");
        }

        let prompt_filename = format!("PROMPT-{}.md", safe_branch_name);
        workmux_dir.join(prompt_filename)
    } else {
        // Legacy: write to temp directory for open command
        let prompt_filename = format!("workmux-prompt-{}.md", safe_branch_name);
        std::env::temp_dir().join(prompt_filename)
    };

    fs::write(&prompt_path, content)
        .with_context(|| format!("Failed to write prompt file '{}'", prompt_path.display()))?;
    Ok(prompt_path)
}

/// Resolve the path to .git/info/exclude, handling worktrees correctly.
/// In a worktree, .git is a file containing "gitdir: /path/to/.git/worktrees/name",
/// so we need to find the actual git directory.
fn resolve_git_exclude_path(dir: &Path) -> Option<PathBuf> {
    let git_path = dir.join(".git");

    if git_path.is_dir() {
        // Regular git repo: .git is a directory
        Some(git_path.join("info/exclude"))
    } else if git_path.is_file() {
        // Git worktree: .git is a file pointing to the real git dir
        // Format: "gitdir: /path/to/main/.git/worktrees/name"
        let content = fs::read_to_string(&git_path).ok()?;
        let gitdir = content.strip_prefix("gitdir: ")?.trim();
        // Go up two levels from worktrees/<name> to get to .git/
        let main_git = Path::new(gitdir).ancestors().nth(2)?;
        Some(main_git.join("info/exclude"))
    } else {
        None
    }
}

/// Recursively copy a directory's contents into the destination, overwriting existing files.
/// Symlinks are preserved rather than followed to avoid infinite recursion on symlink loops.
/// Special files (sockets, FIFOs) are skipped to avoid blocking.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;

        // Remove existing entry at destination to support overwrite
        if let Ok(meta) = dst_path.symlink_metadata() {
            if meta.is_dir() && file_type.is_dir() {
                // Both are directories; merge contents
            } else if meta.is_dir() {
                fs::remove_dir_all(&dst_path)?;
            } else {
                fs::remove_file(&dst_path)?;
            }
        }

        if file_type.is_symlink() {
            let target = fs::read_link(&src_path)?;
            std::os::unix::fs::symlink(&target, &dst_path)?;
        } else if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
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
            open_if_exists: false,
            mode: crate::config::MuxMode::default(),
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
            command: Some("my-custom-agent".to_string()),
            focus: true,
            split: None,
            size: None,
            percentage: None,
            target: None,
        }];
        let config = make_config_with_agent(Some("claude")); // config says claude
        let options = make_options_with_prompt(true);

        // CLI agent is my-custom-agent, which matches the pane
        let result =
            super::validate_prompt_consumption(&panes, Some("my-custom-agent"), &config, &options);
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
    fn validate_prompt_succeeds_with_known_agent_command() {
        let panes = vec![config::PaneConfig {
            command: Some("codex --yolo".to_string()),
            focus: true,
            split: None,
            size: None,
            percentage: None,
            target: None,
        }];
        let config = make_config_with_agent(None); // no global agent
        let options = make_options_with_prompt(true);

        // Known agent command should pass validation even without global agent
        let result = super::validate_prompt_consumption(&panes, None, &config, &options);
        assert!(result.is_ok());
    }

    #[test]
    fn resolve_pane_configuration_known_agent_returns_original() {
        let original_panes = vec![
            config::PaneConfig {
                command: Some("claude --dangerously-skip-permissions".to_string()),
                focus: true,
                split: None,
                size: None,
                percentage: None,
                target: None,
            },
            config::PaneConfig {
                command: Some("codex --yolo".to_string()),
                focus: false,
                split: Some(config::SplitDirection::Vertical),
                size: None,
                percentage: None,
                target: None,
            },
        ];

        // Should NOT overwrite known agent panes with the cli agent
        let result = resolve_pane_configuration(&original_panes, Some("gemini"));
        assert_eq!(
            result[0].command.as_deref(),
            Some("claude --dangerously-skip-permissions")
        );
        assert_eq!(result[1].command.as_deref(), Some("codex --yolo"));
    }

    #[test]
    fn write_prompt_file_sanitizes_branch_with_slashes() {
        use crate::prompt::Prompt;

        let branch_name = "feature/nested/add-login";
        let prompt = Prompt::Inline("test prompt content".to_string());

        // Test legacy mode (None working_dir)
        let path = super::write_prompt_file(None, branch_name, &prompt)
            .expect("Should create prompt file");

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

    #[test]
    fn write_prompt_file_with_working_dir() {
        use crate::prompt::Prompt;
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let branch_name = "feature/test";
        let prompt = Prompt::Inline("test prompt".to_string());

        let path = super::write_prompt_file(Some(temp.path()), branch_name, &prompt)
            .expect("Should create prompt file");

        // Verify it's in .workmux/ directory
        assert!(path.starts_with(temp.path().join(".workmux")));
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("PROMPT-feature-test")
        );

        // Verify content
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "test prompt");
    }
}

/// Symlink CLAUDE.local.md from main worktree if it exists and is gitignored.
fn symlink_claude_local_md(repo_root: &Path, worktree_path: &Path) -> Result<()> {
    let source = repo_root.join("CLAUDE.local.md");
    if !source.exists() {
        return Ok(());
    }

    if !git::is_path_ignored(repo_root, "CLAUDE.local.md") {
        return Ok(());
    }

    let dest = worktree_path.join("CLAUDE.local.md");
    if dest.symlink_metadata().is_ok() {
        // Already exists (file, symlink, or dir) -- skip
        return Ok(());
    }

    let relative_source = pathdiff::diff_paths(&source, worktree_path)
        .ok_or_else(|| anyhow!("Could not create relative path for CLAUDE.local.md symlink"))?;

    #[cfg(unix)]
    std::os::unix::fs::symlink(&relative_source, &dest)
        .context("Failed to symlink CLAUDE.local.md")?;

    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&relative_source, &dest)
        .context("Failed to symlink CLAUDE.local.md")?;

    info!("Symlinked CLAUDE.local.md to worktree");
    Ok(())
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

    // Known agent commands always consume prompts (they have their own agent
    // profile), so the prompt is consumed regardless of whether a global agent
    // is configured.
    let has_self_identifying_agent = panes.iter().any(|pane| {
        pane.command
            .as_deref()
            .is_some_and(crate::multiplexer::agent::is_known_agent)
    });

    if has_self_identifying_agent {
        return Ok(());
    }

    // For non-named panes, require a global agent
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
