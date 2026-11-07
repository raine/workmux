//! Agent-specific conversation forking for resuming sessions across worktrees.

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Information about a conversation session
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Session UUID
    pub id: String,
    /// Full path to the session transcript
    pub path: PathBuf,
    /// Last modification time
    pub timestamp: SystemTime,
}

/// Trait for agent-specific conversation forking
pub trait ConversationForker: Send + Sync {
    /// Find the most recent conversation for a worktree path
    fn find_latest_conversation(&self, worktree_path: &Path) -> Result<Option<SessionInfo>>;

    /// Find a specific conversation by session ID (or prefix)
    fn find_conversation(
        &self,
        worktree_path: &Path,
        session_id: &str,
    ) -> Result<Option<SessionInfo>>;

    /// Make the session resumable from the target worktree.
    ///
    /// Agents that scope conversations by project directory copy the session
    /// there; agents that fork natively leave the stored session alone.
    /// Returns the session ID or path for resume args.
    fn prepare_fork(&self, session: &SessionInfo, target_worktree: &Path) -> Result<String>;

    /// CLI args that resume a specific session in `worktree_path`
    /// (e.g., ["--resume", uuid])
    fn resume_args(&self, session_id: &str, worktree_path: &Path) -> Vec<String>;
}

/// Claude Code conversation forker
pub struct ClaudeForker {
    config_dir: PathBuf,
}

impl ClaudeForker {
    pub fn new() -> Self {
        let config_dir = std::env::var("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                home::home_dir()
                    .expect("could not determine home directory")
                    .join(".claude")
            });
        Self { config_dir }
    }

    /// Encode a path the same way Claude Code does for project directories.
    /// Non-alphanumeric characters (except `-`) become `-`.
    fn encode_path(path: &Path) -> String {
        path.to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }

    fn projects_dir(&self) -> PathBuf {
        self.config_dir.join("projects")
    }

    fn project_dir_for(&self, worktree_path: &Path) -> PathBuf {
        self.projects_dir().join(Self::encode_path(worktree_path))
    }

    /// List all .jsonl sessions in a project dir, sorted by mtime descending
    fn list_sessions(&self, project_dir: &Path) -> Result<Vec<SessionInfo>> {
        if !project_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in fs::read_dir(project_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                let metadata = fs::metadata(&path)?;
                sessions.push(SessionInfo {
                    id: stem.to_string(),
                    path: path.clone(),
                    timestamp: metadata.modified()?,
                });
            }
        }

        sessions.sort_by_key(|session| std::cmp::Reverse(session.timestamp));
        Ok(sessions)
    }
}

impl ConversationForker for ClaudeForker {
    fn find_latest_conversation(&self, worktree_path: &Path) -> Result<Option<SessionInfo>> {
        let project_dir = self.project_dir_for(worktree_path);
        let sessions = self.list_sessions(&project_dir)?;
        Ok(sessions.into_iter().next())
    }

    fn find_conversation(
        &self,
        worktree_path: &Path,
        session_id: &str,
    ) -> Result<Option<SessionInfo>> {
        let project_dir = self.project_dir_for(worktree_path);
        let sessions = self.list_sessions(&project_dir)?;
        // Match by exact ID or prefix
        Ok(sessions
            .into_iter()
            .find(|s| s.id == session_id || s.id.starts_with(session_id)))
    }

    fn prepare_fork(&self, session: &SessionInfo, target_worktree: &Path) -> Result<String> {
        let target_dir = self.project_dir_for(target_worktree);
        fs::create_dir_all(&target_dir).context("Failed to create target project directory")?;

        // Copy the .jsonl file
        let target_jsonl = target_dir.join(format!("{}.jsonl", session.id));
        fs::copy(&session.path, &target_jsonl).context("Failed to copy conversation file")?;

        // Copy the session subdirectory if it exists (tool results, subagent data)
        let source_dir = session.path.parent().unwrap();
        let session_subdir = source_dir.join(&session.id);
        if session_subdir.is_dir() {
            let target_subdir = target_dir.join(&session.id);
            crate::workflow::file_ops::copy_dir_recursive(&session_subdir, &target_subdir)
                .context("Failed to copy session data directory")?;
        }

        Ok(session.id.clone())
    }

