//! Action enum and dispatcher for dashboard key handling.

use super::app::{App, ViewMode};
use super::diff_ops::DiffOps;

/// All possible actions in the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    // Global actions
    ShowHelp,
    Quit,

    // Dashboard navigation
    Next,
    Previous,
    JumpToSelected,
    JumpToIndex(usize),
    JumpToLast,
    PeekSelected,

    // Dashboard commands
    CycleSortMode,
    ToggleStaleFilter,
    EnterInputMode,
    ExitInputMode,
    EnterAddMode,
    CancelAddMode,
    ToggleAddFormField,
    SubmitAddSession,
    DeleteAddChar,
    AppendAddChar(char),
    ScrollPreviewUp,
    ScrollPreviewDown,
    IncreasePreviewSize,
    DecreasePreviewSize,
    LoadWipDiff,
    SendCommitDashboard,
    TriggerMergeDashboard,

    // Input mode
    SendKey(String),

    // Diff view navigation
    CloseDiff,
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ToggleDiffType,
    EnterPatchMode,
    SendCommitDiff,
    TriggerMergeDiff,

    // Patch mode
    StageAndNext,
    SkipHunk,
    UndoStagedHunk,
    SplitHunk,
    StartComment,
    PrevHunk,
    NextHunk,
    ExitPatchMode,

    // Comment input
    CancelComment,
    SendComment,
    DeleteChar,
    AppendChar(char),
}

/// Apply an action to the app state.
/// Returns true if preview should be refreshed immediately.
pub fn apply_action(app: &mut App, action: Action) -> bool {
    match action {
        // Global
        Action::ShowHelp => {
            app.show_help = true;
            false
        }
        Action::Quit => {
            app.should_quit = true;
            false
        }

        // Dashboard navigation
        Action::Next => {
            app.next();
            false
        }
        Action::Previous => {
            app.previous();
            false
        }
        Action::JumpToSelected => {
            app.jump_to_selected();
            false
        }
        Action::JumpToIndex(idx) => {
            app.jump_to_index(idx);
            false
        }
        Action::JumpToLast => {
            app.jump_to_last();
            false
        }
        Action::PeekSelected => {
            app.peek_selected();
            false
        }

        // Dashboard commands
        Action::CycleSortMode => {
            app.cycle_sort_mode();
            false
        }
        Action::ToggleStaleFilter => {
            app.toggle_stale_filter();
            false
        }
        Action::EnterInputMode => {
            if app.table_state.selected().is_some() && !app.agents.is_empty() {
                app.input_mode = true;
            }
            false
        }
        Action::ExitInputMode => {
            app.input_mode = false;
            false
        }
        Action::EnterAddMode => {
            app.enter_add_mode();
            false
        }
        Action::CancelAddMode => {
            app.cancel_add_mode();
            false
        }
        Action::ToggleAddFormField => {
            app.toggle_add_form_field();
            false
        }
        Action::SubmitAddSession => {
            app.submit_add_session();
            false
        }
        Action::DeleteAddChar => {
            app.delete_add_form_char();
            false
        }
        Action::AppendAddChar(c) => {
            app.append_add_form_char(c);
            false
        }
        Action::ScrollPreviewUp => {
            app.scroll_preview_up(app.preview_height, app.preview_line_count);
            false
        }
        Action::ScrollPreviewDown => {
            app.scroll_preview_down(app.preview_height, app.preview_line_count);
            false
        }
        Action::IncreasePreviewSize => {
            app.increase_preview_size();
            false
        }
        Action::DecreasePreviewSize => {
            app.decrease_preview_size();
            false
        }
        Action::LoadWipDiff => {
            app.load_diff(false);
            false
        }
        Action::SendCommitDashboard => {
            app.send_commit_to_selected();
            false
        }
        Action::TriggerMergeDashboard => {
            app.trigger_merge_for_selected();
            false
        }

        // Input mode
        Action::SendKey(key) => {
            app.send_key_to_selected(&key);
            app.refresh_preview();
            true // Signal that preview was refreshed
        }

        // Diff view
        Action::CloseDiff => {
            app.close_diff();
            false
        }
        Action::ScrollUp => {
            if let ViewMode::Diff(ref mut diff) = app.view_mode {
                diff.scroll_up();
            }
            false
        }
        Action::ScrollDown => {
            if let ViewMode::Diff(ref mut diff) = app.view_mode {
                diff.scroll_down();
            }
            false
        }
        Action::ScrollPageUp => {
            if let ViewMode::Diff(ref mut diff) = app.view_mode {
                diff.scroll_page_up();
            }
            false
        }
        Action::ScrollPageDown => {
            if let ViewMode::Diff(ref mut diff) = app.view_mode {
                diff.scroll_page_down();
            }
            false
        }
        Action::ToggleDiffType => {
            let is_branch_diff = if let ViewMode::Diff(ref diff) = app.view_mode {
                diff.is_branch_diff
            } else {
                false
            };
            app.load_diff(!is_branch_diff);
            false
        }
        Action::EnterPatchMode => {
            app.enter_patch_mode();
            false
        }
        Action::SendCommitDiff => {
            app.send_commit_to_agent();
            false
        }
        Action::TriggerMergeDiff => {
            app.trigger_merge();
            false
        }

        // Patch mode
        Action::StageAndNext => {
            app.stage_and_next();
            false
        }
        Action::SkipHunk => {
            app.skip_hunk();
            false
        }
        Action::UndoStagedHunk => {
            app.undo_staged_hunk();
            false
        }
        Action::SplitHunk => {
            app.split_current_hunk();
            false
        }
        Action::StartComment => {
            if let ViewMode::Diff(ref mut diff) = app.view_mode {
                diff.comment_input = Some(String::new());
            }
            false
        }
        Action::PrevHunk => {
            app.prev_hunk();
            false
        }
        Action::NextHunk => {
            let _ = app.next_hunk();
            false
        }
        Action::ExitPatchMode => {
            app.exit_patch_mode();
            false
        }

        // Comment input
        Action::CancelComment => {
            if let ViewMode::Diff(ref mut diff) = app.view_mode {
                diff.comment_input = None;
            }
            false
        }
        Action::SendComment => {
            app.send_hunk_comment();
            false
        }
        Action::DeleteChar => {
            if let ViewMode::Diff(ref mut diff) = app.view_mode
                && let Some(ref mut input) = diff.comment_input
            {
                input.pop();
            }
            false
        }
        Action::AppendChar(c) => {
            if let ViewMode::Diff(ref mut diff) = app.view_mode
                && let Some(ref mut input) = diff.comment_input
            {
                input.push(c);
            }
            false
        }
    }
}
