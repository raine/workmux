//! Agent navigation for `workmux sidebar next`, `prev` and `jump`.
//!
//! Each host gathers a `NavigationContext`; `select_target` picks the agent
//! the same way on every host.

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};

use super::view::WindowView;

/// Navigation action for sidebar hotkeys.
#[derive(Clone, Copy, Debug)]
pub enum NavAction {
    Next,
    Prev,
    Jump(usize),
}

/// Compute the target index for a navigation action given the current index and list length.
fn compute_nav_target(action: &NavAction, current_idx: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match action {
        NavAction::Next => {
            let i = current_idx.unwrap_or(len - 1);
            if i >= len - 1 { 0 } else { i + 1 }
        }
        NavAction::Prev => {
            let i = current_idx.unwrap_or(0);
            if i == 0 { len - 1 } else { i - 1 }
        }
        NavAction::Jump(n) => {
            let idx = n - 1;
            if idx >= len {
                return None;
            }
            idx
        }
    })
}

fn current_listed_window_pane<'a>(
    panes: &'a [&str],
    current_pane_id: &'a str,
    current_window_id: &str,
    pane_window_ids: &HashMap<String, String>,
) -> Option<&'a str> {
    if panes.contains(&current_pane_id) {
        return Some(current_pane_id);
    }

    panes.iter().copied().find(|pane_id| {
        pane_window_ids
            .get(*pane_id)
            .is_some_and(|window_id| window_id == current_window_id)
    })
}

fn navigation_anchor_pane<'a>(
    panes: &'a [&str],
    current_pane_id: &'a str,
    current_window_id: &str,
    pane_window_ids: &HashMap<String, String>,
) -> Option<&'a str> {
    current_listed_window_pane(panes, current_pane_id, current_window_id, pane_window_ids)
        .or(Some(current_pane_id).filter(|pane_id| panes.contains(pane_id)))
}

/// What navigation knows about the caller and the published agents.
#[derive(Debug, Default)]
pub(super) struct NavigationContext {
    /// Agent panes in sidebar order, as the daemon published them.
    pub agents: Vec<String>,
    /// The view the caller's window shows, if a view rule applies to it.
    pub view: Option<WindowView>,
    pub current_pane: String,
    pub current_window: String,
    /// The window of each live pane.
    pub pane_windows: HashMap<String, String>,
    /// The session of each live pane, in the form `session_filter` uses.
    pub pane_sessions: HashMap<String, String>,
    /// The caller's session when the sidebar filter shows only it.
    pub session_filter: Option<String>,
}

