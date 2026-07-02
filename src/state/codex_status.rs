//! Codex-specific workaround for nested hook status updates.
//!
//! Codex currently runs Workmux status hooks for both a parent agent and its
//! spawned subagents in the same tmux pane. A subagent `Stop` hook can therefore
//! run before the parent `Stop` hook and incorrectly mark the pane/window as
//! done while the parent is still active.
//!
//! Codex hook payloads do not currently expose an explicit root/subagent marker
//! such as `agent_path`, `parent_thread_id`, or `is_subagent`. Until they do,
//! Workmux tracks active Codex turns by `session_id` + `turn_id` and renders the
//! pane as working while any tracked Codex turn is active.
//!
//! Keep this module isolated so it can be removed or replaced if Codex adds
//! official hook metadata for subagent/root detection.

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use tracing::warn;

use crate::multiplexer::AgentStatus;
use crate::state::{PaneKey, StateStore};
use crate::util::write_atomic;

#[derive(Debug, Deserialize)]
struct CodexHookProbe {
    hook_event_name: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    transcript_path: Option<String>,
    model: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CodexPaneStatus {
    active: BTreeSet<String>,
}

struct CodexStatusLock {
    _lock: Flock<File>,
}

pub fn detect_run_id_from_stdin() -> Option<String> {
    if io::stdin().is_terminal() {
        return None;
    }

    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() {
        return None;
    }
    if input.trim().is_empty() {
        return None;
    }

    detect_run_id(&input)
}

pub fn apply_status(
    pane_key: &PaneKey,
    run_id: &str,
    requested: AgentStatus,
) -> Result<AgentStatus> {
    let store = StateStore::new()?;
    apply_status_with_store(&store, pane_key, run_id, requested)
}

pub(crate) fn apply_status_with_store(
    store: &StateStore,
    pane_key: &PaneKey,
    run_id: &str,
    requested: AgentStatus,
) -> Result<AgentStatus> {
    let dir = store.codex_status_runtime_dir();
    let _lock = acquire_lock(&dir, pane_key)?;

    let mut status = read_status(&dir, pane_key)?.unwrap_or_default();
    let render_status = status.apply(run_id, requested);

    if status.is_empty() {
        delete_status(&dir, pane_key)?;
    } else {
        write_status(&dir, pane_key, &status)?;
    }

    Ok(render_status)
}

pub fn clear_pane(pane_key: &PaneKey) -> Result<()> {
    let store = StateStore::new()?;
    clear_pane_with_store(&store, pane_key)
}

pub(crate) fn clear_pane_with_store(store: &StateStore, pane_key: &PaneKey) -> Result<()> {
    let dir = store.codex_status_runtime_dir();
    let _lock = acquire_lock(&dir, pane_key)?;
    delete_status(&dir, pane_key)
}

fn detect_run_id(input: &str) -> Option<String> {
    let payload: CodexHookProbe = serde_json::from_str(input).ok()?;

    if payload.agent_id.is_some() || payload.agent_type.is_some() {
        return None;
    }

    let event = payload.hook_event_name.as_deref()?;
    if !matches!(
        event,
        "UserPromptSubmit" | "PreToolUse" | "PermissionRequest" | "PostToolUse" | "Stop"
    ) {
        return None;
    }

    if !has_codex_signal(&payload) {
        return None;
    }

    let session_id = non_empty(payload.session_id.as_deref())?;
    let turn_id = non_empty(payload.turn_id.as_deref())?;

    Some(format!("{session_id}:{turn_id}"))
}

fn has_codex_signal(payload: &CodexHookProbe) -> bool {
    let rollout_transcript = payload
        .transcript_path
        .as_deref()
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"));

    let gpt_model = payload
        .model
        .as_deref()
        .is_some_and(|model| model.starts_with("gpt-"));

    rollout_transcript || gpt_model
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    })
}

