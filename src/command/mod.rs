pub mod add;
pub mod args;
pub mod list;
pub mod merge;
pub mod open;
pub mod path;
pub mod remove;
pub mod set_window_status;

use crate::{config::Config, git, workflow::SetupOptions};
use anyhow::{Context, Result};

/// Represents the different phases where hooks can be executed
pub enum HookPhase {
    PostCreate,
    PreDelete,
}

/// Announce that hooks are about to run, if applicable.
/// Returns true if the announcement was printed (hooks will run).
pub fn announce_hooks(config: &Config, options: Option<&SetupOptions>, phase: HookPhase) -> bool {
    match phase {
        HookPhase::PostCreate => {
            let should_run = options.is_some_and(|opts| opts.run_hooks)
                && config.post_create.as_ref().is_some_and(|v| !v.is_empty());

            if should_run {
                println!("Running setup commands...");
            }
            should_run
        }
        HookPhase::PreDelete => {
            let should_run = config.pre_delete.as_ref().is_some_and(|v| !v.is_empty());

            if should_run {
                println!("Running pre-delete commands...");
            }
            should_run
        }
    }
}

/// Resolve the branch name from CLI argument or current branch.
/// Note: Must be called BEFORE workflow operations that change CWD (like merge/remove).
pub fn resolve_branch(arg: Option<&str>, operation: &str) -> Result<String> {
    match arg {
        Some(name) => Ok(name.to_string()),
        None => git::get_current_branch()
            .with_context(|| format!("Failed to get current branch for {} operation", operation)),
    }
}
