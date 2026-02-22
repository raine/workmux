use anyhow::{Context, Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::debug;

use crate::cmd::Cmd;
use crate::config::MuxMode;
use crate::shell::shell_quote;

use super::{Vcs, VcsStatus, WorkspaceNotFound};

/// Jujutsu (jj) implementation of the Vcs trait.
pub struct JjVcs;

impl JjVcs {
    pub fn new() -> Self {
        JjVcs
    }
}

/// Helper to return a "not yet implemented" error for jj operations.
fn jj_todo(operation: &str) -> anyhow::Error {
    anyhow!("jj support not yet implemented: {}", operation)
}

/// Run a jj command, optionally in a specific workdir.
/// Adds `--quiet` to suppress jj's informational messages.
fn jj_cmd<'a>(workdir: Option<&'a Path>) -> Cmd<'a> {
    let cmd = Cmd::new("jj").arg("--quiet");
    match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    }
}

/// Find the jj repo root by walking up from CWD looking for .jj/.
fn find_jj_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    for dir in cwd.ancestors() {
        if dir.join(".jj").is_dir() {
            return Ok(dir.to_path_buf());
        }
    }
    Err(anyhow!("Not in a jj repository"))
}

/// Find the jj repo root starting from a specific directory.
fn find_jj_root_for(start: &Path) -> Result<PathBuf> {
    for dir in start.ancestors() {
        if dir.join(".jj").is_dir() {
            return Ok(dir.to_path_buf());
        }
    }
    Err(anyhow!("Not in a jj repository: {}", start.display()))
}

