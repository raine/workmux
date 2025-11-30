use crate::prompt::{Prompt, PromptDocument, foreach_from_frontmatter};
use crate::template::{
    TemplateEnv, WorktreeSpec, create_template_env, generate_worktree_specs, parse_foreach_matrix,
    render_prompt_body,
};
use crate::workflow::SetupOptions;
use crate::workflow::pr::detect_remote_branch;
use crate::workflow::prompt_loader::{PromptLoadArgs, load_prompt, parse_prompt_with_frontmatter};
use crate::{config, workflow};
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;

// Re-export the arg types that are used by the CLI
pub use super::args::{MultiArgs, PromptArgs, RescueArgs, SetupFlags};

#[allow(clippy::too_many_arguments)]
pub fn run(
    branch_name: Option<&str>,
    pr: Option<u32>,
    base: Option<&str>,
    name: Option<String>,
    prompt_args: PromptArgs,
    setup: SetupFlags,
    rescue: RescueArgs,
    multi: MultiArgs,
) -> Result<()> {
    // Construct setup options from flags
    let mut options = SetupOptions::new(!setup.no_hooks, !setup.no_file_ops, !setup.no_pane_cmds);
    options.focus_window = !setup.background;

    // Handle PR checkout if --pr flag is provided
    let (final_branch_name, remote_branch_for_pr) = if let Some(pr_number) = pr {
        let result = workflow::pr::resolve_pr_ref(pr_number, branch_name)?;
        (result.local_branch, Some(result.remote_branch))
    } else {
        // Normal flow: use provided branch name
        (
            branch_name
                .expect("branch_name required when --pr not provided")
                .to_string(),
            None,
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
    let has_multi_worktree =
        multi.agent.len() > 1 || multi.count.is_some_and(|c| c > 1) || multi.foreach.is_some();
    if name.is_some() && has_multi_worktree {
        return Err(anyhow!(
            "--name cannot be used with multi-worktree generation (multiple --agent, --count, or --foreach).\n\
             Use the default naming or set worktree_naming/worktree_prefix in config instead."
        ));
    }

    // Handle rescue flow early if requested
    if rescue.with_changes {
        let rescue_config = config::Config::load(multi.agent.first().map(|s| s.as_str()))?;
        let rescue_context = workflow::WorkflowContext::new(rescue_config)?;
        // Derive handle for rescue flow (uses config for naming strategy/prefix)
        let handle =
            crate::naming::derive_handle(branch_name, name.as_deref(), &rescue_context.config)?;
        if handle_rescue_flow(
            branch_name,
            &handle,
            &rescue,
            &rescue_context,
            options.clone(),
        )? {
            return Ok(());
        }
    }

    // Load prompt from arguments
    let prompt_template = load_prompt(&PromptLoadArgs {
        prompt_editor: prompt_args.prompt_editor,
        prompt_inline: prompt_args.prompt.as_deref(),
        prompt_file: prompt_args.prompt_file.as_ref(),
    })?;

    // Parse prompt document to extract frontmatter (if applicable)
    let prompt_doc = if let Some(ref prompt_src) = prompt_template {
        let from_editor_or_file =
            prompt_args.prompt_editor || matches!(prompt_src, Prompt::FromFile(_));
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
    let effective_foreach_rows = determine_foreach_matrix(&multi, prompt_doc.as_ref())?;

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

    // Create worktrees from specs
    create_worktrees_from_specs(
        &specs,
        resolved_base,
        remote_branch.as_deref(),
        prompt_doc.as_ref(),
        options,
        &env,
        name.as_deref(),
    )
}

/// Handle the rescue flow (--with-changes).
/// Returns Ok(true) if rescue flow was handled, Ok(false) if normal flow should continue.
fn handle_rescue_flow(
    branch_name: &str,
    handle: &str,
    rescue: &RescueArgs,
    context: &workflow::WorkflowContext,
    options: SetupOptions,
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

    Ok(true)
}

/// Determine the effective foreach matrix from CLI or frontmatter.
fn determine_foreach_matrix(
    multi: &MultiArgs,
    prompt_doc: Option<&PromptDocument>,
) -> Result<Option<Vec<BTreeMap<String, String>>>> {
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

/// Create worktrees from the provided specs.
fn create_worktrees_from_specs(
    specs: &[WorktreeSpec],
    resolved_base: Option<&str>,
    remote_branch: Option<&str>,
    prompt_doc: Option<&PromptDocument>,
    options: SetupOptions,
    env: &TemplateEnv,
    explicit_name: Option<&str>,
) -> Result<()> {
    if specs.len() > 1 {
        println!("Preparing to create {} worktrees...", specs.len());
    }

    for (i, spec) in specs.iter().enumerate() {
        if specs.len() > 1 {
            println!(
                "\n--- [{}/{}] Creating worktree: {} ---",
                i + 1,
                specs.len(),
                spec.branch_name
            );
        }

        // Load config for this specific agent to ensure correct agent resolution
        let config = config::Config::load(spec.agent.as_deref())?;

        // Derive handle from branch name, optional explicit name, and config
        // For single specs, explicit_name overrides; for multi-specs, it's None (disallowed)
        let handle = crate::naming::derive_handle(&spec.branch_name, explicit_name, &config)?;

        let prompt_for_spec = if let Some(doc) = prompt_doc {
            Some(Prompt::Inline(
                render_prompt_body(&doc.body, env, &spec.template_context).with_context(|| {
                    format!("Failed to render prompt for branch '{}'", spec.branch_name)
                })?,
            ))
        } else {
            None
        };

        super::announce_hooks(&config, Some(&options), super::HookPhase::PostCreate);

        // Create a WorkflowContext for this spec's config
        let context = workflow::WorkflowContext::new(config)?;

        let result = workflow::create(
            &context,
            workflow::CreateArgs {
                branch_name: &spec.branch_name,
                handle: &handle,
                base_branch: resolved_base,
                remote_branch,
                prompt: prompt_for_spec.as_ref(),
                options: options.clone(),
                agent: spec.agent.as_deref(),
            },
        )
        .with_context(|| {
            format!(
                "Failed to create worktree environment for branch '{}'",
                spec.branch_name
            )
        })?;

        if result.post_create_hooks_run > 0 {
            println!("✓ Setup complete");
        }

        println!(
            "✓ Successfully created worktree and tmux window for '{}'",
            result.branch_name
        );
        if let Some(ref base) = result.base_branch {
            println!("  Base: {}", base);
        }
        println!("  Worktree: {}", result.worktree_path.display());
    }

    Ok(())
}
