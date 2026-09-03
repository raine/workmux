use anyhow::{Context, Result, anyhow, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

const CONFIG_OVERRIDES: &[(&str, &str)] = &[
    ("core.fsmonitor", "false"),
    ("core.hooksPath", "/dev/null"),
    ("core.pager", "cat"),
    ("core.editor", "true"),
    ("core.askPass", ""),
    ("sequence.editor", "true"),
    ("credential.interactive", "false"),
    ("commit.gpgSign", "false"),
    ("tag.gpgSign", "false"),
    ("gpg.program", "false"),
    ("diff.external", ""),
    ("interactive.diffFilter", ""),
    ("status.showUntrackedFiles", "all"),
    ("protocol.allow", "never"),
    ("protocol.file.allow", "always"),
    ("protocol.http.allow", "always"),
    ("protocol.https.allow", "always"),
    ("protocol.ssh.allow", "always"),
    ("protocol.git.allow", "always"),
    ("protocol.ext.allow", "never"),
    ("uploadpack.packObjectsHook", ""),
];

const GIT_ENVIRONMENT: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_ASKPASS",
    "GIT_CEILING_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_SYSTEM",
    "GIT_DIR",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_EXEC_PATH",
    "GIT_EXTERNAL_DIFF",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_QUARANTINE_PATH",
    "GIT_SHALLOW_FILE",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_PROXY_COMMAND",
    "GIT_ATTR_SOURCE",
    "GIT_ALLOW_PROTOCOL",
    "GIT_TEMPLATE_DIR",
    "GIT_TRACE2",
    "GIT_TRACE2_EVENT",
    "GIT_TRACE2_PERF",
    "GIT_TRACE2_BRIEF",
    "GIT_TRACE2_CONFIG_PARAMS",
    "GIT_TRACE2_DST_DEBUG",
    "GIT_WORK_TREE",
];

/// Paths that identify one repository without consulting repository configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryIdentity {
    pub worktree: PathBuf,
    pub admin_dir: PathBuf,
    pub common_dir: PathBuf,
    pub dot_git: PathBuf,
    pub is_bare: bool,
}

fn canonical_file(path: &Path, label: &str) -> Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("Failed to inspect {label} at {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{label} must be a regular file: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            bail!("{label} must not have hard links: {}", path.display());
        }
    }
    path.canonicalize()
        .with_context(|| format!("Failed to resolve {label} at {}", path.display()))
}

fn read_pointer(path: &Path, prefix: Option<&str>, label: &str) -> Result<PathBuf> {
    canonical_file(path, label)?;
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {label} at {}", path.display()))?;
    let value = match prefix {
        Some(prefix) => contents
            .trim()
            .strip_prefix(prefix)
            .ok_or_else(|| anyhow!("Invalid {label} at {}", path.display()))?,
        None => contents.trim(),
    };
    if value.is_empty() || contents.lines().count() != 1 {
        bail!("Invalid {label} at {}", path.display());
    }
    let value = Path::new(value);
    Ok(if value.is_absolute() {
        value.to_path_buf()
    } else {
        path.parent().unwrap_or(Path::new("/")).join(value)
    })
}

fn validate_repository_control_files(common_dir: &Path, admin_dir: &Path) -> Result<()> {
    canonical_file(&common_dir.join("config"), "repository config")?;
    let worktree_config = admin_dir.join("config.worktree");
    if worktree_config.exists() {
        canonical_file(&worktree_config, "worktree config")?;
    }
    let hooks = common_dir.join("hooks");
    if hooks.exists() {
        let metadata = std::fs::symlink_metadata(&hooks)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("Git hooks path must be a directory: {}", hooks.display());
        }
        for entry in std::fs::read_dir(&hooks)? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!("Git hooks must not be symbolic links: {}", path.display());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.is_file() && metadata.nlink() > 1 {
                    bail!("Git hooks must not have hard links: {}", path.display());
                }
            }
        }
    }
    Ok(())
}

