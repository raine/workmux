//! Help overlay rendering.

use ratatui::{
    Frame,
    layout::Constraint,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Cell, Paragraph, Row, Table},
};

use super::super::app::{App, RowContextKind};
use super::super::keymap::{Context, help_rows};
use super::popup::{centered_rect, popup_block, render_popup};
use super::theme::ThemePalette;

/// Determine the current keymap context for help display.
fn get_help_context(app: &App) -> Context {
    app.keymap_context()
}

/// Get the title for the help overlay based on context.
fn context_title(ctx: Context) -> &'static str {
    match ctx {
        Context::DashboardNormal => "Dashboard",
        Context::DashboardInput => "Input Mode",
        Context::DashboardFilter | Context::WorktreeFilter => "Filter",
        Context::WorktreeNormal => "Worktrees",
        Context::DiffNormal => "Diff View",
        Context::Patch => "Patch Mode",
        Context::Comment => "Comment",
    }
}

/// Render the kill confirmation popup.
pub fn render_confirm_kill(f: &mut Frame, app: &App) {
    let palette = &app.palette;
    let area = centered_rect(f.area(), 34, 3);
    let block = popup_block(None, palette);
    let text = Line::from(vec![
        Span::styled(" Kill working agent? ", Style::default().fg(palette.text)),
        Span::styled(
            "y",
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("es / ", Style::default().fg(palette.dimmed)),
        Span::styled(
            "n",
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("o", Style::default().fg(palette.dimmed)),
    ]);
    let paragraph = Paragraph::new(text).block(block);
    render_popup(f, area, paragraph);
}

/// Render the remove worktree confirmation modal.
pub fn render_confirm_remove(f: &mut Frame, app: &App) {
    let Some(ref plan) = app.pending_remove else {
        return;
    };
    let palette = &app.palette;

    // Build content lines
    let mut lines: Vec<Line> = Vec::new();

    // Title line + spacer
    lines.push(Line::from(vec![Span::styled(
        format!(" Remove {}?", plan.handle),
        Style::default().fg(palette.text),
    )]));
    lines.push(Line::from(""));

    // Warning lines
    if plan.is_dirty {
        lines.push(Line::from(vec![Span::styled(
            " Has uncommitted changes.",
            Style::default().fg(palette.danger),
        )]));
    }
    if plan.is_unmerged {
        lines.push(Line::from(vec![Span::styled(
            " Has unmerged commits.",
            Style::default().fg(palette.dimmed),
        )]));
    }

    // Branch outcome line
    if plan.keep_branch {
        lines.push(Line::from(vec![Span::styled(
            " Branch will be kept.",
            Style::default().fg(palette.dimmed),
        )]));
    } else {
        lines.push(Line::from(vec![Span::styled(
            " Branch will be deleted.",
            Style::default().fg(palette.dimmed),
        )]));
    }

    // Empty line before actions
    lines.push(Line::from(""));

    // Action line (context-dependent)
    let branch_label = if plan.keep_branch {
        " delete branch"
    } else {
        " keep branch"
    };
    let action_line = if plan.is_dirty && !plan.force_armed {
        // Dirty: must press f to arm force
        render_modal_footer_row(
            palette,
            &[("f", " force  "), ("n", " cancel  "), ("k", branch_label)],
        )
    } else if plan.is_dirty && plan.force_armed {
        // Dirty + force armed: y now available
        render_modal_footer_row(
            palette,
            &[
                ("y", " confirm force  "),
                ("n", " cancel  "),
                ("k", branch_label),
            ],
        )
    } else {
        // Clean or unmerged: y available
        render_modal_footer_row(
            palette,
            &[("y", " remove  "), ("n", " cancel  "), ("k", branch_label)],
        )
    };
    lines.push(action_line);

    // Calculate dimensions
    let height = lines.len() as u16 + 2; // +2 for borders
    let width = 44;

    let area = centered_rect(f.area(), width, height);
    let block = popup_block(None, palette);
    let paragraph = Paragraph::new(Text::from(lines)).block(block);
    render_popup(f, area, paragraph);
}

/// Render the help overlay.
pub fn render_help(f: &mut Frame, app: &App) {
    let ctx = get_help_context(app);
    let title = context_title(ctx);
    let keybindings = help_rows(ctx, app.config.dashboard.input_exit_key());

    // Calculate dimensions based on content
    let row_count = keybindings.len() as u16;
    let height = row_count + 5; // +5 for borders, padding, and empty line at top
    let width = 44;

    let palette = &app.palette;

    // Create styled block with rounded corners
    let block = popup_block(Some(title), palette).title_bottom(Line::from(vec![
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
                        .fg(palette.dimmed)
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

    render_popup(f, centered_rect(f.area(), width, height), table);
}

/// Render the sweep progress overlay.
pub fn render_sweep_progress(f: &mut Frame, app: &App) {
    let Some(ref progress) = app.sweep_progress else {
        return;
    };
    let palette = &app.palette;
    let spinner = super::super::spinner::SPINNER_FRAMES
        [app.spinner_frame as usize % super::super::spinner::SPINNER_FRAMES.len()];

    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            format!(" {spinner} Removing worktrees... "),
            Style::default().fg(palette.text),
        )]),
        Line::from(vec![Span::styled(
            format!(
                " {} ({}/{}) ",
                progress.handle, progress.current, progress.total
            ),
            Style::default().fg(palette.dimmed),
        )]),
        Line::from(""),
    ];

    let height = lines.len() as u16 + 2;
    let content_width = lines.iter().map(|l| l.width()).max().unwrap_or(30);
    let width = (content_width as u16 + 4).max(36);
    let area = centered_rect(f.area(), width, height);
    let block = popup_block(Some("Sweep"), palette);
    let paragraph = Paragraph::new(Text::from(lines)).block(block);
    render_popup(f, area, paragraph);
}

/// Render the sweep cleanup modal.
pub fn render_sweep(f: &mut Frame, app: &App) {
    let Some(ref sweep) = app.pending_sweep else {
        return;
    };
    let palette = &app.palette;

    // Empty state
    if sweep.candidates.is_empty() {
        let lines = vec![
            Line::from(""),
            render_empty_state(" No merged or gone worktrees found.", palette),
            Line::from(""),
        ];

        let height = lines.len() as u16 + 2;
        let width = 38;
        let area = centered_rect(f.area(), width, height);
        let block = popup_block(Some("Sweep"), palette);
        let paragraph = Paragraph::new(Text::from(lines)).block(block);
        render_popup(f, area, paragraph);
        return;
    }

    let selected_count = sweep.candidates.iter().filter(|c| c.selected).count();

    // Build content lines
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    for (i, candidate) in sweep.candidates.iter().enumerate() {
        let cursor = if i == sweep.cursor { "> " } else { "  " };
        let cursor_style = Style::default().fg(palette.text);

        if candidate.is_dirty {
            // Dirty: greyed out, not selectable
            lines.push(Line::from(vec![
                Span::styled(cursor, cursor_style),
                render_modal_dim_text(
                    palette,
                    &format!(
                        "[ ] {} ({}, dirty)",
                        candidate.handle,
                        candidate.reason.label()
                    ),
                ),
            ]));
        } else {
            let checkbox = if candidate.selected { "[x]" } else { "[ ]" };
            let style = Style::default().fg(palette.text);
            lines.push(Line::from(vec![
                Span::styled(cursor, cursor_style),
                Span::styled(format!("{} {} ", checkbox, candidate.handle), style),
                render_modal_dim_text(palette, &format!("({})", candidate.reason.label())),
            ]));
        }
    }

    lines.push(Line::from(""));

    // Action line
    let remove_label = if selected_count > 0 {
        format!(" remove ({})", selected_count)
    } else {
        " remove".to_string()
    };
    lines.push(render_modal_footer_row(
        palette,
        &[
            ("Space", " toggle  "),
            ("Enter", &remove_label),
            ("", "  "),
            ("Esc", " cancel"),
        ],
    ));

    // Calculate dimensions
    let height = lines.len() as u16 + 2; // +2 for borders
    let content_width = sweep
        .candidates
        .iter()
        .map(|c| {
            // cursor + checkbox + handle + reason
            2 + 4 + c.handle.len() + c.reason.label().len() + 10
        })
        .max()
        .unwrap_or(30);
    let width = (content_width as u16 + 4).max(44); // +4 for border+padding

    let area = centered_rect(f.area(), width, height);
    let block = popup_block(Some("Sweep"), palette);
    let paragraph = Paragraph::new(Text::from(lines)).block(block);
    render_popup(f, area, paragraph);
}

/// Render the filter input line showing "/text_" pattern.
fn render_filter_line(filter: &str, palette: &ThemePalette) -> Line<'static> {
    if filter.is_empty() {
        Line::from(vec![
            Span::styled(" /", Style::default().fg(palette.dimmed)),
            Span::styled("_", Style::default().fg(palette.dimmed)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" /", Style::default().fg(palette.dimmed)),
            Span::styled(filter.to_string(), Style::default().fg(palette.text)),
            Span::styled("_", Style::default().fg(palette.text)),
        ])
    }
}

