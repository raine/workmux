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
}

/// Context for a group header row.
pub struct HeaderContext<'a> {
    pub label: String,
    pub count: usize,
    pub palette: &'a ThemePalette,
}

impl TemplateRow for HeaderContext<'_> {
    fn resolve(&self, token: TokenId) -> String {
        match token {
            TokenId::Group => self.label.clone(),
            TokenId::GroupCount => self.count.to_string(),
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
}
