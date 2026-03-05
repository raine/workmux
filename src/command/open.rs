use crate::command::args::PromptArgs;
use crate::config::MuxMode;
use crate::multiplexer::{create_backend, detect_backend};
use crate::workflow::prompt_loader::{PromptLoadArgs, load_prompt};
use crate::workflow::{SetupOptions, WorkflowContext};
use crate::{config, git, workflow};
use anyhow::{Context, Result, bail};

pub fn run(
    name: Option<&str>,
    run_hooks: bool,
    force_files: bool,
    new_window: bool,
    session: bool,
    prompt_args: PromptArgs,
) -> Result<()> {
    // Resolve the worktree name
    let resolved_name = match (name, new_window) {
        (Some(n), _) => n.to_string(),
        (None, true) => super::resolve_name(None).context(
            "Could not infer current worktree. Run inside a worktree or provide a name.",
        )?,
        (None, false) => bail!("Worktree name is required unless --new is provided"),
    };

    let (config, config_location) = config::Config::load_with_location(None)?;
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

    // Note: final mode resolution happens in workflow::open using the canonical
    // base_handle (which may differ from resolved_name when opening by branch name).
    // We pass a preliminary mode here for SetupOptions; workflow::open will override it.
    let preliminary_mode = if session {
        MuxMode::Session
    } else {
        git::get_worktree_mode(&resolved_name)
    };

    // Load prompt if any prompt argument is provided
    let prompt = load_prompt(&PromptLoadArgs {
        prompt_editor: prompt_args.prompt_editor,
        prompt_inline: prompt_args.prompt.as_deref(),
        prompt_file: prompt_args.prompt_file.as_ref(),
    })?;

    // Write prompt to temp file if provided
    // Use unique filename with timestamp to prevent race condition when opening multiple duplicates
    // Note: We use None for working_dir here because we don't know the worktree path yet
    // (open resolves it later). The temp dir approach works fine for open since sandbox
    // wrapping only happens during initial create, not open.
    let prompt_file_path = if let Some(ref p) = prompt {
        let unique_name = format!(
            "{}-{}",
            resolved_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );
        Some(crate::workflow::write_prompt_file(None, &unique_name, p)?)
    } else {
        None
    };

    // Construct setup options (pane commands always run on open)
    let mut options = SetupOptions::new(run_hooks, force_files, true);
    options.mode = preliminary_mode;
    options.prompt_file_path = prompt_file_path;

    // Only announce hooks if we're forcing a new target (otherwise we might just switch)
    if new_window {
        super::announce_hooks(
            &context.config,
            Some(&options),
            super::HookPhase::PostCreate,
        );
    }

    let result = workflow::open(&resolved_name, &context, options, new_window, session)
        .context("Failed to open worktree environment")?;

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

    Ok(())
}
