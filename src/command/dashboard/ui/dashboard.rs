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
use crate::config::AgentColumn;

use super::super::app::{App, DashboardTab};
use super::format;
use super::format::{format_git_status, format_pr_status, truncate};
use super::theme::ThemePalette;
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

/// Per-agent cell content, assembled before the configured column order is
/// applied so every column reads from the same row.
struct AgentRowData {
    jump_key: String,
    project: String,
    worktree_display: String,
    worktree_base: String,
    worktree_suffix: String,
    is_main: bool,
    is_current: bool,
    git_spans: Vec<(String, Style)>,
    pr_spans: Option<Vec<(String, Style)>>,
    status_spans: Vec<(String, Style)>,
    duration_line: Line<'static>,
    title: String,
}

/// Auto-sized widths of the agents table columns that size to their content.
struct AgentColumnWidths {
    project: u16,
    worktree: u16,
    git: u16,
    pr: u16,
    title: u16,
}

/// Columns to render, given the configured order. The PR column carries content
/// only once an agent has GitHub status, so it drops out until then, unless it
/// is the only thing left to show and dropping it would blank the table.
fn visible_columns(configured: &[AgentColumn], show_pr_column: bool) -> Vec<AgentColumn> {
    let visible: Vec<AgentColumn> = configured
        .iter()
        .copied()
        .filter(|column| *column != AgentColumn::Pr || show_pr_column)
        .collect();

    if visible.is_empty() {
        configured.to_vec()
    } else {
        visible
    }
}

/// Build the cell of one configured column for one agent row.
fn agent_cell(column: AgentColumn, row: &AgentRowData, palette: &ThemePalette) -> Cell<'static> {
    match column {
        AgentColumn::Number => {
            Cell::from(row.jump_key.clone()).style(Style::default().fg(palette.keycap))
        }
        AgentColumn::Project => Cell::from(row.project.clone()),
        AgentColumn::Worktree => {
            let worktree_style = format::make_row_style(row.is_current, row.is_main, palette);
            // Worktree name with dimmed pane suffix
            let worktree_line = if row.worktree_suffix.is_empty() {
                Line::from(Span::styled(row.worktree_base.clone(), worktree_style))
            } else {
                Line::from(vec![
                    Span::styled(row.worktree_base.clone(), worktree_style),
                    Span::styled(
                        row.worktree_suffix.clone(),
                        Style::default().fg(palette.dimmed),
                    ),
                ])
            };
            Cell::from(worktree_line)
        }
        AgentColumn::Git => Cell::from(format::spans_to_line(row.git_spans.clone())),
        AgentColumn::Pr => Cell::from(format::spans_to_line(
            row.pr_spans.clone().unwrap_or_default(),
        )),
        AgentColumn::Status => Cell::from(format::spans_to_line(row.status_spans.clone())),
        AgentColumn::Time => Cell::from(row.duration_line.clone()),
        AgentColumn::Title => Cell::from(row.title.clone()),
    }
}

