use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use unicode_width::UnicodeWidthStr;

use super::actions::{Action, apply_action};
use super::app::{
    App, DashboardTab, MouseRowClick, MouseRowId, PaletteCommand, RowContextKind, RowContextMenu,
    ViewMode,
};
use super::layout::{
    FOOTER_HELP_WIDTH, REMOTE_TIP_GUIDE_COLUMNS, REMOTE_TIP_HIDE_COLUMNS, dashboard_content_areas,
    dashboard_outer_areas, remote_tip_area,
};
use crate::config::DashboardLayout;

const TABLE_HEADER_HEIGHT: u16 = 1;
const SCROLL_LINES: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
enum MouseTarget {
    Tab(DashboardTab),
    Row(usize),
    Preview,
    Diff,
    DiffFile(usize),
    Action(Action),
    None,
}

#[derive(Debug, Clone)]
struct FooterItem {
    text: String,
    action: Option<Action>,
}

impl FooterItem {
    fn action(text: impl Into<String>, action: Action) -> Self {
        Self {
            text: text.into(),
            action: Some(action),
        }
    }

    fn label(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            action: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseInput {
    Activate,
    Context,
    ScrollUp,
    ScrollDown,
    Ignore,
}

fn route_mouse(kind: MouseEventKind) -> MouseInput {
    match kind {
        MouseEventKind::Down(MouseButton::Left) => MouseInput::Activate,
        MouseEventKind::Down(MouseButton::Right) => MouseInput::Context,
        MouseEventKind::ScrollUp => MouseInput::ScrollUp,
        MouseEventKind::ScrollDown => MouseInput::ScrollDown,
        _ => MouseInput::Ignore,
    }
}

pub(super) fn handle_mouse_event(app: &mut App, mouse: MouseEvent) -> bool {
    if let Some(result) = handle_row_context_mouse(app, mouse) {
        return result;
    }
    if has_modal(app) {
        return false;
    }

    let target = target_at(app, mouse.column, mouse.row);
    match route_mouse(mouse.kind) {
        MouseInput::Activate => activate_target(app, target),
        MouseInput::Context => open_row_context(app, target, mouse.column, mouse.row),
        MouseInput::ScrollUp => scroll_target(app, target, false),
        MouseInput::ScrollDown => scroll_target(app, target, true),
        MouseInput::Ignore => false,
    }
}

fn handle_row_context_mouse(app: &mut App, mouse: MouseEvent) -> Option<bool> {
    let menu = app.pending_row_context.as_ref()?;
    let item = menu.item_at(app.terminal_area, mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(index) = item else {
                app.pending_row_context = None;
                return Some(false);
            };
            let action = if let Some(mut menu) = app.pending_row_context.take() {
                menu.cursor = index;
                menu.selected_action()
            } else {
                None
            };
            Some(action.is_some_and(|action| apply_action(app, action)))
        }
        MouseEventKind::Down(MouseButton::Right) if item.is_none() => {
            app.pending_row_context = None;
            None
        }
        MouseEventKind::ScrollDown => {
            if let Some(menu) = &mut app.pending_row_context {
                menu.move_next();
            }
            Some(false)
        }
        MouseEventKind::ScrollUp => {
            if let Some(menu) = &mut app.pending_row_context {
                menu.move_previous();
            }
            Some(false)
        }
        _ => Some(false),
    }
}

fn open_row_context(app: &mut App, target: MouseTarget, column: u16, row: u16) -> bool {
    let MouseTarget::Row(index) = target else {
        return false;
    };
    let (kind, commands) = match app.active_tab {
        DashboardTab::Agents => {
            app.select_agent(index);
            (RowContextKind::Agent, agent_context_commands())
        }
        DashboardTab::Worktrees => {
            app.select_worktree(index);
            (RowContextKind::Worktree, worktree_context_commands())
        }
    };
    app.last_mouse_row_click = None;
    app.pending_row_context = Some(RowContextMenu {
        kind,
        commands,
        cursor: 0,
        column,
        row,
    });
    false
}

fn agent_context_commands() -> Vec<PaletteCommand> {
    vec![
        PaletteCommand {
            label: "Jump to agent",
            key_hint: "Enter",
            action: Action::JumpToSelected,
        },
        PaletteCommand {
            label: "Peek agent",
            key_hint: "p",
            action: Action::PeekSelected,
        },
        PaletteCommand {
            label: "View diff",
            key_hint: "d",
            action: Action::LoadWipDiff,
        },
        PaletteCommand {
            label: "Enter input mode",
            key_hint: "i",
            action: Action::EnterInputMode,
        },
        PaletteCommand {
            label: "Open PR",
            key_hint: "o",
            action: Action::OpenPr,
        },
        PaletteCommand {
            label: "Open PR checks",
            key_hint: "O",
            action: Action::OpenPrChecks,
        },
        PaletteCommand {
            label: "Change base branch",
            key_hint: "b",
            action: Action::ShowBaseBranchPicker,
        },
        PaletteCommand {
            label: "Remove worktree",
            key_hint: "r",
            action: Action::RemoveSelectedWorktree,
        },
        PaletteCommand {
            label: "Kill agent",
            key_hint: "X",
            action: Action::KillSelected,
        },
    ]
}

fn worktree_context_commands() -> Vec<PaletteCommand> {
    vec![
        PaletteCommand {
            label: "Jump to worktree",
            key_hint: "Enter",
            action: Action::JumpToSelectedWorktree,
        },
        PaletteCommand {
            label: "Open PR",
            key_hint: "o",
            action: Action::OpenPr,
        },
        PaletteCommand {
            label: "Open PR checks",
            key_hint: "O",
            action: Action::OpenPrChecks,
        },
        PaletteCommand {
            label: "Change base branch",
            key_hint: "b",
            action: Action::ShowBaseBranchPicker,
        },
        PaletteCommand {
            label: "Close mux window",
            key_hint: "c",
            action: Action::CloseSelectedWorktreeWindow,
        },
        PaletteCommand {
            label: "Remove worktree",
            key_hint: "r",
            action: Action::RemoveSelectedWorktree,
        },
    ]
}

fn has_modal(app: &App) -> bool {
    app.sweep_progress.is_some()
        || app.show_help
        || app.pending_kill_pane_id.is_some()
        || app.pending_remove.is_some()
        || app.pending_base_picker.is_some()
        || app.pending_project_picker.is_some()
        || app.pending_sweep.is_some()
        || app.pending_add_worktree.is_some()
        || app.pending_command_palette.is_some()
}

fn activate_target(app: &mut App, target: MouseTarget) -> bool {
    match target {
        MouseTarget::Tab(tab) if tab != app.active_tab => {
            app.switch_tab();
            false
        }
        MouseTarget::Row(index) => {
            let Some(id) = row_id(app, index) else {
                return false;
            };
            let double_click =
                register_row_click(&mut app.last_mouse_row_click, id, Instant::now());
            match app.active_tab {
                DashboardTab::Agents => {
                    app.select_agent(index);
                    if double_click {
                        return apply_action(app, Action::JumpToSelected);
                    }
                }
                DashboardTab::Worktrees => {
                    app.select_worktree(index);
                    if double_click {
                        return apply_action(app, Action::JumpToSelectedWorktree);
                    }
                }
            }
            false
        }
        MouseTarget::DiffFile(index) => {
            if let ViewMode::Diff(diff) = &mut app.view_mode
                && let Some(file) = diff.file_list.get(index)
            {
                if diff.patch_mode {
                    if let Some(hunk) = diff
                        .hunks
                        .iter()
                        .position(|hunk| hunk.filename == file.filename)
                    {
                        diff.current_hunk = hunk;
                        diff.scroll = 0;
                    }
                } else {
                    diff.scroll = file.start_line;
                }
            }
            false
        }
        MouseTarget::Action(action) => apply_action(app, action),
        _ => false,
    }
}

fn row_id(app: &App, index: usize) -> Option<MouseRowId> {
    match app.active_tab {
        DashboardTab::Agents => app
            .agents
            .get(index)
            .map(|agent| MouseRowId::Agent(agent.pane_id.clone())),
        DashboardTab::Worktrees => app
            .worktrees
            .get(index)
            .map(|worktree| MouseRowId::Worktree(worktree.path.clone())),
    }
}

fn register_row_click(previous: &mut Option<MouseRowClick>, id: MouseRowId, now: Instant) -> bool {
    let double_click = previous.as_ref().is_some_and(|click| {
        click.id == id && now.duration_since(click.at) <= Duration::from_millis(500)
    });
    *previous = (!double_click).then_some(MouseRowClick { id, at: now });
    double_click
}

fn scrolled_index(selected: Option<usize>, count: usize, down: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let selected = selected.unwrap_or(0).min(count - 1);
    Some(if down {
        selected.saturating_add(1).min(count - 1)
    } else {
        selected.saturating_sub(1)
    })
}

fn scroll_target(app: &mut App, target: MouseTarget, down: bool) -> bool {
    match target {
        MouseTarget::Preview => {
            app.scroll_preview_lines(if down {
                SCROLL_LINES as isize
            } else {
                -(SCROLL_LINES as isize)
            });
            false
        }
        MouseTarget::Diff | MouseTarget::DiffFile(_) => {
            scroll_diff(app, down);
            false
        }
        MouseTarget::Row(_) | MouseTarget::None | MouseTarget::Tab(_) => {
            match app.active_tab {
                DashboardTab::Agents => {
                    if let Some(index) =
                        scrolled_index(app.table_state.selected(), app.agents.len(), down)
                    {
                        app.select_agent(index);
                    }
                }
                DashboardTab::Worktrees => {
                    if let Some(index) = scrolled_index(
                        app.worktree_table_state.selected(),
                        app.worktrees.len(),
                        down,
                    ) {
                        app.select_worktree(index);
                    }
                }
            }
            false
        }
        MouseTarget::Action(_) => false,
    }
}

fn scroll_diff(app: &mut App, down: bool) {
    let ViewMode::Diff(diff) = &mut app.view_mode else {
        return;
    };
    let total_lines = if diff.patch_mode {
        diff.hunks
            .get(diff.current_hunk)
            .map(|hunk| hunk.parsed_lines.len())
            .unwrap_or(0)
    } else {
        diff.line_count
    };
    if down {
        let max_scroll = total_lines.saturating_sub(diff.viewport_height as usize);
        diff.scroll = (diff.scroll + SCROLL_LINES).min(max_scroll);
    } else {
        diff.scroll = diff.scroll.saturating_sub(SCROLL_LINES);
    }
}

fn target_at(app: &App, column: u16, row: u16) -> MouseTarget {
    match &app.view_mode {
        ViewMode::Dashboard => dashboard_target_at(app, column, row),
        ViewMode::Diff(diff) => diff_target_at(app.terminal_area, diff, column, row),
    }
}

fn dashboard_target_at(app: &App, column: u16, row: u16) -> MouseTarget {
    let area = app.terminal_area;
    if !contains(area, column, row) {
        return MouseTarget::None;
    }

    let outer = dashboard_outer_areas(area);
    if row == outer[2].y {
        return dashboard_footer_target(app, column);
    }

    let offset = match app.active_tab {
        DashboardTab::Agents => app.table_state.offset(),
        DashboardTab::Worktrees => app.worktree_table_state.offset(),
    };
    let count = match app.active_tab {
        DashboardTab::Agents => app.agents.len(),
        DashboardTab::Worktrees => app.worktrees.len(),
    };
    dashboard_surface_target(
        area,
        app.mux.supports_preview(),
        app.preview_size,
        app.config.dashboard.layout.unwrap_or_default(),
        offset,
        count,
        (column, row),
    )
}

fn dashboard_surface_target(
    area: Rect,
    supports_preview: bool,
    preview_size: u8,
    layout: DashboardLayout,
    offset: usize,
    count: usize,
    position: (u16, u16),
) -> MouseTarget {
    let (column, row) = position;
    let outer = dashboard_outer_areas(area);
    if row == outer[0].y {
        if (area.x + 2..area.x + 8).contains(&column) {
            return MouseTarget::Tab(DashboardTab::Agents);
        }
        if (area.x + 11..area.x + 20).contains(&column) {
            return MouseTarget::Tab(DashboardTab::Worktrees);
        }
    }

    let (table_area, preview_area) =
        dashboard_content_areas(outer[1], supports_preview, preview_size, layout);
    if contains(table_area, column, row) {
        let body_y = table_area.y.saturating_add(TABLE_HEADER_HEIGHT);
        if row >= body_y {
            let index = offset + usize::from(row - body_y);
            if index < count {
                return MouseTarget::Row(index);
            }
        }
        return MouseTarget::None;
    }
    if preview_area.is_some_and(|preview| contains(preview, column, row)) {
        return MouseTarget::Preview;
    }

    MouseTarget::None
}

fn dashboard_footer_target(app: &App, column: u16) -> MouseTarget {
    if column >= app.terminal_area.right().saturating_sub(FOOTER_HELP_WIDTH) {
        return MouseTarget::Action(Action::ShowHelp);
    }
    if app.status_message.is_some() {
        return MouseTarget::None;
    }

    match app.active_tab {
        DashboardTab::Agents if app.filter_active => {
            return filter_footer_target(
                app.terminal_area.x,
                column,
                UnicodeWidthStr::width(app.filter_text.as_str()),
            );
        }
        DashboardTab::Agents if app.input_mode => {
            return input_footer_target(
                app.terminal_area.x,
                column,
                &app.config.dashboard.input_exit_key().to_string(),
            );
        }
        DashboardTab::Worktrees if app.worktree_filter_active => {
            return filter_footer_target(
                app.terminal_area.x,
                column,
                UnicodeWidthStr::width(app.worktree_filter_text.as_str()),
            );
        }
        _ => {}
    }

    let items = match app.active_tab {
        DashboardTab::Agents => agent_footer_items(app),
        DashboardTab::Worktrees => worktree_footer_items(app),
    };
    if let Some(tip) = visible_remote_tip_area(app)
        && let Some(target) = remote_tip_target(tip, column)
    {
        return target;
    }
    footer_target_at(app.terminal_area.x + 2, column, &items)
}

/// Where the remote servers tip is on screen, if the agents footer shows it:
/// the footer must be in its normal mode and leave room after the key hints.
pub(super) fn visible_remote_tip_area(app: &App) -> Option<Rect> {
    if !app.show_remote_tip
        || app.active_tab != DashboardTab::Agents
        || app.filter_active
        || app.input_mode
        || app.status_message.is_some()
    {
        return None;
    }
    let footer = dashboard_outer_areas(app.terminal_area)[2];
    remote_tip_area(footer, footer_hints_width(&agent_footer_items(app)))
}

fn remote_tip_target(tip: Rect, column: u16) -> Option<MouseTarget> {
    if !(tip.x..tip.right()).contains(&column) {
        return None;
    }
    let offset = column - tip.x;
    Some(if REMOTE_TIP_GUIDE_COLUMNS.contains(&offset) {
        MouseTarget::Action(Action::OpenRemoteGuide)
    } else if REMOTE_TIP_HIDE_COLUMNS.contains(&offset) {
        MouseTarget::Action(Action::DismissRemoteTip)
    } else {
        MouseTarget::None
    })
}

/// Width of the rendered key hints: two leading spaces, then the items
/// separated by ` │ `.
fn footer_hints_width(items: &[FooterItem]) -> u16 {
    let text: usize = items.iter().map(|item| item.text.width()).sum();
    (2 + text + 3 * items.len().saturating_sub(1)) as u16
}

fn filter_footer_target(start: u16, column: u16, filter_width: usize) -> MouseTarget {
    let accept = start.saturating_add(6 + filter_width as u16);
    let clear = accept.saturating_add(14);
    if (accept..accept.saturating_add(12)).contains(&column) {
        MouseTarget::Action(Action::AcceptFilter)
    } else if (clear..clear.saturating_add(9)).contains(&column) {
        MouseTarget::Action(Action::ClearFilter)
    } else {
        MouseTarget::None
    }
}

fn input_footer_target(start: u16, column: u16, exit_key: &str) -> MouseTarget {
    const EXIT_START: u16 = 43;
    if (start + EXIT_START..start + EXIT_START + UnicodeWidthStr::width(exit_key) as u16 + 5)
        .contains(&column)
    {
        MouseTarget::Action(Action::ExitInputMode)
    } else {
        MouseTarget::None
    }
}

fn agent_footer_items(app: &App) -> Vec<FooterItem> {
    let stale = if app.hide_stale { "hidden" } else { "shown" };
    let mut items = vec![
        FooterItem::action("i Input", Action::EnterInputMode),
        FooterItem::action("d Diff", Action::LoadWipDiff),
        FooterItem::action("o PR", Action::OpenPr),
        FooterItem::label("1-9 Jump"),
        FooterItem::action(
            format!("s Sort ({})", app.sort_mode.label()),
            Action::CycleSortMode,
        ),
        FooterItem::action(
            format!("F Scope ({})", app.scope_mode.label()),
            Action::ToggleScopeFilter,
        ),
        FooterItem::action(format!("f Stale ({stale})"), Action::ToggleStaleFilter),
    ];
    if !app.filter_text.is_empty() {
        items.push(FooterItem::action(
            format!("/ {}", app.filter_text),
            Action::EnterFilterMode,
        ));
    }
    items.extend([
        FooterItem::action("Tab Worktrees", Action::SwitchTab),
        FooterItem::action("q Quit", Action::Quit),
    ]);
    items
}

fn worktree_footer_items(app: &App) -> Vec<FooterItem> {
    let mut items = vec![
        FooterItem::action("a Add", Action::AddWorktree),
        FooterItem::action("r Remove", Action::RemoveSelectedWorktree),
        FooterItem::action("R Sweep", Action::StartSweep),
        FooterItem::action("c Close", Action::CloseSelectedWorktreeWindow),
        FooterItem::action("o PR", Action::OpenPr),
        FooterItem::label("1-9 Jump"),
        FooterItem::action(
            format!("s Sort ({})", app.worktree_sort_mode.label()),
            Action::CycleWorktreeSortMode,
        ),
        FooterItem::action("p Project", Action::ShowProjectPicker),
    ];
    if !app.worktree_filter_text.is_empty() {
        items.push(FooterItem::action(
            format!("/ {}", app.worktree_filter_text),
            Action::EnterFilterMode,
        ));
    }
    items.extend([
        FooterItem::action("Tab Agents", Action::SwitchTab),
        FooterItem::action("q Quit", Action::Quit),
    ]);
    items
}

fn diff_target_at(area: Rect, diff: &super::diff::DiffView, column: u16, row: u16) -> MouseTarget {
    if !contains(area, column, row) {
        return MouseTarget::None;
    }
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    if row == chunks[1].y {
        return footer_target_at(area.x + 2, column, &diff_footer_items(diff));
    }
    if diff.file_list.is_empty() {
        return MouseTarget::Diff;
    }
    let content =
        Layout::horizontal([Constraint::Min(40), Constraint::Percentage(25)]).split(chunks[0]);
    if contains(content[1], column, row) {
        let first_row = content[1].y.saturating_add(1);
        if row >= first_row {
            let index = usize::from(row - first_row);
            if index < diff.file_list.len() {
                return MouseTarget::DiffFile(index);
            }
        }
        return MouseTarget::DiffFile(diff.file_list.len());
    }
    MouseTarget::Diff
}

fn diff_footer_items(diff: &super::diff::DiffView) -> Vec<FooterItem> {
    if diff.patch_mode {
        if diff.comment_input.is_some() {
            return vec![
                FooterItem::action("Enter Send", Action::SendComment),
                FooterItem::action("Esc Cancel", Action::CancelComment),
            ];
        }
        let mut items = vec![
            FooterItem::action("y Stage", Action::StageAndNext),
            FooterItem::action("n Skip", Action::SkipHunk),
        ];
        if !diff.staged_hunks.is_empty() {
            items.push(FooterItem::action("u Undo", Action::UndoStagedHunk));
        }
        items.extend([
            FooterItem::action("s Split", Action::SplitHunk),
            FooterItem::action("o Comment", Action::StartComment),
            FooterItem::label("j/k Nav"),
            FooterItem::action("q Quit", Action::ExitPatchMode),
        ]);
        return items;
    }

    let mut items = vec![FooterItem::action(
        "Tab WIP | review",
        Action::ToggleDiffType,
    )];
    if !diff.is_branch_diff && (diff.lines_added > 0 || diff.lines_removed > 0) {
        items.push(FooterItem::action("a Patch", Action::EnterPatchMode));
    }
    items.extend([
        FooterItem::label("j/k Scroll"),
        FooterItem::action("c Commit", Action::SendCommitDiff),
        FooterItem::action("m Merge", Action::TriggerMergeDiff),
        FooterItem::action("q Close", Action::CloseDiff),
    ]);
    items
}

fn footer_target_at(start: u16, column: u16, items: &[FooterItem]) -> MouseTarget {
    let mut x = start;
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            x = x.saturating_add(3);
        }
        let end = x.saturating_add(item.text.width() as u16);
        if (x..end).contains(&column) {
            return item
                .action
                .clone()
                .map(MouseTarget::Action)
                .unwrap_or(MouseTarget::None);
        }
        x = end;
    }
    MouseTarget::None
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::dashboard::diff::{DiffView, FileEntry};
    use std::path::PathBuf;

    fn diff_view() -> DiffView {
        DiffView {
            content: String::new(),
            parsed_lines: Vec::new(),
            scroll: 0,
            line_count: 0,
            viewport_height: 20,
            title: "Diff".into(),
            worktree_path: PathBuf::new(),
            pane_id: String::new(),
            is_branch_diff: false,
            lines_added: 0,
            lines_removed: 0,
            patch_mode: false,
            hunks: Vec::new(),
            current_hunk: 0,
            hunks_total: 0,
            hunks_processed: 0,
            staged_hunks: Vec::new(),
            comment_input: None,
            file_list: Vec::new(),
            remote: None,
        }
    }

    #[test]
    fn row_context_menu_stays_on_screen_and_hit_tests_items() {
        let terminal = Rect::new(0, 0, 80, 24);
        let menu = RowContextMenu {
            kind: RowContextKind::Worktree,
            commands: worktree_context_commands(),
            cursor: 0,
            column: 79,
            row: 23,
        };
        let area = menu.area(terminal);

        assert!(area.right() <= terminal.right());
        assert!(area.bottom() <= terminal.bottom());
        assert_eq!(menu.item_at(terminal, area.x + 1, area.y + 1), Some(0));
        assert_eq!(menu.item_at(terminal, area.x, area.y), None);
    }

    #[test]
    fn row_context_commands_are_specific_to_each_list() {
        let agent = agent_context_commands();
        let worktree = worktree_context_commands();

        assert!(
            agent
                .iter()
                .any(|command| command.action == Action::KillSelected)
        );
        assert!(
            !worktree
                .iter()
                .any(|command| command.action == Action::KillSelected)
        );
        assert!(
            worktree
                .iter()
                .any(|command| { command.action == Action::CloseSelectedWorktreeWindow })
        );
    }

    #[test]
    fn repeated_row_clicks_activate_within_double_click_window() {
        let mut previous = None;
        let now = Instant::now();
        let id = MouseRowId::Agent("%1".into());

        assert!(!register_row_click(&mut previous, id.clone(), now));
        assert!(register_row_click(
            &mut previous,
            id,
            now + Duration::from_millis(400)
        ));
        assert!(previous.is_none());
    }

    #[test]
    fn row_wheel_scrolling_stops_at_list_boundaries() {
        assert_eq!(scrolled_index(Some(0), 3, false), Some(0));
        assert_eq!(scrolled_index(Some(2), 3, true), Some(2));
        assert_eq!(scrolled_index(Some(1), 3, false), Some(0));
        assert_eq!(scrolled_index(Some(1), 3, true), Some(2));
        assert_eq!(scrolled_index(None, 0, true), None);
    }

    #[test]
    fn mouse_events_route_by_kind() {
        assert_eq!(
            route_mouse(MouseEventKind::Down(MouseButton::Left)),
            MouseInput::Activate
        );
        assert_eq!(route_mouse(MouseEventKind::ScrollUp), MouseInput::ScrollUp);
        assert_eq!(
            route_mouse(MouseEventKind::Down(MouseButton::Right)),
            MouseInput::Context
        );
    }

    #[test]
    fn dashboard_tabs_and_visible_rows_are_hit_tested() {
        let area = Rect::new(0, 0, 100, 24);

        assert_eq!(
            dashboard_surface_target(area, true, 40, DashboardLayout::Stacked, 7, 20, (3, 0)),
            MouseTarget::Tab(DashboardTab::Agents)
        );
        assert_eq!(
            dashboard_surface_target(area, true, 40, DashboardLayout::Stacked, 7, 20, (12, 0)),
            MouseTarget::Tab(DashboardTab::Worktrees)
        );
        assert_eq!(
            dashboard_surface_target(area, true, 40, DashboardLayout::Stacked, 7, 20, (4, 3)),
            MouseTarget::Row(7)
        );
    }

    #[test]
    fn dashboard_preview_is_a_distinct_scroll_target() {
        let area = Rect::new(0, 0, 100, 24);

        assert_eq!(
            dashboard_surface_target(area, true, 50, DashboardLayout::Stacked, 0, 10, (20, 18)),
            MouseTarget::Preview
        );
    }

    #[test]
    fn side_by_side_preview_and_rows_are_hit_tested() {
        let area = Rect::new(0, 0, 100, 24);
        assert_eq!(
            dashboard_surface_target(area, true, 60, DashboardLayout::SideBySide, 7, 30, (20, 3)),
            MouseTarget::Preview
        );
        assert_eq!(
            dashboard_surface_target(area, true, 60, DashboardLayout::SideBySide, 7, 30, (70, 3)),
            MouseTarget::Row(7)
        );
        assert_eq!(
            dashboard_surface_target(area, true, 60, DashboardLayout::SideBySide, 7, 30, (70, 20)),
            MouseTarget::Row(24)
        );
        assert_eq!(
            dashboard_surface_target(area, false, 60, DashboardLayout::SideBySide, 7, 30, (20, 3)),
            MouseTarget::Row(7)
        );
    }

    #[test]
    fn filter_footer_routes_accept_and_clear_controls() {
        assert_eq!(
            filter_footer_target(0, 10, 4),
            MouseTarget::Action(Action::AcceptFilter)
        );
        assert_eq!(
            filter_footer_target(0, 25, 4),
            MouseTarget::Action(Action::ClearFilter)
        );
    }

    #[test]
    fn footer_targets_actions_and_ignores_labels() {
        let items = vec![
            FooterItem::action("a Add", Action::AddWorktree),
            FooterItem::label("1-9 Jump"),
            FooterItem::action("q Quit", Action::Quit),
        ];

        assert_eq!(
            footer_target_at(2, 3, &items),
            MouseTarget::Action(Action::AddWorktree)
        );
        assert_eq!(footer_target_at(2, 12, &items), MouseTarget::None);
        assert_eq!(
            footer_target_at(2, 22, &items),
            MouseTarget::Action(Action::Quit)
        );
    }

    use crate::command::dashboard::layout::REMOTE_TIP_WIDTH;

    fn agent_items() -> Vec<FooterItem> {
        vec![
            FooterItem::action("i Input", Action::EnterInputMode),
            FooterItem::action("q Quit", Action::Quit),
        ]
    }

    #[test]
    fn footer_hints_width_counts_leading_space_and_separators() {
        assert_eq!(footer_hints_width(&agent_items()), 2 + 7 + 3 + 6);
        assert_eq!(footer_hints_width(&[]), 2);
    }

    #[test]
    fn remote_tip_keys_are_hit_tested() {
        let footer = Rect::new(0, 23, 100, 1);
        let tip = remote_tip_area(footer, footer_hints_width(&agent_items())).unwrap();
        let tip_x = 100 - FOOTER_HELP_WIDTH - REMOTE_TIP_WIDTH;
        assert_eq!(tip.x, tip_x);
        let target = |offset: u16| remote_tip_target(tip, tip_x + offset);

        assert_eq!(target(0), Some(MouseTarget::None));
        assert_eq!(
            target(REMOTE_TIP_GUIDE_COLUMNS.start),
            Some(MouseTarget::Action(Action::OpenRemoteGuide))
        );
        assert_eq!(
            target(REMOTE_TIP_GUIDE_COLUMNS.end - 1),
            Some(MouseTarget::Action(Action::OpenRemoteGuide))
        );
        assert_eq!(
            target(REMOTE_TIP_HIDE_COLUMNS.start),
            Some(MouseTarget::Action(Action::DismissRemoteTip))
        );
        assert_eq!(remote_tip_target(tip, tip_x - 1), None);
        assert_eq!(remote_tip_target(tip, tip.right()), None);
    }

    #[test]
    fn input_footer_exit_target_tracks_key_label_width() {
        for label in ["Esc", "Ctrl+q"] {
            let start = 10;
            let first = start + 43;
            let end = first + UnicodeWidthStr::width(label) as u16 + 5;
            assert_eq!(
                input_footer_target(start, first - 1, label),
                MouseTarget::None
            );
            for column in first..end {
                assert_eq!(
                    input_footer_target(start, column, label),
                    MouseTarget::Action(Action::ExitInputMode)
                );
            }
            assert_eq!(input_footer_target(start, end, label), MouseTarget::None);
        }
    }

    #[test]
    fn diff_file_rows_are_hit_tested_inside_sidebar() {
        let mut diff = diff_view();
        diff.file_list = vec![
            FileEntry {
                filename: "src/one.rs".into(),
                start_line: 4,
                lines_added: 1,
                lines_removed: 0,
                is_new: false,
            },
            FileEntry {
                filename: "src/two.rs".into(),
                start_line: 12,
                lines_added: 2,
                lines_removed: 1,
                is_new: false,
            },
        ];
        let area = Rect::new(0, 0, 100, 24);

        assert_eq!(diff_target_at(area, &diff, 90, 1), MouseTarget::DiffFile(0));
        assert_eq!(diff_target_at(area, &diff, 90, 2), MouseTarget::DiffFile(1));
        assert_eq!(diff_target_at(area, &diff, 20, 2), MouseTarget::Diff);
    }

    #[test]
    fn diff_footer_routes_visible_controls() {
        let diff = diff_view();
        let items = diff_footer_items(&diff);

        assert_eq!(
            footer_target_at(2, 3, &items),
            MouseTarget::Action(Action::ToggleDiffType)
        );
    }
}
