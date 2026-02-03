//! Pure helper functions for agent data extraction and formatting.

use std::path::Path;

/// Extract the worktree name from a window name.
/// Returns (worktree_name, is_main) where is_main indicates if this is the main worktree.
pub fn extract_worktree_name(window_name: &str, window_prefix: &str) -> (String, bool) {
    if let Some(stripped) = window_name.strip_prefix(window_prefix) {
        // Workmux-created worktree agent
        (stripped.to_string(), false)
    } else {
        // Non-workmux agent - running in main worktree
        ("main".to_string(), true)
    }
}

/// Extract project name from a worktree path.
/// Finds the git root (where .git is a directory) or falls back to pattern matching.
pub fn extract_project_name(path: &Path) -> String {
    // Walk up the path to find the git root or worktrees pattern
    for ancestor in path.ancestors() {
        // Check if this is the git root (where .git is a directory, not a file)
        let git_path = ancestor.join(".git");
        if git_path.is_dir() && let Some(name) = ancestor.file_name() {
            return name.to_string_lossy().to_string();
        }

        // Fallback: check for sibling pattern (project__worktrees/)
        if let Some(name) = ancestor.file_name() {
            let name_str = name.to_string_lossy();
            if name_str.ends_with("__worktrees") {
                return name_str
                    .strip_suffix("__worktrees")
                    .unwrap_or(&name_str)
                    .to_string();
            }
        }
    }

    // Fallback: use the directory name (for non-worktree projects)
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Check if an agent is stale based on its status timestamp.
pub fn is_stale(status_ts: Option<u64>, stale_threshold_secs: u64, now_secs: u64) -> bool {
    status_ts
        .map(|ts| now_secs.saturating_sub(ts) > stale_threshold_secs)
        .unwrap_or(false)
}

/// Get elapsed seconds since the status timestamp.
pub fn elapsed_secs(status_ts: Option<u64>, now_secs: u64) -> Option<u64> {
    status_ts.map(|ts| now_secs.saturating_sub(ts))
}

/// Format a duration in seconds as HH:MM:SS.
pub fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;
    format!("{:02}:{:02}:{:02}", hours, mins, secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_extract_worktree_name_with_prefix() {
        let (name, is_main) = extract_worktree_name("workmux:fix-bug", "workmux:");
        assert_eq!(name, "fix-bug");
        assert!(!is_main);
    }

    #[test]
    fn test_extract_worktree_name_main() {
        let (name, is_main) = extract_worktree_name("some-window", "workmux:");
        assert_eq!(name, "main");
        assert!(is_main);
    }

    #[test]
    fn test_extract_project_name_worktrees() {
        let path = PathBuf::from("/home/user/myproject__worktrees/fix-bug");
        assert_eq!(extract_project_name(&path), "myproject");
    }

    #[test]
    fn test_extract_project_name_fallback() {
        let path = PathBuf::from("/home/user/myproject");
        assert_eq!(extract_project_name(&path), "myproject");
    }

    #[test]
    fn test_extract_project_name_git_root() {
        // Test custom worktree_dir inside repo (e.g., .worktrees)
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = temp.path().join("myproject");
        std::fs::create_dir_all(project_dir.join(".git")).unwrap();
        std::fs::create_dir_all(project_dir.join(".worktrees").join("fix-bug")).unwrap();

        let worktree_path = project_dir.join(".worktrees").join("fix-bug");
        assert_eq!(extract_project_name(&worktree_path), "myproject");
    }

    #[test]
    fn test_extract_project_name_git_file_skipped() {
        // Worktrees have .git as a file, not directory - should be skipped
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = temp.path().join("myproject");
        let worktree_dir = project_dir.join(".worktrees").join("fix-bug");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        // Create .git as a file (like real worktrees do)
        std::fs::write(worktree_dir.join(".git"), "gitdir: /somewhere/else").unwrap();
        // Create actual git root
        std::fs::create_dir_all(project_dir.join(".git")).unwrap();

        assert_eq!(extract_project_name(&worktree_dir), "myproject");
    }

    #[test]
    fn test_is_stale_true() {
        assert!(is_stale(Some(100), 60, 200)); // 100 seconds elapsed > 60 threshold
    }

    #[test]
    fn test_is_stale_false() {
        assert!(!is_stale(Some(150), 60, 200)); // 50 seconds elapsed < 60 threshold
    }

    #[test]
    fn test_is_stale_none() {
        assert!(!is_stale(None, 60, 200));
    }

    #[test]
    fn test_elapsed_secs() {
        assert_eq!(elapsed_secs(Some(100), 200), Some(100));
        assert_eq!(elapsed_secs(None, 200), None);
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0), "00:00:00");
        assert_eq!(format_duration(61), "00:01:01");
        assert_eq!(format_duration(3661), "01:01:01");
    }
}
