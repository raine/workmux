//! Filesystem-based state persistence for agent state.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tracing::{info, trace, warn};

use super::types::{AgentState, GlobalSettings, PaneKey};
use crate::config::SandboxRuntime;

/// Manages filesystem-based state persistence for workmux agents.
///
/// Directory structure:
/// ```text
/// $XDG_STATE_HOME/workmux/           # ~/.local/state/workmux/
/// ├── settings.json                   # Global dashboard settings
/// └── agents/
///     ├── tmux__default__%1.json     # {backend}__{instance}__{pane_id}.json
///     └── wezterm__main__3.json
/// ```
pub struct StateStore {
    base_path: PathBuf,
}

impl StateStore {
    /// Create a new StateStore using XDG_STATE_HOME.
    ///
    /// Creates the base directory and agents subdirectory if they don't exist.
    pub fn new() -> Result<Self> {
        let base = get_state_dir()?;
        fs::create_dir_all(&base).context("Failed to create state directory")?;
        fs::create_dir_all(base.join("agents")).context("Failed to create agents directory")?;
        Ok(Self { base_path: base })
    }

    /// Create a StateStore with a custom base path (for testing).
    #[cfg(test)]
    pub fn with_path(base_path: PathBuf) -> Result<Self> {
        fs::create_dir_all(&base_path)?;
        fs::create_dir_all(base_path.join("agents"))?;
        Ok(Self { base_path })
    }

    /// Path to agents directory.
    fn agents_dir(&self) -> PathBuf {
        self.base_path.join("agents")
    }

    /// Path to containers directory.
    fn containers_dir(&self) -> PathBuf {
        self.base_path.join("containers")
    }

    /// Path to runtime directory (for daemon-produced ephemeral state).
    fn runtime_dir(&self) -> PathBuf {
        self.base_path.join("runtime")
    }

    /// Path to Codex status workaround runtime directory.
    pub(crate) fn codex_status_runtime_dir(&self) -> PathBuf {
        self.runtime_dir().join("codex-status")
    }

    /// Path to settings file.
    fn settings_path(&self) -> PathBuf {
        self.base_path.join("settings.json")
    }

    /// Path to a specific agent's state file.
    fn agent_path(&self, key: &PaneKey) -> PathBuf {
        self.agents_dir().join(key.to_filename())
    }

    /// Create or update agent state.
    ///
    /// Uses atomic write (temp file + rename) for crash safety.
    pub fn upsert_agent(&self, state: &AgentState) -> Result<()> {
        let path = self.agent_path(&state.pane_key);
        let content = serde_json::to_string_pretty(state)?;
        super::write_atomic(&path, content.as_bytes())
    }

    /// Read agent state by pane key.
    ///
    /// Returns None if the agent doesn't exist or the file is corrupted.
    #[allow(dead_code)] // Used in tests, may be used in future features
    pub fn get_agent(&self, key: &PaneKey) -> Result<Option<AgentState>> {
        read_agent_file(&self.agent_path(key))
    }

