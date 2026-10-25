//! `node provision-workspace`.
//!
//! Without a payload the verb only claims the workspace and prepares the
//! project repository, so the laptop can push the base commit. With one it
//! creates the worktree and provisions it step by step; every step is
//! recorded as it completes, so a retry with the same workspace id resumes
//! where the previous call stopped.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use super::{NodeRoot, Workspace, base_ref, git, hook_env, refuse, run_hook, validate_identity};
use crate::cmd::Cmd;
use crate::config::{Config, FileConfig, RemoteIsolation};
use crate::remote::protocol::{
    FileEntry, FileEntryKind, ProvisionPayload, ProvisionRequest, ProvisionResponse,
    WorkspaceIdentity, code,
};
use crate::remote::transfer;

const DOCKER_TIMEOUT: Duration = Duration::from_secs(20);

/// Branch the node repository's own (never used) checkout is on.
const UNBORN_BRANCH: &str = "workmux-node-unborn";

pub fn provision(root: &NodeRoot, request: ProvisionRequest) -> Result<ProvisionResponse> {
    validate_identity(&request.identity)?;
    let ws_id = request.identity.ws_id.clone();
    let _lock = root.lock_workspace(&ws_id)?;

    let mut workspace = match root.load(&ws_id)? {
        Some(workspace) => {
            if workspace.identity != request.identity || workspace.isolation != request.isolation {
                return Err(refuse(
                    code::CONFLICT,
                    format!("workspace id {ws_id} is already used for a different workspace"),
                ));
            }
            workspace
        }
        None => claim(root, &request.identity, request.isolation)?,
    };

    let has_base = base_matches(&workspace)?;
    if let Some(payload) = request.payload {
        if !has_base {
            return Err(refuse(
                code::FAILED,
                format!(
                    "base commit {} was not pushed to the node",
                    workspace.identity.base_sha
                ),
            ));
        }
        if !workspace.progress.ready {
            provision_worktree(root, &mut workspace, payload)?;
        }
    }
    let mut response = response(&workspace, has_base);
    let _project = root.lock_project(&workspace.identity.project_id)?;
    for path in &request.seed_paths {
        if seed_exists(&workspace.repo_path, path)? {
            response.cached_seeds.push(path.clone());
        }
    }
    Ok(response)
}

fn response(workspace: &Workspace, has_base: bool) -> ProvisionResponse {
    ProvisionResponse {
        repo_path: workspace.repo_path.to_string_lossy().into_owned(),
        worktree_path: workspace.worktree_path.to_string_lossy().into_owned(),
        has_base,
        cached_seeds: Vec::new(),
    }
}

/// Record a new workspace after checking the runtime and that no other
/// workspace of the project uses its handle or branch.
pub(super) fn claim(
    root: &NodeRoot,
    identity: &WorkspaceIdentity,
    isolation: RemoteIsolation,
) -> Result<Workspace> {
    if isolation == RemoteIsolation::Container {
        check_docker()?;
    }
    let _project = root.lock_project(&identity.project_id)?;
    for other in root.workspaces() {
        if other.identity.project_id != identity.project_id
            || other.identity.ws_id == identity.ws_id
        {
            continue;
        }
        if other.identity.handle == identity.handle {
            return Err(refuse(
                code::CONFLICT,
                format!(
                    "handle '{}' is already used by workspace {} on this node",
                    identity.handle, other.identity.ws_id
                ),
            ));
        }
        if other.identity.branch == identity.branch {
            return Err(refuse(
                code::CONFLICT,
                format!(
                    "branch '{}' is already used by workspace {} on this node",
                    identity.branch, other.identity.ws_id
                ),
            ));
        }
    }
    let workspace = Workspace::new(root, identity.clone(), isolation);
    init_repo(&workspace.repo_path)?;
    root.save(&workspace)?;
    Ok(workspace)
}

