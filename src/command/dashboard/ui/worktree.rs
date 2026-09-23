//! Worktree table rendering for the dashboard worktree view.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Cell, Paragraph, Row, Table},
};

use crate::config::WorktreeColumn;

use super::super::agent;
use super::super::app::App;
use super::format;
use super::format::{
    AgentStatusFormat, ResourceHeaderCell, format_agent_status_summary, format_git_status,
    format_pr_status, truncate,
};
use super::theme::ThemePalette;

/// Render the worktree table in the given area.
pub fn render_worktree_table(f: &mut Frame, app: &mut App, area: Rect) {
    // Don't render headers for an empty table - avoids a visual blink
    // as column widths jump when data arrives on the next frame
    if app.worktrees.is_empty() {
        return;
    }

    let show_check_counts = app.config.dashboard.show_check_counts();

    let worktree_max_width = worktree_width_budget(area.width);

    // Show the GitHub column when at least one worktree has a PR or checks.
    let show_pr_column = app.worktrees.iter().any(|worktree| {
        worktree.pr_info.is_some() || app.get_checks_for_worktree(worktree).is_some()
    });

    let columns = visible_columns(&app.config.dashboard.worktree_columns(), show_pr_column);
    let show_pr_column = columns.contains(&WorktreeColumn::Pr);

    // Pre-compute row data
    let row_data: Vec<_> = app
        .worktrees
        .iter()
        .enumerate()
        .map(|(idx, wt)| {
            let jump_key = if idx < 9 {
                format!("{}", idx + 1)
            } else {
                String::new()
            };

            let project = agent::extract_project_name(&wt.path);

            // Main worktree: show branch name (handle is just the repo dir name)
            // Other worktrees: show branch inline when it differs from the handle
            let worktree_display = if wt.is_main {
                wt.branch.clone()
            } else if wt.branch != wt.handle {
                format!("{} \u{2192} {}", wt.handle, wt.branch)
            } else {
                wt.handle.clone()
            };
            let worktree_display = truncate(&worktree_display, worktree_max_width);

            // Git status
            let git_status = app.git_statuses.get(&wt.path);
            let git_spans = format_git_status(git_status, app.spinner_frame, &app.palette);

            // PR status (only computed if column is shown)
            let pr_spans = if show_pr_column {
                format_pr_status(
                    wt.pr_info.as_ref(),
                    app.get_checks_for_worktree(wt),
                    show_check_counts,
                    app.spinner_frame,
                    &app.palette,
                )
            } else {
                Vec::new()
            };

            // Agent status summary
            let agent_spans = format_agent_status_summary(
                wt.agent_status.as_ref(),
                &app.config.status_icons,
                app.spinner_frame,
                &app.palette,
                AgentStatusFormat::TableCell,
            );

            let is_current = app.current_worktree.as_ref().is_some_and(|cwd| {
                if let (Ok(cwd_canonical), Ok(wt_canonical)) =
                    (cwd.canonicalize(), wt.path.canonicalize())
                {
                    cwd_canonical == wt_canonical
                } else {
                    wt.path == *cwd
                }
            });

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let age = wt
                .created_at
                .map(|ts| agent::format_age(now.saturating_sub(ts)));

            WorktreeRowData {
                jump_key,
                project,
                worktree_display,
                is_main: wt.is_main,
                is_current,
                git_spans,
                pr_spans,
                agent_line: format::spans_to_line(agent_spans),
                has_mux_window: wt.has_mux_window,
                age: age.unwrap_or_default(),
            }
        })
        .collect();

    let table = build_worktree_table(
        &columns,
        row_data,
        worktree_max_width,
        format::ResourceHeaderState {
            palette: &app.palette,
            spinner_frame: app.spinner_frame,
            git_fetching: app
                .is_git_fetching
                .load(std::sync::atomic::Ordering::Relaxed),
            pr_fetching: app.is_pr_fetching(),
        },
    );
    f.render_stateful_widget(table, area, &mut app.worktree_table_state);
}

struct WorktreeRowData {
    jump_key: String,
    project: String,
    worktree_display: String,
    is_main: bool,
    is_current: bool,
    git_spans: Vec<(String, Style)>,
    pr_spans: Vec<(String, Style)>,
    agent_line: Line<'static>,
    has_mux_window: bool,
    age: String,
}

