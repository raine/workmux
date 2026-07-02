//! Agent-specific conversation forking for resuming sessions across worktrees.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Information about a conversation session
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Session UUID (stem of the .jsonl file)
    pub id: String,
    /// Full path to the .jsonl file
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

    /// Copy a conversation's files to the target worktree's project directory.
    /// Returns the session UUID for resume args.
    fn fork_conversation(&self, session: &SessionInfo, target_worktree: &Path) -> Result<String>;

    /// CLI args to resume a specific session (e.g., ["--resume", uuid])
    fn resume_args(&self, session_id: &str) -> Vec<String>;
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

    fn fork_conversation(&self, session: &SessionInfo, target_worktree: &Path) -> Result<String> {
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

    fn resume_args(&self, session_id: &str) -> Vec<String> {
        vec!["--resume".to_string(), session_id.to_string()]
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
            .fork_conversation(&session, Path::new("/target/project"))
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
