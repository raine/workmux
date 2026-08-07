use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

// Keep this aligned with Zellij's CLIENT_SERVER_CONTRACT_VERSION. The Zellij
// features required by this backend currently use contract version 1.
const CLIENT_SERVER_CONTRACT_DIR: &str = "contract_version_1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct SocketIdentity {
    device: u64,
    inode: u64,
    changed_at_seconds: i64,
    changed_at_nanoseconds: i64,
}

pub(super) fn session_socket(session: &str) -> Result<(PathBuf, SocketIdentity)> {
    let path = session_socket_path(&socket_base_dir(), session);
    let identity = socket_identity(&path)?;
    Ok((path, identity))
}

fn session_socket_path(base_dir: &Path, session: &str) -> PathBuf {
    base_dir.join(CLIENT_SERVER_CONTRACT_DIR).join(session)
}

pub(super) fn ensure_session_socket(path: &Path, expected: SocketIdentity) -> Result<()> {
    let current = socket_identity(path)?;
    if current != expected {
        return Err(anyhow::anyhow!(
            "Zellij session socket changed while animating tab status: {}",
            path.display()
        ));
    }
    Ok(())
}

fn socket_base_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("ZELLIJ_SOCKET_DIR") {
        return PathBuf::from(path);
    }

    #[cfg(target_os = "linux")]
    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
        && path.is_absolute()
    {
        return path.join("zellij");
    }

    // SAFETY: `geteuid` has no preconditions and does not modify process state.
    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir().join(format!("zellij-{uid}"))
}

fn socket_identity(path: &Path) -> Result<SocketIdentity> {
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "Failed to inspect Zellij session socket: {}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_socket() {
        return Err(anyhow::anyhow!(
            "Zellij session path is not a socket: {}",
            path.display()
        ));
    }
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_at_seconds: metadata.ctime(),
        changed_at_nanoseconds: metadata.ctime_nsec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn session_socket_uses_the_zellij_contract_directory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let contract_dir = temp.path().join(CLIENT_SERVER_CONTRACT_DIR);
        fs::create_dir_all(&contract_dir)?;
        let socket_path = contract_dir.join("dev");
        let _listener = UnixListener::bind(&socket_path)?;

        assert_eq!(session_socket_path(temp.path(), "dev"), socket_path);
        socket_identity(&socket_path)?;
        Ok(())
    }

    #[test]
    fn identity_detects_a_different_incarnation() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("dev");
        let _listener = UnixListener::bind(&socket_path)?;
        let identity = socket_identity(&socket_path)?;
        let different_identity = SocketIdentity {
            inode: identity.inode.wrapping_add(1),
            ..identity
        };

        ensure_session_socket(&socket_path, identity)?;
        assert!(ensure_session_socket(&socket_path, different_identity).is_err());
        Ok(())
    }
}