fn check_docker() -> Result<()> {
    let mut cmd = Command::new("docker");
    cmd.args(["info", "--format", "{{.ServerVersion}}"]);
    let unusable = |detail: String| {
        refuse(
            code::UNSUPPORTED_RUNTIME,
            format!("container isolation needs docker on the node: {detail}"),
        )
    };
    match crate::remote::transport::run_bounded(cmd, Stdio::null(), Some(DOCKER_TIMEOUT)) {
        Ok(Some(output)) if output.success() => Ok(()),
        Ok(Some(output)) => Err(unusable(output.stderr_line())),
        Ok(None) => Err(unusable("`docker info` did not answer".into())),
        Err(_) => Err(unusable("docker is not installed".into())),
    }
}

fn init_repo(repo: &Path) -> Result<()> {
    if repo.join(".git").is_dir() {
        return Ok(());
    }
    fs::create_dir_all(repo).with_context(|| format!("Failed to create {}", repo.display()))?;
    let path = repo.to_string_lossy();
    let default_branch = format!("init.defaultBranch={UNBORN_BRANCH}");
    Cmd::new("git")
        .args(&["-c", &default_branch, "init", "-q", &path])
        .run()
        .context("Failed to create the project repository")?;
    Ok(())
}

fn base_matches(workspace: &Workspace) -> Result<bool> {
    if !workspace.repo_path.join(".git").is_dir() {
        return Ok(false);
    }
    let spec = format!("{}^{{commit}}", base_ref(&workspace.identity.ws_id));
    let sha = git(
        &workspace.repo_path,
        &["rev-parse", "--verify", "-q", &spec],
    )
    .ok();
    Ok(sha.as_deref() == Some(workspace.identity.base_sha.as_str()))
}

pub(super) fn provision_worktree(
    root: &NodeRoot,
    workspace: &mut Workspace,
    payload: ProvisionPayload,
) -> Result<()> {
    let ws_id = workspace.identity.ws_id.clone();
    if !workspace.progress.worktree {
        let _project = root.lock_project(&workspace.identity.project_id)?;
        create_worktree(workspace)?;
        workspace.progress.worktree = true;
        root.save(workspace)?;
    }

    // Each attempt brings the laptop's current config, so a resumed call
    // runs with what the user fixed and the laptop lays out the same panes.
    let config_path = root.config_path(&ws_id);
    crate::frozen_config::write_document(&config_path, &payload.config)?;
    super::secrets::save_workspace(root, &ws_id, &payload.secrets)?;
    workspace.agent = payload.agent.clone();
    workspace.prompt_file_only = payload.prompt_file_only;
    root.save(workspace)?;
    let (config, location) = crate::frozen_config::load(&config_path)?;
    let rel_dir = location
        .map(|location| location.rel_dir)
        .unwrap_or_default();
    // A spawned workspace checks out a commit a remote agent made, which
    // may hold symlinks aimed at paths outside the worktree.
    if workspace.origin.is_some() {
        refuse_symlinks(workspace, &rel_dir, &payload)?;
    }
    let subdir = workspace.worktree_path.join(&rel_dir);
    workspace.working_dir = (!rel_dir.as_os_str().is_empty() && subdir.is_dir()).then_some(subdir);

    if payload.run_file_ops {
        save_recipe(root, &ws_id, &payload)?;
    }
    if !workspace.progress.files {
        if payload.run_file_ops {
            let seed_root = workspace.repo_path.join(&rel_dir);
            let _project = root.lock_project(&workspace.identity.project_id)?;
            fs::create_dir_all(&seed_root)?;
            for path in &payload.cached_seeds {
                if !seed_exists(&seed_root, path)? {
                    return Err(refuse(
                        code::MISSING_SEED,
                        format!("symlink source disappeared: {path}"),
                    ));
                }
            }
            apply_files(workspace, &seed_root, &payload)?;
            exclude_ignored(workspace, &payload.ignored)?;
        }
        workspace.progress.files = true;
        root.save(workspace)?;
    }

    if !workspace.progress.prompt {
        if let Some(text) = &payload.prompt {
            let path = crate::workflow::write_prompt_file(
                Some(&workspace.worktree_path),
                &workspace.identity.branch,
                &crate::prompt::Prompt::Inline(text.clone()),
            )?;
            workspace.prompt_path = Some(path);
        }
        workspace.progress.prompt = true;
        root.save(workspace)?;
    }

    if !workspace.progress.git_identity {
        for (key, value) in [
            ("user.name", &payload.git_user_name),
            ("user.email", &payload.git_user_email),
        ] {
            if let Some(value) = value {
                git(&workspace.repo_path, &["config", key, value])?;
            }
        }
        workspace.progress.git_identity = true;
        root.save(workspace)?;
    }

    if payload.run_hooks {
        run_hooks(root, workspace, &config)?;
    }
    workspace.progress.ready = true;
    root.save(workspace)
}