/// Render the "No matching X" empty state line.
fn render_empty_state(text: &str, palette: &ThemePalette) -> Line<'static> {
    Line::from(vec![Span::styled(
        text.to_string(),
        Style::default().fg(palette.dimmed),
    )])
}

fn render_modal_key_text(palette: &ThemePalette, text: &str) -> Span<'static> {
    Span::styled(
        text.to_string(),
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    )
}

fn render_modal_dim_text(palette: &ThemePalette, text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(palette.dimmed))
}

fn render_modal_footer_row(palette: &ThemePalette, pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::with_capacity(1 + pairs.len() * 2);
    spans.push(Span::raw(" "));

    for (key, label) in pairs {
        if !key.is_empty() {
            spans.push(render_modal_key_text(palette, key));
        }
        spans.push(render_modal_dim_text(palette, label));
    }

    Line::from(spans)
}

fn append_blank_lines(lines: &mut Vec<Line>, count: usize) {
    for _ in 0..count {
        lines.push(Line::from(""));
    }
}

/// Compute a visible window of items around the cursor for scrolling.
fn scroll_window(cursor: usize, total: usize, max_visible: usize) -> (usize, usize) {
    let start = if total <= max_visible || cursor < max_visible / 2 {
        0
    } else if cursor + max_visible / 2 >= total {
        total.saturating_sub(max_visible)
    } else {
        cursor - max_visible / 2
    };
    let end = (start + max_visible).min(total);
    (start, end)
}

