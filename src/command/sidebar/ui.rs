//! Rendering for the sidebar TUI.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, Padding};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_width::UnicodeWidthChar;

use crate::agent_display::{extract_project_name, extract_worktree_name, strip_oc_title_prefix};
use crate::agent_identity::classify_agent_command;
use crate::config::AgentIdentityDisplay;
use crate::git::GitStatus;
use crate::multiplexer::{AgentPane, AgentStatus};
use crate::tmux_style;
use crate::ui::theme::ThemePalette;

use super::app::{SidebarApp, SidebarLayoutMode};

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
                format!(" ({})", pos)
            } else {
                String::new()
            }
        })
        .collect()
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
fn format_sidebar_git_stats(
    status: Option<&GitStatus>,
    palette: &ThemePalette,
    is_stale: bool,
    available_width: usize,
) -> (Vec<(String, Style)>, usize) {
    let Some(status) = status else {
        return (vec![], 0);
    };

    let icons = crate::nerdfont::git_icons();

    // When stale, force all colors to dimmed
    let success = if is_stale {
        palette.dimmed
    } else {
        palette.success
    };
    let danger = if is_stale {
        palette.dimmed
    } else {
        palette.danger
    };
    let accent = if is_stale {
        palette.dimmed
    } else {
        palette.accent
    };

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

    // Width of a set of spans: text widths + spaces between + trailing space
    let calc_width = |spans: &[(String, Style)]| -> usize {
        if spans.is_empty() {
            return 0;
        }
        spans.iter().map(|(s, _)| display_width(s)).sum::<usize>() + spans.len()
    };

    // Build rebase indicator (shown first, highest priority)
    let mut rebase_spans: Vec<(String, Style)> = Vec::new();
    if status.is_rebasing {
        let rebase_color = if is_stale {
            palette.dimmed
        } else {
            palette.warning
        };
        rebase_spans.push((icons.rebase.to_string(), Style::default().fg(rebase_color)));
    }

    // Build uncommitted spans (bright, with diff icon)
    let mut uncommitted_spans: Vec<(String, Style)> = Vec::new();
    if has_uncommitted {
        uncommitted_spans.push((icons.diff.to_string(), Style::default().fg(accent)));
        if status.uncommitted_added > 0 {
            uncommitted_spans.push((
                format!("+{}", status.uncommitted_added),
                Style::default().fg(success),
            ));
        }
        if status.uncommitted_removed > 0 {
            uncommitted_spans.push((
                format!("-{}", status.uncommitted_removed),
                Style::default().fg(danger),
            ));
        }
    }

    // Build committed spans (dimmed) - skip if all changes are uncommitted
    let mut committed_spans: Vec<(String, Style)> = Vec::new();
    if has_committed && !all_uncommitted {
        if status.lines_added > 0 {
            committed_spans.push((
                format!("+{}", status.lines_added),
                Style::default().fg(success).add_modifier(Modifier::DIM),
            ));
        }
        if status.lines_removed > 0 {
            committed_spans.push((
                format!("-{}", status.lines_removed),
                Style::default().fg(danger).add_modifier(Modifier::DIM),
            ));
        }
    }

    let rebase_width = calc_width(&rebase_spans);
    let committed_width = calc_width(&committed_spans);
    let uncommitted_width = calc_width(&uncommitted_spans);

    // Trailing space of each group acts as separator when concatenated
    let full_width = rebase_width + committed_width + uncommitted_width;
    let no_committed_width = rebase_width + uncommitted_width;

    // Priority: full > drop committed > drop uncommitted > rebase only > nothing
    if full_width > 0 && full_width <= available_width {
        let mut spans = rebase_spans;
        spans.extend(committed_spans);
        spans.extend(uncommitted_spans);
        (spans, full_width)
    } else if no_committed_width > 0 && no_committed_width <= available_width {
        let mut spans = rebase_spans;
        spans.extend(uncommitted_spans);
        (spans, no_committed_width)
    } else if rebase_width > 0 && rebase_width <= available_width {
        (rebase_spans, rebase_width)
    } else {
        (vec![], 0)
    }
}

