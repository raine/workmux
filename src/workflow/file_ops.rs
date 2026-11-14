use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::{config, git};
use tracing::info;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOperation {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub kind: FileOperationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperationKind {
    Copy,
    Symlink,
}

pub fn resolve_file_operations(
    repo_root: &Path,
    worktree_path: &Path,
    file_config: &config::FileConfig,
) -> Result<Vec<FileOperation>> {
    let mut operations = Vec::new();
    for (patterns, kind, label) in [
        (
            file_config.copy.as_deref().unwrap_or_default(),
            FileOperationKind::Copy,
            "copy",
        ),
        (
            file_config.symlink.as_deref().unwrap_or_default(),
            FileOperationKind::Symlink,
            "symlink",
        ),
    ] {
        for pattern in patterns {
            let full_pattern = repo_root.join(pattern).to_string_lossy().to_string();
            for entry in glob::glob(&full_pattern)? {
                let source = entry?;
                validate_path_within_repo(&source, repo_root, label, pattern)?;
                let relative = source.strip_prefix(repo_root)?;
                operations.push(FileOperation {
                    destination: worktree_path.join(relative),
                    source,
                    kind,
                });
            }
        }
    }
    Ok(operations)
}

/// Performs copy and symlink operations from the repo root to the worktree
pub fn handle_file_operations(
    repo_root: &Path,
    worktree_path: &Path,
    file_config: &config::FileConfig,
) -> Result<()> {
    tracing::debug!(
        repo = %repo_root.display(),
        worktree = %worktree_path.display(),
        copy_patterns = file_config.copy.as_ref().map(|v| v.len()).unwrap_or(0),
        symlink_patterns = file_config.symlink.as_ref().map(|v| v.len()).unwrap_or(0),
        "file_operations:start"
    );

    let mut copy_count = 0;
    let mut symlink_count = 0;

    // Handle copies
    if let Some(copy_patterns) = &file_config.copy {
        for pattern in copy_patterns {
            let full_pattern = repo_root.join(pattern).to_string_lossy().to_string();
            for entry in glob::glob(&full_pattern)? {
                let source_path = entry?;

                validate_path_within_repo(&source_path, repo_root, "copy", pattern)?;

                let relative_path = source_path.strip_prefix(repo_root)?;
                let dest_path = worktree_path.join(relative_path);

                if source_path.is_dir() {
                    // Recursively copy directory contents
                    copy_dir_recursive(&source_path, &dest_path).with_context(|| {
                        format!(
                            "Failed to copy directory {:?} to {:?}",
                            source_path, dest_path
                        )
                    })?;
                } else {
                    // Copy single file
                    if let Some(parent) = dest_path.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!("Failed to create parent directory for {:?}", dest_path)
                        })?;
                    }
                    fs::copy(&source_path, &dest_path).with_context(|| {
                        format!("Failed to copy file {:?} to {:?}", source_path, dest_path)
                    })?;
                }
                copy_count += 1;
            }
        }
    }

    // Handle symlinks
    if let Some(symlink_patterns) = &file_config.symlink {
        for pattern in symlink_patterns {
            let full_pattern = repo_root.join(pattern).to_string_lossy().to_string();
            for entry in glob::glob(&full_pattern)? {
                let source_path = entry?;

                validate_path_within_repo(&source_path, repo_root, "symlink", pattern)?;

                let relative_path = source_path.strip_prefix(repo_root)?;
                let dest_path = worktree_path.join(relative_path);

                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("Failed to create parent directory for {:?}", dest_path)
                    })?;
                }

                // Critical: create a relative path for the symlink
                let dest_parent = dest_path.parent().ok_or_else(|| {
                    anyhow!(
                        "Could not determine parent directory for destination path: {:?}",
                        dest_path
                    )
                })?;

                let relative_source = pathdiff::diff_paths(&source_path, dest_parent)
                    .ok_or_else(|| anyhow!("Could not create relative path for symlink"))?;

                // Remove existing file/symlink at destination to avoid errors
                // IMPORTANT: Use symlink_metadata to avoid following symlinks
                if let Ok(metadata) = dest_path.symlink_metadata() {
                    if metadata.is_dir() {
                        fs::remove_dir_all(&dest_path).with_context(|| {
                            format!("Failed to remove existing directory at {:?}", dest_path)
                        })?;
                    } else {
                        // Handles both files and symlinks
                        fs::remove_file(&dest_path).with_context(|| {
                            format!("Failed to remove existing file/symlink at {:?}", dest_path)
                        })?;
                    }
                }

                #[cfg(unix)]
                std::os::unix::fs::symlink(&relative_source, &dest_path).with_context(|| {
                    format!(
                        "Failed to create symlink from {:?} to {:?}",
                        relative_source, dest_path
                    )
                })?;

                #[cfg(windows)]
                {
                    if source_path.is_dir() {
                        std::os::windows::fs::symlink_dir(&relative_source, &dest_path)
                    } else {
                        std::os::windows::fs::symlink_file(&relative_source, &dest_path)
                    }
                    .with_context(|| {
                        format!(
                            "Failed to create symlink from {:?} to {:?}",
                            relative_source, dest_path
                        )
                    })?;
                }
                symlink_count += 1;
            }
        }
    }

    if copy_count > 0 || symlink_count > 0 {
        info!(
            copied = copy_count,
            symlinked = symlink_count,
            "file_operations:completed"
        );
    }

    Ok(())
}

