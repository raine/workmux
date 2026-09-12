use crate::config::MuxMode;
use crate::multiplexer::handle::mode_label;
use crate::multiplexer::{MuxHandle, create_backend, detect_backend};
use crate::prompt::{Prompt, PromptDocument, foreach_from_frontmatter};
use crate::spinner;
use crate::template::{
    TemplateEnv, WorktreeSpec, create_template_env, generate_worktree_specs, parse_foreach_matrix,
    render_prompt_body, validate_template_variables,
};
use crate::workflow::SetupOptions;
use crate::workflow::pr::{
    CheckoutRef, Forge, PrReference, detect_remote_branch, detect_remote_branch_dry_run,
};
use crate::workflow::prompt_loader::{PromptLoadArgs, load_prompt, parse_prompt_with_frontmatter};
use crate::{config, git, workflow};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read};
#[cfg(unix)]
use std::os::fd::AsRawFd;

// Re-export the arg types that are used by the CLI
pub use super::args::{MultiArgs, PromptArgs, RescueArgs, SetupFlags};

/// Variable name exposed to templates for stdin input lines
const STDIN_INPUT_VAR: &str = "input";

/// Maximum stdin size to read (10MB) to prevent OOM from infinite streams
const STDIN_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Validate plain agent executables on the host. Shell expressions and relative
/// paths depend on the pane shell and worktree environment.
fn validate_agent_executable(config: &config::Config) -> Result<()> {
    if config.sandbox.is_enabled() {
        return Ok(());
    }
    let Some(command) = config.agent.as_deref() else {
        return Ok(());
    };
    if command.is_empty() {
        bail!("Agent command must not be empty");
    }
    if !command
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.+/".contains(c))
        || (command.contains('/') && !std::path::Path::new(command).is_absolute())
    {
        return Ok(());
    }
    let found =
        config::resolve_executable_path(command).is_some_and(|path| which::which(path).is_ok());
    if !found {
        bail!(
            "Agent executable '{}' not found or not executable. Check --agent or your agent configuration.",
            command
        );
    }
    Ok(())
}

/// Generate a branch name from prompt text using LLM with spinner feedback.
///
/// This helper consolidates the duplicate branch name generation logic that was
/// previously duplicated in both `run()` and `create_worktrees_from_specs()`.
fn generate_branch_name_with_spinner(
    prompt_text: Option<&str>,
    config: &config::Config,
) -> Result<String> {
    let prompt_text = prompt_text.ok_or_else(|| anyhow!("Prompt is required for --auto-name"))?;

    let model = config.auto_name.as_ref().and_then(|c| c.model.as_deref());
    let system_prompt = config
        .auto_name
        .as_ref()
        .and_then(|c| c.system_prompt.as_deref());

    // Resolve effective command: explicit config > agent profile > None (llm fallback)
    let config_command = config
        .auto_name
        .as_ref()
        .and_then(|c| c.command.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let profile_command = crate::multiplexer::agent::resolve_profile_with_type(
        config.agent.as_deref(),
        config.agent_type.as_deref(),
    )
    .auto_name_command();
    let effective_command = config_command.or(profile_command);

    tracing::info!(
        config_command = config_command,
        profile_command = profile_command,
        effective_command = effective_command,
        agent = config.agent.as_deref().unwrap_or("none"),
        "resolved auto-name command"
    );

    // Extract program name from effective command for spinner message
    let program_name = effective_command
        .and_then(|cmd| cmd.split_whitespace().next())
        .unwrap_or("llm");
    let spinner_msg = format!("Generating branch name with {}", program_name);

    let generated = spinner::with_spinner(&spinner_msg, || {
        crate::llm::generate_branch_name(prompt_text, model, system_prompt, effective_command)
    })?;
    println!("  Branch: {}", generated);

    Ok(generated)
}

/// Check for and read lines from stdin if available.
fn read_stdin_lines() -> Result<Vec<String>> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() || !stdin_has_data(&stdin)? {
        return Ok(Vec::new());
    }

    let mut buffer = String::new();
    stdin
        .take(STDIN_MAX_BYTES)
        .read_to_string(&mut buffer)
        .context("Failed to read from stdin")?;

    let lines: Vec<String> = buffer
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(lines)
}

#[cfg(unix)]
fn stdin_has_data(stdin: &std::io::Stdin) -> Result<bool> {
    let fd = stdin.as_raw_fd();
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) };
    if ready < 0 {
        return Err(std::io::Error::last_os_error()).context("Failed to poll stdin");
    }
    Ok(ready > 0)
}

#[cfg(not(unix))]
fn stdin_has_data(_stdin: &std::io::Stdin) -> Result<bool> {
    Ok(true)
}

/// Check preconditions for the add command (git repo and multiplexer session).
/// Returns Ok(()) if all preconditions are met, or an error listing all failures.
fn check_preconditions(headless: bool) -> Result<()> {
    let is_git = git::is_git_repo()?;
    let mux = create_backend(detect_backend());
    let is_mux_running = headless || mux.is_running()?;

    if is_git && is_mux_running {
        return Ok(());
    }

    let mut errors = Vec::new();

    if !is_mux_running {
        errors.push(format!("{} is not running.", mux.name()));
    }
    if !is_git {
        errors.push("Current directory is not a git repository.".to_string());
    }

    // Add blank line before suggestions
    errors.push("".to_string());

    if !is_mux_running {
        errors.push(format!("Please start a {} session first.", mux.name()));
    }
    if !is_git {
        errors.push("Please run this command from within a git repository.".to_string());
    }

    Err(anyhow!(errors.join("\n")))
}