/// What the laptop sent for `files`, kept so workspaces a remote agent
/// spawns from this one get the same files without reading the agent's
/// worktree.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct FileRecipe {
    pub copies: Vec<FileEntry>,
    pub symlink_patterns: Vec<String>,
    pub ignored: Vec<String>,
}

fn save_recipe(root: &NodeRoot, ws_id: &str, payload: &ProvisionPayload) -> Result<()> {
    let recipe = FileRecipe {
        copies: payload.copies.clone(),
        symlink_patterns: payload.symlink_patterns.clone(),
        ignored: payload.ignored.clone(),
    };
    let path = root.files_path(ws_id);
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    let mut writer = transfer::encoder(file.as_file_mut(), &recipe)?;
    transfer::write_files(&mut writer, recipe.copies.iter())?;
    writer.finish()?;
    file.persist(path)?;
    Ok(())
}

/// The recipe workspace `ws_id` was provisioned with, if it ran file
/// operations.
pub(super) fn load_recipe(
    root: &NodeRoot,
    ws_id: &str,
) -> Result<Option<(FileRecipe, tempfile::TempDir)>> {
    let path = root.files_path(ws_id);
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("Failed to read {}", path.display())),
    };
    let mut reader = flate2::bufread::GzDecoder::new(std::io::BufReader::new(file));
    let mut recipe: FileRecipe = transfer::metadata(&mut reader)?;
    let stage = transfer::read_files(&mut reader, recipe.copies.iter_mut())?;
    Ok(Some((recipe, stage)))
}