/// Parse `jj workspace list` output.
/// Format: `<name>: <change_id_short> <commit_id_short> <description>`
fn parse_workspace_list(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let name = line.split(':').next()?.trim();
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

/// Read metadata directly from .jj/repo/config.toml (for batch operations).
/// Returns the raw content of the config file, or empty string if not found.
fn read_jj_repo_config(repo_root: &Path) -> String {
    let config_path = repo_root.join(".jj").join("repo").join("config.toml");
    std::fs::read_to_string(config_path).unwrap_or_default()
}

impl Vcs for JjVcs {
    fn name(&self) -> &str {
        "jj"
    }

    // ── Repo detection ───────────────────────────────────────────────

    fn is_repo(&self) -> Result<bool> {
        let cwd = std::env::current_dir()?;
        for dir in cwd.ancestors() {
            if dir.join(".jj").is_dir() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_commits(&self) -> Result<bool> {
        // jj always has at least a root commit
        Ok(true)
    }

    fn get_repo_root(&self) -> Result<PathBuf> {
        find_jj_root()
    }

    fn get_repo_root_for(&self, dir: &Path) -> Result<PathBuf> {
        find_jj_root_for(dir)
    }

    fn get_main_workspace_root(&self) -> Result<PathBuf> {
        // The main workspace root is the directory containing the real .jj/
        // (not a symlink). Walk up from CWD to find it.
        find_jj_root()
    }

    fn get_shared_dir(&self) -> Result<PathBuf> {
        // For jj, the shared directory is the repo root (where .jj/ lives).
        // This is used for running cleanup commands from a stable directory.
        find_jj_root()
    }

    fn is_path_ignored(&self, repo_path: &Path, file_path: &str) -> bool {
        // Check if the path matches .gitignore patterns (jj respects them)
        std::process::Command::new("jj")
            .args(["file", "show", file_path])
            .current_dir(repo_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(false)
    }

    // ── Workspace lifecycle ──────────────────────────────────────────

    fn workspace_exists(&self, branch_name: &str) -> Result<bool> {
        // Check if any workspace has a bookmark matching this branch name
        match self.get_workspace_path(branch_name) {
            Ok(_) => Ok(true),
            Err(e) => {
                if e.is::<WorkspaceNotFound>() {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    fn create_workspace(
        &self,
        path: &Path,
        branch: &str,
        create_branch: bool,
        base: Option<&str>,
        _track_upstream: bool,
    ) -> Result<()> {
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("Invalid workspace path"))?;

        // Use the directory name as the workspace name
        let handle = path
            .file_name()
            .ok_or_else(|| anyhow!("Invalid workspace path: no directory name"))?
            .to_string_lossy();

        if create_branch {
            // Create workspace from base (or @)
            let base_rev = base.unwrap_or("@");

            // First create the workspace
            jj_cmd(None)
                .args(&["workspace", "add", path_str, "--name", &handle, "--revision", base_rev])
                .run()
                .context("Failed to create jj workspace")?;

            // Create a bookmark pointing to the new workspace's working copy
            jj_cmd(Some(path))
                .args(&["bookmark", "create", branch, "-r", "@"])
                .run()
                .with_context(|| format!("Failed to create bookmark '{}'", branch))?;
        } else {
            // Branch already exists - create workspace and edit the bookmark's change
            jj_cmd(None)
                .args(&["workspace", "add", path_str, "--name", &handle])
                .run()
                .context("Failed to create jj workspace")?;

            // Edit the existing bookmark's change in the new workspace
            jj_cmd(Some(path))
                .args(&["edit", branch])
                .run()
                .with_context(|| format!("Failed to edit bookmark '{}' in workspace", branch))?;
        }

        // Store the path in workmux metadata for later lookup
        self.set_workspace_meta(&handle, "path", path_str)?;

        Ok(())
    }

    fn list_workspaces(&self) -> Result<Vec<(PathBuf, String)>> {
        let root = find_jj_root()?;

        // Get workspace names from jj
        let output = jj_cmd(Some(&root))
            .args(&["workspace", "list"])
            .run_and_capture_stdout()
            .context("Failed to list jj workspaces")?;

        let workspace_names = parse_workspace_list(&output);

        let mut result = Vec::new();
        for name in &workspace_names {
            // Get the stored path from metadata, or derive from workspace name
            let path = self
                .get_workspace_meta(name, "path")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    if name == "default" {
                        root.clone()
                    } else {
                        root.parent()
                            .unwrap_or(&root)
                            .join(name)
                    }
                });

            // Get the bookmark associated with this workspace
            // Query the bookmarks on the workspace's working copy
            let bookmark = if path.exists() {
                jj_cmd(Some(&path))
                    .args(&["log", "-r", "@", "--no-graph", "-T", "bookmarks"])
                    .run_and_capture_stdout()
                    .ok()
                    .and_then(|s| {
                        let trimmed = s.trim().to_string();
                        // jj may output multiple bookmarks separated by spaces;
                        // take the first non-empty one
                        trimmed
                            .split_whitespace()
                            .next()
                            .map(|b| b.trim_end_matches('*').to_string())
                    })
                    .unwrap_or_else(|| name.clone())
            } else {
                name.clone()
            };

            result.push((path, bookmark));
        }

        Ok(result)
    }

    fn find_workspace(&self, name: &str) -> Result<(PathBuf, String)> {
        let workspaces = self.list_workspaces()?;

        // First: try to match by handle (directory name)
        for (path, branch) in &workspaces {
            if let Some(dir_name) = path.file_name()
                && dir_name.to_string_lossy() == name
            {
                return Ok((path.clone(), branch.clone()));
            }
        }

        // Second: try to match by bookmark/branch name
        for (path, branch) in &workspaces {
            if branch == name {
                return Ok((path.clone(), branch.clone()));
            }
        }

        // Third: try to match by workspace name
        let root = find_jj_root()?;
        let stored_path = self
            .get_workspace_meta(name, "path")
            .map(PathBuf::from);

        if let Some(path) = stored_path {
            // Get the bookmark for this workspace
            let bookmark = if path.exists() {
                jj_cmd(Some(&path))
                    .args(&["log", "-r", "@", "--no-graph", "-T", "bookmarks"])
                    .run_and_capture_stdout()
                    .ok()
                    .and_then(|s| {
                        s.trim()
                            .split_whitespace()
                            .next()
                            .map(|b| b.trim_end_matches('*').to_string())
                    })
                    .unwrap_or_else(|| name.to_string())
            } else {
                name.to_string()
            };
            return Ok((path, bookmark));
        }

        // Check if the workspace name exists in jj
        let output = jj_cmd(Some(&root))
            .args(&["workspace", "list"])
            .run_and_capture_stdout()
            .unwrap_or_default();
        let ws_names = parse_workspace_list(&output);
        if ws_names.contains(&name.to_string()) {
            // Workspace exists but no stored path - derive it
            let path = if name == "default" {
                root.clone()
            } else {
                root.parent().unwrap_or(&root).join(name)
            };
            return Ok((path, name.to_string()));
        }

        Err(WorkspaceNotFound(name.to_string()).into())
    }

    fn get_workspace_path(&self, branch: &str) -> Result<PathBuf> {
        let workspaces = self.list_workspaces()?;

        for (path, ws_branch) in workspaces {
            if ws_branch == branch {
                return Ok(path);
            }
        }

        Err(WorkspaceNotFound(branch.to_string()).into())
    }

    fn prune_workspaces(&self, shared_dir: &Path) -> Result<()> {
        // In jj, forgetting stale workspaces serves the same purpose as git worktree prune.
        // List workspaces and forget any whose paths no longer exist on disk.
        let output = jj_cmd(Some(shared_dir))
            .args(&["workspace", "list"])
            .run_and_capture_stdout()
            .unwrap_or_default();

        let ws_names = parse_workspace_list(&output);
        for name in ws_names {
            if name == "default" {
                continue; // Never prune the default workspace
            }

            let path = self
                .get_workspace_meta(&name, "path")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    shared_dir.parent().unwrap_or(shared_dir).join(&name)
                });

            if !path.exists() {
                debug!(workspace = %name, "jj:pruning stale workspace");
                let _ = jj_cmd(Some(shared_dir))
                    .args(&["workspace", "forget", &name])
                    .run();
            }
        }

        Ok(())
    }

    // ── Workspace metadata ───────────────────────────────────────────

    fn set_workspace_meta(&self, handle: &str, key: &str, value: &str) -> Result<()> {
        let root = find_jj_root()?;
        let config_key = format!("workmux.worktree.{}.{}", handle, key);
        jj_cmd(Some(&root))
            .args(&["config", "set", "--repo", &config_key, value])
            .run()
            .with_context(|| format!("Failed to set jj config {}", config_key))?;
        Ok(())
    }

    fn get_workspace_meta(&self, handle: &str, key: &str) -> Option<String> {
        let root = find_jj_root().ok()?;
        let config_key = format!("workmux.worktree.{}.{}", handle, key);
        jj_cmd(Some(&root))
            .args(&["config", "get", &config_key])
            .run_and_capture_stdout()
            .ok()
            .filter(|s| !s.is_empty())
    }

    fn get_workspace_mode(&self, handle: &str) -> MuxMode {
        match self.get_workspace_meta(handle, "mode") {
            Some(mode) if mode == "session" => MuxMode::Session,
            _ => MuxMode::Window,
        }
    }

    fn get_all_workspace_modes(&self) -> HashMap<String, MuxMode> {
        let root = match find_jj_root() {
            Ok(r) => r,
            Err(_) => return HashMap::new(),
        };

        // Parse the config file directly for batch reading
        let config_content = read_jj_repo_config(&root);
        let mut modes = HashMap::new();

        // Look for lines matching workmux.worktree.<handle>.mode pattern
        // The TOML structure is nested tables, but we can grep for the relevant lines
        // by looking for key = "value" under [workmux.worktree.<handle>] sections
        let mut current_handle: Option<String> = None;
        for line in config_content.lines() {
            let trimmed = line.trim();

            // Match [workmux.worktree.<handle>] table headers
            if let Some(rest) = trimmed.strip_prefix("[workmux.worktree.") {
                if let Some(handle) = rest.strip_suffix(']') {
                    current_handle = Some(handle.to_string());
                } else {
                    current_handle = None;
                }
            } else if let Some(ref handle) = current_handle {
                // Match mode = "session" or mode = "window"
                if let Some(rest) = trimmed.strip_prefix("mode") {
                    let rest = rest.trim();
                    if let Some(value) = rest.strip_prefix('=') {
                        let value = value.trim().trim_matches('"');
                        let mode = if value == "session" {
                            MuxMode::Session
                        } else {
                            MuxMode::Window
                        };
                        modes.insert(handle.clone(), mode);
                    }
                }
            } else if trimmed.starts_with('[') {
                // New section that isn't workmux.worktree
                current_handle = None;
            }
        }

        modes
    }

    fn remove_workspace_meta(&self, handle: &str) -> Result<()> {
        let root = find_jj_root()?;

        // Read the config file, remove the [workmux.worktree.<handle>] section, write back
        let config_path = root.join(".jj").join("repo").join("config.toml");
        let content = std::fs::read_to_string(&config_path).unwrap_or_default();

        if content.is_empty() {
            return Ok(());
        }

        let section_header = format!("[workmux.worktree.{}]", handle);
        let mut new_lines = Vec::new();
        let mut in_target_section = false;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed == section_header {
                in_target_section = true;
                continue;
            }
            if in_target_section && trimmed.starts_with('[') {
                // We've hit the next section
                in_target_section = false;
            }
            if !in_target_section {
                new_lines.push(line);
            }
        }

        let new_content = new_lines.join("\n");
        // Only write if we actually removed something
        if new_content.len() != content.len() {
            std::fs::write(&config_path, new_content)
                .context("Failed to update jj repo config")?;
        }

        Ok(())
    }

    // ── Branch/bookmark operations ───────────────────────────────────

    fn get_default_branch(&self) -> Result<String> {
        self.get_default_branch_in(None)
    }

    fn get_default_branch_in(&self, workdir: Option<&Path>) -> Result<String> {
        let root = match workdir {
            Some(d) => find_jj_root_for(d).unwrap_or_else(|_| find_jj_root().unwrap_or_default()),
            None => find_jj_root()?,
        };

        // Try to detect trunk bookmark: check for main, then master
        if self.branch_exists_in("main", Some(&root))? {
            debug!("jj:default branch 'main'");
            return Ok("main".to_string());
        }

        if self.branch_exists_in("master", Some(&root))? {
            debug!("jj:default branch 'master'");
            return Ok("master".to_string());
        }

        // Try checking jj's revset alias for trunk()
        if let Ok(output) = jj_cmd(Some(&root))
            .args(&["config", "get", "revset-aliases.trunk()"])
            .run_and_capture_stdout()
        {
            if !output.is_empty() {
                debug!(trunk_alias = %output, "jj:default branch from trunk() alias");
                return Ok(output);
            }
        }

        Err(anyhow!(
            "Could not determine the default branch. \
            Please specify it in .workmux.yaml using the 'main_branch' key."
        ))
    }

    fn branch_exists(&self, name: &str) -> Result<bool> {
        self.branch_exists_in(name, None)
    }

    fn branch_exists_in(&self, name: &str, workdir: Option<&Path>) -> Result<bool> {
        let root = match workdir {
            Some(d) => find_jj_root_for(d).unwrap_or_else(|_| find_jj_root().unwrap_or_default()),
            None => find_jj_root()?,
        };

        // Use jj bookmark list with exact name filter
        let output = jj_cmd(Some(&root))
            .args(&["bookmark", "list", "--all", "-T", "name ++ \"\\n\""])
            .run_and_capture_stdout()
            .unwrap_or_default();

        Ok(output.lines().any(|line| line.trim() == name))
    }

    fn get_current_branch(&self) -> Result<String> {
        let output = jj_cmd(None)
            .args(&["log", "-r", "@", "--no-graph", "-T", "bookmarks"])
            .run_and_capture_stdout()
            .context("Failed to get current bookmark")?;

        let trimmed = output.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("No bookmark on current change"));
        }

        // Take the first bookmark, strip trailing '*' (indicates local-only)
        let bookmark = trimmed
            .split_whitespace()
            .next()
            .unwrap_or(trimmed)
            .trim_end_matches('*');

        Ok(bookmark.to_string())
    }

    fn list_checkout_branches(&self) -> Result<Vec<String>> {
        let output = jj_cmd(None)
            .args(&["bookmark", "list", "--all", "-T", "name ++ \"\\n\""])
            .run_and_capture_stdout()
            .context("Failed to list jj bookmarks")?;

        Ok(output
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect())
    }

    fn delete_branch(&self, name: &str, _force: bool, shared_dir: &Path) -> Result<()> {
        // jj bookmark delete has no force distinction
        jj_cmd(Some(shared_dir))
            .args(&["bookmark", "delete", name])
            .run()
            .with_context(|| format!("Failed to delete bookmark '{}'", name))?;
        Ok(())
    }

    fn get_merge_base(&self, main_branch: &str) -> Result<String> {
        // For jj, check if the bookmark exists locally
        if self.branch_exists(main_branch)? {
            return Ok(main_branch.to_string());
        }

        // Check for remote tracking bookmark
        let remote_main = format!("{}@origin", main_branch);
        let output = jj_cmd(None)
            .args(&["bookmark", "list", "--all", "-T", "name ++ \"\\n\""])
            .run_and_capture_stdout()
            .unwrap_or_default();

        if output.lines().any(|l| l.trim() == remote_main) {
            Ok(remote_main)
        } else {
            Ok(main_branch.to_string())
        }
    }

    fn get_unmerged_branches(&self, base: &str) -> Result<HashSet<String>> {
        // List all local bookmarks and check which ones are not ancestors of base
        let output = jj_cmd(None)
            .args(&["bookmark", "list", "-T", "name ++ \"\\n\""])
            .run_and_capture_stdout()
            .unwrap_or_default();

        let mut unmerged = HashSet::new();
        for line in output.lines() {
            let bookmark = line.trim();
            if bookmark.is_empty() || bookmark == base {
                continue;
            }

            // Check if this bookmark's changes are all ancestors of base
            // Using revset: if bookmark is merged into base, `bookmark ~ ::base` is empty
            let revset = format!("{} ~ ::{}", bookmark, base);
            if let Ok(result) = jj_cmd(None)
                .args(&["log", "-r", &revset, "--no-graph", "-T", "change_id"])
                .run_and_capture_stdout()
            {
                if !result.trim().is_empty() {
                    unmerged.insert(bookmark.to_string());
                }
            }
        }

        Ok(unmerged)
    }

    fn get_gone_branches(&self) -> Result<HashSet<String>> {
        // In jj, "gone" branches are local bookmarks whose remote tracking
        // bookmark has been deleted. List bookmarks and check for this.
        // For now, return empty set - jj handles this differently via
        // `jj bookmark list --conflicted` and remote tracking.
        Ok(HashSet::new())
    }

    // ── Base branch tracking ─────────────────────────────────────────

    fn set_branch_base(&self, branch: &str, base: &str) -> Result<()> {
        let root = find_jj_root()?;
        let config_key = format!("workmux.base.{}", branch);
        jj_cmd(Some(&root))
            .args(&["config", "set", "--repo", &config_key, base])
            .run()
            .context("Failed to set workmux base config")?;
        Ok(())
    }

    fn get_branch_base(&self, branch: &str) -> Result<String> {
        self.get_branch_base_in(branch, None)
    }

    fn get_branch_base_in(&self, branch: &str, workdir: Option<&Path>) -> Result<String> {
        let root = match workdir {
            Some(d) => find_jj_root_for(d).unwrap_or_else(|_| find_jj_root().unwrap_or_default()),
            None => find_jj_root()?,
        };

        let config_key = format!("workmux.base.{}", branch);
        let output = jj_cmd(Some(&root))
            .args(&["config", "get", &config_key])
            .run_and_capture_stdout()
            .context("Failed to get workmux base config")?;

        if output.is_empty() {
            return Err(anyhow!("No workmux-base found for branch '{}'", branch));
        }

        Ok(output)
    }

    // ── Status ───────────────────────────────────────────────────────

    fn get_status(&self, worktree: &Path) -> VcsStatus {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();

        // Get the current bookmark name
        let branch = jj_cmd(Some(worktree))
            .args(&["log", "-r", "@", "--no-graph", "-T", "bookmarks"])
            .run_and_capture_stdout()
            .ok()
            .and_then(|s| {
                s.trim()
                    .split_whitespace()
                    .next()
                    .map(|b| b.trim_end_matches('*').to_string())
            });

        // Check if working copy has changes (jj diff --stat)
        let is_dirty = jj_cmd(Some(worktree))
            .args(&["diff", "--stat"])
            .run_and_capture_stdout()
            .ok()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);

        let branch_ref = match &branch {
            Some(b) => b.clone(),
            None => {
                return VcsStatus {
                    is_dirty,
                    cached_at: now,
                    branch: None,
                    ..Default::default()
                };
            }
        };

        // Get base branch
        let base_branch = self
            .get_branch_base_in(&branch_ref, Some(worktree))
            .ok()
            .or_else(|| self.get_default_branch_in(Some(worktree)).ok())
            .unwrap_or_else(|| "main".to_string());

        // Check for conflicts
        let has_conflict = jj_cmd(Some(worktree))
            .args(&["log", "-r", "conflicts()", "--no-graph", "-T", "change_id"])
            .run_and_capture_stdout()
            .ok()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);

        // Get diff stats vs base
        let (lines_added, lines_removed) = jj_cmd(Some(worktree))
            .args(&["diff", "--stat", "-r", &format!("{}..@", base_branch)])
            .run_and_capture_stdout()
            .ok()
            .map(|s| parse_jj_diff_stat_totals(&s))
            .unwrap_or((0, 0));

        // Get uncommitted diff stats (working copy changes)
        let (uncommitted_added, uncommitted_removed) = jj_cmd(Some(worktree))
            .args(&["diff", "--stat"])
            .run_and_capture_stdout()
            .ok()
            .map(|s| parse_jj_diff_stat_totals(&s))
            .unwrap_or((0, 0));

        VcsStatus {
            ahead: 0, // jj doesn't have ahead/behind in the same way
            behind: 0,
            has_conflict,
            is_dirty,
            lines_added,
            lines_removed,
            uncommitted_added,
            uncommitted_removed,
            cached_at: now,
            base_branch,
            branch: Some(branch_ref),
            has_upstream: false, // jj tracks this differently
        }
    }

    fn has_uncommitted_changes(&self, worktree: &Path) -> Result<bool> {
        // In jj, the working copy is always a commit. "Uncommitted changes"
        // means the working copy has modifications (jj diff shows output).
        let output = jj_cmd(Some(worktree))
            .args(&["diff", "--stat"])
            .run_and_capture_stdout()?;
        Ok(!output.trim().is_empty())
    }

    fn has_tracked_changes(&self, worktree: &Path) -> Result<bool> {
        // jj auto-tracks all files, so this is the same as has_uncommitted_changes
        self.has_uncommitted_changes(worktree)
    }

    fn has_untracked_files(&self, _worktree: &Path) -> Result<bool> {
        // jj auto-tracks all files (respecting .gitignore), so there are no
        // "untracked" files in the git sense.
        Ok(false)
    }

    fn has_staged_changes(&self, worktree: &Path) -> Result<bool> {
        // jj has no staging area - "staged" maps to "has changes"
        self.has_uncommitted_changes(worktree)
    }

    fn has_unstaged_changes(&self, worktree: &Path) -> Result<bool> {
        // jj has no staging area - "unstaged" maps to "has changes"
        self.has_uncommitted_changes(worktree)
    }

    // ── Merge operations ─────────────────────────────────────────────

    fn commit_with_editor(&self, worktree: &Path) -> Result<()> {
        // `jj commit` creates a new change on top of the current one,
        // prompting for a description via the editor.
        let status = std::process::Command::new("jj")
            .arg("commit")
            .current_dir(worktree)
            .status()
            .context("Failed to run jj commit")?;

        if !status.success() {
            return Err(anyhow!("Commit was aborted or failed"));
        }

        Ok(())
    }

    fn merge_in_workspace(&self, worktree: &Path, branch: &str) -> Result<()> {
        // In jj, merge creates a new change with multiple parents:
        // `jj new @ <branch>` creates a merge commit
        jj_cmd(Some(worktree))
            .args(&["new", "@", branch])
            .run()
            .context("Failed to create merge commit")?;
        Ok(())
    }

    fn rebase_onto_base(&self, worktree: &Path, base: &str) -> Result<()> {
        // `jj rebase -s @ -d <base>` rebases the current change onto base
        jj_cmd(Some(worktree))
            .args(&["rebase", "-s", "@", "-d", base])
            .run()
            .with_context(|| format!("Failed to rebase onto '{}'", base))?;
        Ok(())
    }

    fn merge_squash(&self, worktree: &Path, branch: &str) -> Result<()> {
        // In jj, squash merges the content from source into the current change.
        // First rebase the branch onto @, then squash.
        // Alternative: `jj squash --from <branch> --into @`
        jj_cmd(Some(worktree))
            .args(&["squash", "--from", branch, "--into", "@"])
            .run()
            .context("Failed to perform squash merge")?;
        Ok(())
    }

    fn switch_branch(&self, worktree: &Path, branch: &str) -> Result<()> {
        // `jj edit <bookmark>` switches the working copy to the bookmark's change
        jj_cmd(Some(worktree))
            .args(&["edit", branch])
            .run()
            .with_context(|| format!("Failed to edit bookmark '{}'", branch))?;
        Ok(())
    }

    fn stash_push(&self, _msg: &str, _untracked: bool, _patch: bool) -> Result<()> {
        // jj doesn't need stash - working copy is always committed
        Ok(())
    }

    fn stash_pop(&self, _worktree: &Path) -> Result<()> {
        // jj doesn't need stash - working copy is always committed
        Ok(())
    }

    fn reset_hard(&self, worktree: &Path) -> Result<()> {
        // `jj restore` restores the working copy to match the parent change
        jj_cmd(Some(worktree))
            .args(&["restore"])
            .run()
            .context("Failed to restore working copy")?;
        Ok(())
    }

    fn abort_merge(&self, worktree: &Path) -> Result<()> {
        // `jj undo` undoes the last operation (e.g., a merge)
        jj_cmd(Some(worktree))
            .args(&["undo"])
            .run()
            .context("Failed to undo operation")?;
        Ok(())
    }

    // ── Remotes ──────────────────────────────────────────────────────

    fn list_remotes(&self) -> Result<Vec<String>> {
        Err(jj_todo("list_remotes"))
    }

    fn remote_exists(&self, _name: &str) -> Result<bool> {
        Err(jj_todo("remote_exists"))
    }

    fn fetch_remote(&self, _remote: &str) -> Result<()> {
        Err(jj_todo("fetch_remote"))
    }

    fn fetch_prune(&self) -> Result<()> {
        Err(jj_todo("fetch_prune"))
    }

    fn add_remote(&self, _name: &str, _url: &str) -> Result<()> {
        Err(jj_todo("add_remote"))
    }

    fn set_remote_url(&self, _name: &str, _url: &str) -> Result<()> {
        Err(jj_todo("set_remote_url"))
    }

    fn get_remote_url(&self, _remote: &str) -> Result<String> {
        Err(jj_todo("get_remote_url"))
    }

    fn ensure_fork_remote(&self, _owner: &str) -> Result<String> {
        Err(jj_todo("ensure_fork_remote"))
    }

    fn get_repo_owner(&self) -> Result<String> {
        Err(jj_todo("get_repo_owner"))
    }

    // ── Deferred cleanup ─────────────────────────────────────────────

    fn build_cleanup_commands(
        &self,
        shared_dir: &Path,
        branch: &str,
        handle: &str,
        keep_branch: bool,
        _force: bool,
    ) -> Vec<String> {
        let repo_dir = shell_quote(&shared_dir.to_string_lossy());
        let mut cmds = Vec::new();

        // Forget the workspace
        let handle_q = shell_quote(handle);
        cmds.push(format!(
            "jj --quiet -R {} workspace forget {} >/dev/null 2>&1",
            repo_dir, handle_q
        ));

        // Delete bookmark (if not keeping)
        if !keep_branch {
            let branch_q = shell_quote(branch);
            cmds.push(format!(
                "jj --quiet -R {} bookmark delete {} >/dev/null 2>&1",
                repo_dir, branch_q
            ));
        }

        // Remove workmux metadata from config
        // We can't easily remove a TOML section from a shell script,
        // so use jj config unset for individual known keys
        for key in &["mode", "path"] {
            let config_key = format!("workmux.worktree.{}.{}", handle, key);
            cmds.push(format!(
                "jj --quiet -R {} config unset --repo {} >/dev/null 2>&1",
                repo_dir,
                shell_quote(&config_key)
            ));
        }

        cmds
    }

    // ── Status cache ─────────────────────────────────────────────────

    fn load_status_cache(&self) -> HashMap<PathBuf, VcsStatus> {
        crate::git::load_status_cache()
    }

    fn save_status_cache(&self, statuses: &HashMap<PathBuf, VcsStatus>) {
        crate::git::save_status_cache(statuses)
    }
}

