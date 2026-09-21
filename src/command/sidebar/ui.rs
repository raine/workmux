//! Rendering for the sidebar TUI.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Padding, Paragraph, Wrap};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_width::UnicodeWidthChar;

use crate::git::GitStatus;
use crate::multiplexer::{AgentPane, AgentStatus};
use crate::tmux_style;
use crate::ui::theme::ThemePalette;

use super::app::{SidebarApp, SidebarFilterMode, SidebarLayoutMode};
use super::template::TokenId;
use super::template::context::RowContext;
use super::template::layout::{
    RenderOptions, is_blank_template_line, render_line, render_line_with_options,
};
use super::template::parser::Token;

/// Compute pane suffixes like " (1)", " (2)" for agents sharing the same window.
fn compute_pane_suffixes(agents: &[AgentPane]) -> Vec<String> {
    let mut counts: HashMap<(&str, &str), usize> = HashMap::new();
    for agent in agents {
        *counts
            .entry((&agent.session, &agent.window_name))
            .or_default() += 1;
    }

    let mut positions: HashMap<(&str, &str), usize> = HashMap::new();
    agents
        .iter()
        .map(|agent| {
            let key = (agent.session.as_str(), agent.window_name.as_str());
            if counts[&key] > 1 {
                let pos = positions.entry(key).or_default();
                *pos += 1;
                format!("({})", pos)
            } else {
                String::new()
            }
        })
        .collect()
}

fn stale_color(palette: &ThemePalette, is_stale: bool, color: Color) -> Color {
    if is_stale { palette.dimmed } else { color }
}

type StyledFragment = (String, Style);

struct GitDiffColors {
    success: Color,
    danger: Color,
    accent: Color,
}

fn git_diff_colors(palette: &ThemePalette, is_stale: bool) -> GitDiffColors {
    GitDiffColors {
        success: stale_color(palette, is_stale, palette.success),
        danger: stale_color(palette, is_stale, palette.danger),
        accent: stale_color(palette, is_stale, palette.accent),
    }
}

fn added_removed_fragments(
    added_count: usize,
    removed_count: usize,
    added_style: Style,
    removed_style: Style,
) -> (Option<StyledFragment>, Option<StyledFragment>) {
    let added = (added_count > 0).then(|| (format!("+{}", added_count), added_style));
    let removed = (removed_count > 0).then(|| (format!("-{}", removed_count), removed_style));
    (added, removed)
}

fn diff_variant_ladder(
    prefix: Option<&StyledFragment>,
    added: Option<StyledFragment>,
    removed: Option<StyledFragment>,
    icon_only_fallback: bool,
) -> Vec<Vec<StyledFragment>> {
    let prepend = |mut parts: Vec<StyledFragment>| -> Vec<StyledFragment> {
        if let Some(p) = prefix {
            parts.insert(0, p.clone());
        }
        parts
    };

    let mut variants = Vec::new();
    match (&added, &removed) {
        (Some(a), Some(r)) => {
            variants.push(prepend(vec![a.clone(), r.clone()]));
            variants.push(prepend(vec![a.clone()]));
            variants.push(prepend(vec![r.clone()]));
        }
        (Some(a), None) => variants.push(prepend(vec![a.clone()])),
        (None, Some(r)) => variants.push(prepend(vec![r.clone()])),
        (None, None) => {}
    }
    if icon_only_fallback && let Some(p) = prefix {
        variants.push(vec![p.clone()]);
    }
    variants
}

struct SidebarListSetup {
    now_secs: u64,
    pane_suffixes: Vec<String>,
    selected_idx: Option<usize>,
}

fn sidebar_list_setup(app: &SidebarApp) -> SidebarListSetup {
    SidebarListSetup {
        now_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        pane_suffixes: compute_pane_suffixes(&app.agents),
        selected_idx: app.list_state.selected(),
    }
}

fn build_row_contexts<'a>(app: &'a SidebarApp, setup: &SidebarListSetup) -> Vec<RowContext<'a>> {
    app.agents
        .iter()
        .enumerate()
        .map(|(idx, agent)| {
            RowContext::build(
                app,
                agent,
                idx,
                &setup.pane_suffixes,
                setup.now_secs,
                setup.selected_idx,
            )
        })
        .collect()
}

