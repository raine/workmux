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

use super::app::{SidebarApp, SidebarFilterMode, SidebarLayoutMode, SidebarRow};
use super::snapshot::group_label;
use super::template::TokenId;
use super::template::context::RowContext;
use super::template::layout::{
    RenderOptions, is_blank_template_line, render_line, render_line_with_options,
};
use super::template::parser::Token;
use super::template::row::{GroupStatusCount, HeaderContext};

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

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn sidebar_list_setup(app: &SidebarApp) -> SidebarListSetup {
    SidebarListSetup {
        now_secs: now_secs(),
        pane_suffixes: compute_pane_suffixes(&app.agents),
        selected_idx: app.selected_agent_idx(),
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
        render_help(f, app);
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

    // The footer carries one line: the session filter when it is on, otherwise
    // the grouping hint while it is still on offer.
    let footer = if app.filter_mode == SidebarFilterMode::Session {
        Some(filter_footer_line(app))
    } else if app.show_hint() {
        Some(hint_line(app, inner.width as usize))
    } else {
        None
    };
    let (list_area, filter_area) = match &footer {
        Some(_) if inner.height > 1 => {
            let list = Rect::new(inner.x, inner.y, inner.width, inner.height - 1);
            let footer = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
            (list, Some(footer))
        }
        _ => (inner, None),
    };
    app.list_area = list_area;

    match app.layout_mode {
        SidebarLayoutMode::Compact => render_compact_list(f, app, list_area),
        SidebarLayoutMode::Tiles => render_tile_list(f, app, list_area),
    }

    if let (Some(rect), Some(line)) = (filter_area, footer) {
        f.render_widget(line, rect);
    }

    render_help(f, app);
    render_exit_confirmation(f, app);
}

/// Keys the sidebar answers to, paired with what they do. Keys that only act
/// on groups are listed while grouping is on, since that is the only time they
/// have anything to act on.
fn help_entries(app: &SidebarApp) -> Vec<(&'static str, &'static str)> {
    let mut entries = vec![("j k", "move"), ("g G", "first last"), ("enter", "jump")];
    if app.group_by.is_some() {
        entries.extend([
            ("h l", "fold unfold"),
            ("s", "fold group"),
            ("S", "fold all"),
        ]);
    }
    entries.extend([
        ("t", "grouping"),
        ("v", "layout"),
        ("f", "session filter"),
        ("z", "sleep"),
        ("q", "quit"),
    ]);
    entries
}

/// Draw the key overlay over the list. A sidebar is narrow and has no room for
/// a permanent legend, so the keys it added stay discoverable behind `?`.
/// Style shared by the footer's two occupants. The band background separates
/// the line from the list above it, which otherwise runs straight into it:
/// both are dim text on the terminal background, and the footer is chrome
/// rather than another row.
fn footer_line(app: &SidebarApp, text: String) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(app.palette.dimmed)
            .add_modifier(Modifier::DIM),
    ))
    .alignment(Alignment::Center)
    .style(Style::default().bg(group_band_bg(&app.palette)))
}

fn filter_footer_line(app: &SidebarApp) -> Line<'static> {
    let label = app
        .host_session()
        .map(|s| format!("[session: {}]", s))
        .unwrap_or_else(|| "[session]".to_string());
    footer_line(app, label)
}

/// One dim line offering the two keys worth knowing about: the one that groups
/// the list, and the one that lists every other key. Named so the reader sees
/// what the key would do to this sidebar, not what the feature is called.
///
/// A `new` badge marks the invitation while the sidebar is still the one the
/// reader knows. Once they have taken it the line drops the badge and offers
/// the way back instead, since by then the mode is not news.
fn hint_line(app: &SidebarApp, width: usize) -> Line<'static> {
    let (badge, options): (&str, Vec<String>) = match app.group_by {
        Some(_) => (
            "",
            vec!["t  flat list  ·  ?  keys".into(), "t  flat  ·  ?".into()],
        ),
        None => {
            let by = match app.configured_group_by {
                Some(crate::config::SidebarGroupBy::Session) => "session",
                _ => "project",
            };
            (
                "new",
                vec![
                    format!("t  group by {by}  ·  ?  keys"),
                    "t  group  ·  ?  keys".into(),
                    "t  group  ·  ?".into(),
                ],
            )
        }
    };

    let badge_width = if badge.is_empty() {
        0
    } else {
        display_width(badge) + 2
    };
    let text = options
        .iter()
        .find(|option| badge_width + display_width(option) <= width)
        .cloned()
        .unwrap_or_else(|| options.last().cloned().unwrap_or_default());
    // The badge is the first thing to go: the keys are the point of the line.
    let badge = if badge_width + display_width(&text) <= width {
        badge
    } else {
        ""
    };

    let dim = Style::default()
        .fg(app.palette.dimmed)
        .add_modifier(Modifier::DIM);
    let mut spans = Vec::new();
    if !badge.is_empty() {
        spans.push(Span::styled(
            badge.to_string(),
            Style::default().fg(app.palette.accent),
        ));
        spans.push(Span::styled("  ".to_string(), dim));
    }
    spans.push(Span::styled(truncate_to_width(&text, width), dim));

    Line::from(spans)
        .alignment(Alignment::Center)
        .style(Style::default().bg(group_band_bg(&app.palette)))
}

