//! Row abstraction shared by agent rows and group header rows.
//!
//! The layout solver works over this trait so headers reuse the same fill,
//! truncation, and style handling as agent rows without pretending to be an
//! agent.

use ratatui::style::{Modifier, Style};

use crate::ui::theme::ThemePalette;

use super::TokenId;
use super::context::display_width;

/// A row the layout solver can render.
///
/// Agent-specific accessors default to empty so a row that has no agent only
/// implements the value and style lookups.
pub trait TemplateRow {
    /// Display string for a token.
    fn resolve(&self, token: TokenId) -> String;

    /// Style a token uses unless the template overrides it with `#[...]`.
    fn intrinsic_style(&self, token: TokenId) -> Style;

    /// Natural display width of a token before any layout constraints.
    fn natural_width(&self, token: TokenId) -> usize {
        display_width(&self.resolve(token))
    }

    /// Pre-styled spans for `{status_icon}`.
    fn status_icon_spans(&self) -> &[(String, Style)] {
        &[]
    }

    /// Whether the row uses the dimmed, stale treatment.
    fn is_stale(&self) -> bool {
        false
    }

    /// Spans for a git segment token within an allocated width.
    fn git_segment_spans(&self, _token: TokenId, _width: usize) -> (Vec<(String, Style)>, usize) {
        (Vec::new(), 0)
    }

    /// Spans for `{pr_checks}` within an allocated width.
    fn pr_check_spans(&self, _width: usize) -> (Vec<(String, Style)>, usize) {
        (Vec::new(), 0)
    }

    /// Spans for `{group_status}` within an allocated width.
    fn group_status_spans(&self, _width: usize) -> (Vec<(String, Style)>, usize) {
        (Vec::new(), 0)
    }
}

/// One status the agents of a group are in, with how many are in it. The icon
/// carries its own spans so a configured multi-part icon keeps its styling.
pub struct GroupStatusCount {
    pub icon: Vec<(String, Style)>,
    pub count: usize,
    /// Columns the icon draws beyond its measured width. Some status glyphs
    /// are drawn double width while measuring one, and without the allowance
    /// the overhang paints over the count beside it.
    pub pad: usize,
}

impl GroupStatusCount {
    /// Width of this pair drawn on its own, icon plus the count beside it.
    fn width(&self) -> usize {
        let icon: usize = self.icon.iter().map(|(text, _)| display_width(text)).sum();
        icon + self.pad + display_width(&self.count.to_string())
    }
}

/// Context for a group header row.
pub struct HeaderContext<'a> {
    pub label: String,
    pub count: usize,
    /// Statuses present in the group, most urgent first. Empty unless the
    /// header template asks for them.
    pub statuses: Vec<GroupStatusCount>,
    pub palette: &'a ThemePalette,
}

impl TemplateRow for HeaderContext<'_> {
    fn resolve(&self, token: TokenId) -> String {
        match token {
            TokenId::Group => self.label.clone(),
            TokenId::GroupCount => self.count.to_string(),
            TokenId::GroupStatus => self
                .statuses
                .iter()
                .map(|status| {
                    let icon: String = status.icon.iter().map(|(text, _)| text.as_str()).collect();
                    format!("{icon}{}{}", " ".repeat(status.pad), status.count)
                })
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        }
    }

    fn intrinsic_style(&self, token: TokenId) -> Style {
        match token {
            TokenId::Group => Style::default()
                .fg(self.palette.header)
                .add_modifier(Modifier::BOLD),
            _ => Style::default().fg(self.palette.dimmed),
        }
    }

    /// Emit as many status pairs as fit, dropping the least urgent first so a
    /// narrow header keeps what wants attention.
    fn group_status_spans(&self, width: usize) -> (Vec<(String, Style)>, usize) {
        let mut spans: Vec<(String, Style)> = Vec::new();
        let mut used = 0;
        for status in &self.statuses {
            let separator = usize::from(!spans.is_empty());
            if used + separator + status.width() > width {
                break;
            }
            if separator > 0 {
                spans.push((" ".to_string(), Style::default()));
            }
            spans.extend(status.icon.iter().cloned());
            if status.pad > 0 {
                spans.push((" ".repeat(status.pad), Style::default()));
            }
            spans.push((
                status.count.to_string(),
                Style::default().fg(self.palette.dimmed),
            ));
            used += separator + status.width();
        }
        (spans, used)
    }
}