/// Parse the totals line from `jj diff --stat` output.
/// The last line looks like: ` 3 files changed, 10 insertions(+), 5 deletions(-)`
/// Returns (insertions, deletions).
fn parse_jj_diff_stat_totals(output: &str) -> (usize, usize) {
    let last_line = output.lines().last().unwrap_or("");

    let mut insertions = 0;
    let mut deletions = 0;

    // Parse "N insertions(+)" and "N deletions(-)"
    for part in last_line.split(',') {
        let part = part.trim();
        if part.contains("insertion") {
            if let Some(n) = part.split_whitespace().next() {
                insertions = n.parse().unwrap_or(0);
            }
        } else if part.contains("deletion") {
            if let Some(n) = part.split_whitespace().next() {
                deletions = n.parse().unwrap_or(0);
            }
        }
    }

    (insertions, deletions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_workspace_list_single() {
        let output = "default: sqpusytp 28c83b43 (empty) (no description set)\n";
        let names = parse_workspace_list(output);
        assert_eq!(names, vec!["default"]);
    }

    #[test]
    fn test_parse_workspace_list_multiple() {
        let output = "default: sqpusytp 28c83b43 (empty) (no description set)\n\
                       feature: rlvkpnrz 3d0dead0 implement feature\n";
        let names = parse_workspace_list(output);
        assert_eq!(names, vec!["default", "feature"]);
    }

    #[test]
    fn test_parse_workspace_list_empty() {
        let names = parse_workspace_list("");
        assert!(names.is_empty());
    }

    #[test]
    fn test_parse_jj_diff_stat_totals_full() {
        let output = " src/main.rs | 10 +++++-----\n src/lib.rs  |  5 +++--\n 2 files changed, 8 insertions(+), 7 deletions(-)\n";
        assert_eq!(parse_jj_diff_stat_totals(output), (8, 7));
    }

    #[test]
    fn test_parse_jj_diff_stat_totals_insertions_only() {
        let output = " src/new.rs | 20 ++++++++++++++++++++\n 1 file changed, 20 insertions(+)\n";
        assert_eq!(parse_jj_diff_stat_totals(output), (20, 0));
    }

    #[test]
    fn test_parse_jj_diff_stat_totals_empty() {
        assert_eq!(parse_jj_diff_stat_totals(""), (0, 0));
    }

    #[test]
    fn test_parse_jj_diff_stat_totals_no_changes() {
        let output = "0 files changed\n";
        assert_eq!(parse_jj_diff_stat_totals(output), (0, 0));
    }
}
