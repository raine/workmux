//! Detect which VCS backend to use based on filesystem markers.
//!
//! Mirrors `crate::multiplexer::detect_backend()` / `create_backend()`.

use anyhow::{Result, bail};
use std::path::Path;
use std::sync::Arc;

use super::{RepoKind, VcsBackend, git_backend::GitBackend};

/// Walk upward from `path` toward the filesystem root, looking for `.jj`
/// and `.git` marker directories at each level.
///
/// `.jj` always wins over `.git` when both are present at the same level
/// (a colocated jj repo). Returns [`RepoKind::None`] if neither marker is
/// found before reaching the filesystem root.
pub fn detect_repo_kind_in(path: &Path) -> RepoKind {
    let mut current = Some(path);

    while let Some(dir) = current {
        let has_jj = dir.join(".jj").exists();
        let has_git = dir.join(".git").exists();

        if has_jj {
            return if has_git {
                RepoKind::JjColocated
            } else {
                RepoKind::JjOnly
            };
        }

        if has_git {
            return RepoKind::Git;
        }

        current = dir.parent();
    }

    RepoKind::None
}

/// Create a [`VcsBackend`] instance for the repository containing `path`.
///
/// For now, `JjColocated`/`JjOnly` and `None` are not yet supported
/// (`JjBackend` lands in a later task); this mirrors
/// `multiplexer::detect_backend()`/`create_backend()`.
pub fn detect_backend_in(path: &Path) -> Result<Arc<dyn VcsBackend>> {
    match detect_repo_kind_in(path) {
        RepoKind::Git => Ok(Arc::new(GitBackend)),
        RepoKind::JjColocated | RepoKind::JjOnly => {
            bail!("jj repos are not yet supported")
        }
        RepoKind::None => bail!("Not in a git or jj repository"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_git_only() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        assert_eq!(detect_repo_kind_in(temp.path()), RepoKind::Git);
    }

    #[test]
    fn detects_none_for_empty_dir() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(detect_repo_kind_in(temp.path()), RepoKind::None);
    }

    #[test]
    fn detects_jj_only() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".jj")).unwrap();
        assert_eq!(detect_repo_kind_in(temp.path()), RepoKind::JjOnly);
    }

    #[test]
    fn detects_jj_colocated_when_both_markers_present() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".jj")).unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        assert_eq!(detect_repo_kind_in(temp.path()), RepoKind::JjColocated);
    }

    #[test]
    fn detect_backend_in_returns_git_backend_for_git_repo() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let backend = detect_backend_in(temp.path()).unwrap();
        assert_eq!(backend.name(), "git");
    }

    #[test]
    fn detect_backend_in_bails_for_jj_only() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".jj")).unwrap();
        match detect_backend_in(temp.path()) {
            Ok(_) => panic!("expected jj repos to be unsupported"),
            Err(e) => assert!(e.to_string().contains("jj repos are not yet supported")),
        }
    }

    #[test]
    fn detect_backend_in_bails_for_jj_colocated() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".jj")).unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        match detect_backend_in(temp.path()) {
            Ok(_) => panic!("expected jj repos to be unsupported"),
            Err(e) => assert!(e.to_string().contains("jj repos are not yet supported")),
        }
    }
}
