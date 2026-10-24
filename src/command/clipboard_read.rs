//! Read clipboard image from host via RPC and shared filesystem.
//! Used by wl-paste/xclip shims inside the sandbox.

use anyhow::{Result, bail};
use std::io::Write;

use crate::sandbox::rpc::{RpcClient, RpcRequest, RpcResponse};

/// Read clipboard image and write raw bytes to stdout.
/// Returns exit code (0 = success, 1 = no image).
pub fn run(mime: &str) -> Result<i32> {
    if !crate::sandbox::guest::is_sandbox_guest() {
        bail!("clipboard-read only works inside a sandbox guest (WM_SANDBOX_GUEST=1)");
    }

    let mut client = RpcClient::from_env()?;
    client.send(&RpcRequest::ClipboardRead {
        mime: mime.to_string(),
    })?;

    let response = client.recv()?;
    match response {
        RpcResponse::ClipboardData { path } => {
            let file_path = std::path::Path::new(&path);

            // Read binary data from shared worktree
            let bytes = match std::fs::read(file_path) {
                Ok(b) => b,
                Err(_) => return Ok(1),
            };

            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&bytes)?;
            stdout.flush()?;

            // Best-effort cleanup
            let _ = std::fs::remove_file(file_path);

            Ok(0)
        }
        RpcResponse::Error { message } => {
            eprintln!("workmux: {message}");
            Ok(1)
        }
        _ => Ok(1),
    }
}
