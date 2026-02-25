use crate::command::args::PromptArgs;
use crate::config::MuxMode;
use crate::multiplexer::{create_backend, detect_backend};
use crate::workflow::prompt_loader::{PromptLoadArgs, load_prompt};
use crate::workflow::{SetupOptions, WorkflowContext};
use crate::{config, workflow};
use anyhow::{Context, Result, bail};

pub fn run(
    names: &[String],
    run_hooks: bool,
    force_files: bool,
    new_window: bool,
    colors: bool,
    session: bool,
    continue_session: bool,
    prompt_args: PromptArgs,
) -> Result<()> {
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

    let (mut config, config_location) = config::Config::load_with_location(None)?;
    if colors {
        config.window_colors = Some(true);
    }
    let mux = create_backend(detect_backend());
    let context = WorkflowContext::new(config, mux, config_location)?;

    // Validate backend supports session mode
    if session && context.mux.name() != "tmux" {
        bail!(
            "Session mode (--session) is only supported with tmux.\n\
             Current backend: {}. Use window mode instead.",
            context.mux.name()
        );
    }

    let preliminary_mode = if session {
        MuxMode::Session
    } else {
        context.config.mode()
    };

    // Load prompt if any prompt argument is provided
    let prompt = load_prompt(&PromptLoadArgs {
        prompt_editor: prompt_args.prompt_editor,
        prompt_inline: prompt_args.prompt.as_deref(),
        prompt_file: prompt_args.prompt_file.as_ref(),
    })?;

    let prompt_file_only =
        prompt_args.prompt_file_only || context.config.prompt_file_only.unwrap_or(false);

    let mut errors: Vec<(String, anyhow::Error)> = Vec::new();

    for resolved_name in &resolved_names {
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
            session,
            file_only_prompt,
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
