//! Dashboard view rendering (table, preview, footer).

use ansi_to_tui::IntoText;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Cell, Clear, Paragraph, Row, Table},
    Frame,
};
use std::collections::{BTreeMap, HashSet};

use super::super::app::{AddSessionField, App};
use super::super::spinner::SPINNER_FRAMES;
use super::format::{format_git_status, format_pr_status};

/// Render the dashboard view (table + preview + footer).
pub fn render_dashboard(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Check if backend supports preview
    let supports_preview = app.mux.supports_preview();

    // Layout: table (top), preview (bottom, only if supported), footer
    let chunks = if !supports_preview {
        // Zellij: no preview section
        Layout::vertical([
            Constraint::Min(5),    // Table (takes all space except footer)
            Constraint::Length(1), // Footer
        ])
        .split(area)
    } else {
        // Other multiplexers: include preview
        let table_size = 100u16.saturating_sub(app.preview_size as u16);
        Layout::vertical([
            Constraint::Percentage(table_size), // Table (top)
            Constraint::Min(5),                 // Preview (bottom, at least 5 lines)
            Constraint::Length(1),              // Footer
        ])
        .split(area)
    };

    // Table
    render_table(f, app, chunks[0]);

    // Preview (only for backends that support it)
    let footer_index = if supports_preview {
        render_preview(f, app, chunks[1]);
        2 // Footer is at index 2 when preview is shown
    } else {
        1 // Footer is at index 1 when preview is hidden
    };

    // Footer - show different help based on mode
    let footer_text = if app.add_form.is_some() {
        Paragraph::new(Line::from(vec![
            Span::styled(
                "  CREATE AGENT",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" - branch + prompt  "),
            Span::styled("[Tab]", Style::default().fg(Color::Yellow)),
            Span::raw(" switch  "),
            Span::styled("[Enter]", Style::default().fg(Color::Green)),
            Span::raw(" create  "),
            Span::styled("[Esc]", Style::default().fg(Color::Yellow)),
            Span::raw(" cancel"),
        ]))
    } else if app.input_mode {
        Paragraph::new(Line::from(vec![
            Span::styled(
                "  INPUT MODE",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" - Type to send keys to agent  "),
            Span::styled("[Esc]", Style::default().fg(Color::Yellow)),
            Span::raw(" exit"),
        ]))
    } else {
        let mut spans = vec![
            Span::styled("  [i]", Style::default().fg(Color::Green)),
            Span::raw(" input  "),
            Span::styled("[a]", Style::default().fg(Color::Green)),
            Span::raw(" add  "),
            Span::styled("[d]", Style::default().fg(Color::Yellow)),
            Span::raw(" diff  "),
            Span::styled("[1-9]", Style::default().fg(Color::Yellow)),
            Span::raw(" jump  "),
        ];

        // Only show peek command if backend supports preview
        if supports_preview {
            spans.extend(vec![
                Span::styled("[p]", Style::default().fg(Color::Cyan)),
                Span::raw(" peek  "),
            ]);
        }

        spans.extend(vec![
            Span::styled("[s]", Style::default().fg(Color::Cyan)),
            Span::raw(" sort: "),
            Span::styled(app.sort_mode.label(), Style::default().fg(Color::Green)),
            Span::raw("  "),
            Span::styled("[f]", Style::default().fg(Color::Cyan)),
            Span::raw(" filter: "),
        ]);

        if app.hide_stale {
            spans.push(Span::styled(
                "hiding stale",
                Style::default().fg(Color::Yellow),
            ));
        } else {
            spans.push(Span::styled("all", Style::default().fg(app.palette.dimmed)));
        }

        spans.extend(vec![
            Span::raw("  "),
            Span::styled("[c]", Style::default().fg(Color::Green)),
            Span::raw(" commit  "),
            Span::styled("[m]", Style::default().fg(Color::Yellow)),
            Span::raw(" merge  "),
            Span::styled("[Enter]", Style::default().fg(Color::Cyan)),
            Span::raw(" go  "),
            Span::styled("[q]", Style::default().fg(Color::Cyan)),
            Span::raw(" quit"),
        ]);

        Paragraph::new(Line::from(spans))
    };
    f.render_widget(footer_text, chunks[footer_index]);

    if app.add_form.is_some() {
        render_add_form(f, app);
    }
}