    /// List all agent states.
    ///
    /// Used for reconciliation and dashboard display.
    /// Skips corrupted files (logs warning and deletes them).
    pub fn list_all_agents(&self) -> Result<Vec<AgentState>> {
        let agents_dir = self.agents_dir();
        if !agents_dir.exists() {
            return Ok(Vec::new());
        }

        let mut agents = Vec::new();
        for entry in fs::read_dir(&agents_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json")
                && !path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(".tmp"))
                && let Some(state) = read_agent_file(&path)?
            {
                agents.push(state);
            }
        }
        Ok(agents)
    }

    /// Delete agent state.
    ///
    /// No-op if the file doesn't exist.
    pub fn delete_agent(&self, key: &PaneKey) -> Result<()> {
        let path = self.agent_path(key);
        let agent_result = match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("Failed to delete agent state"),
        };

        if let Err(error) = crate::state::codex_status::clear_pane_with_store(self, key) {
            warn!(error = %error, "failed to clear Codex status state for deleted agent");
        }

        agent_result
    }

    /// Load global settings.
    ///
    /// Returns defaults if the file is missing or corrupted.
    pub fn load_settings(&self) -> Result<GlobalSettings> {
        let path = self.settings_path();
        match fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(settings) => Ok(settings),
                Err(e) => {
                    warn!(?path, error = %e, "corrupted settings file, using defaults");
                    Ok(GlobalSettings::default())
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(GlobalSettings::default()),
            Err(e) => Err(e).context("Failed to read settings"),
        }
    }

    /// Save global settings.
    ///
    /// Uses atomic write for crash safety.
    pub fn save_settings(&self, settings: &GlobalSettings) -> Result<()> {
        let path = self.settings_path();
        let content = serde_json::to_string_pretty(settings)?;
        super::write_atomic(&path, content.as_bytes())
    }

    // ── Container state management ──────────────────────────────────────────

    /// Register a running container for a worktree handle.
    ///
    /// Creates a marker file at `containers/<handle>/<container_name>` with the
    /// runtime's serde name as content for cleanup correctness.
    pub fn register_container(
        &self,
        handle: &str,
        container_name: &str,
        runtime: &SandboxRuntime,
    ) -> Result<()> {
        let dir = self.containers_dir().join(handle);
        fs::create_dir_all(&dir).context("Failed to create container state directory")?;
        fs::write(dir.join(container_name), runtime.serde_name())
            .context("Failed to write container marker")?;
        Ok(())
    }

    /// Unregister a container.
    ///
    /// Removes the marker file and cleans up the directory if empty.
    pub fn unregister_container(&self, handle: &str, container_name: &str) {
        let dir = self.containers_dir().join(handle);
        let path = dir.join(container_name);

        if path.exists() {
            let _ = fs::remove_file(&path);
        }

        // Try to remove the handle directory if empty (ignore errors)
        let _ = fs::remove_dir(&dir);
    }

    /// List registered containers for a worktree handle.
    ///
    /// Returns container names paired with their stored runtime. For backwards
    /// compatibility with empty marker files (pre-runtime-storage), defaults to Docker.
    pub fn list_containers(&self, handle: &str) -> Vec<(String, SandboxRuntime)> {
        let dir = self.containers_dir().join(handle);
        if !dir.exists() {
            return Vec::new();
        }

        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                if name.starts_with('.') {
                    return None;
                }
                let runtime = fs::read_to_string(entry.path())
                    .ok()
                    .and_then(|content| SandboxRuntime::from_serde_name(content.trim()))
                    .unwrap_or_default();
                Some((name, runtime))
            })
            .collect()
    }

    /// Rename the container markers directory from `<old_handle>` to `<new_handle>`.
    ///
    /// No-op if the old directory doesn't exist. Returns an error if the
    /// destination directory already exists (would clobber state).
    pub fn migrate_container_handle(&self, old_handle: &str, new_handle: &str) -> Result<()> {
        if old_handle == new_handle {
            return Ok(());
        }
        let old = self.containers_dir().join(old_handle);
        if !old.exists() {
            return Ok(());
        }
        let new = self.containers_dir().join(new_handle);
        if new.exists() {
            return Err(anyhow::anyhow!(
                "Container state directory already exists: {}",
                new.display()
            ));
        }
        if let Some(parent) = new.parent() {
            fs::create_dir_all(parent)
                .context("Failed to create container state parent directory")?;
        }
        fs::rename(&old, &new).context("Failed to rename container state directory")?;
        Ok(())
    }

    /// Migrate all agent state files whose `workdir` is `old_root` or a
    /// descendant of it, rewriting the path to the corresponding location
    /// under `new_root`. Also rewrites `window_name` / `session_name` that
    /// start with `old_full_base` to use `new_full_base`.
    ///
    /// `old_root_canonical` should be the pre-move canonical path (captured
    /// before `git worktree move` renders the old path non-existent).
    ///
    /// `old_full_base` / `new_full_base` are the prefixed window/session
    /// base names (e.g. "wm-old-handle" / "wm-new-handle"). `-N` duplicate
    /// suffixes on window names are preserved.
    ///
    /// Returns the number of agent state files updated.
    pub fn migrate_worktree_paths(
        &self,
        old_root_canonical: &Path,
        new_root: &Path,
        old_full_base: &str,
        new_full_base: &str,
    ) -> Result<usize> {
        use crate::util::canon_or_self;

        let agents_dir = self.agents_dir();
        if !agents_dir.exists() {
            return Ok(0);
        }

        let mut migrated = 0;

        for entry in fs::read_dir(&agents_dir)? {
            let entry = entry?;
            let path = entry.path();
            let Some(mut state) = read_agent_file(&path)? else {
                continue;
            };

            let stored_canon = canon_or_self(&state.workdir);
            let Ok(relpath) = stored_canon.strip_prefix(old_root_canonical) else {
                continue;
            };

            state.workdir = new_root.join(relpath);
            state.window_name = state
                .window_name
                .map(|n| remap_full_name(&n, old_full_base, new_full_base));
            state.session_name = state
                .session_name
                .map(|n| remap_full_name(&n, old_full_base, new_full_base));

            let content = serde_json::to_string_pretty(&state)?;
            super::write_atomic(&path, content.as_bytes())?;
            migrated += 1;
        }

        Ok(migrated)
    }

    // ── Runtime state management ────────────────────────────────────────────

    /// Write runtime state for a multiplexer instance.
    ///
    /// File path: `runtime/<backend>__<instance>.json`
    pub fn write_runtime(
        &self,
        backend: &str,
        instance: &str,
        state: &super::types::RuntimeState,
    ) -> Result<()> {
        let dir = self.runtime_dir();
        fs::create_dir_all(&dir).context("Failed to create runtime directory")?;
        let safe_instance =
            percent_encoding::utf8_percent_encode(instance, super::types::FILENAME_ENCODE_SET)
                .to_string();
        let path = dir.join(format!("{}__{}.json", backend, safe_instance));
        let content = serde_json::to_string(state)?;
        super::write_atomic(&path, content.as_bytes())
    }

    /// Read runtime state for a multiplexer instance.
    ///
    /// Returns default if missing or corrupted.
    pub fn read_runtime(&self, backend: &str, instance: &str) -> super::types::RuntimeState {
        let safe_instance =
            percent_encoding::utf8_percent_encode(instance, super::types::FILENAME_ENCODE_SET)
                .to_string();
        let path = self
            .runtime_dir()
            .join(format!("{}__{}.json", backend, safe_instance));
        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => super::types::RuntimeState::default(),
        }
    }

    /// Delete runtime state for a multiplexer instance.
    pub fn delete_runtime(&self, backend: &str, instance: &str) {
        let safe_instance =
            percent_encoding::utf8_percent_encode(instance, super::types::FILENAME_ENCODE_SET)
                .to_string();
        let path = self
            .runtime_dir()
            .join(format!("{}__{}.json", backend, safe_instance));
        let _ = fs::remove_file(path);
    }

    /// Load agents with reconciliation against live multiplexer state.
    ///
    /// Uses batched pane queries for performance, with backend-specific fallback validation.
    ///
    /// Returns only valid agents; removes stale state files.
    pub fn load_reconciled_agents(
        &self,
        mux: &dyn crate::multiplexer::Multiplexer,
    ) -> Result<Vec<crate::multiplexer::AgentPane>> {
        let all_agents = self.list_all_agents()?;

        // Fetch all live pane info in a single batched query
        let live_panes = mux.get_all_live_pane_info()?;
        let auto_renamed_tmux_windows = if mux.name() == "tmux" {
            tmux_auto_renamed_windows(&live_panes)
        } else {
            HashSet::new()
        };

        // Get current server boot ID for crash detection
        let current_boot_id = mux.server_boot_id().unwrap_or(None);

        let mut valid_agents = Vec::new();
        let backend = mux.name();
        let instance = mux.instance_id();

        for state in all_agents {
            // Skip agents from other backends/instances
            if state.pane_key.backend != backend || state.pane_key.instance != instance {
                continue;
            }

            // Look up pane in the batched result
            let live_pane = live_panes.get(&state.pane_key.pane_id);

            let pane_id = &state.pane_key.pane_id;
            match live_pane {
                None => {
                    // Pane not in batched result - use backend-specific validation
                    if mux.validate_agent_alive(&state)? {
                        let agent_pane = state.to_agent_pane(
                            state.session_name.clone().unwrap_or_default(),
                            state.window_name.clone().unwrap_or_default(),
                        );
                        valid_agents.push(agent_pane);
                    } else if state.boot_id.is_some() && state.boot_id != current_boot_id {
                        // Server restarted since this state was written. Preserve
                        // the state file for `workmux resurrect` to use.
                        trace!(
                            pane_id,
                            "reconcile: preserving agent from previous server lifecycle for resurrect"
                        );
                    } else {
                        info!(pane_id, "reconcile: removing agent, pane no longer exists");
                        self.delete_agent(&state.pane_key)?;
                        let _ = mux.clear_status(&state.pane_key.pane_id);
                    }
                }
                Some(live) if live.pid.is_some_and(|pid| pid != state.pane_pid) => {
                    if state.boot_id.is_some() && state.boot_id != current_boot_id {
                        // Pane ID recycled after server restart - preserve for resurrect
                        trace!(
                            pane_id,
                            "reconcile: preserving agent from previous server lifecycle for resurrect"
                        );
                    } else {
                        // PID mismatch - pane ID was recycled by a new process
                        info!(
                            pane_id,
                            stored_pid = state.pane_pid,
                            live_pid = live.pid.unwrap_or(0),
                            "reconcile: removing agent, pane PID changed (pane ID recycled)"
                        );
                        self.delete_agent(&state.pane_key)?;
                        let _ = mux.clear_status(&state.pane_key.pane_id);
                    }
                }
                Some(live)
                    if live
                        .current_command
                        .as_ref()
                        .is_some_and(|cmd| *cmd != state.command) =>
                {
                    if state.boot_id.is_some() && state.boot_id != current_boot_id {
                        // Command changed after server restart - preserve for resurrect
                        trace!(
                            pane_id,
                            "reconcile: preserving agent from previous server lifecycle for resurrect"
                        );
                    } else {
                        // Command changed - agent exited (e.g., "node" -> "zsh")
                        info!(
                            pane_id,
                            stored_command = state.command,
                            live_command = live.current_command.as_deref().unwrap_or(""),
                            "reconcile: removing agent, foreground command changed"
                        );
                        self.delete_agent(&state.pane_key)?;
                        let _ = mux.clear_status(&state.pane_key.pane_id);
                    }
                }
                Some(live) => {
                    // Valid - include in dashboard
                    let mut agent_pane = state.to_agent_pane(
                        live.session
                            .clone()
                            .unwrap_or_else(|| state.session_name.clone().unwrap_or_default()),
                        live.window
                            .clone()
                            .unwrap_or_else(|| state.window_name.clone().unwrap_or_default()),
                    );
                    // Prefer live pane title over stored (Claude Code updates title dynamically)
                    if live.title.is_some() {
                        agent_pane.pane_title = live.title.clone();
                    }
                    // Only the tmux backend can reliably distinguish auto-renamed
                    // window names from sticky user-set ones via pane_current_command.
                    if backend == "tmux" {
                        if live
                            .window
                            .as_ref()
                            .is_some_and(|window| auto_renamed_tmux_windows.contains(window))
                        {
                            agent_pane.window_cmd = live.window.clone();
                        } else {
                            agent_pane.window_cmd = live.current_command.clone();
                        }
                    }
                    valid_agents.push(agent_pane);
                }
            }
        }

        Ok(valid_agents)
    }
}

