//! Run command state management for executing commands in worktree panes.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use super::store::get_state_dir;
use crate::util::write_atomic;

/// Specification for a command to execute.
#[derive(Debug, Serialize, Deserialize)]
pub struct RunSpec {
    pub command: String,
    pub worktree_path: PathBuf,
}

/// Result of command execution.
#[derive(Debug, Serialize, Deserialize)]
pub struct RunResult {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

/// Get the base directory for run artifacts.
fn runs_base_dir() -> Result<PathBuf> {
    let dir = get_state_dir()?.join("runs");
    fs::create_dir_all(&dir).context("Failed to create runs directory")?;
    Ok(dir)
}

/// Generate a unique run ID (timestamp + pid for collision resistance).
pub fn generate_run_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{:x}-{}", ts, pid)
}

/// Validate run_id to prevent path traversal (alphanumeric + hyphen only).
fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty()
        || !run_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(anyhow!("Invalid run_id: must be alphanumeric with hyphens"));
    }
    Ok(())
}

/// Create a new run directory and write the spec. Returns absolute path.
pub fn create_run(run_id: &str, spec: &RunSpec) -> Result<PathBuf> {
    validate_run_id(run_id)?;
    let dir = runs_base_dir()?.join(run_id);
    fs::create_dir_all(&dir).context("Failed to create run directory")?;

    let spec_path = dir.join("spec.json");
    let content = serde_json::to_string_pretty(spec)?;
    fs::write(&spec_path, content)?;

    // Pre-create output files (empty)
    fs::write(dir.join("stdout"), "")?;
    fs::write(dir.join("stderr"), "")?;

    Ok(dir)
}

/// Read the spec from a run directory (absolute path).
pub fn read_spec(run_dir: &Path) -> Result<RunSpec> {
    let path = run_dir.join("spec.json");
    let content = fs::read_to_string(&path).context("Failed to read run spec")?;
    serde_json::from_str(&content).context("Failed to parse run spec")
}

/// Read the result from a run directory (returns None if not complete).
pub fn read_result(run_dir: &Path) -> Result<Option<RunResult>> {
    let path = run_dir.join("result.json");
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&content)?))
}

/// Write the result atomically to a run directory.
pub fn write_result(run_dir: &Path, result: &RunResult) -> Result<()> {
    let final_path = run_dir.join("result.json");

    let content = serde_json::to_string_pretty(result)?;
    write_atomic(&final_path, content.as_bytes())?;
    Ok(())
}

/// Clean up a run directory.
pub fn cleanup_run(run_dir: &Path) -> Result<()> {
    if run_dir.exists() {
        fs::remove_dir_all(run_dir)?;
    }
    Ok(())
}