fn apply_selection_bg(spans: &mut [Span<'static>], bg: Color) {
    for span in spans {
        if span.style.bg.is_none() {
            span.style = span.style.bg(bg);
        }
    }
}

/// Format GitHub check status for sidebar display, fitting within `available_width`.
pub(crate) fn format_sidebar_check_status(
    summary: Option<&crate::github::CheckSummary>,
    palette: &ThemePalette,
    is_stale: bool,
    spinner_frame: u8,
    available_width: usize,
) -> (Vec<(String, Style)>, usize) {
    let Some(summary) = summary else {
        return (Vec::new(), 0);
    };
    let checks = &summary.state;

    let check_icons = crate::nerdfont::check_icons();
    let (icon, color, counts) = match checks {
        crate::github::CheckState::Success => {
            (check_icons.success.to_string(), palette.success, None)
        }
        crate::github::CheckState::Failure { passed, total } => (
            check_icons.failure.to_string(),
            palette.danger,
            Some((*passed, *total)),
        ),
        crate::github::CheckState::Pending { passed, total } => {
            let frame = crate::ui::pr_status::SPINNER_FRAMES
                [spinner_frame as usize % crate::ui::pr_status::SPINNER_FRAMES.len()];
            (frame.to_string(), palette.accent, Some((*passed, *total)))
        }
    };
    let style = if is_stale {
        Style::default()
            .fg(stale_color(palette, is_stale, color))
            .add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(stale_color(palette, is_stale, color))
    };
    let full = counts
        .map(|(passed, total)| {
            vec![
                (icon.clone(), style),
                (format!(" {}/{}", passed, total), style),
            ]
        })
        .unwrap_or_else(|| vec![(icon.clone(), style)]);
    let icon_only = vec![(icon, style)];

    for spans in [full, icon_only] {
        let width: usize = spans.iter().map(|(s, _)| display_width(s)).sum();
        if width > 0 && width <= available_width {
            return (spans, width);
        }
    }
    (Vec::new(), 0)
}

/// Format git diff stats for sidebar display, fitting within `available_width`.
/// Uses same colors as dashboard: DIM committed stats, bright uncommitted stats.
/// When `is_stale` is true, all colors are forced to dimmed.
///
/// Priority when space is limited:
/// 1. Uncommitted diff stats (bright +N -M with diff icon)
/// 2. Committed/branch diff stats (dimmed +N -M)
///
/// Returns pre-built spans (without background) and total display width.
pub(crate) fn format_sidebar_git_stats(
    status: Option<&GitStatus>,
    palette: &ThemePalette,
    is_stale: bool,
    available_width: usize,
) -> (Vec<(String, Style)>, usize) {
    let Some(status) = status else {
        return (vec![], 0);
    };

    let icons = crate::nerdfont::git_icons();

    let colors = git_diff_colors(palette, is_stale);

    let has_committed = status.lines_added > 0 || status.lines_removed > 0;
    let has_uncommitted =
        status.uncommitted_added > 0 || status.uncommitted_removed > 0 || status.is_dirty;

    // Same logic as dashboard: if all changes are uncommitted, skip the dimmed committed section
    let all_uncommitted = has_uncommitted
        && status.uncommitted_added == status.lines_added
        && status.uncommitted_removed == status.lines_removed;

    if !has_committed && !has_uncommitted && !status.is_rebasing {
        return (vec![], 0);
    }

    // Build rebase indicator (shown first, highest priority)
    let mut rebase_spans: Vec<(String, Style)> = Vec::new();
    if status.is_rebasing {
        let rebase_color = stale_color(palette, is_stale, palette.warning);
        rebase_spans.push((icons.rebase.to_string(), Style::default().fg(rebase_color)));
    }

    // Build uncommitted spans (bright, with diff icon)
    let mut uncommitted_spans: Vec<(String, Style)> = Vec::new();
    if has_uncommitted {
        uncommitted_spans.push((icons.diff.to_string(), Style::default().fg(colors.accent)));
        if status.uncommitted_added > 0 {
            uncommitted_spans.push((
                format!("+{}", status.uncommitted_added),
                Style::default().fg(colors.success),
            ));
        }
        if status.uncommitted_removed > 0 {
            uncommitted_spans.push((
                format!("-{}", status.uncommitted_removed),
                Style::default().fg(colors.danger),
            ));
        }
    }

    // Build committed spans (dimmed) - skip if all changes are uncommitted
    let mut committed_spans: Vec<(String, Style)> = Vec::new();
    if has_committed && !all_uncommitted {
        if status.lines_added > 0 {
            committed_spans.push((
                format!("+{}", status.lines_added),
                Style::default()
                    .fg(colors.success)
                    .add_modifier(Modifier::DIM),
            ));
        }
        if status.lines_removed > 0 {
            committed_spans.push((
                format!("-{}", status.lines_removed),
                Style::default()
                    .fg(colors.danger)
                    .add_modifier(Modifier::DIM),
            ));
        }
    }

    // Try variants in priority order: full > drop committed > drop uncommitted > rebase only.
    let candidates: Vec<Vec<(String, Style)>> = vec![
        {
            let mut s = rebase_spans.clone();
            s.extend(committed_spans.clone());
            s.extend(uncommitted_spans.clone());
            s
        },
        {
            let mut s = rebase_spans.clone();
            s.extend(uncommitted_spans);
            s
        },
        rebase_spans,
    ];

    for spans in candidates {
        let width = interleaved_width(&spans);
        if width > 0 && width <= available_width {
            return (interleave_spans(spans), width);
        }
    }
    (vec![], 0)
}

/// Width of an interleaved span list (text widths + 1 col per joiner space).
fn interleaved_width(spans: &[(String, Style)]) -> usize {
    if spans.is_empty() {
        return 0;
    }
    spans.iter().map(|(s, _)| display_width(s)).sum::<usize>() + spans.len() - 1
}

/// Insert a single space between adjacent spans (no trailing space).
fn interleave_spans(spans: Vec<(String, Style)>) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::with_capacity(spans.len() * 2);
    let mut first = true;
    for span in spans {
        if !first {
            out.push((" ".to_string(), Style::default()));
        }
        first = false;
        out.push(span);
    }
    out
}

/// Pick the widest variant (in priority order) that fits `max_width`.
/// Variants are pre-interleave: each entry is a list of styled text fragments
/// that will be joined by a single space.
fn pick_fitting_variant(
    variants: Vec<Vec<(String, Style)>>,
    max_width: usize,
) -> (Vec<(String, Style)>, usize) {
    for raw in variants {
        let width = interleaved_width(&raw);
        if width > 0 && width <= max_width {
            return (interleave_spans(raw), width);
        }
    }
    (Vec::new(), 0)
}

