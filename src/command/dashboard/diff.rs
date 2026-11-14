//! Diff domain types and helper functions.

use ratatui::text::Line;
use std::path::{Path, PathBuf};

use super::ansi::{parse_ansi_to_lines, strip_ansi_escapes};

/// A file entry in the diff, used for the sidebar file list
#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    /// The filename (relative path)
    pub filename: String,
    /// Lines added in this file
    pub lines_added: usize,
    /// Lines removed in this file
    pub lines_removed: usize,
    /// Line index in parsed_lines where this file's diff starts
    pub start_line: usize,
    /// Whether this is an untracked (new) file
    pub is_new: bool,
}

/// A single hunk from a diff, suitable for staging with git apply
#[derive(Debug, Clone, PartialEq)]
pub struct DiffHunk {
    /// The file header (diff --git... up to but not including @@)
    pub file_header: String,
    /// The hunk content (starting from @@)
    pub hunk_body: String,
    /// The filename being modified
    pub filename: String,
    /// Lines added in this hunk
    pub lines_added: usize,
    /// Lines removed in this hunk
    pub lines_removed: usize,
    /// Delta-rendered content for display (file_header + hunk_body piped through delta)
    pub rendered_content: String,
    /// Cached parsed lines for efficient rendering (avoids re-parsing ANSI on every frame)
    pub parsed_lines: Vec<Line<'static>>,
}

impl DiffHunk {
    /// Attempt to split this hunk into smaller hunks if there are context lines between changes.
    /// Returns None if the hunk cannot be split.
    pub fn split(&self) -> Option<Vec<DiffHunk>> {
        let lines: Vec<&str> = self.hunk_body.lines().collect();
        if lines.is_empty() {
            return None;
        }

        // First line should be the @@ header
        let header_line = lines.first()?;
        let (old_start, new_start) = parse_hunk_header(header_line)?;

        // Content lines (skip the @@ header)
        let content_lines = &lines[1..];

        // Find indices of change lines (+ or -)
        let change_indices: Vec<usize> = content_lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                matches!(
                    classify_diff_line(line),
                    DiffLineClass::Added | DiffLineClass::Removed
                )
            })
            .map(|(i, _)| i)
            .collect();

        if change_indices.is_empty() {
            return None;
        }

        // Find split points: gaps where context lines separate change groups
        let mut split_ranges = Vec::new();
        for window in change_indices.windows(2) {
            let prev_change = window[0];
            let next_change = window[1];
            // Need at least one context line between changes to split
            if next_change > prev_change + 1 {
                split_ranges.push((next_change, prev_change + 1));
            }
        }

        if split_ranges.is_empty() {
            return None;
        }

        // Create sub-hunks with overlapping context
        let mut hunks = Vec::new();
        let mut start_idx = 0;

        for (end_idx, next_start) in &split_ranges {
            let sub_lines = &content_lines[start_idx..*end_idx];
            if let Some(h) =
                self.create_sub_hunk(sub_lines, old_start, new_start, start_idx, content_lines)
            {
                hunks.push(h);
            }
            start_idx = *next_start;
        }

        // Final hunk: from last start to end
        let sub_lines = &content_lines[start_idx..];
        if let Some(h) =
            self.create_sub_hunk(sub_lines, old_start, new_start, start_idx, content_lines)
        {
            hunks.push(h);
        }

        if hunks.len() > 1 { Some(hunks) } else { None }
    }

    /// Create a sub-hunk from a slice of content lines
    fn create_sub_hunk(
        &self,
        lines: &[&str],
        base_old_start: usize,
        base_new_start: usize,
        offset: usize,
        all_lines: &[&str],
    ) -> Option<DiffHunk> {
        if lines.is_empty() {
            return None;
        }

        // Calculate starting line numbers by simulating progression from base
        let mut current_old = base_old_start;
        let mut current_new = base_new_start;

        for line in &all_lines[0..offset] {
            match classify_diff_line(line) {
                DiffLineClass::Removed => {
                    current_old += 1;
                }
                DiffLineClass::Added => {
                    current_new += 1;
                }
                DiffLineClass::Context => {
                    // Context line
                    current_old += 1;
                    current_new += 1;
                }
            }
        }

        // Count lines in this sub-hunk
        let mut count_old = 0;
        let mut count_new = 0;
        let mut added = 0;
        let mut removed = 0;

        for line in lines {
            match classify_diff_line(line) {
                DiffLineClass::Removed => {
                    count_old += 1;
                    removed += 1;
                }
                DiffLineClass::Added => {
                    count_new += 1;
                    added += 1;
                }
                DiffLineClass::Context => {
                    count_old += 1;
                    count_new += 1;
                }
            }
        }

        // Build new @@ header
        let new_header = format!(
            "@@ -{},{} +{},{} @@",
            current_old, count_old, current_new, count_new
        );

        let hunk_body = std::iter::once(new_header.as_str())
            .chain(lines.iter().copied())
            .collect::<Vec<_>>()
            .join("\n");

        let full_diff = format!("{}\n{}", self.file_header, hunk_body);
        let rendered_content = render_through_delta(&full_diff);
        let parsed_lines = parse_ansi_to_lines(&rendered_content);

        Some(DiffHunk {
            file_header: self.file_header.clone(),
            hunk_body,
            filename: self.filename.clone(),
            lines_added: added,
            lines_removed: removed,
            rendered_content,
            parsed_lines,
        })
    }
}

