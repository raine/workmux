//! Sandbox backends for running agents in isolated environments.

pub(crate) mod clipboard;
mod constant_time;
mod container;
pub mod freshness;
pub mod guest;
pub(crate) mod host_exec_sandbox;
pub mod lima;
pub mod network_proxy;
pub(crate) mod notice;
pub(crate) mod pi;
pub mod rpc;
pub(crate) mod shims;
pub(crate) mod toolchain;

pub use container::CLAUDE_ONBOARDING_JSON;
pub use container::DEFAULT_IMAGE_REGISTRY;
pub use container::DOCKERFILE_BASE;
pub use container::KNOWN_AGENTS;
pub use container::SandboxPaths;
pub(crate) use container::build_docker_run_args;
pub use container::build_image;
pub use container::dockerfile_for_agent;
pub use container::ensure_image_ready;
pub(crate) use container::ensure_sandbox_config_dirs;
pub use container::pull_image;
pub(crate) use container::push_agent_home;
pub use container::stop_containers_for_handle;
pub(crate) use container::validate_git_metadata_boundary;
pub use container::wrap_for_container;
pub use lima::ensure_vm_running as ensure_lima_vm;
pub use lima::wrap_for_lima;
pub(crate) fn proxy_env_vars(
    rpc_host: &str,
    proxy_port: u16,
    proxy_token: &str,
) -> Vec<(String, String)> {
    let proxy_url = format!("http://workmux:{}@{}:{}", proxy_token, rpc_host, proxy_port);
    let no_proxy = format!("localhost,127.0.0.1,{}", rpc_host);

    vec![
        ("HTTPS_PROXY".into(), proxy_url.clone()),
        ("HTTP_PROXY".into(), proxy_url.clone()),
        ("https_proxy".into(), proxy_url.clone()),
        ("http_proxy".into(), proxy_url),
        ("NO_PROXY".into(), no_proxy.clone()),
        ("no_proxy".into(), no_proxy),
        // Pass hostname (not IP literal) so the init script can resolve ALL
        // IPs and whitelist them all in iptables.
        ("WM_PROXY_HOST".into(), rpc_host.to_string()),
        ("WM_PROXY_PORT".into(), proxy_port.to_string()),
    ]
}