fn tmux_auto_renamed_windows(
    live_panes: &std::collections::HashMap<String, crate::multiplexer::LivePaneInfo>,
) -> HashSet<String> {
    live_panes
        .values()
        .filter_map(|pane| match (&pane.window, &pane.current_command) {
            (Some(window), Some(command)) if window == command => Some(window.clone()),
            _ => None,
        })
        .collect()
}

/// Get the workmux state directory (`$XDG_STATE_HOME/workmux`).
///
/// Delegates to `crate::xdg::state_dir()`.
pub fn get_state_dir() -> Result<PathBuf> {
    crate::xdg::state_dir()
}

/// Rewrite a full window/session name when the handle portion has changed.
///
/// - Exact match of `old_base` -> `new_base`.
/// - `<old_base>-N` (numeric duplicate suffix) -> `<new_base>-N`.
/// - Anything else is returned unchanged.
fn remap_full_name(name: &str, old_base: &str, new_base: &str) -> String {
    if name == old_base {
        return new_base.to_string();
    }
    let dash_prefix = format!("{}-", old_base);
    if let Some(suffix) = name.strip_prefix(&dash_prefix)
        && !suffix.is_empty()
        && suffix.chars().all(|c| c.is_ascii_digit())
    {
        return format!("{}-{}", new_base, suffix);
    }
    name.to_string()
}

