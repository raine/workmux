// Module declarations
mod agent_resolve;
pub mod cleanup;
mod cleanup_diagnostics;
mod cleanup_retry;
mod cleanup_tree;
mod context;
mod create;
pub mod file_ops;
mod list;
mod merge;
mod open;
pub mod pr;
pub mod prompt_loader;
mod remove;
mod rename;
pub mod resurrect;
mod setup;
pub mod types;

// Public API re-exports
pub use agent_resolve::{
    AgentProject, find_worktree_root, match_agents_to_worktree, resolve_worktree_agent,
    resolve_worktree_agents, resolve_worktree_agents_from_snapshot,
};
pub use cleanup::run_deferred_cleanup_worker;
pub use create::{create, create_headless, create_with_changes};
pub use list::{list, list_in};
pub use merge::{MergeConflict, merge};
pub use open::open;
pub use remove::{fallback_worktree_path, remove, remove_quiet};
pub use rename::rename;
pub use setup::{
    prompt_file_name, provision_environment, resolve_pane_configuration,
    validate_prompt_consumption, write_prompt_file,
};

// Re-export commonly used types for convenience
pub use context::{WorkflowContext, resolve_configured_base_branch};
pub use types::{CreateArgs, SetupOptions};