impl CodexPaneStatus {
    fn apply(&mut self, run_id: &str, requested: AgentStatus) -> AgentStatus {
        match requested {
            AgentStatus::Working | AgentStatus::Waiting => {
                self.active.insert(run_id.to_string());
                AgentStatus::Working
            }
            AgentStatus::Done => {
                self.active.remove(run_id);
                if self.active.is_empty() {
                    AgentStatus::Done
                } else {
                    AgentStatus::Working
                }
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

fn status_path(dir: &Path, pane_key: &PaneKey) -> PathBuf {
    dir.join(pane_key.to_filename())
}

fn lock_path(dir: &Path, pane_key: &PaneKey) -> PathBuf {
    dir.join(format!("{}.lock", pane_key.to_filename()))
}

fn acquire_lock(dir: &Path, pane_key: &PaneKey) -> Result<CodexStatusLock> {
    fs::create_dir_all(dir)?;
    let path = lock_path(dir, pane_key);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("Failed to open Codex status lock: {}", path.display()))?;
    let lock = Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_file, errno)| errno)
        .with_context(|| format!("Failed to acquire Codex status lock: {}", path.display()))?;
    Ok(CodexStatusLock { _lock: lock })
}

fn read_status(dir: &Path, pane_key: &PaneKey) -> Result<Option<CodexPaneStatus>> {
    let path = status_path(dir, pane_key);
    match fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(status) => Ok(Some(status)),
            Err(error) => {
                warn!(path = %path.display(), error = %error, "corrupted Codex status file, deleting");
                let _ = fs::remove_file(&path);
                Ok(None)
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Failed to read Codex status file"),
    }
}

fn write_status(dir: &Path, pane_key: &PaneKey, status: &CodexPaneStatus) -> Result<()> {
    fs::create_dir_all(dir)?;
    let path = status_path(dir, pane_key);
    let content = serde_json::to_vec_pretty(status)?;
    write_atomic(&path, &content)
}

fn delete_status(dir: &Path, pane_key: &PaneKey) -> Result<()> {
    let path = status_path(dir, pane_key);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Failed to delete Codex status file"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store() -> (StateStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = StateStore::with_path(dir.path().to_path_buf()).unwrap();
        (store, dir)
    }

    fn test_pane_key() -> PaneKey {
        PaneKey {
            backend: "tmux".to_string(),
            instance: "default".to_string(),
            pane_id: "%1".to_string(),
        }
    }

    #[test]
    fn detects_captured_codex_payload() {
        let input = r#"{
            "hook_event_name":"Stop",
            "session_id":"019de386-65c3-79c2-babc-5fc0a2b85dfd",
            "turn_id":"019de386-65c9-7fa2-a25f-1fadd880f63d",
            "transcript_path":"/custom/codex/rollout-2026-05-01T15-32-09.jsonl"
        }"#;