/// Read and parse an agent state file.
///
/// Returns None if file doesn't exist.
/// Deletes corrupted files and returns None (recoverable error).
fn read_agent_file(path: &Path) -> Result<Option<AgentState>> {
    match fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(state) => Ok(Some(state)),
            Err(e) => {
                warn!(?path, error = %e, "corrupted state file, deleting");
                let _ = fs::remove_file(path);
                Ok(None)
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("Failed to read agent state"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::{AgentStatus, LivePaneInfo};
    use crate::state::test_support::{
        default_pane_key as test_pane_key, temp_store as test_store, tmux_pane_key,
    };
    use std::collections::HashMap;

    fn test_agent_state(key: PaneKey) -> AgentState {
        AgentState {
            pane_key: key,
            workdir: PathBuf::from("/home/user/project"),
            status: Some(AgentStatus::Working),
            status_ts: Some(1234567890),
            pane_title: Some("Implementing feature X".to_string()),
            pane_pid: 12345,
            command: "node".to_string(),
            updated_ts: 1234567890,
            window_name: Some("wm-test".to_string()),
            session_name: Some("main".to_string()),
            boot_id: None,
            agent_kind: None,
        }
    }

    #[test]
    fn test_upsert_and_get_agent() {
        let (store, _dir) = test_store();
        let key = test_pane_key();
        let state = test_agent_state(key.clone());

        store.upsert_agent(&state).unwrap();

        let retrieved = store.get_agent(&key).unwrap().unwrap();
        assert_eq!(retrieved.pane_key, state.pane_key);
        assert_eq!(retrieved.workdir, state.workdir);
        assert_eq!(retrieved.status, state.status);
        assert_eq!(retrieved.pane_pid, state.pane_pid);
    }

    #[test]
    fn test_get_nonexistent_agent() {
        let (store, _dir) = test_store();
        let key = test_pane_key();

        let result = store.get_agent(&key).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_list_all_agents() {
        let (store, _dir) = test_store();

        let key1 = tmux_pane_key("%1");
        let key2 = tmux_pane_key("%2");

        store.upsert_agent(&test_agent_state(key1)).unwrap();
        store.upsert_agent(&test_agent_state(key2)).unwrap();

        let agents = store.list_all_agents().unwrap();
        assert_eq!(agents.len(), 2);
    }

    #[test]
    fn test_delete_agent() {
        let (store, _dir) = test_store();
        let key = test_pane_key();
        let state = test_agent_state(key.clone());

        store.upsert_agent(&state).unwrap();
        assert!(store.get_agent(&key).unwrap().is_some());

        store.delete_agent(&key).unwrap();
        assert!(store.get_agent(&key).unwrap().is_none());
    }

    #[test]
    fn test_delete_nonexistent_agent() {
        let (store, _dir) = test_store();
        let key = test_pane_key();

        // Should not error
        store.delete_agent(&key).unwrap();
    }

    #[test]
    fn test_atomic_write_creates_no_tmp_files() {
        let (store, dir) = test_store();
        let key = test_pane_key();
        let state = test_agent_state(key);

        store.upsert_agent(&state).unwrap();

        // Check no .tmp files remain
        let agents_dir = dir.path().join("agents");
        for entry in fs::read_dir(&agents_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(!name.ends_with(".tmp"), "temp file should be cleaned up");
        }
    }

    #[test]
    fn test_corrupted_file_deleted() {
        let (store, dir) = test_store();
        let key = test_pane_key();

        // Write corrupted JSON
        let path = dir.path().join("agents").join(key.to_filename());
        fs::write(&path, "not valid json {{{").unwrap();

        // Should return None, not error
        let result = store.get_agent(&key).unwrap();
        assert!(result.is_none());

        // File should be deleted
        assert!(!path.exists());
    }

    #[test]
    fn test_settings_roundtrip() {
        let (store, _dir) = test_store();

        let settings = GlobalSettings {
            sort_mode: "priority".to_string(),
            hide_stale: true,
            preview_size: Some(30),
            last_pane_id: Some("%5".to_string()),
            dashboard_scope: Some("session".to_string()),
            worktree_sort_mode: Some("age".to_string()),
            last_done_cycle: None,
            sidebar_layout: None,
            sidebar_width: None,
            sidebar_height: None,
            sidebar_filter: None,
        };

        store.save_settings(&settings).unwrap();
        let loaded = store.load_settings().unwrap();

        assert_eq!(loaded.sort_mode, settings.sort_mode);
        assert_eq!(loaded.hide_stale, settings.hide_stale);
        assert_eq!(loaded.preview_size, settings.preview_size);
        assert_eq!(loaded.last_pane_id, settings.last_pane_id);
    }

    #[test]
    fn test_settings_without_sidebar_height_preserve_existing_fields() {
        let (store, _dir) = test_store();
        fs::write(
            store.settings_path(),
            r#"{
  "sort_mode": "priority",
  "hide_stale": true,
  "preview_size": 30,
  "last_pane_id": "%5",
  "dashboard_scope": "session",
  "worktree_sort_mode": "age",
  "last_done_cycle": null,
  "sidebar_layout": null,
  "sidebar_width": 42
}"#,
        )
        .unwrap();

        let loaded = store.load_settings().unwrap();

        assert_eq!(loaded.sort_mode, "priority");
        assert_eq!(loaded.sidebar_width, Some(42));
        assert_eq!(loaded.sidebar_height, None);
        assert_eq!(loaded.last_pane_id.as_deref(), Some("%5"));
    }

    #[test]
    fn test_missing_settings_returns_defaults() {
        let (store, _dir) = test_store();

        let settings = store.load_settings().unwrap();
        assert_eq!(settings.sort_mode, "");
        assert!(!settings.hide_stale);
        assert!(settings.preview_size.is_none());
        assert!(settings.last_pane_id.is_none());
    }

    #[test]
    fn test_corrupted_settings_returns_defaults() {
        let (store, dir) = test_store();

        let path = dir.path().join("settings.json");
        fs::write(&path, "not valid json").unwrap();

        let settings = store.load_settings().unwrap();
        assert_eq!(settings.sort_mode, "");
    }

    #[test]
    fn test_list_all_agents_ignores_tmp_files() {
        let (store, dir) = test_store();
        let key = test_pane_key();
        let state = test_agent_state(key);

        store.upsert_agent(&state).unwrap();

        // Create a stray tmp file
        let tmp_path = dir.path().join("agents").join("some_file.json.tmp");
        fs::write(&tmp_path, "{}").unwrap();

        let agents = store.list_all_agents().unwrap();
        assert_eq!(agents.len(), 1);
    }

    #[test]
    fn test_register_container_stores_runtime() {
        let (store, _dir) = test_store();
        store
            .register_container("handle", "container-1", &SandboxRuntime::AppleContainer)
            .unwrap();

        let containers = store.list_containers("handle");
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].0, "container-1");
        assert_eq!(containers[0].1, SandboxRuntime::AppleContainer);
    }

    #[test]
    fn test_register_container_runtime_roundtrip() {
        let (store, _dir) = test_store();

        for runtime in [
            SandboxRuntime::Docker,
            SandboxRuntime::Podman,
            SandboxRuntime::AppleContainer,
        ] {
            let name = format!("container-{}", runtime.binary_name());
            store.register_container("handle", &name, &runtime).unwrap();
        }

        let containers = store.list_containers("handle");
        assert_eq!(containers.len(), 3);

        let by_name: std::collections::HashMap<&str, &SandboxRuntime> =
            containers.iter().map(|(n, r)| (n.as_str(), r)).collect();
        assert_eq!(by_name["container-docker"], &SandboxRuntime::Docker);
        assert_eq!(by_name["container-podman"], &SandboxRuntime::Podman);
        assert_eq!(
            by_name["container-container"],
            &SandboxRuntime::AppleContainer
        );
    }

    #[test]
    fn test_migrate_worktree_paths_rewrites_root_and_subdirs() {
        let (store, _dir) = test_store();

        // Agent at the worktree root
        let root_key = PaneKey {
            backend: "tmux".to_string(),
            instance: "default".to_string(),
            pane_id: "%1".to_string(),
        };
        let mut root_state = test_agent_state(root_key.clone());
        root_state.workdir = PathBuf::from("/repo/wt/old");
        root_state.window_name = Some("wm-old".to_string());
        root_state.session_name = Some("wm-old".to_string());
        store.upsert_agent(&root_state).unwrap();

        // Agent in a subdirectory of the worktree
        let sub_key = PaneKey {
            backend: "tmux".to_string(),
            instance: "default".to_string(),
            pane_id: "%2".to_string(),
        };
        let mut sub_state = test_agent_state(sub_key.clone());
        sub_state.workdir = PathBuf::from("/repo/wt/old/src/nested");
        sub_state.window_name = Some("wm-old-2".to_string()); // duplicate suffix
        sub_state.session_name = Some("wm-old".to_string());
        store.upsert_agent(&sub_state).unwrap();

        // Unrelated agent in a different worktree
        let other_key = PaneKey {
            backend: "tmux".to_string(),
            instance: "default".to_string(),
            pane_id: "%3".to_string(),
        };
        let mut other_state = test_agent_state(other_key.clone());
        other_state.workdir = PathBuf::from("/repo/wt/unrelated");
        other_state.window_name = Some("wm-unrelated".to_string());
        store.upsert_agent(&other_state).unwrap();

        let migrated = store
            .migrate_worktree_paths(
                &PathBuf::from("/repo/wt/old"),
                &PathBuf::from("/repo/wt/new"),
                "wm-old",
                "wm-new",
            )
            .unwrap();
        assert_eq!(migrated, 2);

        let root_after = store.get_agent(&root_key).unwrap().unwrap();
        assert_eq!(root_after.workdir, PathBuf::from("/repo/wt/new"));
        assert_eq!(root_after.window_name.as_deref(), Some("wm-new"));
        assert_eq!(root_after.session_name.as_deref(), Some("wm-new"));

        let sub_after = store.get_agent(&sub_key).unwrap().unwrap();
        assert_eq!(sub_after.workdir, PathBuf::from("/repo/wt/new/src/nested"));
        assert_eq!(sub_after.window_name.as_deref(), Some("wm-new-2"));
        assert_eq!(sub_after.session_name.as_deref(), Some("wm-new"));

        let other_after = store.get_agent(&other_key).unwrap().unwrap();
        assert_eq!(other_after.workdir, PathBuf::from("/repo/wt/unrelated"));
        assert_eq!(other_after.window_name.as_deref(), Some("wm-unrelated"));
    }

    #[test]
    fn test_tmux_auto_renamed_windows_detects_focused_pane_name() {
        let mut live_panes = HashMap::new();
        live_panes.insert(
            "%1".to_string(),
            LivePaneInfo {
                pid: Some(1),
                current_command: Some("node".to_string()),
                working_dir: PathBuf::from("/repo"),
                title: None,
                session: Some("work".to_string()),
                window: Some("node".to_string()),
                session_id: None,
                window_id: None,
            },
        );
        live_panes.insert(
            "%2".to_string(),
            LivePaneInfo {
                pid: Some(2),
                current_command: Some("python".to_string()),
                working_dir: PathBuf::from("/repo"),
                title: None,
                session: Some("work".to_string()),
                window: Some("node".to_string()),
                session_id: None,
                window_id: None,
            },
        );
        live_panes.insert(
            "%3".to_string(),
            LivePaneInfo {
                pid: Some(3),
                current_command: Some("bash".to_string()),
                working_dir: PathBuf::from("/repo"),
                title: None,
                session: Some("work".to_string()),
                window: Some("user-name".to_string()),
                session_id: None,
                window_id: None,
            },
        );

        let auto_renamed = tmux_auto_renamed_windows(&live_panes);
        assert!(auto_renamed.contains("node"));
        assert!(!auto_renamed.contains("user-name"));
    }

    #[test]
    fn test_migrate_container_handle_renames_directory() {
        let (store, _dir) = test_store();
        store
            .register_container("old-handle", "c1", &SandboxRuntime::Docker)
            .unwrap();

        store
            .migrate_container_handle("old-handle", "new-handle")
            .unwrap();

        assert!(store.list_containers("old-handle").is_empty());
        let containers = store.list_containers("new-handle");
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].0, "c1");
    }

    #[test]
    fn test_migrate_container_handle_noop_when_missing() {
        let (store, _dir) = test_store();
        // Should not error out when the old handle has no containers dir
        store
            .migrate_container_handle("nonexistent", "anything")
            .unwrap();
    }

    #[test]
    fn test_list_containers_empty_marker_defaults_to_docker() {
        let (store, dir) = test_store();

        // Simulate old marker file with empty content
        let container_dir = dir.path().join("containers").join("handle");
        fs::create_dir_all(&container_dir).unwrap();
        fs::write(container_dir.join("old-container"), "").unwrap();

        let containers = store.list_containers("handle");
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].0, "old-container");
        assert_eq!(containers[0].1, SandboxRuntime::Docker);
    }
}
