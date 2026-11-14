//! TUI rendering logic for the dashboard.

mod dashboard;
mod diff;
mod format;
mod help;
mod popup;
pub mod theme;
pub mod worktree;

use ratatui::Frame;
use ratatui::style::{Modifier, Style};

use super::app::{App, ViewMode};

/// Dim all cells in the buffer to create a backdrop effect behind modals.
fn dim_buffer(f: &mut Frame) {
    let area = f.area();
    let buf = f.buffer_mut();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(Style::default().add_modifier(Modifier::DIM));
            }
        }
    }
}

pub use self::dashboard::render_dashboard;
pub use self::diff::render_diff_view;
pub use self::help::{
    render_add_worktree, render_base_picker, render_command_palette, render_confirm_kill,
    render_confirm_remove, render_help, render_project_picker, render_row_context, render_sweep,
    render_sweep_progress,
};

/// Main UI entry point - renders the appropriate view based on app state.
pub fn ui(f: &mut Frame, app: &mut App) {
    app.terminal_area = f.area();

    // Render either dashboard or diff view based on view mode
    match &mut app.view_mode {
        ViewMode::Dashboard => render_dashboard(f, app),
        ViewMode::Diff(diff_view) => render_diff_view(f, diff_view, &app.palette),
    }

    // Render overlays on top
    let has_modal = app.sweep_progress.is_some()
        || app.show_help
        || app.pending_kill_pane_id.is_some()
        || app.pending_remove.is_some()
        || app.pending_base_picker.is_some()
        || app.pending_project_picker.is_some()
        || app.pending_sweep.is_some()
        || app.pending_add_worktree.is_some()
        || app.pending_command_palette.is_some()
        || app.pending_row_context.is_some();

    if has_modal {
        dim_buffer(f);
    }

    if app.sweep_progress.is_some() {
        render_sweep_progress(f, app);
    } else if app.show_help {
        render_help(f, app);
    } else if app.pending_kill_pane_id.is_some() {
        render_confirm_kill(f, app);
    } else if app.pending_remove.is_some() {
        render_confirm_remove(f, app);
    } else if app.pending_row_context.is_some() {
        render_row_context(f, app);
    } else if app.pending_command_palette.is_some() {
        render_command_palette(f, app);
    } else if app.pending_base_picker.is_some() {
        render_base_picker(f, app);
    } else if app.pending_project_picker.is_some() {
        render_project_picker(f, app);
    } else if app.pending_sweep.is_some() {
        render_sweep(f, app);
    } else if app.pending_add_worktree.is_some() {
        render_add_worktree(f, app);
    }
}
