//! Shared helper functions for agent display name extraction.
//!
//! These are used by both the dashboard and sidebar to derive human-readable
//! names from agent pane metadata.

use std::path::Path;

/// Extract the worktree name from a window or session name.
/// Checks window_name first (window mode), then session_name (session mode).
/// Returns (worktree_name, is_main) where is_main indicates if this is the main worktree.
pub fn extract_worktree_name(
    session_name: &str,
    window_name: &str,
    window_prefix: &str,
    path: &Path,
) -> (String, bool) {
    if let Some(stripped) = window_name.strip_prefix(window_prefix) {
        // Window mode: worktree name is in the window name
        (stripped.to_string(), false)
    } else if let Some(stripped) = session_name.strip_prefix(window_prefix) {
        // Session mode: worktree name is in the session name
        (stripped.to_string(), false)
    } else {
        // Non-workmux agent: derive from filesystem path
        derive_worktree_name_from_path(path)
    }
}

/// Derive a worktree name from a filesystem path by matching known worktree
/// directory patterns. Pure string/path-component parsing, no filesystem I/O.
fn derive_worktree_name_from_path(path: &Path) -> (String, bool) {
    let components: Vec<_> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    for (i, comp) in components.iter().enumerate().rev() {
        // Pattern: project__worktrees/<name>[/...]
        if comp.ends_with("__worktrees")
            && let Some(&name) = components.get(i + 1)
        {
            return (name.to_string(), false);
        }

        // Pattern: project/.worktrees/<name>[/...]
        if *comp == ".worktrees"
            && let Some(&name) = components.get(i + 1)
        {
            return (name.to_string(), false);
        }
    }

    ("main".to_string(), true)
}

/// Extract project name from a worktree path.
/// Finds the git root (where .git is a directory) or falls back to pattern matching.
pub fn extract_project_name(path: &Path) -> String {
    // Walk up the path to find the git root or worktrees pattern
    for ancestor in path.ancestors() {
        // Check if this is the git root (where .git is a directory, not a file)
        let git_path = ancestor.join(".git");
        if git_path.is_dir()
            && let Some(name) = ancestor.file_name()
        {
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

/// Source of a sidebar label, used internally by `resolve_labels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LabelSource {
    Worktree,
    Window,
    Session,
    Project,
}

/// Resolve the sidebar's primary and secondary labels for a single row.
///
/// Walks a candidate chain (`worktree → window → session → project`) and takes
/// the first meaningful entry as the primary label and the next as the
/// secondary. The worktree must always remain visible: when demoted (not
/// primary), it is appended to the secondary line as `"<secondary> · <worktree>"`
/// (or stands alone when no other secondary exists).
///
/// The pane title is intentionally not in the chain. It has its own dedicated
/// third line in the renderer, and promoting it to primary/secondary caused
/// duplication and made the first line jump around as the agent's task changed.
///
/// `window_cmd` is the foreground command of the agent's pane; if `None` (non-tmux
/// backends), the window name is never promoted because we have no way to detect
/// auto-tracking vs a sticky user-set name.
pub fn resolve_labels(
    project: &str,
    session: &str,
    worktree: &str,
    window: &str,
    window_cmd: Option<&str>,
) -> (String, String) {
    let candidates: [(LabelSource, &str, bool); 4] = [
        (
            LabelSource::Worktree,
            worktree,
            is_worktree_meaningful(worktree),
        ),
        (
            LabelSource::Window,
            window,
            is_window_meaningful(window, window_cmd),
        ),
        (
            LabelSource::Session,
            session,
            is_session_meaningful(session, project),
        ),
        (LabelSource::Project, project, !project.is_empty()),
    ];

    let mut meaningful = candidates.iter().filter(|(_, _, m)| *m);
    let primary = meaningful.next();

    let (primary_src, primary_text) = match primary {
        Some((src, text, _)) => (*src, (*text).to_string()),
        None => (LabelSource::Project, project.to_string()),
    };

    // Worktree-primary contract: per the plan, the common feature-branch case
    // keeps project as secondary regardless of other meaningful candidates.
    // When worktree is demoted, fall through to the next candidate in the chain
    // and append the worktree so it never disappears.
    let secondary = if primary_src == LabelSource::Worktree {
        if !project.is_empty() && project != worktree {
            project.to_string()
        } else {
            String::new()
        }
    } else {
        let secondary_base = meaningful
            .next()
            .map(|(_, t, _)| (*t).to_string())
            .unwrap_or_default();
        let wt = if worktree.is_empty() {
            "main".to_string()
        } else {
            worktree.to_string()
        };
        if secondary_base.is_empty() {
            wt
        } else {
            format!("{} · {}", secondary_base, wt)
        }
    };

    (primary_text, secondary)
}

/// Generic window names that should never be promoted as a primary label,
/// regardless of whether they happen to differ from `pane_current_command`.
/// Covers default shells (POSIX + alternatives + Windows), the literal
/// "default", and tmux's common defaults.
fn is_generic_window_name(name: &str) -> bool {
    matches!(
        name,
        "zsh"
            | "bash"
            | "sh"
            | "fish"
            | "dash"
            | "ksh"
            | "csh"
            | "tcsh"
            | "nu"
            | "xonsh"
            | "elvish"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "tmux"
            | "default"
            | ""
    )
}

fn is_worktree_meaningful(w: &str) -> bool {
    let w = w.trim();
    !w.is_empty() && !matches!(w, "main" | "master")
}

fn is_window_meaningful(window: &str, cmd: Option<&str>) -> bool {
    let Some(cmd) = cmd else {
        return false;
    };
    let window = window.trim();
    let cmd = cmd.trim();
    if window.is_empty() {
        return false;
    }
    // Block generic shell/default names even when they differ from pane_current_command.
    // E.g., a pane running `node` with window name `zsh` should not promote `zsh`.
    if is_generic_window_name(window) {
        return false;
    }
    // tmux's #{automatic_rename} flag is unreliable; comparing window_name against
    // pane_current_command is the only signal that survives across versions.
    window != cmd
}

fn is_session_meaningful(session: &str, project: &str) -> bool {
    let session = session.trim();
    let project = project.trim();
    if session.is_empty() {
        return false;
    }
    if session == project {
        return false;
    }
    if session.eq_ignore_ascii_case("default") {
        return false;
    }
    if session.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    true
}

/// Clean up a pane title for use as a label, returning `None` if it's noise.
///
/// Strips leading status-icon characters and OpenCode prefixes, then filters out
/// shell names, the generic "Claude Code" title, and any title that duplicates
/// the worktree or project name.
pub fn sanitize_pane_title<'a>(
    raw: Option<&'a str>,
    worktree: &str,
    project: &str,
) -> Option<&'a str> {
    let title = raw?.trim();
    if title.is_empty() {
        return None;
    }

    let title = title
        .trim_start_matches(|c: char| {
            ('\u{2800}'..='\u{28FF}').contains(&c)
                || matches!(c, '✳' | '⠀' | '●' | '○' | '◌' | '✓' | '✗')
        })
        .trim();

    let title = strip_oc_title_prefix(title);

    if title.is_empty() {
        return None;
    }

    if title.starts_with("Claude Code") {
        return None;
    }

    if matches!(title, "zsh" | "bash" | "sh" | "fish") {
        return None;
    }

    if title == worktree || title == project {
        return None;
    }

    Some(title)
}

