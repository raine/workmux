//! Dashboard view rendering (table, preview, footer).

use ansi_to_tui::IntoText;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Cell, Paragraph, Row, Table},
};
use std::collections::{BTreeMap, HashSet};

use crate::agent_display::strip_oc_title_prefix;
use crate::config::DashboardColumn;

use super::super::app::{App, DashboardTab};
use super::format;
use super::format::{format_git_status, format_pr_status, truncate};
use super::worktree::{render_worktree_preview, render_worktree_table};

/// Render the tab header line showing Agents | Worktrees with active tab highlighted.
fn render_tab_header(f: &mut Frame, app: &App, area: Rect) {
    let active_style = Style::default()
        .fg(app.palette.header)
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(app.palette.dimmed);
    let pipe_style = Style::default().fg(app.palette.border);
    let rule_style = Style::default().fg(app.palette.border);

    let (agents_style, worktrees_style) = match app.active_tab {
        DashboardTab::Agents => (active_style, inactive_style),
        DashboardTab::Worktrees => (inactive_style, active_style),
    };

    let tabs_spans = vec![
        Span::raw("  "),
        Span::styled("Agents", agents_style),
        Span::styled(" \u{2502} ", pipe_style),
        Span::styled("Worktrees", worktrees_style),
    ];
    let rule = Line::from(Span::styled(
        "\u{2500}".repeat(area.width as usize),
        rule_style,
    ));

    if app.show_sidebar_tip {
        let tip_new = Style::default().fg(app.palette.header);
        let tip_text = Style::default().fg(app.palette.text);
        let tip_accent = Style::default().fg(app.palette.accent);
        let tip_line = Line::from(vec![
            Span::styled("New: ", tip_new),
            Span::styled("Check out ", tip_text),
            Span::styled("workmux sidebar ", tip_accent),
        ]);
        let tip_width = 32u16;
        let cols = Layout::horizontal([Constraint::Fill(1), Constraint::Length(tip_width)])
            .split(Rect::new(area.x, area.y, area.width, 1));
        f.render_widget(Paragraph::new(Line::from(tabs_spans)), cols[0]);
        f.render_widget(Paragraph::new(tip_line), cols[1]);
        f.render_widget(
            Paragraph::new(rule),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    } else {
        f.render_widget(Paragraph::new(vec![Line::from(tabs_spans), rule]), area);
    }
}

/// Render the dashboard view (table + preview + footer).
pub fn render_dashboard(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Check if backend supports preview
    let supports_preview = app.mux.supports_preview();

    // Outer layout: fixed-height tab header and footer, flexible content area.
    // Fill(1) guarantees the content takes exactly the remaining space.
    let outer = Layout::vertical([
        Constraint::Length(2), // Tab header + spacer
        Constraint::Fill(1),   // Content (table + optional preview)
        Constraint::Length(1), // Footer
    ])
    .split(area);

    let tab_area = outer[0];
    let content_area = outer[1];
    let footer_area = outer[2];

    // Split content area into table + preview (or just table if no preview)
    let (table_area, preview_area) = if !supports_preview {
        (content_area, None)
    } else {
        let table_size = 100u16.saturating_sub(app.preview_size as u16);
        // Use Fill() proportional constraints to split space safely without overflow
        let content_chunks = Layout::vertical([
            Constraint::Fill(table_size),              // Table
            Constraint::Fill(app.preview_size as u16), // Preview
        ])
        .split(content_area);
        (content_chunks[0], Some(content_chunks[1]))
    };

    // Tab header
    render_tab_header(f, app, tab_area);

    // Table (agents or worktrees based on active tab)
    match app.active_tab {
        DashboardTab::Agents => render_table(f, app, table_area),
        DashboardTab::Worktrees => render_worktree_table(f, app, table_area),
    }

    // Preview (only for backends that support it)
    if let Some(preview) = preview_area {
        match app.active_tab {
            DashboardTab::Agents => render_preview(f, app, preview),
            DashboardTab::Worktrees => render_worktree_preview(f, app, preview),
        }
    }

    // Footer - show status message if active, otherwise mode-specific help
    if let Some((msg, _)) = &app.status_message {
        let p = &app.palette;
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                format!("  {msg}"),
                Style::default().fg(p.text),
            )])),
            footer_area,
        );
    } else {
        match app.active_tab {
            DashboardTab::Agents => {
                if app.filter_active {
                    f.render_widget(render_footer_filter(app), footer_area);
                } else if app.input_mode {
                    f.render_widget(render_footer_input(app), footer_area);
                } else {
                    render_footer_normal(f, app, footer_area);
                }
            }
            DashboardTab::Worktrees => {
                if app.worktree_filter_active {
                    f.render_widget(render_worktree_footer_filter(app), footer_area);
                } else {
                    render_worktree_footer_normal(f, app, footer_area);
                }
            }
        }
    }
}