/// Render the base branch picker modal.
pub fn render_base_picker(f: &mut Frame, app: &App) {
    let Some(ref picker) = app.pending_base_picker else {
        return;
    };
    let palette = &app.palette;

    let filtered = picker.filtered();

    let content_width = picker
        .branches
        .iter()
        .map(|b| 2 + b.len())
        .max()
        .unwrap_or(20);
    let width = (content_width as u16 + 4).clamp(44, 60);
    // Fixed height: ~40% of terminal, matching add-worktree modal
    let area = f.area();
    let height = (area.height * 2 / 5).clamp(10, 25);
    // 1 filter + 1 blank + visible items + 1 blank + 1 footer + 2 borders
    let max_visible: usize = height.saturating_sub(6) as usize;

    let mut lines: Vec<Line> = Vec::new();

    lines.push(render_filter_line(&picker.filter, palette));

    lines.push(Line::from(""));

    if filtered.is_empty() {
        lines.push(render_empty_state(" No matching branches.", palette));
        // Fill remaining slots so height stays fixed
        append_blank_lines(&mut lines, max_visible.saturating_sub(1));
    } else {
        let (start, end) = scroll_window(picker.cursor, filtered.len(), max_visible);

        for (fi, &idx) in filtered.iter().enumerate().take(end).skip(start) {
            let branch = &picker.branches[idx];
            let cursor = if fi == picker.cursor { "> " } else { "  " };

            let is_current = picker.current_base.as_ref().is_some_and(|b| b == branch);

            let name_style = if is_current {
                Style::default().fg(palette.accent)
            } else {
                Style::default().fg(palette.text)
            };

            lines.push(Line::from(vec![
                Span::styled(cursor, Style::default().fg(palette.text)),
                Span::styled(branch.clone(), name_style),
            ]));
        }

        // Fill remaining slots so height stays fixed
        append_blank_lines(
            &mut lines,
            max_visible.saturating_sub(end.saturating_sub(start)),
        );
    }

    lines.push(Line::from(""));

    // Footer
    lines.push(render_modal_footer_row(
        palette,
        &[("Enter", " set base  "), ("Esc", " cancel")],
    ));

    let popup_area = centered_rect(f.area(), width, height);

    let block = popup_block(Some("Set Base Branch"), palette);
    let paragraph = Paragraph::new(Text::from(lines)).block(block);

    render_popup(f, popup_area, paragraph);
}

