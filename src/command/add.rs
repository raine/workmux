use crate::prompt::{
    Prompt, PromptDocument, PromptMetadata, foreach_from_frontmatter, parse_prompt_document,
};
use crate::template::{
    TemplateEnv, WorktreeSpec, create_template_env, generate_worktree_specs, parse_foreach_matrix,
    render_prompt_body,
};
use crate::workflow::SetupOptions;
use crate::{config, git, workflow};
use anyhow::{Context, Result, anyhow};
use edit::Builder;
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
    let (final_branch_name, remote_branch_for_pr, resolved_base_for_pr) =
        if let Some(pr_number) = pr {
            handle_pr_checkout(pr_number, branch_name)?
        } else {
            // Normal flow: use provided branch name and base
            (
                branch_name
                    .expect("branch_name required when --pr not provided")
                    .to_string(),
                None,
                base,
            )
        };

    // Use the determined branch name and override base/remote_branch if from PR
    let branch_name = &final_branch_name;
    let base = if remote_branch_for_pr.is_some() {
        resolved_base_for_pr
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
    let prompt_template = load_prompt(&prompt_args)?;

    // Parse prompt document to extract frontmatter (if applicable)
    let prompt_doc = if let Some(ref prompt_src) = prompt_template {
        // Parse frontmatter from file or editor content, but skip for inline prompts
        // that didn't come from the editor (those are pure strings from -p flag)
        let should_parse_frontmatter =
            prompt_args.prompt_editor || matches!(prompt_src, Prompt::FromFile(_));

        if should_parse_frontmatter {
            Some(parse_prompt_document(prompt_src)?)
        } else {
            // Inline prompt without editor: no frontmatter parsing
            Some(PromptDocument {
                body: match prompt_src {
                    Prompt::Inline(s) => s.clone(),
                    Prompt::FromFile(_) => unreachable!(),
                },
                meta: PromptMetadata::default(),
            })
        }
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

/// Load prompt from the provided arguments (editor, inline, or file).
fn load_prompt(prompt_args: &PromptArgs) -> Result<Option<Prompt>> {
    if prompt_args.prompt_editor {
        let mut builder = Builder::new();
        builder.suffix(".md");
        let editor_content = edit::edit_with_builder("", &builder)
            .context("Failed to open editor or read content")?;
        let trimmed = editor_content.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("Aborting: prompt is empty"));
        }
        Ok(Some(Prompt::Inline(trimmed.to_string())))
    } else {
        Ok(
            match (
                prompt_args.prompt.as_ref(),
                prompt_args.prompt_file.as_ref(),
            ) {
                (Some(inline), None) => Some(Prompt::Inline(inline.clone())),
                (None, Some(path)) => Some(Prompt::FromFile(path.clone())),
                (None, None) => None,
                _ => None, // clap enforces exclusivity; this is unreachable
            },
        )
    }
}

/// Detect if branch_name is a remote ref and extract the base name.
/// Handles both "remote/branch" format and "owner:branch" (GitHub fork) format.
/// Returns (remote_branch, template_base_name).
fn detect_remote_branch(branch_name: &str, base: Option<&str>) -> Result<(Option<String>, String)> {
    // 1. Check for owner:branch syntax (GitHub fork format, e.g., "someuser:feature-a")
    if let Some(fork_spec) = git::parse_fork_branch_spec(branch_name) {
        if base.is_some() {
            return Err(anyhow!(
                "Cannot use --base with 'owner:branch' syntax. \
                The branch '{}' from '{}' will be used as the base.",
                fork_spec.branch,
                fork_spec.owner
            ));
        }

        return handle_fork_branch_checkout(&fork_spec);
    }

    // 2. Existing remote/branch detection (e.g., "origin/feature")
    let remotes = git::list_remotes().context("Failed to list git remotes")?;
    let detected_remote = remotes
        .iter()
        .find(|r| branch_name.starts_with(&format!("{}/", r)));

    if let Some(remote_name) = detected_remote {
        if base.is_some() {
            return Err(anyhow!(
                "Cannot use --base with a remote branch reference. \
                The remote branch '{}' will be used as the base.",
                branch_name
            ));
        }

        let spec = git::parse_remote_branch_spec(branch_name)
            .context("Invalid remote branch format. Use <remote>/<branch>")?;

        if spec.remote != *remote_name {
            return Err(anyhow!("Mismatched remote detection"));
        }

        Ok((Some(branch_name.to_string()), spec.branch))
    } else {
        Ok((None, branch_name.to_string()))
    }
}