/// Render the sidebar UI.
pub fn render_sidebar(f: &mut Frame, app: &mut SidebarApp) {
    let area = f.area();

    let padding = match app.layout_mode {
        // Compact mode: pad both sides for breathing room
        SidebarLayoutMode::Compact => Padding::new(1, 1, 0, 0),
        // Tile mode: stripe provides left edge, border is already excluded from inner area
        SidebarLayoutMode::Tiles => Padding::ZERO,
    };

    let block = Block::default().padding(padding);

    let inner = block.inner(area);
    f.render_widget(block, area);
    app.list_area = inner;

    match app.layout_mode {
        SidebarLayoutMode::Compact => render_compact_list(f, app, inner),
        SidebarLayoutMode::Tiles => render_tile_list(f, app, inner),
    }
}

/// Compact single-line-per-agent list (original layout).
fn render_compact_list(f: &mut Frame, app: &mut SidebarApp, area: Rect) {
    if app.agents.is_empty() {
        render_empty_state(f, app, area);
        return;
    }

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let pane_suffixes = compute_pane_suffixes(&app.agents);

    let items: Vec<ListItem> = app
        .agents
        .iter()
        .enumerate()
        .map(|(idx, agent)| {
            let worktree_name = format!("{}{}", app.display_name(agent), pane_suffixes[idx]);

            let is_sleeping = app.sleeping_pane_ids.contains(&agent.pane_id);
            let is_stale = agent
                .status_ts
                .map(|ts| now_secs.saturating_sub(ts) > app.stale_threshold_secs)
                .unwrap_or(false);
            // Auto-stale only for Done/None; manual sleeping always applies
            let is_stale = is_sleeping
                || (is_stale
                    && !matches!(
                        agent.status,
                        Some(AgentStatus::Working) | Some(AgentStatus::Waiting)
                    ));
            let is_interrupted = app.interrupted_pane_ids.contains(&agent.pane_id);
            // Status icon
            let (icon_spans, _icon_style) =
                status_icon_and_style(app, agent.status, is_stale, is_interrupted);

            // Elapsed time: hide when interrupted
            let elapsed = if is_interrupted {
                String::new()
            } else {
                agent
                    .status_ts
                    .map(|ts| format_compact_elapsed(now_secs.saturating_sub(ts)))
                    .unwrap_or_default()
            };

            // Pad icon to fixed 2-column width so emoji and spinners align
            let icon_cols: usize = icon_spans.iter().map(|(t, _)| display_width(t)).sum();
            let icon_pad = if icon_cols < 2 {
                " ".repeat(2 - icon_cols)
            } else {
                String::new()
            };

            // Calculate available width for the name.
            // Layout: "{icon}{pad} {identity} {name} {elapsed}"
            let elapsed_width = elapsed.len();
            let base_reserved = 2 + 1 + 1 + elapsed_width;
            let min_name_width = 8.min(display_width(&worktree_name));
            let identity_max_width = (area.width as usize)
                .saturating_sub(base_reserved)
                .saturating_sub(min_name_width)
                .saturating_sub(1);
            let identity = agent_identity_text(app, agent, identity_max_width);
            let identity_width = display_width(&identity);
            let identity_reserved = if identity.is_empty() {
                0
            } else {
                identity_width + 1
            };
            // Reserve: base + optional identity + separating space
            let reserved = base_reserved + identity_reserved;
            let name_width = (area.width as usize).saturating_sub(reserved);

            let display_name = truncate_to_width(&worktree_name, name_width);
            let padding = name_width.saturating_sub(display_width(&display_name));

            let is_active = app.host_agent_idx == Some(idx);

            let name_style = if is_stale {
                Style::default()
                    .fg(app.palette.dimmed)
                    .add_modifier(Modifier::DIM)
            } else if is_active {
                Style::default()
                    .fg(app.palette.current_worktree_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(app.palette.text)
            };

            let elapsed_style = if is_stale {
                Style::default().fg(app.palette.dimmed)
            } else {
                Style::default().fg(app.palette.text)
            };

            let mut line_spans: Vec<Span> = icon_spans
                .into_iter()
                .map(|(text, style)| Span::styled(text, style))
                .collect();
            line_spans.extend([Span::raw(icon_pad), Span::raw(" ")]);
            if !identity.is_empty() {
                line_spans.push(Span::styled(identity, identity_style(app, is_stale)));
                line_spans.push(Span::raw(" "));
            }
            line_spans.extend([
                Span::styled(display_name, name_style),
                Span::raw(" ".repeat(padding)),
                Span::raw(" "),
                Span::styled(elapsed, elapsed_style),
            ]);
            let line = Line::from(line_spans);

            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).highlight_style(Style::default().bg(app.palette.highlight_row_bg));

    f.render_stateful_widget(list, area, &mut app.list_state);
}

/// Tile layout: variable-height cards per agent with status stripe.
fn render_tile_list(f: &mut Frame, app: &mut SidebarApp, area: Rect) {
    if app.agents.is_empty() {
        render_empty_state(f, app, area);
        return;
    }

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let sep_width = area.width as usize;
    let selected_idx = app.list_state.selected();
    let agent_count = app.agents.len();
    let pane_suffixes = compute_pane_suffixes(&app.agents);

    let items: Vec<ListItem> = app
        .agents
        .iter()
        .enumerate()
        .map(|(idx, agent)| {
            let is_selected = selected_idx == Some(idx);
            let project = extract_project_name(&agent.path);
            let (worktree, is_main) = extract_worktree_name(
                &agent.session,
                &agent.window_name,
                app.window_prefix(),
                &agent.path,
            );

            let base_worktree = if is_main {
                "main".to_string()
            } else {
                worktree.to_string()
            };
            let pane_suffix = &pane_suffixes[idx];
            let display_worktree = format!("{}{}", base_worktree, pane_suffix);

            let is_sleeping = app.sleeping_pane_ids.contains(&agent.pane_id);
            let is_stale = agent
                .status_ts
                .map(|ts| now_secs.saturating_sub(ts) > app.stale_threshold_secs)
                .unwrap_or(false);
            // Auto-stale only for Done/None; manual sleeping always applies
            let is_stale = is_sleeping
                || (is_stale
                    && !matches!(
                        agent.status,
                        Some(AgentStatus::Working) | Some(AgentStatus::Waiting)
                    ));
            let is_interrupted = app.interrupted_pane_ids.contains(&agent.pane_id);
            let is_active = app.host_agent_idx == Some(idx);

            // Status icon and color
            let (icon_spans, icon_base_style) =
                status_icon_and_style(app, agent.status, is_stale, is_interrupted);
            let status_color = icon_base_style.fg.unwrap_or(ratatui::style::Color::Reset);

            // Stripe color on all lines; stale forces dimmed
            let stripe_color = if is_stale {
                app.palette.dimmed
            } else {
                status_color
            };
            let stripe_style = Style::default().fg(stripe_color);

            // Elapsed time: hide when interrupted
            let elapsed = if is_interrupted {
                String::new()
            } else {
                agent
                    .status_ts
                    .map(|ts| format_compact_elapsed(now_secs.saturating_sub(ts)))
                    .unwrap_or_default()
            };

            // Pad icon to fixed 2-column width
            let icon_cols: usize = icon_spans.iter().map(|(t, _)| display_width(t)).sum();
            let icon_pad = if icon_cols < 2 {
                " ".repeat(2 - icon_cols)
            } else {
                String::new()
            };

            // Line 1 content width:
            // area - stripe(2) - icon slot(2) - spaces - optional identity - elapsed.
            let base_line1_reserved = 2 + 2 + 1 + 1 + 1 + elapsed.len();
            let min_name_width = 8.min(display_width(&display_worktree));
            let identity_max_width = (area.width as usize)
                .saturating_sub(base_line1_reserved)
                .saturating_sub(min_name_width)
                .saturating_sub(1);
            let identity = agent_identity_text(app, agent, identity_max_width);
            let identity_width = display_width(&identity);
            let identity_reserved = if identity.is_empty() {
                0
            } else {
                identity_width + 1
            };
            let line1_name_width =
                (area.width as usize).saturating_sub(base_line1_reserved + identity_reserved);
            // Body lines indent to align with worktree name: icon(2) + gap(1) = 3
            let body_indent = "   ";
            let body_width = (area.width as usize).saturating_sub(2 + body_indent.len());

            // Truncate using the full string (base + suffix) for width calculation,
            // then split back into name part and suffix part for separate styling.
            let display_full = truncate_with_ellipsis(&display_worktree, line1_name_width);
            let full_width = display_width(&display_full);
            let name_padding = line1_name_width.saturating_sub(full_width);

            // Split: if the suffix is still fully present in the truncated string, render it dimmed
            let (display_name_part, display_suffix_part) =
                if !pane_suffix.is_empty() && display_full.ends_with(pane_suffix) {
                    let name_end = display_full.len() - pane_suffix.len();
                    (
                        display_full[..name_end].to_string(),
                        pane_suffix.to_string(),
                    )
                } else {
                    (display_full, String::new())
                };

            // Styles - apply highlight background on selected tiles' content lines
            let bg = if is_selected {
                Some(app.palette.highlight_row_bg)
            } else {
                None
            };

            let mut name_style = if is_stale {
                Style::default()
                    .fg(app.palette.dimmed)
                    .add_modifier(Modifier::DIM)
            } else if is_active {
                Style::default()
                    .fg(app.palette.current_worktree_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(app.palette.text)
            };

            let mut project_style = if is_stale {
                Style::default()
                    .fg(app.palette.dimmed)
                    .add_modifier(Modifier::DIM)
            } else {
                Style::default()
                    .fg(app.palette.text)
                    .add_modifier(Modifier::DIM)
            };

            let mut body_style = if is_stale {
                Style::default()
                    .fg(app.palette.dimmed)
                    .add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(app.palette.dimmed)
            };

            let mut elapsed_style = if is_stale {
                Style::default().fg(app.palette.dimmed)
            } else {
                Style::default().fg(app.palette.text)
            };

            let mut stripe_bg_style = stripe_style;

            if let Some(bg_color) = bg {
                name_style = name_style.bg(bg_color);
                project_style = project_style.bg(bg_color);
                body_style = body_style.bg(bg_color);
                elapsed_style = elapsed_style.bg(bg_color);
                stripe_bg_style = stripe_bg_style.bg(bg_color);
            }

            // Padding style (spaces that need background in selected state)
            let pad_style = bg.map(|c| Style::default().bg(c)).unwrap_or_default();

            // Line 1: ▌ icon  worktree-name (N)    elapsed
            // Compute used width to pad trailing space
            let line1_used = 2
                + icon_cols
                + (2usize.saturating_sub(icon_cols))
                + 1
                + identity_reserved
                + full_width
                + name_padding
                + 1
                + elapsed.len();
            let line1_trail = (area.width as usize).saturating_sub(line1_used);

            // Suffix style: slightly dimmed relative to the name
            let mut suffix_style = Style::default().fg(app.palette.dimmed);
            if let Some(bg_color) = bg {
                suffix_style = suffix_style.bg(bg_color);
            }

            // Build icon spans with optional background for selected state
            let mut line1_spans: Vec<Span> = vec![Span::styled("▌ ", stripe_bg_style)];
            for (text, mut style) in icon_spans {
                if let Some(bg_color) = bg {
                    style = style.bg(bg_color);
                }
                line1_spans.push(Span::styled(text, style));
            }
            line1_spans.extend([
                Span::styled(icon_pad, pad_style),
                Span::styled(" ", pad_style),
            ]);
            if !identity.is_empty() {
                let mut identity_style = identity_style(app, is_stale);
                if let Some(bg_color) = bg {
                    identity_style = identity_style.bg(bg_color);
                }
                line1_spans.push(Span::styled(identity, identity_style));
                line1_spans.push(Span::styled(" ", pad_style));
            }
            line1_spans.extend([
                Span::styled(display_name_part, name_style),
                Span::styled(display_suffix_part, suffix_style),
                Span::styled(" ".repeat(name_padding), pad_style),
                Span::styled(" ", pad_style),
                Span::styled(elapsed, elapsed_style),
                Span::styled(" ".repeat(line1_trail), pad_style),
            ]);
            let line1 = Line::from(line1_spans);

            // Line 2: ▌   project name          +N -M *+X -Y
            // Priority: project name > uncommitted stats > committed stats
            // Give project a minimum width, then let git stats use what remains
            let git_status = app.git_statuses.get(&agent.path);
            let project_full_width = display_width(&project);
            let min_project_width = 5.min(project_full_width);
            let git_available = body_width.saturating_sub(min_project_width + 1); // +1 for gap
            let (git_spans, git_width) =
                format_sidebar_git_stats(git_status, &app.palette, is_stale, git_available);

            // Project gets remaining space after git stats
            let project_max_width = if git_width > 0 {
                body_width.saturating_sub(git_width + 1)
            } else {
                body_width
            };
            let project_display = truncate_with_ellipsis(&project, project_max_width);
            let project_display_width = display_width(&project_display);

            // Padding between project name and git stats
            let middle_padding = body_width
                .saturating_sub(project_display_width)
                .saturating_sub(git_width);

            let mut line2_spans = vec![
                Span::styled("▌ ", stripe_bg_style),
                Span::styled(body_indent, pad_style),
                Span::styled(project_display, project_style),
                Span::styled(" ".repeat(middle_padding), pad_style),
            ];

            // Append git stat spans with proper background + trailing space for right padding
            let mut first_git = true;
            for (text, mut style) in git_spans {
                if !first_git {
                    line2_spans.push(Span::styled(" ", pad_style));
                }
                first_git = false;
                if let Some(bg_color) = bg {
                    style = style.bg(bg_color);
                }
                line2_spans.push(Span::styled(text, style));
            }
            if !first_git {
                // Add trailing space for right padding
                line2_spans.push(Span::styled(" ", pad_style));
            }

            let line2 = Line::from(line2_spans);

            // Optional: pane_title (task description) when available
            let title = sanitize_pane_title(agent.pane_title.as_deref(), &worktree, &project);

            // Separator at the top (between tiles, not on first item)
            let mut lines = Vec::new();
            if idx > 0 {
                lines.push(Line::from(Span::styled(
                    "─".repeat(sep_width),
                    Style::default().fg(app.palette.border),
                )));
            }

            lines.push(line1);
            lines.push(line2);

            if let Some(title_text) = title {
                // Reserve 1 char for right padding so ellipsis doesn't touch the edge
                let title_max = body_width.saturating_sub(1);
                let title_display = truncate_with_ellipsis(title_text, title_max);
                let title_padding = body_width.saturating_sub(display_width(&title_display));
                lines.push(Line::from(vec![
                    Span::styled("▌ ", stripe_bg_style),
                    Span::styled(body_indent, pad_style),
                    Span::styled(title_display, body_style),
                    Span::styled(" ".repeat(title_padding), pad_style),
                ]));
            } else {
                let empty_padding = body_width;
                lines.push(Line::from(vec![
                    Span::styled("▌ ", stripe_bg_style),
                    Span::styled(body_indent, pad_style),
                    Span::styled(" ".repeat(empty_padding), pad_style),
                ]));
            }

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

    // No highlight_style - background is baked into content lines to avoid highlighting separators
    let list = List::new(items);

    f.render_stateful_widget(list, area, &mut app.list_state);
}

/// Build the agent identity badge text for the configured display mode.
fn agent_identity_text(app: &SidebarApp, agent: &AgentPane, max_width: usize) -> String {
    if max_width == 0 || app.agent_identity == AgentIdentityDisplay::Off {
        return String::new();
    }

    let Some(kind) = classify_agent_command(agent.agent_command.as_deref()) else {
        return String::new();
    };

    let icon = app.agent_icons.icon_for(kind);
    let label = kind.label();

    choose_identity_text(app.agent_identity, icon, label, max_width)
}

fn choose_identity_text(
    mode: AgentIdentityDisplay,
    icon: &str,
    label: &str,
    max_width: usize,
) -> String {
    if max_width == 0 {
        return String::new();
    }

    let candidates: Vec<String> = match mode {
        AgentIdentityDisplay::Off => return String::new(),
        AgentIdentityDisplay::Icon => vec![icon.to_string()],
        AgentIdentityDisplay::Label => vec![label.to_string()],
        AgentIdentityDisplay::Both => vec![
            format!("{} {}", icon, label),
            icon.to_string(),
            label.to_string(),
        ],
    };

    candidates
        .iter()
        .find(|candidate| display_width(candidate) <= max_width)
        .cloned()
        .unwrap_or_default()
}

fn identity_style(app: &SidebarApp, is_stale: bool) -> Style {
    if is_stale {
        Style::default()
            .fg(app.palette.dimmed)
            .add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(app.palette.accent)
    }
}

/// Get the status icon as parsed styled spans and the base style for an agent.
///
/// Returns `(spans, base_style)` where `spans` contains tmux style codes parsed into
/// individual `(text, style)` pairs, and `base_style` is the fallback style (used for
/// stripe color, etc.).
fn status_icon_and_style(
    app: &SidebarApp,
    status: Option<AgentStatus>,
    is_stale: bool,
    is_interrupted: bool,
) -> (Vec<(String, Style)>, Style) {
    if is_stale {
        let style = Style::default().fg(app.palette.dimmed);
        return (vec![("💤".to_string(), style)], style);
    }
    if is_interrupted {
        let style = Style::default().fg(app.palette.dimmed);
        return (vec![("  ".to_string(), style)], style);
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
            let spans = tmux_style::parse_tmux_styles(app.status_icons.waiting(), base_style);
            (spans, base_style)
        }
        Some(AgentStatus::Done) => {
            let base_style = Style::default().fg(app.palette.success);
            let spans = tmux_style::parse_tmux_styles(app.status_icons.done(), base_style);
            (spans, base_style)
        }
        None => {
            let style = Style::default().fg(app.palette.dimmed);
            (vec![("  ".to_string(), style)], style)
        }
    }
}

/// Clean up a pane title, returning None if it's noise.
fn sanitize_pane_title<'a>(raw: Option<&'a str>, worktree: &str, project: &str) -> Option<&'a str> {
    let title = raw?.trim();
    if title.is_empty() {
        return None;
    }

    // Strip leading status icon characters (braille spinners, symbols like ✳)
    let title = title
        .trim_start_matches(|c: char| {
            // Braille patterns U+2800..U+28FF, common status symbols
            ('\u{2800}'..='\u{28FF}').contains(&c)
                || matches!(c, '✳' | '⠀' | '●' | '○' | '◌' | '✓' | '✗')
        })
        .trim();

    let title = strip_oc_title_prefix(title);

    if title.is_empty() {
        return None;
    }

    // Filter out generic "Claude Code" titles (with optional version)
    if title.starts_with("Claude Code") {
        return None;
    }

    // Filter out shell names
    if matches!(title, "zsh" | "bash" | "sh" | "fish") {
        return None;
    }

    // Filter if identical to worktree or project name
    if title == worktree || title == project {
        return None;
    }

    Some(title)
}

/// Format elapsed seconds compactly: "5s", "2m", "1h", "3d"
fn format_compact_elapsed(secs: u64) -> String {
    if secs < 3600 {
        // MM:SS timer for agents under 1 hour
        format!("{}:{:02}", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn render_empty_state(f: &mut Frame, app: &SidebarApp, area: Rect) {
    let text = Line::from(Span::styled(
        "No agents running",
        Style::default().fg(app.palette.dimmed),
    ))
    .alignment(Alignment::Center);
    let y = area.y + area.height / 2;
    let centered = Rect::new(area.x, y, area.width, 1);
    f.render_widget(text, centered);
}

/// Get the display width of a string, counting wide chars as 2.
fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(1))
        .sum()
}

/// Truncate a string to fit within a given display width (hard cut, no ellipsis).
fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut result = String::new();
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(1);
        if width + w > max_width {
            break;
        }
        width += w;
        result.push(c);
    }
    result
}

/// Truncate a string to fit within a given display width, adding ellipsis if truncated.
fn truncate_with_ellipsis(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 1 {
        return "\u{2026}".to_string();
    }

    let mut out = String::new();
    let mut width = 0;
    for c in s.chars() {
        let char_width = UnicodeWidthChar::width(c).unwrap_or(1);
        // Reserve 1 column for the ellipsis character
        if width + char_width + 1 > max_width {
            break;
        }
        out.push(c);
        width += char_width;
    }
    // Trim trailing spaces so ellipsis attaches to the last word
    let trimmed = out.trim_end();
    let mut result = trimmed.to_string();
    result.push('\u{2026}');
    result
}

#[cfg(test)]
mod tests {
    use super::{choose_identity_text, sanitize_pane_title};
    use crate::agent_display::strip_oc_title_prefix;
    use crate::config::AgentIdentityDisplay;

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

    #[test]
    fn identity_text_degrades_to_fit_available_width() {
        assert_eq!(
            choose_identity_text(AgentIdentityDisplay::Both, "CX", "Codex", 8),
            "CX Codex"
        );
        assert_eq!(
            choose_identity_text(AgentIdentityDisplay::Both, "CX", "Codex", 2),
            "CX"
        );
        assert_eq!(
            choose_identity_text(AgentIdentityDisplay::Both, "CX", "Codex", 1),
            ""
        );
        assert_eq!(
            choose_identity_text(AgentIdentityDisplay::Label, "CX", "Codex", 4),
            ""
        );
        assert_eq!(
            choose_identity_text(AgentIdentityDisplay::Off, "CX", "Codex", 8),
            ""
        );
    }
}
