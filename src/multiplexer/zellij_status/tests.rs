use super::*;

#[test]
fn spinner_is_prefixed_like_zellij_pane_titles() {
    let name = tab_name_with_spinner("wm-feature", '⠙');

    assert!(name.starts_with("\u{2063}⠙\u{2064} "));
    assert_eq!(tab_name_without_status(&name), Some("wm-feature"));
}

#[test]
fn static_status_replaces_spinner_without_losing_base_name() {
    let working = tab_name_with_spinner("wm-feature", '⠙');
    let waiting = tab_name_with_status(&working, "💬");

    assert_eq!(tab_name_without_status(&waiting), Some("wm-feature"));
    assert!(waiting.contains("💬"));
    assert!(!waiting.contains('⠙'));
}

#[test]
fn manual_status_like_text_is_not_owned_by_workmux() {
    assert_eq!(tab_name_without_status("⠙ wm-feature"), None);
    assert_eq!(tab_name_without_status("wm-feature ✅"), None);
}

#[test]
fn animation_state_is_scoped_to_the_tab() {
    assert_eq!(
        tab_key("dev", 7),
        PaneKey {
            backend: "zellij".to_string(),
            instance: "dev".to_string(),
            pane_id: "tab_7".to_string(),
        }
    );
}

#[test]
fn empty_base_name_can_be_recovered_from_spinner() {
    let name = tab_name_with_spinner("", '⠙');

    assert_eq!(tab_name_without_status(&name), Some(""));
}

#[test]
fn spinner_process_identity_requires_command_and_token() {
    assert!(is_expected_spinner_command(
        "/usr/bin/workmux _zellij-status-spinner --token 123-456",
        "123-456"
    ));
    assert!(!is_expected_spinner_command(
        "/usr/bin/sleep 100",
        "123-456"
    ));
    assert!(!is_expected_spinner_command(
        "/usr/bin/workmux _zellij-status-spinner --token other",
        "123-456"
    ));
}

#[test]
fn external_tab_rename_becomes_the_new_spinner_base() {
    assert_eq!(
        synchronized_base_name(
            "renamed-tab",
            &tab_name_with_spinner("old-tab", '⠙'),
            "old-tab"
        ),
        "renamed-tab"
    );
}

#[test]
fn own_animation_frame_keeps_the_stored_base() {
    let expected = tab_name_with_spinner("wm-feature", '⠙');
    assert_eq!(
        synchronized_base_name(&expected, &expected, "wm-feature"),
        "wm-feature"
    );
}
