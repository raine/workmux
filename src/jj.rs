//! Jujutsu support for colocated repositories.
//!
//! jj 0.46+ backs every workspace of a colocated repository with a Git
//! worktree, so worktree discovery, status and metadata stay on the Git code
//! paths. This module covers what Git cannot do on its own: creating and
//! forgetting workspaces, mapping a workspace to its bookmark, and integrating
//! a bookmark into its target. Git worktrees of jj workspaces are always on a
//! detached HEAD, so the bookmark of each workmux workspace is recorded in the
//! `bookmark` worktree metadata key.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::cmd::Cmd;
use crate::git;

const MIN_VERSION: (u32, u32) = (0, 46);

/// Worktree metadata key holding the bookmark of a jj workspace.
pub const BOOKMARK_META_KEY: &str = "bookmark";

/// Return whether the repository whose main worktree is `main_root` is a
/// supported colocated jj repository.
///
/// Errors when the jj repository is not colocated with this Git repository or
/// when the installed jj is too old to colocate workspaces.
pub fn detect(main_root: &Path, git_common_dir: &Path) -> Result<bool> {
    let store = main_root.join(".jj").join("repo").join("store");
    if !store.is_dir() {
        return Ok(false);
    }
    let target = std::fs::read_to_string(store.join("git_target")).unwrap_or_default();
    let colocated = store
        .join(target.trim())
        .canonicalize()
        .ok()
        .zip(git_common_dir.canonicalize().ok())
        .is_some_and(|(target, common)| target == common);
    if !colocated {
        bail!(
            "The jj repository at '{}' is not colocated with Git. workmux supports \
             colocated jj repositories only; run 'jj git colocation enable' to convert it.",
            main_root.display()
        );
    }
    ensure_supported_version()?;
    Ok(true)
}

/// Main workspace root of the jj repository sharing `git_common_dir`, if any.
pub fn repo_root_for_common_dir(git_common_dir: &Path) -> Option<PathBuf> {
    let root = git_common_dir.parent()?;
    root.join(".jj").is_dir().then(|| root.to_path_buf())
}

/// Error for a directory outside any Git repository, naming non-colocated jj
/// repositories explicitly.
pub fn not_a_git_repository_error(dir: &Path) -> anyhow::Error {
    if dir.ancestors().any(|path| path.join(".jj").is_dir()) {
        anyhow!(
            "This jj repository is not colocated with Git. workmux supports colocated jj \
             repositories only; run 'jj git colocation enable' to convert it."
        )
    } else {
        anyhow!("Not in a git repository")
    }
}

fn ensure_supported_version() -> Result<()> {
    static VERSION: OnceLock<std::result::Result<(u32, u32), String>> = OnceLock::new();
    let version = VERSION.get_or_init(|| {
        let output = Command::new("jj")
            .arg("--version")
            .output()
            .map_err(|_| "jj is not installed or not on PATH".to_string())?;
        let text = String::from_utf8_lossy(&output.stdout);
        parse_version(&text)
            .ok_or_else(|| format!("Could not parse jj version from '{}'", text.trim()))
    });
    match version {
        Ok(version) if *version >= MIN_VERSION => Ok(()),
        Ok((major, minor)) => bail!(
            "workmux requires jj {}.{} or newer for jj repositories (found {}.{})",
            MIN_VERSION.0,
            MIN_VERSION.1,
            major,
            minor
        ),
        Err(message) => bail!("{message}"),
    }
}

fn parse_version(text: &str) -> Option<(u32, u32)> {
    let version = text.split_whitespace().nth(1)?;
    let mut parts = version.split(['.', '-', '+']);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Quote a name as a revset string literal.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Revset for the working-copy commit of a workspace.
fn working_copy(workspace: &str) -> String {
    format!("{}@", quoted(workspace))
}

fn jj<'a>(dir: &'a Path) -> Cmd<'a> {
    Cmd::new("jj")
        .workdir(dir)
        .args(&["--no-pager", "--color=never"])
}

/// Evaluate a template for each commit in a revset, one output line per line
/// the template emits.
fn log(dir: &Path, revset: &str, template: &str) -> Result<Vec<String>> {
    let output = jj(dir)
        .args(&["log", "--no-graph", "-r", revset, "-T", template])
        .run_and_capture_stdout()?;
    Ok(output
        .lines()
        .map(str::to_string)
        .filter(|line| !line.is_empty())
        .collect())
}

