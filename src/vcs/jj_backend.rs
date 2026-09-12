//! `JjBackend`: a [`crate::vcs::VcsBackend`] implementation driving the
//! `jj` binary directly.
//!
//! Unlike [`crate::vcs::git_backend::GitBackend`], which delegates to the
//! pre-existing `crate::git::*` free functions, there is no delegation
//! target here — every method builds a hardened `jj` invocation via
//! [`protected_jj`] and parses machine-readable template output.
//!
//! ## jj → git vocabulary mapping
//!
//! | git concept | jj concept used here |
//! |---|---|
//! | worktree | workspace (`jj workspace add/list/forget`) |
//! | main worktree root | the *primary* workspace root (the directory containing `.jj/repo` as a real directory) |
//! | `.git` common dir | also the primary workspace root — see the note on [`JjBackend::get_common_dir_in`] |
//! | branch | bookmark (`jj bookmark create/list/delete`) |
//! | `HEAD` | `@`, the working-copy commit |
//! | uncommitted changes | the content of `@` relative to its parent(s) |
//! | `origin/<branch>` | `<bookmark>@<remote>` |
//!
//! ## Output parsing policy
//!
//! Every `jj` command that this module parses is invoked with an explicit
//! `-T`/`--template` whose fields are separated by ASCII control characters
//! (`US`/`0x1f` between fields, `RS`/`0x1e` between records, `GS`/`0x1d`
//! inside a joined list). Human-formatted output is never parsed, because
//! it is not a stable interface and because workspace root paths and
//! bookmark names can legally contain characters (including newlines) that
//! would break line-oriented parsing. The single exception is
//! `jj diff --stat`, which has no template form for line counts; only its
//! trailing `"N files changed, X insertions(+), Y deletions(-)"` summary
//! line is read, and any parse failure degrades to zero rather than
//! erroring.
//!
//! ## Working-copy snapshotting
//!
//! `jj` snapshots the working copy at the start of almost every command,
//! which takes the working-copy lock and can append to the operation log.
//! Metadata-only queries here therefore pass `--ignore-working-copy` so a
//! background `workmux` poll never contends with the user's own `jj`
//! commands (the jj analog of `git --no-optional-locks`).
//! [`JjBackend::get_status`] deliberately does *not*: `is_dirty` and the
//! uncommitted diff stats are only meaningful against a fresh snapshot,
//! since in jj "uncommitted changes" means "changes `jj` has snapshotted
//! into `@`".

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::jj_security::{JjRepositoryIdentity, protected_jj};
use super::{
    CreateWorkspaceOptions, JjMetaStore, VcsBackend, VcsStatus, WorkmuxMetaStore, WorkspaceEntry,
};

/// Field separator (ASCII US) used inside one template record.
const FIELD_SEP: char = '\u{1f}';
/// Record separator (ASCII RS) terminating each template record.
const RECORD_SEP: char = '\u{1e}';
/// List separator (ASCII GS) joining the elements of a list field.
const LIST_SEP: char = '\u{1d}';

/// `jj workspace list` template: name, recorded root path (empty when
/// unrecorded/stale), and the local bookmarks on that workspace's `@`.
///
/// Verified against jj 0.44.0: `WorkspaceRef` exposes `.name()`,
/// `.target() -> Commit` and `.root() -> Option<FsPath>`.
const WORKSPACE_LIST_TEMPLATE: &str = concat!(
    r#"name ++ "\x1f" ++ if(root, root.absolute(), "") ++ "\x1f""#,
    r#" ++ target.local_bookmarks().map(|b| b.name()).join("\x1d") ++ "\x1e""#
);

/// `jj log` template for one commit: emptiness, conflict state, and local
/// bookmark names.
const COMMIT_STATUS_TEMPLATE: &str = concat!(
    r#"if(empty, "1", "0") ++ "\x1f" ++ if(conflict, "1", "0") ++ "\x1f""#,
    r#" ++ local_bookmarks.map(|b| b.name()).join("\x1d") ++ "\x1e""#
);

/// `jj log` template for resolving `trunk()`: commit id plus local and
/// remote bookmark names at that revision.
const TRUNK_TEMPLATE: &str = concat!(
    r#"commit_id ++ "\x1f" ++ local_bookmarks.map(|b| b.name()).join("\x1d") ++ "\x1f""#,
    r#" ++ remote_bookmarks.map(|b| b.name()).join("\x1d") ++ "\x1e""#
);

/// `jj log` template that emits one line per revision, for counting.
const COUNT_TEMPLATE: &str = r#""x\n""#;

/// `jj bookmark list --tracked` template that emits `<name>@<remote>` for
/// each *remote* entry and nothing for the local entries `--tracked` also
/// prints. Verified against jj 0.44.0: `--tracked` already excludes the
/// colocated repo's internal `@git` tracking bookmarks.
const TRACKED_REMOTE_TEMPLATE: &str = r#"if(remote, name ++ "@" ++ remote ++ "\x1e", "")"#;

/// The all-zeros commit id of jj's virtual `root()` commit.
const ROOT_COMMIT_ID: &str = "0000000000000000000000000000000000000000";

/// The remote name `get_merge_base_in`/`get_status` treat as "the" upstream,
/// mirroring `crate::git`'s hardcoded use of `origin/<branch>`.
const DEFAULT_REMOTE: &str = "origin";

/// [`VcsBackend`] implementation backed by the system `jj` binary.
pub struct JjBackend;

fn resolve_workdir(workdir: Option<&Path>) -> Result<PathBuf> {
    match workdir {
        Some(path) => Ok(path.to_path_buf()),
        None => std::env::current_dir().context("Failed to determine current directory"),
    }
}

/// Build a hardened `jj` command pinned to the repository containing
/// `workdir` (or the process's current directory).
///
/// `protected_jj(None)` deliberately omits `-R`, leaving jj to its own
/// upward cwd discovery; this wrapper always resolves a concrete directory
/// first so every invocation in this module is pinned.
fn jj(workdir: Option<&Path>) -> Result<Command> {
    let dir = resolve_workdir(workdir)?;
    protected_jj(Some(&dir))
}