impl RepositoryIdentity {
    /// Resolve and cross-check the worktree, linked-worktree admin directory, and common directory.
    pub fn discover(path: &Path) -> Result<Self> {
        let start = path
            .canonicalize()
            .with_context(|| format!("Failed to resolve repository path {}", path.display()))?;
        if start.join("HEAD").is_file()
            && start.join("objects").is_dir()
            && start.join("config").is_file()
        {
            validate_repository_control_files(&start, &start)?;
            return Ok(Self {
                worktree: start.clone(),
                admin_dir: start.clone(),
                common_dir: start.clone(),
                dot_git: start,
                is_bare: true,
            });
        }
        let mut candidate = Some(start.as_path());
        let worktree = loop {
            let dir = candidate.ok_or_else(|| anyhow!("Not a Git worktree: {}", path.display()))?;
            if std::fs::symlink_metadata(dir.join(".git")).is_ok() {
                break dir.to_path_buf();
            }
            candidate = dir.parent();
        };

        let dot_git = worktree.join(".git");
        let dot_git_metadata = std::fs::symlink_metadata(&dot_git)?;
        if dot_git_metadata.file_type().is_symlink() {
            bail!("Worktree .git path must not be a symbolic link");
        }
        if dot_git_metadata.is_dir() {
            let common_dir = dot_git.canonicalize()?;
            validate_repository_control_files(&common_dir, &common_dir)?;
            return Ok(Self {
                worktree,
                admin_dir: common_dir.clone(),
                common_dir,
                dot_git,
                is_bare: false,
            });
        }

        let admin_dir = read_pointer(&dot_git, Some("gitdir: "), "worktree .git pointer")?
            .canonicalize()
            .context("Failed to resolve linked-worktree admin directory")?;
        if !admin_dir.is_dir() {
            bail!(
                "Linked-worktree admin directory is not a directory: {}",
                admin_dir.display()
            );
        }

        let commondir_file = admin_dir.join("commondir");
        let common_dir = read_pointer(&commondir_file, None, "commondir pointer")?
            .canonicalize()
            .context("Failed to resolve Git common directory")?;
        let expected_admin_parent = common_dir.join("worktrees").canonicalize()?;
        if admin_dir.parent() != Some(expected_admin_parent.as_path()) {
            bail!("Linked-worktree admin directory is outside the expected common directory");
        }

        let backlink = read_pointer(&admin_dir.join("gitdir"), None, "gitdir pointer")?
            .canonicalize()
            .context("Failed to resolve Git worktree backlink")?;
        if backlink != dot_git.canonicalize()? {
            bail!("Linked-worktree gitdir pointer does not identify the expected worktree");
        }
        validate_repository_control_files(&common_dir, &admin_dir)?;

        Ok(Self {
            worktree,
            admin_dir,
            common_dir,
            dot_git,
            is_bare: false,
        })
    }
}

fn protected_git(workdir: Option<&Path>, interactive: bool) -> Result<Command> {
    let mut command = Command::new("git");
    clear_ambient_git_env(&mut command);
    command.arg("--no-pager").env("GIT_PAGER", "cat");
    if !interactive {
        command
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true")
            .env("GIT_MERGE_AUTOEDIT", "no")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never");
    }
    for (key, value) in CONFIG_OVERRIDES {
        if interactive && matches!(*key, "core.editor" | "sequence.editor") {
            continue;
        }
        command.args(["-c", &format!("{key}={value}")]);
    }

    if let Some(path) = workdir {
        command.current_dir(path);
        match RepositoryIdentity::discover(path) {
            Ok(identity) => {
                command
                    .env("GIT_DIR", &identity.admin_dir)
                    .env("GIT_COMMON_DIR", &identity.common_dir);
                if !identity.is_bare {
                    command.env("GIT_WORK_TREE", &identity.worktree);
                }
            }
            Err(error) => {
                let mut candidate = Some(path);
                while let Some(dir) = candidate {
                    if std::fs::symlink_metadata(dir.join(".git")).is_ok() {
                        return Err(error.context("Refusing to follow invalid Git metadata"));
                    }
                    candidate = dir.parent();
                }
            }
        }
    }
    Ok(command)
}