/// Refuse to provision when a path the node writes into the worktree goes
/// through a symlink: the config directory, the prompt file and the
/// destinations of `files.symlink`. `files.copy` results are confined by
/// [`write_entries`].
fn refuse_symlinks(
    workspace: &Workspace,
    rel_dir: &Path,
    payload: &ProvisionPayload,
) -> Result<()> {
    let worktree = &workspace.worktree_path;
    let prompt = Path::new(".workmux").join(crate::workflow::prompt_file_name(
        &workspace.identity.branch,
    ));
    let mut paths = vec![rel_dir.to_path_buf(), prompt];
    let seed_root = workspace.repo_path.join(rel_dir);
    for pattern in &payload.symlink_patterns {
        let full = seed_root.join(pattern).to_string_lossy().into_owned();
        for source in glob::glob(&full).into_iter().flatten().flatten() {
            if let Ok(relative) = source.strip_prefix(&seed_root) {
                paths.push(rel_dir.join(relative));
            }
        }
    }
    for path in paths {
        let mut current = worktree.clone();
        for component in path.components() {
            current.push(component);
            match current.symlink_metadata() {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(refuse(
                        code::CONFLICT,
                        format!(
                            "{} is a symlink in the checkout of '{}'; not writing through it",
                            current.display(),
                            workspace.identity.branch
                        ),
                    ));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    Ok(())
}

/// Check out the workspace branch, adopting what an interrupted call left.
fn create_worktree(workspace: &Workspace) -> Result<()> {
    let repo = &workspace.repo_path;
    let path = &workspace.worktree_path;
    let branch = &workspace.identity.branch;
    let branch_ref = format!("refs/heads/{branch}");

    if let Some(registered) = registered_branch(repo, path)? {
        if registered.as_deref() == Some(branch_ref.as_str()) {
            return Ok(());
        }
        return Err(refuse(
            code::CONFLICT,
            format!("{} is a worktree of another branch", path.display()),
        ));
    }
    if path.symlink_metadata().is_ok() {
        if path.join(".git").symlink_metadata().is_ok() {
            return Err(refuse(
                code::CONFLICT,
                format!(
                    "{} exists and holds git data; remove it on the node",
                    path.display()
                ),
            ));
        }
        fs::remove_dir_all(path)
            .with_context(|| format!("Failed to remove stale {}", path.display()))?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let path_text = path.to_string_lossy();
    let tip = git(repo, &["rev-parse", "--verify", "-q", &branch_ref]).ok();
    match tip {
        Some(tip) if tip == workspace.identity.base_sha => {
            git(repo, &["worktree", "add", &path_text, branch])?;
        }
        Some(tip) => {
            return Err(refuse(
                code::CONFLICT,
                format!(
                    "branch '{branch}' already exists on the node at {}; delete it there or \
                     use another branch name",
                    &tip[..tip.len().min(12)]
                ),
            ));
        }
        None => {
            git(
                repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    branch,
                    &path_text,
                    &workspace.identity.base_sha,
                ],
            )?;
        }
    }
    Ok(())
}

/// The branch ref checked out at `path` when git has it registered as a
/// worktree (`Some(None)` for a detached one).
fn registered_branch(repo: &Path, path: &Path) -> Result<Option<Option<String>>> {
    let listing = git(repo, &["worktree", "list", "--porcelain"])?;
    let wanted = crate::util::canon_or_self(path);
    let mut current: Option<PathBuf> = None;
    let mut found = None;
    for line in listing.lines().chain(std::iter::once("")) {
        if let Some(rest) = line.strip_prefix("worktree ") {
            current = Some(crate::util::canon_or_self(Path::new(rest)));
            found = None;
        } else if let Some(rest) = line.strip_prefix("branch ") {
            found = Some(rest.to_string());
        } else if line.is_empty() {
            if current.as_deref() == Some(wanted.as_path()) {
                return Ok(Some(found));
            }
            current = None;
        }
    }
    Ok(None)
}

/// Write `files.copy` results into the worktree, seed missing symlink
/// sources into the project directory, then link them.
fn apply_files(workspace: &Workspace, seed_root: &Path, payload: &ProvisionPayload) -> Result<()> {
    let destination = workspace.working_dir().to_path_buf();
    write_entries(&destination, &payload.copies).context("Failed to copy files")?;
    fs::create_dir_all(seed_root)?;
    seed_entries(seed_root, &payload.seeds).context("Failed to seed symlink sources")?;
    if payload.symlink_patterns.is_empty() {
        return Ok(());
    }
    let files = FileConfig {
        copy: None,
        symlink: Some(payload.symlink_patterns.clone()),
    };
    crate::workflow::file_ops::handle_file_operations(seed_root, &destination, &files)
        .context("Failed to create symlinks")
}

/// Make git on the node ignore the transferred paths the laptop ignores, so
/// they never look like unsaved work or get committed by an agent.
fn exclude_ignored(workspace: &Workspace, ignored: &[String]) -> Result<()> {
    let worktree = &workspace.worktree_path;
    let mut paths = Vec::new();
    for path in ignored {
        let full = workspace.working_dir().join(safe_relative(path)?);
        let Ok(relative) = full.strip_prefix(worktree) else {
            continue;
        };
        paths.push(relative.to_string_lossy().into_owned());
    }
    let already_ignored = crate::git::ignored_paths(worktree, &paths);
    let mut patterns = String::new();
    for path in paths {
        if !already_ignored.contains(&path) {
            patterns.push_str(&format!("/{path}\n"));
        }
    }
    if patterns.is_empty() {
        return Ok(());
    }
    let exclude = git(
        worktree,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ],
    )?;
    let exclude = PathBuf::from(exclude.trim());
    if let Some(parent) = exclude.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)
        .with_context(|| format!("Failed to open {}", exclude.display()))?;
    std::io::Write::write_all(&mut file, patterns.as_bytes())
        .with_context(|| format!("Failed to write {}", exclude.display()))
}

/// Match seed installation's first-copy-wins rule without following a leaf
/// symlink. Ancestors must stay inside the project, even for missing paths.
fn seed_exists(root: &Path, name: &str) -> Result<bool> {
    let relative = safe_relative(name)?;
    let canonical_root = root.canonicalize()?;
    let mut path = root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(_) if index + 1 == components.len() => return Ok(true),
            Ok(_) => {
                if !path.canonicalize()?.starts_with(&canonical_root) {
                    return Err(anyhow!("{name} leaves {}", root.display()));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err.into()),
        }
    }
    Ok(false)
}

