//! Pure helper functions for agent data extraction and formatting.

// Re-export shared display helpers so existing `agent::extract_*` paths keep working.
pub use crate::agent_display::{extract_project_name, extract_worktree_name};

/// Get elapsed seconds since the status timestamp.
pub fn elapsed_secs(status_ts: Option<u64>, now_secs: u64) -> Option<u64> {
    status_ts.map(|ts| now_secs.saturating_sub(ts))
}

/// Format a duration in seconds as HH:MM:SS.
pub fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;
    format!("{:02}:{:02}:{:02}", hours, mins, secs)
}

/// Format an age in seconds as a compact relative string (e.g., "2h", "3d", "1w").
pub fn format_age(secs: u64) -> String {
    crate::util::format_compact_age(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_elapsed_secs() {
        assert_eq!(elapsed_secs(Some(100), 200), Some(100));
        assert_eq!(elapsed_secs(None, 200), None);
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0), "00:00:00");
        assert_eq!(format_duration(61), "00:01:01");
        assert_eq!(format_duration(3661), "01:01:01");
    }
}
