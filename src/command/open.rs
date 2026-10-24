use crate::command::args::PromptArgs;
use crate::config::MuxMode;
use crate::multiplexer::{create_backend, detect_backend};
use crate::remote::cache::WorkspaceRecord;
use crate::workflow::prompt_loader::{PromptLoadArgs, load_prompt};
use crate::workflow::{SetupOptions, WorkflowContext};
use crate::{config, workflow};
use anyhow::{Context, Result, bail};

#[allow(clippy::too_many_arguments)]
pub fn run(
    names: &[String],
    run_hooks: bool,
    force_files: bool,
    new_window: bool,
    mode_override: Option<MuxMode>,
    target_name: Option<String>,
    parent_session: Option<String>,
    continue_session: bool,
    prompt_args: PromptArgs,
    config_override: Option<&std::path::Path>,
) -> Result<()> {
    if crate::sandbox::guest::is_sandbox_guest() && config_override.is_some() {
        bail!("--config is not supported from inside a sandbox");
    }
    if crate::sandbox::guest::is_sandbox_guest() && target_name.is_some() {
        bail!("--target-name is not supported from inside a sandbox");
    }
    if crate::sandbox::guest::is_sandbox_guest() && parent_session.is_some() {
        bail!("--parent-session is not supported from inside a sandbox");
    }

    // Resolve names: use provided names, or infer from current directory with --new
    let resolved_names: Vec<String> = if names.is_empty() {
        if new_window {
            let inferred = super::resolve_name(None).context(
                "Could not infer current worktree. Run inside a worktree or provide a name.",
            )?;
            vec![inferred]
        } else {
            bail!("Worktree name is required unless --new is provided")
        }
    } else {
        names.to_vec()
    };

    // Disallow prompt args when opening multiple worktrees
    if resolved_names.len() > 1 && prompt_args.has_any() {
        bail!("Prompt arguments (-p, -P, -e) cannot be used when opening multiple worktrees");
    }
    if resolved_names.len() > 1 && (target_name.is_some() || parent_session.is_some()) {
        bail!("--target-name and --parent-session cannot be used when opening multiple worktrees");
    }

    let target_name = target_name
        .as_deref()
        .map(crate::naming::derive_target_name)
        .transpose()?;
    let parent_session = parent_session
        .as_deref()
        .map(crate::naming::validate_parent_session)
        .transpose()?;

    let remote_records: Vec<Option<WorkspaceRecord>> = resolved_names
        .iter()
        .map(|name| crate::remote::cache::find_here(name))
        .collect();
    if let Some(record) = remote_records.iter().flatten().next() {
        let unsupported = [
            (prompt_args.has_any(), "Prompt arguments (-p, -P, -e)"),
            (continue_session, "--continue"),
            (new_window, "--new"),
            (run_hooks, "--run-hooks"),
            (force_files, "--force-files"),
            (target_name.is_some(), "--target-name"),
            (parent_session.is_some(), "--parent-session"),
            (mode_override == Some(MuxMode::Session), "Session mode"),
        ];
        if let Some((_, option)) = unsupported.iter().find(|(set, _)| *set) {
            bail!(
                "{option} cannot be used with remote workspace '{}' on '{}'",
                record.handle,
                record.remote
            );
        }
    }

    let (config, config_location) = config::Config::load_with_location(None, config_override)?;
    let mux = create_backend(detect_backend());
    let context = WorkflowContext::new(config, mux, config_location)?;

    let preliminary_mode = context.config.mode();

    if new_window && mode_override == Some(MuxMode::Session) {
        bail!("--new is not supported in session mode. Each worktree can only have one session.");
    }

    // Load prompt if any prompt argument is provided
    let prompt = load_prompt(&PromptLoadArgs {
        prompt_editor: prompt_args.prompt_editor,
        prompt_inline: prompt_args.prompt.as_deref(),
        prompt_file: prompt_args.prompt_file.as_ref(),
    })?;

    let prompt_file_only =
        prompt_args.prompt_file_only || context.config.prompt_file_only.unwrap_or(false);

    let mut errors: Vec<(String, anyhow::Error)> = Vec::new();

    for (resolved_name, remote_record) in resolved_names.iter().zip(remote_records) {
        if let Some(record) = remote_record {
            if let Err(e) = crate::remote::lifecycle::open(&context, record, true) {
                eprintln!("✗ {:#}", e);
                errors.push((resolved_name.clone(), e));
            }
            continue;
        }

        // Write prompt to temp file if provided (unique per worktree).
        // In file-only mode, skip writing here; the prompt is passed to
        // workflow::open which writes to the worktree before pane setup.
        let prompt_file_path = if let Some(ref p) = prompt {
            if prompt_file_only {
                None
            } else {
                let unique_name = format!(
                    "{}-{}",
                    resolved_name,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                );
                Some(crate::workflow::write_prompt_file(None, &unique_name, p)?)
            }
        } else {
            None
        };

        // Construct setup options (pane commands always run on open)
        let mut options = SetupOptions::new(run_hooks, force_files, true);
        options.mode = preliminary_mode;
        options.prompt_file_path = prompt_file_path;
        if continue_session {
            options.resume_mode = crate::multiplexer::types::ResumeMode::Continue;
        }
        options.target_window_name = target_name.clone();
        options.target_session_name = target_name.clone();
        options.window_session_name = parent_session.clone();

        // Only announce hooks if we're forcing a new target (otherwise we might just switch)
        if new_window {
            super::announce_hooks(
                &context.config,
                Some(&options),
                super::HookPhase::PostCreate,
            );
        }

        // In file-only mode, pass the prompt to workflow::open so it can write the
        // file before pane commands start (avoids race with editor startup).
        let file_only_prompt = if prompt_file_only {
            prompt.as_ref()
        } else {
            None
        };

        match workflow::open(
            resolved_name,
            &context,
            options,
            new_window,
            mode_override,
            file_only_prompt,
            None,
        ) {
            Ok(result) => {
                let target_type = match result.mode {
                    MuxMode::Session => "session",
                    MuxMode::Window => "window",
                };

                if result.did_switch {
                    println!(
                        "✓ Switched to existing tmux {} for '{}'\n  Worktree: {}",
                        target_type,
                        resolved_name,
                        result.worktree_path.display()
                    );
                } else {
                    if result.post_create_hooks_run > 0 {
                        println!("✓ Setup complete");
                    }

                    println!(
                        "✓ Opened tmux {} for '{}'\n  Worktree: {}",
                        target_type,
                        resolved_name,
                        result.worktree_path.display()
                    );
                }
            }
            Err(e) => {
                eprintln!("✗ {:#}", e);
                errors.push((resolved_name.clone(), e));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else if resolved_names.len() == 1 {
        // Single worktree: error already printed, just exit
        std::process::exit(1);
    } else if errors.len() == resolved_names.len() {
        bail!("Failed to open all {} worktrees", errors.len())
    } else {
        bail!(
            "Failed to open {} of {} worktrees",
            errors.len(),
            resolved_names.len()
        )
    }
}
