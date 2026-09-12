//! Detect which VCS backend to use based on filesystem markers.
//!
//! Mirrors `crate::multiplexer::detect_backend()` / `create_backend()`.

use anyhow::{Result, bail};
use std::path::Path;
use std::sync::Arc;

use super::{RepoKind, VcsBackend, git_backend::GitBackend, jj_backend::JjBackend};

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
/// `JjColocated` and `JjOnly` both map to `JjBackend`: a colocated repo is
/// still driven through `jj`, and `.jj` winning over `.git` in
/// [`detect_repo_kind_in`] is what makes that so. This mirrors
/// `multiplexer::detect_backend()`/`create_backend()`.
pub fn detect_backend_in(path: &Path) -> Result<Arc<dyn VcsBackend>> {
    match detect_repo_kind_in(path) {
        RepoKind::Git => Ok(Arc::new(GitBackend)),
        RepoKind::JjColocated | RepoKind::JjOnly => Ok(Arc::new(JjBackend)),
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
    fn detect_backend_in_returns_jj_backend_for_jj_only() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".jj")).unwrap();
        let backend = detect_backend_in(temp.path()).unwrap();
        assert_eq!(backend.name(), "jj");
    }

    #[test]
    fn detect_backend_in_returns_jj_backend_for_jj_colocated() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".jj")).unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let backend = detect_backend_in(temp.path()).unwrap();
        assert_eq!(backend.name(), "jj");
    }

    #[test]
    fn detect_backend_in_bails_outside_any_repository() {
        let temp = tempfile::tempdir().unwrap();
        match detect_backend_in(temp.path()) {
            Ok(_) => panic!("expected no backend outside a repository"),
            Err(e) => assert!(e.to_string().contains("Not in a git or jj repository")),
        }
    }
}