/// State for the diff view
#[derive(Debug, PartialEq)]
pub struct DiffView {
    /// The diff content (with ANSI colors)
    pub content: String,
    /// Cached parsed lines for efficient rendering (avoids re-parsing ANSI on every frame)
    pub parsed_lines: Vec<Line<'static>>,
    /// Current scroll offset (use usize to handle large diffs)
    pub scroll: usize,
    /// Total line count for scroll bounds
    pub line_count: usize,
    /// Viewport height (updated by UI during render for page scroll)
    pub viewport_height: u16,
    /// Title for the view (e.g., "WIP: fix-bug")
    pub title: String,
    /// Path to the worktree (for commit/merge actions)
    pub worktree_path: PathBuf,
    /// Pane ID for sending commands to agent
    pub pane_id: String,
    /// Whether this is a branch diff (true) or uncommitted diff (false)
    pub is_branch_diff: bool,
    /// Number of lines added in the diff
    pub lines_added: usize,
    /// Number of lines removed in the diff
    pub lines_removed: usize,
    /// Whether patch mode is active (hunk-by-hunk staging)
    pub patch_mode: bool,
    /// Parsed hunks for patch mode
    pub hunks: Vec<DiffHunk>,
    /// Current hunk index in patch mode
    pub current_hunk: usize,
    /// Original total hunk count when patch mode started (for progress display)
    pub hunks_total: usize,
    /// Number of hunks processed (staged/skipped) for progress display
    pub hunks_processed: usize,
    /// Stack of staged hunks for undo functionality
    pub staged_hunks: Vec<DiffHunk>,
    /// Comment input buffer (Some = comment mode active)
    pub comment_input: Option<String>,
    /// List of files in the diff for the sidebar
    pub file_list: Vec<FileEntry>,
    /// The remote workspace whose node produced the diff, which nothing
    /// local can stage.
    pub remote: Option<RemoteDiff>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteDiff {
    pub at: crate::multiplexer::RemoteRef,
    /// The title without a note on the diff's age.
    pub title: String,
}

impl DiffView {
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        let max_scroll = self
            .line_count
            .saturating_sub(self.viewport_height as usize);
        if self.scroll < max_scroll {
            self.scroll += 1;
        }
    }

    pub fn scroll_page_up(&mut self) {
        let page = self.viewport_height as usize;
        self.scroll = self.scroll.saturating_sub(page);
    }

    pub fn scroll_page_down(&mut self) {
        let page = self.viewport_height as usize;
        // In patch mode, use current hunk's line count; otherwise use full diff
        let effective_line_count = if self.patch_mode && !self.hunks.is_empty() {
            self.hunks[self.current_hunk].parsed_lines.len()
        } else {
            self.line_count
        };
        let max_scroll = effective_line_count.saturating_sub(self.viewport_height as usize);
        self.scroll = (self.scroll + page).min(max_scroll);
    }
}

