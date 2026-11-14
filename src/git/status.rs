use anyhow::{Context, Result, anyhow};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::cmd::Cmd;

use super::GitStatus;
use super::branch::get_default_branch_in;

/// Create a git command that won't contend for index.lock.
/// Background monitoring should never block the user's git operations.
fn bg_git<'a>() -> Cmd<'a> {
    Cmd::new("git").arg("--no-optional-locks")
}

pub fn linked_worktree_admin_dir(worktree_path: &Path) -> Option<std::path::PathBuf> {
    let content = std::fs::read_to_string(worktree_path.join(".git")).ok()?;
    let raw = content.trim().strip_prefix("gitdir: ")?;
    let gitdir = Path::new(raw.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir.to_path_buf()
    } else {
        worktree_path.join(gitdir)
    };

    Some(gitdir)
}

pub fn has_missing_admin_dir(worktree_path: &Path) -> bool {
    linked_worktree_admin_dir(worktree_path).is_some_and(|gitdir| !gitdir.is_dir())
}

/// Check if the worktree has uncommitted changes
pub fn has_uncommitted_changes(worktree_path: &Path) -> Result<bool> {
    let output = bg_git()
        .workdir(worktree_path)
        .args(&["status", "--porcelain"])
        .run_and_capture_stdout()?;

    Ok(!output.is_empty())
}