fn render_help(f: &mut Frame, app: &SidebarApp) {
    if !app.show_help {
        return;
    }

    let entries = help_entries(app);
    let terminal = f.area();
    let key_width = entries.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
    let content_width = entries
        .iter()
        .map(|(_, action)| key_width + 1 + action.len())
        .max()
        .unwrap_or(0);
    let width = ((content_width + 4) as u16).min(terminal.width);
    let height = ((entries.len() + 2) as u16).min(terminal.height);
    let area = Rect::new(
        terminal.x + terminal.width.saturating_sub(width) / 2,
        terminal.y + terminal.height.saturating_sub(height) / 2,
        width,
        height,
    );

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(app.palette.help_border))
        .padding(Padding::horizontal(1));
    let inner_width = block.inner(area).width as usize;
    let lines: Vec<Line> = entries
        .iter()
        .map(|(key, action)| {
            let label = format!("{key:key_width$} ");
            Line::from(vec![
                Span::styled(
                    truncate_to_width(&label, inner_width),
                    Style::default()
                        .fg(app.palette.text)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    truncate_to_width(action, inner_width.saturating_sub(label.len())),
                    Style::default().fg(app.palette.help_muted),
                ),
            ])
        })
        .collect();

    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
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
    status_icon_overhang(ctx.agent.status, ctx.is_stale)
}

/// Columns a status icon draws past its measured width. The waiting, done and
/// sleeping glyphs are drawn double width while measuring one, so whatever sits
/// beside them needs the extra column reserved.
fn status_icon_overhang(status: Option<AgentStatus>, is_stale: bool) -> usize {
    if is_stale || matches!(status, Some(AgentStatus::Waiting | AgentStatus::Done)) {
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

/// Presentation row index of the group header owning `row`, if any sits above
/// the viewport start.
fn header_above(app: &SidebarApp, row: usize) -> Option<usize> {
    app.rows[..row]
        .iter()
        .rposition(|r| matches!(r, SidebarRow::Header { .. } | SidebarRow::StaleGroup { .. }))
}

/// Background of the group header band, stepped away from the selection.
///
/// Selection is the sidebar's only other background, so a band that merely
/// approaches it reads as a dimmer selected row rather than as a section break.
/// The step goes in whichever direction the theme has room for it: toward black
/// on a dark palette, toward white on a light one. Palettes whose colors are
/// not RGB cannot be shifted, so they fall back to the current-row tint.
fn group_band_bg(palette: &ThemePalette) -> Color {
    /// Fraction of the distance from the selection toward the theme's extreme.
    const STEP: f32 = 0.35;

    let Color::Rgb(r, g, b) = palette.highlight_row_bg else {
        return palette.current_row_bg;
    };
    let luminance = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0;
    let target = if luminance < 0.5 { 0.0 } else { 255.0 };
    let step = |channel: u8| {
        let channel = f32::from(channel);
        (channel + (target - channel) * STEP).round() as u8
    };
    Color::Rgb(step(r), step(g), step(b))
}

/// Header line for a group, rendered through the shared template solver onto a
/// full-width band. The band, not a divider, is what sets a header apart from
/// the agents under it, so a group costs one row.
fn header_line(app: &SidebarApp, label: &str, count: usize, width: usize) -> Line<'static> {
    let band = group_band_bg(&app.palette);
    // The indent is decoration. A sidebar too narrow to spare it keeps the
    // label and the count instead.
    let indent = display_width(GROUP_LABEL_INDENT);
    let indent = if width.saturating_sub(indent) < 4 {
        0
    } else {
        indent
    };
    let mut spans = vec![Span::raw(&GROUP_LABEL_INDENT[..indent])];
    spans.extend(header_spans(app, label, count, width - indent));
    // A template that sets its own `bg=` keeps it; everything else gets the band.
    apply_selection_bg(&mut spans, band);
    pad_spans_to_width(&mut spans, width, Some(band));
    Line::from(spans)
}

/// Indent shared by every group label, so a header lines up with the label of
/// a collapsed group, which gives its first two columns to the chevron.
const GROUP_LABEL_INDENT: &str = "  ";

/// The header template solved for one group, without the band or the indent.
fn header_spans(app: &SidebarApp, label: &str, count: usize, width: usize) -> Vec<Span<'static>> {
    let statuses = if app
        .templates
        .group_header
        .iter()
        .any(|token| matches!(token, Token::Field(TokenId::GroupStatus)))
    {
        group_status_counts(app, label)
    } else {
        Vec::new()
    };
    let ctx = HeaderContext {
        label: label.to_string(),
        count,
        statuses,
        palette: &app.palette,
    };
    render_line(&ctx, &app.templates.group_header, width)
}