/// Handle checkout of a fork branch specified as "owner:branch".
/// Sets up the fork remote, fetches, and optionally displays PR info.
fn handle_fork_branch_checkout(
    fork_spec: &git::ForkBranchSpec,
) -> Result<(Option<String>, String)> {
    use crate::github;

    // Try to find an associated PR and display info (optional, non-blocking)
    if let Ok(Some(pr)) = github::find_pr_by_head_ref(&fork_spec.owner, &fork_spec.branch) {
        let state_suffix = match pr.state.as_str() {
            "OPEN" if pr.is_draft => " (draft)",
            "OPEN" => "",
            "MERGED" => " (merged)",
            "CLOSED" => " (closed)",
            _ => "",
        };
        println!("PR #{}: {}{}", pr.number, pr.title, state_suffix);
    }

    // Ensure the fork remote exists
    let remote_name = git::ensure_fork_remote(&fork_spec.owner)?;

    // Fetch to get the latest refs
    println!(
        "Fetching branch '{}' from '{}'...",
        fork_spec.branch, remote_name
    );
    git::fetch_remote(&remote_name)
        .with_context(|| format!("Failed to fetch from remote '{}'", remote_name))?;

    // Verify the branch exists on the remote
    let remote_ref = format!("{}/{}", remote_name, fork_spec.branch);
    if !git::branch_exists(&remote_ref)? {
        return Err(anyhow!(
            "Branch '{}' not found on remote '{}' (fork of {})",
            fork_spec.branch,
            remote_name,
            fork_spec.owner
        ));
    }

    Ok((Some(remote_ref), fork_spec.branch.clone()))
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
            &spec.branch_name,
            &handle,
            resolved_base,
            remote_branch,
            prompt_for_spec.as_ref(),
            &context,
            options.clone(),
            spec.agent.as_deref(),
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

/// Handle PR checkout: fetch PR details, setup remote, and return branch info
/// Returns (local_branch_name, remote_branch, base)
fn handle_pr_checkout(
    pr_number: u32,
    custom_branch_name: Option<&str>,
) -> Result<(String, Option<String>, Option<&'static str>)> {
    use crate::github;

    // Fetch PR details
    println!("Fetching PR #{}...", pr_number);
    let pr_details = github::get_pr_details(pr_number)
        .with_context(|| format!("Failed to fetch details for PR #{}", pr_number))?;

    // Display PR information
    println!("PR #{}: {}", pr_number, pr_details.title);
    println!("Author: {}", pr_details.author.login);
    println!("Branch: {}", pr_details.head_ref_name);

    // Warn about PR state
    if pr_details.state != "OPEN" {
        eprintln!(
            "⚠️  Warning: PR #{} is {}. Proceeding with checkout...",
            pr_number, pr_details.state
        );
    }
    if pr_details.is_draft {
        eprintln!("⚠️  Warning: PR #{} is a DRAFT.", pr_number);
    }

    // Determine local branch name
    // Match gh pr checkout behavior: default to the PR's actual branch name
    let local_branch_name = if let Some(custom) = custom_branch_name {
        custom.to_string()
    } else {
        pr_details.head_ref_name.clone()
    };

    // Determine if this is a fork PR and ensure remote exists
    let current_repo_owner =
        git::get_repo_owner().context("Failed to determine repository owner from origin remote")?;

    let remote_name = if pr_details.is_fork(&current_repo_owner) {
        let fork_owner = &pr_details.head_repository_owner.login;
        git::ensure_fork_remote(fork_owner)?
    } else {
        "origin".to_string()
    };

    // Fetch the PR branch
    println!(
        "Fetching branch '{}' from '{}'...",
        pr_details.head_ref_name, remote_name
    );
    git::fetch_remote(&remote_name)
        .with_context(|| format!("Failed to fetch from remote '{}'", remote_name))?;

    // Return the branch info
    let remote_branch = format!("{}/{}", remote_name, pr_details.head_ref_name);
    Ok((local_branch_name, Some(remote_branch), None))
}