/// Check if the worktree has tracked changes (staged or modified)
/// This excludes untracked files
pub fn has_tracked_changes(worktree_path: &Path) -> Result<bool> {
    let output = bg_git()
        .workdir(worktree_path)
        .args(&["status", "--porcelain"])
        .run_and_capture_stdout()?;

    // Filter out untracked files (lines starting with "??")
    for line in output.lines() {
        if !line.starts_with("??") && !line.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Check if the worktree has untracked files
pub fn has_untracked_files(worktree_path: &Path) -> Result<bool> {
    let output = bg_git()
        .workdir(worktree_path)
        .args(&["status", "--porcelain"])
        .run_and_capture_stdout()?;

    // Look for untracked files (lines starting with "??")
    for line in output.lines() {
        if line.starts_with("??") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn git_diff_has_changes(worktree_path: &Path, args: &[&str]) -> Result<bool> {
    let mut command = super::pinned_git(worktree_path)?;
    command.arg("--no-optional-locks");
    if args.first() == Some(&"diff") {
        command.args(["diff", "--no-ext-diff", "--no-textconv"]);
        command.args(&args[1..]);
    } else {
        command.args(args);
    }
    let output = command.output().context("Failed to execute git diff")?;

    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        code => Err(anyhow!(
            "git diff failed with status {:?}: {}",
            code,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Check if the worktree has staged changes
pub fn has_staged_changes(worktree_path: &Path) -> Result<bool> {
    git_diff_has_changes(worktree_path, &["diff", "--cached", "--quiet"])
}

/// Check if the worktree has unstaged changes
pub fn has_unstaged_changes(worktree_path: &Path) -> Result<bool> {
    git_diff_has_changes(worktree_path, &["diff", "--quiet"])
}

/// Count lines in a file, treating it like git (text files only).
/// Returns 0 for binary files or errors.
fn count_lines(path: &Path) -> std::io::Result<usize> {
    use std::fs::File;

    let mut file = File::open(path)?;

    // Check for binary content (heuristic: null byte in first 8KB)
    let mut buffer = [0; 8192];
    let n = file.read(&mut buffer)?;
    if buffer[..n].contains(&0) {
        return Ok(0);
    }

    // Reset file position to start
    file.seek(SeekFrom::Start(0))?;

    let mut count = 0;
    let mut buf = [0; 32 * 1024];
    let mut last_byte = None;

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count += buf[..n].iter().filter(|&&b| b == b'\n').count();
        last_byte = Some(buf[n - 1]);
    }

    // If file ends with non-newline character, count that as a line (like git)
    if let Some(b) = last_byte
        && b != b'\n'
    {
        count += 1;
    }

    Ok(count)
}

fn parse_numstat(output: &[u8]) -> (usize, usize) {
    // Git quotes unusual names without -z; only the first two tab fields matter.
    output.split(|b| *b == b'\n').fold((0, 0), |(a, r), line| {
        let mut fields = line.split(|b| *b == b'\t');
        let number = |field: Option<&[u8]>| {
            field
                .and_then(|f| std::str::from_utf8(f).ok())
                .and_then(|f| f.parse::<usize>().ok())
                .unwrap_or(0)
        };
        (a + number(fields.next()), r + number(fields.next()))
    })
}

fn committed_stats(path: &Path, base: &str, head: &str) -> Result<(usize, usize)> {
    let output = super::status_cache::capture(
        path,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            &format!("{base}...{head}"),
        ],
    )?;
    Ok(parse_numstat(&output))
}

fn uncommitted_stats(path: &Path) -> (usize, usize) {
    use std::os::unix::ffi::OsStrExt;
    let (mut added, removed) = super::status_cache::capture(
        path,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            "HEAD",
        ],
    )
    .map(|bytes| parse_numstat(&bytes))
    .unwrap_or_default();
    if let Ok(output) =
        super::status_cache::capture(path, &["ls-files", "--others", "--exclude-standard", "-z"])
    {
        for name in output.split(|b| *b == 0).filter(|name| !name.is_empty()) {
            let full_path = path.join(std::ffi::OsStr::from_bytes(name));
            if std::fs::symlink_metadata(&full_path).is_ok_and(|m| m.file_type().is_symlink()) {
                added += 1;
            } else if let Ok(lines) = count_lines(&full_path) {
                added += lines;
            }
        }
    }
    (added, removed)
}

#[derive(Debug)]
struct Observation {
    branch: Option<String>,
    ahead: usize,
    behind: usize,
    dirty: bool,
    upstream: bool,
    attributes: Vec<std::path::PathBuf>,
}

fn observe_status(bytes: &[u8]) -> Result<Observation> {
    use std::os::unix::ffi::OsStrExt;
    if !bytes.ends_with(&[0]) {
        return Err(anyhow!("Incomplete porcelain status"));
    }
    let mut observation = Observation {
        branch: None,
        ahead: 0,
        behind: 0,
        dirty: false,
        upstream: false,
        attributes: Vec::new(),
    };
    let mut records = bytes[..bytes.len() - 1].split(|b| *b == 0);
    let mut oid = false;
    let mut branch = false;
    while let Some(record) = records.next() {
        if let Some(header) = record.strip_prefix(b"# ") {
            let header = std::str::from_utf8(header)?;
            if let Some(name) = header.strip_prefix("branch.head ") {
                if name.is_empty() {
                    return Err(anyhow!("Missing branch"));
                }
                branch = true;
                if name != "(detached)" {
                    observation.branch = Some(name.to_owned());
                }
            } else if let Some(value) = header.strip_prefix("branch.oid ") {
                oid = value == "(initial)"
                    || (value.len() >= 40 && value.bytes().all(|b| b.is_ascii_hexdigit()));
            } else if header.starts_with("branch.upstream ") {
                observation.upstream = true;
            } else if let Some(value) = header.strip_prefix("branch.ab ") {
                let (a, b) = value
                    .split_once(' ')
                    .ok_or_else(|| anyhow!("Invalid branch counts"))?;
                observation.ahead = a
                    .strip_prefix('+')
                    .ok_or_else(|| anyhow!("Invalid ahead"))?
                    .parse()?;
                observation.behind = b
                    .strip_prefix('-')
                    .ok_or_else(|| anyhow!("Invalid behind"))?
                    .parse()?;
            }
            continue;
        }
        let fields = match record.first() {
            Some(b'1') => 8,
            Some(b'2') => 9,
            Some(b'u') => 10,
            Some(b'?') => 1,
            _ => return Err(anyhow!("Unknown porcelain entry")),
        };
        let name = record
            .splitn(fields + 1, |b| *b == b' ')
            .nth(fields)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow!("Incomplete porcelain entry"))?;
        observation.dirty = true;
        let name = Path::new(std::ffi::OsStr::from_bytes(name));
        if name.file_name() == Some(std::ffi::OsStr::new(".gitattributes")) {
            observation.attributes.push(name.to_path_buf());
        }
        if record.first() == Some(&b'2') && records.next().is_none_or(|name| name.is_empty()) {
            return Err(anyhow!("Incomplete rename"));
        }
    }
    if !oid || !branch {
        return Err(anyhow!("Missing branch headers"));
    }
    Ok(observation)
}

fn predict_conflict(path: &Path, base: &str, head: &str) -> Result<bool> {
    let mut command = super::pinned_git(path)?;
    let status = command
        .args([
            "--no-optional-locks",
            "merge-tree",
            "--write-tree",
            base,
            head,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    match status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(anyhow!("Merge prediction failed")),
    }
}

fn discover_base(path: &Path, branch: &str, main: Option<&str>) -> (String, bool) {
    let key = format!("branch.{branch}.workmux-base");
    let local = super::status_cache::output(path, &["config", "--local", &key]);
    let definitive = match local {
        Ok(output) if output.status.success() => {
            if let Ok(value) = String::from_utf8(output.stdout) {
                let value = value.trim();
                if !value.is_empty() {
                    return (value.to_owned(), true);
                }
            }
            false
        }
        Ok(output) => output.status.code() == Some(1),
        Err(_) => false,
    };
    if let Some(main) = main.filter(|s| !s.is_empty()) {
        return (main.to_owned(), definitive);
    }
    match get_default_branch_in(Some(path)) {
        Ok(base) => (base, definitive),
        Err(_) => ("main".to_owned(), false),
    }
}

/// Check if a rebase is in progress by looking for rebase state directories in the git dir.
/// For linked worktrees, resolves the actual gitdir from the `.git` file.
fn is_rebasing(worktree_path: &Path) -> bool {
    let dot_git = worktree_path.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        // Linked worktree: .git is a file containing "gitdir: /path/to/real/gitdir"
        let content = std::fs::read_to_string(&dot_git).unwrap_or_default();
        match content.strip_prefix("gitdir: ") {
            Some(gitdir) => {
                let path = std::path::PathBuf::from(gitdir.trim());
                if path.is_absolute() {
                    path
                } else {
                    worktree_path.join(path)
                }
            }
            None => return false,
        }
    } else {
        return false;
    };

    // Interactive rebase: rebase-merge/
    // Non-interactive rebase or git am: rebase-apply/
    git_dir.join("rebase-merge").is_dir() || git_dir.join("rebase-apply").is_dir()
}

fn resolve_commits(path: &Path, base_branch: &str) -> Option<(String, String)> {
    let bytes = super::status_cache::capture(
        path,
        &[
            "rev-parse",
            "--revs-only",
            "--end-of-options",
            "HEAD^{commit}",
            &format!("{base_branch}^{{commit}}"),
        ],
    )
    .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    let head = lines.next()?.to_owned();
    let base = lines.next()?.to_owned();
    let valid = |oid: &str| oid.len() >= 40 && oid.bytes().all(|b| b.is_ascii_hexdigit());
    (lines.next().is_none() && valid(&head) && valid(&base)).then_some((head, base))
}

/// Get git status for a worktree (ahead/behind, conflicts, dirty state, diff stats).
/// This is designed for dashboard display and prioritizes speed over completeness.
/// Uses `git status --porcelain=v2 --branch` to get most info in a single command.
pub fn get_git_status(worktree_path: &Path, main_branch: Option<&str>) -> GitStatus {
    collect_status(worktree_path, main_branch, None, std::time::Instant::now())
}

/// Collect fresh working-tree status while reusing successful committed computations.
pub fn get_git_status_cached(
    path: &Path,
    main_branch: Option<&str>,
    cache: &mut super::GitStatusCache,
) -> GitStatus {
    collect_status(path, main_branch, Some(cache), std::time::Instant::now())
}

fn collect_status(
    path: &Path,
    main_branch: Option<&str>,
    mut cache: Option<&mut super::GitStatusCache>,
    observed_at: std::time::Instant,
) -> GitStatus {
    use super::status_cache::{Dependencies, Entry, capture};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs());
    let is_rebasing = is_rebasing(path);
    let failure = || GitStatus {
        cached_at: now,
        is_dirty: true,
        is_rebasing,
        ..Default::default()
    };
    let identity = match super::RepositoryIdentity::discover(path) {
        Ok(identity) => identity,
        Err(_) => {
            if let Some(cache) = cache.as_mut() {
                cache
                    .entries
                    .remove(&path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
            }
            return failure();
        }
    };
    let root = identity.worktree.clone();
    let observation = capture(
        &root,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
            "-z",
        ],
    )
    .and_then(|bytes| observe_status(&bytes));
    let observation = match observation {
        Ok(observation) => observation,
        Err(_) => {
            if let Some(cache) = cache.as_mut() {
                cache.entries.remove(&root);
            }
            return failure();
        }
    };
    let mut status = GitStatus {
        ahead: observation.ahead,
        behind: observation.behind,
        is_dirty: observation.dirty,
        branch: observation.branch.clone(),
        has_upstream: observation.upstream,
        is_rebasing,
        cached_at: now,
        ..Default::default()
    };
    let Some(branch) = observation.branch else {
        return status;
    };
    let dependencies = cache.as_ref().and_then(|cache| {
        Dependencies::observe(
            identity,
            &branch,
            main_branch,
            &observation.attributes,
            cache
                .live(&root, observed_at)
                .map(|entry| &entry.dependencies),
        )
        .ok()
    });
    let cached_base = dependencies.as_ref().and_then(|deps| {
        cache
            .as_ref()?
            .live(&root, observed_at)
            .filter(|entry| entry.dependencies == *deps)
            .map(|entry| entry.base_branch.clone())
    });
    let fresh = cached_base.is_some();
    if !fresh && let Some(cache) = cache.as_mut() {
        cache.entries.remove(&root);
    }
    let (base_branch, definitive) = match cached_base {
        Some(base) => (base, true),
        None => discover_base(&root, &branch, main_branch),
    };
    status.base_branch = base_branch.clone();
    if branch != base_branch {
        let mut entry = match (cache.as_mut(), dependencies) {
            (Some(cache), Some(deps)) => match resolve_commits(&root, &base_branch) {
                Some((head, base)) => {
                    let reusable = fresh
                        && cache
                            .entries
                            .get(&root)
                            .is_some_and(|entry| entry.head == head && entry.base == base);
                    if !reusable && definitive {
                        let merge_eligible =
                            deps.merge_cache_eligible(&head, &base).unwrap_or(false);
                        cache.entries.insert(
                            root.clone(),
                            Entry {
                                dependencies: deps,
                                observed_at,
                                base_branch: base_branch.clone(),
                                head,
                                base,
                                committed: None,
                                conflict: None,
                                merge_eligible,
                            },
                        );
                    }
                    cache.entries.get_mut(&root)
                }
                None => {
                    cache.entries.remove(&root);
                    None
                }
            },
            _ => None,
        };
        let committed = match &mut entry {
            Some(entry) => {
                if entry.committed.is_none() {
                    entry.committed = committed_stats(&root, &entry.base, &entry.head).ok();
                }
                entry.committed
            }
            None => committed_stats(&root, &base_branch, "HEAD").ok(),
        };
        (status.lines_added, status.lines_removed) = committed.unwrap_or_default();
        let conflict = match entry.filter(|entry| entry.merge_eligible) {
            Some(entry) => {
                if entry.conflict.is_none() {
                    entry.conflict = predict_conflict(&root, &entry.base, &entry.head).ok();
                }
                entry.conflict
            }
            None => predict_conflict(&root, &base_branch, "HEAD").ok(),
        };
        status.has_conflict = conflict.unwrap_or(false);
    } else if let (Some(cache), Some(deps)) = (cache.as_mut(), dependencies)
        && !fresh
        && definitive
    {
        cache.entries.insert(
            root.clone(),
            Entry {
                dependencies: deps,
                observed_at,
                base_branch,
                head: String::new(),
                base: String::new(),
                committed: Some((0, 0)),
                conflict: Some(false),
                merge_eligible: false,
            },
        );
    }
    if observation.dirty {
        (status.uncommitted_added, status.uncommitted_removed) = uncommitted_stats(&root);
    }
    status
}

#[cfg(test)]
mod tests {
    use super::super::GitStatusCache;
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;
    use std::time::Duration;

    fn git(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();
        git(path, &["init", "-qb", "main"]);
        git(path, &["config", "user.name", "Test"]);
        git(path, &["config", "user.email", "test@example.com"]);
        std::fs::create_dir(path.join("nested")).unwrap();
        std::fs::write(path.join("nested/file"), "base\n").unwrap();
        std::fs::write(path.join("nested/.gitattributes"), "").unwrap();
        git(path, &["add", "."]);
        git(path, &["commit", "-qm", "base"]);
        git(path, &["checkout", "-qb", "topic"]);
        std::fs::write(path.join("nested/file"), "base\ncommitted\n").unwrap();
        git(path, &["commit", "-qam", "topic"]);
        temp
    }

    fn refresh(path: &Path, cache: &mut GitStatusCache) -> GitStatus {
        get_git_status_cached(path, Some("main"), cache)
    }

    fn oracle(path: &Path, cache: &mut GitStatusCache) -> GitStatus {
        let actual = refresh(path, cache);
        let expected = get_git_status(path, Some("main"));
        let root = path.canonicalize().unwrap();
        assert_eq!(
            (actual.uncommitted_added, actual.uncommitted_removed),
            uncommitted_stats(&root)
        );
        assert_eq!(actual.branch, expected.branch);
        assert_eq!(actual.base_branch, expected.base_branch);
        assert_eq!(actual.is_dirty, expected.is_dirty);
        assert_eq!(actual.has_conflict, expected.has_conflict);
        assert_eq!(
            (
                actual.lines_added,
                actual.lines_removed,
                actual.uncommitted_added,
                actual.uncommitted_removed
            ),
            (
                expected.lines_added,
                expected.lines_removed,
                expected.uncommitted_added,
                expected.uncommitted_removed
            )
        );
        actual
    }

    #[test]
    fn complete_byte_status_handles_renames_and_rejects_truncation() {
        let header = format!("# branch.oid {}\0# branch.head topic\0", "a".repeat(40));
        let clean = observe_status(header.as_bytes()).unwrap();
        assert!(!clean.dirty);
        for tail in [
            "? space\nname\0",
            "1 .M S.M. 100644 100644 100644 a b file\0",
            "u UU N... 100644 100644 100644 100644 a b c file\0",
            "2 R. N... 100644 100644 100644 a b R100 new\nname\0? old\0",
        ] {
            assert!(
                observe_status(format!("{header}{tail}").as_bytes())
                    .unwrap()
                    .dirty
            );
        }
        for tail in [
            "? file",
            "2 R. N... 100644 100644 100644 a b R100 new\0",
            "x unknown\0",
            "1 .M\0",
            "\0",
        ] {
            assert!(
                observe_status(format!("{header}{tail}").as_bytes()).is_err(),
                "{tail:?}"
            );
        }
        assert!(observe_status(b"# branch.head main\0").is_err());
        let mut bytes = header.into_bytes();
        bytes.extend_from_slice(b"? invalid-\xff\0");
        assert!(observe_status(&bytes).unwrap().dirty);
    }

    #[test]
    fn edits_and_index_refresh_reuse_comparisons_but_not_dirty_contents() {
        let repo = repo();
        let path = repo.path();
        let mut cache = GitStatusCache::default();
        let clean = oracle(path, &mut cache);
        assert!(!clean.is_dirty);
        assert_eq!(clean.lines_added, 1);
        let timestamp = cache.entries[&path.canonicalize().unwrap()].observed_at;
        std::fs::write(path.join("nested/file"), "base\ncommitted\none\n").unwrap();
        assert_eq!(oracle(path, &mut cache).uncommitted_added, 1);
        std::fs::write(path.join("nested/file"), "base\ncommitted\none\ntwo\n").unwrap();
        assert_eq!(oracle(path, &mut cache).uncommitted_added, 2);
        // Plain status may rewrite index stat data; it is not an attribute change.
        git(path, &["status", "--porcelain"]);
        oracle(path, &mut cache);
        assert_eq!(
            cache.entries[&path.canonicalize().unwrap()].observed_at,
            timestamp
        );
        std::fs::write(path.join("untracked"), "one\n").unwrap();
        assert_eq!(oracle(path, &mut cache).uncommitted_added, 3);
        std::fs::write(path.join("untracked"), "one\ntwo\n").unwrap();
        assert_eq!(oracle(path, &mut cache).uncommitted_added, 4);
        assert_eq!(
            cache.entries[&path.canonicalize().unwrap()].observed_at,
            timestamp
        );
    }

    #[test]
    fn head_base_override_and_main_argument_invalidate() {
        let repo = repo();
        let path = repo.path();
        let mut cache = GitStatusCache::default();
        oracle(path, &mut cache);
        let old = cache.entries[&path.canonicalize().unwrap()].observed_at;
        std::fs::write(path.join("nested/file"), "base\ncommitted\nnext\n").unwrap();
        git(path, &["commit", "-qam", "next"]);
        assert_eq!(oracle(path, &mut cache).lines_added, 2);
        assert_ne!(
            cache.entries[&path.canonicalize().unwrap()].observed_at,
            old
        );
        git(path, &["branch", "-f", "main", "HEAD"]);
        assert_eq!(oracle(path, &mut cache).lines_added, 0);
        git(path, &["config", "branch.topic.workmux-base", "topic"]);
        assert_eq!(oracle(path, &mut cache).base_branch, "topic");
        git(path, &["config", "--unset", "branch.topic.workmux-base"]);
        assert_eq!(
            get_git_status_cached(path, Some("topic"), &mut cache).base_branch,
            "topic"
        );
        assert_eq!(oracle(path, &mut cache).base_branch, "main");
    }

    #[test]
    fn config_includes_worktree_config_and_nested_attributes_invalidate() {
        let repo = repo();
        let path = repo.path();
        let mut cache = GitStatusCache::default();
        let include = repo.path().join(".git/included");
        std::fs::write(&include, "[diff]\n algorithm = myers\n").unwrap();
        git(path, &["config", "include.path", include.to_str().unwrap()]);
        oracle(path, &mut cache);
        let old = cache.entries[&path.canonicalize().unwrap()].observed_at;
        std::fs::write(&include, "[diff]\n algorithm = patience\n").unwrap();
        oracle(path, &mut cache);
        assert_ne!(
            cache.entries[&path.canonicalize().unwrap()].observed_at,
            old
        );
        git(path, &["config", "extensions.worktreeConfig", "true"]);
        git(
            path,
            &["config", "--worktree", "diff.algorithm", "histogram"],
        );
        oracle(path, &mut cache);
        let old = cache.entries[&path.canonicalize().unwrap()].observed_at;
        git(path, &["config", "--worktree", "diff.algorithm", "myers"]);
        oracle(path, &mut cache);
        assert_ne!(
            cache.entries[&path.canonicalize().unwrap()].observed_at,
            old
        );
        std::fs::write(path.join("nested/.gitattributes"), "file -diff\n").unwrap();
        assert_eq!(oracle(path, &mut cache).lines_added, 0);
        git(path, &["add", "nested/.gitattributes"]);
        std::fs::remove_file(path.join("nested/.gitattributes")).unwrap();
        oracle(path, &mut cache);
        git(path, &["reset", "-q", "HEAD", "nested/.gitattributes"]);
        std::fs::write(path.join("nested/.gitattributes"), "").unwrap();
        assert_eq!(oracle(path, &mut cache).lines_added, 1);
        let global = path.join(".git/global-attributes");
        std::fs::write(&global, "nested/file -diff\n").unwrap();
        git(
            path,
            &["config", "core.attributesFile", global.to_str().unwrap()],
        );
        assert_eq!(oracle(path, &mut cache).lines_added, 0);
        std::fs::write(&global, "").unwrap();
        assert_eq!(oracle(path, &mut cache).lines_added, 1);
    }

    #[test]
    fn global_and_conditional_includes_are_observed_in_an_isolated_process() {
        if std::env::var_os("WM_CACHE_CONFIG_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "git::status::tests::global_and_conditional_includes_are_observed_in_an_isolated_process"])
                .env("HOME", home.path()).env("XDG_CONFIG_HOME", home.path().join("config"))
                .env("WM_CACHE_CONFIG_CHILD", "1").output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let repo = repo();
        let path = repo.path();
        let mut cache = GitStatusCache::default();
        let include = path.join(".git/global-include");
        std::fs::write(&include, "[diff]\n algorithm = myers\n").unwrap();
        git(
            path,
            &[
                "config",
                "--global",
                "includeIf.onbranch:topic.path",
                include.to_str().unwrap(),
            ],
        );
        oracle(path, &mut cache);
        let old = cache.entries[&path.canonicalize().unwrap()].observed_at;
        std::fs::write(&include, "[diff]\n algorithm = patience\n").unwrap();
        oracle(path, &mut cache);
        assert_ne!(
            cache.entries[&path.canonicalize().unwrap()].observed_at,
            old
        );
        // A previously absent include can appear without modifying its parent config.
        let absent = path.join(".git/new-include");
        git(
            path,
            &[
                "config",
                "--global",
                "include.path",
                absent.to_str().unwrap(),
            ],
        );
        oracle(path, &mut cache);
        let old = cache.entries[&path.canonicalize().unwrap()].observed_at;
        std::fs::write(&absent, "[diff]\n algorithm = histogram\n").unwrap();
        oracle(path, &mut cache);
        assert_ne!(
            cache.entries[&path.canonicalize().unwrap()].observed_at,
            old
        );
    }

    #[test]
    fn ignored_attributes_and_expiry_have_bounded_not_sliding_freshness() {
        let repo = repo();
        let path = repo.path();
        let mut cache = GitStatusCache::default();
        // An ignored attribute file in a directory with no tracked attributes is
        // not in either the attribute index or status inventory.
        git(path, &["rm", "nested/.gitattributes"]);
        git(path, &["commit", "-qm", "remove attributes"]);
        std::fs::write(path.join(".git/info/exclude"), ".gitattributes\n").unwrap();
        oracle(path, &mut cache);
        let start = cache.entries[&path.canonicalize().unwrap()].observed_at;
        std::fs::write(path.join("nested/.gitattributes"), "file -diff\n").unwrap();
        let warm = collect_status(
            path,
            Some("main"),
            Some(&mut cache),
            start + Duration::from_secs(29),
        );
        assert_eq!(warm.lines_added, 1);
        assert_eq!(
            cache.entries[&path.canonicalize().unwrap()].observed_at,
            start
        );
        let expired = collect_status(
            path,
            Some("main"),
            Some(&mut cache),
            start + Duration::from_secs(30),
        );
        assert_eq!(expired.lines_added, 0);
        assert_eq!(
            expired.lines_added,
            get_git_status(path, Some("main")).lines_added
        );
    }

    #[test]
    fn history_and_failed_computations_do_not_reuse_success() {
        let repo = repo();
        let path = repo.path();
        let mut cache = GitStatusCache::default();
        oracle(path, &mut cache);
        let old = cache.entries[&path.canonicalize().unwrap()].observed_at;
        let head = git(path, &["rev-parse", "HEAD"]);
        let base = git(path, &["rev-parse", "main"]);
        git(path, &["replace", &head, &base]);
        assert_eq!(oracle(path, &mut cache).lines_added, 0);
        assert_ne!(
            cache.entries[&path.canonicalize().unwrap()].observed_at,
            old
        );
        git(path, &["replace", "-d", &head]);
        std::fs::write(path.join(".git/shallow"), format!("{head}\n")).unwrap();
        oracle(path, &mut cache);
        assert!(
            cache.entries[&path.canonicalize().unwrap()]
                .committed
                .is_none()
        );
        assert!(
            cache.entries[&path.canonicalize().unwrap()]
                .conflict
                .is_none()
        );
        std::fs::remove_file(path.join(".git/shallow")).unwrap();
        assert_eq!(oracle(path, &mut cache).lines_added, 1);
        git(path, &["config", "branch.topic.workmux-base", "missing"]);
        oracle(path, &mut cache);
        assert!(cache.entries.is_empty());
        git(path, &["branch", "missing", "main"]);
        oracle(path, &mut cache);
        assert!(
            cache.entries[&path.canonicalize().unwrap()]
                .committed
                .is_some()
        );
        std::fs::write(path.join(".git/index"), "bad index").unwrap();
        assert!(refresh(path, &mut cache).is_dirty);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn odd_untracked_names_renames_symlinks_binary_and_subpaths() {
        use std::os::unix::fs::symlink;
        let repo = repo();
        let path = repo.path();
        let mut cache = GitStatusCache::default();
        std::fs::write(
            path.join(OsString::from_vec(if cfg!(target_os = "macos") {
                b" odd-name \n".to_vec()
            } else {
                b" invalid-\xff \n".to_vec()
            })),
            "one\ntwo",
        )
        .unwrap();
        std::fs::write(path.join("binary"), b"hello\0world").unwrap();
        symlink("missing", path.join("link")).unwrap();
        assert_eq!(oracle(path, &mut cache).uncommitted_added, 3);
        git(path, &["mv", "nested/file", "nested/new name"]);
        assert_eq!(oracle(path, &mut cache).uncommitted_added, 3);
        let nested = get_git_status_cached(&path.join("nested"), Some("main"), &mut cache);
        assert_eq!(nested.uncommitted_added, 3);
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn custom_drivers_execute_each_refresh_and_keep_branch_labels() {
        let repo = repo();
        let path = repo.path();
        let mut cache = GitStatusCache::default();
        git(path, &["checkout", "main"]);
        std::fs::write(path.join("nested/file"), "main change\n").unwrap();
        git(path, &["commit", "-qam", "main"]);
        git(path, &["checkout", "topic"]);
        std::fs::write(
            path.join(".git/info/attributes"),
            "nested/file merge=test\n",
        )
        .unwrap();
        let marker = path.join(".git/driver-calls");
        git(
            path,
            &[
                "config",
                "merge.test.driver",
                &format!("echo %X %Y >> '{}'; exit 1", marker.display()),
            ],
        );
        refresh(path, &mut cache);
        refresh(path, &mut cache);
        assert!(!cache.entries[&path.canonicalize().unwrap()].merge_eligible);
        let calls = std::fs::read_to_string(marker).unwrap();
        assert_eq!(calls.lines().count(), 2);
        assert!(
            calls
                .lines()
                .all(|line| line.contains("main") && line.contains("HEAD"))
        );
    }

    #[test]
    fn linked_identity_invalid_pointers_and_pruning() {
        let repo = repo();
        let path = repo.path();
        let linked = tempfile::tempdir().unwrap();
        let root = linked.path().join("worktree");
        git(
            path,
            &["worktree", "add", "-qb", "linked", root.to_str().unwrap()],
        );
        let mut cache = GitStatusCache::default();
        oracle(&root, &mut cache);
        assert_eq!(cache.entries.len(), 1);
        std::fs::write(root.join(".git"), "gitdir: /tmp\n").unwrap();
        assert!(refresh(&root, &mut cache).is_dirty);
        assert!(cache.entries.is_empty());
        oracle(path, &mut cache);
        cache.retain(&std::collections::HashSet::new());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn same_path_repository_replacement_cannot_reuse_old_entry() {
        let parent = tempfile::tempdir().unwrap();
        let source = repo();
        let root = parent.path().join("root");
        let old = parent.path().join("old");
        git(
            parent.path(),
            &[
                "clone",
                "-q",
                source.path().to_str().unwrap(),
                root.to_str().unwrap(),
            ],
        );
        git(&root, &["branch", "main", "origin/main"]);
        let mut cache = GitStatusCache::default();
        oracle(&root, &mut cache);
        let timestamp = cache.entries[&root.canonicalize().unwrap()].observed_at;
        std::fs::rename(&root, &old).unwrap();
        git(
            parent.path(),
            &[
                "clone",
                "-q",
                source.path().to_str().unwrap(),
                root.to_str().unwrap(),
            ],
        );
        git(&root, &["branch", "main", "origin/main"]);
        oracle(&root, &mut cache);
        assert_ne!(
            cache.entries[&root.canonicalize().unwrap()].observed_at,
            timestamp
        );
    }

    #[test]
    fn submodules_bypass_merge_cache_and_preserve_dirty_semantics() {
        let child = repo();
        let repo = repo();
        let path = repo.path();
        git(
            path,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                child.path().to_str().unwrap(),
                "sub",
            ],
        );
        git(path, &["commit", "-qam", "submodule"]);
        let mut cache = GitStatusCache::default();
        oracle(path, &mut cache);
        assert!(!cache.entries[&path.canonicalize().unwrap()].merge_eligible);
        std::fs::write(path.join("sub/nested/file"), "dirty\n").unwrap();
        assert!(oracle(path, &mut cache).is_dirty);
        git(path, &["config", "submodule.sub.ignore", "all"]);
        assert!(!oracle(path, &mut cache).is_dirty);
    }

    #[test]
    #[ignore = "release workload harness; uses disposable repositories"]
    fn release_workload() {
        let roots: Vec<_> = (0..40).map(|_| repo()).collect();
        for root in roots.iter().take(4) {
            std::fs::write(root.path().join("large"), "base line\n".repeat(20000)).unwrap();
            git(root.path(), &["add", "large"]);
            git(
                root.path(),
                &["commit", "-qm", "large committed comparison"],
            );
        }
        // Include ignored build-tree noise without traversing it for attributes.
        for root in &roots {
            std::fs::write(root.path().join(".git/info/exclude"), "target/\n").unwrap();
            std::fs::create_dir(root.path().join("target")).unwrap();
            for i in 0..1000 {
                std::fs::write(root.path().join(format!("target/{i}")), "ignored\n").unwrap();
            }
        }
        for root in &roots {
            git(
                root.path(),
                &["update-ref", "refs/remotes/origin/main", "main"],
            );
            git(
                root.path(),
                &[
                    "symbolic-ref",
                    "refs/remotes/origin/HEAD",
                    "refs/remotes/origin/main",
                ],
            );
        }
        let cached = std::env::var_os("WM_BENCH_CACHED").is_some();
        let mut cache = GitStatusCache::default();
        for cycle in 0..6 {
            for (i, root) in roots.iter().enumerate() {
                if cycle > 1 && i % 3 == 0 {
                    std::fs::write(
                        root.path().join("nested/file"),
                        format!("base\ncommitted\n{}", "dirty\n".repeat(cycle)),
                    )
                    .unwrap();
                    std::fs::write(root.path().join("untracked"), "untracked\n".repeat(cycle))
                        .unwrap();
                }
                if cycle == 4 && i == 0 {
                    std::fs::write(root.path().join("nested/.gitattributes"), "file -diff\n")
                        .unwrap();
                }
                let status = if cached {
                    get_git_status_cached(root.path(), None, &mut cache)
                } else {
                    get_git_status(root.path(), None)
                };
                assert!(status.branch.is_some());
                println!(
                    "cycle={cycle} root={i} committed={} dirty={}",
                    status.lines_added, status.uncommitted_added
                );
            }
        }
    }
}
