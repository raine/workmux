//! Sandbox backends for running agents in isolated environments.

pub(crate) mod clipboard;
mod container;
pub mod freshness;
pub mod guest;
pub(crate) mod host_exec_sandbox;
pub mod lima;
pub mod network_proxy;
pub(crate) mod pi;
pub mod rpc;
pub(crate) mod shims;
pub(crate) mod toolchain;

pub use container::DEFAULT_IMAGE_REGISTRY;
pub use container::DOCKERFILE_BASE;
pub use container::KNOWN_AGENTS;
pub(crate) use container::build_docker_run_args;
pub use container::build_image;
pub use container::dockerfile_for_agent;
pub use container::ensure_image_ready;
pub(crate) use container::ensure_sandbox_config_dirs;
pub use container::pull_image;
pub use container::stop_containers_for_handle;
pub use container::wrap_for_container;
pub use lima::ensure_vm_running as ensure_lima_vm;
pub use lima::wrap_for_lima;
