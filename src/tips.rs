//! Persistent tip state for promoting features to users.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::state::store::get_state_dir;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TipsState {
    #[serde(default)]
    pub sidebar_tip_views: u32,
    #[serde(default)]
    pub sidebar_used: bool,
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
}