/// Render the project picker modal.
pub fn render_project_picker(f: &mut Frame, app: &App) {
    let Some(ref picker) = app.pending_project_picker else {
        return;
    };
    let palette = &app.palette;

    let filtered = picker.filtered();

    let mut lines: Vec<Line> = Vec::new();

    // Filter input line (shown when typing)
    if !picker.filter.is_empty() {
        lines.push(render_filter_line(&picker.filter, palette));
    }

    lines.push(Line::from(""));

    if filtered.is_empty() {
        lines.push(render_empty_state(" No matching projects.", palette));
    } else {
        for (fi, &idx) in filtered.iter().enumerate() {
            let project = &picker.projects[idx];
            let cursor = if fi == picker.cursor { "> " } else { "  " };

            let is_current = picker
                .current_name
                .as_ref()
                .is_some_and(|n| *n == project.name);

            let name_style = if is_current {
                Style::default().fg(palette.accent)
            } else {
                Style::default().fg(palette.text)
            };

            lines.push(Line::from(vec![
                Span::styled(cursor, Style::default().fg(palette.text)),
                Span::styled(project.name.clone(), name_style),
            ]));
        }
    }

    lines.push(Line::from(""));

    // Footer
    lines.push(render_modal_footer_row(
        palette,
        &[("Enter", " switch  "), ("Esc", " cancel")],
    ));

    // Calculate dimensions
    let height = lines.len() as u16 + 2;
    let content_width = picker
        .projects
        .iter()
        .map(|p| 2 + p.name.len())
        .max()
        .unwrap_or(20);
    let width = (content_width as u16 + 4).clamp(36, 60);

    let area = centered_rect(f.area(), width, height);
    let block = popup_block(Some("Switch Project"), palette);
    let paragraph = Paragraph::new(Text::from(lines)).block(block);

    render_popup(f, area, paragraph);
}

/// Render row actions at the pointer position.
pub fn render_row_context(f: &mut Frame, app: &App) {
    let Some(menu) = &app.pending_row_context else {
        return;
    };
    let area = menu.area(f.area());
    let inner_width = area.width.saturating_sub(2) as usize;
    let lines = menu
        .commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let selected = index == menu.cursor;
            let label_style = if selected {
                Style::default().fg(app.palette.accent)
            } else {
                Style::default().fg(app.palette.text)
            };
            let prefix = if selected { "> " } else { "  " };
            let used = 2 + command.label.len() + command.key_hint.len();
            let padding = inner_width.saturating_sub(used);
            Line::from(vec![
                Span::styled(prefix, label_style),
                Span::styled(command.label, label_style),
                Span::raw(" ".repeat(padding)),
                Span::styled(command.key_hint, Style::default().fg(app.palette.dimmed)),
            ])
        })
        .collect::<Vec<_>>();
    let title = match menu.kind {
        RowContextKind::Agent => "Agent actions",
        RowContextKind::Worktree => "Worktree actions",
    };
    let paragraph = Paragraph::new(Text::from(lines)).block(popup_block(Some(title), &app.palette));
    render_popup(f, area, paragraph);
}