/// Format the committed/branch-diff segment of git stats with self-fitting.
///
/// Variant ladder (widest first): `+N -M` → `+N` → `-M` → empty.
/// Returns empty when there are no committed changes or when all changes
/// are uncommitted (the composite hides committed in that case to avoid
/// duplicating the uncommitted numbers).
pub(crate) fn format_committed_spans(
    status: Option<&GitStatus>,
    palette: &ThemePalette,
    is_stale: bool,
    max_width: usize,
) -> (Vec<(String, Style)>, usize) {
    let Some(status) = status else {
        return (Vec::new(), 0);
    };

    let has_committed = status.lines_added > 0 || status.lines_removed > 0;
    let has_uncommitted =
        status.uncommitted_added > 0 || status.uncommitted_removed > 0 || status.is_dirty;
    let all_uncommitted = has_uncommitted
        && status.uncommitted_added == status.lines_added
        && status.uncommitted_removed == status.lines_removed;

    if !has_committed || all_uncommitted {
        return (Vec::new(), 0);
    }

    let colors = git_diff_colors(palette, is_stale);
    let style_a = Style::default()
        .fg(colors.success)
        .add_modifier(Modifier::DIM);
    let style_r = Style::default()
        .fg(colors.danger)
        .add_modifier(Modifier::DIM);
    let (added, removed) =
        added_removed_fragments(status.lines_added, status.lines_removed, style_a, style_r);

    pick_fitting_variant(diff_variant_ladder(None, added, removed, false), max_width)
}

/// Format the uncommitted/diff segment with self-fitting.
///
/// Variant ladder: `icon +N -M` → `icon +N` → `icon -M` → `icon` → empty.
pub(crate) fn format_uncommitted_spans(
    status: Option<&GitStatus>,
    palette: &ThemePalette,
    is_stale: bool,
    max_width: usize,
) -> (Vec<(String, Style)>, usize) {
    let Some(status) = status else {
        return (Vec::new(), 0);
    };

    let has_uncommitted =
        status.uncommitted_added > 0 || status.uncommitted_removed > 0 || status.is_dirty;
    if !has_uncommitted {
        return (Vec::new(), 0);
    }

    let icons = crate::nerdfont::git_icons();
    let colors = git_diff_colors(palette, is_stale);
    let icon = (icons.diff.to_string(), Style::default().fg(colors.accent));
    let (added, removed) = added_removed_fragments(
        status.uncommitted_added,
        status.uncommitted_removed,
        Style::default().fg(colors.success),
        Style::default().fg(colors.danger),
    );

    pick_fitting_variant(
        diff_variant_ladder(Some(&icon), added, removed, true),
        max_width,
    )
}

/// Format the rebase indicator with self-fitting.
pub(crate) fn format_rebase_spans(
    status: Option<&GitStatus>,
    palette: &ThemePalette,
    is_stale: bool,
    max_width: usize,
) -> (Vec<(String, Style)>, usize) {
    let Some(status) = status else {
        return (Vec::new(), 0);
    };
    if !status.is_rebasing {
        return (Vec::new(), 0);
    }
    let icons = crate::nerdfont::git_icons();
    let color = stale_color(palette, is_stale, palette.warning);
    let icon = (icons.rebase.to_string(), Style::default().fg(color));
    pick_fitting_variant(vec![vec![icon]], max_width)
}

/// Render the sidebar UI.
pub fn render_sidebar(f: &mut Frame, app: &mut SidebarApp) {
    let area = f.area();

    if app.position == crate::config::SidebarPosition::Top {
        render_horizontal_bar(f, app, area);
        render_exit_confirmation(f, app);
        return;
    }

    let padding = match app.layout_mode {
        // Compact mode: pad both sides for breathing room
        SidebarLayoutMode::Compact => Padding::new(1, 1, 0, 0),
        // Tile mode: stripe provides left edge, border is already excluded from inner area
        SidebarLayoutMode::Tiles => Padding::ZERO,
    };

    let block = Block::default().padding(padding);

    let inner = block.inner(area);
    f.render_widget(block, area);
    let inner = render_template_error(f, app, inner);

    // Carve footer rows from the bottom of the inner area. The session RAM
    // total (when the memory readout has data) is the bottom-most row; the
    // transient session-filter label sits just above it. The list gets what
    // remains, so it never collides with tile mode's own bottom "↓ N more" row.
    let mut body = inner;
    let mut memory_area = None;
    if !app.memory.is_empty() && body.height > 1 {
        let row = Rect::new(body.x, body.y + body.height - 1, body.width, 1);
        body.height -= 1;
        memory_area = Some(row);
    }
    let mut filter_area = None;
    if app.filter_mode == SidebarFilterMode::Session && body.height > 1 {
        let row = Rect::new(body.x, body.y + body.height - 1, body.width, 1);
        body.height -= 1;
        filter_area = Some(row);
    }
    let list_area = body;
    app.list_area = list_area;

    match app.layout_mode {
        SidebarLayoutMode::Compact => render_compact_list(f, app, list_area),
        SidebarLayoutMode::Tiles => render_tile_list(f, app, list_area),
    }

    if let Some(filter_rect) = filter_area {
        let label = app
            .host_session()
            .map(|s| format!("[session: {}]", s))
            .unwrap_or_else(|| "[session]".to_string());
        let label = truncate_to_width(&label, filter_rect.width as usize);
        let line = Line::from(Span::styled(
            label,
            Style::default()
                .fg(app.palette.dimmed)
                .add_modifier(Modifier::DIM),
        ))
        .alignment(Alignment::Center);
        f.render_widget(line, filter_rect);
    }

    if let Some(memory_rect) = memory_area {
        let total: u64 = app.memory.values().sum();
        let label = format!("Σ {}", crate::mem::format_kb(total));
        let label = truncate_to_width(&label, memory_rect.width as usize);
        let line = Line::from(Span::styled(
            label,
            Style::default()
                .fg(app.palette.dimmed)
                .add_modifier(Modifier::DIM),
        ))
        .alignment(Alignment::Center);
        f.render_widget(line, memory_rect);
    }

    render_exit_confirmation(f, app);
}