fn render_table(f: &mut Frame, app: &mut App, area: Rect) {
    // Show the GitHub column when at least one agent has a PR or checks.
    let show_pr_column = app.has_any_github_status();
    let show_check_counts = app.config.dashboard.show_check_counts();
    // Header cells, row cells and width constraints are all derived from this
    // one list, so a reordered config can never desynchronise them.
    let trailing_columns = app.config.dashboard.columns();

    let header = format::resource_table_header(
        format::ResourceHeaderState {
            palette: &app.palette,
            spinner_frame: app.spinner_frame,
            git_fetching: app
                .is_git_fetching
                .load(std::sync::atomic::Ordering::Relaxed),
            pr_fetching: app.is_pr_fetching(),
        },
        show_pr_column,
        &trailing_columns
            .iter()
            .map(|column| match column {
                DashboardColumn::Window => "Win",
                DashboardColumn::Status => "Status",
                DashboardColumn::Time => "Time",
                DashboardColumn::Title => "Title",
            })
            .collect::<Vec<_>>(),
    );

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
                format!(" ({})", pos)
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
            let worktree_base = truncate(
                &worktree_name,
                25usize.saturating_sub(pane_suffix.chars().count()),
            );
            let worktree_suffix = pane_suffix;
            let title = agent
                .pane_title
                .as_ref()
                .map(|t| {
                    let t = strip_oc_title_prefix(t);
                    t.strip_prefix("... ").unwrap_or(t).to_string()
                })
                .unwrap_or_default();
            let status_spans = app.get_status_display(agent);
            // Blank rather than a placeholder when the backend does not number
            // its windows, so the column stays quiet on wezterm/zellij/kitty
            let window_index_label = agent
                .window_index
                .map(|index| index.to_string())
                .unwrap_or_default();
            let elapsed = app.get_elapsed(agent);
            let duration = elapsed
                .map(|d| app.format_duration(d))
                .unwrap_or_else(|| "-".to_string());
            let duration_line = format::elapsed_time_line(duration, elapsed, &app.palette);

            // Get git status for this worktree (may be None if not yet fetched)
            let git_status = app.git_statuses.get(&agent.path);
            let git_spans = format_git_status(git_status, app.spinner_frame, &app.palette);

            // Get PR status for this agent (only if column is shown)
            let pr_spans = if show_pr_column {
                let pr = app.get_pr_for_agent(agent);
                let checks = app.get_checks_for_agent(agent);
                Some(format_pr_status(
                    pr,
                    checks,
                    show_check_counts,
                    app.spinner_frame,
                    &app.palette,
                ))
            } else {
                None
            };

            (
                jump_key,
                project,
                worktree_display,
                worktree_base,
                worktree_suffix,
                is_main,
                is_current,
                git_spans,
                pr_spans,
                status_spans,
                duration_line,
                title,
                window_index_label,
            )
        })
        .collect();

    // Calculate max project name width (with padding, capped)
    let project_names: Vec<String> = row_data.iter().map(|r| r.1.clone()).collect();
    let max_project_width = format::calc_column_width(&project_names, 5, 20, 2);

    // Calculate max worktree name width (with padding, capped)
    // Use at least 8 to fit the "Worktree" header, at most 25 to keep layout compact
    let worktree_names: Vec<String> = row_data.iter().map(|r| r.2.clone()).collect();
    let max_worktree_width = format::calc_column_width(&worktree_names, 8, 25, 1);

    // Calculate max git status width (sum of all span character counts)
    // Use chars().count() instead of len() because Nerd Font icons are multi-byte
    let max_git_width = row_data
        .iter()
        .map(|(_, _, _, _, _, _, _, git_spans, _, _, _, _, _)| {
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
            .filter_map(|(_, _, _, _, _, _, _, _, pr_spans, _, _, _, _)| pr_spans.as_ref())
            .map(|spans| {
                spans
                    .iter()
                    .map(|(text, _)| text.chars().count())
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(4)
            .clamp(4, 20) // Accommodate check icons + counts + inline timer
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
                _worktree_display,
                worktree_base,
                worktree_suffix,
                is_main,
                is_current,
                git_spans,
                pr_spans,
                status_spans,
                duration_line,
                title,
                window_index_label,
            )| {
                let worktree_style = format::make_row_style(is_current, is_main, &app.palette);

                // Worktree name with dimmed pane suffix
                let worktree_line = if worktree_suffix.is_empty() {
                    Line::from(Span::styled(worktree_base, worktree_style))
                } else {
                    Line::from(vec![
                        Span::styled(worktree_base, worktree_style),
                        Span::styled(worktree_suffix, Style::default().fg(app.palette.dimmed)),
                    ])
                };

                // Convert git spans to a Line
                let git_line = format::spans_to_line(git_spans);

                let mut cells = vec![
                    Cell::from(jump_key).style(Style::default().fg(app.palette.keycap)),
                    Cell::from(project),
                    Cell::from(worktree_line),
                    Cell::from(git_line),
                ];

                // Add PR cell if column is shown
                if let Some(pr_spans) = pr_spans {
                    let pr_line = format::spans_to_line(pr_spans);
                    cells.push(Cell::from(pr_line));
                }

                let status_line = format::spans_to_line(status_spans);
                let mut window_cell = Some(window_index_label);
                let mut status_cell = Some(status_line);
                let mut duration_cell = Some(duration_line);
                let mut title_cell = Some(title);
                for column in &trailing_columns {
                    // take() so each column is moved out once: columns() already
                    // dropped duplicates, this keeps the compiler happy about it
                    match column {
                        DashboardColumn::Window => {
                            if let Some(label) = window_cell.take() {
                                cells.push(
                                    Cell::from(label)
                                        .style(Style::default().fg(app.palette.dimmed)),
                                );
                            }
                        }
                        DashboardColumn::Status => {
                            if let Some(line) = status_cell.take() {
                                cells.push(Cell::from(line));
                            }
                        }
                        DashboardColumn::Time => {
                            if let Some(line) = duration_cell.take() {
                                cells.push(Cell::from(line));
                            }
                        }
                        DashboardColumn::Title => {
                            if let Some(line) = title_cell.take() {
                                cells.push(Cell::from(line));
                            }
                        }
                    }
                }

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
        Constraint::Length(2),                    // #: jump key
        Constraint::Length(max_project_width),    // Project: auto-sized
        Constraint::Length(max_worktree_width),   // Worktree: auto-sized
        Constraint::Length(max_git_width as u16), // Git: auto-sized
    ];

    if show_pr_column {
        constraints.push(Constraint::Length(max_pr_width as u16)); // PR: auto-sized
    }

    constraints.extend(trailing_columns.iter().map(|column| match column {
        DashboardColumn::Window => Constraint::Length(4), // window index
        DashboardColumn::Status => Constraint::Length(8), // fixed (icons)
        DashboardColumn::Time => Constraint::Length(10),  // HH:MM:SS + padding
        DashboardColumn::Title => Constraint::Fill(1),    // takes remaining space
    }));

    let highlight_symbol = Text::from(Line::from(Span::styled(
        "▌ ",
        Style::default().fg(app.palette.info),
    )));
    let table = Table::new(rows, constraints)
        .header(header)
        .block(Block::default())
        .row_highlight_style(Style::default().bg(app.palette.highlight_row_bg))
        .highlight_symbol(highlight_symbol);

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
                .fg(app.palette.success)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(app.palette.success),
        )
    } else if let Some(agent) = selected_agent {
        let worktree_name = app.extract_worktree_name(agent).0;
        (
            format!(" Preview: {} ", worktree_name),
            Style::default().fg(app.palette.header),
            Style::default().fg(app.palette.border),
        )
    } else {
        (
            " Preview ".to_string(),
            Style::default().fg(app.palette.header),
            Style::default().fg(app.palette.border),
        )
    };

    let mut block = Block::bordered()
        .title(title)
        .title_style(title_style)
        .border_style(border_style);

    // Add right-aligned PR check detail to the preview border
    if let Some(agent) = selected_agent
        && let Some(pr) = app.get_pr_for_agent(agent)
    {
        let detail_spans = format::format_pr_details(pr, app.spinner_frame, &app.palette);
        if !detail_spans.is_empty() {
            let mut title_spans = vec![Span::raw(" ")];
            title_spans.extend(detail_spans);
            title_spans.push(Span::raw(" "));
            block = block.title_top(Line::from(title_spans).right_aligned());
        }
    }

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
                        // Fallback: strip ANSI escapes to prevent raw control
                        // sequences from corrupting the terminal display
                        let safe = super::super::ansi::strip_ansi_escapes(trimmed);
                        let count = safe.lines().count() as u16;
                        (Text::raw(safe), count)
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

// ── Footer rendering ────────────────────────────────────────────

/// Filter mode footer
fn render_footer_filter_mode<'a>(app: &'a App, filter_text: &'a str) -> Paragraph<'a> {
    Paragraph::new(Line::from(vec![
        Span::styled(
            "  /",
            Style::default()
                .fg(app.palette.keycap)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(filter_text),
        Span::styled("_", Style::default().fg(app.palette.keycap)),
        Span::raw("  "),
        Span::styled("Enter", Style::default().fg(app.palette.dimmed)),
        Span::raw(" accept  "),
        Span::styled("Esc", Style::default().fg(app.palette.dimmed)),
        Span::raw(" clear"),
    ]))
}

fn render_footer_filter<'a>(app: &'a App) -> Paragraph<'a> {
    render_footer_filter_mode(app, app.filter_text.as_str())
}