/// Write `entries` under `root` where nothing exists yet; an existing entry
/// is kept along with everything below it. New entries are built in a
/// staging directory and renamed into place whole, so an interrupted call
/// leaves no partial tree for a retry to keep.
fn seed_entries(root: &Path, entries: &[FileEntry]) -> Result<()> {
    let mut missing = Vec::new();
    let mut kept = Vec::new();
    for entry in entries {
        let relative = safe_relative(&entry.path)?;
        if kept
            .iter()
            .any(|prefix: &PathBuf| relative.starts_with(prefix))
        {
            continue;
        }
        if seed_exists(root, &entry.path)? {
            kept.push(relative);
        } else {
            missing.push(entry);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let stage = tempfile::Builder::new()
        .prefix(".workmux-seed-")
        .tempdir_in(root)
        .context("Failed to create a staging directory")?;
    write_entries(stage.path(), missing.iter().copied())?;
    let mut settled: Vec<PathBuf> = Vec::new();
    for entry in missing {
        let relative = safe_relative(&entry.path)?;
        if settled.iter().any(|prefix| relative.starts_with(prefix)) {
            continue;
        }
        let path = root.join(&relative);
        if path.symlink_metadata().is_err() {
            create_parent_within(root, &path, &entry.path)?;
            fs::rename(stage.path().join(&relative), &path)
                .with_context(|| format!("Failed to place {}", path.display()))?;
        }
        settled.push(relative);
    }
    Ok(())
}

/// Create the parent of `path`, refusing one that resolves outside `root`.
fn create_parent_within(root: &Path, path: &Path, name: &str) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("Failed to resolve {}", root.display()))?;
    if !parent.canonicalize()?.starts_with(&canonical_root) {
        return Err(anyhow!("{name} leaves {}", root.display()));
    }
    Ok(())
}

/// Write `entries` under `root`, replacing what is there.
fn write_entries<'a>(root: &Path, entries: impl IntoIterator<Item = &'a FileEntry>) -> Result<()> {
    for entry in entries {
        let relative = safe_relative(&entry.path)?;
        let path = root.join(&relative);
        let existing = path.symlink_metadata().ok();
        create_parent_within(root, &path, &entry.path)?;
        if let Some(meta) = existing {
            let both_dirs = meta.is_dir() && matches!(entry.kind, FileEntryKind::Dir);
            if meta.is_dir() && !both_dirs {
                fs::remove_dir_all(&path)?;
            } else if !meta.is_dir() {
                fs::remove_file(&path)?;
            }
        }
        match &entry.kind {
            FileEntryKind::Dir => fs::create_dir_all(&path)?,
            FileEntryKind::File { mode, source, .. } => {
                fs::copy(source, &path)
                    .with_context(|| format!("Failed to write {}", path.display()))?;
                fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o777))?;
            }
            FileEntryKind::Symlink { target } => {
                std::os::unix::fs::symlink(target, &path)
                    .with_context(|| format!("Failed to create symlink {}", path.display()))?;
            }
        }
    }
    Ok(())
}

pub(crate) fn safe_relative(path: &str) -> Result<PathBuf> {
    let relative = PathBuf::from(path);
    let valid = !path.is_empty()
        && relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if !valid {
        return Err(refuse(
            code::BAD_REQUEST,
            format!("file path must be relative without '..': {path:?}"),
        ));
    }
    Ok(relative)
}

