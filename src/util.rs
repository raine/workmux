use anyhow::{Context, Result, anyhow};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

const STALE_ATOMIC_TEMP_AGE: Duration = Duration::from_secs(60);

/// Write content through a same-directory temporary file and atomically replace the target.
pub fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    cleanup_stale_atomic_temps(path)?;

    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let prefix = atomic_temp_prefix(path);
    let mut tmp = tempfile::Builder::new()
        .prefix(&prefix)
        .tempfile_in(parent)
        .with_context(|| format!("Failed to create temp file for {}", path.display()))?;

    tmp.write_all(content)
        .with_context(|| format!("Failed to write temp file for {}", path.display()))?;
    tmp.flush()
        .with_context(|| format!("Failed to flush temp file for {}", path.display()))?;
    tmp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Failed to rename temp file for {}", path.display()))?;
    Ok(())
}

fn cleanup_stale_atomic_temps(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Ok(entries) = fs::read_dir(parent) else {
        return Ok(());
    };
    let prefix = atomic_temp_prefix(path);

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) {
            continue;
        }
        if !is_stale_atomic_temp(&entry) {
            continue;
        }
        let _ = fs::remove_file(entry.path());
    }

    Ok(())
}

fn is_stale_atomic_temp(entry: &fs::DirEntry) -> bool {
    entry
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= STALE_ATOMIC_TEMP_AGE)
}

fn atomic_temp_prefix(path: &Path) -> String {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("atomic");
    format!(".{file_name}.tmp.")
}

/// Canonicalize a path, falling back to the original if canonicalization fails.
pub fn canon_or_self(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Lexically normalize a path by resolving `.` and `..` components without
/// touching the filesystem.  Unlike `canonicalize()` this works even when the
/// target path does not exist yet.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(components.last(), Some(Component::Normal(_))) {
                    components.pop();
                } else if matches!(
                    components.last(),
                    Some(Component::RootDir) | Some(Component::Prefix(_))
                ) {
                    // Already at root, ignore the ".."
                } else {
                    components.push(component);
                }
            }
            _ => components.push(component),
        }
    }
    components.iter().collect()
}

/// Expand `~` or `~/...` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    expand_tilde_with_home(path, home::home_dir().as_deref())
}

fn expand_tilde_with_home(path: &str, home: Option<&Path>) -> PathBuf {
    if path == "~" {
        if let Some(h) = home {
            return h.to_path_buf();
        }
    } else if let Some(rest) = path.strip_prefix("~/")
        && let Some(h) = home
    {
        return h.join(rest);
    }
    PathBuf::from(path)
}

/// Expand a `worktree_dir` template against a project root.
///
/// Supported syntax:
/// - Leading `~` or `~/...` expands to the user's home directory.
/// - `{project}` is replaced with `project_root.file_name()`.
///
/// Any other `{...}` token is rejected as an unknown placeholder.
/// Relative results are joined to `project_root` and lexically normalized.
/// Absolute results are returned verbatim (no normalization), matching
/// the prior behavior of `workmux add` for absolute `worktree_dir` values.
pub fn expand_worktree_dir(template: &str, project_root: &Path) -> Result<PathBuf> {
    expand_worktree_dir_with_home(template, project_root, home::home_dir().as_deref())
}

pub(crate) fn expand_worktree_dir_with_home(
    template: &str,
    project_root: &Path,
    home: Option<&Path>,
) -> Result<PathBuf> {
    let mut cursor = 0usize;
    while let Some(rel_open) = template[cursor..].find('{') {
        let open = cursor + rel_open;
        let rel_close = template[open..]
            .find('}')
            .ok_or_else(|| anyhow!("worktree_dir: unterminated '{{' in template '{}'", template))?;
        let close = open + rel_close;
        let token = &template[open..=close];
        if token != "{project}" {
            return Err(anyhow!(
                "worktree_dir: unknown placeholder '{}' in '{}' (only '{{project}}' is supported)",
                token,
                template
            ));
        }
        cursor = close + 1;
    }

    let tilde_expanded = expand_tilde_with_home(template, home);
    let project_name = project_root
        .file_name()
        .ok_or_else(|| {
            anyhow!(
                "Could not determine project name from path: {}",
                project_root.display()
            )
        })?
        .to_string_lossy();
    let as_str = tilde_expanded.to_string_lossy();
    let with_project = as_str.replace("{project}", &project_name);
    let path = Path::new(&with_project);

    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(normalize_path(&project_root.join(path)))
    }
}