#[derive(Clone, Copy)]
enum DiffLineClass {
    Added,
    Removed,
    Context,
}

fn classify_diff_line(line: &str) -> DiffLineClass {
    let stripped = strip_ansi_escapes(line);
    if stripped.starts_with('+') && !stripped.starts_with("+++") {
        DiffLineClass::Added
    } else if stripped.starts_with('-') && !stripped.starts_with("---") {
        DiffLineClass::Removed
    } else {
        DiffLineClass::Context
    }
}

/// Parse "@@ -10,5 +12,7 @@" -> Some((10, 12))
pub fn parse_hunk_header(header: &str) -> Option<(usize, usize)> {
    let stripped = strip_ansi_escapes(header);

    // Find content between @@ markers using split
    // Format: "@@ -old,count +new,count @@" or "@@ -old,count +new,count @@ context"
    let mut parts = stripped.split("@@");
    parts.next()?; // Skip before first @@
    let meta = parts.next()?; // Content between @@ markers

    // Parse -old,count and +new,count
    let mut old_start = None;
    let mut new_start = None;

    for part in meta.split_whitespace() {
        if let Some(rest) = part.strip_prefix('-') {
            old_start = rest.split(',').next()?.parse().ok();
        } else if let Some(rest) = part.strip_prefix('+') {
            new_start = rest.split(',').next()?.parse().ok();
        }
    }

    Some((old_start?, new_start?))
}

/// Count added/removed lines in a single hunk
pub fn count_hunk_stats(hunk_body: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in hunk_body.lines() {
        match classify_diff_line(line) {
            DiffLineClass::Added => added += 1,
            DiffLineClass::Removed => removed += 1,
            DiffLineClass::Context => {}
        }
    }
    (added, removed)
}

/// Count added and removed lines from raw diff content
pub fn count_diff_stats(content: &[u8]) -> (usize, usize) {
    let text = String::from_utf8_lossy(content);
    let mut added = 0;
    let mut removed = 0;
    for line in text.lines() {
        match classify_diff_line(line) {
            DiffLineClass::Added => added += 1,
            DiffLineClass::Removed => removed += 1,
            DiffLineClass::Context => {}
        }
    }
    (added, removed)
}

/// Check if delta pager is available
pub fn has_delta() -> bool {
    std::process::Command::new("which")
        .arg("delta")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Render diff content through delta for syntax highlighting
/// Falls back to basic ANSI coloring if delta is not available
pub fn render_through_delta(content: &str) -> String {
    if content.is_empty() {
        return content.to_string();
    }

    if has_delta() {
        let mut delta = match std::process::Command::new("delta")
            .arg("--paging=never")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
        {
            Ok(p) => p,
            Err(_) => return apply_basic_diff_colors(content),
        };

        // Spawn thread to write stdin to avoid pipe deadlock on large diffs
        if let Some(mut stdin) = delta.stdin.take() {
            let content = content.to_string();
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(content.as_bytes());
            });
        }

        match delta.wait_with_output() {
            Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
            Err(_) => apply_basic_diff_colors(content),
        }
    } else {
        apply_basic_diff_colors(content)
    }
}

