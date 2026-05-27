use crate::multiplexer::handle::mode_label;
use crate::multiplexer::{MuxHandle, WindowTarget, create_backend, detect_backend};
use crate::{config, git, sandbox};
use anyhow::{Context, Result, anyhow};

pub fn run(name: Option<&str>) -> Result<()> {
    if crate::sandbox::guest::is_sandbox_guest() {
        let name_to_close = super::resolve_name(name)?;
        return run_via_rpc(&name_to_close);
    }

    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let prefix = config.window_prefix();

    // Resolve the handle first. When the user passes a branch name that differs
    // from the worktree directory name, find_worktree resolves through both handle
    // and branch lookups, then we extract the true handle from the path basename.
    let resolved_handle = match name {
        Some(n) => {
            let (path, _branch) = git::find_worktree(n).map_err(|_| {
                anyhow!(
                    "Worktree '{}' not found. Use 'workmux list' to see available worktrees.",
                    n
                )
            })?;
            path.file_name()
                .ok_or_else(|| anyhow!("Invalid worktree path: no directory name"))?
                .to_string_lossy()
                .to_string()
        }
        None => super::resolve_name(None)?,
    };

    // Determine if this worktree was created as a session or window
    let mode = git::get_worktree_mode(&resolved_handle);
    let target_name = if mode == crate::config::MuxMode::Session {
        git::get_worktree_target_session(&resolved_handle)
            .unwrap_or_else(|| resolved_handle.clone())
    } else {
        git::get_worktree_target_window(&resolved_handle).unwrap_or_else(|| resolved_handle.clone())
    };
    let window_session = if mode == crate::config::MuxMode::Window {
        git::get_worktree_window_session(&resolved_handle)
    } else {
        None
    };

    // When no name is provided, prefer the current window/session name
    // This handles duplicate windows/sessions (e.g., wm:feature-2) correctly
    let (full_target_name, is_current_target) = match name {
        Some(_) => {
            // Explicit name provided - worktree already validated above
            let target = MuxHandle::new(mux.as_ref(), mode, prefix, &target_name);
            let full = target.full_name();
            let current = target.current_name()?;
            let is_current = if mode == crate::config::MuxMode::Window {
                current.as_deref() == Some(full.as_str())
                    && window_session
                        .as_deref()
                        .is_none_or(|session| mux.current_session().as_deref() == Some(session))
            } else {
                current.as_deref() == Some(full.as_str())
            };
            (full, is_current)
        }
        None => {
            // No name provided - check if we're in a workmux window/session
            let target = MuxHandle::new(mux.as_ref(), mode, prefix, &target_name);
            let current_name = target.current_name()?;
            if let Some(current) = current_name {
                if current.starts_with(prefix) {
                    // We're in a workmux target, use it directly
                    (current.clone(), true)
                } else {
                    // Not in a workmux target, fall back to resolved handle
                    (target.full_name(), false)
                }
            } else {
                // Not in multiplexer, use resolved handle
                (target.full_name(), false)
            }
        }
    };

    let kind = mode_label(mode);
    let window_target = WindowTarget::new(full_target_name.clone(), window_session.clone());
    let target_exists = if mode == crate::config::MuxMode::Window {
        mux.window_target_exists(&window_target)?
    } else {
        MuxHandle::exists_full(mux.as_ref(), mode, &full_target_name)?
    };

    if !target_exists {
        return Err(anyhow!(
            "No active {} found for '{}'. The worktree exists but has no open {}.",
            kind,
            full_target_name,
            kind
        ));
    }

    // Stop any running containers for this worktree before killing the target.
    sandbox::stop_containers_for_handle(&resolved_handle);

    if is_current_target {
        let delay = std::time::Duration::from_millis(100);
        if mode == crate::config::MuxMode::Window {
            MuxHandle::schedule_window_target_close(mux.as_ref(), &window_target, delay)?;
        } else {
            MuxHandle::schedule_close_full(mux.as_ref(), mode, &full_target_name, delay)?;
        }
    } else {
        if mode == crate::config::MuxMode::Window {
            MuxHandle::kill_window_target(mux.as_ref(), &window_target)
                .context("Failed to close target")?;
        } else {
            MuxHandle::kill_full(mux.as_ref(), mode, &full_target_name)
                .context("Failed to close target")?;
        }
        println!("✓ Closed {} '{}' (worktree kept)", kind, full_target_name);
    }

    Ok(())
}

fn run_via_rpc(name: &str) -> Result<()> {
    use crate::sandbox::rpc::{RpcClient, RpcRequest, RpcResponse};
    use std::io::Write;

    let mut client = RpcClient::from_env()?;
    client.send(&RpcRequest::Close {
        name: name.to_string(),
    })?;

    loop {
        let response = client.recv()?;
        match response {
            RpcResponse::Output { message } => {
                print!("{}", message);
                std::io::stdout().flush().ok();
            }
            RpcResponse::Ok => return Ok(()),
            RpcResponse::Error { message } => anyhow::bail!("{}", message),
            other => anyhow::bail!("Unexpected RPC response: {:?}", other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::rpc::{RpcRequest, RpcResponse};
    use crate::test_support;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[test]
    fn sandbox_guest_close_sends_rpc_request() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let token = "close-test-token";
        let server = std::thread::spawn(move || -> Result<String> {
            let (stream, _) = listener.accept()?;
            let mut reader = BufReader::new(stream.try_clone()?);
            let mut writer = stream;

            let mut auth_line = String::new();
            reader.read_line(&mut auth_line)?;
            let auth: serde_json::Value = serde_json::from_str(auth_line.trim())?;
            assert_eq!(auth["token"], json!(token));

            let mut request_line = String::new();
            reader.read_line(&mut request_line)?;
            let request: RpcRequest = serde_json::from_str(request_line.trim())?;
            let name = match request {
                RpcRequest::Close { name } => name,
                other => panic!("Expected Close request, got {:?}", other),
            };

            let mut response = serde_json::to_string(&RpcResponse::Ok)?;
            response.push('\n');
            writer.write_all(response.as_bytes())?;
            Ok(name)
        });

        let tmp = tempfile::tempdir()?;
        let worktree_dir = tmp.path().join("repo__worktrees").join("feature-x");
        std::fs::create_dir_all(&worktree_dir)?;

        let mut process = test_support::process_state()?;
        process.set_current_dir(&worktree_dir)?;
        process.set_env("WM_SANDBOX_GUEST", "1");
        process.set_env("WM_RPC_HOST", "127.0.0.1");
        process.set_env("WM_RPC_PORT", port.to_string());
        process.set_env("WM_RPC_TOKEN", token);

        run(None)?;
        let name = server.join().unwrap()?;
        assert_eq!(name, "feature-x");
        Ok(())
    }
}
