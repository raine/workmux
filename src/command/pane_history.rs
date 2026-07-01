/// Return the pane to remember as the previous location after a successful switch.
///
/// We track any distinct source pane, not just agent panes, so pane-level hotkeys
/// can round-trip cleanly between ordinary tmux panes and agent panes.
pub fn pane_to_remember<'a>(
    current_pane: Option<&'a str>,
    target_pane_id: &str,
) -> Option<&'a str> {
    current_pane.filter(|pane_id| *pane_id != target_pane_id)
}

#[cfg(test)]
mod tests {
    use super::pane_to_remember;

    #[test]
    fn remembers_non_agent_source_pane() {
        assert_eq!(pane_to_remember(Some("%42"), "%7"), Some("%42"));
    }

    #[test]
    fn ignores_same_pane_switch() {
        assert_eq!(pane_to_remember(Some("%7"), "%7"), None);
    }

    #[test]
    fn ignores_missing_current_pane() {
        assert_eq!(pane_to_remember(None, "%7"), None);
    }
}