/// Tally the statuses of a group's agents, most urgent first, so a header can
/// say what a section holds and not only how much. Stale agents are one bucket
/// however they got there, matching the fold that hides them.
fn group_status_counts(app: &SidebarApp, label: &str) -> Vec<GroupStatusCount> {
    let Some(group_by) = app.group_by else {
        return Vec::new();
    };
    // An agent with no status and no staleness has no icon of its own, so it
    // is left out rather than tallied behind a blank.
    let mut counts: Vec<usize> = vec![0; 4];
    for agent in &app.agents {
        if group_label(agent, group_by) != label {
            continue;
        }
        let bucket = if app.stale_pane_ids.contains(&agent.pane_id) {
            Some(3)
        } else {
            match agent.status {
                Some(AgentStatus::Waiting) => Some(0),
                Some(AgentStatus::Done) => Some(1),
                Some(AgentStatus::Working) => Some(2),
                None => None,
            }
        };
        if let Some(bucket) = bucket {
            counts[bucket] += 1;
        }
    }

    let buckets = [
        (Some(AgentStatus::Waiting), false),
        (Some(AgentStatus::Done), false),
        (Some(AgentStatus::Working), false),
        (None, true),
    ];
    counts
        .into_iter()
        .zip(buckets)
        .filter(|(count, _)| *count > 0)
        .map(|(count, (status, stale))| GroupStatusCount {
            icon: status_icon_and_style(app, status, stale).0,
            count,
            pad: status_icon_overhang(status, stale),
        })
        .collect()
}

/// Chevron showing whether a toggle's agents are visible.
fn chevron(expanded: bool) -> &'static str {
    if expanded { "\u{25be}" } else { "\u{25b8}" }
}

/// Row standing for the stale agents of a group that also holds live ones.
/// The count sits inline so it cannot be mistaken for a group header, which
/// carries its count on the right. Selection brightens the text as well as the
/// background: one short dim line leaves the background little to show through.
fn stale_tail_line(
    app: &SidebarApp,
    count: usize,
    expanded: bool,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let style = if selected {
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.highlight_row_bg)
    } else {
        Style::default().fg(app.palette.dimmed)
    };
    let text = format!("  {} {} stale", chevron(expanded), count);
    let mut spans = vec![Span::styled(truncate_to_width(&text, width), style)];
    if selected {
        pad_spans_to_width(&mut spans, width, Some(app.palette.highlight_row_bg));
    }
    Line::from(spans)
}

/// Header of a group with no live agents, which is also its toggle.
fn stale_group_line(
    app: &SidebarApp,
    label: &str,
    count: usize,
    expanded: bool,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let bg = if selected {
        app.palette.highlight_row_bg
    } else {
        group_band_bg(&app.palette)
    };
    let marker_fg = if selected {
        app.palette.text
    } else {
        app.palette.dimmed
    };
    let marker = format!("{} ", chevron(expanded));
    let marker_cols = display_width(&marker);
    let mut spans = vec![Span::styled(marker, Style::default().fg(marker_fg))];
    spans.extend(header_spans(
        app,
        label,
        count,
        width.saturating_sub(marker_cols),
    ));
    for span in &mut spans {
        span.style = span.style.bg(bg);
    }
    pad_spans_to_width(&mut spans, width, Some(bg));
    Line::from(spans)
}

/// Clamp a compact offset so the selected row stays visible, mirroring the/// Clamp a compact offset so the selected row stays visible, mirroring the
/// rule ratatui applies during render. Computing it here keeps the sticky
/// header in sync with the frame being drawn.
fn compact_offset(offset: usize, selected: usize, height: usize) -> usize {
    if height == 0 {
        return offset;
    }
    let mut start = offset.min(selected);
    if selected >= start + height {
        start = selected + 1 - height;
    }
    start
}