/// Render the command palette modal.
pub fn render_command_palette(f: &mut Frame, app: &App) {
    let Some(ref palette) = app.pending_command_palette else {
        return;
    };
    let palette_ref = &app.palette;

    let filtered = palette.filtered();

    let area = f.area();
    let width = (area.width * 3 / 5).clamp(40, 70);
    let height = (area.height * 2 / 5).clamp(10, 25);
    // overhead: filter + blank + blank_after_items + footer + borders(2)
    let overhead: u16 = 6;
    let max_visible: usize = height.saturating_sub(overhead) as usize;

    // Compute final popup dimensions (respects small terminals)
    let popup_width = width.min(area.width);
    let popup_height = height.min(area.height);
    let inner_width = popup_width.saturating_sub(2) as usize; // minus borders

    let mut lines: Vec<Line> = Vec::new();

    lines.push(render_filter_line(&palette.filter, palette_ref));

    lines.push(Line::from(""));

    if filtered.is_empty() {
        lines.push(render_empty_state(" No matching commands.", palette_ref));
        append_blank_lines(&mut lines, max_visible.saturating_sub(1));
    } else {
        let total = filtered.len();
        let (start, end) = scroll_window(palette.cursor, total, max_visible);

        for (fi, &idx) in filtered.iter().enumerate().take(end).skip(start) {
            let cmd = &palette.commands[idx];
            let is_selected = fi == palette.cursor;
            let cursor_str = if is_selected { "> " } else { "  " };

            let label_style = if is_selected {
                Style::default().fg(palette_ref.accent)
            } else {
                Style::default().fg(palette_ref.text)
            };

            let mut spans = vec![
                Span::styled(cursor_str, Style::default().fg(palette_ref.text)),
                Span::styled(cmd.label, label_style),
            ];

            if !cmd.key_hint.is_empty() {
                // Right-align the key hint by padding
                let label_len = 2 + cmd.label.len() + 1 + cmd.key_hint.len();
                let pad = inner_width.saturating_sub(label_len);
                spans.push(Span::raw(" ".repeat(pad)));
                spans.push(Span::styled(
                    cmd.key_hint,
                    Style::default()
                        .fg(palette_ref.dimmed)
                        .add_modifier(Modifier::BOLD),
                ));
            }

            lines.push(Line::from(spans));
        }

        append_blank_lines(
            &mut lines,
            max_visible.saturating_sub(end.saturating_sub(start)),
        );
    }

    lines.push(Line::from(""));

    // Footer
    lines.push(render_modal_footer_row(
        palette_ref,
        &[("Enter", " run  "), ("Esc", " cancel")],
    ));

    let popup_area = centered_rect(f.area(), popup_width, popup_height);

    let block = popup_block(Some("Command Palette"), palette_ref);
    let paragraph = Paragraph::new(Text::from(lines)).block(block);

    render_popup(f, popup_area, paragraph);
}