/// The agent pane a navigation action moves to.
///
/// Under the session filter, an agent whose session is unknown is skipped:
/// it cannot be shown to be in the caller's session.
pub(super) fn select_target(context: &NavigationContext, action: &NavAction) -> Result<String> {
    let mut panes: Vec<&str> = context.agents.iter().map(String::as_str).collect();
    if panes.is_empty() {
        bail!("no sidebar agents found (is the sidebar running?)");
    }

    if let Some(view) = &context.view {
        panes.retain(|pane| view.pane_ids.contains(*pane));
    }

    if let Some(current_session) = &context.session_filter {
        panes.retain(|pane| {
            context
                .pane_sessions
                .get(*pane)
                .is_some_and(|session| session == current_session)
        });
    }

    if panes.is_empty() {
        bail!("no matching agents found");
    }

    let current_idx = navigation_anchor_pane(
        &panes,
        &context.current_pane,
        &context.current_window,
        &context.pane_windows,
    )
    .and_then(|pane_id| panes.iter().position(|&pid| pid == pane_id));

    let len = panes.len();
    let target_idx = match action {
        NavAction::Jump(n) => compute_nav_target(action, current_idx, len)
            .ok_or_else(|| anyhow!("agent {} out of range (1-{})", n, len))?,
        _ => compute_nav_target(action, current_idx, len)
            .expect("len > 0 guarantees a result for Next/Prev"),
    };
    Ok(panes[target_idx].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_wraps_from_last_to_first() {
        assert_eq!(compute_nav_target(&NavAction::Next, Some(2), 3), Some(0));
    }

    #[test]
    fn next_advances_normally() {
        assert_eq!(compute_nav_target(&NavAction::Next, Some(0), 3), Some(1));
        assert_eq!(compute_nav_target(&NavAction::Next, Some(1), 3), Some(2));
    }

    #[test]
    fn next_without_current_wraps_from_last() {
        // No current window match: starts from last, wraps to first
        assert_eq!(compute_nav_target(&NavAction::Next, None, 3), Some(0));
    }

    #[test]
    fn prev_wraps_from_first_to_last() {
        assert_eq!(compute_nav_target(&NavAction::Prev, Some(0), 3), Some(2));
    }

    #[test]
    fn prev_goes_back_normally() {
        assert_eq!(compute_nav_target(&NavAction::Prev, Some(2), 3), Some(1));
        assert_eq!(compute_nav_target(&NavAction::Prev, Some(1), 3), Some(0));
    }

    #[test]
    fn prev_without_current_wraps_to_last() {
        // No current window match: starts from 0, wraps to last
        assert_eq!(compute_nav_target(&NavAction::Prev, None, 3), Some(2));
    }

    #[test]
    fn jump_converts_1_indexed_to_0_indexed() {
        assert_eq!(compute_nav_target(&NavAction::Jump(1), None, 3), Some(0));
        assert_eq!(compute_nav_target(&NavAction::Jump(2), None, 3), Some(1));
        assert_eq!(compute_nav_target(&NavAction::Jump(3), None, 3), Some(2));
    }

    #[test]
    fn jump_out_of_range_returns_none() {
        assert_eq!(compute_nav_target(&NavAction::Jump(4), None, 3), None);
        assert_eq!(compute_nav_target(&NavAction::Jump(10), None, 3), None);
    }

    #[test]
    fn empty_list_returns_none() {
        assert_eq!(compute_nav_target(&NavAction::Next, None, 0), None);
        assert_eq!(compute_nav_target(&NavAction::Prev, None, 0), None);
        assert_eq!(compute_nav_target(&NavAction::Jump(1), None, 0), None);
    }

    #[test]
    fn single_agent_next_stays() {
        assert_eq!(compute_nav_target(&NavAction::Next, Some(0), 1), Some(0));
    }

    #[test]
    fn single_agent_prev_stays() {
        assert_eq!(compute_nav_target(&NavAction::Prev, Some(0), 1), Some(0));
    }

    #[test]
    fn current_window_falls_back_to_listed_agent_pane() {
        let panes = vec!["%1", "%2"];
        let pane_window_ids = HashMap::from([
            ("%1".to_string(), "@10".to_string()),
            ("%2".to_string(), "@20".to_string()),
        ]);

        assert_eq!(
            current_listed_window_pane(&panes, "%99", "@20", &pane_window_ids),
            Some("%2")
        );
    }

    #[test]
    fn current_listed_pane_wins_over_window_fallback() {
        let panes = vec!["%1", "%2"];
        let pane_window_ids = HashMap::from([
            ("%1".to_string(), "@10".to_string()),
            ("%2".to_string(), "@10".to_string()),
        ]);

        assert_eq!(
            current_listed_window_pane(&panes, "%2", "@10", &pane_window_ids),
            Some("%2")
        );
    }

    #[test]
    fn navigation_anchor_uses_agent_in_current_window() {
        let panes = vec!["%1", "%2"];
        let pane_window_ids = HashMap::from([
            ("%sidebar".to_string(), "@10".to_string()),
            ("%1".to_string(), "@10".to_string()),
            ("%2".to_string(), "@20".to_string()),
        ]);

        assert_eq!(
            navigation_anchor_pane(&panes, "%sidebar", "@10", &pane_window_ids),
            Some("%1")
        );
    }

    #[test]
    fn navigation_anchor_prefers_current_agent_pane() {
        let panes = vec!["%1", "%2"];
        let pane_window_ids = HashMap::from([
            ("%1".to_string(), "@10".to_string()),
            ("%2".to_string(), "@10".to_string()),
        ]);

        assert_eq!(
            navigation_anchor_pane(&panes, "%2", "@10", &pane_window_ids),
            Some("%2")
        );
    }

    #[test]
    fn next_from_sidebar_in_filtered_two_window_session_advances_to_other_window() {
        let panes = vec!["%0", "%317"];
        let pane_window_ids = HashMap::from([
            ("%sidebar".to_string(), "@0".to_string()),
            ("%0".to_string(), "@0".to_string()),
            ("%317".to_string(), "@33".to_string()),
        ]);

        let current_idx = navigation_anchor_pane(&panes, "%sidebar", "@0", &pane_window_ids)
            .and_then(|pane_id| panes.iter().position(|&pid| pid == pane_id));

        assert_eq!(current_idx, Some(0));
        assert_eq!(
            compute_nav_target(&NavAction::Next, current_idx, panes.len()),
            Some(1)
        );
    }

    #[test]
    fn next_from_agent_in_filtered_two_window_session_advances_to_other_window() {
        let panes = vec!["%0", "%317"];
        let pane_window_ids = HashMap::from([
            ("%0".to_string(), "@0".to_string()),
            ("%317".to_string(), "@33".to_string()),
        ]);

        let current_idx = navigation_anchor_pane(&panes, "%0", "@0", &pane_window_ids)
            .and_then(|pane_id| panes.iter().position(|&pid| pid == pane_id));

        assert_eq!(current_idx, Some(0));
        assert_eq!(
            compute_nav_target(&NavAction::Next, current_idx, panes.len()),
            Some(1)
        );
    }

    /// One layout seen through each host's identifiers: two sessions, the
    /// caller in an agent-less window of the first, a view hiding nothing,
    /// and one agent whose session is unknown.
    struct Fixture {
        sidebar: &'static str,
        window: &'static str,
        /// (pane, window, session); `None` session means unknown.
        agents: [(&'static str, &'static str, Option<&'static str>); 4],
        session: &'static str,
    }

    const TMUX: Fixture = Fixture {
        sidebar: "%9",
        window: "@1",
        agents: [
            ("%1", "@1", Some("main")),
            ("%2", "@2", Some("main")),
            ("%3", "@3", Some("other")),
            ("%4", "@4", None),
        ],
        session: "main",
    };

    const ZELLIJ: Fixture = Fixture {
        sidebar: "terminal_9",
        window: "tab:1",
        agents: [
            ("terminal_1", "tab:1", Some("work")),
            ("terminal_2", "tab:2", Some("work")),
            ("terminal_3", "tab:3", Some("other")),
            ("terminal_4", "tab:4", None),
        ],
        session: "work",
    };

    const REX: Fixture = Fixture {
        sidebar: "block-9",
        window: "window-1",
        agents: [
            ("block-1", "window-1", Some("s-1")),
            ("block-2", "window-2", Some("s-1")),
            ("block-3", "window-3", Some("s-2")),
            ("block-4", "window-4", None),
        ],
        session: "s-1",
    };

    fn context(fixture: &Fixture, filtered: bool) -> NavigationContext {
        let mut pane_windows =
            HashMap::from([(fixture.sidebar.to_string(), fixture.window.to_string())]);
        let mut pane_sessions =
            HashMap::from([(fixture.sidebar.to_string(), fixture.session.to_string())]);
        for (pane, window, session) in fixture.agents {
            pane_windows.insert(pane.to_string(), window.to_string());
            if let Some(session) = session {
                pane_sessions.insert(pane.to_string(), session.to_string());
            }
        }
        NavigationContext {
            agents: fixture
                .agents
                .iter()
                .map(|(pane, ..)| pane.to_string())
                .collect(),
            view: None,
            current_pane: fixture.sidebar.to_string(),
            current_window: fixture.window.to_string(),
            pane_windows,
            pane_sessions,
            session_filter: filtered.then(|| fixture.session.to_string()),
        }
    }

    /// The index of the target among the fixture's agents.
    fn pick(fixture: &Fixture, filtered: bool, action: NavAction) -> Result<usize> {
        let target = select_target(&context(fixture, filtered), &action)?;
        Ok(fixture
            .agents
            .iter()
            .position(|(pane, ..)| *pane == target)
            .unwrap())
    }

    #[test]
    fn every_host_picks_the_same_target() {
        let cases = [
            (false, NavAction::Next, Some(1)),
            (false, NavAction::Prev, Some(3)),
            (false, NavAction::Jump(3), Some(2)),
            (false, NavAction::Jump(4), Some(3)),
            (false, NavAction::Jump(5), None),
            (true, NavAction::Next, Some(1)),
            (true, NavAction::Prev, Some(1)),
            (true, NavAction::Jump(2), Some(1)),
            // The unknown-session agent is not a target under the filter.
            (true, NavAction::Jump(3), None),
        ];
        for fixture in [&TMUX, &ZELLIJ, &REX] {
            for (filtered, action, expected) in cases {
                let picked = pick(fixture, filtered, action).ok();
                assert_eq!(
                    picked, expected,
                    "{} {action:?} filtered={filtered}",
                    fixture.sidebar
                );
            }
        }
    }

    #[test]
    fn view_limits_targets_to_its_panes() {
        for fixture in [&TMUX, &ZELLIJ, &REX] {
            let mut context = context(fixture, false);
            context.view = Some(WindowView {
                name: "Work".to_string(),
                pane_ids: [fixture.agents[0].0, fixture.agents[2].0]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            });
            let target = select_target(&context, &NavAction::Next).unwrap();
            assert_eq!(target, fixture.agents[2].0);
        }
    }

    #[test]
    fn empty_or_filtered_out_lists_explain_why() {
        let mut context = context(&TMUX, false);
        context.agents.clear();
        let error = select_target(&context, &NavAction::Next).unwrap_err();
        assert!(error.to_string().contains("is the sidebar running"));

        let mut context = self::context(&TMUX, true);
        context.session_filter = Some("elsewhere".to_string());
        let error = select_target(&context, &NavAction::Next).unwrap_err();
        assert_eq!(error.to_string(), "no matching agents found");
    }
}