/// Compact single-line-per-row list (original layout, plus group headers).
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

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .enumerate()
        .map(|(row_idx, row)| match row {
            SidebarRow::Agent(idx) => {
                let ctx = &contexts[*idx];
                let mut spans = render_line_with_options(ctx, &template, width, &render_options);

                // Post-pass: apply selection background where the template has
                // not already supplied an explicit user `bg=`.
                if ctx.is_selected {
                    apply_selection_bg(&mut spans, app.palette.highlight_row_bg);
                }

                ListItem::new(Line::from(spans))
            }
            SidebarRow::Header { label, count } => {
                ListItem::new(header_line(app, label, *count, width))
            }
            SidebarRow::Rule { label } => ListItem::new(labeled_rule(app, label, width)),
            SidebarRow::StaleTail {
                count, expanded, ..
            } => ListItem::new(stale_tail_line(
                app,
                *count,
                *expanded,
                app.list_state.selected() == Some(row_idx),
                width,
            )),
            SidebarRow::StaleGroup {
                label,
                count,
                expanded,
            } => ListItem::new(stale_group_line(
                app,
                label,
                *count,
                *expanded,
                app.list_state.selected() == Some(row_idx),
                width,
            )),
        })
        .collect();

    // Own the offset so the sticky header describes this frame, not the last.
    let selected = app.list_state.selected().unwrap_or(0);
    let height = area.height as usize;
    let full_start = compact_offset(app.list_state.offset(), selected, height);

    let mut sticky: Option<usize> = None;
    let mut start = full_start;
    if height > 1
        && matches!(app.rows.get(full_start), Some(SidebarRow::Agent(_)))
        && header_above(app, full_start).is_some()
    {
        let shrunk = compact_offset(app.list_state.offset(), selected, height - 1);
        // Re-derive after shrinking: the smaller viewport can start on the
        // next group's real header, which then needs no pin.
        if !matches!(
            app.rows.get(shrunk),
            Some(SidebarRow::Header { .. } | SidebarRow::StaleGroup { .. })
        ) {
            sticky = header_above(app, shrunk);
            start = shrunk;
        }
    }

    *app.list_state.offset_mut() = start;

    let list_area = match sticky {
        Some(header_row) => {
            let pinned = match app.rows.get(header_row) {
                Some(SidebarRow::Header { label, count }) => {
                    Some(header_line(app, label, *count, width))
                }
                Some(SidebarRow::StaleGroup {
                    label,
                    count,
                    expanded,
                }) => Some(stale_group_line(
                    app, label, *count, *expanded, false, width,
                )),
                _ => None,
            };
            if let Some(line) = pinned {
                f.render_widget(line, Rect::new(area.x, area.y, area.width, 1));
            }
            Rect::new(area.x, area.y + 1, area.width, area.height - 1)
        }
        None => area,
    };
    // The pinned band is not a row, so it must not resolve to an agent.
    app.list_area = list_area;

    let list = List::new(items)
        .scroll_padding(0)
        .highlight_style(Style::default().bg(app.palette.highlight_row_bg));

    f.render_stateful_widget(list, list_area, &mut app.list_state);
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

/// Divider line closing a tile or a group header.
fn tile_divider(app: &SidebarApp, width: usize) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width),
        Style::default().fg(app.palette.border),
    ))
}

/// A divider carrying a centered label, marking a break in the list itself
/// rather than the start of one group.
fn labeled_rule(app: &SidebarApp, label: &str, width: usize) -> Line<'static> {
    let text = format!(" {label} ");
    let text_cols = display_width(&text).min(width);
    let left = (width - text_cols) / 2;
    let right = width - text_cols - left;
    Line::from(vec![
        Span::styled(
            "\u{2500}".repeat(left),
            Style::default().fg(app.palette.border),
        ),
        Span::styled(
            truncate_to_width(&text, text_cols),
            Style::default().fg(app.palette.dimmed),
        ),
        Span::styled(
            "\u{2500}".repeat(right),
            Style::default().fg(app.palette.border),
        ),
    ])
}

/// Lines of a tile-mode group header: the divider that closes the previous
/// group, unless the header starts the list, and the label line. The label sits
/// directly above its first agent, so a group costs one row more than its
/// tiles.
fn tile_header_lines(
    app: &SidebarApp,
    label: &str,
    count: usize,
    width: usize,
    is_first_row: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !is_first_row {
        lines.push(tile_divider(app, width));
    }
    // One column of right margin, so the count lines up with the elapsed
    // column of the tiles below it; the band still spans the full width.
    let mut spans = header_line(app, label, count, width.saturating_sub(1)).spans;
    pad_spans_to_width(&mut spans, width, Some(group_band_bg(&app.palette)));
    lines.push(Line::from(spans));
    lines
}