/// Apply basic ANSI colors to diff content (fallback when delta unavailable)
pub fn apply_basic_diff_colors(content: &str) -> String {
    content
        .lines()
        .map(|line| {
            if line.starts_with('+') && !line.starts_with("+++") {
                format!("\x1b[32m{}\x1b[0m", line) // Green
            } else if line.starts_with('-') && !line.starts_with("---") {
                format!("\x1b[31m{}\x1b[0m", line) // Red
            } else if line.starts_with("@@") {
                format!("\x1b[36m{}\x1b[0m", line) // Cyan
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse raw diff output into individual hunks for patch mode
pub fn parse_diff_into_hunks(raw_diff: &str) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();
    let mut current_file_header = String::new();
    let mut current_filename = String::new();
    let mut current_hunk_lines: Vec<&str> = Vec::new();
    let mut in_hunk = false;
    let finalize_hunk = |hunks: &mut Vec<DiffHunk>,
                         current_file_header: &str,
                         current_filename: &str,
                         current_hunk_lines: &mut Vec<&str>| {
        let hunk_body = current_hunk_lines.join("\n");
        let (added, removed) = count_hunk_stats(&hunk_body);
        let full_diff = format!("{}\n{}", current_file_header, hunk_body);
        let rendered_content = render_through_delta(&full_diff);
        let parsed_lines = parse_ansi_to_lines(&rendered_content);
        hunks.push(DiffHunk {
            file_header: current_file_header.to_string(),
            hunk_body,
            filename: current_filename.to_string(),
            lines_added: added,
            lines_removed: removed,
            rendered_content,
            parsed_lines,
        });
        current_hunk_lines.clear();
    };

    for line in raw_diff.lines() {
        let stripped = strip_ansi_escapes(line);

        if stripped.starts_with("diff --git") {
            // Save previous hunk if any
            if in_hunk && !current_hunk_lines.is_empty() {
                finalize_hunk(
                    &mut hunks,
                    &current_file_header,
                    &current_filename,
                    &mut current_hunk_lines,
                );
            }

            // Start new file
            current_file_header = line.to_string();
            in_hunk = false;

            // Extract filename from "diff --git <prefix>/path <prefix>/path"
            if let Some(last_part) = stripped.split_whitespace().last()
                && let Some((prefix, path)) = last_part.split_once('/')
                && prefix.len() == 1
            {
                current_filename = path.to_string();
            }
        } else if stripped.starts_with("@@") {
            // Save previous hunk if any
            if in_hunk && !current_hunk_lines.is_empty() {
                finalize_hunk(
                    &mut hunks,
                    &current_file_header,
                    &current_filename,
                    &mut current_hunk_lines,
                );
            }

            // Start new hunk
            in_hunk = true;
            current_hunk_lines.push(line);
        } else if in_hunk {
            // Continue current hunk
            current_hunk_lines.push(line);
        } else {
            // Part of file header (---, +++, index, etc.)
            current_file_header.push('\n');
            current_file_header.push_str(line);
        }
    }

    // Don't forget the last hunk
    if in_hunk && !current_hunk_lines.is_empty() {
        finalize_hunk(
            &mut hunks,
            &current_file_header,
            &current_filename,
            &mut current_hunk_lines,
        );
    }

    hunks
}

/// Extract file entries from hunks, aggregating stats per file
pub fn extract_file_list(hunks: &[DiffHunk]) -> Vec<FileEntry> {
    use std::collections::BTreeMap;

    // Aggregate stats by filename (BTreeMap for stable ordering)
    let mut file_stats: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for hunk in hunks {
        let entry = file_stats.entry(&hunk.filename).or_insert((0, 0));
        entry.0 += hunk.lines_added;
        entry.1 += hunk.lines_removed;
    }

    file_stats
        .into_iter()
        .map(|(filename, (lines_added, lines_removed))| FileEntry {
            filename: filename.to_string(),
            lines_added,
            lines_removed,
            start_line: 0, // Will be mapped later
            is_new: false, // Can't determine from hunks alone
        })
        .collect()
}

/// Get file list using git diff --numstat --summary (single command for stats and status)
pub fn get_file_list_numstat(
    path: &Path,
    diff_arg: &str,
    include_untracked: bool,
) -> Vec<FileEntry> {
    use std::collections::HashMap;

    let mut file_map: HashMap<String, FileEntry> = HashMap::new();

    let Ok(mut cmd) = crate::git::unattended_git(Some(path)) else {
        return Vec::new();
    };
    cmd.args([
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--numstat",
        "--summary",
    ]);
    if !diff_arg.is_empty() {
        cmd.arg(diff_arg);
    }

    if let Ok(output) = cmd.output() {
        let output_str = String::from_utf8_lossy(&output.stdout);
        for line in output_str.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("create mode ") {
                // Summary line: "create mode 100644 filename"
                // Skip the mode (e.g., "100644") to get the filename
                if let Some(filename) = rest.split_once(' ').map(|(_, f)| f) {
                    file_map
                        .entry(filename.to_string())
                        .or_insert_with(|| FileEntry {
                            filename: filename.to_string(),
                            lines_added: 0,
                            lines_removed: 0,
                            start_line: 0,
                            is_new: true,
                        })
                        .is_new = true;
                }
            } else if !trimmed.starts_with("delete mode")
                && !trimmed.starts_with("rename")
                && !trimmed.starts_with("copy")
                && !trimmed.starts_with("mode change")
            {
                // Numstat line: "added\tremoved\tfilename"
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 3 {
                    let added = parts[0].parse().unwrap_or(0);
                    let removed = parts[1].parse().unwrap_or(0);
                    let filename = parts[2].to_string();

                    let entry = file_map.entry(filename.clone()).or_insert(FileEntry {
                        filename,
                        lines_added: 0,
                        lines_removed: 0,
                        start_line: 0,
                        is_new: false,
                    });
                    entry.lines_added = added;
                    entry.lines_removed = removed;
                }
            }
        }
    }

    let mut entries: Vec<FileEntry> = file_map.into_values().collect();

    // Include untracked files if requested (separate command required)
    if include_untracked
        && let Ok(mut command) = crate::git::unattended_git(Some(path))
        && let Ok(out) = command
            .args(["ls-files", "--others", "--exclude-standard"])
            .output()
    {
        for file in String::from_utf8_lossy(&out.stdout).lines() {
            if !file.trim().is_empty() {
                let file_path = path.join(file);
                let lines_added = std::fs::read_to_string(&file_path)
                    .map(|c| c.lines().count())
                    .unwrap_or(0);
                entries.push(FileEntry {
                    filename: file.to_string(),
                    lines_added,
                    lines_removed: 0,
                    start_line: 0,
                    is_new: true,
                });
            }
        }
    }

    entries
}

/// Map file start_line offsets by scanning parsed_lines for file headers
pub fn map_file_offsets(file_list: &mut [FileEntry], parsed_lines: &[Line]) {
    if file_list.is_empty() {
        return;
    }

    // For each file, scan all lines to find where it first appears
    for file in file_list.iter_mut() {
        let target = &file.filename;

        for (line_idx, line) in parsed_lines.iter().enumerate() {
            let text = line.to_string();

            // Match filename in various diff formats
            let is_match = text.ends_with(target)
                || text.ends_with(&format!("/{}", target))
                || text.contains(&format!("/{} ", target))
                || text.contains(&format!(" {} ", target))
                || (text.starts_with("diff --git") && text.contains(target));

            if is_match {
                file.start_line = line_idx;
                break;
            }
        }
    }

    // Sort file_list by start_line to match diff order
    file_list.sort_by_key(|f| f.start_line);
}

/// Get diff content, optionally piped through delta for syntax highlighting
/// Returns (content, lines_added, lines_removed, hunks)
pub fn get_diff_content(
    path: &Path,
    diff_arg: &str,
    include_untracked: bool,
    parse_hunks: bool,
) -> Result<(String, usize, usize, Vec<DiffHunk>), String> {
    let diff_content = raw_diff(path, diff_arg, include_untracked)?;
    render_diff(diff_content, parse_hunks)
}

/// Uncolored `git diff <diff_arg>` of `path`, with untracked files as new
/// files when `include_untracked`.
pub fn raw_diff(path: &Path, diff_arg: &str, include_untracked: bool) -> Result<Vec<u8>, String> {
    // Run git diff without color - delta will add syntax highlighting
    let mut cmd =
        crate::git::pinned_git(path).map_err(|e| format!("Cannot validate Git metadata: {e}"))?;
    cmd.args(["diff", "--no-ext-diff", "--no-textconv"]);

    // Only add diff_arg if non-empty (empty = unstaged changes only)
    if !diff_arg.is_empty() {
        cmd.arg(diff_arg);
    }

    let git_output = cmd
        .output()
        .map_err(|e| format!("Error running git diff: {}", e))?;

    let mut diff_content = git_output.stdout;

    // For uncommitted changes, also include untracked files
    if include_untracked {
        let untracked_diff = get_untracked_files_diff(path)?;
        if !untracked_diff.is_empty() {
            diff_content.extend_from_slice(untracked_diff.as_bytes());
        }
    }
    Ok(diff_content)
}

/// Colored display text of a raw diff, with its line counts and, when
/// `parse_hunks`, its hunks.
pub fn render_diff(
    diff_content: Vec<u8>,
    parse_hunks: bool,
) -> Result<(String, usize, usize, Vec<DiffHunk>), String> {
    // Count stats before any transformation
    let (lines_added, lines_removed) = count_diff_stats(&diff_content);

    // Parse hunks from raw diff (before delta processing)
    let raw_diff = String::from_utf8_lossy(&diff_content).to_string();
    let hunks = if parse_hunks {
        parse_diff_into_hunks(&raw_diff)
    } else {
        Vec::new()
    };

    // If empty, return as-is
    if diff_content.is_empty() {
        return Ok((raw_diff, lines_added, lines_removed, hunks));
    }

    // If delta not available, apply basic colors
    if !has_delta() {
        let colored = apply_basic_diff_colors(&raw_diff);
        return Ok((colored, lines_added, lines_removed, hunks));
    }

    // Pipe through delta for syntax highlighting
    let mut delta = std::process::Command::new("delta")
        .arg("--paging=never")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Error running delta: {}", e))?;

    // Spawn thread to write stdin to avoid pipe deadlock on large diffs
    if let Some(mut stdin) = delta.stdin.take() {
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(&diff_content);
        });
    }

    let delta_output = delta
        .wait_with_output()
        .map_err(|e| format!("Error reading delta output: {}", e))?;

    Ok((
        String::from_utf8_lossy(&delta_output.stdout).to_string(),
        lines_added,
        lines_removed,
        hunks,
    ))
}