    fn resume_args(&self, session_id: &str, _worktree_path: &Path) -> Vec<String> {
        // The conversation was copied into the target worktree's project
        // directory, so Claude resumes it wherever it is launched.
        vec!["--resume".to_string(), session_id.to_string()]
    }
}

/// Pi forks natively from a transcript path into the launch directory.
pub struct PiForker {
    agent_dir: PathBuf,
    session_dir: Option<PathBuf>,
}

impl PiForker {
    pub fn new() -> Self {
        let env_path = |name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| crate::util::expand_tilde(&value))
        };
        Self {
            agent_dir: env_path("PI_CODING_AGENT_DIR").unwrap_or_else(|| {
                home::home_dir()
                    .expect("could not determine home directory")
                    .join(".pi/agent")
            }),
            session_dir: env_path("PI_CODING_AGENT_SESSION_DIR"),
        }
    }

    fn list_sessions(&self, worktree_path: &Path) -> Result<Vec<SessionInfo>> {
        let dirs = if let Some(dir) = &self.session_dir {
            vec![dir.clone()]
        } else {
            let root = self.agent_dir.join("sessions");
            if !root.is_dir() {
                return Ok(Vec::new());
            }
            // Read session headers rather than reproducing Pi's path encoding.
            // This also handles sessions created through a symlink to the project.
            fs::read_dir(root)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?
        };
        let mut sessions = Vec::new();
        for dir in dirs {
            if !dir.is_dir() {
                continue;
            }
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl")
                    || !entry.file_type()?.is_file()
                {
                    continue;
                }
                let mut head = String::new();
                BufReader::new(fs::File::open(&path)?).read_line(&mut head)?;
                let Ok(header) = serde_json::from_str::<serde_json::Value>(&head) else {
                    continue;
                };
                let Some(id) = header
                    .get("id")
                    .and_then(|id| id.as_str())
                    .filter(|id| !id.is_empty())
                else {
                    continue;
                };
                let Some(cwd) = header.get("cwd").and_then(|cwd| cwd.as_str()) else {
                    continue;
                };
                if header.get("type").and_then(|kind| kind.as_str()) != Some("session")
                    || !paths_match(Path::new(cwd), worktree_path)
                {
                    continue;
                }
                sessions.push(SessionInfo {
                    id: id.to_string(),
                    path,
                    timestamp: entry.metadata()?.modified()?,
                });
            }
        }
        sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(a.path.cmp(&b.path)));
        Ok(sessions)
    }
}

impl ConversationForker for PiForker {
    fn find_latest_conversation(&self, worktree_path: &Path) -> Result<Option<SessionInfo>> {
        Ok(self.list_sessions(worktree_path)?.into_iter().next())
    }

    fn find_conversation(
        &self,
        worktree_path: &Path,
        session_id: &str,
    ) -> Result<Option<SessionInfo>> {
        let matches: Vec<_> = self
            .list_sessions(worktree_path)?
            .into_iter()
            .filter(|session| session.id.starts_with(session_id))
            .collect();
        if let Some(exact) = matches.iter().find(|session| session.id == session_id) {
            return Ok(Some(exact.clone()));
        }
        if matches.len() > 1 {
            bail!(
                "Multiple conversations match '{}'. Use a longer session ID prefix.",
                session_id
            );
        }
        Ok(matches.into_iter().next())
    }