/// Hide PR until GitHub status is available, unless it is the only configured
/// column and hiding it would blank the table.
fn visible_columns(configured: &[WorktreeColumn], show_pr_column: bool) -> Vec<WorktreeColumn> {
    let visible: Vec<_> = configured
        .iter()
        .copied()
        .filter(|column| *column != WorktreeColumn::Pr || show_pr_column)
        .collect();
    if visible.is_empty() {
        configured.to_vec()
    } else {
        visible
    }
}

fn worktree_cell(
    column: WorktreeColumn,
    row: &WorktreeRowData,
    palette: &ThemePalette,
) -> Cell<'static> {
    match column {
        WorktreeColumn::Number => {
            Cell::from(row.jump_key.clone()).style(Style::default().fg(palette.keycap))
        }
        WorktreeColumn::Project => Cell::from(row.project.clone()),
        WorktreeColumn::Worktree => Cell::from(row.worktree_display.clone())
            .style(format::make_row_style(row.is_current, row.is_main, palette)),
        WorktreeColumn::Git => Cell::from(format::spans_to_line(row.git_spans.clone())),
        WorktreeColumn::Pr => Cell::from(format::spans_to_line(row.pr_spans.clone())),
        WorktreeColumn::Mux => {
            if row.has_mux_window {
                Cell::from("\u{25cf}").style(Style::default().fg(palette.success))
            } else {
                Cell::from("-").style(Style::default().fg(palette.dimmed))
            }
        }
        WorktreeColumn::Age => {
            Cell::from(row.age.clone()).style(Style::default().fg(palette.dimmed))
        }
        WorktreeColumn::Agent => Cell::from(row.agent_line.clone()),
    }
}

/// Upper bound for the `worktree` cell.
///
/// The cell can carry both the handle and the branch, which a fixed cap cut off
/// even in a very wide terminal. Half the table leaves room for every other
/// column; the lower bound keeps narrow panes rendering as they always have,
/// and the upper one stops a single long branch from crowding the rest out.
fn worktree_width_budget(area_width: u16) -> usize {
    (area_width as usize / 2).clamp(25, 80)
}