/// Symlink CLAUDE.local.md from main worktree if it exists and is gitignored.
pub fn symlink_claude_local_md(repo_root: &Path, worktree_path: &Path) -> Result<()> {
    let source = repo_root.join("CLAUDE.local.md");
    if !source.exists() {
        return Ok(());
    }

    if !git::is_path_ignored(repo_root, "CLAUDE.local.md") {
        return Ok(());
    }

    let dest = worktree_path.join("CLAUDE.local.md");
    if dest.symlink_metadata().is_ok() {
        // Already exists (file, symlink, or dir) -- skip
        return Ok(());
    }

    let relative_source = pathdiff::diff_paths(&source, worktree_path)
        .ok_or_else(|| anyhow!("Could not create relative path for CLAUDE.local.md symlink"))?;

    #[cfg(unix)]
    std::os::unix::fs::symlink(&relative_source, &dest)
        .context("Failed to symlink CLAUDE.local.md")?;

    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&relative_source, &dest)
        .context("Failed to symlink CLAUDE.local.md")?;

    info!("Symlinked CLAUDE.local.md to worktree");
    Ok(())
}

fn validate_path_within_repo(
    source_path: &Path,
    repo_root: &Path,
    op: &str,
    pattern: &str,
) -> Result<()> {
    let relative = source_path.strip_prefix(repo_root).map_err(|_| {
        anyhow!(
            "Path traversal detected for {} pattern '{}'. The path '{}' is outside the repository root.",
            op, pattern, source_path.display()
        )
    })?;

    if relative
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(anyhow!(
            "Path traversal detected for {} pattern '{}'. The path '{}' contains '..' components.",
            op,
            pattern,
            source_path.display()
        ));
    }

    Ok(())
}

/// Recursively copy a directory's contents into the destination, overwriting existing files.
/// Symlinks are preserved rather than followed to avoid infinite recursion on symlink loops.
/// Special files (sockets, FIFOs) are skipped to avoid blocking.
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;

        // Remove existing entry at destination to support overwrite
        if let Ok(meta) = dst_path.symlink_metadata() {
            if meta.is_dir() && file_type.is_dir() {
                // Both are directories; merge contents
            } else if meta.is_dir() {
                fs::remove_dir_all(&dst_path)?;
            } else {
                fs::remove_file(&dst_path)?;
            }
        }

        if file_type.is_symlink() {
            let target = fs::read_link(&src_path)?;
            std::os::unix::fs::symlink(&target, &dst_path)?;
        } else if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
