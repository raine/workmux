//! Bounded, metadata-only evidence captured after recursive deletion fails.

use std::collections::VecDeque;
use std::fmt::Write;
use std::path::Path;
use std::time::UNIX_EPOCH;

const MAX_ENTRIES: usize = 128;
const MAX_DEPTH: usize = 6;
const MAX_BYTES: usize = 24 * 1024;

/// Snapshot remaining entries without retrying deletion or reading file contents.
/// This is a post-failure observation, not the location of the failed syscall.
pub(super) fn remaining_entries(root: &Path) -> String {
    let mut output = String::from(
        "post-failure snapshot (not the failing syscall path; entries may change concurrently):",
    );
    let mut queue = VecDeque::from([(root.to_path_buf(), 0)]);
    let mut count = 0;
    while let Some((path, depth)) = queue.pop_front() {
        if count >= MAX_ENTRIES || output.len() >= MAX_BYTES {
            output.push_str("\n[snapshot truncated]");
            break;
        }
        count += 1;
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = write!(output, "\n{relative:?}: metadata error={error}");
                continue;
            }
        };
        let kind = if metadata.is_symlink() {
            "symlink"
        } else if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|time| time.as_millis());
        let _ = write!(
            output,
            "\n{relative:?}: type={kind} bytes={} modified_unix_ms={modified:?}",
            metadata.len()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let _ = write!(
                output,
                " device={} inode={}",
                metadata.dev(),
                metadata.ino()
            );
        }
        if !metadata.is_dir() {
            continue;
        }
        if depth >= MAX_DEPTH {
            output.push_str(" [depth limit]");
            continue;
        }
        match std::fs::read_dir(&path) {
            Ok(entries) => {
                for entry in entries {
                    if count + queue.len() >= MAX_ENTRIES {
                        output.push_str("\n[entry limit; additional entries omitted]");
                        break;
                    }
                    match entry {
                        Ok(entry) => queue.push_back((entry.path(), depth + 1)),
                        Err(error) => {
                            let _ = write!(output, "\n{relative:?}: read entry error={error}");
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                let _ = write!(output, "\n{relative:?}: read directory error={error}");
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_metadata_without_file_contents_or_symlink_traversal() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("remaining"), "private file contents").unwrap();
        std::fs::write(outside.path().join("outside-marker"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let snapshot = remaining_entries(root.path());
        assert!(snapshot.contains("remaining"));
        assert!(snapshot.contains("type=file bytes=21"));
        assert!(snapshot.contains("type=symlink"));
        assert!(snapshot.contains("modified_unix_ms=Some("));
        assert!(!snapshot.contains("private file contents"));
        assert!(!snapshot.contains("outside-marker"));
        assert!(root.path().join("remaining").exists());
    }

    #[test]
    fn bounds_wide_and_deep_trees() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..200 {
            std::fs::write(root.path().join(format!("file-{i}")), "").unwrap();
        }
        let snapshot = remaining_entries(root.path());
        assert!(snapshot.contains("entry limit"));
        assert!(snapshot.matches("type=").count() <= MAX_ENTRIES);
        let deep = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(deep.path().join("a/b/c/d/e/f/g/h")).unwrap();
        assert!(remaining_entries(deep.path()).contains("depth limit"));
    }

    #[test]
    fn reports_missing_paths_without_panicking() {
        let root = tempfile::tempdir().unwrap();
        assert!(remaining_entries(&root.path().join("missing")).contains("metadata error="));
    }
}