/// Format an age in seconds as a compact relative string (e.g., "2h", "3d", "1w", "2mo").
pub fn format_compact_age(secs: u64) -> String {
    let mins = secs / 60;
    let hours = secs / 3600;
    let days = secs / 86400;
    let weeks = days / 7;
    let months = days / 30;
    let years = days / 365;

    if years > 0 {
        format!("{}y", years)
    } else if months > 0 {
        format!("{}mo", months)
    } else if weeks > 0 {
        format!("{}w", weeks)
    } else if days > 0 {
        format!("{}d", days)
    } else if hours > 0 {
        format!("{}h", hours)
    } else if mins > 0 {
        format!("{}m", mins)
    } else {
        "<1m".to_string()
    }
}

/// Format a duration as a human-readable elapsed time string.
/// Used by `status` and `wait` commands.
pub fn format_elapsed_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{}h", h)
        } else {
            format!("{}h {}m", h, m)
        }
    }
}

/// Format a Duration as a human-readable elapsed time string (with seconds).
/// Used by `wait` command for more precise timing.
pub fn format_elapsed_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{}m {:02}s", m, s)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{}h {:02}m", h, m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_replaces_target_and_removes_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn write_atomic_does_not_remove_fresh_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let temp = dir.path().join(".state.json.tmp.keep");
        fs::write(&temp, "pending").unwrap();

        write_atomic(&path, b"stored").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "stored");
        assert_eq!(fs::read_to_string(&temp).unwrap(), "pending");
    }

    #[test]
    fn expand_worktree_dir_tilde_and_project() {
        let home = PathBuf::from("/home/alice");
        let project = PathBuf::from("/Users/alice/code/myproj");
        let expanded =
            expand_worktree_dir_with_home("~/.workmux/{project}", &project, Some(&home)).unwrap();
        assert_eq!(expanded, PathBuf::from("/home/alice/.workmux/myproj"));
    }

    #[test]
    fn expand_worktree_dir_relative_with_project() {
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home("{project}-wts", &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from("/x/y/foo/foo-wts"));
    }

    #[test]
    fn expand_worktree_dir_absolute_with_project() {
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home("/tmp/wts-{project}", &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from("/tmp/wts-foo"));
    }

    #[test]
    fn expand_worktree_dir_relative_no_placeholder() {
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home(".worktrees", &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from("/x/y/foo/.worktrees"));
    }

    #[test]
    fn expand_worktree_dir_absolute_no_placeholder_preserved() {
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home("/abs/path", &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from("/abs/path"));
    }

    #[test]
    fn expand_worktree_dir_absolute_with_dotdot_preserved() {
        // Absolute templates must be returned verbatim, matching prior
        // create.rs behavior. No lexical normalization.
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home("/tmp/foo/../bar", &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from("/tmp/foo/../bar"));
    }

    #[test]
    fn expand_worktree_dir_tilde_only() {
        let home = PathBuf::from("/home/alice");
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home("~", &project, Some(&home)).unwrap();
        assert_eq!(expanded, PathBuf::from("/home/alice"));
    }

    #[test]
    fn expand_worktree_dir_unknown_placeholder_errors() {
        let project = PathBuf::from("/x/y/foo");
        let err =
            expand_worktree_dir_with_home("~/.workmux/{unknown}", &project, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("{unknown}"), "error should name token: {msg}");
    }

    #[test]
    fn expand_worktree_dir_unterminated_brace_errors() {
        let project = PathBuf::from("/x/y/foo");
        let err = expand_worktree_dir_with_home("/tmp/{project", &project, None).unwrap_err();
        assert!(err.to_string().contains("unterminated"));
    }

    #[test]
    fn expand_worktree_dir_validates_raw_template_only() {
        // Project name containing `{` must not trigger unknown-placeholder
        // detection because validation runs on the raw template.
        let project = PathBuf::from("/x/y/repo-{core}");
        let expanded = expand_worktree_dir_with_home("/tmp/{project}", &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from("/tmp/repo-{core}"));
    }

    #[test]
    fn expand_tilde_basic() {
        let home = PathBuf::from("/home/u");
        assert_eq!(expand_tilde_with_home("~", Some(&home)), home);
        assert_eq!(
            expand_tilde_with_home("~/foo/bar", Some(&home)),
            PathBuf::from("/home/u/foo/bar")
        );
        assert_eq!(
            expand_tilde_with_home("/abs", Some(&home)),
            PathBuf::from("/abs")
        );
        assert_eq!(
            expand_tilde_with_home("rel", Some(&home)),
            PathBuf::from("rel")
        );
    }

    #[test]
    fn format_elapsed_secs_seconds() {
        assert_eq!(format_elapsed_secs(0), "0s");
        assert_eq!(format_elapsed_secs(30), "30s");
        assert_eq!(format_elapsed_secs(59), "59s");
    }

    #[test]
    fn format_elapsed_secs_minutes() {
        assert_eq!(format_elapsed_secs(60), "1m");
        assert_eq!(format_elapsed_secs(150), "2m");
        assert_eq!(format_elapsed_secs(3599), "59m");
    }

    #[test]
    fn format_elapsed_secs_hours() {
        assert_eq!(format_elapsed_secs(3600), "1h");
        assert_eq!(format_elapsed_secs(7200), "2h");
    }

    #[test]
    fn format_elapsed_secs_hours_and_minutes() {
        assert_eq!(format_elapsed_secs(3660), "1h 1m");
        assert_eq!(format_elapsed_secs(5400), "1h 30m");
        assert_eq!(format_elapsed_secs(86400), "24h");
    }

    #[test]
    fn format_elapsed_duration_seconds() {
        assert_eq!(format_elapsed_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed_duration(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn format_elapsed_duration_minutes_and_seconds() {
        assert_eq!(format_elapsed_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(
            format_elapsed_duration(Duration::from_secs(3599)),
            "59m 59s"
        );
    }

    #[test]
    fn format_elapsed_duration_hours_and_minutes() {
        assert_eq!(format_elapsed_duration(Duration::from_secs(3600)), "1h 00m");
        assert_eq!(format_elapsed_duration(Duration::from_secs(3661)), "1h 01m");
        assert_eq!(format_elapsed_duration(Duration::from_secs(7260)), "2h 01m");
    }

    #[test]
    fn format_compact_age_sub_minute() {
        assert_eq!(format_compact_age(0), "<1m");
        assert_eq!(format_compact_age(30), "<1m");
        assert_eq!(format_compact_age(59), "<1m");
    }

    #[test]
    fn format_compact_age_minutes() {
        assert_eq!(format_compact_age(60), "1m");
        assert_eq!(format_compact_age(300), "5m");
        assert_eq!(format_compact_age(3599), "59m");
    }

    #[test]
    fn format_compact_age_hours() {
        assert_eq!(format_compact_age(3600), "1h");
        assert_eq!(format_compact_age(7200), "2h");
        assert_eq!(format_compact_age(86399), "23h");
    }

    #[test]
    fn format_compact_age_days() {
        assert_eq!(format_compact_age(86400), "1d");
        assert_eq!(format_compact_age(259200), "3d");
        assert_eq!(format_compact_age(604799), "6d");
    }

    #[test]
    fn format_compact_age_weeks() {
        assert_eq!(format_compact_age(604800), "1w");
        assert_eq!(format_compact_age(1209600), "2w");
    }

    #[test]
    fn format_compact_age_months() {
        assert_eq!(format_compact_age(30 * 86400), "1mo");
        assert_eq!(format_compact_age(60 * 86400), "2mo");
        assert_eq!(format_compact_age(364 * 86400), "12mo");
    }

    #[test]
    fn format_compact_age_years() {
        assert_eq!(format_compact_age(365 * 86400), "1y");
        assert_eq!(format_compact_age(730 * 86400), "2y");
    }

    #[test]
    fn normalize_path_collapses_parent_dir() {
        let p = Path::new("/Users/test/repo/../wm/handle");
        assert_eq!(normalize_path(p), PathBuf::from("/Users/test/wm/handle"));
    }

    #[test]
    fn normalize_path_collapses_multiple_parent_dirs() {
        let p = Path::new("/a/b/c/../../d");
        assert_eq!(normalize_path(p), PathBuf::from("/a/d"));
    }

    #[test]
    fn normalize_path_strips_cur_dir() {
        let p = Path::new("/a/./b/./c");
        assert_eq!(normalize_path(p), PathBuf::from("/a/b/c"));
    }

    #[test]
    fn normalize_path_preserves_leading_parent() {
        let p = Path::new("../wm/handle");
        assert_eq!(normalize_path(p), PathBuf::from("../wm/handle"));
    }

    #[test]
    fn normalize_path_no_op_for_clean_path() {
        let p = Path::new("/Users/test/wm/handle");
        assert_eq!(normalize_path(p), PathBuf::from("/Users/test/wm/handle"));
    }

    #[test]
    fn normalize_path_root_parent_stays_at_root() {
        let p = Path::new("/../foo");
        assert_eq!(normalize_path(p), PathBuf::from("/foo"));
    }
}