    fn prepare_fork(&self, session: &SessionInfo, _target_worktree: &Path) -> Result<String> {
        let path = session
            .path
            .canonicalize()
            .context("Pi conversation file no longer exists")?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn resume_args(&self, session_path: &str, _worktree_path: &Path) -> Vec<String> {
        vec!["--fork".to_string(), session_path.to_string()]
    }
}

/// Length of the session UUID that terminates a Codex rollout filename.
const UUID_LEN: usize = 36;

/// Session sources that `codex fork` can resume. Other sources (`exec`, `mcp`,
/// subagents) are recorded but not offered as fork targets.
const CODEX_INTERACTIVE_SOURCES: &[&str] = &["cli", "vscode"];

/// Codex conversation forker.
///
/// Codex forks natively via `codex fork <session-id>` and keeps rollouts in its
/// own store, so workmux only resolves which session to fork.
pub struct CodexForker {
    codex_home: PathBuf,
    /// Session the current process was launched from, if any.
    thread_id: Option<String>,
}

/// Fields workmux reads from the `session_meta` line of a Codex rollout.
struct CodexSessionMeta {
    id: String,
    cwd: Option<PathBuf>,
    interactive: bool,
}

impl CodexForker {
    pub fn new() -> Self {
        let codex_home = std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                home::home_dir()
                    .expect("could not determine home directory")
                    .join(".codex")
            });
        let thread_id = std::env::var("CODEX_THREAD_ID")
            .ok()
            .filter(|id| !id.trim().is_empty());
        Self {
            codex_home,
            thread_id,
        }
    }

    fn sessions_dir(&self) -> PathBuf {
        self.codex_home.join("sessions")
    }

    /// Extract the session UUID from `rollout-<timestamp>-<uuid>.jsonl`.
    fn session_id_from_filename(path: &Path) -> Option<String> {
        let stem = path.file_stem().and_then(|s| s.to_str())?;
        let rest = stem.strip_prefix("rollout-")?;
        rest.get(rest.len().checked_sub(UUID_LEN)?..)
            .map(|id| id.to_string())
    }

    /// Read the `session_meta` line at the head of a rollout file.
    fn read_session_meta(path: &Path) -> Option<CodexSessionMeta> {
        let file = fs::File::open(path).ok()?;
        let mut head = String::new();
        BufReader::new(file).read_line(&mut head).ok()?;
        let line: serde_json::Value = serde_json::from_str(head.trim()).ok()?;
        if line.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            return None;
        }

        let payload = line.get("payload")?;
        let id = payload
            .get("id")
            .or_else(|| payload.get("session_id"))
            .and_then(|id| id.as_str())?
            .to_string();
        let cwd = payload
            .get("cwd")
            .and_then(|cwd| cwd.as_str())
            .map(PathBuf::from);
        // Sources recorded as objects describe subagent and internal threads.
        // Rollouts predating the field are interactive CLI sessions.
        let interactive = match payload.get("source") {
            None => true,
            Some(source) => source
                .as_str()
                .is_some_and(|source| CODEX_INTERACTIVE_SOURCES.contains(&source)),
        };

        Some(CodexSessionMeta {
            id,
            cwd,
            interactive,
        })
    }

    /// List every rollout file under `sessions/`, most recently modified first.
    fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let mut sessions = Vec::new();
        collect_rollouts(&self.sessions_dir(), &mut sessions)?;
        sessions.sort_by_key(|session| std::cmp::Reverse(session.timestamp));
        Ok(sessions)
    }

    /// Return the session as a fork candidate when the rollout is interactive
    /// and was recorded in `worktree_path`, carrying the recorded session ID.
    fn fork_candidate(session: &SessionInfo, worktree_path: &Path) -> Option<SessionInfo> {
        let meta = Self::read_session_meta(&session.path)?;
        if !meta.interactive {
            return None;
        }
        if !meta
            .cwd
            .as_deref()
            .is_some_and(|cwd| paths_match(cwd, worktree_path))
        {
            return None;
        }
        Some(SessionInfo {
            id: meta.id,
            ..session.clone()
        })
    }
}

impl ConversationForker for CodexForker {
    fn find_latest_conversation(&self, worktree_path: &Path) -> Result<Option<SessionInfo>> {
        let sessions = self.list_sessions()?;

        // Codex exports the session it launched a command from, which is a more
        // precise answer than the newest rollout recorded for this directory.
        if let Some(thread_id) = &self.thread_id
            && let Some(session) = sessions.iter().find(|session| &session.id == thread_id)
        {
            return Ok(Some(session.clone()));
        }

        Ok(sessions
            .iter()
            .find_map(|session| Self::fork_candidate(session, worktree_path)))
    }