/// Input mode footer
fn render_footer_input<'a>(app: &'a App) -> Paragraph<'a> {
    Paragraph::new(Line::from(vec![
        Span::styled(
            "  INPUT MODE",
            Style::default()
                .fg(app.palette.success)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" \u{2014} type to send keys to agent  "),
        Span::styled("Esc", Style::default().fg(app.palette.keycap)),
        Span::raw(" exit"),
    ]))
}

// ── Shared footer helpers ───────────────────────────────────────

fn footer_cmd(k: &str, l: &str, dimmed: Style, bold_text: Style) -> Vec<Span<'static>> {
    vec![
        Span::styled(k.to_string(), dimmed),
        Span::styled(format!(" {l}"), bold_text),
    ]
}

fn footer_toggle(
    k: &str,
    l: &str,
    v: &str,
    active: bool,
    dimmed: Style,
    bold_text: Style,
    active_style: Style,
) -> Vec<Span<'static>> {
    vec![
        Span::styled(k.to_string(), dimmed),
        Span::styled(format!(" {l} "), bold_text),
        Span::styled(format!("({v})"), if active { active_style } else { dimmed }),
    ]
}

fn footer_pipe(pipe_style: Style) -> Span<'static> {
    Span::styled(" \u{2502} ", pipe_style)
}

