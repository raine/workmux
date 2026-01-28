use crate::config::TmuxTarget;
use crate::prompt::{Prompt, PromptDocument, foreach_from_frontmatter};
use crate::spinner;
use crate::template::{
    TemplateEnv, WorktreeSpec, create_template_env, generate_worktree_specs, parse_foreach_matrix,
    render_prompt_body, validate_template_variables,
};
use crate::workflow::SetupOptions;
use crate::workflow::pr::detect_remote_branch;
use crate::workflow::prompt_loader::{PromptLoadArgs, load_prompt, parse_prompt_with_frontmatter};
use crate::{config, git, tmux, workflow};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read};

// Re-export the arg types that are used by the CLI
pub use super::args::{MultiArgs, PromptArgs, RescueArgs, SetupFlags};

/// Variable name exposed to templates for stdin input lines
const STDIN_INPUT_VAR: &str = "input";

/// Maximum stdin size to read (10MB) to prevent OOM from infinite streams
const STDIN_MAX_BYTES: u64 = 10 * 1024 * 1024;

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

    let generated = spinner::with_spinner("Generating branch name", || {
        crate::llm::generate_branch_name(prompt_text, model, system_prompt)
    })?;
    println!("  Branch: {}", generated);

    Ok(generated)
}

/// Check for and read lines from stdin if available.
fn read_stdin_lines() -> Result<Vec<String>> {
    if std::io::stdin().is_terminal() {
        return Ok(Vec::new());
    }

    let mut buffer = String::new();
    std::io::stdin()
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

/// Check preconditions for the add command (git repo and tmux session).
/// Returns Ok(()) if all preconditions are met, or an error listing all failures.
fn check_preconditions() -> Result<()> {
    let is_git = git::is_git_repo()?;
    let is_tmux = tmux::is_running()?;

    if is_git && is_tmux {
        return Ok(());
    }

    let mut errors = Vec::new();

    if !is_tmux {
        errors.push("tmux is not running.");
    }
    if !is_git {
        errors.push("Current directory is not a git repository.");
    }

    // Add blank line before suggestions
    errors.push("");

    if !is_tmux {
        errors.push("Please start a tmux session first.");
    }
    if !is_git {
        errors.push("Please run this command from within a git repository.");
    }

    Err(anyhow!(errors.join("\n")))
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    branch_name: Option<&str>,
    pr: Option<u32>,
    auto_name: bool,
    base: Option<&str>,
    name: Option<String>,
    prompt_args: PromptArgs,
    setup: SetupFlags,
    rescue: RescueArgs,
    multi: MultiArgs,
    wait: bool,
    session: bool,
) -> Result<()> {
    // Ensure preconditions are met (git repo and tmux session)
    check_preconditions()?;

    // Load config early to determine target (CLI flag overrides config)
    let initial_config = config::Config::load(multi.agent.first().map(|s| s.as_str()))?;
    let target = if session {
        TmuxTarget::Session
    } else {
        initial_config.target
    };

    // Construct setup options from flags
    let mut options = SetupOptions::new(!setup.no_hooks, !setup.no_file_ops, !setup.no_pane_cmds);
    options.focus_window = !setup.background;
    options.target = target;

    // Detect stdin input early
    let stdin_lines = read_stdin_lines()?;
    let has_stdin = !stdin_lines.is_empty();

    // Determine if we're in explicit multi-worktree mode (before loading prompt)
    let is_explicit_multi =
        has_stdin || multi.foreach.is_some() || multi.count.is_some() || multi.agent.len() > 1;

    // Handle auto-name: load prompt first, generate branch name
    // In multi-worktree mode with auto-name, we defer LLM generation to the loop
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
                let config = config::Config::load(multi.agent.first().map(|s| s.as_str()))?;
                let generated = generate_branch_name_with_spinner(Some(&prompt_text), &config)?;
                (generated, Some(prompt), None, false)
            }
        } else if let Some(pr_number) = pr {
            // Handle PR checkout if --pr flag is provided
            let result = workflow::pr::resolve_pr_ref(pr_number, branch_name)?;
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
    let base = if remote_branch_for_pr.is_some() {
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

    // Handle rescue flow early if requested
    if rescue.with_changes {
        let (rescue_config, rescue_location) =
            config::Config::load_with_location(multi.agent.first().map(|s| s.as_str()))?;
        let rescue_context = workflow::WorkflowContext::new(rescue_config, rescue_location)?;
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

    // Validate multi-worktree arguments
    if multi.count.is_some() && multi.agent.len() > 1 {
        return Err(anyhow!(
            "--count can only be used with zero or one --agent, but {} were provided",
            multi.agent.len()
        ));
    }

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

    // Detect remote branch and extract base name
    // If we have a PR remote branch, use that; otherwise detect from branch_name
    let (remote_branch, template_base_name) = if let Some(ref pr_remote) = remote_branch_for_pr {
        (Some(pr_remote.clone()), branch_name.to_string())
    } else {
        detect_remote_branch(branch_name, base)?
    };
    let resolved_base = if remote_branch.is_some() { None } else { base };

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

    // Validate prompt template variables before proceeding to create worktrees.
    // We use the context from the first spec (variable schema is consistent across specs).
    if let Some(doc) = &prompt_doc
        && let Some(first_spec) = specs.first()
    {
        validate_template_variables(&env, &doc.body, &first_spec.template_context)
            .context("Prompt template uses undefined variables")?;
    }

    // Create worktrees from specs
    let plan = CreationPlan {
        specs: &specs,
        resolved_base,
        remote_branch: remote_branch.as_deref(),
        prompt_doc: prompt_doc.as_ref(),
        options,
        env: &env,
        explicit_name: name.as_deref(),
        wait,
        deferred_auto_name,
        max_concurrent: multi.max_concurrent,
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
        let full_window_name = tmux::prefixed(&context.prefix, handle);
        tmux::wait_until_windows_closed(&[full_window_name])?;
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
    prompt_doc: Option<&'a PromptDocument>,
    options: SetupOptions,
    env: &'a TemplateEnv,
    explicit_name: Option<&'a str>,
    wait: bool,
    deferred_auto_name: bool,
    max_concurrent: Option<u32>,
}

impl<'a> CreationPlan<'a> {
    /// Execute the creation plan, creating all worktrees according to the specs.
    fn execute(&self) -> Result<()> {
        self.create_worktrees()
    }

    fn create_worktrees(&self) -> Result<()> {
        if self.specs.len() > 1 {
            println!("Preparing to create {} worktrees...", self.specs.len());
        }

        // Track windows for --wait (all created windows)
        let mut created_windows = Vec::new();
        // Track currently active windows for --max-concurrent
        let mut active_windows: Vec<String> = Vec::new();

        for (i, spec) in self.specs.iter().enumerate() {
            // Concurrency control: wait for a slot if at limit
            if let Some(limit) = self.max_concurrent {
                let limit = limit as usize;
                // Only enter polling loop if we're at capacity
                if active_windows.len() >= limit {
                    loop {
                        active_windows = tmux::filter_active_windows(&active_windows)?;
                        if active_windows.len() < limit {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(WORKER_POOL_POLL_MS));
                    }
                }
            }
            // Load config for this specific agent to ensure correct agent resolution
            let (config, config_location) =
                config::Config::load_with_location(spec.agent.as_deref())?;

            // Render prompt first (needed for deferred auto-name)
            let rendered_prompt = if let Some(doc) = self.prompt_doc {
                Some(
                    render_prompt_body(&doc.body, self.env, &spec.template_context)
                        .with_context(|| format!("Failed to render prompt for spec index {}", i))?,
                )
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
                println!(
                    "\n--- [{}/{}] Creating worktree: {} ---",
                    i + 1,
                    self.specs.len(),
                    final_branch_name
                );
            }

            // Derive handle from branch name, optional explicit name, and config
            // For single specs, explicit_name overrides; for multi-specs, it's None (disallowed)
            let handle =
                crate::naming::derive_handle(&final_branch_name, self.explicit_name, &config)?;

            let prompt_for_spec = rendered_prompt.map(Prompt::Inline);

            super::announce_hooks(&config, Some(&self.options), super::HookPhase::PostCreate);

            // Create a WorkflowContext for this spec's config
            let context = workflow::WorkflowContext::new(config, config_location)?;

            // Calculate window name for tracking
            let full_window_name = tmux::prefixed(&context.prefix, &handle);

            if self.wait {
                created_windows.push(full_window_name.clone());
            }

            // Track for concurrency control
            if self.max_concurrent.is_some() {
                active_windows.push(full_window_name);
            }

            let result = workflow::create(
                &context,
                workflow::CreateArgs {
                    branch_name: &final_branch_name,
                    handle: &handle,
                    base_branch: self.resolved_base,
                    remote_branch: self.remote_branch,
                    prompt: prompt_for_spec.as_ref(),
                    options: self.options.clone(),
                    agent: spec.agent.as_deref(),
                },
            )
            .with_context(|| {
                format!(
                    "Failed to create worktree environment for branch '{}'",
                    final_branch_name
                )
            })?;

            if result.post_create_hooks_run > 0 {
                println!("✓ Setup complete");
            }

            let tmux_type = match self.options.target {
                TmuxTarget::Session => "session",
                TmuxTarget::Window => "window",
            };
            println!(
                "✓ Successfully created worktree and tmux {} for '{}'",
                tmux_type, result.branch_name
            );
            if let Some(ref base) = result.base_branch {
                println!("  Base: {}", base);
            }
            println!("  Worktree: {}", result.worktree_path.display());
        }

        if self.wait && !created_windows.is_empty() {
            tmux::wait_until_windows_closed(&created_windows)?;
        }

        Ok(())
    }
}