fn non_empty(dir: &Path, revset: &str) -> Result<bool> {
    Ok(!log(dir, revset, r#"commit_id ++ "\n""#)?.is_empty())
}

fn commit_id(dir: &Path, revset: &str) -> Result<String> {
    let ids = log(dir, revset, r#"commit_id ++ "\n""#)?;
    match ids.as_slice() {
        [id] => Ok(id.clone()),
        [] => bail!("Revision '{revset}' does not exist"),
        _ => bail!("Revision '{revset}' resolves to more than one commit"),
    }
}

fn bookmark_names(dir: &Path, revset: &str) -> Result<Vec<String>> {
    let mut names = log(
        dir,
        revset,
        r#"local_bookmarks.map(|b| b.name() ++ "\n").join("")"#,
    )?;
    names.sort();
    names.dedup();
    Ok(names)
}

/// Create a colocated workspace at `path` named `workspace` whose working-copy
/// commit sits on `revision`.
pub fn add_workspace(main_root: &Path, path: &Path, workspace: &str, revision: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create '{}'", parent.display()))?;
    }
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("Invalid workspace path"))?;
    jj(main_root)
        .args(&[
            "workspace",
            "add",
            "--colocate",
            "--name",
            workspace,
            "-r",
            revision,
            path,
        ])
        .run()
        .context("Failed to create jj workspace")?;
    Ok(())
}

pub fn create_bookmark(dir: &Path, name: &str, revision: &str) -> Result<()> {
    jj(dir)
        .args(&["bookmark", "create", name, "-r", revision])
        .run()
        .with_context(|| format!("Failed to create bookmark '{name}'"))?;
    Ok(())
}

/// Forget a workspace without touching the current working copy.
pub fn forget_workspace(main_root: &Path, workspace: &str) -> Result<()> {
    jj(main_root)
        .args(&["--ignore-working-copy", "workspace", "forget", workspace])
        .run()
        .with_context(|| format!("Failed to forget jj workspace '{workspace}'"))?;
    Ok(())
}

/// Bookmarks of all workmux workspaces in the repository.
fn workspace_bookmarks(dir: &Path) -> HashSet<String> {
    git::get_all_worktree_meta_key_in(Some(dir), BOOKMARK_META_KEY)
        .into_values()
        .collect()
}

/// Bookmark to use as the default base for a new workspace created from `dir`.
///
/// Inside a workmux workspace this is the workspace's own bookmark. Elsewhere
/// it is the single bookmark on `@`, or on `@-` when `@` is an empty,
/// undescribed working-copy commit. Bookmarks of workmux workspaces are not
/// candidates there, since they start out on their base commit.
pub fn default_base(dir: &Path) -> Result<String> {
    if let Some(bookmark) = git::get_workspace_bookmark_in(dir) {
        return Ok(bookmark);
    }

    let owned = workspace_bookmarks(dir);
    let candidates = |revset: &str| -> Result<Vec<String>> {
        Ok(bookmark_names(dir, revset)?
            .into_iter()
            .filter(|name| !owned.contains(name))
            .collect())
    };

    let mut checked = "@";
    let mut names = candidates("@")?;
    if names.is_empty() && non_empty(dir, "@ & empty() & description(exact:\"\")")? {
        checked = "@ or @-";
        names = candidates("@-")?;
    }
    match names.as_slice() {
        [name] => Ok(name.clone()),
        [] => {
            let nearby = candidates("heads(::@ & bookmarks())")?;
            let hint = if nearby.is_empty() {
                String::new()
            } else {
                format!(" Nearest bookmarks: {}.", nearby.join(", "))
            };
            bail!("No bookmark on {checked} to use as the base. Use --base to choose one.{hint}")
        }
        _ => bail!(
            "Multiple bookmarks on {checked} ({}). Use --base to choose one.",
            names.join(", ")
        ),
    }
}

/// When `base` is the bookmark of a workmux workspace, move it to that
/// workspace's tip so a workspace created from it includes the latest work.
pub fn advance_base(dir: &Path, base: &str) -> Result<()> {
    let bookmarks = git::get_all_worktree_meta_key_in(Some(dir), BOOKMARK_META_KEY);
    let Some(handle) = bookmarks
        .iter()
        .find_map(|(handle, bookmark)| (bookmark == base).then_some(handle))
    else {
        return Ok(());
    };
    match git::find_worktree_in(handle, Some(dir)) {
        Ok((path, _)) if path.exists() => advance_workspace_bookmark(&path, base),
        _ => Ok(()),
    }
}

/// The latest commit of a workspace: its working-copy commit when it has
/// changes and a description, otherwise the parent of the working-copy commit.
/// An undescribed working-copy commit with changes is the jj counterpart of
/// uncommitted changes.
fn workspace_tip(workspace_root: &Path) -> Result<WorkspaceTip> {
    let state = log(
        workspace_root,
        "@",
        r#"if(empty, "empty", "changed") ++ " " ++ if(description, "described", "undescribed") ++ "\n""#,
    )?;
    let state = state.first().map(String::as_str).unwrap_or_default();
    let uncommitted = state == "changed undescribed";
    let revision = if state == "changed described" {
        "@"
    } else {
        "@-"
    };
    Ok(WorkspaceTip {
        commit: commit_id(workspace_root, revision)?,
        uncommitted,
    })
}

struct WorkspaceTip {
    commit: String,
    uncommitted: bool,
}

/// Move a workspace bookmark forward to the workspace tip. Leaves the bookmark
/// alone when it is not an ancestor of the tip.
pub fn advance_workspace_bookmark(workspace_root: &Path, bookmark: &str) -> Result<()> {
    let tip = workspace_tip(workspace_root)?;
    let bookmark_rev = quoted(bookmark);
    if non_empty(
        workspace_root,
        &format!(
            "present({bookmark_rev}) & ::{} ~ {}",
            tip.commit, tip.commit
        ),
    )? {
        set_bookmark(workspace_root, bookmark, &tip.commit, false)?;
    }
    Ok(())
}

fn set_bookmark(dir: &Path, name: &str, revision: &str, allow_backwards: bool) -> Result<()> {
    let mut cmd = jj(dir).args(&["bookmark", "set", name, "-r", revision]);
    if allow_backwards {
        cmd = cmd.arg("--allow-backwards");
    }
    cmd.run()
        .with_context(|| format!("Failed to move bookmark '{name}'"))?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStrategy {
    /// Fast-forward the target, or create a merge commit when it has diverged.
    Merge,
    /// Rebase the workspace onto the target, then fast-forward the target.
    Rebase,
    /// Create a single commit on the target with all changes of the bookmark.
    Squash,
}

pub struct MergeRequest<'a> {
    pub main_root: &'a Path,
    pub workspace_root: &'a Path,
    pub workspace: &'a str,
    pub bookmark: &'a str,
    pub target: &'a str,
    pub strategy: MergeStrategy,
    pub ignore_uncommitted: bool,
    pub keep: bool,
}

/// Point the workspace bookmark at the workspace tip and validate that the
/// merge can proceed. Runs before pre-merge hooks.
pub fn prepare_merge(request: &MergeRequest<'_>) -> Result<()> {
    let MergeRequest {
        workspace_root,
        workspace,
        bookmark,
        target,
        ignore_uncommitted,
        keep,
        ..
    } = *request;

    let tip = workspace_tip(workspace_root)?;
    if tip.uncommitted && !ignore_uncommitted && !keep {
        bail!(
            "Workspace for '{bookmark}' has uncommitted changes: its working-copy commit has \
             changes but no description. Describe it with 'jj describe' or 'jj commit', \
             or use --ignore-uncommitted."
        );
    }
    if !non_empty(workspace_root, &format!("present({})", quoted(target)))? {
        bail!("Target bookmark '{target}' does not exist");
    }

    let bookmark_rev = quoted(bookmark);
    if !non_empty(workspace_root, &format!("present({bookmark_rev})"))? {
        create_bookmark(workspace_root, bookmark, &tip.commit)?;
    } else if non_empty(
        workspace_root,
        &format!("{bookmark_rev} & ::{}", working_copy(workspace)),
    )? {
        // The bookmark is somewhere on the workspace's own history, so
        // pointing it at the tip only moves it within that history.
        set_bookmark(workspace_root, bookmark, &tip.commit, true)?;
    } else if !non_empty(
        workspace_root,
        &format!("{} & ::{bookmark_rev}", working_copy(workspace)),
    )? {
        bail!(
            "Bookmark '{bookmark}' is not on the history of workspace '{workspace}'. \
             Move it with 'jj bookmark set {bookmark} -r <revision>' and retry."
        );
    }
    Ok(())
}

/// Integrate the workspace bookmark into the target bookmark.
pub fn merge(request: &MergeRequest<'_>) -> Result<()> {
    let MergeRequest {
        main_root,
        workspace_root,
        bookmark,
        target,
        strategy,
        keep,
        ..
    } = *request;
    let source = quoted(bookmark);
    let target_rev = quoted(target);
    let old_target = commit_id(main_root, &target_rev)?;

    if non_empty(main_root, &format!("{source} & ::{target_rev}"))? {
        println!("Bookmark '{bookmark}' is already merged into '{target}'.");
        return Ok(());
    }

    let new_target = match strategy {
        MergeStrategy::Merge => {
            if non_empty(main_root, &format!("{target_rev} & ::{source}"))? {
                commit_id(main_root, &source)?
            } else {
                let message = format!("Merge bookmark '{bookmark}' into '{target}'");
                let merge = new_commit(main_root, &[&target_rev, &source], &message)?;
                abandon_if_conflicted(main_root, &merge, || {
                    conflict_error(request, &format!("jj rebase -b @ -d {target}"))
                })?;
                merge
            }
        }
        MergeStrategy::Rebase => {
            println!("Rebasing '{bookmark}' onto '{target}'...");
            jj(main_root)
                .args(&["rebase", "-b", &source, "-d", &target_rev])
                .run()
                .with_context(|| format!("Failed to rebase '{bookmark}' onto '{target}'"))?;
            update_stale(workspace_root)?;
            if non_empty(
                main_root,
                &format!("({target_rev}..{source}) & conflicts()"),
            )? {
                bail!(
                    "Rebase produced conflicts in the workspace at '{}'.\n\n\
                     Resolve them there (see 'jj status'), then retry the merge.",
                    workspace_root.display()
                );
            }
            commit_id(main_root, &source)?
        }
        MergeStrategy::Squash => {
            let merge = new_commit(main_root, &[&target_rev, &source], "workmux squash")?;
            abandon_if_conflicted(main_root, &merge, || {
                conflict_error(request, &format!("jj rebase -b @ -d {target}"))
            })?;
            let squashed = new_commit(main_root, &[&target_rev], "")?;
            jj(main_root)
                .args(&["restore", "--from", &merge, "--into", &squashed])
                .run()
                .context("Failed to squash changes")?;
            abandon(main_root, &merge)?;

            let descriptions = jj(main_root)
                .args(&[
                    "log",
                    "--no-graph",
                    "-r",
                    &format!("{target_rev}..{source}"),
                    "--reversed",
                    "-T",
                    r#"if(description, description ++ "\n")"#,
                ])
                .run_and_capture_stdout()?;
            println!("Please provide a commit message for the squashed changes in your editor.");
            let status = Command::new("jj")
                .current_dir(main_root)
                .args(["describe", "-r", &squashed, "--editor", "-m", &descriptions])
                .status()
                .context("Failed to run jj describe")?;
            if !status.success() {
                abandon(main_root, &squashed)?;
                bail!("Describing the squashed commit was aborted or failed");
            }
            squashed
        }
    };

    set_bookmark(main_root, target, &new_target, false)?;
    follow_target_in_main_workspace(main_root, &old_target, target)?;
    if keep && strategy == MergeStrategy::Squash {
        // The squashed changes are not on the workspace's history, so later
        // work in the kept workspace is rebased onto the target explicitly.
        println!(
            "Note: '{bookmark}' was squashed; rebase the workspace with 'jj rebase -b @ -d {target}' before continuing."
        );
    }
    Ok(())
}

fn conflict_error(request: &MergeRequest<'_>, resolve: &str) -> anyhow::Error {
    anyhow!(
        "Merge failed due to conflicts. Target bookmark '{}' kept unchanged.\n\n\
         To resolve, rebase the workspace at {}:\n  {}\n\
         After resolving conflicts, retry the merge.",
        request.target,
        request.workspace_root.display(),
        resolve
    )
}

fn abandon_if_conflicted(
    dir: &Path,
    commit: &str,
    error: impl FnOnce() -> anyhow::Error,
) -> Result<()> {
    if non_empty(dir, &format!("{commit} & conflicts()"))? {
        abandon(dir, commit)?;
        return Err(error());
    }
    Ok(())
}

fn abandon(dir: &Path, commit: &str) -> Result<()> {
    jj(dir)
        .args(&["abandon", commit])
        .run()
        .with_context(|| format!("Failed to abandon commit {commit}"))?;
    Ok(())
}

/// Create a commit with the given parents without editing it, returning its
/// change ID, which remains stable when its contents or description change.
fn new_commit(dir: &Path, parents: &[&str], message: &str) -> Result<String> {
    let first = parents
        .first()
        .ok_or_else(|| anyhow!("A new commit needs a parent"))?;
    let children = format!("children({first})");
    let template = r#"change_id ++ " " ++ commit_id ++ "\n""#;
    let before: HashSet<String> = log(dir, &children, template)?
        .into_iter()
        .filter_map(|line| line.split_once(' ').map(|(change, _)| change.to_string()))
        .collect();

    let mut cmd = jj(dir).args(&["new", "--no-edit", "-m", message]);
    for parent in parents {
        cmd = cmd.args(&["-r", parent]);
    }
    cmd.run().context("Failed to create commit")?;

    let created: Vec<String> = log(dir, &children, template)?
        .into_iter()
        .filter_map(|line| {
            let (change, _) = line.split_once(' ')?;
            (!before.contains(change)).then(|| change.to_string())
        })
        .collect();
    match created.as_slice() {
        [commit] => Ok(commit.clone()),
        _ => bail!("Could not identify the commit created by 'jj new'"),
    }
}

/// When the main workspace sits on an empty, undescribed commit directly on the
/// old target, move it onto the new target so its files show the merge.
fn follow_target_in_main_workspace(main_root: &Path, old_target: &str, target: &str) -> Result<()> {
    if non_empty(
        main_root,
        &format!("@ & children({old_target}) & empty() & description(exact:\"\") ~ merges()"),
    )? {
        jj(main_root)
            .args(&["rebase", "-r", "@", "-d", &quoted(target)])
            .run()
            .context("Failed to move the main workspace onto the merged target")?;
    }
    Ok(())
}

fn update_stale(workspace_root: &Path) -> Result<()> {
    if workspace_root.exists() {
        jj(workspace_root)
            .args(&["workspace", "update-stale"])
            .run()
            .context("Failed to update the workspace working copy")?;
    }
    Ok(())
}

/// Rebase all work of a workspace onto `base`.
pub fn rebase_workspace(workspace_root: &Path, base: &str) -> Result<()> {
    jj(workspace_root)
        .args(&["rebase", "-b", "@", "-d", &quoted(base)])
        .run()
        .with_context(|| format!("Failed to rebase onto '{base}'"))?;
    if non_empty(
        workspace_root,
        &format!("({}..@) & conflicts()", quoted(base)),
    )? {
        bail!(
            "Rebase produced conflicts in the workspace at '{}'. Resolve them there (see 'jj status').",
            workspace_root.display()
        );
    }
    let bookmark = git::get_current_branch_in(workspace_root).unwrap_or_default();
    if !bookmark.is_empty() {
        advance_workspace_bookmark(workspace_root, &bookmark)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_and_development_versions() {
        assert_eq!(parse_version("jj 0.46.0\n"), Some((0, 46)));
        assert_eq!(parse_version("jj 0.47.0-abcdef"), Some((0, 47)));
        assert_eq!(parse_version("jj 1.2.3+dirty"), Some((1, 2)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn quotes_revset_string_literals() {
        assert_eq!(quoted("feature/x"), "\"feature/x\"");
        assert_eq!(quoted(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(working_copy("ws"), "\"ws\"@");
    }

    #[test]
    fn non_colocated_repository_is_named_in_error() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            not_a_git_repository_error(temp.path()).to_string(),
            "Not in a git repository"
        );
        std::fs::create_dir(temp.path().join(".jj")).unwrap();
        assert!(
            not_a_git_repository_error(temp.path())
                .to_string()
                .contains("not colocated")
        );
    }
}