fn render_add_form(f: &mut Frame, app: &App) {
    let Some(form) = app.add_form.as_ref() else {
        return;
    };

    let area = f.area();
    let width = area.width.min(90).saturating_sub(4).max(40);
    let height = 11;
    let popup = Rect {
        x: area.width.saturating_sub(width) / 2,
        y: area.height.saturating_sub(height) / 2,
        width,
        height: height.min(area.height),
    };

    let palette = &app.palette;

    let branch_style = if form.active_field == AddSessionField::Branch {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(palette.text)
    };

    let prompt_style = if form.active_field == AddSessionField::Prompt {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(palette.text)
    };

    let branch_text = if form.branch.is_empty() {
        Span::styled("feature/name", Style::default().fg(palette.dimmed))
    } else {
        Span::styled(form.branch.as_str(), branch_style)
    };

    let prompt_text = if form.prompt.is_empty() {
        Span::styled(
            "What should this agent do?",
            Style::default().fg(palette.dimmed),
        )
    } else {
        Span::styled(form.prompt.as_str(), prompt_style)
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                " Branch ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            branch_text,
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                " Prompt ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            prompt_text,
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Tip: ", Style::default().fg(Color::Cyan)),
            Span::styled(
                "Prompt is optional; leave blank to start without one.",
                Style::default().fg(palette.dimmed),
            ),
        ]),
    ];

    if let Some(err) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            err,
            Style::default().fg(Color::Red),
        )));
    }

    let block = Block::bordered()
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(palette.help_border))
        .title(Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(
                "Create Agent",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default()),
        ]))
        .title_bottom(Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled("Tab", Style::default().fg(Color::Yellow)),
            Span::styled(" switch field · ", Style::default().fg(palette.help_muted)),
            Span::styled("Enter", Style::default().fg(Color::Green)),
            Span::styled(" create · ", Style::default().fg(palette.help_muted)),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::styled(" cancel ", Style::default().fg(palette.help_muted)),
        ]));

    let panel = Paragraph::new(lines).block(block);

    f.render_widget(Clear, popup);
    f.render_widget(panel, popup);
}