/// Run `command`, requiring success, and return its stdout as a `String`.
fn capture(mut command: Command, what: &str) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("Failed to execute {what}"))?;
    if !output.status.success() {
        bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a metadata-only `jj` query (no working-copy snapshot) and return stdout.
fn query(workdir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = jj(workdir)?;
    command.arg("--ignore-working-copy").args(args);
    capture(command, &format!("jj {}", args.join(" ")))
}

/// Quote `value` as a jj string-pattern/revset string literal.
///
/// jj's revset and string-pattern grammars accept double-quoted strings with
/// backslash escapes, so a bookmark or remote name is interpolated as
/// `exact:"<escaped>"` rather than bare — this keeps a name containing glob
/// metacharacters (or a `)`) from being reinterpreted as pattern syntax.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Revset selecting the local bookmark named `name`, exactly.
fn local_bookmark_revset(name: &str) -> String {
    format!("bookmarks(exact:{})", quote(name))
}

/// Revset selecting the remote bookmark `name@remote`, exactly. Passing a
/// remote of `*` would also match jj's internal `git` remote, so callers
/// always name a concrete remote.
fn remote_bookmark_revset(name: &str, remote: &str) -> String {
    format!(
        "remote_bookmarks(exact:{}, remote=exact:{})",
        quote(name),
        quote(remote)
    )
}

/// Split the `<name>@<remote>` form (jj's spelling of git's
/// `<remote>/<branch>`) into its parts, or `None` for a plain local name.
fn split_remote_ref(name: &str) -> Option<(&str, &str)> {
    let (bookmark, remote) = name.rsplit_once('@')?;
    if bookmark.is_empty() || remote.is_empty() {
        None
    } else {
        Some((bookmark, remote))
    }
}

/// Count the revisions matched by `revset`.
fn count_revs(workdir: Option<&Path>, revset: &str) -> Result<usize> {
    let output = query(
        workdir,
        &["log", "--no-graph", "-r", revset, "-T", COUNT_TEMPLATE],
    )?;
    Ok(output.lines().filter(|line| !line.is_empty()).count())
}

/// Split template output into records and then fields, dropping the trailing
/// empty record produced by the record terminator.
fn records(output: &str) -> Vec<Vec<String>> {
    output
        .split(RECORD_SEP)
        .filter(|record| !record.is_empty())
        .map(|record| {
            record
                .split(FIELD_SEP)
                .map(|field| field.to_string())
                .collect()
        })
        .collect()
}

fn list_field(field: &str) -> Vec<String> {
    field
        .split(LIST_SEP)
        .filter(|part| !part.is_empty())
        .map(|part| part.to_string())
        .collect()
}

/// Parse the trailing summary line of `jj diff --stat`, which jj 0.44.0
/// always emits (even for an empty diff, as
/// `0 files changed, 0 insertions(+), 0 deletions(-)`), into
/// `(insertions, deletions)`. Unparseable output degrades to `(0, 0)`.
fn parse_diff_stat(output: &str) -> (usize, usize) {
    let summary = output
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .unwrap_or_default();

    let mut added = 0;
    let mut removed = 0;
    for segment in summary.split(',') {
        let mut parts = segment.split_whitespace();
        let Some(Ok(count)) = parts.next().map(str::parse::<usize>) else {
            continue;
        };
        match parts.next() {
            // "insertion(+)" / "insertions(+)"
            Some(word) if word.starts_with("insertion") => added = count,
            // "deletion(-)" / "deletions(-)"
            Some(word) if word.starts_with("deletion") => removed = count,
            _ => {}
        }
    }
    (added, removed)
}

/// The primary workspace root: the directory whose `.jj/repo` is a real
/// directory rather than a pointer file. Derived from the validated
/// [`JjRepositoryIdentity`] rather than assumed from a relative path.
fn primary_workspace_root(workdir: Option<&Path>) -> Result<PathBuf> {
    let dir = resolve_workdir(workdir)?;
    let identity = JjRepositoryIdentity::discover(&dir)?;
    let dot_jj = identity.repo_dir.parent().ok_or_else(|| {
        anyhow!(
            "jj repo directory has no parent: {}",
            identity.repo_dir.display()
        )
    })?;
    let root = dot_jj
        .parent()
        .ok_or_else(|| anyhow!("jj .jj directory has no parent: {}", dot_jj.display()))?;
    Ok(root.to_path_buf())
}

impl JjBackend {
    /// Whether a bookmark named `name` exists, as a *local* bookmark.
    fn local_bookmark_exists(&self, name: &str, workdir: Option<&Path>) -> Result<bool> {
        Ok(count_revs(workdir, &local_bookmark_revset(name))? > 0)
    }

    /// The remotes that `bookmark`'s local counterpart is tracking.
    ///
    /// `jj bookmark list --tracked` prints both the local entry and each
    /// tracked remote entry; the template keeps only the remote ones, and
    /// `--tracked` already omits a colocated repository's internal `@git`
    /// tracking bookmarks.
    fn tracked_remotes(&self, bookmark: &str, workdir: Option<&Path>) -> Result<Vec<String>> {
        let output = query(
            workdir,
            &[
                "bookmark",
                "list",
                "--tracked",
                "-T",
                TRACKED_REMOTE_TEMPLATE,
                &format!("exact:{bookmark}"),
            ],
        )?;
        Ok(output
            .split(RECORD_SEP)
            .filter(|record| !record.is_empty())
            .filter_map(|record| split_remote_ref(record).map(|(_, remote)| remote.to_string()))
            .collect())
    }

    /// A revset expression naming the comparison base for diffs: the local
    /// bookmark if it exists, else the `origin` remote bookmark, else the
    /// name as a literal revset symbol (which may fail to resolve — callers
    /// tolerate that).
    fn base_revset(&self, base: &str, workdir: Option<&Path>) -> String {
        if let Some((bookmark, remote)) = split_remote_ref(base) {
            return remote_bookmark_revset(bookmark, remote);
        }
        if self.local_bookmark_exists(base, workdir).unwrap_or(false) {
            return local_bookmark_revset(base);
        }
        let remote = remote_bookmark_revset(base, DEFAULT_REMOTE);
        if count_revs(workdir, &remote).unwrap_or(0) > 0 {
            return remote;
        }
        quote(base)
    }

    /// Insertions/deletions for `revset`, or `(0, 0)` if the diff can't be
    /// computed (mirroring `git::status::get_diff_stats`, which also
    /// swallows diff failures rather than failing the whole status).
    fn diff_stat(&self, workspace_path: &Path, revset: &str) -> (usize, usize) {
        let Ok(mut command) = jj(Some(workspace_path)) else {
            return (0, 0);
        };
        command.args(["diff", "--ignore-working-copy", "-r", revset, "--stat"]);
        match command.output() {
            Ok(output) if output.status.success() => {
                parse_diff_stat(&String::from_utf8_lossy(&output.stdout))
            }
            _ => (0, 0),
        }
    }
}

impl VcsBackend for JjBackend {
    fn name(&self) -> &'static str {
        "jj"
    }

    fn is_repo_in(&self, workdir: Option<&Path>) -> Result<bool> {
        let dir = resolve_workdir(workdir)?;
        Ok(JjRepositoryIdentity::discover(&dir).is_ok())
    }

    fn get_main_worktree_root_in(&self, workdir: Option<&Path>) -> Result<PathBuf> {
        primary_workspace_root(workdir)
    }

    /// The jj analog of git's "common dir".
    ///
    /// git has a genuinely separate control directory (`.git`) that every
    /// linked worktree shares; jj's equivalent shared location is the
    /// *primary workspace root* — the directory containing the real
    /// `.jj/repo`. This MUST agree with
    /// [`crate::vcs::jj_meta::JjMetaStore`], whose `remove_all_at` treats
    /// the `common_dir` it is handed as the directory under which
    /// `workmux/metadata.toml` lives. Returning some jj-internal path (e.g.
    /// `.jj/repo` itself) here would make `meta()`'s workdir-based
    /// resolution and `remove_all_at`'s explicit-path resolution disagree
    /// about where the metadata file is.
    fn get_common_dir_in(&self, workdir: Option<&Path>) -> Result<PathBuf> {
        primary_workspace_root(workdir)
    }

    /// Whether the repository has any real commit.
    ///
    /// `git rev-parse --verify HEAD` (the git implementation) fails only in
    /// a repository with no commits at all. jj has no such state: a freshly
    /// initialized repo already has the virtual `root()` commit *and* an
    /// empty, undescribed working-copy commit on top of it. The analog is
    /// therefore "some revision exists that is neither `root()` nor an
    /// empty, undescribed commit".
    fn has_commits_in(&self, workdir: Option<&Path>) -> Result<bool> {
        let revset = r#"(all() ~ root()) ~ (empty() & description(exact:""))"#;
        Ok(count_revs(workdir, revset)? > 0)
    }

    /// Create a jj workspace at `opts.path`, named `opts.name_or_branch`.
    ///
    /// Per the plan's bookmark-per-workspace ruling, a bookmark named
    /// `opts.name_or_branch` is created on the new workspace's `@` when
    /// `opts.create_branch` is set.
    ///
    /// `opts.base` maps to `jj workspace add -r <base>`, which makes the new
    /// workspace's working-copy commit a child of `<base>`. With no base, the
    /// flag is omitted so jj's default applies: the new working-copy commit
    /// gets the *same parents* as the current workspace's `@`. That is the
    /// closest analog of git's `git worktree add` defaulting to `HEAD` —
    /// notably it does not inherit the source workspace's uncommitted
    /// changes, which live in `@` itself.
    ///
    /// `opts.track_upstream` has no effect: in jj, tracking is a property of
    /// a *remote* bookmark (`jj bookmark track <name>@<remote>`), and a
    /// freshly created local bookmark has no remote counterpart to track
    /// yet. `jj git push` tracks automatically on first push.
    fn create_workspace_in(
        &self,
        opts: &CreateWorkspaceOptions,
        workdir: Option<&Path>,
    ) -> Result<()> {
        let base_dir = resolve_workdir(workdir)?;
        let path = if opts.path.is_absolute() {
            opts.path.clone()
        } else {
            base_dir.join(&opts.path)
        };

        let mut command = jj(Some(&base_dir))?;
        command
            .args(["workspace", "add", "--name", &opts.name_or_branch])
            .arg(&path);
        if let Some(base) = opts.base.as_deref().filter(|base| !base.is_empty()) {
            command.args(["-r", base]);
        }
        capture(
            command,
            &format!("jj workspace add {}", opts.name_or_branch),
        )?;

        if opts.create_branch {
            let mut bookmark = jj(Some(&path))?;
            bookmark.args(["bookmark", "create", &opts.name_or_branch, "-r", "@"]);
            capture(
                bookmark,
                &format!("jj bookmark create {}", opts.name_or_branch),
            )?;
        }

        Ok(())
    }

    /// List the repository's workspaces.
    ///
    /// Workspaces whose recorded root path is absent are skipped. jj records
    /// a workspace's root relative to `.jj/repo`, and that record is empty
    /// for workspaces created before jj 0.38.0 and becomes unresolvable if
    /// the workspace directory is moved or deleted (verified against jj
    /// 0.44.0: after `mv`, `root` renders empty and `jj workspace root
    /// --name <ws>` errors). [`WorkspaceEntry::path`] is not optional, so
    /// such an entry cannot be represented; see [`JjBackend::move_workspace`]
    /// for why workmux refuses to create that state itself.
    ///
    /// `WorkspaceEntry::name` carries jj's *workspace name*, not the
    /// directory basename `GitBackend` uses: the workspace name is jj's own
    /// stable identifier (it is what `jj workspace forget` takes) and can
    /// legitimately differ from the basename, whereas the basename is always
    /// recoverable from `path`.
    fn list_workspaces_in(&self, workdir: Option<&Path>) -> Result<Vec<WorkspaceEntry>> {
        let output = query(
            workdir,
            &["workspace", "list", "-T", WORKSPACE_LIST_TEMPLATE],
        )?;
        Ok(records(&output)
            .into_iter()
            .filter_map(|fields| {
                let name = fields.first()?.clone();
                let root = fields.get(1)?;
                if root.is_empty() {
                    return None;
                }
                let bookmarks = fields.get(2).map(|f| list_field(f)).unwrap_or_default();
                Some(WorkspaceEntry {
                    path: PathBuf::from(root),
                    name: Some(name),
                    branch_or_bookmark: bookmarks.into_iter().next(),
                })
            })
            .collect())
    }

    /// Always an error: jj 0.44.0 has no "move workspace" primitive.
    ///
    /// `jj workspace` offers only `add`, `forget`, `list`, `rename` (which
    /// renames the workspace *identifier*, not its path), `root` and
    /// `update-stale`. The workspace root is recorded in the repo as a path
    /// relative to `.jj/repo`, and nothing re-records it: moving the
    /// directory on disk leaves the record dangling (verified against jj
    /// 0.44.0 — `jj workspace root --name <ws>` then fails with "Cannot
    /// resolve absolute workspace path", and running jj commands from inside
    /// the moved directory does not repair it).
    ///
    /// `forget` + re-`add` is not equivalent: `forget` discards the
    /// workspace's working-copy commit association, so the working-copy
    /// commit (and with it any uncommitted work) is abandoned. Rather than
    /// do that silently, this returns an explicit error.
    fn move_workspace(&self, old_path: &Path, new_path: &Path) -> Result<()> {
        bail!(
            "Moving a jj workspace is not supported (jj has no 'workspace move' primitive; \
             its recorded workspace root cannot be updated in place). Refusing to move {} to {}.",
            old_path.display(),
            new_path.display()
        )
    }

    /// Deregister a workspace via `jj workspace forget`.
    ///
    /// This matches git's forget-then-delete-directory two-step (see
    /// `src/command/remove.rs`): `jj workspace forget` explicitly "will not
    /// touch the workspace on disk", leaving directory removal to the
    /// caller.
    ///
    /// `handle_or_name` is matched — in order — against the jj workspace
    /// name, the workspace directory's basename, and the bookmark on the
    /// workspace's `@`, mirroring `git::find_worktree_in`'s
    /// directory-name-or-branch lookup. `jj workspace forget` on an unknown
    /// name merely warns and exits 0, so the lookup is done here in order to
    /// surface a real error.
    fn remove_workspace_at(&self, handle_or_name: &str, common_dir: &Path) -> Result<()> {
        let output = query(
            Some(common_dir),
            &["workspace", "list", "-T", WORKSPACE_LIST_TEMPLATE],
        )?;
        let entries = records(&output);

        let matched = entries
            .iter()
            .find(|fields| fields.first().is_some_and(|name| name == handle_or_name))
            .or_else(|| {
                entries.iter().find(|fields| {
                    fields.get(1).is_some_and(|root| {
                        !root.is_empty()
                            && Path::new(root).file_name().and_then(|n| n.to_str())
                                == Some(handle_or_name)
                    })
                })
            })
            .or_else(|| {
                entries.iter().find(|fields| {
                    fields
                        .get(2)
                        .is_some_and(|f| list_field(f).iter().any(|b| b == handle_or_name))
                })
            })
            .ok_or_else(|| anyhow!("Workspace not found: {handle_or_name}"))?;

        let workspace_name = matched
            .first()
            .ok_or_else(|| anyhow!("Workspace not found: {handle_or_name}"))?;

        let mut command = jj(Some(common_dir))?;
        command
            .arg("--ignore-working-copy")
            .args(["workspace", "forget", workspace_name]);
        capture(command, &format!("jj workspace forget {workspace_name}"))
            .with_context(|| format!("Failed to remove workspace '{handle_or_name}'"))?;
        Ok(())
    }

    /// No-op: jj has nothing to prune.
    ///
    /// `git worktree prune` exists because git keeps per-worktree admin
    /// directories under `$GIT_COMMON_DIR/worktrees/<name>` that outlive a
    /// deleted worktree directory. jj keeps no such out-of-tree admin
    /// directory — a workspace's entire `.jj` lives inside the workspace
    /// itself, so deleting the directory leaves nothing behind on disk — and
    /// jj 0.44.0 has no `jj workspace prune` subcommand (only `add`,
    /// `forget`, `list`, `rename`, `root`, `update-stale`). Deregistration is
    /// `jj workspace forget`, which `remove_workspace_at` already performs.
    fn prune_workspaces_in(&self, _common_dir: &Path) -> Result<()> {
        Ok(())
    }

    /// Resolve the default bookmark, mirroring
    /// `git::get_default_branch_in`'s fallback order:
    ///
    /// 1. jj's `trunk()` (the analog of git's `origin/HEAD` symbolic ref):
    ///    the bookmark at that revision, preferring a local bookmark over a
    ///    remote one. `trunk()` evaluating to `root()` means "no trunk", and
    ///    is skipped.
    /// 2. a local `main` bookmark, then a local `master` bookmark.
    /// 3. the same two error messages git's version produces, distinguishing
    ///    "no commits yet" from "could not determine".
    ///
    /// Note that `trunk()` is a *revset alias* resolved from jj's config
    /// layers, and `crate::vcs::jj_security` documents that a hostile config
    /// layer could redefine it. It is used here because it is the only
    /// mechanism jj exposes for "the repository's default bookmark", and its
    /// result is treated as a hint: steps 2 and 3 still apply if it yields
    /// nothing usable.
    fn get_default_branch_in(&self, workdir: Option<&Path>) -> Result<String> {
        if let Ok(output) = query(
            workdir,
            &["log", "--no-graph", "-r", "trunk()", "-T", TRUNK_TEMPLATE],
        ) {
            for fields in records(&output) {
                if fields.first().map(String::as_str) == Some(ROOT_COMMIT_ID) {
                    continue;
                }
                let local = fields.get(1).map(|f| list_field(f)).unwrap_or_default();
                if let Some(name) = local.into_iter().next() {
                    return Ok(name);
                }
                let remote = fields.get(2).map(|f| list_field(f)).unwrap_or_default();
                if let Some(name) = remote.into_iter().next() {
                    return Ok(name);
                }
            }
        }

        for candidate in ["main", "master"] {
            if self.local_bookmark_exists(candidate, workdir)? {
                return Ok(candidate.to_string());
            }
        }

        if !self.has_commits_in(workdir)? {
            return Err(anyhow!(
                "The repository has no commits yet. Please make an initial commit before using workmux, \
                or specify the main branch in .workmux.yaml using the 'main_branch' key."
            ));
        }

        Err(anyhow!(
            "Could not determine the default branch (e.g., 'main' or 'master'). \
            Please specify it in .workmux.yaml using the 'main_branch' key."
        ))
    }

    /// Whether a bookmark named `name` exists, locally or on a remote.
    ///
    /// Mirrors `git::branch_exists_in`, which accepts remote-tracking names
    /// too: a `<name>@<remote>` argument is checked against that remote
    /// specifically, and a plain name matches a local bookmark or a remote
    /// bookmark of the same name on any (non-`git`) remote.
    fn branch_exists_in(&self, name: &str, workdir: Option<&Path>) -> Result<bool> {
        let revset = match split_remote_ref(name) {
            Some((bookmark, remote)) => remote_bookmark_revset(bookmark, remote),
            None => format!(
                "{} | remote_bookmarks(exact:{})",
                local_bookmark_revset(name),
                quote(name)
            ),
        };
        Ok(count_revs(workdir, &revset)? > 0)
    }

    /// The bookmark on this workspace's `@`, or `None`.
    ///
    /// `None` is the *normal* case in jj: the working-copy commit usually
    /// carries no bookmark at all, which is why this reports absence rather
    /// than erroring.
    fn get_current_branch_in(&self, workdir: &Path) -> Result<Option<String>> {
        let output = query(
            Some(workdir),
            &["log", "--no-graph", "-r", "@", "-T", COMMIT_STATUS_TEMPLATE],
        )?;
        Ok(records(&output)
            .first()
            .and_then(|fields| fields.get(2))
            .map(|f| list_field(f))
            .unwrap_or_default()
            .into_iter()
            .next())
    }

    /// Delete a bookmark via `jj bookmark delete`.
    ///
    /// `force` is ignored: git's `-d` refuses to delete a branch that isn't
    /// merged, and `-D` overrides that. jj has no such safety check —
    /// `jj bookmark delete` never refuses, and deleting a bookmark does not
    /// abandon its commits — so there is nothing for `force` to override.
    ///
    /// The name is passed as an `exact:` string pattern (jj's default is a
    /// glob), and existence is checked first because `jj bookmark delete` on
    /// an unknown name only warns and exits 0, where git's `branch -d`
    /// fails.
    fn delete_branch_in(&self, name: &str, force: bool, common_dir: &Path) -> Result<()> {
        let _ = force;
        if !self.local_bookmark_exists(name, Some(common_dir))? {
            bail!("Failed to delete branch: bookmark '{name}' does not exist");
        }
        let mut command = jj(Some(common_dir))?;
        command
            .arg("--ignore-working-copy")
            .args(["bookmark", "delete", &format!("exact:{name}")]);
        capture(command, &format!("jj bookmark delete {name}"))
            .context("Failed to delete branch")?;
        Ok(())
    }

    /// Resolve which reference to compare against for `base`, mirroring
    /// `git::get_merge_base_in` exactly: prefer the local bookmark (which
    /// may be ahead of the remote), else the `origin` remote bookmark
    /// spelled jj-style as `<base>@origin`, else `base` unchanged.
    ///
    /// Despite the name this is a *reference name* resolver, not a
    /// merge-base commit resolver — that is what the git implementation it
    /// mirrors does, and what its callers expect. (jj does expose a real
    /// merge base as the `fork_point(a | b)` revset, verified against jj
    /// 0.44.0, should a later task need it.)
    fn get_merge_base_in(&self, workdir: Option<&Path>, base: &str) -> Result<String> {
        if self.local_bookmark_exists(base, workdir)? {
            return Ok(base.to_string());
        }
        let remote = format!("{base}@{DEFAULT_REMOTE}");
        if count_revs(workdir, &remote_bookmark_revset(base, DEFAULT_REMOTE))? > 0 {
            Ok(remote)
        } else {
            Ok(base.to_string())
        }
    }

    /// Build a [`VcsStatus`] for a jj workspace.
    ///
    /// Field-by-field mapping (see the module docs for the general
    /// vocabulary mapping):
    ///
    /// - `is_dirty`: `@` is non-empty. In jj there is no index and no
    ///   untracked-file category — `jj` snapshots the working copy into `@`,
    ///   so "`@` differs from its parent" is exactly git's
    ///   "staged-or-unstaged-or-untracked".
    /// - `has_conflict`: the `conflict` template keyword on `@`. This is a
    ///   deliberate semantic difference from `GitBackend`, which performs a
    ///   *speculative* `git merge-tree` against the base to predict a future
    ///   conflict. jj materializes conflicts into commits instead, so the
    ///   directly observable fact is whether `@` currently *has* conflicts.
    /// - `is_rebasing`: always `false`. git leaves `rebase-merge`/
    ///   `rebase-apply` state directories behind while a rebase is stopped
    ///   mid-way; `jj rebase` is atomic, rebases descendants automatically,
    ///   and records conflicts in the commits themselves, so there is no
    ///   persistent "rebase in progress" state to report.
    /// - `ahead`/`behind`/`has_upstream`: from the tracked remote bookmark of
    ///   the bookmark on `@`. Counts are revset cardinalities
    ///   (`<b>@<remote>..<b>` and `<b>..<b>@<remote>`) between the *bookmark*
    ///   and its remote counterpart, which is the analog of git's
    ///   `branch.ab` comparing the branch ref to its upstream. With no
    ///   bookmark, or a bookmark tracking no remote, `has_upstream` is false
    ///   and both counts are 0.
    /// - `lines_added`/`lines_removed`: the diff of the committed part of the
    ///   branch, `(<base>..@) ~ @` — everything from the base up to but
    ///   excluding the working-copy commit. `<base>..@` is jj's
    ///   symmetric-ancestry range, matching git's three-dot `base...HEAD`.
    /// - `uncommitted_added`/`uncommitted_removed`: the diff of `@` itself.
    /// - `branch`: the local bookmark on `@`, or `None`.
    /// - `base_branch`: `meta().get_branch_base()`, else `main_branch`, else
    ///   [`Self::get_default_branch_in`], else `"main"` — the same priority
    ///   order as `git::get_git_status`.
    fn get_status(&self, workspace_path: &Path, main_branch: Option<&str>) -> Result<VcsStatus> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();

        // Deliberately snapshots the working copy (no `--ignore-working-copy`):
        // `is_dirty` is only meaningful against a fresh snapshot.
        let mut command = jj(Some(workspace_path))?;
        command.args(["log", "--no-graph", "-r", "@", "-T", COMMIT_STATUS_TEMPLATE]);
        let output = capture(command, "jj log -r @")?;
        let fields = records(&output)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("jj log -r @ produced no output"))?;

        let is_dirty = fields.first().map(String::as_str) != Some("1");
        let has_conflict = fields.get(1).map(String::as_str) == Some("1");
        let branch = fields
            .get(2)
            .map(|f| list_field(f))
            .unwrap_or_default()
            .into_iter()
            .next();

        // jj has no persistent mid-rebase state; see the doc comment.
        let is_rebasing = false;

        // Ahead/behind are the only fields that need a bookmark. Unlike
        // `git::get_git_status`, which early-returns an otherwise-empty
        // status for a detached `HEAD`, everything else is still computed
        // when `@` carries no bookmark: in jj that is the *ordinary* state,
        // not an unusual one, so returning zeroed diff stats for it would
        // blank out the status of most workspaces.
        let (has_upstream, ahead, behind) = match branch.as_deref() {
            Some(bookmark) => match self
                .tracked_remotes(bookmark, Some(workspace_path))?
                .into_iter()
                .next()
            {
                Some(remote) => {
                    let local = local_bookmark_revset(bookmark);
                    let upstream = remote_bookmark_revset(bookmark, &remote);
                    let ahead = count_revs(Some(workspace_path), &format!("{upstream}..{local}"))
                        .unwrap_or(0);
                    let behind = count_revs(Some(workspace_path), &format!("{local}..{upstream}"))
                        .unwrap_or(0);
                    (true, ahead, behind)
                }
                None => (false, 0, 0),
            },
            None => (false, 0, 0),
        };

        let base_branch = branch
            .as_deref()
            .and_then(|bookmark| self.meta().get_branch_base(bookmark, Some(workspace_path)))
            .or_else(|| {
                main_branch
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            })
            .or_else(|| self.get_default_branch_in(Some(workspace_path)).ok())
            .unwrap_or_else(|| "main".to_string());

        let (uncommitted_added, uncommitted_removed) = self.diff_stat(workspace_path, "@");

        // On the base branch there is no branch-level diff, but uncommitted
        // changes are still reported — mirroring `git::get_git_status`.
        if branch.as_deref() == Some(base_branch.as_str()) {
            return Ok(VcsStatus {
                ahead,
                behind,
                has_conflict,
                is_dirty,
                uncommitted_added,
                uncommitted_removed,
                cached_at: now,
                base_branch,
                branch,
                has_upstream,
                is_rebasing,
                ..Default::default()
            });
        }

        let base_revset = self.base_revset(&base_branch, Some(workspace_path));
        let (lines_added, lines_removed) =
            self.diff_stat(workspace_path, &format!("({base_revset}..@) ~ @"));

        Ok(VcsStatus {
            ahead,
            behind,
            has_conflict,
            is_dirty,
            lines_added,
            lines_removed,
            uncommitted_added,
            uncommitted_removed,
            cached_at: now,
            base_branch,
            branch,
            has_upstream,
            is_rebasing,
        })
    }

    fn meta(&self) -> &dyn WorkmuxMetaStore {
        const INSTANCE: JjMetaStore = JjMetaStore;
        &INSTANCE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{clear_local_jj_env, init_colocated_repo, init_jj_repo};
    use std::process::Command as StdCommand;

    /// Run a raw `jj` command in `dir`, asserting success. Used only to
    /// build fixtures — production code paths always go through
    /// `protected_jj`.
    fn jj_in(dir: &Path, args: &[&str]) -> String {
        let mut command = StdCommand::new("jj");
        clear_local_jj_env(&mut command);
        let output = command
            .arg("--no-pager")
            .arg("--color=never")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("jj should run");
        assert!(
            output.status.success(),
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A repo with one described commit bookmarked `main`, and `@` a fresh
    /// empty child of it.
    fn seed(dir: &Path) {
        std::fs::write(dir.join("README.md"), "base\n").unwrap();
        jj_in(dir, &["describe", "-m", "initial"]);
        jj_in(dir, &["bookmark", "create", "main", "-r", "@"]);
        jj_in(dir, &["new"]);
    }

    fn jj_only_fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        seed(temp.path());
        temp
    }

    fn colocated_fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        init_colocated_repo(temp.path());
        seed(temp.path());
        temp
    }

    #[test]
    fn name_is_jj() {
        assert_eq!(JjBackend.name(), "jj");
    }

    #[test]
    fn is_repo_in_detects_jj_only_and_colocated_and_rejects_plain_dir() {
        let jj_only = jj_only_fixture();
        let colocated = colocated_fixture();
        let plain = tempfile::tempdir().unwrap();

        assert!(JjBackend.is_repo_in(Some(jj_only.path())).unwrap());
        assert!(JjBackend.is_repo_in(Some(colocated.path())).unwrap());
        assert!(!JjBackend.is_repo_in(Some(plain.path())).unwrap());
    }

    #[test]
    fn common_dir_and_main_root_are_the_primary_workspace_root() {
        let temp = jj_only_fixture();
        let expected = temp.path().canonicalize().unwrap();

        assert_eq!(
            JjBackend.get_common_dir_in(Some(temp.path())).unwrap(),
            expected
        );
        assert_eq!(
            JjBackend
                .get_main_worktree_root_in(Some(temp.path()))
                .unwrap(),
            expected
        );

        // And from inside a secondary workspace, both still point at the
        // primary root — this is the invariant `JjMetaStore` depends on.
        let secondary = temp.path().join("ws");
        JjBackend
            .create_workspace_in(
                &CreateWorkspaceOptions {
                    path: secondary.clone(),
                    name_or_branch: "ws".to_string(),
                    create_branch: false,
                    base: None,
                    track_upstream: false,
                },
                Some(temp.path()),
            )
            .unwrap();
        assert_eq!(
            JjBackend.get_common_dir_in(Some(&secondary)).unwrap(),
            expected
        );
    }

    #[test]
    fn meta_resolution_agrees_between_workdir_and_common_dir() {
        let temp = jj_only_fixture();
        let backend = JjBackend;
        backend
            .meta()
            .set("handle", "mode", "window", Some(temp.path()))
            .unwrap();
        assert_eq!(
            backend.meta().get("handle", "mode", Some(temp.path())),
            Some("window".to_string())
        );

        // `remove_all_at` uses the `common_dir` directly; it must find the
        // same file `meta().set` wrote via workdir discovery.
        let common_dir = backend.get_common_dir_in(Some(temp.path())).unwrap();
        backend.meta().remove_all_at("handle", &common_dir).unwrap();
        assert_eq!(
            backend.meta().get("handle", "mode", Some(temp.path())),
            None
        );
    }

    #[test]
    fn has_commits_in_distinguishes_fresh_from_seeded_repo() {
        let fresh = tempfile::tempdir().unwrap();
        init_jj_repo(fresh.path());
        assert!(!JjBackend.has_commits_in(Some(fresh.path())).unwrap());

        let seeded = jj_only_fixture();
        assert!(JjBackend.has_commits_in(Some(seeded.path())).unwrap());
    }

    fn create_list_remove_round_trip(temp: &Path) {
        let backend = JjBackend;
        let workspace_path = temp.join("feature-wt");
        let opts = CreateWorkspaceOptions {
            path: workspace_path.clone(),
            name_or_branch: "feature".to_string(),
            create_branch: true,
            base: Some("main".to_string()),
            track_upstream: false,
        };
        backend.create_workspace_in(&opts, Some(temp)).unwrap();

        // The bookmark was created as part of workspace creation.
        assert!(backend.branch_exists_in("feature", Some(temp)).unwrap());
        assert_eq!(
            backend.get_current_branch_in(&workspace_path).unwrap(),
            Some("feature".to_string())
        );

        let entries = backend.list_workspaces_in(Some(temp)).unwrap();
        let canonical = workspace_path.canonicalize().unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.path == canonical || entry.path == workspace_path)
            .expect("created workspace should be listed");
        assert_eq!(entry.name.as_deref(), Some("feature"));
        assert_eq!(entry.branch_or_bookmark.as_deref(), Some("feature"));
        // The primary workspace is listed too.
        assert!(
            entries
                .iter()
                .any(|entry| entry.name.as_deref() == Some("default"))
        );

        // The new workspace's `@` is a child of `main`.
        let parent = jj_in(
            &workspace_path,
            &[
                "log",
                "--no-graph",
                "-r",
                "@-",
                "-T",
                r#"local_bookmarks.map(|b| b.name()).join(",")"#,
            ],
        );
        assert_eq!(parent.trim(), "main");

        let common_dir = backend.get_common_dir_in(Some(temp)).unwrap();
        backend.remove_workspace_at("feature", &common_dir).unwrap();
        let after = backend.list_workspaces_in(Some(temp)).unwrap();
        assert!(
            !after
                .iter()
                .any(|entry| entry.name.as_deref() == Some("feature"))
        );

        // Prune is a no-op but must not error.
        backend.prune_workspaces_in(&common_dir).unwrap();
    }

    #[test]
    fn create_list_remove_round_trip_jj_only() {
        let temp = jj_only_fixture();
        create_list_remove_round_trip(temp.path());
    }

    #[test]
    fn create_list_remove_round_trip_colocated() {
        let temp = colocated_fixture();
        create_list_remove_round_trip(temp.path());
    }

    #[test]
    fn remove_workspace_at_matches_directory_basename_and_reports_unknown() {
        let temp = jj_only_fixture();
        let backend = JjBackend;
        let workspace_path = temp.path().join("dir-name");
        backend
            .create_workspace_in(
                &CreateWorkspaceOptions {
                    path: workspace_path.clone(),
                    name_or_branch: "ws-name".to_string(),
                    create_branch: false,
                    base: None,
                    track_upstream: false,
                },
                Some(temp.path()),
            )
            .unwrap();

        let common_dir = backend.get_common_dir_in(Some(temp.path())).unwrap();
        let error = backend
            .remove_workspace_at("no-such-thing", &common_dir)
            .unwrap_err();
        assert!(error.to_string().contains("Workspace not found"));

        backend
            .remove_workspace_at("dir-name", &common_dir)
            .unwrap();
        assert!(
            !backend
                .list_workspaces_in(Some(temp.path()))
                .unwrap()
                .iter()
                .any(|entry| entry.name.as_deref() == Some("ws-name"))
        );
    }

    #[test]
    fn create_workspace_without_create_branch_makes_no_bookmark() {
        let temp = jj_only_fixture();
        let backend = JjBackend;
        let workspace_path = temp.path().join("plain");
        backend
            .create_workspace_in(
                &CreateWorkspaceOptions {
                    path: workspace_path.clone(),
                    name_or_branch: "plain".to_string(),
                    create_branch: false,
                    base: None,
                    track_upstream: false,
                },
                Some(temp.path()),
            )
            .unwrap();
        assert!(
            !backend
                .branch_exists_in("plain", Some(temp.path()))
                .unwrap()
        );
        assert_eq!(
            backend.get_current_branch_in(&workspace_path).unwrap(),
            None
        );
    }

    #[test]
    fn move_workspace_is_an_explicit_error() {
        let error = JjBackend
            .move_workspace(Path::new("/tmp/old"), Path::new("/tmp/new"))
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("not supported"), "{message}");
        assert!(message.contains("/tmp/old"), "{message}");
    }

    #[test]
    fn get_default_branch_in_resolves_trunk_bookmark() {
        for temp in [jj_only_fixture(), colocated_fixture()] {
            assert_eq!(
                JjBackend.get_default_branch_in(Some(temp.path())).unwrap(),
                "main"
            );
        }
    }

    #[test]
    fn get_default_branch_in_falls_back_to_master() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        std::fs::write(temp.path().join("f.txt"), "x\n").unwrap();
        jj_in(temp.path(), &["describe", "-m", "initial"]);
        jj_in(temp.path(), &["bookmark", "create", "master", "-r", "@"]);
        assert_eq!(
            JjBackend.get_default_branch_in(Some(temp.path())).unwrap(),
            "master"
        );
    }

    #[test]
    fn get_default_branch_in_errors_on_repo_with_no_commits() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        let error = JjBackend
            .get_default_branch_in(Some(temp.path()))
            .unwrap_err();
        assert!(error.to_string().contains("no commits yet"), "{}", error);
    }

    #[test]
    fn get_default_branch_in_errors_when_undeterminable() {
        let temp = tempfile::tempdir().unwrap();
        init_jj_repo(temp.path());
        std::fs::write(temp.path().join("f.txt"), "x\n").unwrap();
        jj_in(temp.path(), &["describe", "-m", "initial"]);
        jj_in(temp.path(), &["bookmark", "create", "trunkish", "-r", "@"]);
        let error = JjBackend
            .get_default_branch_in(Some(temp.path()))
            .unwrap_err();
        assert!(
            error.to_string().contains("Could not determine"),
            "{}",
            error
        );
    }

    #[test]
    fn bookmark_creation_lookup_and_deletion() {
        let temp = jj_only_fixture();
        let backend = JjBackend;
        let common_dir = backend.get_common_dir_in(Some(temp.path())).unwrap();

        assert!(backend.branch_exists_in("main", Some(temp.path())).unwrap());
        assert!(!backend.branch_exists_in("nope", Some(temp.path())).unwrap());

        jj_in(
            temp.path(),
            &["bookmark", "create", "feature/x", "-r", "@-"],
        );
        assert!(
            backend
                .branch_exists_in("feature/x", Some(temp.path()))
                .unwrap()
        );

        backend
            .delete_branch_in("feature/x", false, &common_dir)
            .unwrap();
        assert!(
            !backend
                .branch_exists_in("feature/x", Some(temp.path()))
                .unwrap()
        );

        let error = backend
            .delete_branch_in("feature/x", true, &common_dir)
            .unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{}", error);
    }

    #[test]
    fn get_merge_base_in_prefers_local_bookmark_then_falls_through() {
        let temp = jj_only_fixture();
        let backend = JjBackend;
        assert_eq!(
            backend
                .get_merge_base_in(Some(temp.path()), "main")
                .unwrap(),
            "main"
        );
        // No local and no origin bookmark: returns the name unchanged.
        assert_eq!(
            backend
                .get_merge_base_in(Some(temp.path()), "nonexistent")
                .unwrap(),
            "nonexistent"
        );
    }

    fn status_clean_then_dirty(temp: &Path) {
        let backend = JjBackend;
        let status = backend.get_status(temp, Some("main")).unwrap();
        // `@` is a fresh empty child of `main`, so: clean, no bookmark.
        assert!(!status.is_dirty, "{status:?}");
        assert!(!status.has_conflict, "{status:?}");
        assert!(!status.is_rebasing, "{status:?}");
        assert_eq!(status.branch, None, "{status:?}");
        assert!(!status.has_upstream, "{status:?}");
        assert_eq!((status.ahead, status.behind), (0, 0), "{status:?}");

        std::fs::write(temp.join("new.txt"), "one\ntwo\nthree\n").unwrap();
        let status = backend.get_status(temp, Some("main")).unwrap();
        assert!(status.is_dirty, "{status:?}");
        assert_eq!(status.uncommitted_added, 3, "{status:?}");
        assert_eq!(status.uncommitted_removed, 0, "{status:?}");
    }

    #[test]
    fn get_status_clean_then_dirty_jj_only() {
        let temp = jj_only_fixture();
        status_clean_then_dirty(temp.path());
    }

    #[test]
    fn get_status_clean_then_dirty_colocated() {
        let temp = colocated_fixture();
        status_clean_then_dirty(temp.path());
    }

    #[test]
    fn get_status_reports_bookmark_base_branch_and_committed_lines() {
        let temp = jj_only_fixture();
        let backend = JjBackend;
        let workspace_path = temp.path().join("feature-wt");
        backend
            .create_workspace_in(
                &CreateWorkspaceOptions {
                    path: workspace_path.clone(),
                    name_or_branch: "feature".to_string(),
                    create_branch: true,
                    base: Some("main".to_string()),
                    track_upstream: false,
                },
                Some(temp.path()),
            )
            .unwrap();

        // One committed change on the branch, plus an uncommitted one.
        std::fs::write(workspace_path.join("a.txt"), "1\n2\n").unwrap();
        jj_in(&workspace_path, &["describe", "-m", "committed"]);
        jj_in(&workspace_path, &["new"]);
        std::fs::write(workspace_path.join("b.txt"), "3\n").unwrap();

        let status = backend.get_status(&workspace_path, Some("main")).unwrap();
        assert_eq!(status.base_branch, "main", "{status:?}");
        assert!(status.is_dirty, "{status:?}");
        assert_eq!(status.lines_added, 2, "{status:?}");
        assert_eq!(status.lines_removed, 0, "{status:?}");
        assert_eq!(status.uncommitted_added, 1, "{status:?}");
        assert!(!status.has_upstream, "{status:?}");
        assert_eq!((status.ahead, status.behind), (0, 0), "{status:?}");
    }

    #[test]
    fn get_status_reports_conflict() {
        let temp = jj_only_fixture();
        let root = temp.path();
        // Two siblings both editing README.md, then a merge of the two.
        let base = jj_in(root, &["log", "--no-graph", "-r", "@-", "-T", "commit_id"])
            .trim()
            .to_string();
        std::fs::write(root.join("README.md"), "left\n").unwrap();
        jj_in(root, &["describe", "-m", "left"]);
        let left = jj_in(root, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
            .trim()
            .to_string();
        jj_in(root, &["new", &base, "-m", "right"]);
        std::fs::write(root.join("README.md"), "right\n").unwrap();
        let right = jj_in(root, &["log", "--no-graph", "-r", "@", "-T", "commit_id"])
            .trim()
            .to_string();
        jj_in(root, &["new", &left, &right, "-m", "merge"]);

        let status = JjBackend.get_status(root, Some("main")).unwrap();
        assert!(status.has_conflict, "{status:?}");
    }

    #[test]
    fn get_status_reports_ahead_behind_against_tracked_remote() {
        let temp = tempfile::tempdir().unwrap();
        let origin = temp.path().join("origin.git");
        let mut git = StdCommand::new("git");
        crate::test_support::clear_local_git_env(&mut git);
        assert!(
            git.args(["init", "--bare", "-b", "main"])
                .arg(&origin)
                .output()
                .expect("git init --bare should run")
                .status
                .success()
        );

        let work = temp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        init_colocated_repo(&work);
        std::fs::write(work.join("README.md"), "base\n").unwrap();
        jj_in(&work, &["describe", "-m", "initial"]);
        jj_in(&work, &["bookmark", "create", "main", "-r", "@"]);
        jj_in(
            &work,
            &["git", "remote", "add", "origin", origin.to_str().unwrap()],
        );
        // A pushed commit normally becomes immutable, which makes jj move
        // `@` off it onto a fresh empty child — that would leave `@` without
        // a bookmark and is beside the point here, so immutability is
        // disabled for the push only.
        jj_in(
            &work,
            &[
                "--config",
                r#"revset-aliases."immutable_heads()"=none()"#,
                "git",
                "push",
                "-b",
                "main",
            ],
        );

        // Synchronized: `main` and `main@origin` are the same commit.
        let synced = JjBackend.get_status(&work, Some("main")).unwrap();
        assert!(synced.has_upstream, "{synced:?}");
        assert_eq!(synced.branch.as_deref(), Some("main"), "{synced:?}");
        assert_eq!((synced.ahead, synced.behind), (0, 0), "{synced:?}");

        // One local commit on top, with the bookmark moved to it.
        jj_in(&work, &["new", "-m", "ahead"]);
        std::fs::write(work.join("ahead.txt"), "x\n").unwrap();
        jj_in(&work, &["describe", "-m", "ahead"]);
        jj_in(&work, &["bookmark", "set", "main", "-r", "@"]);

        let after = JjBackend.get_status(&work, Some("main")).unwrap();
        assert!(after.has_upstream, "{after:?}");
        assert_eq!(after.branch.as_deref(), Some("main"), "{after:?}");
        assert_eq!(after.ahead, 1, "{after:?}");
        assert_eq!(after.behind, 0, "{after:?}");
        assert!(after.is_dirty, "{after:?}");
    }

    #[test]
    fn parse_diff_stat_handles_jj_summary_forms() {
        assert_eq!(
            parse_diff_stat("0 files changed, 0 insertions(+), 0 deletions(-)\n"),
            (0, 0)
        );
        assert_eq!(
            parse_diff_stat("one.txt | 1 +\n1 file changed, 1 insertion(+), 0 deletions(-)\n"),
            (1, 0)
        );
        assert_eq!(
            parse_diff_stat(
                "b.bin  | (binary) +9 bytes\nf1.txt | 3 ---\n2 files changed, 0 insertions(+), 3 deletions(-)\n"
            ),
            (0, 3)
        );
        assert_eq!(parse_diff_stat(""), (0, 0));
    }

    #[test]
    fn quote_escapes_revset_string_literals() {
        assert_eq!(quote("main"), "\"main\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn split_remote_ref_splits_only_well_formed_names() {
        assert_eq!(split_remote_ref("main@origin"), Some(("main", "origin")));
        assert_eq!(split_remote_ref("main"), None);
        assert_eq!(split_remote_ref("@origin"), None);
        assert_eq!(split_remote_ref("main@"), None);
        // Rightmost `@` wins, mirroring jj's `<name>@<remote>` spelling.
        assert_eq!(split_remote_ref("a@b@c"), Some(("a@b", "c")));
    }
}