/// Construct a non-interactive Git process with hooks, prompts, and ambient overrides disabled.
pub fn unattended_git(workdir: Option<&Path>) -> Result<Command> {
    protected_git(workdir, false)
}

/// Construct an interactive Git process with hooks and ambient overrides disabled.
pub fn interactive_git(workdir: &Path) -> Result<Command> {
    protected_git(Some(workdir), true)
}

/// Construct a non-interactive Git process with a validated and pinned repository identity.
pub fn pinned_git(workdir: &Path) -> Result<Command> {
    RepositoryIdentity::discover(workdir)?;
    protected_git(Some(workdir), false)
}

pub fn clear_ambient_git_env(command: &mut Command) {
    for key in GIT_ENVIRONMENT {
        command.env_remove(key);
    }
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_CONFIG_KEY_")
            || key.to_string_lossy().starts_with("GIT_CONFIG_VALUE_")
        {
            command.env_remove(key);
        }
    }
}

/// Copy repository-local configuration without following include directives.
pub fn snapshot_local_config(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".config-snapshot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::write(&temp, b"")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }
    if !source.is_file() {
        std::fs::rename(temp, destination)?;
        return Ok(());
    }

    let output = unattended_git(None)?
        .args([
            OsStr::new("config"),
            OsStr::new("--file"),
            source.as_os_str(),
            OsStr::new("--null"),
            OsStr::new("--list"),
        ])
        .output()?;
    if !output.status.success() {
        bail!("Failed to read Git config at {}", source.display());
    }
    let mut entries = Vec::new();
    for entry in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let (key, value) = match entry.iter().position(|byte| *byte == b'\n') {
            Some(separator) => (
                String::from_utf8_lossy(&entry[..separator]),
                Some(String::from_utf8_lossy(&entry[separator + 1..]).into_owned()),
            ),
            None => (String::from_utf8_lossy(entry), None),
        };
        if key.eq_ignore_ascii_case("include.path")
            || key.to_ascii_lowercase().starts_with("includeif.")
            || key.eq_ignore_ascii_case("core.worktree")
        {
            continue;
        }
        entries.push((key.into_owned(), value));
    }

    std::fs::write(&temp, serialize_config(&entries))?;
    std::fs::rename(temp, destination)?;
    Ok(())
}

/// Render `git config --list` entries back into config file syntax, quoting every value so that
/// whitespace, comment characters, quotes, backslashes, newlines, and tabs survive a round trip.
fn serialize_config(entries: &[(String, Option<String>)]) -> String {
    let mut rendered = String::new();
    let mut current: Option<(String, Option<String>)> = None;
    for (key, value) in entries {
        let Some((section, subsection, name)) = split_key(key) else {
            continue;
        };
        let group = (section.to_string(), subsection.map(str::to_string));
        if current.as_ref() != Some(&group) {
            match &group.1 {
                Some(subsection) => rendered.push_str(&format!(
                    "[{} \"{}\"]\n",
                    group.0,
                    escape_subsection(subsection)
                )),
                None => rendered.push_str(&format!("[{}]\n", group.0)),
            }
            current = Some(group);
        }
        match value {
            Some(value) => {
                rendered.push_str(&format!("\t{} = \"{}\"\n", name, escape_quoted(value)))
            }
            None => rendered.push_str(&format!("\t{}\n", name)),
        }
    }
    rendered
}

fn split_key(key: &str) -> Option<(&str, Option<&str>, &str)> {
    let (section, rest) = key.split_once('.')?;
    if section.is_empty() {
        return None;
    }
    match rest.rsplit_once('.') {
        Some((subsection, name)) => Some((section, Some(subsection), name)),
        None => Some((section, None, rest)),
    }
}