fn render_exit_confirmation(f: &mut Frame, app: &SidebarApp) {
    if !app.pending_exit {
        return;
    }

    let terminal = f.area();
    let width = 30.min(terminal.width);
    let height = 3.min(terminal.height);
    let area = Rect::new(
        terminal.x + terminal.width.saturating_sub(width) / 2,
        terminal.y + terminal.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(app.palette.help_border));
    let text = Line::from(vec![
        Span::styled(" Quit sidebar? ", Style::default().fg(app.palette.text)),
        Span::styled(
            "y",
            Style::default()
                .fg(app.palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("es / ", Style::default().fg(app.palette.dimmed)),
        Span::styled(
            "n",
            Style::default()
                .fg(app.palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("o", Style::default().fg(app.palette.dimmed)),
    ]);
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(text).block(block), area);
}

fn render_template_error(f: &mut Frame, app: &SidebarApp, area: Rect) -> Rect {
    let Some(error) = &app.template_error else {
        return area;
    };
    if area.height == 0 {
        return area;
    }

    let message = error.display_message();
    let warning_height =
        wrapped_line_count(&message, area.width as usize).min(area.height as usize) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(warning_height), Constraint::Min(0)])
        .split(area);
    let warning = Paragraph::new(message)
        .style(Style::default().fg(app.palette.warning))
        .wrap(Wrap { trim: false });
    f.render_widget(warning, chunks[0]);
    chunks[1]
}

fn wrapped_line_count(s: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut lines = 1;
    let mut current = 0;
    for word in s.split_inclusive(' ') {
        let word_width = display_width(word);
        if current > 0 && current + word_width > width {
            lines += 1;
            current = 0;
        }
        current += word_width;
        while current > width {
            lines += 1;
            current -= width;
        }
    }
    lines
}

fn render_horizontal_bar(f: &mut Frame, app: &mut SidebarApp, area: Rect) {
    let block = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let inner = render_template_error(f, app, inner);
    app.list_area = inner;
    app.horizontal_hitboxes.clear();

    if app.agents.is_empty() {
        render_horizontal_no_agents(f, app, inner);
        return;
    }

    let setup = sidebar_list_setup(app);
    let top_templates: Vec<_> = app
        .templates
        .horizontal
        .iter()
        .filter(|template| !is_blank_template_line(template))
        .take(inner.height as usize)
        .cloned()
        .collect();
    let top_templates = if top_templates.is_empty() {
        vec![app.templates.compact.clone()]
    } else {
        top_templates
    };
    let row_count = top_templates.len().min(inner.height as usize);
    let mut visible_count = 0;
    let mut rows = vec![Vec::new(); row_count];
    let mut x = inner.x;
    let max_x = inner.x.saturating_add(inner.width);

    let start = app
        .first_visible_agent_idx
        .min(app.agents.len().saturating_sub(1));
    app.first_visible_agent_idx = start;

    for (idx, agent) in app.agents.iter().enumerate().skip(start) {
        let ctx = RowContext::build(
            app,
            agent,
            idx,
            &setup.pane_suffixes,
            setup.now_secs,
            setup.selected_idx,
        );
        let available = max_x.saturating_sub(x) as usize;
        if available == 0 {
            break;
        }
        let chip_width = available.min(app.horizontal_item_width);
        let has_status_icon = ctx
            .status_icon_spans
            .iter()
            .any(|(text, _)| !text.trim().is_empty());
        let render_options = RenderOptions::default().with_field_min_width(
            TokenId::StatusIcon,
            ctx.natural_width(TokenId::StatusIcon) + status_icon_extra_width(&ctx),
        );
        let mut chip_lines: Vec<Vec<Span<'static>>> = top_templates
            .iter()
            .map(|template| {
                let template = if has_status_icon {
                    template.clone()
                } else {
                    remove_blank_status_prefix(template)
                };
                let mut line =
                    render_line_with_options(&ctx, &template, chip_width, &render_options);
                if ctx.is_selected {
                    for span in &mut line {
                        if span.style.bg.is_none() {
                            span.style = span.style.bg(app.palette.highlight_row_bg);
                        }
                    }
                }
                line
            })
            .collect();
        let has_content = chip_lines.iter().any(|line| {
            line.iter()
                .any(|span| !span.content.as_ref().trim().is_empty())
        });
        let width = chip_width as u16;
        if !has_content || x.saturating_add(width) > max_x {
            break;
        }
        for line in &mut chip_lines {
            pad_spans_to_width(
                line,
                chip_width,
                ctx.is_selected.then_some(app.palette.highlight_row_bg),
            );
        }
        app.horizontal_hitboxes.push(super::app::HitBox {
            idx,
            x_start: x,
            x_end: x.saturating_add(width),
        });
        for (row, chip_line) in rows.iter_mut().zip(chip_lines.iter_mut()) {
            row.extend(std::mem::take(chip_line));
        }
        x = x.saturating_add(width);
        visible_count += 1;

        if x.saturating_add(2) < max_x && idx + 1 < app.agents.len() {
            for row in &mut rows {
                row.push(Span::raw(" "));
                row.push(Span::styled("│", Style::default().fg(app.palette.border)));
                row.push(Span::raw(" "));
            }
            x = x.saturating_add(3);
        }
    }

    app.ensure_selected_visible(visible_count);
    for (row_idx, spans) in rows.into_iter().enumerate() {
        let area = Rect::new(inner.x, inner.y + row_idx as u16, inner.width, 1);
        f.render_widget(Line::from(spans), area);
    }
}

fn remove_blank_status_prefix(template: &[Token]) -> Vec<Token> {
    let mut output = Vec::with_capacity(template.len());
    let mut iter = template.iter().peekable();
    while let Some(token) = iter.next() {
        if matches!(token, Token::Field(TokenId::StatusIcon)) {
            if matches!(iter.peek(), Some(Token::Literal(s)) if !s.is_empty() && s.chars().all(char::is_whitespace))
            {
                iter.next();
            }
            continue;
        }
        output.push(token.clone());
    }
    output
}

fn pad_spans_to_width(spans: &mut Vec<Span<'static>>, width: usize, bg: Option<Color>) {
    let current = spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum::<usize>();
    if current < width {
        let mut style = Style::default();
        if let Some(bg) = bg {
            style = style.bg(bg);
        }
        spans.push(Span::styled(" ".repeat(width - current), style));
    }
}

fn status_icon_extra_width(ctx: &RowContext<'_>) -> usize {
    if ctx.is_stale
        || matches!(
            ctx.agent.status,
            Some(AgentStatus::Waiting | AgentStatus::Done)
        )
    {
        1
    } else {
        0
    }
}

fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(1);
        if width + ch_width > max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out
}

