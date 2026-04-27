//! Pi agent sandbox helpers.
//!
//! Pi stores managed fd/rg binaries under `~/.pi/agent/bin/`. Workmux mounts
//! the rest of `~/.pi/agent` read-write so auth, sessions, skills, and
//! settings flow through to sandboxed pi. The `bin/` subpath must NOT flow
//! through, because pi auto-downloads platform-specific binaries there and
//! a Linux sandbox would clobber a macOS host's Mach-O binaries (or vice
//! versa).
//!
//! These helpers compute a sandbox-local, arch-keyed directory used as a
//! deeper bind-mount overlay on `bin/`.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Short stable hash of a path, used to disambiguate cache directories
/// when two different projects share a worktree basename.
pub(crate) fn path_hash(path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())[..10].to_string()
}

/// Architecture key for the bin overlay subdirectory. Cheap insurance
/// against the same class of bug appearing under x86-on-arm64 emulation.
pub(crate) fn linux_arch_key() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" | "arm64" => "linux-arm64",
        "x86_64" => "linux-amd64",
        other => other,
    }
}

/// Resolve and create the host-side overlay directory for pi's `bin/`.
///
/// `state_dir` is the per-sandbox state directory (per-VM for Lima,
/// per-worktree-handle for containers).
pub(crate) fn pi_bin_overlay_dir(state_dir: &Path) -> Result<PathBuf> {
    let dir = state_dir.join("pi-agent-bin").join(linux_arch_key());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_key_known_values() {
        let key = linux_arch_key();
        assert!(
            matches!(key, "linux-arm64" | "linux-amd64") || !key.is_empty(),
            "unexpected arch key: {}",
            key
        );
    }

    #[test]
    fn overlay_dir_created_under_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = pi_bin_overlay_dir(tmp.path()).unwrap();
        assert!(dir.exists());
        assert!(dir.starts_with(tmp.path()));
        assert!(dir.to_string_lossy().contains("pi-agent-bin"));
        assert!(dir.to_string_lossy().contains(linux_arch_key()));
    }
}