/// Derive headers, cells and constraints from the same visible column order.
fn build_worktree_table(
    columns: &[WorktreeColumn],
    row_data: Vec<WorktreeRowData>,
    worktree_max_width: usize,
    header_state: format::ResourceHeaderState<'_>,
) -> Table<'static> {
    let palette = header_state.palette;
    let header_cells: Vec<_> = columns
        .iter()
        .map(|column| match column {
            WorktreeColumn::Number => ResourceHeaderCell::Plain("#"),
            WorktreeColumn::Project => ResourceHeaderCell::Plain("Project"),
            WorktreeColumn::Worktree => ResourceHeaderCell::Plain("Worktree"),
            WorktreeColumn::Git => ResourceHeaderCell::Git,
            WorktreeColumn::Pr => ResourceHeaderCell::Pr,
            WorktreeColumn::Mux => ResourceHeaderCell::Plain("Mux"),
            WorktreeColumn::Age => ResourceHeaderCell::Plain("Age"),
            WorktreeColumn::Agent => ResourceHeaderCell::Plain("Agent"),
        })
        .collect();

    let project_names: Vec<String> = row_data.iter().map(|r| r.project.clone()).collect();
    let max_project_width = format::calc_column_width(&project_names, 5, 20, 2);
    let worktree_names: Vec<String> = row_data
        .iter()
        .map(|r| r.worktree_display.clone())
        .collect();
    let max_worktree_width = format::calc_column_width(&worktree_names, 8, worktree_max_width, 1);
    let max_git_width = row_data
        .iter()
        .map(|row| {
            row.git_spans
                .iter()
                .map(|(text, _)| text.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(4)
        .clamp(4, 30)
        + 1;
    let max_pr_width = row_data
        .iter()
        .map(|row| &row.pr_spans)
        .map(|spans| {
            spans
                .iter()
                .map(|(text, _)| text.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(4)
        .clamp(4, 16)
        + 1;
    let max_agent_width = row_data
        .iter()
        .map(|row| row.agent_line.width())
        .max()
        .unwrap_or(0)
        .clamp(5, u16::MAX as usize) as u16;

    let last_column = columns.len().saturating_sub(1);
    let constraints: Vec<_> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| match column {
            WorktreeColumn::Number => Constraint::Length(2),
            WorktreeColumn::Project => Constraint::Length(max_project_width),
            WorktreeColumn::Worktree => Constraint::Length(max_worktree_width),
            WorktreeColumn::Git => Constraint::Length(max_git_width as u16),
            WorktreeColumn::Pr => Constraint::Length(max_pr_width as u16),
            WorktreeColumn::Mux => Constraint::Length(4),
            WorktreeColumn::Age => Constraint::Length(4),
            // Fill only at the end, so columns after Agent stay contiguous.
            WorktreeColumn::Agent if index == last_column => Constraint::Fill(1),
            WorktreeColumn::Agent => Constraint::Length(max_agent_width),
        })
        .collect();

    let rows: Vec<_> = row_data
        .into_iter()
        .map(|data| {
            let cells: Vec<_> = columns
                .iter()
                .map(|column| worktree_cell(*column, &data, palette))
                .collect();
            let row = Row::new(cells);
            if data.is_current {
                row.style(Style::default().bg(palette.current_row_bg))
            } else {
                row
            }
        })
        .collect();

    let highlight_symbol = Text::from(Line::from(Span::styled(
        "▌ ",
        Style::default().fg(palette.info),
    )));
    Table::new(rows, constraints)
        .header(format::resource_table_header(header_state, &header_cells))
        .block(Block::default())
        .row_highlight_style(Style::default().bg(palette.highlight_row_bg))
        .highlight_symbol(highlight_symbol)
}

/// Render the worktree preview: info panel (left) + styled git log (right).
pub fn render_worktree_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let selected_worktree = app
        .worktree_table_state
        .selected()
        .and_then(|idx| app.worktrees.get(idx));

    // Split preview area into info panel (left) and git log (right)
    let chunks = Layout::horizontal([
        Constraint::Length(40), // Info panel: fixed width
        Constraint::Fill(1),    // Git log: remaining space
    ])
    .split(area);

    render_info_panel(f, app, chunks[0], selected_worktree);
    render_git_log(f, app, chunks[1], selected_worktree);
}

/// Render the info panel showing worktree metadata.
fn render_info_panel(
    f: &mut Frame,
    app: &App,
    area: Rect,
    worktree: Option<&crate::workflow::types::WorktreeInfo>,
) {
    let label_style = Style::default().fg(app.palette.dimmed);
    let text_style = Style::default().fg(app.palette.text);

    let title = if let Some(wt) = worktree {
        format!(" {} ", wt.handle)
    } else {
        " Info ".to_string()
    };

    let block = format::panel_block(title, &app.palette);

    let Some(wt) = worktree else {
        let paragraph = Paragraph::new(Text::raw("(no worktree selected)")).block(block);
        f.render_widget(paragraph, area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();

    // Branch
    lines.push(Line::from(vec![
        Span::styled("Branch  ", label_style),
        Span::styled(&wt.branch, text_style),
    ]));

    // Git status details (base branch, ahead/behind, diff stats)
    let git_status = app.git_statuses.get(&wt.path);
    if let Some(status) = git_status {
        // Base branch + ahead/behind
        let mut base_spans = vec![Span::styled("Base    ", label_style)];
        if !status.base_branch.is_empty() {
            base_spans.push(Span::styled(&status.base_branch, text_style));
        } else {
            base_spans.push(Span::styled("main", text_style));
        }
        if status.ahead > 0 || status.behind > 0 {
            base_spans.push(Span::styled(" (", label_style));
            if status.ahead > 0 {
                base_spans.push(Span::styled(
                    format!("\u{2191}{}", status.ahead),
                    Style::default().fg(app.palette.info),
                ));
            }
            if status.ahead > 0 && status.behind > 0 {
                base_spans.push(Span::styled(" ", label_style));
            }
            if status.behind > 0 {
                base_spans.push(Span::styled(
                    format!("\u{2193}{}", status.behind),
                    Style::default().fg(app.palette.accent),
                ));
            }
            base_spans.push(Span::styled(")", label_style));
        }
        lines.push(Line::from(base_spans));

        // Committed diff stats
        if status.lines_added > 0 || status.lines_removed > 0 {
            let mut diff_spans = vec![Span::styled("Diff    ", label_style)];
            if status.lines_added > 0 {
                diff_spans.push(Span::styled(
                    format!("+{}", status.lines_added),
                    Style::default().fg(app.palette.success),
                ));
            }
            if status.lines_added > 0 && status.lines_removed > 0 {
                diff_spans.push(Span::styled(" ", text_style));
            }
            if status.lines_removed > 0 {
                diff_spans.push(Span::styled(
                    format!("-{}", status.lines_removed),
                    Style::default().fg(app.palette.danger),
                ));
            }
            diff_spans.push(Span::styled(" committed", label_style));
            lines.push(Line::from(diff_spans));
        }

        // Uncommitted changes
        if status.uncommitted_added > 0 || status.uncommitted_removed > 0 {
            let mut uc_spans = vec![Span::styled("        ", label_style)];
            if status.uncommitted_added > 0 {
                uc_spans.push(Span::styled(
                    format!("+{}", status.uncommitted_added),
                    Style::default().fg(app.palette.success),
                ));
            }
            if status.uncommitted_added > 0 && status.uncommitted_removed > 0 {
                uc_spans.push(Span::styled(" ", text_style));
            }
            if status.uncommitted_removed > 0 {
                uc_spans.push(Span::styled(
                    format!("-{}", status.uncommitted_removed),
                    Style::default().fg(app.palette.danger),
                ));
            }
            uc_spans.push(Span::styled(" uncommitted", label_style));
            lines.push(Line::from(uc_spans));
        }

        // Rebase indicator
        if status.is_rebasing {
            let git_icons = crate::nerdfont::git_icons();
            lines.push(Line::from(vec![
                Span::styled("        ", label_style),
                Span::styled(
                    format!("{} ", git_icons.rebase),
                    Style::default().fg(app.palette.warning),
                ),
                Span::styled("rebase in progress", label_style),
            ]));
        }

        // Conflict indicator
        if status.has_conflict {
            lines.push(Line::from(vec![
                Span::styled("        ", label_style),
                Span::styled(
                    "conflict with base",
                    Style::default().fg(app.palette.danger),
                ),
            ]));
        }
    }

    // PR info
    if let Some(ref pr) = wt.pr_info {
        let pr_icons = crate::nerdfont::pr_icons();
        let (icon, color) = if pr.is_draft {
            (pr_icons.draft, app.palette.dimmed)
        } else {
            match pr.state.as_str() {
                "OPEN" => (pr_icons.open, app.palette.success),
                "MERGED" => (pr_icons.merged, app.palette.accent),
                "CLOSED" => (pr_icons.closed, app.palette.danger),
                _ => ("?", app.palette.dimmed),
            }
        };
        let mut pr_spans = vec![
            Span::styled("PR      ", label_style),
            Span::styled(format!("#{} ", pr.number), Style::default().fg(color)),
            Span::styled(icon, Style::default().fg(color)),
        ];
        // Check status
        if let Some(ref checks) = pr.checks {
            use crate::github::CheckState;
            let check_icons = crate::nerdfont::check_icons();
            let (check_icon, check_color) = match checks {
                CheckState::Success => (check_icons.success.to_string(), app.palette.success),
                CheckState::Failure { .. } => (check_icons.failure.to_string(), app.palette.danger),
                CheckState::Pending { .. } => (check_icons.pending.to_string(), app.palette.accent),
            };
            pr_spans.push(Span::styled(" ", text_style));
            pr_spans.push(Span::styled(check_icon, Style::default().fg(check_color)));
        }
        lines.push(Line::from(pr_spans));

        // PR title (truncated to fit)
        let inner_width = area.width.saturating_sub(2) as usize; // border
        let title_max = inner_width.saturating_sub(8); // label width
        let truncated_title = if pr.title.chars().count() > title_max {
            format!(
                "{}...",
                pr.title
                    .chars()
                    .take(title_max.saturating_sub(3))
                    .collect::<String>()
            )
        } else {
            pr.title.clone()
        };
        lines.push(Line::from(vec![
            Span::styled("        ", label_style),
            Span::styled(truncated_title, Style::default().fg(color)),
        ]));

        // Check detail: failing check name or pending elapsed time
        let detail_spans = super::format::format_pr_details(pr, app.spinner_frame, &app.palette);
        if !detail_spans.is_empty() {
            let mut line_spans = vec![Span::styled("        ", label_style)];
            line_spans.extend(detail_spans);
            lines.push(Line::from(line_spans));
        }
    }

    // Agent status
    if wt.agent_status.is_some() {
        let mut agent_spans = vec![Span::styled("Agent   ", label_style)];
        for (text, style) in format_agent_status_summary(
            wt.agent_status.as_ref(),
            &app.config.status_icons,
            app.spinner_frame,
            &app.palette,
            AgentStatusFormat::DetailLine,
        ) {
            agent_spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(agent_spans));
    }

    // Mux window
    let mux_spans = vec![
        Span::styled("Mux     ", label_style),
        if wt.has_mux_window {
            Span::styled("\u{25cf} active", Style::default().fg(app.palette.success))
        } else {
            Span::styled("- none", Style::default().fg(app.palette.dimmed))
        },
    ];
    lines.push(Line::from(mux_spans));

    let paragraph = Paragraph::new(Text::from(lines)).block(block);
    f.render_widget(paragraph, area);
}

/// Render the styled git log panel.
fn render_git_log(
    f: &mut Frame,
    app: &App,
    area: Rect,
    worktree: Option<&crate::workflow::types::WorktreeInfo>,
) {
    let block = format::panel_block(" Git Log ", &app.palette);

    let text = match (&app.worktree_preview, worktree) {
        (Some(log), Some(_)) if !log.trim().is_empty() => {
            let hash_style = Style::default().fg(app.palette.accent);
            let date_style = Style::default().fg(app.palette.dimmed);
            let msg_style = Style::default().fg(app.palette.text);

            let lines: Vec<Line> = log
                .lines()
                .map(|line| {
                    let parts: Vec<&str> = line.splitn(3, '\t').collect();
                    if parts.len() == 3 {
                        Line::from(vec![
                            Span::styled(parts[0], hash_style),
                            Span::styled("  ", date_style),
                            Span::styled(parts[1], date_style),
                            Span::styled("  ", msg_style),
                            Span::styled(parts[2], msg_style),
                        ])
                    } else {
                        // Fallback for lines that don't match format
                        Line::styled(line, msg_style)
                    }
                })
                .collect();
            Text::from(lines)
        }
        (None, Some(_)) => Text::raw(""),
        (Some(_), Some(_)) => Text::raw("(no commits)"),
        (_, None) => Text::raw(""),
    };

    let paragraph = Paragraph::new(text).block(block);
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_WORKTREE_COLUMNS, ThemeConfig, ThemeMode};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, widgets::TableState};

    fn palette() -> ThemePalette {
        ThemePalette::from_config(&ThemeConfig::default(), ThemeMode::Dark)
    }

    fn row() -> WorktreeRowData {
        WorktreeRowData {
            jump_key: "1".into(),
            project: "proj".into(),
            worktree_display: "wt".into(),
            is_main: false,
            is_current: false,
            git_spans: vec![("+1".into(), Style::default())],
            pr_spans: vec![("#7".into(), Style::default())],
            agent_line: Line::from("working"),
            has_mux_window: true,
            age: "2h".into(),
        }
    }

    fn render(
        columns: &[WorktreeColumn],
        rows: Vec<WorktreeRowData>,
        width: u16,
        selected: Option<usize>,
    ) -> Buffer {
        let palette = palette();
        let table = build_worktree_table(
            columns,
            rows,
            worktree_width_budget(width),
            format::ResourceHeaderState {
                palette: &palette,
                spinner_frame: 0,
                git_fetching: false,
                pr_fetching: false,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(width, 4)).unwrap();
        let mut state = TableState::default().with_selected(selected);
        terminal
            .draw(|f| f.render_stateful_widget(table, f.area(), &mut state))
            .unwrap();
        assert_eq!(state.selected(), selected);
        terminal.backend().buffer().clone()
    }

    fn line(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn worktree_table_preserves_default_layout() {
        let buffer = render(&DEFAULT_WORKTREE_COLUMNS, vec![row()], 80, None);
        assert_eq!(
            line(&buffer, 0),
            "#  Project Worktree  Git   PR    Mux  Age  Agent"
        );
        assert_eq!(
            line(&buffer, 1),
            "1  proj    wt        +1    #7    ●    2h   working"
        );
    }

    #[test]
    fn worktree_table_reorders_headers_and_cells() {
        use WorktreeColumn::*;
        let buffer = render(
            &[Agent, Age, Mux, Pr, Git, Worktree, Project, Number],
            vec![row()],
            80,
            None,
        );
        assert_eq!(
            line(&buffer, 0),
            "Agent   Age  Mux  PR    Git   Worktree  Project #"
        );
        assert_eq!(
            line(&buffer, 1),
            "working 2h   ●    #7    +1    wt        proj    1"
        );
    }

    #[test]
    fn worktree_table_omits_unlisted_columns_at_narrow_width() {
        use WorktreeColumn::*;
        let buffer = render(&[Worktree, Agent], vec![row()], 24, None);
        assert_eq!(line(&buffer, 0), "Worktree  Agent");
        assert_eq!(line(&buffer, 1), "wt        working");
    }

    #[test]
    fn agent_width_uses_display_cells_and_fits_header() {
        use WorktreeColumn::*;
        for (agent, expected) in [("", "Agent Mux"), ("作業中です", "Agent      Mux")] {
            let mut data = row();
            data.agent_line = Line::from(agent);
            let buffer = render(&[Agent, Mux], vec![data], 40, None);
            assert_eq!(line(&buffer, 0), expected);
        }
    }

    #[test]
    fn trailing_agent_fills_after_pr_is_hidden() {
        use WorktreeColumn::*;
        let columns = visible_columns(&[Agent, Pr], false);
        assert_eq!(columns, [Agent]);
        let mut data = row();
        data.agent_line = Line::from("working").right_aligned();
        let buffer = render(&columns, vec![data], 20, None);
        assert_eq!(line(&buffer, 1), "             working");
    }

    #[test]
    fn pr_visibility_preserves_order_and_pr_only_fallback() {
        use WorktreeColumn::*;
        assert_eq!(
            visible_columns(&[Agent, Pr, Worktree], false),
            [Agent, Worktree]
        );
        assert_eq!(
            visible_columns(&[Agent, Pr, Worktree], true),
            [Agent, Pr, Worktree]
        );
        assert_eq!(visible_columns(&[Pr], false), [Pr]);
        let mut data = row();
        data.pr_spans = format_pr_status(None, None, false, 0, &palette());
        let buffer = render(&visible_columns(&[Pr], false), vec![data], 20, None);
        assert_eq!(line(&buffer, 0), "PR");
        assert_eq!(line(&buffer, 1), "-");
    }

    #[test]
    fn reordered_table_preserves_row_styles_and_selection() {
        use WorktreeColumn::*;
        let mut current = row();
        current.is_current = true;
        let mut main = row();
        main.is_main = true;
        main.has_mux_window = false;
        let palette = palette();
        let buffer = render(&[Mux, Worktree], vec![current, main], 30, Some(1));
        assert_eq!(buffer[(7, 1)].fg, palette.current_worktree_fg);
        assert_eq!(buffer[(7, 1)].bg, palette.current_row_bg);
        assert_eq!(buffer[(7, 2)].fg, palette.dimmed);
        assert_eq!(buffer[(7, 2)].bg, palette.highlight_row_bg);
        assert_eq!(buffer[(2, 1)].fg, palette.success);
        assert_eq!(buffer[(2, 2)].fg, palette.dimmed);
        assert_eq!(buffer[(0, 2)].symbol(), "▌");
    }

    #[test]
    fn worktree_width_budget_scales_with_the_pane() {
        // Narrow panes keep the historical fixed width.
        assert_eq!(worktree_width_budget(40), 25);
        assert_eq!(worktree_width_budget(50), 25);
        // Wider panes get proportionally more, up to a ceiling.
        assert_eq!(worktree_width_budget(120), 60);
        assert_eq!(worktree_width_budget(200), 80);
        assert_eq!(worktree_width_budget(400), 80);
    }

    #[test]
    fn worktree_column_uses_the_extra_width_in_a_wide_pane() {
        let long = "feat/some-rather-long-branch-name";
        let mut r = row();
        r.worktree_display = long.into();
        let buffer = render(&[WorktreeColumn::Worktree], vec![r], 120, None);
        assert!(
            line(&buffer, 1).contains(long),
            "long worktree label should survive in a wide pane, got {:?}",
            line(&buffer, 1)
        );
    }
}