    fn find_conversation(
        &self,
        worktree_path: &Path,
        session_id: &str,
    ) -> Result<Option<SessionInfo>> {
        let matches: Vec<SessionInfo> = self
            .list_sessions()?
            .iter()
            .filter(|session| session.id.starts_with(session_id))
            .filter_map(|session| Self::fork_candidate(session, worktree_path))
            .collect();

        if let Some(exact) = matches.iter().find(|session| session.id == session_id) {
            return Ok(Some(exact.clone()));
        }
        if matches.len() > 1 {
            let ids: Vec<&str> = matches.iter().map(|session| session.id.as_str()).collect();
            bail!(
                "Multiple conversations match '{}': {}\n\
                 Use a longer session ID prefix.",
                session_id,
                ids.join(", ")
            );
        }
        Ok(matches.into_iter().next())
    }

    fn prepare_fork(&self, session: &SessionInfo, _target_worktree: &Path) -> Result<String> {
        // `codex fork` reads the session from Codex's own store and writes a new
        // rollout, so the parent conversation stays untouched.
        if !session.path.exists() {
            bail!(
                "Codex conversation file no longer exists: {}",
                session.path.display()
            );
        }
        Ok(session.id.clone())
    }

    fn resume_args(&self, session_id: &str, worktree_path: &Path) -> Vec<String> {
        // Without `-C`, Codex asks whether to run the fork in the directory the
        // parent session recorded or the current one, defaulting to the parent.
        // Passing the worktree pins the fork to it and skips the prompt.
        vec![
            "fork".to_string(),
            "-C".to_string(),
            worktree_path.to_string_lossy().to_string(),
            session_id.to_string(),
        ]
    }
}

/// Recursively collect `rollout-*.jsonl` files under `dir`.
fn collect_rollouts(dir: &Path, sessions: &mut Vec<SessionInfo>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, sessions)?;
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(id) = CodexForker::session_id_from_filename(&path) {
            let metadata = entry.metadata()?;
            sessions.push(SessionInfo {
                id,
                path,
                timestamp: metadata.modified()?,
            });
        }
    }
    Ok(())
}