/// Strip repeated OpenCode title prefixes used for terminal title metadata.
pub fn strip_oc_title_prefix(mut title: &str) -> &str {
    while let Some((prefix, rest)) = title.split_once('|') {
        if prefix.trim() != "OC" {
            break;
        }

        title = rest.trim();
    }

    title
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_extract_worktree_name_window_mode() {
        let path = Path::new("/home/user/myproject__worktrees/fix-bug");
        let (name, is_main) =
            extract_worktree_name("main-session", "workmux:fix-bug", "workmux:", path);
        assert_eq!(name, "fix-bug");
        assert!(!is_main);
    }

    #[test]
    fn test_extract_worktree_name_session_mode() {
        let path = Path::new("/home/user/myproject__worktrees/feature-auth");
        let (name, is_main) =
            extract_worktree_name("workmux:feature-auth", "zsh", "workmux:", path);
        assert_eq!(name, "feature-auth");
        assert!(!is_main);
    }

    #[test]
    fn test_extract_worktree_name_window_preferred_over_session() {
        let path = Path::new("/home/user/myproject__worktrees/from-window");
        let (name, is_main) = extract_worktree_name(
            "workmux:from-session",
            "workmux:from-window",
            "workmux:",
            path,
        );
        assert_eq!(name, "from-window");
        assert!(!is_main);
    }

    #[test]
    fn test_extract_worktree_name_path_fallback_sibling() {
        let path = Path::new("/home/user/myproject__worktrees/fix-bug");
        let (name, is_main) = extract_worktree_name("0", "zsh", "workmux:", path);
        assert_eq!(name, "fix-bug");
        assert!(!is_main);
    }

    #[test]
    fn test_extract_worktree_name_path_fallback_subdir() {
        let path = Path::new("/home/user/myproject/.worktrees/fix-bug");
        let (name, is_main) = extract_worktree_name("0", "zsh", "workmux:", path);
        assert_eq!(name, "fix-bug");
        assert!(!is_main);
    }

    #[test]
    fn test_extract_worktree_name_path_fallback_nested_cwd() {
        // Agent cwd is a subdirectory of the worktree
        let path = Path::new("/home/user/myproject__worktrees/fix-bug/src/lib");
        let (name, is_main) = extract_worktree_name("0", "zsh", "workmux:", path);
        assert_eq!(name, "fix-bug");
        assert!(!is_main);
    }

    #[test]
    fn test_extract_worktree_name_path_fallback_main() {
        let path = Path::new("/home/user/myproject");
        let (name, is_main) = extract_worktree_name("0", "zsh", "workmux:", path);
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
    fn resolve_labels_feature_branch_keeps_worktree_primary() {
        // Common case: feature-branch worktree, project as secondary.
        let (primary, secondary) = resolve_labels("api", "api", "fix-auth", "zsh", Some("zsh"));
        assert_eq!(primary, "fix-auth");
        assert_eq!(secondary, "api");
    }

    #[test]
    fn resolve_labels_promotes_sticky_window_on_main_worktree() {
        // Multiple agents on `main` with sticky tmux window names.
        let (primary, secondary) =
            resolve_labels("api", "api", "main", "db-migration", Some("sleep"));
        assert_eq!(primary, "db-migration");
        assert_eq!(secondary, "api · main");
    }

    #[test]
    fn resolve_labels_promotes_session_when_window_auto_tracks() {
        // Session-per-project: session name carries identity, window auto-tracks shell.
        let (primary, secondary) =
            resolve_labels("monorepo", "frontend-refactor", "main", "zsh", Some("zsh"));
        assert_eq!(primary, "frontend-refactor");
        assert_eq!(secondary, "monorepo · main");
    }

    #[test]
    fn resolve_labels_falls_back_to_project_when_nothing_else_meaningful() {
        // Numeric session, auto-tracked window, main worktree → project wins.
        let (primary, secondary) = resolve_labels("api", "0", "main", "zsh", Some("zsh"));
        assert_eq!(primary, "api");
        assert_eq!(secondary, "main");
    }

    #[test]
    fn resolve_labels_window_never_promoted_without_cmd() {
        // Non-tmux backends pass None for window_cmd; window must not be promoted.
        let (primary, secondary) = resolve_labels("api", "api", "main", "looks-meaningful", None);
        assert_eq!(primary, "api");
        assert_eq!(secondary, "main");
    }

    #[test]
    fn resolve_labels_session_equal_to_project_not_meaningful() {
        let (primary, secondary) = resolve_labels("api", "api", "main", "zsh", Some("zsh"));
        assert_eq!(primary, "api");
        assert_eq!(secondary, "main");
    }

    #[test]
    fn resolve_labels_default_session_not_meaningful() {
        let (primary, secondary) = resolve_labels("api", "default", "main", "zsh", Some("zsh"));
        assert_eq!(primary, "api");
        assert_eq!(secondary, "main");
    }

    #[test]
    fn resolve_labels_master_treated_like_main() {
        let (primary, secondary) =
            resolve_labels("api", "release-prep", "master", "zsh", Some("zsh"));
        assert_eq!(primary, "release-prep");
        assert_eq!(secondary, "api · master");
    }

    #[test]
    fn resolve_labels_worktree_primary_keeps_project_secondary_even_with_sticky_window() {
        let (primary, secondary) =
            resolve_labels("api", "session-x", "fix-auth", "scratchpad", Some("zsh"));
        assert_eq!(primary, "fix-auth");
        assert_eq!(secondary, "api");
    }

    #[test]
    fn resolve_labels_window_zsh_not_promoted_when_pane_runs_node() {
        // Generic window names are blocklisted even when they differ from cmd.
        let (primary, secondary) = resolve_labels("api", "0", "main", "zsh", Some("node"));
        assert_eq!(primary, "api");
        assert_eq!(secondary, "main");
    }

    #[test]
    fn resolve_labels_window_default_blocked() {
        let (primary, secondary) = resolve_labels("api", "0", "main", "default", Some("node"));
        assert_eq!(primary, "api");
        assert_eq!(secondary, "main");
    }

    #[test]
    fn resolve_labels_worktree_primary_no_project_yields_empty_secondary() {
        let (primary, secondary) = resolve_labels("", "", "fix-auth", "", None);
        assert_eq!(primary, "fix-auth");
        assert_eq!(secondary, "");
    }

    #[test]
    fn resolve_labels_blocks_pwsh_powershell_nu() {
        for shell in ["pwsh", "powershell", "nu", "csh", "tcsh", "xonsh", "elvish"] {
            let (primary, _) = resolve_labels("api", "0", "main", shell, Some("node"));
            assert_eq!(primary, "api", "expected {shell} to be blocked");
        }
    }

    #[test]
    fn resolve_labels_trims_whitespace_in_predicates() {
        // Whitespace-padded generic values must not bypass meaningfulness checks.
        let (primary, _) = resolve_labels("api", " default ", "main", " zsh ", Some("zsh"));
        assert_eq!(primary, "api");

        let (primary, _) = resolve_labels("api", " 0 ", " main ", " zsh ", Some("zsh"));
        assert_eq!(primary, "api");
    }

    #[test]
    fn test_strip_oc_title_prefix() {
        assert_eq!(
            strip_oc_title_prefix("OC | Investigating..."),
            "Investigating..."
        );
        assert_eq!(
            strip_oc_title_prefix("OC | OC | Investigating..."),
            "Investigating..."
        );
        assert_eq!(
            strip_oc_title_prefix("Claude Code | Investigating..."),
            "Claude Code | Investigating..."
        );
    }
}
