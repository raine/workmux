//! Persistent tip state for promoting features to users.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::state::store::get_state_dir;

/// Guide linked from the remote servers tip and notice.
pub const REMOTE_GUIDE_URL: &str = "https://workmux.dev/guide/remote/";

/// Dashboard opens that show the remote servers tip before it retires.
const REMOTE_TIP_MAX_VIEWS: u32 = 5;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TipsState {
    #[serde(default)]
    pub sidebar_tip_views: u32,
    #[serde(default)]
    pub sidebar_used: bool,
    #[serde(default)]
    pub remote_tip_views: u32,
    /// The user hid or followed a remote servers hint, or has configured a
    /// remote. Silences both the dashboard tip and the CLI notice.
    #[serde(default)]
    pub remote_tip_dismissed: bool,
    #[serde(default)]
    pub remote_notice_shown: bool,
}

fn tips_path() -> Option<PathBuf> {
    get_state_dir().ok().map(|dir| dir.join("tips.json"))
}

fn load_tips() -> TipsState {
    let Some(path) = tips_path() else {
        return TipsState::default();
    };
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_tips(state: &TipsState) {
    let Some(path) = tips_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = serde_json::to_string(state).map(|json| fs::write(&path, json));
}

/// Check whether to show the sidebar tip and increment the view counter.
///
/// Returns true when all conditions are met:
/// - TMUX env var is set (user is inside tmux)
/// - sidebar has not been used before
/// - tip has been shown fewer than 5 times
pub fn should_show_sidebar_tip() -> bool {
    should_show_sidebar_tip_for_tmux(std::env::var("TMUX").is_ok())
}

fn should_show_sidebar_tip_for_tmux(in_tmux: bool) -> bool {
    if !in_tmux {
        return false;
    }
    let mut state = load_tips();
    if state.sidebar_used || state.sidebar_tip_views >= 5 {
        return false;
    }
    state.sidebar_tip_views += 1;
    save_tips(&state);
    true
}

/// Mark the sidebar feature as used so the tip is never shown again.
pub fn mark_sidebar_used() {
    let mut state = load_tips();
    state.sidebar_used = true;
    save_tips(&state);
}

/// Apply `f` to the stored tips state, saving only when it changed.
fn update_tips<T>(f: impl FnOnce(&mut TipsState) -> T) -> T {
    let mut state = load_tips();
    let before = state.clone();
    let result = f(&mut state);
    if state != before {
        save_tips(&state);
    }
    result
}

/// Users with a configured remote already know the feature.
fn note_remotes(state: &mut TipsState, remotes_configured: bool) {
    if remotes_configured {
        state.remote_tip_dismissed = true;
    }
}

fn remote_tip_is_due(state: &mut TipsState, remotes_configured: bool) -> bool {
    note_remotes(state, remotes_configured);
    !state.remote_tip_dismissed && state.remote_tip_views < REMOTE_TIP_MAX_VIEWS
}

fn take_remote_notice(state: &mut TipsState, remotes_configured: bool) -> bool {
    note_remotes(state, remotes_configured);
    if state.remote_tip_dismissed || state.remote_notice_shown {
        return false;
    }
    state.remote_notice_shown = true;
    true
}

/// Check whether the dashboard may show the remote servers tip. Only a
/// dashboard session that draws it uses up a view; see
/// [`record_remote_tip_view`].
pub fn remote_tip_due(remotes_configured: bool) -> bool {
    update_tips(|state| remote_tip_is_due(state, remotes_configured))
}

/// Count a dashboard session that drew the remote servers tip.
pub fn record_remote_tip_view() {
    update_tips(|state| state.remote_tip_views += 1);
}

/// Check whether to print the one-time remote servers notice, recording it
/// as shown when it is due.
pub fn take_remote_notice_once(remotes_configured: bool) -> bool {
    update_tips(|state| take_remote_notice(state, remotes_configured))
}

/// Silence the remote servers tip and notice for good.
pub fn dismiss_remote_tip() {
    update_tips(|state| state.remote_tip_dismissed = true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tips_state_defaults() {
        let state = TipsState::default();
        assert_eq!(state.sidebar_tip_views, 0);
        assert!(!state.sidebar_used);
    }

    #[test]
    fn tips_state_roundtrip() {
        let state = TipsState {
            sidebar_tip_views: 3,
            sidebar_used: true,
            ..TipsState::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: TipsState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.sidebar_tip_views, 3);
        assert!(parsed.sidebar_used);
    }

    #[test]
    fn tips_state_deserializes_empty_object() {
        let parsed: TipsState = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.sidebar_tip_views, 0);
        assert!(!parsed.sidebar_used);
    }

    #[test]
    fn should_show_requires_tmux() {
        assert!(!should_show_sidebar_tip_for_tmux(false));
    }

    #[test]
    fn remote_tip_retires_after_max_drawn_views() {
        let mut state = TipsState::default();
        for _ in 0..REMOTE_TIP_MAX_VIEWS {
            assert!(remote_tip_is_due(&mut state, false));
            state.remote_tip_views += 1;
        }
        assert!(!remote_tip_is_due(&mut state, false));
    }

    #[test]
    fn checking_the_tip_does_not_use_a_view() {
        let mut state = TipsState::default();
        for _ in 0..REMOTE_TIP_MAX_VIEWS + 3 {
            assert!(remote_tip_is_due(&mut state, false));
        }
        assert_eq!(state.remote_tip_views, 0);
    }

    #[test]
    fn remote_notice_is_taken_once() {
        let mut state = TipsState::default();
        assert!(take_remote_notice(&mut state, false));
        assert!(!take_remote_notice(&mut state, false));
    }

    #[test]
    fn showing_the_notice_leaves_the_dashboard_tip() {
        let mut state = TipsState::default();
        assert!(take_remote_notice(&mut state, false));
        assert!(remote_tip_is_due(&mut state, false));
    }

    #[test]
    fn dismissing_the_tip_silences_the_notice() {
        let mut state = TipsState {
            remote_tip_dismissed: true,
            ..TipsState::default()
        };
        assert!(!remote_tip_is_due(&mut state, false));
        assert!(!take_remote_notice(&mut state, false));
        assert!(!state.remote_notice_shown);
    }

    #[test]
    fn configured_remote_silences_both_for_good() {
        let mut state = TipsState::default();
        assert!(!remote_tip_is_due(&mut state, true));
        assert!(!take_remote_notice(&mut state, true));
        assert!(state.remote_tip_dismissed);
        assert_eq!(state.remote_tip_views, 0);

        // Removing the remote later does not bring the hints back.
        assert!(!remote_tip_is_due(&mut state, false));
        assert!(!take_remote_notice(&mut state, false));
    }

    #[test]
    fn older_tips_file_gains_remote_defaults() {
        let parsed: TipsState =
            serde_json::from_str(r#"{"sidebar_tip_views":2,"sidebar_used":false}"#).unwrap();
        assert_eq!(parsed.remote_tip_views, 0);
        assert!(!parsed.remote_tip_dismissed);
        assert!(!parsed.remote_notice_shown);
    }
}
