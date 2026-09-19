//! Filesystem-based state persistence for agent state.

use anyhow::{Context, Result, anyhow};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, trace, warn};

use super::types::{AgentState, GlobalSettings, PaneKey};
use crate::agent_identity::AgentKind;
use crate::config::SandboxRuntime;
use crate::util::{write_atomic, write_atomic_durable};

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

struct AgentStateLock {
    _lock: Flock<File>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRevision {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
}

#[derive(Clone)]
struct CachedAgentFile {
    revision: FileRevision,
    state: Option<AgentState>,
}

/// Persistent cache used by the sidebar's repeated context reads.
#[derive(Default)]
pub(crate) struct AgentStateCache {
    files: HashMap<PathBuf, CachedAgentFile>,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct AgentCacheStats {
    pub listed: usize,
    pub metadata: usize,
    pub reads: usize,
    pub parses: usize,
}

pub(crate) struct CachedAgentLoad {
    pub agents: Vec<AgentState>,
    pub stats: AgentCacheStats,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RecoveryManifest {
    version: u32,
    backend: String,
    instance: String,
    #[serde(default)]
    last_compacted_boot: Option<String>,
    #[serde(default)]
    entries: Vec<RecoveryEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RecoveryEntry {
    state: AgentState,
    #[serde(default = "default_source_count")]
    source_count: usize,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    pending_sources: Vec<RecoverySourceId>,
    #[serde(default)]
    recent_sources: Vec<RecoverySourceId>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct RecoverySourceId {
    pane_key: PaneKey,
    workdir: PathBuf,
    boot_id: Option<String>,
    pane_pid: u32,
    command: String,
    updated_ts: u64,
}

impl From<&AgentState> for RecoverySourceId {
    fn from(state: &AgentState) -> Self {
        Self {
            pane_key: state.pane_key.clone(),
            workdir: state.workdir.clone(),
            boot_id: state.boot_id.clone(),
            pane_pid: state.pane_pid,
            command: state.command.clone(),
            updated_ts: state.updated_ts,
        }
    }
}

fn default_source_count() -> usize {
    1
}

#[derive(Debug, Clone)]
pub(crate) enum AgentStateSource {
    Flat {
        path: PathBuf,
        revision: FileRevision,
        backend: String,
        instance: String,
        source: Box<RecoverySourceId>,
    },
    Recovery {
        backend: String,
        instance: String,
        workdir: PathBuf,
        revision: u64,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ResurrectionAgentState {
    pub state: AgentState,
    pub source: AgentStateSource,
    pub represented_count: usize,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CompactionStats {
    pub scanned: usize,
    pub compacted: usize,
    pub retained_flat: usize,
    pub recovery_entries: usize,
}

/// Agent observation and the state inventory used to produce it.
pub struct ReconciledAgentReport {
    pub backend: String,
    pub instance: String,
    pub state_files_total: usize,
    pub state_files_invalid: usize,
    pub state_files_invalid_unattributed: usize,
    pub state_files_invalid_matching_context: usize,
    pub state_files_matching_context: usize,
    pub agents: Vec<crate::multiplexer::AgentPane>,
}

struct AgentInventory {
    states: Vec<AgentState>,
    total: usize,
    invalid: Vec<Option<PaneKey>>,
}

fn unambiguous_pane_key_from_filename(filename: &str) -> Option<PaneKey> {
    let stem = filename.strip_suffix(".json")?;
    if stem.matches("__").count() != 2 || stem.contains("___") {
        return None;
    }
    let key = PaneKey::from_filename(filename)?;
    (!key.backend.is_empty()
        && !key.instance.is_empty()
        && !key.pane_id.is_empty()
        && key.to_filename() == filename)
        .then_some(key)
}

impl AgentStateLock {
    fn acquire(base_path: &Path) -> Result<Self> {
        let path = base_path.join("agent-state.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("Failed to open agent state lock: {}", path.display()))?;
        let lock = Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_file, errno)| errno)
            .with_context(|| format!("Failed to acquire agent state lock: {}", path.display()))?;
        Ok(Self { _lock: lock })
    }
}

fn file_revision(metadata: &fs::Metadata) -> FileRevision {
    FileRevision {
        dev: metadata.dev(),
        ino: metadata.ino(),
        len: metadata.len(),
        mtime_sec: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
        ctime_sec: metadata.ctime(),
        ctime_nsec: metadata.ctime_nsec(),
    }
}

fn is_agent_json(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "json")
        && !path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".tmp"))
}

fn read_agent_with_revision(path: &Path) -> Result<Option<(AgentState, FileRevision)>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read agent state: {}", path.display()));
        }
    };
    let revision = file_revision(&file.metadata()?);
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    match serde_json::from_str(&content) {
        Ok(state) => Ok(Some((state, revision))),
        Err(error) => {
            warn!(?path, %error, "invalid agent state file");
            Ok(None)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryAgentChoice {
    pub command: String,
    updated_ts: u64,
    source_rank: u8,
}

impl RecoveryAgentChoice {
    pub(crate) fn from_state(state: &AgentState) -> Option<Self> {
        let (command, source_rank) = if let Some(kind) = state.agent_kind.as_deref()
            && AgentKind::from_str(kind).is_some()
        {
            (kind.to_string(), 1)
        } else {
            let profile = crate::multiplexer::agent::resolve_profile(Some(&state.command));
            if profile.name() == "default" {
                return None;
            }
            (profile.name().to_string(), 0)
        };
        Some(Self {
            command,
            updated_ts: state.updated_ts,
            source_rank,
        })
    }

    pub(crate) fn preferred_over(&self, current: &Self) -> bool {
        self.updated_ts > current.updated_ts
            || (self.updated_ts == current.updated_ts
                && (self.source_rank > current.source_rank
                    || (self.source_rank == current.source_rank && self.command < current.command)))
    }
}

fn prefer_recovery_state(candidate: &AgentState, current: &AgentState) -> bool {
    match (
        RecoveryAgentChoice::from_state(candidate),
        RecoveryAgentChoice::from_state(current),
    ) {
        (Some(candidate), Some(current)) => candidate.preferred_over(&current),
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => candidate.updated_ts > current.updated_ts,
    }
}

impl AgentStateCache {
    pub(crate) fn load_context(
        &mut self,
        store: &StateStore,
        backend: &str,
        instance: &str,
    ) -> Result<CachedAgentLoad> {
        let agents_dir = store.agents_dir();
        if !agents_dir.exists() {
            self.files.clear();
            return Ok(CachedAgentLoad {
                agents: Vec::new(),
                stats: AgentCacheStats::default(),
            });
        }

        let mut stats = AgentCacheStats::default();
        let mut seen = HashSet::new();
        let mut agents = Vec::new();
        for entry in fs::read_dir(&agents_dir)? {
            let path = entry?.path();
            if !is_agent_json(&path) {
                continue;
            }
            stats.listed += 1;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if unambiguous_pane_key_from_filename(name)
                .is_some_and(|key| key.backend != backend || key.instance != instance)
            {
                continue;
            }
            seen.insert(path.clone());

            let mut file = match File::open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            stats.metadata += 1;
            let revision = file_revision(&file.metadata()?);
            let cached = self
                .files
                .get(&path)
                .filter(|cached| cached.revision == revision)
                .cloned();
            let cached = match cached {
                Some(cached) => cached,
                None => {
                    let mut content = String::new();
                    file.read_to_string(&mut content)?;
                    stats.reads += 1;
                    stats.parses += 1;
                    let state = match serde_json::from_str(&content) {
                        Ok(state) => Some(state),
                        Err(error) => {
                            warn!(?path, %error, "invalid agent state file");
                            None
                        }
                    };
                    let cached = CachedAgentFile { revision, state };
                    self.files.insert(path.clone(), cached.clone());
                    cached
                }
            };
            if let Some(state) = cached.state
                && state.pane_key.backend == backend
                && state.pane_key.instance == instance
            {
                agents.push(state);
            }
        }
        self.files.retain(|path, _| seen.contains(path));
        Ok(CachedAgentLoad { agents, stats })
    }
}

impl StateStore {
    /// Open a StateStore without creating state directories.
    pub fn open_read_only() -> Result<Self> {
        Ok(Self {
            base_path: get_state_dir()?,
        })
    }

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

    /// Path to settings file.
    fn settings_path(&self) -> PathBuf {
        self.base_path.join("settings.json")
    }

    fn recovery_dir(&self) -> PathBuf {
        self.base_path.join("agent-recovery")
    }

    fn recovery_path(&self, backend: &str, instance: &str) -> PathBuf {
        let safe_backend =
            percent_encoding::utf8_percent_encode(backend, super::types::FILENAME_ENCODE_SET);
        let safe_instance =
            percent_encoding::utf8_percent_encode(instance, super::types::FILENAME_ENCODE_SET);
        self.recovery_dir()
            .join(safe_backend.to_string())
            .join(format!("{safe_instance}.json"))
    }

    pub(crate) fn with_agent_lock<T>(
        &self,
        operation: impl FnOnce(&Self) -> Result<T>,
    ) -> Result<T> {
        let _lock = AgentStateLock::acquire(&self.base_path)?;
        operation(self)
    }

    /// Path to a specific agent's state file.
    fn agent_path(&self, key: &PaneKey) -> PathBuf {
        self.agents_dir().join(key.to_filename())
    }

    /// Create or update agent state.
    ///
    /// Uses atomic write (temp file + rename) for crash safety.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn upsert_agent(&self, state: &AgentState) -> Result<()> {
        self.with_agent_lock(|store| store.upsert_agent_locked(state))
    }

    pub(crate) fn upsert_agent_locked(&self, state: &AgentState) -> Result<()> {
        let path = self.agent_path(&state.pane_key);
        let historical = read_agent_file(&path)?.filter(|existing| {
            existing
                .boot_id
                .as_deref()
                .zip(state.boot_id.as_deref())
                .is_some_and(|(stored, current)| {
                    stored != current
                        && !(same_server_boot(&state.pane_key.backend, stored, current)
                            && existing.pane_pid == state.pane_pid)
                })
        });
        if let Some(existing) = historical.as_ref() {
            self.merge_recovery_locked(
                &state.pane_key.backend,
                &state.pane_key.instance,
                std::slice::from_ref(existing),
            )?;
        }
        let content = serde_json::to_string_pretty(state)?;
        write_atomic(&path, content.as_bytes())?;
        if historical.is_some() {
            self.finalize_recovery_locked(&state.pane_key.backend, &state.pane_key.instance, None)?;
        }
        Ok(())
    }

    /// Read agent state by pane key.
    ///
    /// Returns None if the agent doesn't exist or the file is corrupted.
    #[allow(dead_code)] // Used in tests, may be used in future features
    pub fn get_agent(&self, key: &PaneKey) -> Result<Option<AgentState>> {
        read_agent_file(&self.agent_path(key))
    }

    pub(crate) fn get_agent_with_revision(
        &self,
        key: &PaneKey,
    ) -> Result<Option<(AgentState, FileRevision)>> {
        read_agent_with_revision(&self.agent_path(key))
    }

    pub(crate) fn upsert_agent_if_revision(
        &self,
        state: &AgentState,
        expected: &FileRevision,
    ) -> Result<bool> {
        self.with_agent_lock(|store| {
            let path = store.agent_path(&state.pane_key);
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if file_revision(&metadata) != *expected {
                return Ok(false);
            }
            let content = serde_json::to_string_pretty(state)?;
            write_atomic(&path, content.as_bytes())?;
            Ok(true)
        })
    }

    /// List all agent states.
    ///
    /// Used for reconciliation and dashboard display.
    /// Skips invalid files and preserves them for diagnosis.
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

    fn inspect_agent_inventory(&self) -> Result<AgentInventory> {
        let agents_dir = self.agents_dir();
        if !agents_dir.exists() {
            return Ok(AgentInventory {
                states: Vec::new(),
                total: 0,
                invalid: Vec::new(),
            });
        }

        let mut states = Vec::new();
        let mut total = 0;
        let mut invalid = Vec::new();
        for entry in fs::read_dir(&agents_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json")
                || path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(".tmp"))
            {
                continue;
            }

            let content = match fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Failed to read agent state: {}", path.display())
                    });
                }
            };
            total += 1;
            match serde_json::from_str(&content) {
                Ok(state) => states.push(state),
                Err(error) => {
                    warn!(?path, %error, "invalid agent state file");
                    invalid.push(
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .and_then(unambiguous_pane_key_from_filename),
                    );
                }
            }
        }

        Ok(AgentInventory {
            states,
            total,
            invalid,
        })
    }

    /// Clear the stored activity status of one agent without deleting it.
    ///
    /// Resets only the status fields so the pane identity and metadata survive
    /// an explicit clear. Returns whether a record existed for the key.
    pub fn clear_agent_status(&self, key: &PaneKey) -> Result<bool> {
        self.with_agent_lock(|store| store.clear_agent_status_locked(key))
    }

    fn clear_agent_status_locked(&self, key: &PaneKey) -> Result<bool> {
        let Some(mut state) = self.get_agent(key)? else {
            return Ok(false);
        };
        state.status = None;
        state.status_ts = None;
        state.updated_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        self.upsert_agent_locked(&state)?;
        Ok(true)
    }

    /// Delete agent state.
    ///
    /// No-op if the file doesn't exist.
    pub fn delete_agent(&self, key: &PaneKey) -> Result<()> {
        self.with_agent_lock(|store| store.delete_agent_locked(key))
    }

    fn delete_agent_locked(&self, key: &PaneKey) -> Result<()> {
        let path = self.agent_path(key);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("Failed to delete agent state"),
        }
    }

    fn load_recovery_manifest_locked(
        &self,
        backend: &str,
        instance: &str,
    ) -> Result<RecoveryManifest> {
        let path = self.recovery_path(backend, instance);
        match self.load_recovery_manifest_path_locked(&path) {
            Ok(manifest) => Ok(manifest),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                Ok(RecoveryManifest {
                    version: 1,
                    backend: backend.to_string(),
                    instance: instance.to_string(),
                    last_compacted_boot: None,
                    entries: Vec::new(),
                })
            }
            Err(error) => Err(error),
        }
    }

    fn load_recovery_manifest_path_locked(&self, path: &Path) -> Result<RecoveryManifest> {
        let content = fs::read_to_string(path).map_err(anyhow::Error::from)?;
        let manifest: RecoveryManifest = serde_json::from_str(&content)
            .with_context(|| format!("Invalid recovery state: {}", path.display()))?;
        if manifest.version != 1
            || self.recovery_path(&manifest.backend, &manifest.instance) != path
        {
            return Err(anyhow!(
                "Recovery state identity mismatch: {}",
                path.display()
            ));
        }
        Ok(manifest)
    }

    fn save_recovery_manifest_locked(&self, manifest: &RecoveryManifest) -> Result<()> {
        let path = self.recovery_path(&manifest.backend, &manifest.instance);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("recovery path has no parent"))?;
        fs::create_dir_all(parent)?;
        let content = serde_json::to_vec_pretty(manifest)?;
        write_atomic_durable(&path, &content)
    }

    fn merge_recovery_locked(
        &self,
        backend: &str,
        instance: &str,
        states: &[AgentState],
    ) -> Result<usize> {
        if states.is_empty() {
            return Ok(self
                .load_recovery_manifest_locked(backend, instance)?
                .entries
                .len());
        }
        let mut manifest = self.load_recovery_manifest_locked(backend, instance)?;
        for state in states {
            let source = RecoverySourceId::from(state);
            if let Some(entry) = manifest
                .entries
                .iter_mut()
                .find(|entry| entry.state.workdir == state.workdir)
            {
                if entry.pending_sources.contains(&source) || entry.recent_sources.contains(&source)
                {
                    continue;
                }
                entry.source_count = entry.source_count.saturating_add(1);
                entry.revision = entry.revision.saturating_add(1);
                entry.pending_sources.push(source);
                if prefer_recovery_state(state, &entry.state) {
                    entry.state = state.clone();
                }
            } else {
                manifest.entries.push(RecoveryEntry {
                    state: state.clone(),
                    source_count: 1,
                    revision: 1,
                    pending_sources: vec![source],
                    recent_sources: Vec::new(),
                });
            }
        }
        self.save_recovery_manifest_locked(&manifest)?;
        Ok(manifest.entries.len())
    }

    fn finalize_recovery_locked(
        &self,
        backend: &str,
        instance: &str,
        compacted_boot: Option<&str>,
    ) -> Result<usize> {
        let mut manifest = self.load_recovery_manifest_locked(backend, instance)?;
        for entry in &mut manifest.entries {
            if !entry.pending_sources.is_empty() {
                entry.recent_sources = std::mem::take(&mut entry.pending_sources);
            }
        }
        if let Some(boot) = compacted_boot {
            manifest.last_compacted_boot = Some(boot.to_string());
        }
        let entries = manifest.entries.len();
        self.save_recovery_manifest_locked(&manifest)?;
        Ok(entries)
    }

    /// Compact known historical generations for one authoritatively observed context.
    pub(crate) fn compact_context(
        &self,
        backend: &str,
        instance: &str,
        current_boot_id: Option<&str>,
    ) -> Result<CompactionStats> {
        let Some(current_boot_id) = current_boot_id else {
            return Ok(CompactionStats::default());
        };
        self.with_agent_lock(|store| {
            store.compact_context_locked(backend, instance, current_boot_id)
        })
    }

    pub(crate) fn compact_context_locked(
        &self,
        backend: &str,
        instance: &str,
        current_boot_id: &str,
    ) -> Result<CompactionStats> {
        let existing_manifest = self.load_recovery_manifest_locked(backend, instance)?;
        if existing_manifest.last_compacted_boot.as_deref() == Some(current_boot_id) {
            return Ok(CompactionStats {
                recovery_entries: existing_manifest.entries.len(),
                ..CompactionStats::default()
            });
        }

        let mut historical = Vec::new();
        let mut retained_flat = 0;
        for entry in fs::read_dir(self.agents_dir())? {
            let path = entry?.path();
            if !is_agent_json(&path) {
                continue;
            }
            let Some((state, revision)) = read_agent_with_revision(&path)? else {
                continue;
            };
            if state.pane_key.backend != backend || state.pane_key.instance != instance {
                continue;
            }
            if state
                .boot_id
                .as_deref()
                .is_some_and(|boot| !same_server_boot(backend, boot, current_boot_id))
            {
                historical.push((path, revision, state));
            } else {
                retained_flat += 1;
            }
        }

        let states: Vec<_> = historical
            .iter()
            .map(|(_, _, state)| state.clone())
            .collect();
        self.merge_recovery_locked(backend, instance, &states)?;
        let mut compacted = 0;
        for (path, revision, _) in &historical {
            if self.delete_path_if_revision_locked(path, revision)? {
                compacted += 1;
            }
        }
        let recovery_entries = if compacted == historical.len() {
            self.finalize_recovery_locked(backend, instance, Some(current_boot_id))?
        } else {
            self.load_recovery_manifest_locked(backend, instance)?
                .entries
                .len()
        };
        Ok(CompactionStats {
            scanned: historical.len() + retained_flat,
            compacted,
            retained_flat,
            recovery_entries,
        })
    }

    fn delete_path_if_revision_locked(&self, path: &Path, expected: &FileRevision) -> Result<bool> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if file_revision(&metadata) != *expected {
            return Ok(false);
        }
        fs::remove_file(path)?;
        Ok(true)
    }

    pub(crate) fn resurrection_snapshot(
        &self,
        backend: &str,
        instance: &str,
    ) -> Result<Vec<ResurrectionAgentState>> {
        self.with_agent_lock(|store| {
            let mut result = Vec::new();
            let agents_dir = store.agents_dir();
            if agents_dir.exists() {
                for entry in fs::read_dir(&agents_dir)? {
                    let path = entry?.path();
                    if !is_agent_json(&path) {
                        continue;
                    }
                    let Some((state, revision)) = read_agent_with_revision(&path)? else {
                        continue;
                    };
                    if state.pane_key.backend == backend && state.pane_key.instance == instance {
                        result.push(ResurrectionAgentState {
                            source: AgentStateSource::Flat {
                                path,
                                revision,
                                backend: backend.to_string(),
                                instance: instance.to_string(),
                                source: Box::new(RecoverySourceId::from(&state)),
                            },
                            represented_count: 1,
                            state,
                        });
                    }
                }
            }
            let manifest = store.load_recovery_manifest_locked(backend, instance)?;
            result.extend(
                manifest
                    .entries
                    .into_iter()
                    .map(|entry| ResurrectionAgentState {
                        source: AgentStateSource::Recovery {
                            backend: backend.to_string(),
                            instance: instance.to_string(),
                            workdir: entry.state.workdir.clone(),
                            revision: entry.revision,
                        },
                        represented_count: entry.source_count,
                        state: entry.state,
                    }),
            );
            Ok(result)
        })
    }

    pub(crate) fn consume_agent_sources(&self, sources: &[AgentStateSource]) -> Result<()> {
        self.with_agent_lock(|store| {
            let mut manifests: HashMap<(String, String), RecoveryManifest> = HashMap::new();
            for source in sources {
                match source {
                    AgentStateSource::Flat {
                        path,
                        revision,
                        backend,
                        instance,
                        source,
                    } => {
                        if !store.delete_path_if_revision_locked(path, revision)? {
                            let key = (backend.clone(), instance.clone());
                            if !manifests.contains_key(&key) {
                                manifests.insert(
                                    key.clone(),
                                    store.load_recovery_manifest_locked(backend, instance)?,
                                );
                            }
                            manifests.get_mut(&key).unwrap().entries.retain(|entry| {
                                // A workdir entry can include history outside this plan.
                                // Recent source membership alone does not authorize deletion.
                                let fully_planned = entry.pending_sources.is_empty()
                                    && entry.source_count == entry.recent_sources.len()
                                    && entry.recent_sources.iter().all(|recent| {
                                        sources.iter().any(|planned| {
                                            matches!(planned,
                                                    AgentStateSource::Flat {
                                                        backend: planned_backend,
                                                        instance: planned_instance,
                                                        source: planned_source,
                                                        ..
                                                    } if planned_backend == backend
                                                        && planned_instance == instance
                                                        && planned_source.as_ref() == recent)
                                        })
                                    });
                                !(fully_planned && entry.recent_sources.contains(source.as_ref()))
                            });
                        }
                    }
                    AgentStateSource::Recovery {
                        backend,
                        instance,
                        workdir,
                        revision,
                    } => {
                        let key = (backend.clone(), instance.clone());
                        if !manifests.contains_key(&key) {
                            manifests.insert(
                                key.clone(),
                                store.load_recovery_manifest_locked(backend, instance)?,
                            );
                        }
                        manifests.get_mut(&key).unwrap().entries.retain(|entry| {
                            let matches =
                                entry.state.workdir == *workdir && entry.revision == *revision;
                            trace!(
                                entry_workdir = %entry.state.workdir.display(),
                                planned_workdir = %workdir.display(),
                                entry_revision = entry.revision,
                                planned_revision = *revision,
                                matches,
                                "resurrect:consume recovery state"
                            );
                            !matches
                        });
                    }
                }
            }
            for manifest in manifests.values_mut() {
                store.save_recovery_manifest_locked(manifest)?;
            }
            Ok(())
        })
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
        write_atomic(&path, content.as_bytes())
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
        self.with_agent_lock(|store| {
            store.migrate_worktree_paths_locked(
                old_root_canonical,
                new_root,
                old_full_base,
                new_full_base,
            )
        })
    }

    fn migrate_worktree_paths_locked(
        &self,
        old_root_canonical: &Path,
        new_root: &Path,
        old_full_base: &str,
        new_full_base: &str,
    ) -> Result<usize> {
        use crate::util::canon_or_self;

        let mut migrated = 0;
        let agents_dir = self.agents_dir();
        if agents_dir.exists() {
            for entry in fs::read_dir(&agents_dir)? {
                let path = entry?.path();
                let Some(mut state) = read_agent_file(&path)? else {
                    continue;
                };
                let stored_canon = canon_or_self(&state.workdir);
                let Ok(relpath) = stored_canon.strip_prefix(old_root_canonical) else {
                    continue;
                };
                remap_agent_state(
                    &mut state,
                    new_root.join(relpath),
                    old_full_base,
                    new_full_base,
                );
                let content = serde_json::to_string_pretty(&state)?;
                write_atomic(&path, content.as_bytes())?;
                migrated += 1;
            }
        }

        let recovery_dir = self.recovery_dir();
        if recovery_dir.exists() {
            for backend_dir in fs::read_dir(&recovery_dir)? {
                let backend_dir = backend_dir?.path();
                if !backend_dir.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(backend_dir)? {
                    let path = entry?.path();
                    if path.extension().is_none_or(|extension| extension != "json") {
                        continue;
                    }
                    let mut manifest = match self.load_recovery_manifest_path_locked(&path) {
                        Ok(manifest) => manifest,
                        Err(error) => {
                            warn!(?path, %error, "invalid recovery state file");
                            continue;
                        }
                    };
                    let mut changed = false;
                    for entry in &mut manifest.entries {
                        let stored_canon = canon_or_self(&entry.state.workdir);
                        let Ok(relpath) = stored_canon.strip_prefix(old_root_canonical) else {
                            continue;
                        };
                        remap_agent_state(
                            &mut entry.state,
                            new_root.join(relpath),
                            old_full_base,
                            new_full_base,
                        );
                        for source in entry
                            .pending_sources
                            .iter_mut()
                            .chain(&mut entry.recent_sources)
                        {
                            let source_canon = canon_or_self(&source.workdir);
                            if let Ok(source_relpath) =
                                source_canon.strip_prefix(old_root_canonical)
                            {
                                source.workdir = new_root.join(source_relpath);
                            }
                        }
                        entry.revision = entry.revision.saturating_add(1);
                        migrated += 1;
                        changed = true;
                    }
                    if changed {
                        self.save_recovery_manifest_locked(&manifest)?;
                    }
                }
            }
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
        write_atomic(&path, content.as_bytes())
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
        let instance = mux.instance_id();
        self.reconcile_agents_for_context(all_agents, mux, &instance, false, true)
    }

    /// Observe reconciled agents together with the complete state-file census.
    pub fn load_reconciled_agent_report(
        &self,
        mux: &dyn crate::multiplexer::Multiplexer,
    ) -> Result<ReconciledAgentReport> {
        let inventory = self.inspect_agent_inventory()?;
        let backend = mux.name();
        let instance = mux.resolve_instance_id()?;
        let state_files_matching_context = inventory
            .states
            .iter()
            .filter(|state| {
                state.pane_key.backend == backend && state.pane_key.instance == instance
            })
            .count();
        let state_files_invalid = inventory.invalid.len();
        let state_files_invalid_unattributed =
            inventory.invalid.iter().filter(|key| key.is_none()).count();
        let state_files_invalid_matching_context = inventory
            .invalid
            .iter()
            .filter_map(Option::as_ref)
            .filter(|key| key.backend == backend && key.instance == instance)
            .count();
        let agents =
            self.reconcile_agents_for_context(inventory.states, mux, &instance, true, false)?;

        Ok(ReconciledAgentReport {
            backend: backend.to_string(),
            instance,
            state_files_total: inventory.total,
            state_files_invalid,
            state_files_invalid_unattributed,
            state_files_invalid_matching_context,
            state_files_matching_context,
            agents,
        })
    }

    fn reconcile_agents_for_context(
        &self,
        all_agents: Vec<AgentState>,
        mux: &dyn crate::multiplexer::Multiplexer,
        instance: &str,
        strict_server_identity: bool,
        prune_stale: bool,
    ) -> Result<Vec<crate::multiplexer::AgentPane>> {
        let live_panes = if strict_server_identity {
            mux.get_all_live_pane_info_strict()?
        } else {
            mux.get_all_live_pane_info()?
        };
        let current_boot_id = if strict_server_identity {
            mux.server_boot_id()?
        } else {
            mux.server_boot_id().unwrap_or(None)
        };
        self.reconcile_agents_from_snapshot(
            all_agents,
            mux,
            instance,
            &live_panes,
            current_boot_id.as_deref(),
            prune_stale,
        )
    }

    pub(crate) fn load_reconciled_agents_from_snapshot_cached(
        &self,
        cache: &mut AgentStateCache,
        mux: &dyn crate::multiplexer::Multiplexer,
        live_panes: &HashMap<String, crate::multiplexer::LivePaneInfo>,
        current_boot_id: Option<&str>,
    ) -> Result<(Vec<crate::multiplexer::AgentPane>, AgentCacheStats)> {
        let instance = mux.instance_id();
        let load = cache.load_context(self, mux.name(), &instance)?;
        let agents = self.reconcile_agents_from_snapshot(
            load.agents,
            mux,
            &instance,
            live_panes,
            current_boot_id,
            true,
        )?;
        Ok((agents, load.stats))
    }

    fn reconcile_agents_from_snapshot(
        &self,
        all_agents: Vec<AgentState>,
        mux: &dyn crate::multiplexer::Multiplexer,
        instance: &str,
        live_panes: &HashMap<String, crate::multiplexer::LivePaneInfo>,
        current_boot_id: Option<&str>,
        prune_stale: bool,
    ) -> Result<Vec<crate::multiplexer::AgentPane>> {
        let backend = mux.name();
        let auto_renamed_tmux_windows = if backend == "tmux" {
            tmux_auto_renamed_windows(live_panes)
        } else {
            HashSet::new()
        };

        let mut valid_agents = Vec::new();

        for state in all_agents {
            // Skip agents from other backends/instances
            if state.pane_key.backend != backend || state.pane_key.instance != instance {
                continue;
            }

            // Look up pane in the batched result
            let live_pane = live_panes.get(&state.pane_key.pane_id);

            let pane_id = &state.pane_key.pane_id;
            let previous_server =
                server_lifecycle_changed(backend, state.boot_id.as_deref(), current_boot_id);
            match live_pane {
                Some(_) if previous_server => {
                    trace!(
                        pane_id,
                        "reconcile: excluding agent from previous server lifecycle"
                    );
                }
                None if previous_server => {
                    trace!(
                        pane_id,
                        "reconcile: preserving agent from previous server lifecycle for resurrect"
                    );
                }
                None => {
                    // Pane not in batched result - use backend-specific validation
                    if mux.validate_agent_alive(&state)? {
                        let agent_pane = state.to_agent_pane(
                            state.session_name.clone().unwrap_or_default(),
                            state.window_name.clone().unwrap_or_default(),
                        );
                        valid_agents.push(agent_pane);
                    } else {
                        info!(pane_id, "reconcile: agent pane no longer exists");
                        if prune_stale {
                            self.delete_agent(&state.pane_key)?;
                            let _ = mux.clear_status(&state.pane_key.pane_id);
                        }
                    }
                }
                Some(live) if live.pid.is_some_and(|pid| pid != state.pane_pid) => {
                    if previous_server {
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
                            "reconcile: pane PID changed (pane ID recycled)"
                        );
                        if prune_stale {
                            self.delete_agent(&state.pane_key)?;
                            let _ = mux.clear_status(&state.pane_key.pane_id);
                        }
                    }
                }
                Some(live)
                    if live
                        .current_command
                        .as_ref()
                        .is_some_and(|cmd| *cmd != state.command) =>
                {
                    if previous_server {
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
                            "reconcile: foreground command changed"
                        );
                        if prune_stale {
                            self.delete_agent(&state.pane_key)?;
                            let _ = mux.clear_status(&state.pane_key.pane_id);
                        }
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

/// Legacy tmux IDs identify a possible matching server by start time alone.
/// Callers merging pane state must also verify the pane PID.
pub(super) fn same_server_boot(backend: &str, stored: &str, current: &str) -> bool {
    stored == current
        || (backend == "tmux"
            && !stored.is_empty()
            && stored.bytes().all(|byte| byte.is_ascii_digit())
            && current.split_once(':').is_some_and(|(start, pid)| {
                start == stored && !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit())
            }))
}

fn server_lifecycle_changed(backend: &str, stored: Option<&str>, current: Option<&str>) -> bool {
    stored.is_some_and(|stored| {
        current.is_none_or(|current| !same_server_boot(backend, stored, current))
    })
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
fn remap_agent_state(
    state: &mut AgentState,
    workdir: PathBuf,
    old_full_base: &str,
    new_full_base: &str,
) {
    state.workdir = workdir;
    state.window_name = state
        .window_name
        .take()
        .map(|name| remap_full_name(&name, old_full_base, new_full_base));
    state.session_name = state
        .session_name
        .take()
        .map(|name| remap_full_name(&name, old_full_base, new_full_base));
}

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
/// Returns None if the file doesn't exist or cannot be decoded.
fn read_agent_file(path: &Path) -> Result<Option<AgentState>> {
    match fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(state) => Ok(Some(state)),
            Err(e) => {
                warn!(?path, error = %e, "invalid agent state file");
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
            activity_ts: Some(1234567890),
            pane_title: Some("Implementing feature X".to_string()),
            pane_pid: 12345,
            command: "node".to_string(),
            updated_ts: 1234567890,
            window_name: Some("wm-test".to_string()),
            session_name: Some("main".to_string()),
            boot_id: None,
            agent_kind: None,
            agent_session_id: None,
        }
    }

    #[test]
    fn test_previous_server_lifecycle_requires_stored_boot_id() {
        assert!(server_lifecycle_changed(
            "tmux",
            Some("old"),
            Some("current")
        ));
        assert!(server_lifecycle_changed("tmux", Some("old"), None));
        assert!(!server_lifecycle_changed(
            "tmux",
            Some("current"),
            Some("current")
        ));
        assert!(!server_lifecycle_changed("tmux", None, Some("current")));
        assert!(!server_lifecycle_changed("tmux", None, None));
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
        assert_eq!(retrieved.activity_ts, state.activity_ts);
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
    fn clear_agent_status_resets_status_and_preserves_identity() {
        let (store, _dir) = test_store();
        let key = tmux_pane_key("%1");
        let mut state = test_agent_state(key.clone());
        state.agent_kind = Some("claude".to_string());
        state.agent_session_id = Some("session-1".to_string());
        store.upsert_agent(&state).unwrap();

        assert!(store.clear_agent_status(&key).unwrap());

        let cleared = store.get_agent(&key).unwrap().unwrap();
        assert_eq!(cleared.status, None);
        assert_eq!(cleared.status_ts, None);
        assert_eq!(cleared.pane_key, state.pane_key);
        assert_eq!(cleared.workdir, state.workdir);
        assert_eq!(cleared.pane_title, state.pane_title);
        assert_eq!(cleared.pane_pid, state.pane_pid);
        assert_eq!(cleared.command, state.command);
        assert_eq!(cleared.window_name, state.window_name);
        assert_eq!(cleared.session_name, state.session_name);
        assert_eq!(cleared.agent_kind, state.agent_kind);
        assert_eq!(cleared.agent_session_id, state.agent_session_id);
        assert_eq!(cleared.activity_ts, state.activity_ts);
    }

    #[test]
    fn clear_agent_status_leaves_other_panes_and_instances_untouched() {
        let (store, _dir) = test_store();
        let target = tmux_pane_key("%1");
        let other_pane = tmux_pane_key("%2");
        let other_instance = PaneKey {
            backend: "zellij".to_string(),
            instance: "other-session".to_string(),
            pane_id: "%1".to_string(),
        };
        store
            .upsert_agent(&test_agent_state(target.clone()))
            .unwrap();
        store
            .upsert_agent(&test_agent_state(other_pane.clone()))
            .unwrap();
        store
            .upsert_agent(&test_agent_state(other_instance.clone()))
            .unwrap();

        assert!(store.clear_agent_status(&target).unwrap());

        assert_eq!(store.get_agent(&target).unwrap().unwrap().status, None);
        assert_eq!(
            store.get_agent(&other_pane).unwrap().unwrap().status,
            Some(AgentStatus::Working)
        );
        assert_eq!(
            store.get_agent(&other_instance).unwrap().unwrap().status,
            Some(AgentStatus::Working)
        );
    }

    #[test]
    fn clear_agent_status_missing_record_is_a_noop() {
        let (store, _dir) = test_store();
        let key = test_pane_key();

        assert!(!store.clear_agent_status(&key).unwrap());
        assert!(store.get_agent(&key).unwrap().is_none());
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
            assert!(
                !name.contains(".tmp"),
                "temp file should be cleaned up: {name}"
            );
        }
    }

    #[test]
    fn invalid_agent_file_is_preserved() {
        let (store, dir) = test_store();
        let key = test_pane_key();
        let path = dir.path().join("agents").join(key.to_filename());
        fs::write(&path, "not valid json {{{").unwrap();

        let result = store.get_agent(&key).unwrap();

        assert!(result.is_none());
        assert!(path.exists());
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
            sidebar_group_by: None,
            sidebar_hint_version: None,
            sidebar_hint_since: None,
            sidebar_hint_dismissed: false,
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
    fn agent_inventory_counts_invalid_files_without_deleting_them() {
        let (store, dir) = test_store();
        store
            .upsert_agent(&test_agent_state(test_pane_key()))
            .unwrap();
        let invalid_path = dir.path().join("agents").join("invalid.json");
        fs::write(&invalid_path, "{").unwrap();

        let inventory = store.inspect_agent_inventory().unwrap();

        assert_eq!(inventory.total, 2);
        assert_eq!(inventory.invalid.len(), 1);
        assert_eq!(inventory.states.len(), 1);
        assert!(invalid_path.exists());
    }

    #[test]
    fn ambiguous_invalid_filename_is_not_attributed() {
        assert!(unambiguous_pane_key_from_filename("tmux__instance__part__%1.json").is_none());
        assert!(unambiguous_pane_key_from_filename("tmux__instance___%1.json").is_none());
        assert!(unambiguous_pane_key_from_filename("tmux__%FF__%1.json").is_none());
        assert!(unambiguous_pane_key_from_filename("tmux__instance__%251.json").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn agent_inventory_ignores_files_that_disappear_before_read() {
        let (store, dir) = test_store();
        let agents_dir = dir.path().join("agents");
        std::os::unix::fs::symlink(
            agents_dir.join("already-gone"),
            agents_dir.join("vanished.json"),
        )
        .unwrap();

        let inventory = store.inspect_agent_inventory().unwrap();

        assert_eq!(inventory.total, 0);
        assert!(inventory.invalid.is_empty());
        assert!(inventory.states.is_empty());
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

    fn write_raw_agent(store: &StateStore, state: &AgentState) {
        let path = store.agent_path(&state.pane_key);
        fs::write(path, serde_json::to_vec_pretty(state).unwrap()).unwrap();
    }

    fn context_state(
        backend: &str,
        instance: &str,
        pane: usize,
        boot: Option<&str>,
        workdir: usize,
    ) -> AgentState {
        let mut state = test_agent_state(PaneKey {
            backend: backend.to_string(),
            instance: instance.to_string(),
            pane_id: format!("%{pane}"),
        });
        state.boot_id = boot.map(str::to_string);
        state.workdir = PathBuf::from(format!("/repo/worktree-{workdir}"));
        state.updated_ts = pane as u64;
        state
    }

    #[test]
    fn sidebar_cache_filters_foreign_context_and_reuses_unchanged_parses() {
        let (store, dir) = test_store();
        for pane in 0..285 {
            write_raw_agent(
                &store,
                &context_state("tmux", "default", pane, Some("boot"), pane % 43),
            );
        }
        for pane in 285..302 {
            write_raw_agent(
                &store,
                &context_state("tmux", "test", pane, Some("test-boot"), pane),
            );
        }
        for pane in 302..311 {
            write_raw_agent(&store, &context_state("wezterm", "main", pane, None, pane));
        }
        fs::write(
            dir.path().join("agents/tmux__default__legacy__name.json"),
            "{",
        )
        .unwrap();

        let mut cache = AgentStateCache::default();
        let cold = cache.load_context(&store, "tmux", "default").unwrap();
        assert_eq!(cold.agents.len(), 285);
        assert_eq!(cold.stats.listed, 312);
        assert_eq!(cold.stats.reads, 286);
        assert_eq!(cold.stats.parses, 286);

        let warm = cache.load_context(&store, "tmux", "default").unwrap();
        assert_eq!(warm.agents.len(), 285);
        assert_eq!(warm.stats.listed, 312);
        assert_eq!(warm.stats.reads, 0);
        assert_eq!(warm.stats.parses, 0);
    }

    #[test]
    fn sidebar_cache_invalidates_atomic_replacement_and_deletion() {
        let (store, _dir) = test_store();
        let mut state = context_state("tmux", "default", 1, Some("boot"), 1);
        write_raw_agent(&store, &state);
        let mut cache = AgentStateCache::default();
        let first = cache.load_context(&store, "tmux", "default").unwrap();
        assert_eq!(first.stats.reads, 1);

        let path = store.agent_path(&state.pane_key);
        let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        state.updated_ts += 1;
        let replacement = serde_json::to_vec_pretty(&state).unwrap();
        crate::util::write_atomic(&path, &replacement).unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(original_mtime))
            .unwrap();

        let replaced = cache.load_context(&store, "tmux", "default").unwrap();
        assert_eq!(replaced.stats.reads, 1);
        assert_eq!(replaced.agents[0].updated_ts, state.updated_ts);

        fs::remove_file(path).unwrap();
        let deleted = cache.load_context(&store, "tmux", "default").unwrap();
        assert!(deleted.agents.is_empty());
        assert!(cache.files.is_empty());
    }

    #[test]
    fn sidebar_cache_reparses_repaired_malformed_file() {
        let (store, _dir) = test_store();
        let state = context_state("tmux", "default", 1, Some("boot"), 1);
        let path = store.agent_path(&state.pane_key);
        fs::write(&path, "{").unwrap();
        let mut cache = AgentStateCache::default();
        let malformed = cache.load_context(&store, "tmux", "default").unwrap();
        assert!(malformed.agents.is_empty());
        assert_eq!(malformed.stats.parses, 1);
        let unchanged = cache.load_context(&store, "tmux", "default").unwrap();
        assert_eq!(unchanged.stats.parses, 0);

        crate::util::write_atomic(&path, &serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        let repaired = cache.load_context(&store, "tmux", "default").unwrap();
        assert_eq!(repaired.stats.parses, 1);
        assert_eq!(repaired.agents.len(), 1);
    }

    #[test]
    fn compaction_bounds_boot_history_and_preserves_foreign_and_unknown_state() {
        let (store, _dir) = test_store();
        for pane in 0..242 {
            let boot = format!("boot-{}", pane % 17);
            write_raw_agent(
                &store,
                &context_state("tmux", "default", pane, Some(&boot), pane % 21),
            );
        }
        for pane in 242..285 {
            write_raw_agent(
                &store,
                &context_state("tmux", "default", pane, Some("current"), pane),
            );
        }
        write_raw_agent(&store, &context_state("tmux", "default", 285, None, 999));
        for pane in 286..303 {
            write_raw_agent(
                &store,
                &context_state("tmux", "test", pane, Some("old"), pane),
            );
        }
        for pane in 303..312 {
            write_raw_agent(&store, &context_state("wezterm", "main", pane, None, pane));
        }

        let stats = store
            .compact_context("tmux", "default", Some("current"))
            .unwrap();
        assert_eq!(stats.compacted, 242);
        assert_eq!(stats.retained_flat, 44);
        assert_eq!(stats.recovery_entries, 21);
        assert_eq!(store.list_all_agents().unwrap().len(), 70);
        assert_eq!(
            store
                .resurrection_snapshot("tmux", "default")
                .unwrap()
                .len(),
            65
        );

        let repeated = store
            .compact_context("tmux", "default", Some("current"))
            .unwrap();
        assert_eq!(repeated.compacted, 0);
        assert_eq!(repeated.recovery_entries, 21);
    }

    #[test]
    fn repeated_boots_remain_bounded_by_recovery_targets() {
        let (store, _dir) = test_store();
        for boot in 0..100 {
            for pane in 0..5 {
                write_raw_agent(
                    &store,
                    &context_state(
                        "tmux",
                        "default",
                        boot * 10 + pane,
                        Some(&format!("boot-{boot}")),
                        pane % 3,
                    ),
                );
            }
            let current = format!("boot-{}", boot + 1);
            store
                .compact_context("tmux", "default", Some(&current))
                .unwrap();
            assert_eq!(store.list_all_agents().unwrap().len(), 0);
            assert_eq!(
                store
                    .resurrection_snapshot("tmux", "default")
                    .unwrap()
                    .len(),
                3
            );
        }
    }

    #[test]
    fn compaction_preserves_resurrection_agent_selection() {
        let (store, _dir) = test_store();
        let mut recognized = context_state("tmux", "default", 1, Some("old-a"), 1);
        recognized.command = "claude".to_string();
        recognized.updated_ts = 10;
        let mut newer_unknown = context_state("tmux", "default", 2, Some("old-b"), 1);
        newer_unknown.command = "shell-with-no-profile".to_string();
        newer_unknown.updated_ts = 20;
        write_raw_agent(&store, &recognized);
        write_raw_agent(&store, &newer_unknown);

        store
            .compact_context("tmux", "default", Some("current"))
            .unwrap();
        let snapshot = store.resurrection_snapshot("tmux", "default").unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            RecoveryAgentChoice::from_state(&snapshot[0].state)
                .unwrap()
                .command,
            "claude"
        );
        assert_eq!(snapshot[0].represented_count, 2);
    }

    #[test]
    fn concurrent_cache_reads_observe_only_complete_atomic_writes() {
        let (store, _dir) = test_store();
        let store = std::sync::Arc::new(store);
        let state = context_state("tmux", "default", 1, Some("current"), 1);
        let key = state.pane_key.clone();
        store.upsert_agent(&state).unwrap();

        let writer_store = store.clone();
        let writer = std::thread::spawn(move || {
            for updated_ts in 1..=100 {
                let mut update = state.clone();
                update.updated_ts = updated_ts;
                writer_store.upsert_agent(&update).unwrap();
            }
        });
        let mut cache = AgentStateCache::default();
        for _ in 0..100 {
            let loaded = cache.load_context(&store, "tmux", "default").unwrap();
            assert!(loaded.agents.len() <= 1);
            if let Some(agent) = loaded.agents.first() {
                assert_eq!(agent.boot_id.as_deref(), Some("current"));
            }
        }
        writer.join().unwrap();
        assert_eq!(store.get_agent(&key).unwrap().unwrap().updated_ts, 100);
    }

    #[test]
    fn upsert_preserves_previous_boot_before_recycled_key_replacement() {
        let (store, _dir) = test_store();
        let mut old = context_state("tmux", "default", 1, Some("boot-a"), 7);
        old.command = "claude".to_string();
        store.upsert_agent(&old).unwrap();
        let mut current = old.clone();
        current.boot_id = Some("boot-b".to_string());
        current.pane_pid += 1;
        current.updated_ts += 1;
        store.upsert_agent(&current).unwrap();

        assert_eq!(
            store.get_agent(&current.pane_key).unwrap().unwrap().boot_id,
            current.boot_id
        );
        let recovery = store.resurrection_snapshot("tmux", "default").unwrap();
        assert!(
            recovery
                .iter()
                .any(|record| record.state.boot_id == old.boot_id)
        );
    }

    #[test]
    fn interrupted_recovery_publication_is_idempotent() {
        let (store, _dir) = test_store();
        let state = context_state("tmux", "default", 1, Some("old"), 1);
        store
            .with_agent_lock(|store| {
                store.merge_recovery_locked("tmux", "default", std::slice::from_ref(&state))?;
                store.merge_recovery_locked("tmux", "default", std::slice::from_ref(&state))?;
                Ok(())
            })
            .unwrap();
        let snapshot = store.resurrection_snapshot("tmux", "default").unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].represented_count, 1);
    }

    #[test]
    fn flat_plan_consumes_state_compacted_during_restoration() {
        let (store, _dir) = test_store();
        let state = context_state("tmux", "default", 1, Some("old"), 1);
        write_raw_agent(&store, &state);
        let snapshot = store.resurrection_snapshot("tmux", "default").unwrap();
        store
            .compact_context("tmux", "default", Some("current"))
            .unwrap();

        let sources: Vec<_> = snapshot.into_iter().map(|record| record.source).collect();
        store.consume_agent_sources(&sources).unwrap();
        assert!(
            store
                .resurrection_snapshot("tmux", "default")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn recovery_consumption_removes_the_planned_revision() {
        let (store, _dir) = test_store();
        let state = context_state("tmux", "default", 1, Some("old"), 1);
        write_raw_agent(&store, &state);
        store
            .compact_context("tmux", "default", Some("current"))
            .unwrap();
        let snapshot = store.resurrection_snapshot("tmux", "default").unwrap();
        let sources: Vec<_> = snapshot.into_iter().map(|record| record.source).collect();
        store.consume_agent_sources(&sources).unwrap();
        assert!(
            store
                .resurrection_snapshot("tmux", "default")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn conditional_consumption_preserves_atomic_replacement() {
        let (store, _dir) = test_store();
        let old = context_state("tmux", "default", 1, Some("boot-a"), 1);
        write_raw_agent(&store, &old);
        let snapshot = store.resurrection_snapshot("tmux", "default").unwrap();
        let mut replacement = old.clone();
        replacement.boot_id = Some("boot-b".to_string());
        replacement.updated_ts += 1;
        write_raw_agent(&store, &replacement);

        let sources: Vec<_> = snapshot.into_iter().map(|record| record.source).collect();
        store.consume_agent_sources(&sources).unwrap();
        assert_eq!(
            store
                .get_agent(&replacement.pane_key)
                .unwrap()
                .unwrap()
                .boot_id,
            replacement.boot_id
        );
    }

    #[test]
    fn recovery_path_migration_updates_compacted_state() {
        let (store, _dir) = test_store();
        let mut state = context_state("tmux", "default", 1, Some("old"), 1);
        state.workdir = PathBuf::from("/repo/wt/old/src");
        write_raw_agent(&store, &state);
        store
            .compact_context("tmux", "default", Some("current"))
            .unwrap();

        let migrated = store
            .migrate_worktree_paths(
                Path::new("/repo/wt/old"),
                Path::new("/repo/wt/new"),
                "wm-old",
                "wm-new",
            )
            .unwrap();
        assert_eq!(migrated, 1);
        let snapshot = store.resurrection_snapshot("tmux", "default").unwrap();
        assert_eq!(snapshot[0].state.workdir, PathBuf::from("/repo/wt/new/src"));
    }

    #[test]
    fn recovery_path_migration_preserves_identity_mismatch() {
        let (store, _dir) = test_store();
        let path = store.recovery_path("tmux", "default");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let manifest = RecoveryManifest {
            version: 1,
            backend: "wezterm".to_string(),
            instance: "other".to_string(),
            last_compacted_boot: None,
            entries: Vec::new(),
        };
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let migrated = store
            .migrate_worktree_paths(
                Path::new("/repo/old"),
                Path::new("/repo/new"),
                "wm-old",
                "wm-new",
            )
            .unwrap();
        assert_eq!(migrated, 0);
        assert!(path.exists());
        assert!(!store.recovery_path("wezterm", "other").exists());
    }

    #[test]
    #[ignore = "manual release-mode filesystem benchmark"]
    fn benchmark_sidebar_cache_with_311_files() {
        let (store, _dir) = test_store();
        for pane in 0..242 {
            write_raw_agent(
                &store,
                &context_state(
                    "tmux",
                    "default",
                    pane,
                    Some(&format!("old-{}", pane % 18)),
                    pane % 103,
                ),
            );
        }
        for pane in 242..285 {
            write_raw_agent(
                &store,
                &context_state("tmux", "default", pane, Some("current"), pane),
            );
        }
        for pane in 285..311 {
            write_raw_agent(
                &store,
                &context_state("tmux", "other", pane, Some("other"), pane),
            );
        }

        let listing_start = std::time::Instant::now();
        let initial_file_count = fs::read_dir(store.agents_dir()).unwrap().count();
        let listing_elapsed = listing_start.elapsed();
        let compact_start = std::time::Instant::now();
        let compact = store
            .compact_context("tmux", "default", Some("current"))
            .unwrap();
        let compact_elapsed = compact_start.elapsed();
        let retained_agent_files = fs::read_dir(store.agents_dir()).unwrap().count();

        let mut cache = AgentStateCache::default();
        let cold_start = std::time::Instant::now();
        let cold = cache.load_context(&store, "tmux", "default").unwrap();
        let cold_elapsed = cold_start.elapsed();
        let mut warm_samples = Vec::new();
        let mut warm = CachedAgentLoad {
            agents: Vec::new(),
            stats: AgentCacheStats::default(),
        };
        for _ in 0..100 {
            let start = std::time::Instant::now();
            warm = cache.load_context(&store, "tmux", "default").unwrap();
            warm_samples.push(start.elapsed());
        }
        warm_samples.sort_unstable();

        let key = PaneKey {
            backend: "tmux".to_string(),
            instance: "default".to_string(),
            pane_id: "%242".to_string(),
        };
        let mut updated = store.get_agent(&key).unwrap().unwrap();
        updated.updated_ts += 1;
        store.upsert_agent(&updated).unwrap();
        let update_start = std::time::Instant::now();
        let update = cache.load_context(&store, "tmux", "default").unwrap();
        let update_elapsed = update_start.elapsed();

        eprintln!(
            "initial_files={initial_file_count} listing_311={listing_elapsed:?} compact={compact_elapsed:?} compacted={} recovery_entries={} retained_agent_files={retained_agent_files} cold={cold_elapsed:?} cold_reads={} cold_parses={} warm_p50={:?} warm_p95={:?} warm_reads={} warm_parses={} update={update_elapsed:?} update_reads={} update_parses={}",
            compact.compacted,
            compact.recovery_entries,
            cold.stats.reads,
            cold.stats.parses,
            warm_samples[50],
            warm_samples[95],
            warm.stats.reads,
            warm.stats.parses,
            update.stats.reads,
            update.stats.parses,
        );
    }

    #[test]
    fn legacy_tmux_boot_matches_only_a_valid_same_start_time() {
        assert!(same_server_boot("tmux", "1700000000", "1700000000:42"));
        assert!(!same_server_boot("tmux", "1700000000", "1700000001:42"));
        assert!(!same_server_boot("tmux", "1700000000:41", "1700000000:42"));
        assert!(!same_server_boot("tmux", "1700000000", "1700000000:"));
        assert!(!same_server_boot(
            "tmux",
            "1700000000",
            "1700000000:unknown"
        ));
        assert!(!same_server_boot("tmux", "unknown", "unknown:42"));
        assert!(!same_server_boot("wezterm", "1700000000", "1700000000:42"));
        assert!(!server_lifecycle_changed(
            "tmux",
            Some("1700000000"),
            Some("1700000000:42")
        ));
    }

    #[test]
    fn compaction_retains_same_start_legacy_tmux_records() {
        let (store, _dir) = test_store();
        let legacy = context_state("tmux", "default", 1, Some("1700000000"), 1);
        let older = context_state("tmux", "default", 2, Some("1699999999"), 2);
        write_raw_agent(&store, &legacy);
        write_raw_agent(&store, &older);
        let stats = store
            .compact_context("tmux", "default", Some("1700000000:42"))
            .unwrap();
        assert_eq!(stats.compacted, 1);
        assert_eq!(stats.retained_flat, 1);
        assert!(store.get_agent(&legacy.pane_key).unwrap().is_some());
        assert!(store.get_agent(&older.pane_key).unwrap().is_none());
    }

    #[test]
    fn legacy_boot_upgrade_archives_only_a_different_pane_process() {
        for same_pid in [true, false] {
            let (store, _dir) = test_store();
            let legacy = context_state("tmux", "default", 1, Some("1700000000"), 1);
            write_raw_agent(&store, &legacy);
            let mut current = legacy.clone();
            current.boot_id = Some("1700000000:42".to_string());
            if !same_pid {
                current.pane_pid += 1;
            }
            store.upsert_agent(&current).unwrap();
            let manifest = store
                .load_recovery_manifest_locked("tmux", "default")
                .unwrap();
            assert_eq!(manifest.entries.len(), usize::from(!same_pid));
        }
    }

    #[test]
    fn flat_consumption_preserves_unplanned_compacted_sources() {
        let (store, _dir) = test_store();
        let mut planned = context_state("tmux", "default", 1, Some("old"), 1);
        planned.command = "claude".to_string();
        write_raw_agent(&store, &planned);
        let sources: Vec<_> = store
            .resurrection_snapshot("tmux", "default")
            .unwrap()
            .into_iter()
            .map(|record| record.source)
            .collect();
        let mut unplanned = context_state("tmux", "default", 2, Some("old"), 1);
        unplanned.command = "codex".to_string();
        store.upsert_agent(&unplanned).unwrap();
        store
            .compact_context("tmux", "default", Some("current"))
            .unwrap();
        store.consume_agent_sources(&sources).unwrap();
        let remaining = store.resurrection_snapshot("tmux", "default").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].represented_count, 2);
        assert_eq!(remaining[0].state.command, "codex");
    }

    #[test]
    fn flat_consumption_removes_a_fully_planned_compaction_batch() {
        let (store, _dir) = test_store();
        for pane in 1..=2 {
            write_raw_agent(
                &store,
                &context_state("tmux", "default", pane, Some("old"), 1),
            );
        }
        let sources: Vec<_> = store
            .resurrection_snapshot("tmux", "default")
            .unwrap()
            .into_iter()
            .map(|record| record.source)
            .collect();
        store
            .compact_context("tmux", "default", Some("current"))
            .unwrap();
        store.consume_agent_sources(&sources).unwrap();
        assert!(
            store
                .resurrection_snapshot("tmux", "default")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn flat_consumption_preserves_history_outside_the_recent_batch() {
        let (store, _dir) = test_store();
        write_raw_agent(&store, &context_state("tmux", "default", 1, Some("old"), 1));
        store
            .compact_context("tmux", "default", Some("middle"))
            .unwrap();
        write_raw_agent(
            &store,
            &context_state("tmux", "default", 2, Some("middle"), 1),
        );
        let sources: Vec<_> = store
            .resurrection_snapshot("tmux", "default")
            .unwrap()
            .into_iter()
            .map(|record| record.source)
            .collect();
        store
            .compact_context("tmux", "default", Some("current"))
            .unwrap();
        store.consume_agent_sources(&sources).unwrap();
        let remaining = store.resurrection_snapshot("tmux", "default").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].represented_count, 2);
    }
}
