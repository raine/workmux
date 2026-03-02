//! Keymap definitions for dashboard contexts.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::actions::Action;

/// Context for key handling - determines which keymap is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    DashboardNormal,
    DashboardInput,
    DashboardAdd,
    DiffNormal,
    Patch,
    Comment,
}

/// Map a key event to an action for the given context.
pub fn action_for_key(ctx: Context, key: KeyEvent) -> Option<Action> {
    match ctx {
        Context::DashboardNormal => dashboard_normal_key(key),
        Context::DashboardInput => dashboard_input_key(key),
        Context::DashboardAdd => dashboard_add_key(key),
        Context::DiffNormal => diff_normal_key(key),
        Context::Patch => patch_key(key),
        Context::Comment => comment_key(key),
    }
}

fn dashboard_normal_key(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('?') => Some(Action::ShowHelp),
        KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Action::Quit),
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Action::Next),
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Action::Previous)
        }
        KeyCode::Char('j') | KeyCode::Down => Some(Action::Next),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::Previous),
        KeyCode::Enter => Some(Action::JumpToSelected),
        KeyCode::Tab => Some(Action::JumpToLast),
        KeyCode::Char('p') => Some(Action::PeekSelected),
        KeyCode::Char('s') => Some(Action::CycleSortMode),
        KeyCode::Char('f') => Some(Action::ToggleStaleFilter),
        KeyCode::Char('i') => Some(Action::EnterInputMode),
        KeyCode::Char('a') => Some(Action::EnterAddMode),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Action::ScrollPreviewUp)
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Action::ScrollPreviewDown)
        }
        KeyCode::Char('+') | KeyCode::Char('=') => Some(Action::IncreasePreviewSize),
        KeyCode::Char('-') | KeyCode::Char('_') => Some(Action::DecreasePreviewSize),
        KeyCode::Char('d') => Some(Action::LoadWipDiff),
        KeyCode::Char('c') => Some(Action::SendCommitDashboard),
        KeyCode::Char('m') => Some(Action::TriggerMergeDashboard),
        KeyCode::Char(c @ '1'..='9') => Some(Action::JumpToIndex((c as u8 - b'1') as usize)),
        _ => None,
    }
}

fn dashboard_add_key(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Action::Quit),
        KeyCode::Esc => Some(Action::CancelAddMode),
        KeyCode::Enter => Some(Action::SubmitAddSession),
        KeyCode::Tab | KeyCode::Up | KeyCode::Down => Some(Action::ToggleAddFormField),
        KeyCode::Backspace => Some(Action::DeleteAddChar),
        KeyCode::Char(c) => Some(Action::AppendAddChar(c)),
        _ => None,
    }
}

fn dashboard_input_key(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc => Some(Action::ExitInputMode),
        KeyCode::Enter => Some(Action::SendKey("Enter".to_string())),
        KeyCode::Backspace => Some(Action::SendKey("BSpace".to_string())),
        KeyCode::Tab => Some(Action::SendKey("Tab".to_string())),
        KeyCode::Up => Some(Action::SendKey("Up".to_string())),
        KeyCode::Down => Some(Action::SendKey("Down".to_string())),
        KeyCode::Left => Some(Action::SendKey("Left".to_string())),
        KeyCode::Right => Some(Action::SendKey("Right".to_string())),
        KeyCode::Char(c) => Some(Action::SendKey(c.to_string())),
        _ => None,
    }
}

fn diff_normal_key(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('?') => Some(Action::ShowHelp),
        KeyCode::Esc | KeyCode::Char('q') => Some(Action::CloseDiff),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Action::Quit),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::ScrollDown),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::ScrollUp),
        KeyCode::PageDown => Some(Action::ScrollPageDown),
        KeyCode::PageUp => Some(Action::ScrollPageUp),
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Action::ScrollPageDown)
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Action::ScrollPageUp)
        }
        KeyCode::Tab => Some(Action::ToggleDiffType),
        KeyCode::Char('a') => Some(Action::EnterPatchMode),
        KeyCode::Char('c') => Some(Action::SendCommitDiff),
        KeyCode::Char('m') => Some(Action::TriggerMergeDiff),
        _ => None,
    }
}

fn patch_key(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('?') => Some(Action::ShowHelp),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Action::Quit),
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Action::ScrollPageDown)
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Action::ScrollPageUp)
        }
        KeyCode::Char('y') => Some(Action::StageAndNext),
        KeyCode::Char('n') => Some(Action::SkipHunk),
        KeyCode::Char('u') => Some(Action::UndoStagedHunk),
        KeyCode::Char('s') => Some(Action::SplitHunk),
        KeyCode::Char('o') => Some(Action::StartComment),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::PrevHunk),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::NextHunk),
        KeyCode::Char('c') => Some(Action::SendCommitDiff),
        KeyCode::Char('m') => Some(Action::TriggerMergeDiff),
        KeyCode::Esc | KeyCode::Char('q') => Some(Action::ExitPatchMode),
        _ => None,
    }
}

fn comment_key(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc => Some(Action::CancelComment),
        KeyCode::Enter => Some(Action::SendComment),
        KeyCode::Backspace => Some(Action::DeleteChar),
        KeyCode::Char(c) => Some(Action::AppendChar(c)),
        _ => None,
    }
}