fn render_right_help(f: &mut Frame, area: Rect, dimmed: Style, bold_text: Style) {
    let right = Line::from(vec![
        Span::styled("?", dimmed),
        Span::styled(" Help ", bold_text),
    ]);
    let cols = Layout::horizontal([Constraint::Fill(1), Constraint::Length(7)]).split(area);
    f.render_widget(Paragraph::new(right), cols[1]);
}

fn render_pinned_footer(
    f: &mut Frame,
    area: Rect,
    items: &[Vec<Span<'static>>],
    dimmed: Style,
    bold_text: Style,
    pipe_style: Style,
) {
    let mut s: Vec<Span<'static>> = vec![Span::raw("  ")];
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            s.push(footer_pipe(pipe_style));
        }
        s.extend(item.iter().cloned());
    }

    let cols = Layout::horizontal([Constraint::Fill(1), Constraint::Length(7)]).split(area);
    f.render_widget(Paragraph::new(Line::from(s)), cols[0]);
    render_right_help(f, area, dimmed, bold_text);
}

/// Normal mode footer with right-pinned help
fn render_footer_normal(f: &mut Frame, app: &App, area: Rect) {
    let p = &app.palette;

    let dimmed = Style::default().fg(p.dimmed);
    let bold_text = Style::default().fg(p.text).add_modifier(Modifier::BOLD);
    let pipe_style = Style::default().fg(p.border);
    let active_style = Style::default().fg(p.info);

    let sort = app.sort_mode.label();
    let scope = app.scope_mode.label();
    let stale = if app.hide_stale { "hidden" } else { "shown" };
    let scope_active = scope != "all";
    let stale_active = stale == "hidden";

    let mut items: Vec<Vec<Span<'static>>> = vec![
        footer_cmd("i", "Input", dimmed, bold_text),
        footer_cmd("d", "Diff", dimmed, bold_text),
        footer_cmd("o", "PR", dimmed, bold_text),
        footer_cmd("1-9", "Jump", dimmed, bold_text),
        footer_toggle("s", "Sort", sort, true, dimmed, bold_text, active_style),
        footer_toggle(
            "F",
            "Scope",
            scope,
            scope_active,
            dimmed,
            bold_text,
            active_style,
        ),
        footer_toggle(
            "f",
            "Stale",
            stale,
            stale_active,
            dimmed,
            bold_text,
            active_style,
        ),
    ];
    if !app.filter_text.is_empty() {
        items.push(footer_cmd("/", &app.filter_text, dimmed, bold_text));
    }
    items.push(footer_cmd("Tab", "Worktrees", dimmed, bold_text));
    items.push(footer_cmd("q", "Quit", dimmed, bold_text));

    render_pinned_footer(f, area, &items, dimmed, bold_text, pipe_style);
}

