//! Snapshot data types and builder for daemon-to-client communication.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent_display::extract_project_name;
use crate::config::{SidebarGroupBy, SidebarPosition, SidebarSort, StatusIcons};
use crate::git::GitStatus;
use crate::github::{CheckSummary, PrSummary};
use crate::multiplexer::{AgentPane, AgentStatus};

use super::app::{SidebarFilterMode, SidebarLayoutMode};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PrPathEntry {
    pub branch: String,
    pub summary: PrSummary,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CheckPathEntry {
    pub branch: String,
    pub summary: CheckSummary,
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
    /// GitHub check summary per worktree path (computed by daemon background worker).
    #[serde(default)]
    pub check_statuses: HashMap<PathBuf, CheckSummary>,
    /// Pane IDs of agents detected as interrupted (working but no pane output change).
    #[serde(default)]
    pub interrupted_pane_ids: HashSet<String>,
    /// Pane IDs of agents manually marked as sleeping by the user.
    #[serde(default)]
    pub sleeping_pane_ids: HashSet<String>,
    /// Grouping the daemon ordered `agents` with, if any. Clients render the
    /// grouping the daemon sorted by rather than re-deriving it from config.
    #[serde(default)]
    pub group_by: Option<SidebarGroupBy>,
    /// Group labels whose stale agents the user expanded. Shared through tmux
    /// so every sidebar pane shows the same thing.
    #[serde(default)]
    pub expanded_groups: Vec<String>,
    /// Pane IDs the daemon judged stale. Published so clients, the pane list
    /// behind `jump` and `{idx}` all fold the same agents away.
    #[serde(default)]
    pub stale_pane_ids: HashSet<String>,
    /// Whether stale agents fold. Published rather than read per client: the
    /// daemon's pane list decides which agents `jump` can reach, so a client
    /// that folded differently would number rows the list does not carry.
    #[serde(default)]
    pub collapse_stale: bool,
    pub agents: Vec<AgentPane>,
    /// Increments whenever the daemon reloads the merged config.
    /// Clients use this to trigger their own per-project config reload.
    #[serde(default)]
    pub config_version: u64,
}

/// Label used when an agent's group cannot be named.
pub const UNKNOWN_GROUP_LABEL: &str = "(unknown)";

/// Seconds of inactivity after which an agent counts as stale for ordering.
pub const STALE_THRESHOLD_SECS: u64 = 60 * 60;

/// The group an agent belongs to under the given grouping mode.
pub fn group_label(agent: &AgentPane, group_by: SidebarGroupBy) -> String {
    let raw = match group_by {
        SidebarGroupBy::Project => extract_project_name(&agent.path),
        SidebarGroupBy::Session => agent.session.clone(),
        SidebarGroupBy::None => String::new(),
    };
    if raw.trim().is_empty() {
        UNKNOWN_GROUP_LABEL.to_string()
    } else {
        raw
    }
}

/// Alphabetical group ordering: case-insensitive, with the raw label breaking
/// ties so labels differing only in case keep a deterministic order.
fn group_sort_key(agent: &AgentPane, group_by: Option<SidebarGroupBy>) -> (String, String) {
    match group_by {
        Some(mode) => {
            let label = group_label(agent, mode);
            (label.to_lowercase(), label)
        }
        None => (String::new(), String::new()),
    }
}

/// Numeric pane id for stable ordering. Handles tmux (`%3`) and numeric ids.
fn pane_num(agent: &AgentPane) -> u64 {
    agent
        .pane_id
        .strip_prefix('%')
        .unwrap_or(&agent.pane_id)
        .parse()
        .unwrap_or(u64::MAX)
}

fn activity_age(agent: &AgentPane, now: u64) -> u64 {
    agent
        .activity_ts()
        .map(|ts| now.saturating_sub(ts))
        .unwrap_or(u64::MAX)
}

/// Attention category for `SidebarSort::Priority`, lower is more urgent.
///
/// Waiting and done agents stay actionable no matter how long they have
/// waited. Working agents demote only when interruption detection says they
/// stopped progressing; unknown agents demote on activity age. Sleeping
/// overrides every other status.
fn priority_category(
    agent: &AgentPane,
    now: u64,
    sleeping: &HashSet<String>,
    interrupted: &HashSet<String>,
) -> u8 {
    if sleeping.contains(&agent.pane_id) {
        return 5;
    }
    match agent.status {
        Some(AgentStatus::Waiting) => 0,
        Some(AgentStatus::Done) => 1,
        Some(AgentStatus::Working) => {
            if interrupted.contains(&agent.pane_id) {
                4
            } else {
                2
            }
        }
        None => {
            if activity_age(agent, now) > STALE_THRESHOLD_SECS {
                4
            } else {
                3
            }
        }
    }
}

/// Labels of the groups holding no live work. They sort below every other
/// group, so a project that is entirely asleep stops sitting between projects
/// that are not.
fn dormant_groups(
    agents: &[AgentPane],
    group_by: Option<SidebarGroupBy>,
    now: u64,
    sleeping: &HashSet<String>,
    interrupted: &HashSet<String>,
) -> HashSet<String> {
    let Some(mode) = group_by else {
        return HashSet::new();
    };
    let mut all = HashSet::new();
    let mut live = HashSet::new();
    for agent in agents {
        let label = group_label(agent, mode);
        if !crate::command::sidebar::template::context::agent_is_stale(
            agent,
            now,
            STALE_THRESHOLD_SECS,
            sleeping.contains(&agent.pane_id),
            interrupted.contains(&agent.pane_id),
        ) {
            live.insert(label.clone());
        }
        all.insert(label);
    }
    all.retain(|label| !live.contains(label));
    all
}

/// Order agents for publication: groups alphabetically, then the configured
/// ordering within each group. Grouping only prefixes the key, so an agent can
/// never leave its group. Ordering stays with the daemon so the published pane
/// list, `{idx}` and `workmux sidebar jump` agree with what clients draw.
pub(crate) fn order_agents(
    agents: &mut [AgentPane],
    group_by: Option<SidebarGroupBy>,
    sort: SidebarSort,
    now: u64,
    sleeping: &HashSet<String>,
    interrupted: &HashSet<String>,
    sink_dormant_groups: bool,
) {
    let dormant = if sink_dormant_groups {
        dormant_groups(agents, group_by, now, sleeping, interrupted)
    } else {
        HashSet::new()
    };
    // With collapsing, staleness also ranks inside a group so the stale agents
    // form one contiguous tail that a single toggle row can stand for, whatever
    // the configured sort does.
    let group_sort_key = |agent: &AgentPane| {
        let (lower, label) = group_sort_key(agent, group_by);
        let stale = sink_dormant_groups
            && crate::command::sidebar::template::context::agent_is_stale(
                agent,
                now,
                STALE_THRESHOLD_SECS,
                sleeping.contains(&agent.pane_id),
                interrupted.contains(&agent.pane_id),
            );
        (dormant.contains(&label), lower, label, stale)
    };
    match sort {
        SidebarSort::Recency => agents.sort_by_cached_key(|a| {
            (
                group_sort_key(a),
                sleeping.contains(&a.pane_id),
                activity_age(a, now),
                pane_num(a),
            )
        }),
        SidebarSort::Priority => agents.sort_by_cached_key(|a| {
            (
                group_sort_key(a),
                priority_category(a, now, sleeping, interrupted),
                activity_age(a, now),
                pane_num(a),
            )
        }),
        // Stable window order: session, then window index (unresolved last).
        // Sleeping agents keep their place; stability is the point.
        SidebarSort::Window => agents.sort_by_cached_key(|a| {
            (
                group_sort_key(a),
                a.session.clone(),
                a.window_index.map(u64::from).unwrap_or(u64::MAX),
                pane_num(a),
            )
        }),
    }
}

/// Whether an agent is folded away in the sidebar this snapshot describes.
///
/// Folding belongs to the grouped left sidebar: a flat list has no groups to
/// fold behind, the top bar draws no toggles, and `collapse_stale: false` asks
/// for every agent at full height. Anything that hides an agent from the rows
/// must hide it from the pane list behind `jump` as well, or the numbers under
/// the rows address agents the list does not carry.
pub fn is_folded(snapshot: &SidebarSnapshot, agent: &AgentPane) -> bool {
    let Some(group_by) = snapshot.group_by else {
        return false;
    };
    if !snapshot.collapse_stale || snapshot.position == SidebarPosition::Top {
        return false;
    }
    snapshot.stale_pane_ids.contains(&agent.pane_id)
        && !snapshot
            .expanded_groups
            .contains(&group_label(agent, group_by))
}

/// Inputs for one snapshot build.
#[derive(Default)]
pub(crate) struct SnapshotInputs {
    pub agents: Vec<AgentPane>,
    pub tmux_statuses: HashMap<String, Option<String>>,
    pub pane_window_ids: HashMap<String, String>,
    pub pane_window_indexes: HashMap<String, u32>,
    pub active_windows: HashSet<(String, String)>,
    pub active_pane_ids: HashSet<String>,
    pub window_pane_counts: HashMap<String, usize>,
    pub position: SidebarPosition,
    pub layout_mode: SidebarLayoutMode,
    pub filter_mode: SidebarFilterMode,
    pub sort: SidebarSort,
    pub group_by: Option<SidebarGroupBy>,
    /// Sink groups holding no live work below the rest, matching the client's
    /// collapsed rendering of stale agents.
    pub collapse_stale: bool,
    pub expanded_groups: Vec<String>,
    pub status_icons: StatusIcons,
    pub git_statuses: HashMap<PathBuf, GitStatus>,
    pub pr_statuses: HashMap<PathBuf, PrPathEntry>,
    pub check_statuses: HashMap<PathBuf, CheckPathEntry>,
    pub sleeping_pane_ids: HashSet<String>,
    pub interrupted_pane_ids: HashSet<String>,
}

/// Build a snapshot from reconciled agents and tmux state.
pub(crate) fn build_snapshot(input: SnapshotInputs) -> SidebarSnapshot {
    let SnapshotInputs {
        mut agents,
        tmux_statuses,
        pane_window_ids,
        pane_window_indexes,
        active_windows,
        active_pane_ids,
        window_pane_counts,
        position,
        layout_mode,
        filter_mode,
        sort,
        group_by,
        collapse_stale,
        expanded_groups,
        status_icons,
        git_statuses,
        pr_statuses,
        check_statuses,
        sleeping_pane_ids,
        interrupted_pane_ids,
    } = input;

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

    // Populate window_id and window_index from the tmux state lookup
    // (before sorting: window sort reads the freshly stamped index)
    for agent in &mut agents {
        if let Some(wid) = pane_window_ids.get(&agent.pane_id) {
            agent.window_id = wid.clone();
        }
        agent.window_index = pane_window_indexes.get(&agent.pane_id).copied();
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let stale_pane_ids: HashSet<String> = agents
        .iter()
        .filter(|agent| {
            crate::command::sidebar::template::context::agent_is_stale(
                agent,
                now,
                STALE_THRESHOLD_SECS,
                sleeping_pane_ids.contains(&agent.pane_id),
                interrupted_pane_ids.contains(&agent.pane_id),
            )
        })
        .map(|agent| agent.pane_id.clone())
        .collect();

    order_agents(
        &mut agents,
        group_by,
        sort,
        now,
        &sleeping_pane_ids,
        &interrupted_pane_ids,
        collapse_stale,
    );

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

    let check_statuses = check_statuses
        .into_iter()
        .filter_map(|(path, entry)| {
            let branch = git_statuses.get(&path)?.branch.as_deref()?;
            if live_paths.contains(&path) && branch == entry.branch {
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
        check_statuses,
        interrupted_pane_ids,
        sleeping_pane_ids: live_sleeping,
        group_by,
        expanded_groups,
        stale_pane_ids,
        collapse_stale,
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
            window_index: None,
            path: PathBuf::from(path),
            pane_title: None,
            status: None,
            status_ts: None,
            activity_ts: None,
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

    fn check_entry(branch: &str) -> CheckPathEntry {
        CheckPathEntry {
            branch: branch.to_string(),
            summary: CheckSummary {
                state: crate::github::CheckState::Success,
                meta: None,
            },
        }
    }

    fn build(
        agents: Vec<AgentPane>,
        git_statuses: HashMap<PathBuf, GitStatus>,
        pr_statuses: HashMap<PathBuf, PrPathEntry>,
    ) -> SidebarSnapshot {
        build_with_checks(agents, git_statuses, pr_statuses, HashMap::new())
    }

    fn build_with_checks(
        agents: Vec<AgentPane>,
        git_statuses: HashMap<PathBuf, GitStatus>,
        pr_statuses: HashMap<PathBuf, PrPathEntry>,
        check_statuses: HashMap<PathBuf, CheckPathEntry>,
    ) -> SidebarSnapshot {
        build_snapshot(SnapshotInputs {
            agents,
            git_statuses,
            pr_statuses,
            check_statuses,
            ..Default::default()
        })
    }

    /// Agent in project `project`, pane `%n`, with `age` seconds since activity.
    fn grouped_agent(project: &str, session: &str, pane: u32, age: u64, now: u64) -> AgentPane {
        let mut a = agent(&format!("/tmp/{project}__worktrees/w{pane}"));
        a.session = session.to_string();
        a.pane_id = format!("%{pane}");
        a.activity_ts = Some(now.saturating_sub(age));
        a.status_ts = a.activity_ts;
        a
    }

    #[test]
    fn only_a_sidebar_that_folds_hides_an_agent_from_the_pane_list() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let stale = grouped_agent("api", "main", 1, 6 * 60 * 60, now);
        let mut snapshot = build(vec![stale.clone()], HashMap::new(), HashMap::new());
        snapshot.group_by = Some(SidebarGroupBy::Project);
        snapshot.stale_pane_ids = HashSet::from([stale.pane_id.clone()]);
        snapshot.collapse_stale = true;
        assert!(is_folded(&snapshot, &stale));

        // Folding is a property of the grouped left sidebar. Every way of
        // turning it off has to reach the pane list too, or `jump <N>` counts
        // agents the rows do not fold away.
        snapshot.collapse_stale = false;
        assert!(!is_folded(&snapshot, &stale));

        snapshot.collapse_stale = true;
        snapshot.position = SidebarPosition::Top;
        assert!(!is_folded(&snapshot, &stale));

        snapshot.position = SidebarPosition::Left;
        snapshot.group_by = None;
        assert!(!is_folded(&snapshot, &stale));

        snapshot.group_by = Some(SidebarGroupBy::Project);
        snapshot.expanded_groups = vec!["api".to_string()];
        assert!(!is_folded(&snapshot, &stale));
    }

    fn order(agents: &[AgentPane]) -> Vec<String> {
        agents.iter().map(|a| a.pane_id.clone()).collect()
    }

    /// Fixture spanning two projects and two sessions with mixed statuses.
    fn grouping_fixture(now: u64) -> Vec<AgentPane> {
        let mut waiting_old = grouped_agent("mobile", "beta", 1, 10_000, now);
        waiting_old.status = Some(AgentStatus::Waiting);
        let mut working_fresh = grouped_agent("api", "alpha", 2, 5, now);
        working_fresh.status = Some(AgentStatus::Working);
        let mut done = grouped_agent("api", "beta", 3, 100, now);
        done.status = Some(AgentStatus::Done);
        let unknown_old = grouped_agent("mobile", "alpha", 4, 10_000, now);
        let mut working_recent = grouped_agent("api", "alpha", 5, 1, now);
        working_recent.status = Some(AgentStatus::Working);
        vec![
            waiting_old,
            working_fresh,
            done,
            unknown_old,
            working_recent,
        ]
    }

    fn ordered(
        now: u64,
        group_by: Option<SidebarGroupBy>,
        sort: SidebarSort,
        sleeping: &[&str],
        interrupted: &[&str],
    ) -> Vec<String> {
        let mut agents = grouping_fixture(now);
        for a in &mut agents {
            a.window_index = Some(a.pane_id.trim_start_matches('%').parse().unwrap());
        }
        let sleeping: HashSet<String> = sleeping.iter().map(|s| s.to_string()).collect();
        let interrupted: HashSet<String> = interrupted.iter().map(|s| s.to_string()).collect();
        order_agents(
            &mut agents,
            group_by,
            sort,
            now,
            &sleeping,
            &interrupted,
            false,
        );
        order(&agents)
    }

    const NOW: u64 = 1_000_000;

    #[test]
    fn ungrouped_orders_match_each_sort() {
        assert_eq!(
            ordered(NOW, None, SidebarSort::Recency, &[], &[]),
            ["%5", "%2", "%3", "%1", "%4"]
        );
        assert_eq!(
            ordered(NOW, None, SidebarSort::Priority, &[], &[]),
            ["%1", "%3", "%5", "%2", "%4"]
        );
        assert_eq!(
            ordered(NOW, None, SidebarSort::Window, &[], &[]),
            ["%2", "%4", "%5", "%1", "%3"]
        );
    }

    #[test]
    fn project_grouping_keeps_groups_contiguous_and_alphabetical() {
        let by_project = Some(SidebarGroupBy::Project);
        assert_eq!(
            ordered(NOW, by_project, SidebarSort::Recency, &[], &[]),
            ["%5", "%2", "%3", "%1", "%4"]
        );
        assert_eq!(
            ordered(NOW, by_project, SidebarSort::Priority, &[], &[]),
            ["%3", "%5", "%2", "%1", "%4"]
        );
        assert_eq!(
            ordered(NOW, by_project, SidebarSort::Window, &[], &[]),
            ["%2", "%5", "%3", "%4", "%1"]
        );
    }

    #[test]
    fn session_grouping_keeps_groups_contiguous_and_alphabetical() {
        let by_session = Some(SidebarGroupBy::Session);
        assert_eq!(
            ordered(NOW, by_session, SidebarSort::Recency, &[], &[]),
            ["%5", "%2", "%4", "%3", "%1"]
        );
        assert_eq!(
            ordered(NOW, by_session, SidebarSort::Priority, &[], &[]),
            ["%5", "%2", "%4", "%1", "%3"]
        );
        assert_eq!(
            ordered(NOW, by_session, SidebarSort::Window, &[], &[]),
            ["%2", "%4", "%5", "%1", "%3"]
        );
    }

    #[test]
    fn waiting_and_done_stay_actionable_regardless_of_age() {
        // %1 has waited 10000s, %5 worked 1s ago: waiting still leads.
        let order = ordered(NOW, None, SidebarSort::Priority, &[], &[]);
        assert_eq!(order[0], "%1");
        assert_eq!(order[1], "%3");
    }

    #[test]
    fn interrupted_working_demotes_but_waiting_does_not() {
        let order = ordered(NOW, None, SidebarSort::Priority, &[], &["%5"]);
        assert_eq!(order, ["%1", "%3", "%2", "%5", "%4"]);
    }

    #[test]
    fn sleeping_sinks_to_the_bottom_of_its_own_group() {
        // %3 (api) asleep: last within api, never below the mobile group.
        let order = ordered(
            NOW,
            Some(SidebarGroupBy::Project),
            SidebarSort::Priority,
            &["%3"],
            &[],
        );
        assert_eq!(order, ["%5", "%2", "%3", "%1", "%4"]);

        // Ungrouped, the same agent sinks to the bottom of the whole list.
        let order = ordered(NOW, None, SidebarSort::Priority, &["%3"], &[]);
        assert_eq!(order, ["%1", "%5", "%2", "%4", "%3"]);
    }

    #[test]
    fn window_sort_keeps_sleeping_agents_in_place() {
        assert_eq!(
            ordered(NOW, None, SidebarSort::Window, &["%2"], &[]),
            ordered(NOW, None, SidebarSort::Window, &[], &[])
        );
    }

    #[test]
    fn priority_order_is_stable_across_ticks() {
        let first = ordered(
            NOW,
            Some(SidebarGroupBy::Project),
            SidebarSort::Priority,
            &[],
            &[],
        );
        let mut agents = grouping_fixture(NOW);
        let empty = HashSet::new();
        order_agents(
            &mut agents,
            Some(SidebarGroupBy::Project),
            SidebarSort::Priority,
            NOW + 30,
            &empty,
            &empty,
            false,
        );
        assert_eq!(order(&agents), first);
    }

    #[test]
    fn groups_with_no_live_work_sort_below_the_rest() {
        // "alpha" holds one agent that has been quiet for hours, "zed" one
        // that is working, so alphabetical order alone would bury the live one.
        let mut working = grouped_agent("zed", "s", 2, 5, NOW);
        working.status = Some(AgentStatus::Working);
        let quiet = grouped_agent("alpha", "s", 1, 10_000, NOW);
        let empty = HashSet::new();

        let mut agents = vec![quiet.clone(), working.clone()];
        order_agents(
            &mut agents,
            Some(SidebarGroupBy::Project),
            SidebarSort::Priority,
            NOW,
            &empty,
            &empty,
            true,
        );
        assert_eq!(order(&agents), ["%2", "%1"]);

        // Without collapsing, groups stay strictly alphabetical.
        let mut agents = vec![quiet, working];
        order_agents(
            &mut agents,
            Some(SidebarGroupBy::Project),
            SidebarSort::Priority,
            NOW,
            &empty,
            &empty,
            false,
        );
        assert_eq!(order(&agents), ["%1", "%2"]);
    }

    #[test]
    fn group_label_falls_back_when_the_session_is_empty() {
        let mut a = agent("/tmp/api__worktrees/w1");
        a.session = String::new();
        assert_eq!(
            group_label(&a, SidebarGroupBy::Session),
            UNKNOWN_GROUP_LABEL
        );
        assert_eq!(group_label(&a, SidebarGroupBy::Project), "api");
    }

    #[test]
    fn group_order_is_case_insensitive_with_a_deterministic_tiebreak() {
        let mut upper = grouped_agent("api", "API", 1, 5, NOW);
        upper.session = "API".to_string();
        let mut lower = grouped_agent("api", "api", 2, 5, NOW);
        lower.session = "api".to_string();
        let mut zed = grouped_agent("api", "zed", 3, 5, NOW);
        zed.session = "zed".to_string();
        let mut agents = vec![zed, lower, upper];
        let empty = HashSet::new();
        order_agents(
            &mut agents,
            Some(SidebarGroupBy::Session),
            SidebarSort::Recency,
            NOW,
            &empty,
            &empty,
            false,
        );
        assert_eq!(order(&agents), ["%1", "%2", "%3"]);
    }

    #[test]
    fn snapshot_without_group_by_deserializes_as_ungrouped() {
        let snapshot = build_snapshot(SnapshotInputs::default());
        let mut json: serde_json::Value = serde_json::to_value(&snapshot).unwrap();
        json.as_object_mut().unwrap().remove("group_by");
        let restored: SidebarSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(restored.group_by, None);
    }

    #[test]
    fn newly_registered_agent_sorts_first_by_activity() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut registered = agent("/repo/new");
        registered.pane_id = "%2".to_string();
        registered.activity_ts = Some(now);

        let mut done = agent("/repo/done");
        done.pane_id = "%1".to_string();
        done.status = Some(AgentStatus::Done);
        done.status_ts = Some(now - 60);
        done.activity_ts = Some(now - 60);

        let snapshot = build(vec![done, registered], HashMap::new(), HashMap::new());
        assert_eq!(snapshot.agents[0].pane_id, "%2");
        assert!(snapshot.agents[0].status.is_none());
        assert!(snapshot.agents[0].status_ts.is_none());
    }

    #[test]
    fn legacy_statusless_registration_uses_updated_time_for_recency() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut registered = agent("/repo/new");
        registered.pane_id = "%2".to_string();
        registered.updated_ts = Some(now);

        let mut done = agent("/repo/done");
        done.pane_id = "%1".to_string();
        done.status = Some(AgentStatus::Done);
        done.status_ts = Some(now - 60);

        let snapshot = build(vec![done, registered], HashMap::new(), HashMap::new());
        assert_eq!(snapshot.agents[0].pane_id, "%2");
    }

    #[test]
    fn window_index_stamped_from_tmux_state() {
        let agents = vec![agent("/repo")];
        let indexes = HashMap::from([("%1".to_string(), 4u32)]);
        let snapshot = build_snapshot(SnapshotInputs {
            agents,
            pane_window_indexes: indexes,
            ..Default::default()
        });
        assert_eq!(snapshot.agents[0].window_index, Some(4));

        // A pane missing from the lookup clears any stale index.
        let mut stale = agent("/repo");
        stale.window_index = Some(9);
        let snapshot =
            build_with_checks(vec![stale], HashMap::new(), HashMap::new(), HashMap::new());
        assert_eq!(snapshot.agents[0].window_index, None);
    }

    #[test]
    fn window_sort_orders_by_index_not_recency() {
        let mut a = agent("/repo/a");
        a.pane_id = "%1".to_string();
        a.status_ts = Some(1); // oldest — recency sort would put it last
        let mut b = agent("/repo/b");
        b.pane_id = "%2".to_string();
        b.status_ts = Some(999);
        let mut c = agent("/repo/c");
        c.pane_id = "%3".to_string();
        c.status_ts = Some(500);
        let indexes = HashMap::from([("%1".to_string(), 2u32), ("%2".to_string(), 7u32)]);
        let snapshot = build_snapshot(SnapshotInputs {
            agents: vec![b, c, a],
            pane_window_indexes: indexes,
            sort: SidebarSort::Window,
            ..Default::default()
        });
        let order: Vec<_> = snapshot.agents.iter().map(|a| a.pane_id.clone()).collect();
        assert_eq!(order, vec!["%1", "%2", "%3"]); // idx 2, idx 7, unresolved
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
    fn check_statuses_include_main_branch_paths() {
        let path = PathBuf::from("/repo");
        let git = GitStatus {
            branch: Some("main".to_string()),
            base_branch: "main".to_string(),
            ..Default::default()
        };

        let snapshot = build_with_checks(
            vec![agent("/repo")],
            HashMap::from([(path.clone(), git)]),
            HashMap::new(),
            HashMap::from([(path.clone(), check_entry("main"))]),
        );

        assert!(snapshot.check_statuses.contains_key(&path));
    }

    #[test]
    fn check_statuses_exclude_mismatched_branch() {
        let path = PathBuf::from("/repo");
        let git = GitStatus {
            branch: Some("feature-b".to_string()),
            ..Default::default()
        };

        let snapshot = build_with_checks(
            vec![agent("/repo")],
            HashMap::from([(path.clone(), git)]),
            HashMap::new(),
            HashMap::from([(path.clone(), check_entry("feature-a"))]),
        );

        assert!(!snapshot.check_statuses.contains_key(&path));
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