/// Render the add-worktree modal.
pub fn render_add_worktree(f: &mut Frame, app: &App) {
    use super::super::app::{AddWorktreeMode, PrListState};

    let Some(ref state) = app.pending_add_worktree else {
        return;
    };
    let palette = &app.palette;

    let is_pr_mode = state.mode == AddWorktreeMode::Pr;

    let area = f.area();
    let width = (area.width * 3 / 5).clamp(44, 80);

    let area = f.area();
    let height = (area.height * 2 / 5).clamp(10, 25);
    // overhead: filter + blank + action_row + blank + footer + blank_after_footer + borders(2)
    let has_action_row = !is_pr_mode && !state.filter.trim().is_empty();
    let overhead: u16 = 7 + if has_action_row { 1 } else { 0 };
    let max_visible: usize = height.saturating_sub(overhead) as usize;

    let mut lines: Vec<Line> = Vec::new();

    // Filter input line
    lines.push(render_filter_line(&state.filter, palette));

    lines.push(Line::from(""));

    if is_pr_mode {
        // PR mode: show PR list
        match &state.pr_list {
            Some(PrListState::Loading) => {
                lines.push(Line::from(vec![Span::styled(
                    " Loading PRs...",
                    Style::default().fg(palette.dimmed),
                )]));
                append_blank_lines(&mut lines, max_visible.saturating_sub(1));
            }
            Some(PrListState::Loaded { prs, .. }) => {
                let filtered = state.filtered_prs();
                if filtered.is_empty() {
                    lines.push(Line::from(vec![Span::styled(
                        if state.filter.is_empty() {
                            " No open PRs."
                        } else {
                            " No matching PRs."
                        },
                        Style::default().fg(palette.dimmed),
                    )]));
                    append_blank_lines(&mut lines, max_visible.saturating_sub(1));
                } else {
                    let total = filtered.len();
                    let (start, end) = scroll_window(state.cursor, total, max_visible);

                    for (fi, &idx) in filtered.iter().enumerate().take(end).skip(start) {
                        let pr = &prs[idx];
                        let is_selected = fi == state.cursor;
                        let cursor_str = if is_selected { "> " } else { "  " };

                        let title_style = if is_selected {
                            Style::default().fg(palette.accent)
                        } else {
                            Style::default().fg(palette.text)
                        };

                        let mut spans = vec![
                            Span::styled(cursor_str, Style::default().fg(palette.text)),
                            Span::styled(
                                format!("#{} ", pr.number),
                                Style::default().fg(palette.dimmed),
                            ),
                            Span::styled(pr.title.clone(), title_style),
                        ];
                        if pr.is_draft {
                            spans.push(render_modal_dim_text(palette, " [draft]"));
                        }

                        lines.push(Line::from(spans));
                    }

                    append_blank_lines(
                        &mut lines,
                        max_visible.saturating_sub(end.saturating_sub(start)),
                    );
                }
            }
            Some(PrListState::Error { message }) => {
                lines.push(Line::from(vec![Span::styled(
                    format!(" {}", message),
                    Style::default().fg(palette.danger),
                )]));
                append_blank_lines(&mut lines, max_visible.saturating_sub(1));
            }
            None => {
                append_blank_lines(&mut lines, max_visible);
            }
        }
    } else {
        // Branch mode
        let filtered = state.filtered();

        // Action row: "Create" or "Checkout PR #N"
        if !state.filter.trim().is_empty() {
            let cursor_str = if state.cursor == 0 { "> " } else { "  " };
            let action_style = if state.cursor == 0 {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.text)
            };

            let label = if let Some(pr_num) = state.detected_pr_number() {
                format!("+ Checkout PR #{}", pr_num)
            } else {
                format!("+ Create \"{}\"", state.filter.trim())
            };

            lines.push(Line::from(vec![
                Span::styled(cursor_str, Style::default().fg(palette.text)),
                Span::styled(label, action_style),
            ]));
        }

        // Branch rows
        if filtered.is_empty() && state.filter.trim().is_empty() {
            lines.push(Line::from(vec![Span::styled(
                " Type to search or create...",
                Style::default().fg(palette.dimmed),
            )]));
            append_blank_lines(&mut lines, max_visible.saturating_sub(1));
        } else if filtered.is_empty() {
            append_blank_lines(&mut lines, max_visible);
        } else {
            let has_create_row = !state.filter.trim().is_empty();
            let branch_cursor = if has_create_row {
                state.cursor.checked_sub(1)
            } else {
                Some(state.cursor)
            };

            let total = filtered.len();
            let effective_cursor = branch_cursor.unwrap_or(0);
            let (start, end) = scroll_window(effective_cursor, total, max_visible);

            for (fi, &idx) in filtered.iter().enumerate().take(end).skip(start) {
                let branch = &state.branches[idx];
                let is_selected = branch_cursor == Some(fi);
                let cursor_str = if is_selected { "> " } else { "  " };
                let is_occupied = state.occupied_branches.contains(branch);

                let branch_style = if is_occupied {
                    Style::default().fg(palette.dimmed)
                } else if is_selected {
                    Style::default().fg(palette.accent)
                } else {
                    Style::default().fg(palette.text)
                };

                let mut spans = vec![
                    Span::styled(cursor_str, Style::default().fg(palette.text)),
                    Span::styled(branch.clone(), branch_style),
                ];
                if is_occupied {
                    spans.push(render_modal_dim_text(palette, " (in use)"));
                }

                lines.push(Line::from(spans));
            }

            append_blank_lines(
                &mut lines,
                max_visible.saturating_sub(end.saturating_sub(start)),
            );
        }
    }

    // Contextual hint based on current selection
    if !is_pr_mode {
        let has_create_row = !state.filter.trim().is_empty();
        let hint = if has_create_row && state.cursor == 0 {
            if state.detected_pr_number().is_some() {
                None // PR checkout is self-explanatory
            } else {
                Some(format!("New branch from {}", state.base_branch))
            }
        } else {
            // Existing branch selected
            let branch_cursor = if has_create_row {
                state.cursor.checked_sub(1)
            } else {
                Some(state.cursor)
            };
            let filtered = state.filtered();
            branch_cursor
                .and_then(|bc| filtered.get(bc))
                .map(|&idx| format!("Worktree for existing branch '{}'", state.branches[idx]))
        };
        if let Some(hint) = hint {
            lines.push(Line::from(vec![Span::styled(
                format!(" {}", hint),
                Style::default().fg(palette.dimmed),
            )]));
        } else {
            lines.push(Line::from(""));
        }
    } else {
        lines.push(Line::from(""));
    }

    // Footer (mode-dependent)
    if is_pr_mode {
        lines.push(render_modal_footer_row(
            palette,
            &[
                ("Enter", " checkout  "),
                ("^p", " branches  "),
                ("Esc", " cancel"),
            ],
        ));
    } else {
        lines.push(render_modal_footer_row(
            palette,
            &[
                ("Enter", " select  "),
                ("^b", " base  "),
                ("^p", " PRs  "),
                ("Esc", " cancel"),
            ],
        ));
    }
    lines.push(Line::from(""));

    let popup_area = centered_rect(f.area(), width, height);

    // Title and bottom border
    let title_text = if is_pr_mode {
        "Checkout PR"
    } else {
        "Add Worktree"
    };

    let mut block = popup_block(Some(title_text), palette);

    // Show base branch on bottom border only in branch mode
    if !is_pr_mode {
        let base_title = if state.editing_base {
            Line::from(vec![
                Span::styled(" Base: ", Style::default().fg(palette.dimmed)),
                Span::styled(
                    state.base_filter.clone(),
                    Style::default().fg(palette.accent),
                ),
                Span::styled("_ ", Style::default().fg(palette.accent)),
            ])
        } else {
            Line::from(vec![
                Span::styled(" Base: ", Style::default().fg(palette.dimmed)),
                Span::styled(
                    format!("{} ", state.base_branch),
                    Style::default().fg(palette.text),
                ),
            ])
        };
        block = block.title_bottom(base_title);
    }

    let paragraph = Paragraph::new(Text::from(lines)).block(block);

    render_popup(f, popup_area, paragraph);
}