/// Tile layout: variable-height cards per agent with status stripe.
fn render_tile_list(f: &mut Frame, app: &mut SidebarApp, area: Rect) {
    if app.agents.is_empty() {
        render_no_agents(f, app, area);
        return;
    }

    let setup = sidebar_list_setup(app);

    let sep_width = area.width as usize;
    let tile_templates: Vec<_> = app.templates.tiles.clone();
    let body_width = (area.width as usize).saturating_sub(6); // stripe(2) + icon(2) + gap(1) + right margin(1)

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .enumerate()
        .map(|(row_idx, row)| {
            let idx = match row {
                SidebarRow::Agent(idx) => *idx,
                SidebarRow::Header { label, count } => {
                    // The labeled rule above a section already closes the
                    // group before it, so the header adds no divider of its own.
                    let opens_list =
                        row_idx == 0 || matches!(app.rows[row_idx - 1], SidebarRow::Rule { .. });
                    return ListItem::new(tile_header_lines(
                        app, label, *count, sep_width, opens_list,
                    ));
                }
                SidebarRow::Rule { label } => {
                    return ListItem::new(labeled_rule(app, label, sep_width));
                }
                SidebarRow::StaleTail {
                    count, expanded, ..
                } => {
                    let mut lines = Vec::new();
                    if row_idx > 0 && matches!(app.rows[row_idx - 1], SidebarRow::Agent(_)) {
                        lines.push(tile_divider(app, sep_width));
                    }
                    lines.push(stale_tail_line(
                        app,
                        *count,
                        *expanded,
                        app.list_state.selected() == Some(row_idx),
                        sep_width,
                    ));
                    if row_idx == app.rows.len() - 1 {
                        lines.push(tile_divider(app, sep_width));
                    }
                    return ListItem::new(lines);
                }
                SidebarRow::StaleGroup {
                    label,
                    count,
                    expanded,
                } => {
                    let mut lines = Vec::new();
                    if row_idx > 0 && !matches!(app.rows[row_idx - 1], SidebarRow::Rule { .. }) {
                        lines.push(tile_divider(app, sep_width));
                    }
                    let selected = app.list_state.selected() == Some(row_idx);
                    let mut spans = stale_group_line(
                        app,
                        label,
                        *count,
                        *expanded,
                        selected,
                        sep_width.saturating_sub(1),
                    )
                    .spans;
                    let bg = if selected {
                        app.palette.highlight_row_bg
                    } else {
                        group_band_bg(&app.palette)
                    };
                    pad_spans_to_width(&mut spans, sep_width, Some(bg));
                    lines.push(Line::from(spans));
                    return ListItem::new(lines);
                }
            };
            let agent = &app.agents[idx];
            let collapsed = app.renders_collapsed(agent);
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

            // Separator at the top, except after a header, which already
            // closes with one, and except on the very first row. A run of
            // collapsed rows reads as one block, so no divider splits it.
            let mut lines = Vec::new();
            let previous_collapsed = row_idx
                .checked_sub(1)
                .and_then(|prev| match app.rows[prev] {
                    SidebarRow::Agent(prev_idx) => {
                        Some(app.renders_collapsed(&app.agents[prev_idx]))
                    }
                    _ => None,
                });
            if let Some(previous_collapsed) = previous_collapsed
                && !(collapsed && previous_collapsed)
            {
                lines.push(tile_divider(app, sep_width));
            }

            let mut visible_lines = 0;

            for (line_idx, template) in tile_templates.iter().enumerate() {
                if is_blank_template_line(template) {
                    continue;
                }
                // A collapsed agent keeps its first line and drops the rest.
                if collapsed && visible_lines == 1 {
                    break;
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
                lines.push(Line::from(vec![
                    Span::styled("▌ ", stripe_bg_style),
                    Span::raw("  "),
                    Span::raw(" "),
                    Span::raw(" ".repeat(body_width)),
                    Span::raw(" "),
                ]));
            }

            // Bottom separator after the last row
            if row_idx == app.rows.len() - 1 {
                lines.push(tile_divider(app, sep_width));
            }

            ListItem::new(lines)
        })
        .collect();

    let heights: Vec<_> = items.iter().map(ListItem::height).collect();
    app.tile_heights.clone_from(&heights);
    let selected_row = app.list_state.selected().unwrap_or(app.list_state.offset());
    let full_height = area.height as usize;
    let full_view = tile_viewport(&heights, app.list_state.offset(), selected_row, full_height);

    // Pin the current group's header when the viewport opens mid-group. The
    // pin renders exactly like a header that opens the list: one label line.
    const PIN_ROWS: usize = 1;
    let mut sticky: Option<usize> = None;
    let mut viewport = full_view;
    if let Some(view) = &viewport
        && full_height > PIN_ROWS
        && matches!(app.rows.get(view.start), Some(SidebarRow::Agent(_)))
        && header_above(app, view.start).is_some()
        && let Some(shrunk) = tile_viewport(
            &heights,
            app.list_state.offset(),
            selected_row,
            full_height - PIN_ROWS,
        )
        // Re-derive after shrinking: the smaller viewport can start on the
        // next group's real header, and the selected tile must still fit.
        && !matches!(
            app.rows.get(shrunk.start),
            Some(SidebarRow::Header { .. } | SidebarRow::StaleGroup { .. })
        )
        && (shrunk.start..shrunk.end).contains(&selected_row)
    {
        sticky = header_above(app, shrunk.start);
        viewport = Some(shrunk);
    }

    let more = viewport.as_ref().is_some_and(|view| view.more);
    let pin_rows = u16::from(sticky.is_some()) * PIN_ROWS as u16;
    if let Some(header_row) = sticky
        && let Some(lines) = match app.rows.get(header_row) {
            Some(SidebarRow::Header { label, count }) => {
                Some(tile_header_lines(app, label, *count, sep_width, true))
            }
            Some(SidebarRow::StaleGroup {
                label,
                count,
                expanded,
            }) => Some(vec![stale_group_line(
                app, label, *count, *expanded, false, sep_width,
            )]),
            _ => None,
        }
    {
        f.render_widget(
            ratatui::text::Text::from(lines),
            Rect::new(area.x, area.y, area.width, PIN_ROWS as u16),
        );
    }
    let list_area = Rect::new(
        area.x,
        area.y + pin_rows,
        area.width,
        area.height - pin_rows - u16::from(more),
    );
    if let Some(view) = &viewport {
        *app.list_state.offset_mut() = view.start;
        let visible_height: usize = heights[view.start..view.end].iter().sum();
        // The pinned band is not a row, so it must not resolve to an agent.
        app.list_area = Rect::new(area.x, list_area.y, area.width, visible_height as u16);
    } else {
        app.list_area = Rect::new(area.x, list_area.y, area.width, 0);
    }

    // Selection backgrounds belong to tile content, not separators or the footer.
    let list = List::new(items).scroll_padding(0);
    f.render_stateful_widget(list, list_area, &mut app.list_state);

    if let Some(view) = viewport.filter(|view| view.more) {
        // Count agents, including the ones a collapsed toggle stands for,
        // never presentation rows.
        let hidden: usize = app.rows[view.end..]
            .iter()
            .map(|row| match row {
                SidebarRow::Agent(_) => 1,
                SidebarRow::StaleTail {
                    count,
                    expanded: false,
                    ..
                }
                | SidebarRow::StaleGroup {
                    count,
                    expanded: false,
                    ..
                } => *count,
                _ => 0,
            })
            .sum();
        let text = truncate_to_width(&format!("↓ {} more", hidden), area.width as usize);
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
            });
        }
        app.rebuild_rows();
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

    fn grouped_tile_app() -> SidebarApp {
        let mut app = tile_app();
        app.group_by = Some(crate::config::SidebarGroupBy::Session);
        app.rebuild_rows();
        app.list_state.select(app.row_of_agent(0));
        app
    }

    fn grouped_compact_app() -> SidebarApp {
        let mut app = tile_fixture();
        app.layout_mode = SidebarLayoutMode::Compact;
        app.group_by = Some(crate::config::SidebarGroupBy::Session);
        app.rebuild_rows();
        app.list_state.select(app.row_of_agent(0));
        app
    }

    fn rendered(app: &mut SidebarApp, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render_sidebar(f, app)).unwrap();
        (0..height)
            .map(|y| {
                buffer_row(terminal.backend().buffer(), y)
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn compact_headers_label_each_group_and_are_not_selectable() {
        let mut app = grouped_compact_app();
        let lines = rendered(&mut app, 30, 9);

        assert_eq!(lines[0].trim(), "api                      3");
        assert!(lines[1].contains("auth-refresh"));
        assert_eq!(lines[4].trim(), "mobile                   2");
        assert_eq!(lines[7].trim(), "workmux                  1");

        // Headers hold no agent; the agents around them still resolve.
        assert_eq!(app.hit_test(1, 0), None);
        assert_eq!(app.hit_test(1, 1), Some(0));
        assert_eq!(app.hit_test(1, 4), None);
        assert_eq!(app.hit_test(1, 5), Some(3));
    }

    #[test]
    fn stale_agents_fold_behind_a_toggle_and_expand_to_one_line_each() {
        let mut app = tile_app();
        app.group_by = Some(crate::config::SidebarGroupBy::Session);
        app.collapse_stale = true;
        app.templates.tiles = ["{primary} {pane_suffix} {fill}", "{pane_title} {fill}"]
            .into_iter()
            .map(|line| super::super::template::parser::parse_line(line).unwrap())
            .collect();
        // Working and waiting agents are never stale. Clearing the status of
        // the rest leaves them with no activity at all, which is stale.
        for idx in [1, 2] {
            app.agents[idx].status = None;
        }
        app.refresh_stale_pane_ids();
        app.rebuild_rows();
        app.list_state.select(app.row_of_agent(0));

        let lines = rendered(&mut app, 34, 20);

        // The live agent keeps both of its tile lines; the stale ones are one
        // toggle row saying how many it stands for.
        assert_eq!(lines[0].trim(), "api                           3");
        assert!(lines[1].contains("auth-refresh"));
        assert!(lines[3].starts_with('─'));
        assert_eq!(lines[4].trim(), "▸ 2 stale");
        assert_eq!(lines[6].trim(), "mobile                        2");

        // Clicking the toggle shows them, one line each, with no divider
        // splitting the block they form.
        let group = app.hit_test_toggle(1, 4).expect("toggle under the cursor");
        assert_eq!(group, "api");
        app.expanded_groups.insert(group);
        app.rebuild_rows();
        let lines = rendered(&mut app, 34, 20);
        assert_eq!(lines[4].trim(), "▾ 2 stale");
        assert!(lines[5].contains("rate-limit (1)"));
        assert!(lines[6].contains("rate-limit (2)"));
        assert!(lines[7].starts_with('─'));
    }

    #[test]
    fn a_selected_toggle_shows_it_is_selected_in_both_layouts() {
        let mut app = tile_app();
        app.group_by = Some(crate::config::SidebarGroupBy::Session);
        app.collapse_stale = true;
        app.host_agent_idx = None;
        for idx in [1, 2] {
            app.agents[idx].status = None;
        }
        app.refresh_stale_pane_ids();
        app.rebuild_rows();
        let toggle = app
            .rows
            .iter()
            .position(|row| matches!(row, SidebarRow::StaleTail { .. }))
            .expect("a toggle row");
        app.list_state.select(Some(toggle));

        for mode in [SidebarLayoutMode::Tiles, SidebarLayoutMode::Compact] {
            app.layout_mode = mode;
            let mut terminal = Terminal::new(TestBackend::new(34, 20)).unwrap();
            terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let y = (0..20)
                .find(|y| buffer_row(buffer, *y).contains("stale"))
                .expect("the toggle is drawn");
            let cell = &buffer[(2, y)];
            assert_eq!(
                cell.bg, app.palette.highlight_row_bg,
                "{mode:?} draws the selected toggle with the selection background"
            );
            assert_eq!(
                cell.fg, app.palette.text,
                "{mode:?} brightens the selected toggle"
            );
        }
    }

    #[test]
    fn a_group_with_no_live_work_collapses_to_its_own_header() {
        let mut app = grouped_compact_app();
        app.collapse_stale = true;
        // The last group's only agent goes quiet, which is how the daemon
        // comes to sort that group last. It is not the sidebar's own window,
        // whose group never collapses.
        app.agents[5].status = None;
        app.host_agent_idx = None;
        app.refresh_stale_pane_ids();
        app.rebuild_rows();
        app.list_state.select(app.row_of_agent(0));

        let lines = rendered(&mut app, 30, 9);

        assert_eq!(lines[0].trim(), "api                      3");
        assert_eq!(lines[4].trim(), "mobile                   2");
        assert!(lines[7].contains(" STALE "));
        // One row for the whole group, header and toggle at once.
        assert_eq!(lines[8].trim(), "▸ workmux                  1");

        // It holds no agent, and toggling it names its own group.
        assert_eq!(app.hit_test(1, 8), None);
        assert_eq!(app.hit_test_toggle(1, 8).as_deref(), Some("workmux"));
    }

    #[test]
    fn compact_group_label_truncates_before_the_count_is_dropped() {
        let mut app = grouped_compact_app();
        let lines = rendered(&mut app, 10, 9);
        assert_eq!(lines[0].trim(), "api  3");

        // Even at the narrowest width the count survives and the label gives way.
        let lines = rendered(&mut app, 6, 9);
        assert!(lines[4].trim().starts_with('…'));
        assert!(lines[4].trim().ends_with('2'));
    }

    #[test]
    fn a_tile_header_is_one_line_above_its_first_agent() {
        let mut app = grouped_tile_app();
        let lines = rendered(&mut app, 24, 26);

        // The label sits directly above its first agent, with no rule between.
        assert_eq!(lines[0].trim(), "api                 3");
        assert!(lines[1].contains("auth-refresh"));
        assert_eq!(app.hit_test(1, 0), None);
        assert_eq!(app.hit_test(1, 1), Some(0));

        // A later group is opened by the divider that closes the previous one.
        let header = lines
            .iter()
            .position(|line| line.trim().starts_with("mobile"))
            .unwrap();
        assert_eq!(lines[header - 1], "─".repeat(24));
        assert!(lines[header + 1].contains("ios-refactor"));
        assert_eq!(app.hit_test(1, header as u16), None);
    }

    #[test]
    fn a_group_header_renders_as_a_full_width_band() {
        let mut app = grouped_tile_app();
        let mut terminal = Terminal::new(TestBackend::new(24, 26)).unwrap();
        terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();

        // Every cell of the header row carries the band background, including
        // the padding past the label and count.
        for x in 0..24 {
            assert_eq!(
                buffer[(x, 0)].style().bg,
                Some(group_band_bg(&app.palette)),
                "column {x} of the header band"
            );
        }
        // The agent row below it does not.
        assert_ne!(buffer[(0, 1)].style().bg, Some(group_band_bg(&app.palette)));
    }

    #[test]
    fn tile_overflow_counts_hidden_agents_not_header_rows() {
        let mut app = grouped_tile_app();
        let lines = rendered(&mut app, 24, 10);
        assert_eq!(lines[9].trim(), "↓ 4 more");
    }

    #[test]
    fn compact_sticky_header_pins_the_current_group() {
        let mut app = grouped_compact_app();
        app.select_index(2); // third api agent
        let lines = rendered(&mut app, 24, 3);

        // Viewport starts inside the api group, so its header is pinned.
        assert_eq!(lines[0].trim(), "api                3");
        assert!(lines[2].contains("rate-limit"));
        // The pinned band is not a row and resolves to no agent.
        assert_eq!(app.hit_test(1, 0), None);
        assert_eq!(app.hit_test(1, 1), Some(1));
    }

    #[test]
    fn compact_sticky_header_is_not_duplicated_or_stale_at_a_boundary() {
        let mut app = grouped_compact_app();

        // Real header visible at the top: no pin, no duplicate.
        app.select_index(1);
        let lines = rendered(&mut app, 24, 5);
        assert_eq!(lines[0].trim(), "api                3");
        assert!(lines[1].contains("auth-refresh"));

        // Scrolling into the next group swaps the pinned header.
        app.select_index(4);
        let lines = rendered(&mut app, 24, 3);
        assert!(lines.iter().any(|line| line.trim().starts_with("mobile")));
        assert!(!lines.iter().any(|line| line.trim().starts_with("api")));
    }

    #[test]
    fn tile_sticky_header_matches_an_inline_header_and_yields_to_the_selection() {
        let mut app = grouped_tile_app();
        app.select_index(2);
        let lines = rendered(&mut app, 24, 8);
        assert_eq!(lines[0].trim(), "api                 3");
        // One pinned line, then list content, never a second header line.
        assert!(lines[1] == "─".repeat(24) || lines[1].starts_with('▌'));
        assert_eq!(app.hit_test(1, 0), None);

        // Too short to hold the pin and the selected tile: the selection wins.
        let lines = rendered(&mut app, 24, 4);
        assert!(lines.iter().any(|line| line.contains("rate-limit")));
        assert!(
            !lines
                .iter()
                .any(|line| line.trim() == "api                 3")
        );
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
        app.rebuild_rows();
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
            })
            .collect();
        app.rebuild_rows();
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
    fn a_header_can_tally_what_its_group_holds() {
        let mut app = grouped_tile_app();
        app.templates.group_header = crate::command::sidebar::template::parser::parse_line(
            "{group} {fill} {group_status} {group_count}",
        )
        .unwrap();
        app.refresh_stale_pane_ids();

        let counts = group_status_counts(&app, "api");
        let tally: Vec<(String, usize)> = counts
            .iter()
            .map(|status| {
                (
                    status.icon.iter().map(|(text, _)| text.as_str()).collect(),
                    status.count,
                )
            })
            .collect();
        // Waiting first, stale last: the order the header should be read in.
        // The working icon is an animation frame, so only its count is fixed.
        assert_eq!(tally.len(), 3);
        assert_eq!(tally[0], ("\u{1f4ac}".to_string(), 1));
        assert_eq!(tally[1].1, 1);
        assert_eq!(tally[2], ("\u{1f4a4}".to_string(), 1));

        // The tally reaches the drawn header, not just the helper.
        let lines = rendered(&mut app, 36, 26);
        assert!(lines[0].contains('1'), "header was {:?}", lines[0]);
    }

    #[test]
    fn the_grouping_hint_waits_for_a_sidebar_it_would_change() {
        let mut app = grouped_tile_app();
        app.hint_pending = true;
        app.group_by = None;
        app.rebuild_rows();
        assert!(app.show_hint(), "flat list across projects offers the hint");

        // Grouped: the offer survives the switch it invited, so acting on it
        // rewords the line instead of removing the row under the list.
        app.group_by = Some(crate::config::SidebarGroupBy::Project);
        assert!(app.show_hint());
        let lines = rendered(&mut app, 36, 12);
        assert!(
            lines.last().unwrap().contains("flat list"),
            "footer was {:?}",
            lines.last()
        );
        app.group_by = None;

        // The top bar cannot spare a line, and a template error owns the same
        // one and matters more.
        app.position = crate::config::SidebarPosition::Top;
        assert!(!app.show_hint());
        app.position = crate::config::SidebarPosition::Left;
        app.template_error = Some(TemplateError {
            location: "tiles[0]".to_string(),
            message: "unknown token".to_string(),
        });
        assert!(!app.show_hint());
        app.template_error = None;

        // One project: sections would only add a header.
        app.agents
            .retain(|agent| agent.path.starts_with("/demo/api"));
        app.rebuild_rows();
        assert!(!app.show_hint());
    }

    #[test]
    fn the_grouping_hint_draws_on_the_footer_line() {
        let mut app = grouped_tile_app();
        app.hint_pending = true;
        app.group_by = None;
        app.filter_mode = super::SidebarFilterMode::None;
        app.rebuild_rows();

        let lines = rendered(&mut app, 36, 12);
        let footer = lines.last().unwrap();
        assert!(footer.contains("group by project"), "footer was {footer:?}");
        assert!(footer.contains('?'));
        // The hint takes the last row and nothing else: the list stops above it.
        assert!(app.list_area.y + app.list_area.height <= 11);
    }

    #[test]
    fn render_sidebar_shows_the_keys_behind_question_mark() {
        let backend = TestBackend::new(34, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });
        app.template_error = None;
        app.show_help = true;

        let text = |terminal: &Terminal<TestBackend>| {
            let buffer = terminal.backend().buffer();
            (0..20)
                .flat_map(|y| (0..34).map(move |x| buffer[(x, y)].symbol()))
                .collect::<String>()
        };

        terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();
        let flat = text(&terminal);
        assert!(flat.contains("grouping"));
        assert!(flat.contains("quit"));
        // Nothing is grouped, so the group keys would act on nothing.
        assert!(!flat.contains("fold"));

        app.group_by = Some(crate::config::SidebarGroupBy::Project);
        terminal.draw(|f| render_sidebar(f, &mut app)).unwrap();
        let grouped = text(&terminal);
        assert!(grouped.contains("fold group"));
        assert!(grouped.contains("fold unfold"));
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