        assert_eq!(
            detect_run_id(input).as_deref(),
            Some("019de386-65c3-79c2-babc-5fc0a2b85dfd:019de386-65c9-7fa2-a25f-1fadd880f63d")
        );
    }

    #[test]
    fn detects_codex_payload_by_gpt_model() {
        let input = r#"{
            "hook_event_name":"UserPromptSubmit",
            "session_id":"session",
            "turn_id":"turn",
            "model":"gpt-5.5"
        }"#;

        assert_eq!(detect_run_id(input).as_deref(), Some("session:turn"));
    }

    #[test]
    fn rejects_payload_without_codex_signal() {
        let input = r#"{
            "hook_event_name":"Stop",
            "session_id":"session",
            "turn_id":"turn"
        }"#;

        assert!(detect_run_id(input).is_none());
    }

    #[test]
    fn rejects_missing_session_id() {
        let input = r#"{
            "hook_event_name":"Stop",
            "turn_id":"turn",
            "model":"gpt-5.5"
        }"#;

        assert!(detect_run_id(input).is_none());
    }

    #[test]
    fn rejects_missing_turn_id() {
        let input = r#"{
            "hook_event_name":"Stop",
            "session_id":"session",
            "model":"gpt-5.5"
        }"#;

        assert!(detect_run_id(input).is_none());
    }

    #[test]
    fn rejects_empty_identifiers() {
        let input = r#"{
            "hook_event_name":"Stop",
            "session_id":"  ",
            "turn_id":"turn",
            "model":"gpt-5.5"
        }"#;

        assert!(detect_run_id(input).is_none());
    }

    #[test]
    fn rejects_non_turn_scoped_event() {
        let input = r#"{
            "hook_event_name":"SessionStart",
            "session_id":"session",
            "turn_id":"turn",
            "model":"gpt-5.5"
        }"#;

        assert!(detect_run_id(input).is_none());
    }

    #[test]
    fn rejects_claude_like_agent_payload() {
        let input = r#"{
            "hook_event_name":"Stop",
            "session_id":"session",
            "turn_id":"turn",
            "model":"gpt-5.5",
            "agent_id":"agent"
        }"#;

        assert!(detect_run_id(input).is_none());
    }

    #[test]
    fn aggregate_keeps_parent_working_after_child_done() {
        let mut pane = CodexPaneStatus::default();
        assert_eq!(
            pane.apply("parent", AgentStatus::Working),
            AgentStatus::Working
        );
        assert_eq!(
            pane.apply("child", AgentStatus::Working),
            AgentStatus::Working
        );
        assert_eq!(pane.apply("child", AgentStatus::Done), AgentStatus::Working);
        assert_eq!(pane.apply("parent", AgentStatus::Done), AgentStatus::Done);
        assert!(pane.is_empty());
    }

    #[test]
    fn done_unknown_run_on_empty_set_renders_done() {
        let mut pane = CodexPaneStatus::default();
        assert_eq!(pane.apply("unknown", AgentStatus::Done), AgentStatus::Done);
        assert!(pane.is_empty());
    }

    #[test]
    fn double_done_does_not_remove_other_runs() {
        let mut pane = CodexPaneStatus::default();
        assert_eq!(
            pane.apply("parent", AgentStatus::Working),
            AgentStatus::Working
        );
        assert_eq!(
            pane.apply("child", AgentStatus::Working),
            AgentStatus::Working
        );
        assert_eq!(pane.apply("child", AgentStatus::Done), AgentStatus::Working);
        assert_eq!(pane.apply("child", AgentStatus::Done), AgentStatus::Working);
        assert!(!pane.is_empty());
    }

    #[test]
    fn scoped_waiting_renders_working() {
        let mut pane = CodexPaneStatus::default();
        assert_eq!(
            pane.apply("run", AgentStatus::Waiting),
            AgentStatus::Working
        );
        assert!(!pane.is_empty());
    }

    #[test]
    fn apply_status_writes_and_removes_runtime_file() {
        let (store, _dir) = test_store();
        let key = test_pane_key();
        let path = status_path(&store.codex_status_runtime_dir(), &key);

        let rendered =
            apply_status_with_store(&store, &key, "parent", AgentStatus::Working).unwrap();
        assert_eq!(rendered, AgentStatus::Working);
        assert!(path.exists());

        let rendered = apply_status_with_store(&store, &key, "parent", AgentStatus::Done).unwrap();
        assert_eq!(rendered, AgentStatus::Done);
        assert!(!path.exists());
    }

    #[test]
    fn apply_status_deletes_corrupt_runtime_file() {
        let (store, _dir) = test_store();
        let key = test_pane_key();
        let dir = store.codex_status_runtime_dir();
        fs::create_dir_all(&dir).unwrap();
        let path = status_path(&dir, &key);
        fs::write(&path, "not json").unwrap();

        let rendered = apply_status_with_store(&store, &key, "run", AgentStatus::Done).unwrap();

        assert_eq!(rendered, AgentStatus::Done);
        assert!(!path.exists());
    }
}