/// Worktree filter mode footer
fn render_worktree_footer_filter<'a>(app: &'a App) -> Paragraph<'a> {
    render_footer_filter_mode(app, app.worktree_filter_text.as_str())
}

/// Worktree normal mode footer
fn render_worktree_footer_normal(f: &mut Frame, app: &App, area: Rect) {
    let p = &app.palette;

    let dimmed = Style::default().fg(p.dimmed);
    let bold_text = Style::default().fg(p.text).add_modifier(Modifier::BOLD);
    let pipe_style = Style::default().fg(p.border);
    let active_style = Style::default().fg(p.accent);

    let sort = app.worktree_sort_mode.label();

    let mut items: Vec<Vec<Span<'static>>> = vec![
        footer_cmd("a", "Add", dimmed, bold_text),
        footer_cmd("r", "Remove", dimmed, bold_text),
        footer_cmd("R", "Sweep", dimmed, bold_text),
        footer_cmd("c", "Close", dimmed, bold_text),
        footer_cmd("o", "PR", dimmed, bold_text),
        footer_cmd("1-9", "Jump", dimmed, bold_text),
        footer_toggle("s", "Sort", sort, true, dimmed, bold_text, active_style),
        footer_cmd("p", "Project", dimmed, bold_text),
    ];
    if !app.worktree_filter_text.is_empty() {
        items.push(footer_cmd(
            "/",
            &app.worktree_filter_text,
            dimmed,
            bold_text,
        ));
    }
    items.push(footer_cmd("Tab", "Agents", dimmed, bold_text));
    items.push(footer_cmd("q", "Quit", dimmed, bold_text));

    render_pinned_footer(f, area, &items, dimmed, bold_text, pipe_style);
}