fn escape_subsection(subsection: &str) -> String {
    subsection.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape_quoted(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn linked_repo() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let worktree = temp.path().join("worktree");
        std::fs::create_dir(&main).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&main)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&main)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&main)
            .status()
            .unwrap();
        std::fs::write(main.join("tracked"), "base").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&main)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-qm", "base"])
            .current_dir(&main)
            .status()
            .unwrap();
        Command::new("git")
            .args(["worktree", "add", "-qb", "topic"])
            .arg(&worktree)
            .current_dir(&main)
            .status()
            .unwrap();
        (temp, worktree)
    }

    #[test]
    fn production_git_processes_use_the_protected_constructor() {
        fn visit(path: &Path, violations: &mut Vec<String>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, violations);
                } else if path.extension() == Some(OsStr::new("rs")) {
                    let source = std::fs::read_to_string(&path).unwrap();
                    let production_end = source
                        .rfind("\n#[cfg(test)]\nmod tests")
                        .unwrap_or(source.len());
                    let production = &source[..production_end];
                    if production.contains("Command::new(\"git\")")
                        || production.contains("std::process::Command::new(\"git\")")
                    {
                        let is_constructor = path.ends_with("git/security.rs")
                            && production.matches("Command::new(\"git\")").count() == 1;
                        let is_test_support = path.ends_with("test_support.rs");
                        if !is_constructor && !is_test_support {
                            violations.push(path.display().to_string());
                        }
                    }
                }
            }
        }

        let mut violations = Vec::new();
        visit(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut violations,
        );
        assert!(
            violations.is_empty(),
            "Git commands bypass the protected constructor: {violations:?}"
        );
    }

    #[test]
    fn protected_git_clears_transport_and_config_environment() {
        let (temp, worktree) = linked_repo();
        let marker = temp.path().join("transport-marker");
        let script = worktree.join("transport.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();

        let command = pinned_git(&worktree).unwrap();
        let environment = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        for key in [
            "GIT_SSH",
            "GIT_SSH_COMMAND",
            "GIT_PROXY_COMMAND",
            "GIT_ALLOW_PROTOCOL",
            "GIT_ATTR_SOURCE",
            "GIT_TRACE2",
        ] {
            assert_eq!(environment.get(key), Some(&None), "{key} was not cleared");
        }
        for key in [
            "GIT_CONFIG",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
        ] {
            assert_eq!(environment.get(key), Some(&None), "{key} was not cleared");
        }
        assert!(!marker.exists());
    }

    #[test]
    fn protected_git_honors_global_excludes() {
        let (temp, worktree) = linked_repo();
        let global_config = temp.path().join("global-config");
        let global_excludes = temp.path().join("global-excludes");
        std::fs::write(&global_excludes, "globally-ignored\n").unwrap();
        std::fs::write(
            &global_config,
            format!("[core]\n\texcludesFile = {}\n", global_excludes.display()),
        )
        .unwrap();
        std::fs::write(worktree.join("globally-ignored"), "ignored\n").unwrap();

        let output = pinned_git(&worktree)
            .unwrap()
            .env("GIT_CONFIG_GLOBAL", &global_config)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();

        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().is_empty());
    }

    #[test]
    fn protected_git_disables_static_executable_policy() {
        let (_temp, worktree) = linked_repo();
        let command = pinned_git(&worktree).unwrap();
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for expected in [
            "core.fsmonitor=false",
            "core.hooksPath=/dev/null",
            "gpg.program=false",
            "interactive.diffFilter=",
            "protocol.allow=never",
            "status.showUntrackedFiles=all",
        ] {
            assert!(arguments.iter().any(|argument| argument == expected));
        }
    }

    #[test]
    fn identity_rejects_modified_worktree_pointer() {
        let (_temp, worktree) = linked_repo();
        std::fs::write(worktree.join(".git"), "gitdir: /tmp\n").unwrap();
        assert!(RepositoryIdentity::discover(&worktree).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn identity_rejects_hard_linked_policy_files() {
        let (_temp, worktree) = linked_repo();
        let identity = RepositoryIdentity::discover(&worktree).unwrap();
        std::fs::hard_link(
            identity.common_dir.join("config"),
            worktree.join("config-alias"),
        )
        .unwrap();
        assert!(RepositoryIdentity::discover(&worktree).is_err());
    }

    #[test]
    fn identity_rejects_modified_admin_pointers() {
        let (_temp, worktree) = linked_repo();
        let identity = RepositoryIdentity::discover(&worktree).unwrap();
        std::fs::write(identity.admin_dir.join("commondir"), "/tmp\n").unwrap();
        assert!(RepositoryIdentity::discover(&worktree).is_err());

        let (_temp, worktree) = linked_repo();
        let identity = RepositoryIdentity::discover(&worktree).unwrap();
        std::fs::write(identity.admin_dir.join("gitdir"), "/tmp/missing\n").unwrap();
        assert!(RepositoryIdentity::discover(&worktree).is_err());
    }

    #[test]
    fn protected_commit_does_not_run_repository_hooks() {
        let (temp, worktree) = linked_repo();
        let identity = RepositoryIdentity::discover(&worktree).unwrap();
        let marker = temp.path().join("hook-marker");
        let hook = identity.common_dir.join("hooks/pre-commit");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&hook, permissions).unwrap();
        }
        let status = pinned_git(&worktree)
            .unwrap()
            .args(["commit", "--allow-empty", "-m", "safe"])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn protected_worktree_add_runs_configured_content_filter() {
        use std::os::unix::fs::PermissionsExt;

        let (temp, worktree) = linked_repo();
        let marker = temp.path().join("filter-marker");
        let filter = temp.path().join("smudge-filter.sh");
        std::fs::write(
            &filter,
            format!("#!/bin/sh\ncat\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&filter).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&filter, permissions).unwrap();

        Command::new("git")
            .args([
                "config",
                "filter.workmux-test.smudge",
                filter.to_string_lossy().as_ref(),
            ])
            .current_dir(&worktree)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "filter.workmux-test.clean", "cat"])
            .current_dir(&worktree)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "filter.workmux-test.required", "true"])
            .current_dir(&worktree)
            .status()
            .unwrap();
        std::fs::write(
            worktree.join(".gitattributes"),
            "filtered filter=workmux-test\n",
        )
        .unwrap();
        std::fs::write(worktree.join("filtered"), "filtered content\n").unwrap();
        Command::new("git")
            .args(["add", ".gitattributes", "filtered"])
            .current_dir(&worktree)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-qm", "add filtered content"])
            .current_dir(&worktree)
            .status()
            .unwrap();

        let destination = temp.path().join("filtered-worktree");
        let output = pinned_git(&worktree)
            .unwrap()
            .args(["worktree", "add", "-qb", "filtered-topic"])
            .arg(&destination)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(marker.exists());
        assert_eq!(
            std::fs::read_to_string(destination.join("filtered")).unwrap(),
            "filtered content\n"
        );
    }

    #[test]
    fn protected_git_honors_credential_and_transport_configuration() {
        let (_temp, worktree) = linked_repo();
        let command = pinned_git(&worktree).unwrap();
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for blocked in [
            "credential.helper=",
            "core.sshCommand=ssh",
            "core.gitProxy=none",
        ] {
            assert!(!arguments.iter().any(|argument| argument == blocked));
        }
    }

    #[test]
    fn unprotected_status_fixture_executes_fsmonitor() {
        let (temp, worktree) = linked_repo();
        let marker = temp.path().join("positive-control-marker");
        let script = worktree.join("monitor.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions).unwrap();
        }
        let common = RepositoryIdentity::discover(&worktree).unwrap().common_dir;
        Command::new("git")
            .args([
                "config",
                "--file",
                common.join("config").to_string_lossy().as_ref(),
                "core.fsmonitor",
                script.to_string_lossy().as_ref(),
            ])
            .status()
            .unwrap();
        Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&worktree)
            .status()
            .unwrap();
        assert!(marker.exists());
    }

    #[test]
    fn pinned_status_ignores_fsmonitor_and_ambient_config() {
        let (temp, worktree) = linked_repo();
        let marker = temp.path().join("marker");
        let script = worktree.join("monitor.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let common = RepositoryIdentity::discover(&worktree).unwrap().common_dir;
        Command::new("git")
            .args([
                "config",
                "--file",
                common.join("config").to_string_lossy().as_ref(),
                "core.fsmonitor",
                script.to_string_lossy().as_ref(),
            ])
            .status()
            .unwrap();

        let status = pinned_git(&worktree)
            .unwrap()
            .args(["status", "--porcelain"])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!marker.exists());
    }

    #[test]
    fn config_snapshot_round_trips_entries_and_drops_includes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("config");
        std::fs::write(
            &source,
            concat!(
                "[core]\n",
                "\tbare = false\n",
                "\tworktree = ../elsewhere\n",
                "[remote \"origin\"]\n",
                "\turl = https://example.com/repo.git\n",
                "\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
                "\tfetch = +refs/tags/*:refs/tags/*\n",
                "[branch \"feat/foo.bar\"]\n",
                "\tmerge = refs/heads/feat/foo.bar\n",
                "\tdescription = \"leading and trailing \"\n",
                "[alias]\n",
                "\tlg = log --format=\"# %h\"\n",
                "\tquiet\n",
                "[includeIf \"gitdir:/somewhere/\"]\n",
                "\tpath = ../other-config\n",
                "[include]\n",
                "\tpath = ../another-config\n",
            ),
        )
        .unwrap();

        let snapshot = temp.path().join("nested/snapshot");
        snapshot_local_config(&source, &snapshot).unwrap();

        let expected: Vec<String> = list_config(&source)
            .into_iter()
            .filter(|entry| {
                !entry.starts_with("includeif.")
                    && !entry.starts_with("include.path")
                    && !entry.starts_with("core.worktree")
            })
            .collect();
        assert_eq!(list_config(&snapshot), expected);
        assert!(expected.iter().any(|entry| entry == "alias.quiet"));
        assert!(
            expected
                .iter()
                .any(|entry| entry == "branch.feat/foo.bar.description=leading and trailing ")
        );
    }

    #[test]
    fn config_snapshot_round_trips_awkward_values() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("config");
        let values = [
            ("workmux.hash", "# not a comment"),
            ("workmux.semicolon", "; also not a comment"),
            ("workmux.quote", "say \"hi\""),
            ("workmux.backslash", "C:\\path\\to"),
            ("workmux.newline", "first\nsecond"),
            ("workmux.tab", "left\tright"),
            ("workmux.spaces", "  padded  "),
            ("workmux.empty", ""),
            ("submodule.deps/vendor \"x\".path", "deps/vendor"),
            ("branch.topic\tname.remote", "origin"),
        ];
        std::fs::write(&source, b"").unwrap();
        for (key, value) in values {
            let status = Command::new("git")
                .args([
                    "config",
                    "--file",
                    source.to_string_lossy().as_ref(),
                    "--add",
                    key,
                    value,
                ])
                .status()
                .unwrap();
            assert!(status.success());
        }

        let snapshot = temp.path().join("snapshot");
        snapshot_local_config(&source, &snapshot).unwrap();
        assert_eq!(list_config(&snapshot), list_config(&source));
    }

    fn list_config(path: &Path) -> Vec<String> {
        let output = Command::new("git")
            .args([
                "config",
                "--file",
                path.to_string_lossy().as_ref(),
                "--null",
                "--list",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| String::from_utf8_lossy(entry).replace('\n', "="))
            .collect()
    }
}