/// Assemble the agents table. Header cells, row cells and width constraints are
/// all derived from `columns`, so a reordered config can never desynchronise
/// them.
fn build_agent_table(
    columns: &[AgentColumn],
    row_data: Vec<AgentRowData>,
    widths: AgentColumnWidths,
    header_state: format::ResourceHeaderState<'_>,
) -> Table<'static> {
    let palette = header_state.palette;

    let header_cells: Vec<format::ResourceHeaderCell> = columns
        .iter()
        .map(|column| match column {
            AgentColumn::Number => format::ResourceHeaderCell::Plain("#"),
            AgentColumn::Project => format::ResourceHeaderCell::Plain("Project"),
            AgentColumn::Worktree => format::ResourceHeaderCell::Plain("Worktree"),
            AgentColumn::Git => format::ResourceHeaderCell::Git,
            AgentColumn::Pr => format::ResourceHeaderCell::Pr,
            AgentColumn::Status => format::ResourceHeaderCell::Plain("Status"),
            AgentColumn::Time => format::ResourceHeaderCell::Plain("Time"),
            AgentColumn::Title => format::ResourceHeaderCell::Plain("Title"),
        })
        .collect();

    let rows: Vec<Row> = row_data
        .into_iter()
        .map(|row_data| {
            let cells: Vec<Cell> = columns
                .iter()
                .map(|column| agent_cell(*column, &row_data, palette))
                .collect();

            let row = Row::new(cells);
            // Subtle background for the active worktree row
            if row_data.is_current {
                row.style(Style::default().bg(palette.current_row_bg))
            } else {
                row
            }
        })
        .collect();

    let last_column = columns.len().saturating_sub(1);
    let constraints: Vec<Constraint> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| match column {
            AgentColumn::Number => Constraint::Length(2), // jump key
            AgentColumn::Project => Constraint::Length(widths.project), // auto-sized
            AgentColumn::Worktree => Constraint::Length(widths.worktree), // auto-sized
            AgentColumn::Git => Constraint::Length(widths.git), // auto-sized
            AgentColumn::Pr => Constraint::Length(widths.pr), // auto-sized
            AgentColumn::Status => Constraint::Length(8), // fixed (icons)
            AgentColumn::Time => Constraint::Length(10),  // HH:MM:SS + padding
            // A Fill column absorbs slack at its own position, which would
            // strand every column after it against the right edge. Only the
            // trailing title takes the remaining width; elsewhere it sizes to
            // its content like any other column, leaving the slack at the end.
            AgentColumn::Title if index == last_column => Constraint::Fill(1),
            AgentColumn::Title => Constraint::Length(widths.title),
        })
        .collect();

    let highlight_symbol = Text::from(Line::from(Span::styled(
        "▌ ",
        Style::default().fg(palette.info),
    )));
    let row_highlight_style = Style::default().bg(palette.highlight_row_bg);

    Table::new(rows, constraints)
        .header(format::resource_table_header(header_state, &header_cells))
        .block(Block::default())
        .row_highlight_style(row_highlight_style)
        .highlight_symbol(highlight_symbol)
}

