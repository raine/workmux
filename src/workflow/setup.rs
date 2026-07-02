use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{MuxMode, WindowConfig, WindowPlacement};
use crate::multiplexer::{
    CreateSessionParams, CreateWindowInSessionParams, CreateWindowParams, Multiplexer,
    PaneSetupOptions,
};
use crate::{cmd, config, git, prompt::Prompt};
use tracing::{debug, info};

use super::file_ops::{handle_file_operations, symlink_claude_local_md};
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
    let repo_root = match &options.config_root {
        Some(path) => path.clone(),
        None => git::get_main_worktree_root()?,
    };

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
    let all_resolved_panes = resolve_pane_configuration(&all_panes, config, agent);

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
        resume_mode: options.resume_mode.clone(),
    };

    let target_window_name = options.target_window_name.as_deref().unwrap_or(handle);
    let target_session_name = options.target_session_name.as_deref().unwrap_or(handle);
    let mux_target_full_name = match options.mode {
        MuxMode::Window => crate::multiplexer::util::prefixed(prefix, target_window_name),
        MuxMode::Session => crate::multiplexer::util::prefixed(prefix, target_session_name),
    };

    // Track the focus and zoom pane across all windows
    let mut focus_pane_id: Option<String> = None;
    let mut zoom_pane_id: Option<String> = None;

    match options.mode {
        MuxMode::Window => {
            // Window mode: single window, use panes config (window_plans always has 1 entry)
            let panes = window_plans[0].panes.as_deref().unwrap_or(&[]);
            let resolved_panes = resolve_pane_configuration(panes, config, agent);

            let initial_pane_id = if let Some(parent_session) =
                options.window_session_name.as_deref()
            {
                let parent_session_full_name = parent_session.to_string();
                if mux.session_exists(&parent_session_full_name)? {
                    mux.create_window_in_session(CreateWindowInSessionParams {
                        session_name: &parent_session_full_name,
                        name: Some(&mux_target_full_name),
                        cwd: effective_working_dir,
                    })
                    .context("Failed to create window in session")?
                } else {
                    mux.create_session(CreateSessionParams {
                        prefix: "",
                        name: &parent_session_full_name,
                        cwd: effective_working_dir,
                        initial_window_name: Some(&mux_target_full_name),
                    })
                    .context("Failed to create session")?
                }
            } else {
                let placement_window_id = match config.window_placement() {
                    WindowPlacement::AfterCurrent => mux.current_window_id()?,
                    WindowPlacement::Rightmost => mux.rightmost_window_id()?,
                };
                let insertion_target = after_window.as_deref().or(placement_window_id.as_deref());
                mux.create_window(CreateWindowParams {
                    prefix,
                    name: target_window_name,
                    cwd: effective_working_dir,
                    after_window: insertion_target,
                })
                .context("Failed to create window")?
            };
            info!(
                branch = branch_name,
                handle = handle,
                target = %mux_target_full_name,
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
            zoom_pane_id = result.zoom_pane_id;
        }
        MuxMode::Session => {
            let session_full_name = crate::multiplexer::util::prefixed(prefix, target_session_name);

            for (i, window_plan) in window_plans.iter().enumerate() {
                let panes = window_plan.panes.as_deref().unwrap_or(&[]);
                let resolved_panes = resolve_pane_configuration(panes, config, agent);

                let initial_pane_id = if i == 0 {
                    // First window: create the session
                    let pane_id = mux
                        .create_session(CreateSessionParams {
                            prefix,
                            name: target_session_name,
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
                let has_explicit_focus = resolved_panes.iter().any(|p| p.focus || p.zoom);
                if i == 0 || has_explicit_focus {
                    focus_pane_id = Some(result.focus_pane_id);
                }

                if result.zoom_pane_id.is_some() {
                    zoom_pane_id = result.zoom_pane_id;
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
    if options.focus_window {
        match options.mode {
            MuxMode::Window => {
                // select_pane automatically selects the containing window in tmux.
                mux.select_pane(&focus_pane_id)?;
                if options.window_session_name.is_none() {
                    mux.select_window(prefix, target_window_name)?;
                }
            }
            MuxMode::Session => {
                // switch_to_pane switches the client directly to the pane,
                // which also selects the correct window within the session.
                // Using select_pane + switch_to_session would lose the window
                // selection because switch_to_session targets the session by
                // name, defaulting to its first window.
                mux.switch_to_pane(&focus_pane_id, None)?;
            }
        }
    }

    // Zoom the pane after focus is applied
    if let Some(ref zoom_id) = zoom_pane_id {
        mux.zoom_pane(zoom_id)?;
    }

    Ok(CreateResult {
        worktree_path: worktree_path.to_path_buf(),
        branch_name: branch_name.to_string(),
        post_create_hooks_run: hooks_run,
        base_branch: None,
        did_switch: false,
        resolved_handle: handle.to_string(),
        mux_target_full_name,
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

    let shell = mux.get_default_shell()?;

    // Check if any pane will actually need Lima wrapping by resolving
    // commands the same way setup_panes does (respects run_commands flag).
    let any_pane_needs_lima = panes.iter().any(|pane_config| {
        let resolved = crate::multiplexer::util::resolve_pane_command_with_config(
            pane_config.command.as_deref(),
            options.run_pane_commands,
            options.prompt_file_path.as_deref(),
            working_dir,
            config,
            agent,
            &shell,
        );
        if resolved.is_none() {
            return false;
        }
        let is_agent_pane = resolved
            .as_ref()
            .is_some_and(|resolved| resolved.selected_agent.is_some());
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

fn resolve_effective_agent(config: &config::Config, cli_agent: Option<&str>) -> Option<String> {
    cli_agent
        .map(|agent| {
            config
                .agents
                .get(agent)
                .map(|entry| entry.command_or_default(agent))
                .unwrap_or_else(|| agent.to_string())
        })
        .or_else(|| config.agent.clone())
}

fn pane_runs_agent(command: &str, agent_command: &str, agent_type: Option<&str>) -> bool {
    let trimmed = command.trim();
    trimmed == "<agent>"
        || (trimmed.starts_with("<agent:") && trimmed.ends_with('>'))
        || crate::multiplexer::agent::is_known_agent(command)
        || config::is_agent_command(command, agent_command)
        || agent_type.is_some_and(|kind| config::is_agent_command(command, kind))
}

pub fn resolve_pane_configuration(
    original_panes: &[config::PaneConfig],
    config: &config::Config,
    agent: Option<&str>,
) -> Vec<config::PaneConfig> {
    let Some(agent_cmd) = agent else {
        return original_panes.to_vec();
    };
    let resolved_agent = config
        .agents
        .get(agent_cmd)
        .map(|entry| entry.command_or_default(agent_cmd))
        .unwrap_or_else(|| agent_cmd.to_string());
    let injected_agent = "<agent>".to_string();

    if original_panes.iter().any(|pane| {
        pane.command
            .as_deref()
            .is_some_and(|cmd| pane_runs_agent(cmd, &resolved_agent, config.agent_type.as_deref()))
    }) {
        return original_panes.to_vec();
    }

    let mut panes = original_panes.to_vec();

    if let Some(focused) = panes.iter_mut().find(|pane| pane.focus) {
        focused.command = Some(injected_agent.clone());
        return panes;
    }

    if let Some(first) = panes.get_mut(0) {
        first.command = Some(injected_agent.clone());
        return panes;
    }

    vec![config::PaneConfig {
        command: Some(injected_agent),
        focus: true,
        ..Default::default()
    }]
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

    let effective_agent = resolve_effective_agent(config, cli_agent);

    let Some(agent_cmd) = effective_agent else {
        return Err(anyhow!(
            "Prompt provided but no agent is configured to consume it. \
             Set 'agent' in config or use -a/--agent flag."
        ));
    };

    let consumes_prompt = panes.iter().any(|pane| {
        pane.command
            .as_deref()
            .map(|cmd| pane_runs_agent(cmd, &agent_cmd, config.agent_type.as_deref()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_pane_configuration_no_agent_returns_original() {
        let original_panes = vec![config::PaneConfig {
            command: Some("vim".to_string()),
            focus: true,
            ..Default::default()
        }];

        let result = resolve_pane_configuration(&original_panes, &config::Config::default(), None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].command, Some("vim".to_string()));
    }

    #[test]
    fn resolve_pane_configuration_agent_placeholder_returns_original() {
        let original_panes = vec![config::PaneConfig {
            command: Some("<agent>".to_string()),
            focus: true,
            ..Default::default()
        }];

        let result = resolve_pane_configuration(
            &original_panes,
            &make_config_with_agent(Some("claude")),
            Some("claude"),
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].command, Some("<agent>".to_string()));
    }

    #[test]
    fn resolve_pane_configuration_agent_sets_focused_pane() {
        let original_panes = vec![
            config::PaneConfig {
                command: Some("vim".to_string()),
                ..Default::default()
            },
            config::PaneConfig {
                command: Some("npm run dev".to_string()),
                focus: true,
                ..Default::default()
            },
        ];

        let result = resolve_pane_configuration(
            &original_panes,
            &make_config_with_agent(Some("claude")),
            Some("claude"),
        );
        assert_eq!(result[0].command, Some("vim".to_string()));
        assert_eq!(result[1].command, Some("<agent>".to_string()));
    }

    #[test]
    fn resolve_pane_configuration_agent_sets_first_pane_when_no_focus() {
        let original_panes = vec![config::PaneConfig {
            command: Some("vim".to_string()),
            ..Default::default()
        }];

        let result = resolve_pane_configuration(
            &original_panes,
            &make_config_with_agent(Some("claude")),
            Some("claude"),
        );
        assert_eq!(result[0].command, Some("<agent>".to_string()));
    }

    #[test]
    fn resolve_pane_configuration_agent_creates_new_pane_when_empty() {
        let result = resolve_pane_configuration(
            &[],
            &make_config_with_agent(Some("claude")),
            Some("claude"),
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].command, Some("<agent>".to_string()));
        assert!(result[0].focus);
    }

    #[test]
    fn resolve_pane_configuration_preserves_named_profile_selector() {
        let original_panes = vec![config::PaneConfig {
            command: Some("vim".to_string()),
            focus: true,
            ..Default::default()
        }];
        let mut config = make_config_with_agent(Some("cc-work"));
        config.agents.insert(
            "cc-work".to_string(),
            config::AgentEntry {
                command: Some("claude".to_string()),
                agent_type: Some("claude".to_string()),
                args: vec!["-p".to_string()],
                env: std::collections::BTreeMap::new(),
            },
        );

        let result = resolve_pane_configuration(&original_panes, &config, Some("cc-work"));
        assert_eq!(result[0].command, Some("<agent>".to_string()));
    }

    // --- validate_prompt_consumption tests ---

    fn make_config_with_agent(agent: Option<&str>) -> config::Config {
        config::Config {
            agent: agent.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    fn make_config_with_typed_agent(agent: &str, agent_type: &str) -> config::Config {
        config::Config {
            agent: Some(agent.to_string()),
            agent_type: Some(agent_type.to_string()),
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
            target_window_name: None,
            target_session_name: None,
            window_session_name: None,
            resume_mode: crate::multiplexer::types::ResumeMode::default(),
        }
    }

    fn prompt_pane(command: &str) -> config::PaneConfig {
        config::PaneConfig {
            command: Some(command.into()),
            focus: true,
            ..Default::default()
        }
    }

    fn split_prompt_pane(command: &str) -> config::PaneConfig {
        config::PaneConfig {
            command: Some(command.into()),
            split: Some(config::SplitDirection::Horizontal),
            ..Default::default()
        }
    }

    fn validate_prompt(
        panes: &[config::PaneConfig],
        cli_agent: Option<&str>,
        config: &config::Config,
        run_pane_commands: bool,
    ) -> anyhow::Result<()> {
        let options = make_options_with_prompt(run_pane_commands);
        super::validate_prompt_consumption(panes, cli_agent, config, &options)
    }

    #[test]
    fn validate_prompt_errors_when_pane_commands_disabled() {
        let panes = vec![prompt_pane("<agent>")];
        let config = make_config_with_agent(Some("claude"));

        let result = validate_prompt(&panes, None, &config, false);
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
        let panes = vec![prompt_pane("vim")];
        let config = make_config_with_agent(None); // no agent

        let result = validate_prompt(&panes, None, &config, true);
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
                focus: true,
                ..Default::default()
            },
            split_prompt_pane("clear"),
        ];
        let config = make_config_with_agent(Some("claude"));

        let result = validate_prompt(&panes, None, &config, true);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("no pane is configured to run the agent"));
        assert!(err_msg.contains("claude"));
    }

    #[test]
    fn validate_prompt_succeeds_with_agent_placeholder() {
        let panes = vec![prompt_pane("<agent>")];
        let config = make_config_with_agent(Some("claude"));

        assert!(validate_prompt(&panes, None, &config, true).is_ok());
    }

    #[test]
    fn validate_prompt_succeeds_with_matching_agent_command() {
        let panes = vec![prompt_pane("claude")];
        let config = make_config_with_agent(Some("claude"));

        assert!(validate_prompt(&panes, None, &config, true).is_ok());
    }

    #[test]
    fn validate_prompt_succeeds_with_typed_agent_wrapper() {
        let panes = vec![prompt_pane("claudeg --dangerously-skip-permissions")];
        let config =
            make_config_with_typed_agent("claudeg --dangerously-skip-permissions", "claude");

        assert!(validate_prompt(&panes, None, &config, true).is_ok());
    }

    #[test]
    fn validate_prompt_cli_agent_overrides_config() {
        let panes = vec![prompt_pane("my-custom-agent")];
        let config = make_config_with_agent(Some("claude")); // config says claude

        // CLI agent is my-custom-agent, which matches the pane
        assert!(validate_prompt(&panes, Some("my-custom-agent"), &config, true).is_ok());

        // CLI agent is None, falls back to config (claude), which doesn't match
        assert!(validate_prompt(&panes, None, &config, true).is_err());
    }

    #[test]
    fn validate_prompt_succeeds_when_any_pane_matches() {
        let panes = vec![
            config::PaneConfig {
                command: Some("vim".into()), // doesn't match
                ..Default::default()
            },
            config::PaneConfig {
                focus: true,
                ..split_prompt_pane("claude --verbose") // matches
            },
        ];
        let config = make_config_with_agent(Some("claude"));

        assert!(validate_prompt(&panes, None, &config, true).is_ok());
    }

    #[test]
    fn validate_prompt_succeeds_with_known_agent_command() {
        let panes = vec![prompt_pane("codex --yolo")];
        let config = make_config_with_agent(None); // no global agent

        // Known agent command should pass validation even without global agent
        assert!(validate_prompt(&panes, None, &config, true).is_ok());
    }

    #[test]
    fn validate_prompt_succeeds_with_quoted_env_agent_command() {
        let panes = vec![prompt_pane("env FOO='bar baz' claude --verbose")];
        let config = make_config_with_agent(Some("claude"));

        assert!(validate_prompt(&panes, None, &config, true).is_ok());
    }

    #[test]
    fn resolve_pane_configuration_typed_agent_returns_original() {
        let original_panes = vec![config::PaneConfig {
            command: Some("claudeg --dangerously-skip-permissions".to_string()),
            focus: true,
            ..Default::default()
        }];
        let config =
            make_config_with_typed_agent("claudeg --dangerously-skip-permissions", "claude");

        let result = resolve_pane_configuration(&original_panes, &config, None);
        assert_eq!(
            result[0].command.as_deref(),
            Some("claudeg --dangerously-skip-permissions")
        );
    }

    #[test]
    fn resolve_pane_configuration_known_agent_returns_original() {
        let original_panes = vec![
            config::PaneConfig {
                command: Some("claude --dangerously-skip-permissions".to_string()),
                focus: true,
                ..Default::default()
            },
            config::PaneConfig {
                command: Some("codex --yolo".to_string()),
                split: Some(config::SplitDirection::Vertical),
                ..Default::default()
            },
        ];
        let config = make_config_with_agent(Some("gemini"));

        let result = resolve_pane_configuration(&original_panes, &config, Some("gemini"));
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