/// Run the `post_create` hooks that have not finished yet. A hook is
/// recorded before it starts, so one that was cut off runs again.
fn run_hooks(root: &NodeRoot, workspace: &mut Workspace, config: &Config) -> Result<()> {
    let hooks = config.post_create.clone().unwrap_or_default();
    if workspace.progress.hooks_done >= hooks.len() {
        return Ok(());
    }
    let working_dir = workspace.working_dir().to_path_buf();
    let env = hook_env(workspace, Some(&working_dir));
    for (index, command) in hooks.iter().enumerate().skip(workspace.progress.hooks_done) {
        if workspace.progress.hook_started == Some(index) {
            eprintln!(
                "workmux: running post_create hook again, it did not finish last time: {command}"
            );
        }
        workspace.progress.hook_started = Some(index);
        root.save(workspace)?;
        run_hook(config, command, &working_dir, &env)
            .with_context(|| format!("Failed to run post-create command: '{command}'"))?;
        workspace.progress.hooks_done = index + 1;
        workspace.progress.hook_started = None;
        root.save(workspace)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_only() {
        assert!(safe_relative("a/b").is_ok());
        for bad in ["", "/etc", "../x", "a/../../x", "./a"] {
            assert!(safe_relative(bad).is_err(), "{bad}");
        }
    }

    fn file(source_dir: &Path, path: &str, text: &str) -> FileEntry {
        let source = source_dir.join(path);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, text).unwrap();
        FileEntry {
            path: path.into(),
            kind: FileEntryKind::File {
                mode: 0o640,
                size: text.len() as u64,
                source,
            },
        }
    }

    #[test]
    fn seeds_keep_existing_subtrees() {
        let source = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("deps")).unwrap();
        fs::write(dir.path().join("deps/old"), "old").unwrap();
        let entries = vec![
            FileEntry {
                path: "deps".into(),
                kind: FileEntryKind::Dir,
            },
            file(source.path(), "deps/new", "new"),
            file(source.path(), ".env", "A=1"),
            FileEntry {
                path: "lib".into(),
                kind: FileEntryKind::Dir,
            },
            file(source.path(), "lib/a", "a"),
        ];
        seed_entries(dir.path(), &entries).unwrap();
        assert!(!dir.path().join("deps/new").exists());
        assert_eq!(fs::read_to_string(dir.path().join("lib/a")).unwrap(), "a");
        let left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".workmux-seed-"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(fs::read_to_string(dir.path().join(".env")).unwrap(), "A=1");
        let mode = fs::metadata(dir.path().join(".env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
    }

    #[test]
    fn seed_queries_match_existing_entries_and_refuse_escaping_parents() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sub/cache")).unwrap();
        assert!(seed_exists(dir.path(), "sub/cache").unwrap());
        assert!(!seed_exists(dir.path(), "sub/missing").unwrap());
        fs::remove_dir(dir.path().join("sub/cache")).unwrap();
        assert!(!seed_exists(dir.path(), "sub/cache").unwrap());
        std::os::unix::fs::symlink("missing", dir.path().join("dangling")).unwrap();
        assert!(seed_exists(dir.path(), "dangling").unwrap());
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        assert!(seed_exists(dir.path(), "escape/child").is_err());
        for bad in ["../x", "/etc", "", "sub/../../x"] {
            assert!(seed_exists(dir.path(), bad).is_err());
        }
    }

    #[test]
    fn existing_seeds_are_not_read_or_staged() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("cache")).unwrap();
        let entries = [
            FileEntry {
                path: "cache".into(),
                kind: FileEntryKind::Dir,
            },
            FileEntry {
                path: "cache/data".into(),
                kind: FileEntryKind::File {
                    mode: 0o600,
                    size: 1,
                    source: dir.path().join("absent"),
                },
            },
        ];
        seed_entries(dir.path(), &entries).unwrap();
        assert!(!dir.path().join("cache/data").exists());
        fs::remove_dir(dir.path().join("cache")).unwrap();
        assert!(seed_entries(dir.path(), &entries).is_err());
        assert!(!dir.path().join("cache").exists());
    }

    #[test]
    fn copies_overwrite_and_refuse_escaping_symlinks() {
        let source = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), "old").unwrap();
        write_entries(dir.path(), &[file(source.path(), "a", "new")]).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "new");

        let entries = vec![
            FileEntry {
                path: "link".into(),
                kind: FileEntryKind::Symlink {
                    target: outside.path().to_string_lossy().into_owned(),
                },
            },
            file(source.path(), "link/escaped", "x"),
        ];
        assert!(write_entries(dir.path(), &entries).is_err());
        assert!(!outside.path().join("escaped").exists());
    }
}