/// Compact single-line-per-agent list (original layout).
fn render_compact_list(f: &mut Frame, app: &mut SidebarApp, area: Rect) {
    if app.agents.is_empty() {
        render_no_agents(f, app, area);
        return;
    }

    let setup = sidebar_list_setup(app);
    let template = app.templates.compact.clone();
    let width = area.width as usize;
    let contexts = build_row_contexts(app, &setup);
    let status_icon_width = contexts
        .iter()
        .map(|ctx| ctx.natural_width(TokenId::StatusIcon))
        .max()
        .unwrap_or(0);
    let render_options =
        RenderOptions::default().with_field_min_width(TokenId::StatusIcon, status_icon_width);

    let items: Vec<ListItem> = contexts
        .iter()
        .map(|ctx| {
            let mut spans = render_line_with_options(ctx, &template, width, &render_options);

            // Post-pass: apply selection background where the template has
            // not already supplied an explicit user `bg=`.
            if ctx.is_selected {
                apply_selection_bg(&mut spans, app.palette.highlight_row_bg);
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items).highlight_style(Style::default().bg(app.palette.highlight_row_bg));

    f.render_stateful_widget(list, area, &mut app.list_state);
}

/// Fully visible tiles and the space reserved for their overflow indicator.
#[derive(Debug, PartialEq, Eq)]
struct TileViewport {
    start: usize,
    end: usize,
    more: bool,
}

fn tile_viewport(
    heights: &[usize],
    offset: usize,
    selected: usize,
    height: usize,
) -> Option<TileViewport> {
    let selected = selected.min(heights.len().checked_sub(1)?);
    if heights[selected] > height {
        return None;
    }
    let fit = |budget: usize, offset: usize| {
        let mut start = offset.min(selected);
        let mut rows: usize = heights[start..=selected].iter().sum();
        while rows > budget && start < selected {
            rows -= heights[start];
            start += 1;
        }
        let mut end = selected + 1;
        while end < heights.len() && rows + heights[end] <= budget {
            rows += heights[end];
            end += 1;
        }
        (start, end)
    };
    let (mut start, mut end) = fit(height, offset);
    let mut more = end < heights.len() && heights[selected] < height;
    if more {
        (start, end) = fit(height - 1, offset);
        // A shifted viewport can reach the end without needing an indicator.
        let (_, full_end) = fit(height, start);
        if full_end == heights.len() {
            end = full_end;
            more = false;
        }
    }
    Some(TileViewport { start, end, more })
}

/// Tile layout: variable-height cards per agent with status stripe.
fn render_tile_list(f: &mut Frame, app: &mut SidebarApp, area: Rect) {
    if app.agents.is_empty() {
        render_no_agents(f, app, area);
        return;
    }

    let setup = sidebar_list_setup(app);

    let sep_width = area.width as usize;
    let agent_count = app.agents.len();
    let tile_templates: Vec<_> = app.templates.tiles.clone();
    let body_width = (area.width as usize).saturating_sub(6); // stripe(2) + icon(2) + gap(1) + right margin(1)

    let mut tile_heights = Vec::new();

    let items: Vec<ListItem> = app
        .agents
        .iter()
        .enumerate()
        .map(|(idx, agent)| {
            let ctx = RowContext::build(
                app,
                agent,
                idx,
                &setup.pane_suffixes,
                setup.now_secs,
                setup.selected_idx,
            );

            // Stripe color on all lines; stale forces dimmed
            let stripe_color = if ctx.is_stale {
                app.palette.dimmed
            } else {
                ctx.status_color
            };
            let stripe_style = Style::default().fg(stripe_color);

            let bg = if ctx.is_selected {
                Some(app.palette.highlight_row_bg)
            } else {
                None
            };

            let mut stripe_bg_style = stripe_style;
            if let Some(bg_color) = bg {
                stripe_bg_style = stripe_bg_style.bg(bg_color);
            }

            // Pad icon to fixed 2-column width
            let icon_cols: usize = ctx
                .status_icon_spans
                .iter()
                .map(|(t, _)| display_width(t))
                .sum();
            let icon_pad = if icon_cols < 2 {
                " ".repeat(2 - icon_cols)
            } else {
                String::new()
            };

            // Separator at the top (between tiles, not on first item)
            let mut lines = Vec::new();
            if idx > 0 {
                lines.push(Line::from(Span::styled(
                    "─".repeat(sep_width),
                    Style::default().fg(app.palette.border),
                )));
            }

            let mut visible_lines = 0;

            for (line_idx, template) in tile_templates.iter().enumerate() {
                if is_blank_template_line(template) {
                    continue;
                }
                visible_lines += 1;

                let mut line_spans: Vec<Span> = vec![Span::styled("▌ ", stripe_bg_style)];

                // Chrome: icon column (status icon on line 1, blank on lines 2+)
                if line_idx == 0 {
                    for (text, style) in &ctx.status_icon_spans {
                        line_spans.push(Span::styled(text.clone(), *style));
                    }
                    line_spans.push(Span::raw(icon_pad.clone()));
                } else {
                    line_spans.push(Span::raw("  "));
                }

                // Chrome: gap
                line_spans.push(Span::raw(" "));

                // Body: template rendering
                let body_spans = render_line(&ctx, template, body_width);
                line_spans.extend(body_spans);

                // Right margin: 1 blank column so content doesn't touch the edge.
                line_spans.push(Span::raw(" "));

                // Post-pass: apply selection background where the template
                // has not already supplied an explicit user `bg=`.
                if ctx.is_selected {
                    for span in &mut line_spans {
                        if span.style.bg.is_none() {
                            span.style = span.style.bg(app.palette.highlight_row_bg);
                        }
                    }
                }

                lines.push(Line::from(line_spans));
            }

            // If all lines were empty, render at least one blank line so the tile doesn't collapse
            if visible_lines == 0 {
                visible_lines = 1;
                lines.push(Line::from(vec![
                    Span::styled("▌ ", stripe_bg_style),
                    Span::raw("  "),
                    Span::raw(" "),
                    Span::raw(" ".repeat(body_width)),
                    Span::raw(" "),
                ]));
            }

            tile_heights.push(visible_lines);

            // Bottom separator after the last item
            if idx == agent_count - 1 {
                lines.push(Line::from(Span::styled(
                    "─".repeat(sep_width),
                    Style::default().fg(app.palette.border),
                )));
            }

            ListItem::new(lines)
        })
        .collect();

    app.tile_heights = tile_heights;
    let heights: Vec<_> = items.iter().map(ListItem::height).collect();
    let viewport = tile_viewport(
        &heights,
        app.list_state.offset(),
        app.list_state.selected().unwrap_or(app.list_state.offset()),
        area.height as usize,
    );
    let more = viewport.as_ref().is_some_and(|view| view.more);
    let list_area = Rect::new(area.x, area.y, area.width, area.height - u16::from(more));
    if let Some(view) = &viewport {
        *app.list_state.offset_mut() = view.start;
        let visible_height: usize = heights[view.start..view.end].iter().sum();
        app.list_area = Rect::new(area.x, area.y, area.width, visible_height as u16);
    } else {
        app.list_area = Rect::new(area.x, area.y, area.width, 0);
    }

    // Selection backgrounds belong to tile content, not separators or the footer.
    let list = List::new(items).scroll_padding(0);
    f.render_stateful_widget(list, list_area, &mut app.list_state);

    if let Some(view) = viewport.filter(|view| view.more) {
        let text = truncate_to_width(
            &format!("↓ {} more", agent_count - view.end),
            area.width as usize,
        );
        f.render_widget(
            Line::from(Span::styled(text, Style::default().fg(app.palette.dimmed))),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

/// Get the status icon as parsed styled spans and the base style for an agent.
///
/// Returns `(spans, base_style)` where `spans` contains tmux style codes parsed into
/// individual `(text, style)` pairs, and `base_style` is the fallback style (used for
/// stripe color, etc.).
pub(crate) fn status_icon_and_style(
    app: &SidebarApp,
    status: Option<AgentStatus>,
    is_stale: bool,
) -> (Vec<(String, Style)>, Style) {
    let use_nf = crate::nerdfont::is_enabled();

    if is_stale {
        let style = Style::default().fg(app.palette.dimmed);
        let icon = if use_nf {
            "\u{f04b2}" // 󰒲 nf-md-sleep
        } else {
            "💤"
        };
        return (vec![(icon.to_string(), style)], style);
    }
    match status {
        Some(AgentStatus::Working) => {
            let base_style = Style::default().fg(app.palette.info);
            let spans = match &app.status_icons.working {
                Some(custom) => tmux_style::parse_tmux_styles(custom, base_style),
                None => {
                    let frames: &[&str] =
                        &["⠋⠙", "⠙⠹", "⠹⠸", "⠸⠼", "⠼⠴", "⠴⠦", "⠦⠧", "⠧⠇", "⠇⠏", "⠏⠋"];
                    vec![(
                        frames[app.spinner_frame as usize % frames.len()].to_string(),
                        base_style,
                    )]
                }
            };
            (spans, base_style)
        }
        Some(AgentStatus::Waiting) => {
            let base_style = Style::default().fg(app.palette.accent);
            let spans = if use_nf && app.status_icons.waiting.is_none() {
                vec![("\u{f075}".to_string(), base_style)] //  nf-fa-comment
            } else {
                tmux_style::parse_tmux_styles(app.status_icons.waiting(), base_style)
            };
            (spans, base_style)
        }
        Some(AgentStatus::Done) => {
            let base_style = Style::default().fg(app.palette.success);
            let spans = if use_nf && app.status_icons.done.is_none() {
                vec![("\u{f0134}".to_string(), base_style)] // 󰄴 nf-md-check_circle
            } else {
                tmux_style::parse_tmux_styles(app.status_icons.done(), base_style)
            };
            (spans, base_style)
        }
        None => {
            let style = Style::default().fg(app.palette.dimmed);
            (vec![("  ".to_string(), style)], style)
        }
    }
}

fn render_horizontal_no_agents(f: &mut Frame, app: &SidebarApp, area: Rect) {
    let line = no_agents_line(app).alignment(Alignment::Center);
    let y = if area.height >= 3 {
        area.y + area.height / 2
    } else {
        area.y
    };
    let target = Rect::new(area.x, y, area.width, 1);
    f.render_widget(line, target);
}

fn render_no_agents(f: &mut Frame, app: &SidebarApp, area: Rect) {
    let text = no_agents_line(app).alignment(Alignment::Center);
    let y = area.y + area.height / 2;
    let centered = Rect::new(area.x, y, area.width, 1);
    f.render_widget(text, centered);
}

fn no_agents_line(app: &SidebarApp) -> Line<'static> {
    if app.has_loaded_snapshot {
        Line::from(Span::styled(
            "No agents running",
            Style::default().fg(app.palette.dimmed),
        ))
    } else {
        const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        Line::from(vec![
            Span::styled(
                FRAMES[app.spinner_frame as usize % FRAMES.len()],
                Style::default().fg(app.palette.dimmed),
            ),
            Span::styled(" Loading", Style::default().fg(app.palette.dimmed)),
        ])
    }
}

/// Get the display width of a string, counting wide chars as 2.
pub(crate) fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(1))
        .sum()
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::agent_display::{sanitize_pane_title, strip_oc_title_prefix};
    use crate::command::sidebar::app::TemplateError;

    fn tile_fixture() -> SidebarApp {
        use super::super::template::parser::parse_line;
        use std::path::PathBuf;

        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });
        app.template_error = None;
        app.dim_stale = false;
        app.templates.compact =
            parse_line("{status_icon} {primary} {pane_suffix} {fill} {elapsed}").unwrap();
        for (idx, (project, name, status)) in [
            ("api", "auth-refresh", AgentStatus::Working),
            ("api", "rate-limit", AgentStatus::Waiting),
            ("api", "rate-limit", AgentStatus::Done),
            ("mobile", "ios-refactor-tests", AgentStatus::Working),
            ("mobile", "ios-refactor-ui", AgentStatus::Waiting),
            ("workmux", "sidebar-groups", AgentStatus::Working),
        ]
        .into_iter()
        .enumerate()
        {
            let path = PathBuf::from(format!("/example/{project}/{name}"));
            app.agents.push(AgentPane {
                session: project.to_string(),
                window_name: format!("wm-{name}"),
                pane_id: format!("%{idx}"),
                window_id: format!("@{idx}"),
                window_index: Some(idx as u32),
                path,
                pane_title: None,
                status: Some(status),
                status_ts: None,
                activity_ts: None,
                updated_ts: None,
                window_cmd: None,
                agent_command: None,
                agent_kind: None,
                pane_pid: 0,
            });
        }
        app.list_state.select(Some(0));
        app.host_agent_idx = Some(5);
        app
    }

    fn buffer_row(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    fn tile_app() -> SidebarApp {
        let mut app = tile_fixture();
        app.layout_mode = SidebarLayoutMode::Tiles;
        app.templates.tiles = ["{primary}", "{secondary}", "{pane_title}"]
            .into_iter()
            .map(|line| super::super::template::parser::parse_line(line).unwrap())
            .collect();
        app
    }

    #[test]
    fn tile_footer_counts_hidden_agents_and_has_no_mouse_target() {
        let mut app = tile_app();
        let mut terminal = Terminal::new(TestBackend::new(36, 12)).unwrap();
        terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();
        assert_eq!(
            buffer_row(terminal.backend().buffer(), 11).trim(),
            "↓ 3 more"
        );
        assert_eq!(app.hit_test(1, 0), Some(0));
        assert_eq!(app.hit_test(1, 4), Some(1));
        assert_eq!(app.hit_test(1, 8), Some(2));
        assert_eq!(app.hit_test(1, 11), None);
        let offset = app.list_state.offset();
        terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();
        assert_eq!(app.list_state.offset(), offset);

        app.select_last();
        terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();
        let text = (0..12)
            .map(|y| buffer_row(terminal.backend().buffer(), y))
            .collect::<String>();
        assert!(!text.contains("more"));
        assert!(text.contains("sidebar-groups"));
        assert!((0..12).any(|y| app.hit_test(1, y) == Some(5)));
        assert_eq!(app.hit_test(1, 11), None);
    }

    #[test]
    fn tile_footer_uses_template_heights_and_respects_session_footer() {
        let mut app = tile_app();
        app.filter_mode = SidebarFilterMode::Session;
        // Blank template lines do not consume tile rows.
        app.templates.tiles[1].clear();
        app.templates.tiles[2].clear();
        app.agents.truncate(4);
        let mut terminal = Terminal::new(TestBackend::new(36, 6)).unwrap();
        terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();
        assert_eq!(
            buffer_row(terminal.backend().buffer(), 4).trim(),
            "↓ 2 more"
        );
        assert_eq!(
            buffer_row(terminal.backend().buffer(), 5).trim(),
            "[session]"
        );
        assert_eq!(app.hit_test(1, 3), None);
        assert_eq!(app.hit_test(1, 4), None);
        assert_eq!(app.hit_test(1, 5), None);
    }

    #[test]
    fn tile_viewport_preserves_selection_and_stays_stable() {
        for heights in [vec![3, 4, 4, 4, 5], vec![1, 2, 2, 3], vec![2, 6, 3, 1]] {
            for height in 0..24 {
                for selected in 0..heights.len() {
                    for offset in 0..heights.len() + 2 {
                        let view = tile_viewport(&heights, offset, selected, height);
                        if heights[selected] > height {
                            assert!(view.is_none());
                            continue;
                        }
                        let view = view.unwrap();
                        assert!(view.start <= selected && selected < view.end);
                        let rows: usize = heights[view.start..view.end].iter().sum();
                        assert!(rows + usize::from(view.more) <= height);
                        assert_eq!(
                            Some(&view),
                            tile_viewport(&heights, view.start, selected, height).as_ref()
                        );
                        assert!(!view.more || view.end < heights.len());
                    }
                }
            }
        }
        assert!(tile_viewport(&[], 0, 0, 10).is_none());
    }

    #[test]
    fn tile_footer_disappears_when_agents_fit_and_never_hides_selection() {
        let mut app = tile_app();
        for height in 0..30 {
            app.list_state.select(Some(0));
            *app.list_state.offset_mut() = 0;
            let mut terminal = Terminal::new(TestBackend::new(25, height)).unwrap();
            terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();
            if height >= 3 {
                assert!((0..height).any(|y| app.hit_test(1, y) == Some(0)));
            }
            if height >= 24 {
                let text = (0..height)
                    .map(|y| buffer_row(terminal.backend().buffer(), y))
                    .collect::<String>();
                assert!(!text.contains("more"));
            }
        }
    }

    #[test]
    #[ignore = "manual performance benchmark"]
    fn benchmark_sidebar_render_refresh_workload() {
        use std::hint::black_box;
        use std::time::Instant;

        use crate::command::sidebar::template::parser::parse_line;

        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });
        app.template_error = None;
        app.filter_mode = SidebarFilterMode::None;
        app.templates.compact = parse_line(
            "{status_icon} {primary}{pane_suffix} {fill} {git_stats} {pr_checks} {elapsed}",
        )
        .unwrap();
        let worktree_path = std::env::var_os("WMPERF_RENDER_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        app.agents = (0..64)
            .map(|idx| AgentPane {
                session: format!("session-{}", idx % 4),
                window_name: format!("wm-feature-{idx}"),
                pane_id: format!("%{idx}"),
                window_id: format!("@{}", idx / 2),
                window_index: Some(idx),
                path: worktree_path.clone(),
                pane_title: Some(format!("Implementing sidebar performance work {idx}")),
                status: Some(match idx % 3 {
                    0 => AgentStatus::Working,
                    1 => AgentStatus::Waiting,
                    _ => AgentStatus::Done,
                }),
                status_ts: Some(1_700_000_000),
                activity_ts: Some(1_700_000_000),
                updated_ts: Some(1_700_000_000),
                window_cmd: None,
                agent_command: Some("claude".to_string()),
                agent_kind: Some("claude".to_string()),
                pane_pid: 0,
            })
            .collect();
        for agent in &app.agents {
            app.git_statuses.insert(
                agent.path.clone(),
                GitStatus {
                    branch: Some("perf-sidebar-render".to_string()),
                    lines_added: 123,
                    lines_removed: 45,
                    uncommitted_added: 12,
                    uncommitted_removed: 3,
                    is_dirty: true,
                    ..Default::default()
                },
            );
        }

        let backend = TestBackend::new(48, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        const WARMUP: usize = 200;
        const SAMPLES: usize = 2_000;
        for _ in 0..WARMUP {
            terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();
            app.spinner_frame = app.spinner_frame.wrapping_add(1);
        }

        let started = Instant::now();
        for _ in 0..SAMPLES {
            terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();
            app.spinner_frame = app.spinner_frame.wrapping_add(1);
        }
        let elapsed = started.elapsed();
        black_box(terminal.backend().buffer());
        println!(
            "sidebar_render_refresh: samples={SAMPLES} elapsed_ns={} ns_per_frame={}",
            elapsed.as_nanos(),
            elapsed.as_nanos() / SAMPLES as u128
        );
    }

    #[test]
    fn render_sidebar_shows_exit_confirmation() {
        let backend = TestBackend::new(34, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });
        app.template_error = None;
        app.pending_exit = true;

        terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let text = (0..5)
            .flat_map(|y| (0..34).map(move |x| buffer[(x, y)].symbol()))
            .collect::<String>();
        assert!(text.contains("Quit sidebar?"));
        assert!(text.contains("yes / no"));
    }

    #[test]
    fn render_sidebar_shows_template_error_warning() {
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: "tiles[0]".to_string(),
            message: "unknown token 'pr_status' at column 1".to_string(),
        });
        app.filter_mode = super::SidebarFilterMode::None;

        terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let text = (0..6)
            .flat_map(|y| (0..30).map(move |x| buffer[(x, y)].symbol()))
            .collect::<String>();
        assert!(text.contains("template error:"));
        assert!(text.contains("unknown"));
        assert!(text.contains("token"));
        assert!(text.contains("pr_status"));
        assert!(text.contains("tiles[0]"));
        assert!(app.list_area.y > 1);
        assert_eq!(app.list_area.y + app.list_area.height, 6);
        assert_eq!(app.hit_test(0, 0), None);
        assert_eq!(app.hit_test(0, app.list_area.y - 1), None);
    }

    #[test]
    fn strips_oc_prefixes() {
        assert_eq!(
            strip_oc_title_prefix("OC | Investigating..."),
            "Investigating..."
        );
        assert_eq!(
            strip_oc_title_prefix("OC | OC | Investigating..."),
            "Investigating..."
        );
    }

    #[test]
    fn keeps_non_agent_pipe_titles() {
        assert_eq!(
            strip_oc_title_prefix("Build | Investigating..."),
            "Build | Investigating..."
        );
        assert_eq!(
            strip_oc_title_prefix("Claude Code | Investigating..."),
            "Claude Code | Investigating..."
        );
    }

    #[test]
    fn sanitize_pane_title_drops_empty_after_prefix_strip() {
        assert_eq!(
            sanitize_pane_title(Some("OC |"), "worktree", "project"),
            None
        );
    }

    #[test]
    fn sanitize_pane_title_strips_icons_and_agent_prefixes() {
        assert_eq!(
            sanitize_pane_title(Some("⠋⠙ OC | Investigating..."), "worktree", "project"),
            Some("Investigating...")
        );
    }
}