fn render_table(f: &mut Frame, app: &mut App, area: Rect) {
    // Check if we should show the PR column (only when at least one agent has a PR)
    let show_pr_column = app.has_any_pr();
    let show_check_counts = app.config.dashboard.show_check_counts();

    // Check if git data is being refreshed
    let is_git_fetching = app
        .is_git_fetching
        .load(std::sync::atomic::Ordering::Relaxed);

    // Build header with spinner in Git column when fetching
    let git_header = if is_git_fetching {
        let spinner = SPINNER_FRAMES[app.spinner_frame as usize % SPINNER_FRAMES.len()];
        Line::from(vec![
            Span::styled("Git ", Style::default().fg(Color::Cyan).bold()),
            Span::styled(spinner.to_string(), Style::default().fg(app.palette.dimmed)),
        ])
    } else {
        Line::from(Span::styled("Git", Style::default().fg(Color::Cyan).bold()))
    };

    // Build PR header with spinner when fetching
    let pr_header = if app.is_pr_fetching() {
        let spinner = SPINNER_FRAMES[app.spinner_frame as usize % SPINNER_FRAMES.len()];
        Line::from(vec![
            Span::styled("PR ", Style::default().fg(Color::Cyan).bold()),
            Span::styled(spinner.to_string(), Style::default().fg(app.palette.dimmed)),
        ])
    } else {
        Line::from(Span::styled("PR", Style::default().fg(Color::Cyan).bold()))
    };

    let header_style = Style::default().fg(Color::Cyan).bold();
    let mut header_cells = vec![
        Cell::from("#").style(header_style),
        Cell::from("Project").style(header_style),
        Cell::from("Worktree").style(header_style),
        Cell::from(git_header),
    ];

    if show_pr_column {
        header_cells.push(Cell::from(pr_header));
    }

    header_cells.extend(vec![
        Cell::from("Status").style(header_style),
        Cell::from("Time").style(header_style),
        Cell::from("Title").style(header_style),
    ]);

    let header = Row::new(header_cells).height(1);

    // Group agents by (session, window_name) to detect multi-pane windows
    let mut window_groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (idx, agent) in app.agents.iter().enumerate() {
        let key = (agent.session.clone(), agent.window_name.clone());
        window_groups.entry(key).or_default().push(idx);
    }

    // Build a set of windows with multiple panes
    let multi_pane_windows: HashSet<(String, String)> = window_groups
        .iter()
        .filter(|(_, indices)| indices.len() > 1)
        .map(|(key, _)| key.clone())
        .collect();

    // Track position within each window group for pane numbering
    let mut window_positions: BTreeMap<(String, String), usize> = BTreeMap::new();

    // Pre-compute row data to calculate max widths
    let row_data: Vec<_> = app
        .agents
        .iter()
        .enumerate()
        .map(|(idx, agent)| {
            let key = (agent.session.clone(), agent.window_name.clone());
            let is_multi_pane = multi_pane_windows.contains(&key);

            let pane_suffix = if is_multi_pane {
                let pos = window_positions.entry(key.clone()).or_insert(0);
                *pos += 1;
                format!(" [{}]", pos)
            } else {
                String::new()
            };

            let jump_key = if idx < 9 {
                format!("{}", idx + 1)
            } else {
                String::new()
            };

            let project = App::extract_project_name(agent);
            let (worktree_name, is_main) = app.extract_worktree_name(agent);
            // Check if this agent corresponds to the current working directory.
            // Try canonicalized comparison first (handles symlinks), fall back to direct comparison.
            let is_current = app.current_worktree.as_ref().is_some_and(|cwd| {
                // Try canonical comparison first (resolves symlinks like /var -> /private/var on macOS)
                if let (Ok(cwd_canonical), Ok(agent_canonical)) =
                    (cwd.canonicalize(), agent.path.canonicalize())
                {
                    cwd_canonical == agent_canonical
                } else {
                    // Fall back to direct comparison
                    agent.path == *cwd
                }
            });
            let worktree_display = format!("{}{}", worktree_name, pane_suffix);
            let title = agent
                .pane_title
                .as_ref()
                .map(|t| t.strip_prefix("... ").unwrap_or(t).to_string())
                .unwrap_or_default();
            let status_spans = app.get_status_display(agent);
            let duration = app
                .get_elapsed(agent)
                .map(|d| app.format_duration(d))
                .unwrap_or_else(|| "-".to_string());

            // Get git status for this worktree (may be None if not yet fetched)
            let git_status = app.git_statuses.get(&agent.path);
            let git_spans = format_git_status(git_status, app.spinner_frame, &app.palette);

            // Get PR status for this agent (only if column is shown)
            let pr_spans = if show_pr_column {
                let pr = app.get_pr_for_agent(agent);
                Some(format_pr_status(pr, show_check_counts, &app.palette))
            } else {
                None
            };

            (
                jump_key,
                project,
                worktree_display,
                is_main,
                is_current,
                git_spans,
                pr_spans,
                status_spans,
                duration,
                title,
            )
        })
        .collect();

    // Calculate max project name width (with padding, capped)
    let max_project_width = row_data
        .iter()
        .map(|(_, project, _, _, _, _, _, _, _, _)| project.len())
        .max()
        .unwrap_or(5)
        .clamp(5, 20) // min 5, max 20
        + 2; // padding

    // Calculate max worktree name width (with padding)
    // Use at least 8 to fit the "Worktree" header
    let max_worktree_width = row_data
        .iter()
        .map(|(_, _, worktree_display, _, _, _, _, _, _, _)| worktree_display.len())
        .max()
        .unwrap_or(8)
        .max(8) // min 8 (header width)
        + 1; // padding

    // Calculate max git status width (sum of all span character counts)
    // Use chars().count() instead of len() because Nerd Font icons are multi-byte
    let max_git_width = row_data
        .iter()
        .map(|(_, _, _, _, _, git_spans, _, _, _, _)| {
            git_spans
                .iter()
                .map(|(text, _)| text.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(4)
        .clamp(4, 30) // min 4, max 30 (increased for base branch)
        + 1; // padding

    // Calculate max PR status width (only if showing PR column)
    let max_pr_width = if show_pr_column {
        row_data
            .iter()
            .filter_map(|(_, _, _, _, _, _, pr_spans, _, _, _)| pr_spans.as_ref())
            .map(|spans| {
                spans
                    .iter()
                    .map(|(text, _)| text.chars().count())
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(4)
            .clamp(4, 16) // Increased from 12 to accommodate check icons + counts
            + 1
    } else {
        0
    };

    let rows: Vec<Row> = row_data
        .into_iter()
        .map(
            |(
                jump_key,
                project,
                worktree_display,
                is_main,
                is_current,
                git_spans,
                pr_spans,
                status_spans,
                duration,
                title,
            )| {
                let worktree_style = if is_current {
                    Style::default().fg(app.palette.current_worktree_fg)
                } else if is_main {
                    Style::default().fg(app.palette.dimmed)
                } else {
                    Style::default()
                };
                // Convert git spans to a Line
                let git_line = Line::from(
                    git_spans
                        .into_iter()
                        .map(|(text, style)| Span::styled(text, style))
                        .collect::<Vec<_>>(),
                );

                let mut cells = vec![
                    Cell::from(jump_key).style(Style::default().fg(Color::Yellow)),
                    Cell::from(project),
                    Cell::from(worktree_display).style(worktree_style),
                    Cell::from(git_line),
                ];

                // Add PR cell if column is shown
                if let Some(pr_spans) = pr_spans {
                    let pr_line = Line::from(
                        pr_spans
                            .into_iter()
                            .map(|(text, style)| Span::styled(text, style))
                            .collect::<Vec<_>>(),
                    );
                    cells.push(Cell::from(pr_line));
                }

                let status_line = Line::from(
                    status_spans
                        .into_iter()
                        .map(|(text, style)| Span::styled(text, style))
                        .collect::<Vec<_>>(),
                );
                cells.extend(vec![
                    Cell::from(status_line),
                    Cell::from(duration),
                    Cell::from(title),
                ]);

                let row = Row::new(cells);
                // Subtle background for the active worktree row
                if is_current {
                    row.style(Style::default().bg(app.palette.current_row_bg))
                } else {
                    row
                }
            },
        )
        .collect();

    // Build column constraints conditionally based on whether PR column is shown
    let mut constraints = vec![
        Constraint::Length(2),                         // #: jump key
        Constraint::Length(max_project_width as u16),  // Project: auto-sized
        Constraint::Length(max_worktree_width as u16), // Worktree: auto-sized
        Constraint::Length(max_git_width as u16),      // Git: auto-sized
    ];

    if show_pr_column {
        constraints.push(Constraint::Length(max_pr_width as u16)); // PR: auto-sized
    }

    constraints.extend(vec![
        Constraint::Length(8),  // Status: fixed (icons)
        Constraint::Length(10), // Time: HH:MM:SS + padding
        Constraint::Fill(1),    // Title: takes remaining space
    ]);

    let table = Table::new(rows, constraints)
        .header(header)
        .block(Block::default())
        .row_highlight_style(Style::default().bg(app.palette.highlight_row_bg))
        .highlight_symbol("> ");

    f.render_stateful_widget(table, area, &mut app.table_state);
}

fn render_preview(f: &mut Frame, app: &mut App, area: Rect) {
    // Get info about the selected agent for the title
    let selected_agent = app
        .table_state
        .selected()
        .and_then(|idx| app.agents.get(idx));

    let (title, title_style, border_style) = if app.input_mode {
        let worktree_name = selected_agent
            .map(|a| app.extract_worktree_name(a).0)
            .unwrap_or_default();
        (
            format!(" INPUT: {} ", worktree_name),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Green),
        )
    } else if let Some(agent) = selected_agent {
        let worktree_name = app.extract_worktree_name(agent).0;
        (
            format!(" Preview: {} ", worktree_name),
            Style::default().fg(Color::Cyan),
            Style::default().fg(app.palette.dimmed),
        )
    } else {
        (
            " Preview ".to_string(),
            Style::default().fg(Color::Cyan),
            Style::default().fg(app.palette.dimmed),
        )
    };

    let block = Block::bordered()
        .title(title)
        .title_style(title_style)
        .border_style(border_style);

    // Calculate the inner area to determine scroll offset
    let inner_area = block.inner(area);

    // Update preview height for scroll calculations
    app.preview_height = inner_area.height;

    // Get preview content or show placeholder
    let (text, line_count) = match (&app.preview, selected_agent) {
        (Some(preview), Some(_)) => {
            let trimmed = preview.trim_end();
            if trimmed.is_empty() {
                (Text::raw("(empty output)"), 1u16)
            } else {
                // Parse ANSI escape sequences to get colored text
                match trimmed.into_text() {
                    Ok(text) => {
                        let count = text.lines.len() as u16;
                        (text, count)
                    }
                    Err(_) => {
                        // Fallback to plain text if ANSI parsing fails
                        let count = trimmed.lines().count() as u16;
                        (Text::raw(trimmed), count)
                    }
                }
            }
        }
        (None, Some(_)) => (Text::raw("(pane not available)"), 1),
        (_, None) => (Text::raw("(no agent selected)"), 1),
    };

    // Update line count for scroll calculations
    app.preview_line_count = line_count;

    // Calculate scroll offset: use manual scroll if set, otherwise auto-scroll to bottom
    let max_scroll = line_count.saturating_sub(inner_area.height);
    let scroll_offset = app.preview_scroll.unwrap_or(max_scroll);

    let paragraph = Paragraph::new(text).block(block).scroll((scroll_offset, 0));

    f.render_widget(paragraph, area);
}