/// Get help rows for a context: (key, description) pairs.
pub fn help_rows(ctx: Context) -> Vec<(&'static str, &'static str)> {
    match ctx {
        Context::DashboardNormal => vec![
            ("?", "Show help"),
            ("q/Esc", "Quit"),
            ("j/k/C-n/C-p", "Navigate up/down"),
            ("Enter", "Jump to agent"),
            ("Tab", "Toggle last agent"),
            ("p", "Peek agent (keep popup)"),
            ("s", "Cycle sort mode"),
            ("f", "Toggle stale filter"),
            ("i", "Enter input mode"),
            ("a", "Create agent"),
            ("Ctrl+u/d", "Scroll preview"),
            ("+/-", "Resize preview"),
            ("d", "View diff"),
            ("c", "Commit changes"),
            ("m", "Merge branch"),
            ("1-9", "Quick jump"),
        ],
        Context::DashboardInput => vec![("Esc", "Exit input mode"), ("<keys>", "Send to agent")],
        Context::DashboardAdd => vec![
            ("Tab", "Switch field"),
            ("<type>", "Edit field"),
            ("Enter", "Create agent"),
            ("Esc", "Cancel"),
            ("Ctrl+c", "Quit"),
        ],
        Context::DiffNormal => vec![
            ("?", "Show help"),
            ("q/Esc", "Close diff"),
            ("j/k", "Scroll line"),
            ("Ctrl+d/u", "Scroll page"),
            ("Tab", "Toggle WIP/Review"),
            ("a", "Enter patch mode (WIP only)"),
            ("c", "Commit changes"),
            ("m", "Merge branch"),
        ],
        Context::Patch => vec![
            ("?", "Show help"),
            ("y", "Stage hunk"),
            ("n", "Skip hunk"),
            ("u", "Undo last staged"),
            ("s", "Split hunk"),
            ("o", "Add comment"),
            ("j/k", "Next/prev hunk"),
            ("Ctrl+d/u", "Scroll hunk"),
            ("c", "Commit changes"),
            ("m", "Merge branch"),
            ("q/Esc", "Exit patch mode"),
        ],
        Context::Comment => vec![
            ("Esc", "Cancel"),
            ("Enter", "Send comment"),
            ("<type>", "Input text"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_each_context_has_help_rows() {
        assert!(!help_rows(Context::DashboardNormal).is_empty());
        assert!(!help_rows(Context::DashboardInput).is_empty());
        assert!(!help_rows(Context::DiffNormal).is_empty());
        assert!(!help_rows(Context::Patch).is_empty());
        assert!(!help_rows(Context::Comment).is_empty());
    }

    #[test]
    fn test_no_duplicate_keys_in_context() {
        for ctx in [
            Context::DashboardNormal,
            Context::DashboardInput,
            Context::DashboardAdd,
            Context::DiffNormal,
            Context::Patch,
            Context::Comment,
        ] {
            let rows = help_rows(ctx);
            let keys: Vec<_> = rows.iter().map(|(k, _)| *k).collect();
            let mut seen = std::collections::HashSet::new();
            for key in &keys {
                assert!(
                    seen.insert(*key),
                    "Duplicate key '{key}' in context {ctx:?}"
                );
            }
        }
    }

    #[test]
    fn test_dashboard_quit_keys() {
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert_eq!(
            action_for_key(Context::DashboardNormal, q),
            Some(Action::Quit)
        );
        assert_eq!(
            action_for_key(Context::DashboardNormal, esc),
            Some(Action::Quit)
        );
        assert_eq!(
            action_for_key(Context::DashboardNormal, ctrl_c),
            Some(Action::Quit)
        );
    }

    #[test]
    fn test_diff_close_keys() {
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        assert_eq!(
            action_for_key(Context::DiffNormal, q),
            Some(Action::CloseDiff)
        );
        assert_eq!(
            action_for_key(Context::DiffNormal, esc),
            Some(Action::CloseDiff)
        );
    }

    #[test]
    fn test_patch_stage_key() {
        let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        assert_eq!(
            action_for_key(Context::Patch, y),
            Some(Action::StageAndNext)
        );
    }

    #[test]
    fn test_dashboard_add_mode_keys() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert_eq!(
            action_for_key(Context::DashboardAdd, esc),
            Some(Action::CancelAddMode)
        );
        assert_eq!(
            action_for_key(Context::DashboardAdd, enter),
            Some(Action::SubmitAddSession)
        );
        assert_eq!(
            action_for_key(Context::DashboardAdd, tab),
            Some(Action::ToggleAddFormField)
        );
        assert_eq!(
            action_for_key(Context::DashboardAdd, backspace),
            Some(Action::DeleteAddChar)
        );
        assert_eq!(
            action_for_key(Context::DashboardAdd, a),
            Some(Action::AppendAddChar('a'))
        );
        assert_eq!(
            action_for_key(Context::DashboardAdd, ctrl_c),
            Some(Action::Quit)
        );
    }

    #[test]
    fn test_dashboard_normal_add_key() {
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(
            action_for_key(Context::DashboardNormal, a),
            Some(Action::EnterAddMode)
        );
    }
}
