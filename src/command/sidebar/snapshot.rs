//! Snapshot data types and builder for daemon-to-client communication.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{SidebarPosition, StatusIcons};
use crate::git::GitStatus;
use crate::github::PrSummary;
use crate::multiplexer::{AgentPane, AgentStatus};

use super::app::{SidebarFilterMode, SidebarLayoutMode};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PrPathEntry {
    pub branch: String,
    pub summary: PrSummary,
}

/// A complete sidebar state snapshot, pushed from daemon to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarSnapshot {
    pub position: SidebarPosition,
    pub layout_mode: SidebarLayoutMode,
    #[serde(default)]
    pub filter_mode: SidebarFilterMode,
    pub active_windows: HashSet<(String, String)>,
    #[serde(default)]
    pub active_pane_ids: HashSet<String>,
    /// Number of panes per window (used by clients to detect last-pane condition).
    #[serde(default)]
    pub window_pane_counts: HashMap<String, usize>,
    /// Git status per worktree path (computed by daemon background worker).
    #[serde(default)]
    pub git_statuses: HashMap<PathBuf, GitStatus>,
    /// PR summary per worktree path (computed by daemon background worker).
    #[serde(default)]
    pub pr_statuses: HashMap<PathBuf, PrSummary>,
    /// Pane IDs of agents detected as interrupted (working but no pane output change).
    #[serde(default)]
    pub interrupted_pane_ids: HashSet<String>,
    /// Pane IDs of agents manually marked as sleeping by the user.
    #[serde(default)]
    pub sleeping_pane_ids: HashSet<String>,
    pub agents: Vec<AgentPane>,
    /// Increments whenever the daemon reloads the merged config.
    /// Clients use this to trigger their own per-project config reload.
    #[serde(default)]
    pub config_version: u64,
}

