//! Help overlay rendering.

use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, Row, Table},
    Frame,
};

use super::super::app::{App, ViewMode};
use super::super::keymap::{help_rows, Context};

/// Determine the current keymap context for help display.
fn get_help_context(app: &App) -> Context {
    match &app.view_mode {
        ViewMode::Dashboard => {
            if app.add_form.is_some() {
                Context::DashboardAdd
            } else if app.input_mode {
                Context::DashboardInput
            } else {
                Context::DashboardNormal
            }
        }
        ViewMode::Diff(diff) => {
            if diff.patch_mode {
                if diff.comment_input.is_some() {
                    Context::Comment
                } else {
                    Context::Patch
                }
            } else {
                Context::DiffNormal
            }
        }
    }
}

/// Get the title for the help overlay based on context.
fn context_title(ctx: Context) -> &'static str {
    match ctx {
        Context::DashboardNormal => "Dashboard",
        Context::DashboardInput => "Input Mode",
        Context::DashboardAdd => "Create Agent",
        Context::DiffNormal => "Diff View",
        Context::Patch => "Patch Mode",
        Context::Comment => "Comment",
    }
}

/// Render the help overlay.
pub fn render_help(f: &mut Frame, app: &App) {
    let ctx = get_help_context(app);
    let title = context_title(ctx);
    let keybindings = help_rows(ctx);

    // Calculate dimensions based on content
    let row_count = keybindings.len() as u16;
    let height = row_count + 5; // +5 for borders, padding, and empty line at top
    let width = 44;

    // Center the popup
    let area = f.area();
    let popup_area = Rect {
        x: area.width.saturating_sub(width) / 2,
        y: area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    };

    let palette = &app.palette;

    // Create styled block with rounded corners
    let block = Block::bordered()
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(palette.help_border))
        .title(Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(
                title,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default()),
        ]))
        .title_bottom(Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled("any key", Style::default().fg(palette.dimmed)),
            Span::styled(" to close ", Style::default().fg(palette.help_muted)),
        ]));

    // Build styled rows with empty line at top for padding
    let mut rows: Vec<Row> = vec![Row::new(vec![Cell::from(""), Cell::from("")])];
    rows.extend(keybindings.into_iter().map(|(key, desc)| {
        Row::new(vec![
            Cell::from(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(
                    format!("{:>8}", key),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            Cell::from(Line::from(vec![
                Span::styled(" · ", Style::default().fg(palette.help_muted)),
                Span::styled(desc, Style::default().fg(palette.text)),
            ])),
        ])
    }));

    let table = Table::new(rows, [Constraint::Length(10), Constraint::Min(25)])
        .block(block)
        .column_spacing(0);

    f.render_widget(Clear, popup_area);
    f.render_widget(table, popup_area);
}