/// Compare paths, resolving symlinks so `/tmp` and `/private/tmp` agree.
fn paths_match(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Resolve a conversation forker for the given agent name.
/// Returns None if the agent doesn't support conversation forking.
pub fn resolve_forker(agent_name: &str) -> Option<Box<dyn ConversationForker>> {
    // Normalize: strip path, take basename
    let basename = agent_name.rsplit('/').next().unwrap_or(agent_name);
    let name = basename
        .split_whitespace()
        .next()
        .unwrap_or(basename)
        .to_lowercase();

    match name.as_str() {
        "claude" => Some(Box::new(ClaudeForker::new())),
        "codex" => Some(Box::new(CodexForker::new())),
        "pi" => Some(Box::new(PiForker::new())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_path() {
        assert_eq!(
            ClaudeForker::encode_path(Path::new("/Users/raine/code/myproject")),
            "-Users-raine-code-myproject"
        );
    }

    #[test]
    fn test_encode_path_worktree() {
        assert_eq!(
            ClaudeForker::encode_path(Path::new("/Users/raine/code/myproject__worktrees/feature")),
            "-Users-raine-code-myproject--worktrees-feature"
        );
    }

    #[test]
    fn test_encode_path_dots_and_underscores() {
        assert_eq!(
            ClaudeForker::encode_path(Path::new("/home/user/.config/my_app")),
            "-home-user--config-my-app"
        );
    }

    #[test]
    fn test_resolve_forker_claude() {
        assert!(resolve_forker("claude").is_some());
        assert!(resolve_forker("Claude").is_some());
        assert!(resolve_forker("/usr/bin/claude --flag").is_some());
    }

    #[test]
    fn test_resolve_forker_codex() {
        assert!(resolve_forker("codex").is_some());
        assert!(resolve_forker("Codex").is_some());
        assert!(resolve_forker("/usr/local/bin/codex --yolo").is_some());
    }

    #[test]
    fn test_resolve_forker_unknown() {
        assert!(resolve_forker("unknown-agent").is_none());
    }

    #[test]
    fn test_list_sessions_ordering() {
        let tmp = tempfile::tempdir().unwrap();
        let forker = ClaudeForker {
            config_dir: tmp.path().to_path_buf(),
        };
        let project_dir = forker.project_dir_for(Path::new("/test/project"));
        fs::create_dir_all(&project_dir).unwrap();

        // Create two session files with a small delay to ensure different mtimes
        let old_file = project_dir.join("old-session.jsonl");
        fs::write(&old_file, "{}").unwrap();

        // Set the old file's mtime to the past
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
        filetime::set_file_mtime(&old_file, filetime::FileTime::from_system_time(old_time))
            .unwrap();

        let new_file = project_dir.join("new-session.jsonl");
        fs::write(&new_file, "{}").unwrap();

        let sessions = forker.list_sessions(&project_dir).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "new-session");
        assert_eq!(sessions[1].id, "old-session");
    }

    #[test]
    fn test_list_sessions_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let forker = ClaudeForker {
            config_dir: tmp.path().to_path_buf(),
        };
        let sessions = forker
            .list_sessions(Path::new("/nonexistent/path"))
            .unwrap();
        assert!(sessions.is_empty());
    }

    // === Pi ===

    fn write_pi_session(dir: &Path, id: &str, cwd: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        let header = serde_json::json!({
            "type": "session", "version": 3, "id": id,
            "timestamp": "2026-01-01T00:00:00.000Z", "cwd": cwd,
        });
        fs::write(&path, format!("{header}\n{{\"type\":\"message\"}}\n")).unwrap();
        path
    }

    fn pi_forker(agent_dir: &Path) -> PiForker {
        PiForker {
            agent_dir: agent_dir.to_path_buf(),
            session_dir: None,
        }
    }

    #[test]
    fn test_resolve_forker_pi() {
        for name in ["pi", "Pi", "/usr/local/bin/pi --flag"] {
            assert!(resolve_forker(name).is_some());
        }
    }

    #[test]
    fn pi_latest_filters_headers_and_orders_by_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let forker = pi_forker(tmp.path());
        let cwd = Path::new("/test/project");
        assert!(forker.find_latest_conversation(cwd).unwrap().is_none());
        let dir = tmp.path().join("sessions/--test-project--");
        let old = write_pi_session(&dir, "old", cwd);
        filetime::set_file_mtime(&old, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        let latest = write_pi_session(&dir, "latest", cwd);
        write_pi_session(&dir, "other-project", Path::new("/other"));
        fs::write(dir.join("invalid.jsonl"), "not json\n").unwrap();
        fs::write(dir.join("empty.jsonl"), "").unwrap();
        fs::write(
            dir.join("wrong-type.jsonl"),
            r#"{"type":"message","id":"bad","cwd":"/test/project"}"#,
        )
        .unwrap();
        fs::write(
            dir.join("missing-id.jsonl"),
            r#"{"type":"session","cwd":"/test/project"}"#,
        )
        .unwrap();
        fs::create_dir(dir.join("directory.jsonl")).unwrap();
        assert_eq!(forker.list_sessions(cwd).unwrap().len(), 2);
        assert_eq!(
            forker.find_latest_conversation(cwd).unwrap().unwrap().path,
            latest
        );
        assert!(
            forker
                .find_conversation(cwd, "other-project")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pi_exact_prefix_ambiguous_and_missing_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let forker = pi_forker(tmp.path());
        let dir = tmp.path().join("sessions/--test--");
        let cwd = Path::new("/test");
        write_pi_session(&dir, "abc", cwd);
        write_pi_session(&dir, "abc-def", cwd);
        assert_eq!(
            forker.find_conversation(cwd, "abc").unwrap().unwrap().id,
            "abc"
        );
        assert_eq!(
            forker.find_conversation(cwd, "abc-d").unwrap().unwrap().id,
            "abc-def"
        );
        assert!(
            forker
                .find_conversation(cwd, "ab")
                .unwrap_err()
                .to_string()
                .contains("longer session ID prefix")
        );
        assert!(forker.find_conversation(cwd, "missing").unwrap().is_none());
    }

    #[test]
    fn pi_custom_session_dir_filters_cwd_and_overrides_default_store() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = Path::new("/test");
        let custom = tmp.path().join("custom sessions");
        let forker = PiForker {
            agent_dir: tmp.path().to_path_buf(),
            session_dir: Some(custom.clone()),
        };
        assert!(forker.find_latest_conversation(cwd).unwrap().is_none());
        write_pi_session(&custom, "custom", cwd);
        write_pi_session(&custom, "other", Path::new("/other"));
        write_pi_session(&tmp.path().join("sessions/default"), "default", cwd);
        let sessions = forker.list_sessions(cwd).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "custom");
    }

    #[test]
    fn pi_prepares_absolute_path_without_copying_or_changing_source() {
        let tmp = tempfile::tempdir().unwrap();
        let forker = pi_forker(tmp.path());
        let cwd = Path::new("/test");
        let path = write_pi_session(&tmp.path().join("sessions/path with 'quotes'"), "id", cwd);
        let original = fs::read(&path).unwrap();
        let session = forker.find_latest_conversation(cwd).unwrap().unwrap();
        let target = tmp.path().join("target");
        let source = forker.prepare_fork(&session, &target).unwrap();
        assert_eq!(Path::new(&source), path.canonicalize().unwrap());
        assert_eq!(
            forker.resume_args(&source, &target),
            vec!["--fork", &source]
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!target.exists());
        fs::remove_file(path).unwrap();
        assert!(forker.prepare_fork(&session, &target).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pi_matches_project_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("project");
        fs::create_dir(&cwd).unwrap();
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&cwd, &alias).unwrap();
        let forker = pi_forker(tmp.path());
        write_pi_session(&tmp.path().join("sessions/alias"), "id", &alias);
        assert!(forker.find_latest_conversation(&cwd).unwrap().is_some());
    }

    // === Codex ===

    /// Write a rollout file mirroring Codex's `sessions/YYYY/MM/DD` layout.
    fn write_codex_rollout(
        codex_home: &Path,
        day: &str,
        session_id: &str,
        cwd: &Path,
        source: serde_json::Value,
        modified_offset_secs: u64,
    ) -> PathBuf {
        let (year, month, day_of_month) = (&day[0..4], &day[5..7], &day[8..10]);
        let dir = codex_home
            .join("sessions")
            .join(year)
            .join(month)
            .join(day_of_month);
        fs::create_dir_all(&dir).unwrap();

        let path = dir.join(format!("rollout-{}T00-00-00-{}.jsonl", day, session_id));
        let meta = serde_json::json!({
            "timestamp": format!("{}T00:00:00.000Z", day),
            "type": "session_meta",
            "payload": {
                "id": session_id,
                "session_id": session_id,
                "timestamp": format!("{}T00:00:00Z", day),
                "cwd": cwd,
                "originator": "codex",
                "cli_version": "0.147.0",
                "source": source,
            },
        });
        fs::write(&path, format!("{}\n", meta)).unwrap();

        let modified =
            std::time::SystemTime::now() - std::time::Duration::from_secs(modified_offset_secs);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(modified)).unwrap();
        path
    }

    fn codex_forker(codex_home: &Path, thread_id: Option<&str>) -> CodexForker {
        CodexForker {
            codex_home: codex_home.to_path_buf(),
            thread_id: thread_id.map(|id| id.to_string()),
        }
    }

    #[test]
    fn test_codex_session_id_from_filename() {
        assert_eq!(
            CodexForker::session_id_from_filename(Path::new(
                "/home/u/.codex/sessions/2026/08/12/rollout-2026-08-12T23-59-14-019ff7c5-a9d3-77b3-8cab-253e05f6f729.jsonl"
            )),
            Some("019ff7c5-a9d3-77b3-8cab-253e05f6f729".to_string())
        );
        assert_eq!(
            CodexForker::session_id_from_filename(Path::new("/tmp/notes.jsonl")),
            None
        );
    }

    #[test]
    fn test_codex_find_latest_conversation_scopes_to_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path().join("codex");
        let worktree = tmp.path().join("project");
        let other = tmp.path().join("other-project");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&other).unwrap();

        write_codex_rollout(
            &codex_home,
            "2026-08-11",
            "11111111-1111-1111-1111-111111111111",
            &worktree,
            serde_json::json!("cli"),
            60,
        );
        // Newer, but recorded for a different working directory.
        write_codex_rollout(
            &codex_home,
            "2026-08-12",
            "22222222-2222-2222-2222-222222222222",
            &other,
            serde_json::json!("cli"),
            10,
        );

        let session = codex_forker(&codex_home, None)
            .find_latest_conversation(&worktree)
            .unwrap()
            .expect("expected a session for the worktree");
        assert_eq!(session.id, "11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn test_codex_find_latest_conversation_skips_non_interactive_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path().join("codex");
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();

        write_codex_rollout(
            &codex_home,
            "2026-08-11",
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            &worktree,
            serde_json::json!("vscode"),
            60,
        );
        write_codex_rollout(
            &codex_home,
            "2026-08-12",
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            &worktree,
            serde_json::json!("exec"),
            30,
        );
        write_codex_rollout(
            &codex_home,
            "2026-08-12",
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
            &worktree,
            serde_json::json!({"subagent": {"depth": 1}}),
            10,
        );

        let session = codex_forker(&codex_home, None)
            .find_latest_conversation(&worktree)
            .unwrap()
            .expect("expected an interactive session");
        assert_eq!(session.id, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    }

    #[test]
    fn test_codex_find_latest_conversation_prefers_current_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path().join("codex");
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();

        write_codex_rollout(
            &codex_home,
            "2026-08-11",
            "dddddddd-dddd-dddd-dddd-dddddddddddd",
            &worktree,
            serde_json::json!("cli"),
            60,
        );
        write_codex_rollout(
            &codex_home,
            "2026-08-12",
            "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee",
            &worktree,
            serde_json::json!("cli"),
            10,
        );

        let session = codex_forker(&codex_home, Some("dddddddd-dddd-dddd-dddd-dddddddddddd"))
            .find_latest_conversation(&worktree)
            .unwrap()
            .expect("expected the session workmux was launched from");
        assert_eq!(session.id, "dddddddd-dddd-dddd-dddd-dddddddddddd");
    }

    #[test]
    fn test_codex_find_latest_conversation_none_for_empty_home() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();

        let session = codex_forker(&tmp.path().join("codex"), None)
            .find_latest_conversation(&worktree)
            .unwrap();
        assert!(session.is_none());
    }

    #[test]
    fn test_codex_find_conversation_by_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path().join("codex");
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();

        write_codex_rollout(
            &codex_home,
            "2026-08-12",
            "019ff7c5-a9d3-77b3-8cab-253e05f6f729",
            &worktree,
            serde_json::json!("cli"),
            10,
        );
        let forker = codex_forker(&codex_home, None);

        let session = forker
            .find_conversation(&worktree, "019ff7c5")
            .unwrap()
            .expect("expected a prefix match");
        assert_eq!(session.id, "019ff7c5-a9d3-77b3-8cab-253e05f6f729");

        assert!(
            forker
                .find_conversation(&worktree, "does-not-exist")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_codex_find_conversation_ambiguous_prefix_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path().join("codex");
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();

        write_codex_rollout(
            &codex_home,
            "2026-08-11",
            "019ff7c5-1111-1111-1111-111111111111",
            &worktree,
            serde_json::json!("cli"),
            60,
        );
        write_codex_rollout(
            &codex_home,
            "2026-08-12",
            "019ff7c5-2222-2222-2222-222222222222",
            &worktree,
            serde_json::json!("cli"),
            10,
        );

        let error = codex_forker(&codex_home, None)
            .find_conversation(&worktree, "019ff7c5")
            .unwrap_err()
            .to_string();
        assert!(error.contains("Multiple conversations match"), "{}", error);
    }

    #[test]
    fn test_codex_find_conversation_prefers_exact_match() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path().join("codex");
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();

        // One session id is a prefix of the other only in the leading segment,
        // so an exact request must not be reported as ambiguous.
        write_codex_rollout(
            &codex_home,
            "2026-08-11",
            "019ff7c5-1111-1111-1111-111111111111",
            &worktree,
            serde_json::json!("cli"),
            60,
        );
        write_codex_rollout(
            &codex_home,
            "2026-08-12",
            "019ff7c5-2222-2222-2222-222222222222",
            &worktree,
            serde_json::json!("cli"),
            10,
        );

        let session = codex_forker(&codex_home, None)
            .find_conversation(&worktree, "019ff7c5-1111-1111-1111-111111111111")
            .unwrap()
            .expect("expected the exact match");
        assert_eq!(session.id, "019ff7c5-1111-1111-1111-111111111111");
    }

    #[test]
    fn test_codex_prepare_fork_leaves_parent_rollout_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path().join("codex");
        let worktree = tmp.path().join("project");
        let target = tmp.path().join("target");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&target).unwrap();

        let rollout = write_codex_rollout(
            &codex_home,
            "2026-08-12",
            "019ff7c5-a9d3-77b3-8cab-253e05f6f729",
            &worktree,
            serde_json::json!("cli"),
            10,
        );
        let original = fs::read_to_string(&rollout).unwrap();
        let forker = codex_forker(&codex_home, None);
        let session = forker.find_latest_conversation(&worktree).unwrap().unwrap();

        let session_id = forker.prepare_fork(&session, &target).unwrap();

        assert_eq!(session_id, "019ff7c5-a9d3-77b3-8cab-253e05f6f729");
        assert_eq!(fs::read_to_string(&rollout).unwrap(), original);
        assert!(!target.join("sessions").exists());
    }

    #[test]
    fn test_codex_resume_args_pin_the_fork_to_the_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            codex_forker(tmp.path(), None).resume_args(
                "019ff7c5-a9d3-77b3-8cab-253e05f6f729",
                Path::new("/code/project__worktrees/feature")
            ),
            vec![
                "fork".to_string(),
                "-C".to_string(),
                "/code/project__worktrees/feature".to_string(),
                "019ff7c5-a9d3-77b3-8cab-253e05f6f729".to_string()
            ]
        );
    }

    #[test]
    fn test_fork_conversation_copies_files() {
        let tmp = tempfile::tempdir().unwrap();
        let forker = ClaudeForker {
            config_dir: tmp.path().to_path_buf(),
        };

        // Create source project dir with a session
        let source_dir = forker.project_dir_for(Path::new("/source/project"));
        fs::create_dir_all(&source_dir).unwrap();
        let session_file = source_dir.join("abc123.jsonl");
        fs::write(&session_file, "{\"test\": true}").unwrap();

        // Create session subdirectory with data
        let session_subdir = source_dir.join("abc123");
        fs::create_dir_all(&session_subdir).unwrap();
        fs::write(session_subdir.join("data.json"), "{}").unwrap();

        let session = SessionInfo {
            id: "abc123".to_string(),
            path: session_file,
            timestamp: std::time::SystemTime::now(),
        };

        let result = forker
            .prepare_fork(&session, Path::new("/target/project"))
            .unwrap();
        assert_eq!(result, "abc123");

        // Verify files were copied
        let target_dir = forker.project_dir_for(Path::new("/target/project"));
        assert!(target_dir.join("abc123.jsonl").exists());
        assert!(target_dir.join("abc123").join("data.json").exists());
    }

    #[test]
    fn test_find_conversation_by_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let forker = ClaudeForker {
            config_dir: tmp.path().to_path_buf(),
        };
        let project_dir = forker.project_dir_for(Path::new("/test/project"));
        fs::create_dir_all(&project_dir).unwrap();

        fs::write(project_dir.join("abc123-def456.jsonl"), "{}").unwrap();

        let session = forker
            .find_conversation(Path::new("/test/project"), "abc123")
            .unwrap();
        assert!(session.is_some());
        assert_eq!(session.unwrap().id, "abc123-def456");
    }
}