/// Resolve a named layout by replacing `config.panes` with the layout's panes.
fn resolve_layout(config: &mut config::Config, layout_name: &str) -> Result<()> {
    let layouts = config.layouts.as_ref().ok_or_else(|| {
        anyhow!(
            "Layout '{}' requested but no layouts are defined in config",
            layout_name
        )
    })?;
    let layout = layouts.get(layout_name).ok_or_else(|| {
        let mut available: Vec<_> = layouts.keys().collect();
        available.sort();
        anyhow!(
            "Layout '{}' not found. Available layouts: {}",
            layout_name,
            available
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    config.panes = Some(layout.panes.clone());
    config.windows = None; // Layout overrides session windows
    Ok(())
}

#[derive(Serialize)]
struct HeadlessReceipt<'a> {
    schema_version: u32,
    handle: &'a str,
    branch: &'a str,
    worktree_path: &'a std::path::Path,
    working_directory: &'a std::path::Path,
    base_branch: Option<&'a str>,
    post_create_hooks_run: usize,
}

#[allow(clippy::too_many_arguments)]
fn run_headless(
    branch_name: Option<&str>,
    pr: Option<PrReference>,
    auto_name: bool,
    base: Option<&str>,
    name: Option<&str>,
    target_name: Option<&str>,
    parent_session: Option<&str>,
    prompt: &PromptArgs,
    setup: &SetupFlags,
    rescue: &RescueArgs,
    multi: &MultiArgs,
    layout: Option<&str>,
    fork: Option<&str>,
    wait: bool,
    dry_run: bool,
    mode_override: Option<MuxMode>,
    json: bool,
    config_override: Option<&std::path::Path>,
) -> Result<()> {
    if crate::sandbox::guest::is_sandbox_guest() {
        bail!("--headless is not supported from inside a sandbox");
    }
    if pr.is_some() {
        bail!("--pr cannot be used with --headless");
    }
    if auto_name {
        bail!("--auto-name cannot be used with --headless");
    }
    if target_name.is_some() || parent_session.is_some() || mode_override.is_some() {
        bail!("Multiplexer target options cannot be used with --headless");
    }
    if prompt.has_any() || prompt.prompt_file_only {
        bail!("Prompt options cannot be used with --headless");
    }
    if setup.no_pane_cmds || setup.background || setup.open_if_exists || setup.sandbox {
        bail!(
            "Pane, background, open-existing, and sandbox options cannot be used with --headless"
        );
    }
    if rescue.with_changes || rescue.patch || rescue.include_untracked {
        bail!("Change rescue options cannot be used with --headless");
    }
    if !multi.agent.is_empty()
        || multi.count.is_some()
        || multi.foreach.is_some()
        || multi.max_concurrent.is_some()
    {
        bail!("Multi-worktree options cannot be used with --headless");
    }
    if layout.is_some() || fork.is_some() || wait || dry_run {
        bail!("Layout, fork, wait, and dry-run options cannot be used with --headless");
    }

    check_preconditions(true)?;
    let branch_name = branch_name.context("A branch name is required with --headless")?;
    let (config, config_location) = config::Config::load_with_location(None, config_override)?;
    let mux = create_backend(detect_backend());
    let context = workflow::WorkflowContext::new(config, mux, config_location)?;
    if let Some((candidate_remote, _)) = branch_name.split_once('/')
        && git::remote_exists_in(candidate_remote, Some(&context.execution_dir))?
    {
        bail!(
            "--headless treats branch names as local and does not resolve remote branch '{}'; pass a local branch name",
            branch_name
        );
    }
    let handle = crate::naming::derive_handle(branch_name, name, &context.config)?;
    let configured_base = if base.is_none() {
        workflow::resolve_configured_base_branch(
            &context.config,
            &context.execution_dir,
            context.vcs.as_ref(),
        )?
    } else {
        None
    };
    let resolved_base = base.or(configured_base.as_deref());
    let options = SetupOptions::new(!setup.no_hooks, !setup.no_file_ops, false);

    if !json {
        super::announce_hooks(
            &context.config,
            Some(&options),
            super::HookPhase::PostCreate,
        );
    }

    let result = workflow::create_headless(
        &context,
        workflow::CreateArgs {
            branch_name,
            handle: &handle,
            base_branch: resolved_base,
            remote_branch: None,
            checkout_ref: None,
            prompt: None,
            options,
            mode_override: None,
            agent: None,
            is_explicit_name: name.is_some(),
            prompt_file_only: false,
            fork_source: None,
        },
        json,
    )
    .with_context(|| {
        format!(
            "Failed to provision headless worktree for branch '{}'",
            branch_name
        )
    })?;

    if json {
        let receipt = HeadlessReceipt {
            schema_version: 1,
            handle: &result.resolved_handle,
            branch: &result.branch_name,
            worktree_path: &result.worktree_path,
            working_directory: &result.working_directory,
            base_branch: result.base_branch.as_deref(),
            post_create_hooks_run: result.post_create_hooks_run,
        };
        println!("{}", serde_json::to_string(&receipt)?);
    } else {
        println!(
            "✓ Successfully provisioned headless worktree for '{}'\n  Worktree: {}\n  Working directory: {}",
            result.branch_name,
            result.worktree_path.display(),
            result.working_directory.display()
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    branch_name: Option<&str>,
    pr: Option<PrReference>,
    forge: Option<Forge>,
    auto_name: bool,
    base: Option<&str>,
    name: Option<String>,
    target_name: Option<String>,
    parent_session: Option<String>,
    prompt_args: PromptArgs,
    setup: SetupFlags,
    rescue: RescueArgs,
    multi: MultiArgs,
    layout: Option<String>,
    continue_session: bool,
    fork: Option<String>,
    wait: bool,
    dry_run: bool,
    mode_override: Option<MuxMode>,
    headless: bool,
    json: bool,
    config_override: Option<&std::path::Path>,
) -> Result<()> {
    if headless {
        return run_headless(
            branch_name,
            pr,
            auto_name,
            base,
            name.as_deref(),
            target_name.as_deref(),
            parent_session.as_deref(),
            &prompt_args,
            &setup,
            &rescue,
            &multi,
            layout.as_deref(),
            fork.as_deref(),
            wait,
            dry_run,
            mode_override,
            json,
            config_override,
        );
    }

    // Inside a sandbox guest, route through RPC to the host supervisor
    if crate::sandbox::guest::is_sandbox_guest() {
        if dry_run {
            bail!("--dry-run is not supported from inside a sandbox");
        }
        if layout.is_some() {
            bail!("--layout is not supported from inside a sandbox");
        }
        if continue_session {
            bail!("--continue is not supported from inside a sandbox");
        }
        if fork.is_some() {
            bail!("--fork is not supported from inside a sandbox");
        }
        if config_override.is_some() {
            bail!("--config is not supported from inside a sandbox");
        }
        if target_name.is_some() {
            bail!("--target-name is not supported from inside a sandbox");
        }
        if parent_session.is_some() {
            bail!("--parent-session is not supported from inside a sandbox");
        }
        return run_add_via_rpc(
            branch_name,
            auto_name,
            &prompt_args,
            &setup,
            &rescue,
            &multi,
            base,
            pr,
            name.as_deref(),
            wait,
            mode_override,
        );
    }

    // Creation requires both git and a running multiplexer. A dry run only reads
    // repository and configuration state.
    if !dry_run {
        check_preconditions(false)?;
    }

    // Extract sandbox override before consuming setup flags
    let sandbox_override = setup.sandbox;

    // Load config early to determine mode
    let mut initial_config = config::Config::load_with_override(
        multi.agent.first().map(|s| s.as_str()),
        config_override,
    )?;

    // Resolve fork source if --fork is set
    let fork_source = if let Some(ref fork_arg) = fork {
        let agent_name = initial_config.agent.as_deref().unwrap_or("claude");
        let forker =
            crate::multiplexer::conversation::resolve_forker(agent_name).ok_or_else(|| {
                anyhow!(
                    "Agent '{}' does not support conversation forking",
                    agent_name
                )
            })?;

        // Use worktree root (not cwd) so subdirectory invocation works correctly
        let source_path = git::get_repo_root()?;
        let session = if fork_arg.is_empty() {
            // --fork without value: use most recent
            forker
                .find_latest_conversation(&source_path)?
                .ok_or_else(|| {
                    anyhow!(
                        "No conversations found in current worktree.\n\
                     Path searched: {}",
                        source_path.display()
                    )
                })?
        } else {
            // --fork=<session-id>: find specific session
            forker
                .find_conversation(&source_path, fork_arg)?
                .ok_or_else(|| {
                    anyhow!(
                        "No conversation matching '{}' found in current worktree",
                        fork_arg
                    )
                })?
        };
        Some(crate::workflow::types::ForkSource { forker, session })
    } else {
        None
    };
    let mode = mode_override.unwrap_or(initial_config.mode());

    // Validate layout early to fail fast before any LLM calls
    if let Some(layout_name) = &layout {
        resolve_layout(&mut initial_config, layout_name)?;
    }

    // Construct setup options from flags
    let mut options = SetupOptions::new(!setup.no_hooks, !setup.no_file_ops, !setup.no_pane_cmds);
    options.focus_window = !setup.background;
    options.open_if_exists = setup.open_if_exists;
    options.mode = mode;
    if continue_session {
        options.resume_mode = crate::multiplexer::types::ResumeMode::Continue;
    }

    // If using --auto-name and config has auto_name.background = true, run in background
    if auto_name && options.focus_window {
        let config = config::Config::load_with_override(
            multi.agent.first().map(|s| s.as_str()),
            config_override,
        )?;
        if config
            .auto_name
            .as_ref()
            .and_then(|c| c.background)
            .unwrap_or(false)
        {
            options.focus_window = false;
        }
    }

    // Detect stdin input early
    let stdin_lines = read_stdin_lines()?;
    let has_stdin = !stdin_lines.is_empty();

    // Determine if we're in explicit multi-worktree mode (before loading prompt)
    let is_explicit_multi =
        has_stdin || multi.foreach.is_some() || multi.count.is_some() || multi.agent.len() > 1;

    // Handle auto-name: load prompt first, generate branch name
    // In multi-worktree mode with auto-name, we defer LLM generation to the loop
    let mut checkout_ref = None;
    let (final_branch_name, preloaded_prompt, remote_branch_for_pr, deferred_auto_name) =
        if auto_name {
            // Use editor if no prompt source specified, otherwise use provided source
            let use_editor = prompt_args.prompt.is_none() && prompt_args.prompt_file.is_none();

            // Cannot use interactive editor when stdin is piped (editor can't read terminal)
            if has_stdin && (prompt_args.prompt_editor || use_editor) {
                return Err(anyhow!(
                    "Cannot use interactive prompt editor when piping input from stdin.\n\
                    Please provide a prompt via --prompt or --prompt-file."
                ));
            }

            let prompt = load_prompt(&PromptLoadArgs {
                prompt_editor: use_editor || prompt_args.prompt_editor,
                prompt_inline: prompt_args.prompt.as_deref(),
                prompt_file: prompt_args.prompt_file.as_ref(),
            })?
            .ok_or_else(|| anyhow!("Prompt is required for --auto-name"))?;

            // Check if we need to defer auto-name generation to the loop
            // This happens when we have multi-worktree mode OR frontmatter foreach
            let prompt_doc_preview = parse_prompt_with_frontmatter(&prompt, true)?;
            let has_frontmatter_foreach = prompt_doc_preview.meta.foreach.is_some();

            if is_explicit_multi || has_frontmatter_foreach {
                // Defer LLM generation - use placeholder branch name
                ("deferred".to_string(), Some(prompt), None, true)
            } else {
                // Single worktree mode - generate branch name now
                let prompt_text = prompt.read_content()?;
                let config = config::Config::load_with_override(
                    multi.agent.first().map(|s| s.as_str()),
                    config_override,
                )?;
                let generated = generate_branch_name_with_spinner(Some(&prompt_text), &config)?;
                (generated, Some(prompt), None, false)
            }
        } else if let Some(pr_reference) = pr {
            let result = pr_reference.resolve(forge, branch_name, dry_run)?;
            checkout_ref = Some(result.checkout_ref);
            (result.local_branch, None, Some(result.remote_branch), false)
        } else {
            // Normal flow: use provided branch name
            (
                branch_name
                    .expect("branch_name required when --pr and --auto-name not provided")
                    .to_string(),
                None,
                None,
                false,
            )
        };

    // Use the determined branch name and override base if from PR
    let branch_name = &final_branch_name;
    let cli_base = if remote_branch_for_pr.is_some() {
        None
    } else {
        base
    };

    // Validate --with-changes compatibility
    if rescue.with_changes && multi.agent.len() > 1 {
        return Err(anyhow!(
            "--with-changes cannot be used with multiple --agent flags. Use zero or one --agent."
        ));
    }

    // Validate --name compatibility with multi-worktree generation
    let has_multi_worktree = multi.agent.len() > 1
        || multi.count.is_some_and(|c| c > 1)
        || multi.foreach.is_some()
        || has_stdin;
    if name.is_some() && has_multi_worktree {
        return Err(anyhow!(
            "--name cannot be used with multi-worktree generation (multiple --agent, --count, --foreach, or stdin).\n\
             Use the default naming or set worktree_naming/worktree_prefix in config instead."
        ));
    }
    if (target_name.is_some() || parent_session.is_some()) && has_multi_worktree {
        return Err(anyhow!(
            "--target-name and --parent-session cannot be used with multi-worktree generation (multiple --agent, --count, --foreach, or stdin)."
        ));
    }

    let target_name = target_name
        .as_deref()
        .map(crate::naming::derive_target_name)
        .transpose()?;
    let parent_session = parent_session
        .as_deref()
        .map(crate::naming::validate_parent_session)
        .transpose()?;
    if mode == MuxMode::Session && parent_session.is_some() {
        return Err(anyhow!("--parent-session requires window mode"));
    }
    if mode == MuxMode::Session {
        options.target_session_name = target_name;
    } else {
        options.target_window_name = target_name;
        options.window_session_name = parent_session;
    }

    // Validate multi-worktree arguments
    if multi.count.is_some() && multi.agent.len() > 1 {
        return Err(anyhow!(
            "--count can only be used with zero or one --agent, but {} were provided",
            multi.agent.len()
        ));
    }

    if !setup.no_pane_cmds && !sandbox_override && !dry_run {
        for agent in &multi.agent {
            let config = config::Config::load_with_override(Some(agent), config_override)?;
            validate_agent_executable(&config)?;
        }
    }

    // Handle rescue flow early if requested
    if rescue.with_changes && !dry_run {
        let (mut rescue_config, rescue_location) = config::Config::load_with_location(
            multi.agent.first().map(|s| s.as_str()),
            config_override,
        )?;
        if sandbox_override {
            rescue_config.sandbox.enabled = Some(true);
        }
        if rescue_config.sandbox.is_enabled() {
            crate::sandbox::notice::show_once();
        }
        if let Some(layout_name) = &layout {
            resolve_layout(&mut rescue_config, layout_name)?;
        }
        let mux = create_backend(detect_backend());
        let rescue_context = workflow::WorkflowContext::new(rescue_config, mux, rescue_location)?;
        // Derive handle for rescue flow (uses config for naming strategy/prefix)
        let handle =
            crate::naming::derive_handle(branch_name, name.as_deref(), &rescue_context.config)?;
        if handle_rescue_flow(
            branch_name,
            &handle,
            &rescue,
            &rescue_context,
            options.clone(),
            wait,
        )? {
            return Ok(());
        }
    }

    // Use preloaded prompt (from auto-name) OR load it now (standard flow)
    let prompt_template = if let Some(p) = preloaded_prompt {
        Some(p)
    } else {
        load_prompt(&PromptLoadArgs {
            prompt_editor: prompt_args.prompt_editor,
            prompt_inline: prompt_args.prompt.as_deref(),
            prompt_file: prompt_args.prompt_file.as_ref(),
        })?
    };

    // Parse prompt document to extract frontmatter (if applicable)
    let prompt_doc = if let Some(ref prompt_src) = prompt_template {
        // Account for implicit editor usage triggered by auto_name
        let implicit_editor =
            auto_name && prompt_args.prompt.is_none() && prompt_args.prompt_file.is_none();
        let from_editor_or_file = prompt_args.prompt_editor
            || implicit_editor
            || matches!(prompt_src, Prompt::FromFile(_));
        Some(parse_prompt_with_frontmatter(
            prompt_src,
            from_editor_or_file,
        )?)
    } else {
        None
    };

    let has_foreach_in_prompt = prompt_doc
        .as_ref()
        .and_then(|d| d.meta.foreach.as_ref())
        .is_some();

    if has_foreach_in_prompt && !multi.agent.is_empty() {
        return Err(anyhow!(
            "Cannot use --agent when 'foreach' is defined in the prompt frontmatter. \
            These multi-worktree generation methods are mutually exclusive."
        ));
    }

    // Create template environment
    let env = create_template_env();

    // Detect explicit remote branches and extract their base names. Generated names
    // always identify local branches, even when their first component names a remote.
    // Only pass CLI --base to detect_remote_branch; config base_branch should not
    // interfere with remote/fork branch detection.
    let (remote_branch, template_base_name) = if let Some(ref pr_remote) = remote_branch_for_pr {
        (Some(pr_remote.clone()), branch_name.to_string())
    } else if auto_name {
        (None, branch_name.to_string())
    } else if dry_run {
        detect_remote_branch_dry_run(branch_name, cli_base)?
    } else {
        detect_remote_branch(branch_name, cli_base)?
    };
    let resolved_base = if remote_branch.is_some() {
        None
    } else {
        cli_base
    };

    // Determine effective foreach matrix
    let effective_foreach_rows =
        determine_foreach_matrix(&multi, prompt_doc.as_ref(), stdin_lines)?;

    // Generate worktree specifications
    let specs = generate_worktree_specs(
        &template_base_name,
        &multi.agent,
        multi.count,
        effective_foreach_rows.as_deref(),
        &env,
        &multi.branch_template,
    )?;

    if specs.is_empty() {
        return Err(anyhow!("No worktree specifications were generated"));
    }

    let prompt_template_enabled =
        effective_foreach_rows.is_some() || multi.count.is_some() || multi.agent.len() > 1;

    // Validate prompt template variables before proceeding to create worktrees.
    // We use the context from the first spec (variable schema is consistent across specs).
    if prompt_template_enabled
        && let Some(doc) = &prompt_doc
        && let Some(first_spec) = specs.first()
    {
        validate_template_variables(&env, &doc.body, &first_spec.template_context)
            .context("Prompt template uses undefined variables")?;
    }

    // Determine prompt_file_only from CLI flag or config
    let prompt_file_only =
        prompt_args.prompt_file_only || initial_config.prompt_file_only.unwrap_or(false);

    // Create worktrees from specs
    let mut plan = CreationPlan {
        specs: &specs,
        resolved_base,
        remote_branch: remote_branch.as_deref(),
        checkout_ref,
        prompt_doc: prompt_doc.as_ref(),
        options,
        mode_override,
        env: &env,
        explicit_name: name.as_deref(),
        wait,
        dry_run,
        deferred_auto_name,
        max_concurrent: multi.max_concurrent,
        sandbox_override,
        prompt_file_only,
        prompt_template_enabled,
        layout: layout.as_deref(),
        fork_source,
        config_override,
    };
    plan.execute()
}

/// Handle the rescue flow (--with-changes).
/// Returns Ok(true) if rescue flow was handled, Ok(false) if normal flow should continue.
fn handle_rescue_flow(
    branch_name: &str,
    handle: &str,
    rescue: &RescueArgs,
    context: &workflow::WorkflowContext,
    options: SetupOptions,
    wait: bool,
) -> Result<bool> {
    if !rescue.with_changes {
        return Ok(false);
    }

    let result = workflow::create_with_changes(
        branch_name,
        handle,
        rescue.include_untracked,
        rescue.patch,
        context,
        options,
    )
    .context("Failed to move uncommitted changes")?;

    println!(
        "✓ Moved uncommitted changes to new worktree for branch '{}'\n  Worktree: {}\n  Original worktree is now clean",
        result.branch_name,
        result.worktree_path.display()
    );

    if wait {
        if result.mode == MuxMode::Session {
            context
                .mux
                .wait_until_session_closed(&result.mux_target_full_name)?;
        } else {
            context
                .mux
                .wait_until_windows_closed(std::slice::from_ref(&result.mux_target_full_name))?;
        }
    }

    Ok(true)
}

/// Determine the effective foreach matrix from CLI, stdin, or frontmatter.
/// Priority: CLI --foreach > stdin > frontmatter foreach
fn determine_foreach_matrix(
    multi: &MultiArgs,
    prompt_doc: Option<&PromptDocument>,
    stdin_lines: Vec<String>,
) -> Result<Option<Vec<BTreeMap<String, String>>>> {
    let has_stdin = !stdin_lines.is_empty();
    let has_frontmatter_foreach = prompt_doc.and_then(|d| d.meta.foreach.as_ref()).is_some();

    // Stdin conflicts with --foreach
    if has_stdin && multi.foreach.is_some() {
        return Err(anyhow!("Cannot use --foreach when piping input from stdin"));
    }

    // Handle stdin input - converts lines to matrix
    // Supports both plain text (becomes {{ input }}) and JSON lines (each key becomes a variable)
    if has_stdin {
        if has_frontmatter_foreach {
            eprintln!("Warning: stdin input overrides prompt frontmatter 'foreach'");
        }

        let rows = stdin_lines
            .into_iter()
            .map(|line| {
                let mut map = BTreeMap::new();

                // Always set {{ input }} to the raw line
                map.insert(STDIN_INPUT_VAR.to_string(), line.clone());

                // Try to parse as JSON if it looks like an object
                if line.starts_with('{')
                    && let Ok(Value::Object(obj)) = serde_json::from_str(&line)
                {
                    for (k, v) in obj {
                        // Convert JSON values to strings
                        let val_str = match v {
                            Value::String(s) => s,
                            Value::Null => String::new(),
                            other => other.to_string(),
                        };
                        // JSON keys can overwrite {{ input }} if explicitly provided
                        map.insert(k, val_str);
                    }
                }

                map
            })
            .collect();

        return Ok(Some(rows));
    }

    // Fall back to existing CLI/frontmatter logic
    match (
        &multi.foreach,
        prompt_doc.and_then(|d| d.meta.foreach.as_ref()),
    ) {
        (Some(cli_str), Some(_frontmatter_map)) => {
            eprintln!("Warning: --foreach overrides prompt frontmatter");
            Ok(Some(parse_foreach_matrix(cli_str)?))
        }
        (Some(cli_str), None) => Ok(Some(parse_foreach_matrix(cli_str)?)),
        (None, Some(frontmatter_map)) => Ok(Some(foreach_from_frontmatter(frontmatter_map)?)),
        (None, None) => Ok(None),
    }
}

/// Polling interval for checking window status in worker pool mode
const WORKER_POOL_POLL_MS: u64 = 250;

/// Encapsulates all parameters needed for worktree creation.
struct CreationPlan<'a> {
    specs: &'a [WorktreeSpec],
    resolved_base: Option<&'a str>,
    remote_branch: Option<&'a str>,
    checkout_ref: Option<CheckoutRef>,
    prompt_doc: Option<&'a PromptDocument>,
    options: SetupOptions,
    mode_override: Option<MuxMode>,
    env: &'a TemplateEnv,
    explicit_name: Option<&'a str>,
    wait: bool,
    dry_run: bool,
    deferred_auto_name: bool,
    max_concurrent: Option<u32>,
    sandbox_override: bool,
    prompt_file_only: bool,
    prompt_template_enabled: bool,
    layout: Option<&'a str>,
    fork_source: Option<crate::workflow::types::ForkSource>,
    config_override: Option<&'a std::path::Path>,
}

impl<'a> CreationPlan<'a> {
    /// Execute the creation plan, creating all worktrees according to the specs.
    fn execute(&mut self) -> Result<()> {
        self.create_worktrees()
    }

    fn create_worktrees(&mut self) -> Result<()> {
        if self.specs.len() > 1 {
            if self.dry_run {
                println!("Dry run for {} worktrees:", self.specs.len());
            } else {
                println!("Preparing to create {} worktrees...", self.specs.len());
            }
        }

        // Create backend once for all specs
        let mux = create_backend(detect_backend());

        // Track targets for --wait (all created windows/sessions)
        let mut created_targets = Vec::new();
        // Track currently active targets for --max-concurrent
        let mut active_targets: Vec<String> = Vec::new();
        let mode = self.options.mode;
        let mode_override = self.mode_override;

        for (i, spec) in self.specs.iter().enumerate() {
            // Concurrency control: wait for a slot if at limit
            if let Some(limit) = self.max_concurrent {
                let limit = limit as usize;
                // Only enter polling loop if we're at capacity
                if active_targets.len() >= limit {
                    loop {
                        // Filter to only targets that still exist
                        if mode == MuxMode::Session {
                            let live_sessions = mux.get_all_session_names()?;
                            active_targets.retain(|t| live_sessions.contains(t));
                        } else {
                            active_targets = mux.filter_active_windows(&active_targets)?;
                        }
                        if active_targets.len() < limit {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(WORKER_POOL_POLL_MS));
                    }
                }
            }
            // Load config for this specific agent to ensure correct agent resolution
            let (mut config, config_location) =
                config::Config::load_with_location(spec.agent.as_deref(), self.config_override)?;
            if self.sandbox_override {
                config.sandbox.enabled = Some(true);
            }
            if config.sandbox.is_enabled() && !self.dry_run {
                crate::sandbox::notice::show_once();
            }

            // Resolve layout: replace top-level panes with layout's panes
            if let Some(layout_name) = self.layout {
                resolve_layout(&mut config, layout_name)?;
            }

            // Render prompt first (needed for deferred auto-name)
            let rendered_prompt = if let Some(doc) = self.prompt_doc {
                if self.prompt_template_enabled {
                    Some(
                        render_prompt_body(&doc.body, self.env, &spec.template_context)
                            .with_context(|| {
                                format!("Failed to render prompt for spec index {}", i)
                            })?,
                    )
                } else {
                    Some(doc.body.clone())
                }
            } else {
                None
            };

            // If auto-name was deferred, run it now using the rendered prompt
            let final_branch_name = if self.deferred_auto_name {
                generate_branch_name_with_spinner(rendered_prompt.as_deref(), &config)?
            } else {
                spec.branch_name.clone()
            };

            if self.specs.len() > 1 {
                let action = if self.dry_run {
                    "Worktree"
                } else {
                    "Creating worktree"
                };
                println!(
                    "\n--- [{}/{}] {}: {} ---",
                    i + 1,
                    self.specs.len(),
                    action,
                    final_branch_name
                );
            }

            // Derive handle from branch name, optional explicit name, and config
            // For single specs, explicit_name overrides; for multi-specs, it's None (disallowed)
            let handle =
                crate::naming::derive_handle(&final_branch_name, self.explicit_name, &config)?;

            let prompt_for_spec = rendered_prompt.map(Prompt::Inline);

            // For multi-worktree, re-resolve fork source for earlier specs;
            // last spec takes ownership to avoid unnecessary re-resolution.
            let fork_for_spec = if i == self.specs.len() - 1 {
                self.fork_source.take()
            } else if let Some(ref fork) = self.fork_source {
                let agent_name = spec
                    .agent
                    .as_deref()
                    .or(config.agent.as_deref())
                    .unwrap_or("claude");
                crate::multiplexer::conversation::resolve_forker(agent_name).map(|forker| {
                    crate::workflow::types::ForkSource {
                        forker,
                        session: fork.session.clone(),
                    }
                })
            } else {
                None
            };

            // Create a WorkflowContext for this spec's config (reuse shared mux)
            let context = workflow::WorkflowContext::new(config, mux.clone(), config_location)?;
            let configured_base = if self.resolved_base.is_none() && self.remote_branch.is_none() {
                workflow::resolve_configured_base_branch(
                    &context.config,
                    &context.execution_dir,
                    context.vcs.as_ref(),
                )?
            } else {
                None
            };
            let resolved_base = self.resolved_base.or(configured_base.as_deref());

            if self.dry_run {
                print_dry_run(
                    &context,
                    &final_branch_name,
                    &handle,
                    resolved_base,
                    self.remote_branch,
                    &self.options,
                    self.explicit_name.is_some(),
                )?;
                continue;
            }

            super::announce_hooks(
                &context.config,
                Some(&self.options),
                super::HookPhase::PostCreate,
            );

            let result = workflow::create(
                &context,
                workflow::CreateArgs {
                    branch_name: &final_branch_name,
                    handle: &handle,
                    base_branch: resolved_base,
                    remote_branch: self.remote_branch,
                    checkout_ref: self.checkout_ref,
                    prompt: prompt_for_spec.as_ref(),
                    options: self.options.clone(),
                    mode_override,
                    agent: spec.agent.as_deref(),
                    is_explicit_name: self.explicit_name.is_some(),
                    prompt_file_only: self.prompt_file_only,
                    fork_source: fork_for_spec,
                },
            )
            .with_context(|| {
                format!(
                    "Failed to create worktree environment for branch '{}'",
                    final_branch_name
                )
            })?;

            let full_window_name = result.mux_target_full_name.clone();

            if self.wait {
                created_targets.push(full_window_name.clone());
            }

            // Track for concurrency control
            if self.max_concurrent.is_some() {
                active_targets.push(full_window_name);
            }

            if result.post_create_hooks_run > 0 {
                println!("✓ Setup complete");
            }

            println!(
                "✓ Successfully created worktree and tmux {} for '{}'",
                mode_label(mode),
                result.branch_name
            );
            if let Some(ref base) = result.base_branch {
                println!("  Base: {}", base);
            }
            println!("  Worktree: {}", result.worktree_path.display());
        }

        if self.wait && !created_targets.is_empty() {
            if mode == MuxMode::Session {
                // For sessions, wait for each one to close
                for session_name in &created_targets {
                    mux.wait_until_session_closed(session_name)?;
                }
            } else {
                mux.wait_until_windows_closed(&created_targets)?;
            }
        }

        Ok(())
    }
}

fn print_dry_run(
    context: &workflow::WorkflowContext,
    branch_name: &str,
    handle: &str,
    base_branch: Option<&str>,
    remote_branch: Option<&str>,
    options: &SetupOptions,
    is_explicit_name: bool,
) -> Result<()> {
    if context.config.panes.is_some() && context.config.windows.is_some() {
        bail!("Cannot specify both 'panes' and 'windows' in configuration.");
    }
    if let Some(windows) = &context.config.windows {
        if options.mode != MuxMode::Session {
            bail!(
                "'windows' configuration requires 'mode: session'. Either add 'mode: session' to your config or use --session flag."
            );
        }
        crate::config::validate_windows_config(windows)?;
    }
    if let Some(panes) = &context.config.panes {
        crate::config::validate_panes_config(panes)?;
    }

    let branch_exists = git::branch_exists_in(branch_name, Some(&context.execution_dir))?;
    let base = if let Some(remote) = remote_branch {
        remote.to_string()
    } else if branch_exists {
        "existing branch".to_string()
    } else if let Some(base) = base_branch.filter(|base| !base.trim().is_empty()) {
        base.to_string()
    } else {
        git::get_current_branch_in(&context.execution_dir)
            .context("Failed to determine the current branch to use as the base")?
    };

    let base_dir = if let Some(ref worktree_dir) = context.config.worktree_dir {
        crate::util::expand_worktree_dir(worktree_dir, &context.main_worktree_root)?
    } else {
        let project_name = context
            .main_worktree_root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("Could not determine project name"))?;
        context
            .main_worktree_root
            .parent()
            .ok_or_else(|| anyhow!("Could not determine parent directory"))?
            .join(format!("{}__worktrees", project_name))
    };
    let worktree_path = base_dir.join(handle);
    let target_name = options.primary_mux_target_name(handle);
    let target = MuxHandle::new(
        context.mux.as_ref(),
        options.mode,
        &context.prefix,
        target_name,
    );

    println!("Worktree: {}", worktree_path.display());
    println!("Branch:   {}", branch_name);
    println!("Base:     {}", base.trim());
    println!(
        "Target:   {} ({})",
        target.full_name(),
        mode_label(options.mode)
    );
    if is_explicit_name {
        println!("Handle:   {}", handle);
    }

    println!("\nFiles to copy or symlink:");
    if !options.run_file_ops {
        println!("  (disabled)");
    } else {
        let operations = workflow::file_ops::resolve_file_operations(
            &context.config_source_dir,
            &worktree_path,
            &context.config.files,
        )?;
        if operations.is_empty() {
            println!("  (none)");
        }
        for operation in operations {
            let source = operation
                .source
                .strip_prefix(&context.config_source_dir)
                .unwrap_or(&operation.source);
            let destination = operation
                .destination
                .strip_prefix(&worktree_path)
                .unwrap_or(&operation.destination);
            let kind = match operation.kind {
                workflow::file_ops::FileOperationKind::Copy => "copy",
                workflow::file_ops::FileOperationKind::Symlink => "symlink",
            };
            println!(
                "  {} -> {} ({})",
                source.display(),
                destination.display(),
                kind
            );
        }
    }

    println!("\nPost-create hooks:");
    if !options.run_hooks {
        println!("  (disabled)");
    } else if let Some(hooks) = &context.config.post_create {
        if hooks.is_empty() {
            println!("  (none)");
        }
        for hook in hooks {
            println!("  {}", hook);
        }
    } else {
        println!("  (none)");
    }

    Ok(())
}

/// Route `workmux add` through SpawnAgent RPC when running inside a sandbox.
///
/// Only a subset of `add` flags are supported over RPC. Unsupported flags
/// are explicitly rejected with a clear error rather than silently ignored.
#[allow(clippy::too_many_arguments)]
fn run_add_via_rpc(
    branch_name: Option<&str>,
    auto_name: bool,
    prompt_args: &PromptArgs,
    setup: &SetupFlags,
    rescue: &RescueArgs,
    multi: &MultiArgs,
    base: Option<&str>,
    pr: Option<PrReference>,
    name: Option<&str>,
    wait: bool,
    mode_override: Option<MuxMode>,
) -> Result<()> {
    use crate::sandbox::rpc::{RpcClient, RpcRequest, RpcResponse};
    use crate::workflow::prompt_loader::{PromptLoadArgs, load_prompt};

    // --- Validate: reject unsupported flags explicitly ---
    if base.is_some() {
        bail!("--base is not supported from inside a sandbox");
    }
    if pr.is_some() {
        bail!("--pr is not supported from inside a sandbox");
    }
    if name.is_some() {
        bail!("--name is not supported from inside a sandbox");
    }
    if wait {
        bail!("--wait is not supported from inside a sandbox");
    }
    if rescue.with_changes {
        bail!("--with-changes is not supported from inside a sandbox");
    }
    if !multi.agent.is_empty() {
        bail!("--agent is not supported from inside a sandbox (uses host config)");
    }
    if multi.count.is_some() {
        bail!(
            "--count is not supported from inside a sandbox. Call workmux add multiple times instead."
        );
    }
    if multi.foreach.is_some() {
        bail!("--foreach is not supported from inside a sandbox");
    }
    if mode_override.is_some() {
        bail!(
            "Mode overrides (--mode / --session) are not supported from inside a sandbox \
             (host controls mode via its config)"
        );
    }
    if prompt_args.prompt_file_only {
        bail!("--prompt-file-only is not supported from inside a sandbox");
    }

    // --- Resolve prompt via existing loader (handles -p, -P, -e) ---
    let prompt_content = load_prompt(&PromptLoadArgs {
        prompt_editor: prompt_args.prompt_editor,
        prompt_inline: prompt_args.prompt.as_deref(),
        prompt_file: prompt_args.prompt_file.as_ref(),
    })?;
    let prompt_text = match prompt_content {
        Some(Prompt::Inline(text)) => Some(text),
        Some(Prompt::FromFile(path)) => Some(
            std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read prompt file: {}", path.display()))?,
        ),
        None => None,
    };

    // --- Build RPC request ---
    let rpc_branch = if auto_name {
        None
    } else {
        branch_name.map(|s| s.to_string())
    };

    let mut client = RpcClient::from_env().context(
        "Failed to connect to host RPC server. Is this running inside a workmux sandbox?",
    )?;

    let resp = client.call(&RpcRequest::SpawnAgent {
        prompt: prompt_text.unwrap_or_default(),
        branch_name: rpc_branch.clone(),
        background: if setup.background { Some(true) } else { None },
    })?;

    match resp {
        RpcResponse::Ok => {
            let display_name = rpc_branch.as_deref().unwrap_or("(auto-named)");
            println!("✓ Spawned agent: {}", display_name);
            Ok(())
        }
        RpcResponse::Error { message } => {
            bail!("Host failed to spawn agent: {}", message)
        }
        other => bail!("Unexpected RPC response: {:?}", other),
    }
}

#[cfg(test)]
mod agent_validation_tests {
    use super::*;

    fn config(agent: &str) -> config::Config {
        config::Config {
            agent: Some(agent.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn rejects_missing_agent() {
        let error =
            validate_agent_executable(&config("workmux-nonexistent-agent-938472")).unwrap_err();
        assert!(error.to_string().contains("Agent executable"));
    }

    #[test]
    fn checks_absolute_executable_paths() {
        assert!(validate_agent_executable(&config("/bin/sh")).is_ok());
        assert!(validate_agent_executable(&config("/nonexistent/workmux-agent")).is_err());
    }

    #[test]
    fn preserves_shell_commands_and_worktree_relative_paths() {
        for command in [
            "./agent",
            "agent --flag",
            "AGENT_MODE=test agent",
            "$HOME/bin/agent",
        ] {
            assert!(validate_agent_executable(&config(command)).is_ok());
        }
    }

    #[test]
    fn skips_sandbox_agents() {
        let mut config = config("workmux-nonexistent-agent-938472");
        config.sandbox.enabled = Some(true);
        assert!(validate_agent_executable(&config).is_ok());
    }
}
