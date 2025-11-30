// Module declarations
mod cleanup;
mod context;
mod create;
mod list;
mod merge;
mod open;
pub mod pr;
pub mod prompt_loader;
mod remove;
mod setup;
pub mod types;

// Public API re-exports
pub use create::{create, create_with_changes};
pub use list::list;
pub use merge::merge;
pub use open::open;
pub use remove::remove;

// Re-export commonly used types for convenience
pub use context::WorkflowContext;
pub use types::{CreateArgs, SetupOptions};