/// Generate diff output for untracked files (new files not yet staged)
pub fn get_untracked_files_diff(path: &Path) -> Result<String, String> {
    // Get list of untracked files
    let output = crate::git::pinned_git(path)
        .map_err(|e| format!("Cannot validate Git metadata: {e}"))?
        .arg("ls-files")
        .arg("--others")
        .arg("--exclude-standard")
        .output()
        .map_err(|e| format!("Error listing untracked files: {}", e))?;

    let output_str = String::from_utf8_lossy(&output.stdout).to_string();
    let untracked_files: Vec<&str> = output_str.lines().filter(|l| !l.is_empty()).collect();

    if untracked_files.is_empty() {
        return Ok(String::new());
    }

    // Generate diff for each untracked file using git diff --no-index
    let mut result = String::new();
    for file in untracked_files {
        let file_path = path.join(file);
        if !file_path.is_file() {
            continue;
        }

        // Use git diff --no-index to generate proper diff format for new files
        let diff_output = crate::git::pinned_git(path).and_then(|mut command| {
            command
                .args([
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-index",
                    "/dev/null",
                ])
                .arg(file);
            command.output().map_err(anyhow::Error::from)
        });

        if let Ok(output) = diff_output {
            // git diff --no-index returns exit code 1 when files differ, which is expected
            let diff_text = String::from_utf8_lossy(&output.stdout);
            if !diff_text.is_empty() {
                result.push_str(&diff_text);
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hunk_header() {
        assert_eq!(parse_hunk_header("@@ -10,5 +12,7 @@"), Some((10, 12)));
        assert_eq!(parse_hunk_header("@@ -1,3 +1,4 @@ fn main()"), Some((1, 1)));
        assert_eq!(parse_hunk_header("invalid"), None);
    }

    #[test]
    fn test_count_hunk_stats() {
        let hunk = "@@ -1,3 +1,4 @@\n context\n+added\n-removed\n context";
        let (added, removed) = count_hunk_stats(hunk);
        assert_eq!(added, 1);
        assert_eq!(removed, 1);
    }

    #[test]
    fn test_count_diff_stats() {
        let diff = b"diff --git a/file.rs b/file.rs\n--- a/file.rs\n+++ b/file.rs\n+new line\n-old line\n context";
        let (added, removed) = count_diff_stats(diff);
        assert_eq!(added, 1);
        assert_eq!(removed, 1);
    }

    #[test]
    fn test_apply_basic_diff_colors() {
        let input = "+added\n-removed\n@@header@@\n context";
        let colored = apply_basic_diff_colors(input);
        assert!(colored.contains("\x1b[32m+added\x1b[0m")); // Green
        assert!(colored.contains("\x1b[31m-removed\x1b[0m")); // Red
        assert!(colored.contains("\x1b[36m@@header@@\x1b[0m")); // Cyan
        assert!(colored.contains(" context")); // No color
    }

    #[test]
    fn test_extract_file_list() {
        let hunks = vec![
            DiffHunk {
                file_header: String::new(),
                hunk_body: String::new(),
                filename: "file1.rs".to_string(),
                lines_added: 5,
                lines_removed: 2,
                rendered_content: String::new(),
                parsed_lines: vec![],
            },
            DiffHunk {
                file_header: String::new(),
                hunk_body: String::new(),
                filename: "file1.rs".to_string(),
                lines_added: 3,
                lines_removed: 1,
                rendered_content: String::new(),
                parsed_lines: vec![],
            },
            DiffHunk {
                file_header: String::new(),
                hunk_body: String::new(),
                filename: "file2.rs".to_string(),
                lines_added: 10,
                lines_removed: 0,
                rendered_content: String::new(),
                parsed_lines: vec![],
            },
        ];

        let files = extract_file_list(&hunks);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].filename, "file1.rs");
        assert_eq!(files[0].lines_added, 8); // 5 + 3
        assert_eq!(files[0].lines_removed, 3); // 2 + 1
        assert_eq!(files[1].filename, "file2.rs");
        assert_eq!(files[1].lines_added, 10);
    }

    #[test]
    fn test_diff_hunk_split_no_context_gap() {
        // Hunk with continuous changes - cannot split
        let hunk = DiffHunk {
            file_header: "diff --git a/test.rs b/test.rs".to_string(),
            hunk_body: "@@ -1,3 +1,4 @@\n+line1\n+line2\n+line3".to_string(),
            filename: "test.rs".to_string(),
            lines_added: 3,
            lines_removed: 0,
            rendered_content: String::new(),
            parsed_lines: vec![],
        };
        assert!(hunk.split().is_none());
    }

    #[test]
    fn test_diff_hunk_split_with_context_gap() {
        // Hunk with context line between changes - can split
        let hunk = DiffHunk {
            file_header: "diff --git a/test.rs b/test.rs".to_string(),
            hunk_body: "@@ -1,5 +1,6 @@\n+added1\n context\n+added2".to_string(),
            filename: "test.rs".to_string(),
            lines_added: 2,
            lines_removed: 0,
            rendered_content: String::new(),
            parsed_lines: vec![],
        };
        let result = hunk.split();
        assert!(result.is_some());
        let hunks = result.unwrap();
        assert_eq!(hunks.len(), 2);
    }

    #[test]
    fn test_map_file_offsets() {
        use ratatui::text::Line;

        let mut files = vec![
            FileEntry {
                filename: "src/main.rs".to_string(),
                lines_added: 5,
                lines_removed: 2,
                is_new: false,
                start_line: 0,
            },
            FileEntry {
                filename: "src/lib.rs".to_string(),
                lines_added: 3,
                lines_removed: 1,
                is_new: false,
                start_line: 0,
            },
        ];

        let parsed_lines = vec![
            Line::raw("diff --git a/src/main.rs b/src/main.rs"),
            Line::raw("+added line"),
            Line::raw("-removed line"),
            Line::raw("diff --git a/src/lib.rs b/src/lib.rs"),
            Line::raw("+another add"),
        ];

        map_file_offsets(&mut files, &parsed_lines);

        assert_eq!(files[0].start_line, 0);
        assert_eq!(files[1].start_line, 3);
    }
}