/// Build a snapshot from reconciled agents and tmux state.
#[allow(clippy::too_many_arguments)]
pub fn build_snapshot(
    mut agents: Vec<AgentPane>,
    tmux_statuses: &HashMap<String, Option<String>>,
    pane_window_ids: &HashMap<String, String>,
    active_windows: HashSet<(String, String)>,
    active_pane_ids: HashSet<String>,
    window_pane_counts: HashMap<String, usize>,
    position: SidebarPosition,
    layout_mode: SidebarLayoutMode,
    filter_mode: SidebarFilterMode,
    status_icons: &StatusIcons,
    git_statuses: HashMap<PathBuf, GitStatus>,
    pr_statuses: HashMap<PathBuf, PrPathEntry>,
    sleeping_pane_ids: &HashSet<String>,
) -> SidebarSnapshot {
    let done_icon = status_icons.done();
    let waiting_icon = status_icons.waiting();

    // Suppress Done/Waiting when tmux's auto-clear hook has already cleared
    for agent in &mut agents {
        if let Some(observed) = tmux_statuses.get(&agent.pane_id) {
            match agent.status {
                Some(AgentStatus::Done) if observed.as_deref() != Some(done_icon) => {
                    agent.status = None;
                }
                Some(AgentStatus::Waiting) if observed.as_deref() != Some(waiting_icon) => {
                    agent.status = None;
                }
                _ => {}
            }
        }
    }

    // Sort by recency
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    agents.sort_by_cached_key(|a| {
        let is_sleeping = sleeping_pane_ids.contains(&a.pane_id);
        let elapsed = a
            .status_ts
            .map(|ts| now.saturating_sub(ts))
            .unwrap_or(u64::MAX);
        let pane_num: u64 = a
            .pane_id
            .strip_prefix('%')
            .unwrap_or(&a.pane_id)
            .parse()
            .unwrap_or(u64::MAX);
        (is_sleeping, elapsed, pane_num)
    });

    // Populate window_id from the tmux state lookup
    for agent in &mut agents {
        if let Some(wid) = pane_window_ids.get(&agent.pane_id) {
            agent.window_id = wid.clone();
        }
    }

    // Prune sleeping set to only include live agents
    let live_sleeping: HashSet<String> = sleeping_pane_ids
        .iter()
        .filter(|id| agents.iter().any(|a| &a.pane_id == *id))
        .cloned()
        .collect();

    let live_paths: HashSet<&PathBuf> = agents.iter().map(|a| &a.path).collect();
    let pr_statuses = pr_statuses
        .into_iter()
        .filter_map(|(path, entry)| {
            let branch = git_statuses.get(&path)?.branch.as_deref()?;
            if live_paths.contains(&path)
                && branch != "main"
                && branch != "master"
                && branch == entry.branch
            {
                Some((path, entry.summary))
            } else {
                None
            }
        })
        .collect();

    SidebarSnapshot {
        position,
        layout_mode,
        filter_mode,
        active_windows,
        active_pane_ids,
        window_pane_counts,
        git_statuses,
        pr_statuses,
        interrupted_pane_ids: HashSet::new(),
        sleeping_pane_ids: live_sleeping,
        agents,
        config_version: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(path: &str) -> AgentPane {
        AgentPane {
            session: "s".to_string(),
            window_name: "w".to_string(),
            pane_id: "%1".to_string(),
            window_id: String::new(),
            path: PathBuf::from(path),
            pane_title: None,
            status: None,
            status_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
        }
    }

    fn pr(number: u32) -> PrSummary {
        PrSummary {
            number,
            title: "test".to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            checks: None,
            check_meta: None,
            url: None,
        }
    }

    fn pr_entry(branch: &str, number: u32) -> PrPathEntry {
        PrPathEntry {
            branch: branch.to_string(),
            summary: pr(number),
        }
    }

    fn build(
        agents: Vec<AgentPane>,
        git_statuses: HashMap<PathBuf, GitStatus>,
        pr_statuses: HashMap<PathBuf, PrPathEntry>,
    ) -> SidebarSnapshot {
        build_snapshot(
            agents,
            &HashMap::new(),
            &HashMap::new(),
            HashSet::new(),
            HashSet::new(),
            HashMap::new(),
            SidebarPosition::Left,
            SidebarLayoutMode::default(),
            SidebarFilterMode::default(),
            &StatusIcons::default(),
            git_statuses,
            pr_statuses,
            &HashSet::new(),
        )
    }

    #[test]
    fn pr_statuses_exclude_main_branch_paths() {
        let path = PathBuf::from("/repo");
        let git = GitStatus {
            branch: Some("main".to_string()),
            base_branch: "main".to_string(),
            ..Default::default()
        };

        let snapshot = build(
            vec![agent("/repo")],
            HashMap::from([(path.clone(), git)]),
            HashMap::from([(path.clone(), pr_entry("main", 10757))]),
        );

        assert!(!snapshot.pr_statuses.contains_key(&path));
    }

    #[test]
    fn pr_statuses_keep_feature_branch_paths() {
        let path = PathBuf::from("/repo");
        let git = GitStatus {
            branch: Some("feature".to_string()),
            ..Default::default()
        };

        let snapshot = build(
            vec![agent("/repo")],
            HashMap::from([(path.clone(), git)]),
            HashMap::from([(path.clone(), pr_entry("feature", 123))]),
        );

        assert_eq!(
            snapshot.pr_statuses.get(&path).map(|pr| pr.number),
            Some(123)
        );
    }

    #[test]
    fn pr_statuses_exclude_master_branch_paths() {
        let path = PathBuf::from("/repo");
        let git = GitStatus {
            branch: Some("master".to_string()),
            base_branch: "master".to_string(),
            ..Default::default()
        };

        let snapshot = build(
            vec![agent("/repo")],
            HashMap::from([(path.clone(), git)]),
            HashMap::from([(path.clone(), pr_entry("master", 10757))]),
        );

        assert!(!snapshot.pr_statuses.contains_key(&path));
    }

    #[test]
    fn pr_statuses_exclude_mismatched_branch() {
        let path = PathBuf::from("/repo");
        let git = GitStatus {
            branch: Some("feature-b".to_string()),
            ..Default::default()
        };

        let snapshot = build(
            vec![agent("/repo")],
            HashMap::from([(path.clone(), git)]),
            HashMap::from([(path.clone(), pr_entry("feature-a", 123))]),
        );

        assert!(!snapshot.pr_statuses.contains_key(&path));
    }

    #[test]
    fn pr_statuses_exclude_missing_branch() {
        let path = PathBuf::from("/repo");

        let snapshot = build(
            vec![agent("/repo")],
            HashMap::from([(path.clone(), GitStatus::default())]),
            HashMap::from([(path.clone(), pr_entry("feature", 123))]),
        );

        assert!(!snapshot.pr_statuses.contains_key(&path));
    }

    #[test]
    fn pr_statuses_exclude_stale_paths() {
        let live_path = PathBuf::from("/repo");
        let stale_path = PathBuf::from("/old-repo");
        let git = GitStatus {
            branch: Some("feature".to_string()),
            ..Default::default()
        };

        let snapshot = build(
            vec![agent("/repo")],
            HashMap::from([(live_path.clone(), git.clone()), (stale_path.clone(), git)]),
            HashMap::from([
                (live_path.clone(), pr_entry("feature", 1)),
                (stale_path.clone(), pr_entry("feature", 2)),
            ]),
        );

        assert!(snapshot.pr_statuses.contains_key(&live_path));
        assert!(!snapshot.pr_statuses.contains_key(&stale_path));
    }
}