fn render_table(f: &mut Frame, app: &mut App, area: Rect) {
    // Show the GitHub column when at least one agent has a PR or checks.
    let show_pr_column = app.has_any_github_status();
    let show_check_counts = app.config.dashboard.show_check_counts();
    let columns = visible_columns(&app.config.dashboard.agent_columns(), show_pr_column);

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

            AgentRowData {
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
            }
        })
        .collect();

    // Calculate max project name width (with padding, capped)
    let project_names: Vec<String> = row_data.iter().map(|r| r.project.clone()).collect();
    let max_project_width = format::calc_column_width(&project_names, 5, 20, 2);

    // Calculate max worktree name width (with padding, capped)
    // Use at least 8 to fit the "Worktree" header, at most 25 to keep layout compact
    let worktree_names: Vec<String> = row_data
        .iter()
        .map(|r| r.worktree_display.clone())
        .collect();
    let max_worktree_width = format::calc_column_width(&worktree_names, 8, 25, 1);

    // Calculate max git status width (sum of all span character counts)
    // Use chars().count() instead of len() because Nerd Font icons are multi-byte
    let max_git_width = row_data
        .iter()
        .map(|r| {
            r.git_spans
                .iter()
                .map(|(text, _)| text.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(4)
        .clamp(4, 30) // min 4, max 30 (increased for base branch)
        + 1; // padding

    // Calculate max PR status width (only if showing PR column)
    let max_pr_width = row_data
        .iter()
        .filter_map(|r| r.pr_spans.as_ref())
        .map(|spans| {
            spans
                .iter()
                .map(|(text, _)| text.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(4)
        .clamp(4, 20) // Accommodate check icons + counts + inline timer
        + 1;

    // Title width, used when the title is not the trailing column and so has to
    // size to its content. Capped to leave room for the columns after it.
    let titles: Vec<String> = row_data.iter().map(|r| r.title.clone()).collect();
    let max_title_width = format::calc_column_width(&titles, 5, 60, 1);

    let table = build_agent_table(
        &columns,
        row_data,
        AgentColumnWidths {
            project: max_project_width,
            worktree: max_worktree_width,
            git: max_git_width as u16,
            pr: max_pr_width as u16,
            title: max_title_width,
        },
        format::ResourceHeaderState {
            palette: &app.palette,
            spinner_frame: app.spinner_frame,
            git_fetching: app
                .is_git_fetching
                .load(std::sync::atomic::Ordering::Relaxed),
            pr_fetching: app.is_pr_fetching(),
        },
    );

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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::TableState;

    use super::*;
    use crate::config::{ThemeConfig, ThemeMode};

    fn palette() -> ThemePalette {
        ThemePalette::from_config(&ThemeConfig::default(), ThemeMode::Dark)
    }

    fn row(palette: &ThemePalette) -> AgentRowData {
        AgentRowData {
            jump_key: "1".to_string(),
            project: "proj".to_string(),
            worktree_display: "wt".to_string(),
            worktree_base: "wt".to_string(),
            worktree_suffix: String::new(),
            is_main: false,
            is_current: false,
            git_spans: vec![("+1".to_string(), Style::default())],
            pr_spans: Some(vec![("#7".to_string(), Style::default())]),
            status_spans: vec![("work".to_string(), Style::default())],
            duration_line: format::elapsed_time_line("00:42".to_string(), Some(42), palette),
            title: "the title".to_string(),
        }
    }

    /// Render the agents table and return one line of the output buffer.
    fn render_line(columns: &[AgentColumn], line: u16) -> String {
        let palette = palette();
        let table = build_agent_table(
            columns,
            vec![row(&palette)],
            AgentColumnWidths {
                project: 8,
                worktree: 9,
                git: 6,
                pr: 5,
                title: 12,
            },
            format::ResourceHeaderState {
                palette: &palette,
                spinner_frame: 0,
                git_fetching: false,
                pr_fetching: false,
            },
        );

        let width = 80u16;
        let mut terminal = Terminal::new(TestBackend::new(width, 4)).unwrap();
        terminal
            .draw(|f| f.render_stateful_widget(table, f.area(), &mut TableState::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        (0..width)
            .map(|x| buffer[(x, line)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// Reordered list, including the `#` jump key away from its usual place.
    const REORDERED: [AgentColumn; 4] = [
        AgentColumn::Title,
        AgentColumn::Status,
        AgentColumn::Number,
        AgentColumn::Project,
    ];

    #[test]
    fn agents_table_header_follows_column_order() {
        assert_eq!(
            render_line(&crate::config::DEFAULT_AGENT_COLUMNS, 0),
            "#  Project  Worktree  Git    PR    Status   Time       Title"
        );
        assert_eq!(
            render_line(&REORDERED, 0),
            "Title        Status   #  Project"
        );
    }

    #[test]
    fn agents_table_rows_follow_column_order() {
        assert_eq!(
            render_line(&crate::config::DEFAULT_AGENT_COLUMNS, 1),
            "1  proj     wt        +1     #7    work     00:42      the title"
        );
        assert_eq!(render_line(&REORDERED, 1), "the title    work     1  proj");
    }

    #[test]
    fn agents_table_omits_unlisted_columns() {
        assert_eq!(
            render_line(&[AgentColumn::Worktree, AgentColumn::Title], 0),
            "Worktree  Title"
        );
        assert_eq!(
            render_line(&[AgentColumn::Worktree, AgentColumn::Title], 1),
            "wt        the title"
        );
    }

    #[test]
    fn agents_table_keeps_columns_after_a_title_contiguous() {
        // A Fill title would push Status and Project to the right edge
        assert_eq!(
            render_line(&[AgentColumn::Title, AgentColumn::Status], 0),
            "Title        Status"
        );
        assert_eq!(
            render_line(&[AgentColumn::Title, AgentColumn::Status], 1),
            "the title    work"
        );
    }

    #[test]
    fn pr_column_hidden_until_an_agent_has_github_status() {
        assert_eq!(
            visible_columns(&[AgentColumn::Worktree, AgentColumn::Pr], false),
            vec![AgentColumn::Worktree]
        );
        assert_eq!(
            visible_columns(&[AgentColumn::Worktree, AgentColumn::Pr], true),
            vec![AgentColumn::Worktree, AgentColumn::Pr]
        );
    }

    #[test]
    fn hiding_the_pr_column_never_empties_the_table() {
        assert_eq!(
            visible_columns(&[AgentColumn::Pr], false),
            vec![AgentColumn::Pr]
        );
    }
}
