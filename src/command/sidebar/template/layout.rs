//! Layout solver: turns a parsed token line and a RowContext into styled spans.

use ratatui::style::Style;
use ratatui::text::Span;

use super::context::RowContext;
use super::parser::{Token, TokenId};
use crate::tmux_style::apply_tmux_directives;

/// Optional layout constraints applied while rendering a template line.
#[derive(Default)]
pub struct RenderOptions {
    field_min_widths: Vec<(TokenId, usize)>,
}

impl RenderOptions {
    pub fn with_field_min_width(mut self, token: TokenId, width: usize) -> Self {
        self.field_min_widths.push((token, width));
        self
    }

    fn min_width(&self, token: TokenId) -> usize {
        self.field_min_widths
            .iter()
            .rev()
            .find_map(|(id, width)| (*id == token).then_some(*width))
            .unwrap_or(0)
    }
}

/// Render a line of tokens into spans, fitting within `width` columns.
///
/// The algorithm:
/// 1. Split at `{fill}` if present.
/// 2. Right segment: each token renders at natural width.
/// 3. Left segment: fixed tokens at natural width; first flex token absorbs slack.
/// 4. If total exceeds width: drop right-segment field tokens in reverse order.
/// 5. If still exceeding: truncate leftmost flex token with ellipsis.
pub fn render_line(ctx: &RowContext, tokens: &[Token], width: usize) -> Vec<Span<'static>> {
    render_line_with_options(ctx, tokens, width, &RenderOptions::default())
}

pub fn render_line_with_options(
    ctx: &RowContext,
    tokens: &[Token],
    width: usize,
    options: &RenderOptions,
) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }

    let fill_pos = tokens.iter().position(|t| matches!(t, Token::Fill));
    let (left_tokens, right_tokens) = match fill_pos {
        Some(pos) => (&tokens[..pos], &tokens[pos + 1..]),
        None => (tokens, &[][..]),
    };

    // Compute natural widths for all tokens
    let left_info: Vec<TokenInfo> = left_tokens
        .iter()
        .map(|t| TokenInfo::new(t, ctx, options))
        .collect();
    let right_info: Vec<TokenInfo> = right_tokens
        .iter()
        .map(|t| TokenInfo::new(t, ctx, options))
        .collect();
    let left_info = collapse_empty_fields(left_info);
    let right_info = collapse_empty_fields(right_info);

    let right_width: usize = right_info.iter().map(|i| i.natural_width).sum();
    let left_fixed_width: usize = left_info
        .iter()
        .filter(|i| !i.is_flex)
        .map(|i| i.natural_width)
        .sum();

    let mut available = width.saturating_sub(right_width + left_fixed_width);

    // If available is negative, try dropping right-segment field tokens
    if available == 0 && right_width > 0 && right_width + left_fixed_width > width {
        let mut dropped_right_width = right_width;
        let mut right_kept: Vec<&TokenInfo> = right_info.iter().collect();

        // Drop field tokens (not literals) from the right in reverse order.
        // First pop any trailing non-field tokens (zero-width style tokens or
        // literals) so a trailing `#[default]` or whitespace does not block
        // the loop from reaching the field that needs to be dropped.
        loop {
            while let Some(last) = right_kept.last() {
                if !last.is_field {
                    dropped_right_width -= last.natural_width;
                    right_kept.pop();
                } else {
                    break;
                }
            }
            let Some(last) = right_kept.last() else { break };
            if !last.is_field {
                break;
            }
            dropped_right_width -= last.natural_width;
            right_kept.pop();
            // Drop any trailing non-field tokens that follow the dropped field
            while let Some(last) = right_kept.last() {
                if !last.is_field {
                    dropped_right_width -= last.natural_width;
                    right_kept.pop();
                } else {
                    break;
                }
            }
            available = width.saturating_sub(dropped_right_width + left_fixed_width);
            if available > 0 || dropped_right_width + left_fixed_width <= width {
                break;
            }
        }

        // Rebuild right_info from kept tokens
        let right_info: Vec<TokenInfo> = right_kept.into_iter().cloned().collect();
        return render_with_layout(ctx, &left_info, &right_info, width, available);
    }

    render_with_layout(ctx, &left_info, &right_info, width, available)
}

/// Check whether a configured tile line is intentionally blank.
///
/// Blank tile templates are skipped for every agent. Rows containing fields are
/// still rendered, even when those fields resolve to empty for a given agent.
pub fn is_blank_template_line(tokens: &[Token]) -> bool {
    tokens.iter().all(|token| match token {
        Token::Literal(s) => s.trim().is_empty(),
        Token::Fill | Token::Style(_) => true,
        Token::Field(_) => false,
    })
}

#[derive(Clone)]
struct TokenInfo {
    token: Token,
    natural_width: usize,
    is_flex: bool,
    is_field: bool,
}

impl TokenInfo {
    fn new(token: &Token, ctx: &RowContext, options: &RenderOptions) -> Self {
        match token {
            Token::Literal(s) => Self {
                token: Token::Literal(s.clone()),
                natural_width: display_width(s),
                is_flex: false,
                is_field: false,
            },
            Token::Fill => Self {
                token: Token::Fill,
                natural_width: 0,
                is_flex: false,
                is_field: false,
            },
            Token::Style(s) => Self {
                token: Token::Style(s.clone()),
                natural_width: 0,
                is_flex: false,
                is_field: false,
            },
            Token::Field(id) => Self {
                token: Token::Field(*id),
                natural_width: ctx.natural_width(*id).max(options.min_width(*id)),
                is_flex: id.is_flex(),
                is_field: true,
            },
        }
    }
}

/// Drop a single adjacent whitespace literal next to any field that resolves
/// to empty, so optional fields like `{pane_suffix}` don't leave dangling
/// joiner spaces in the output.
fn collapse_empty_fields(infos: Vec<TokenInfo>) -> Vec<TokenInfo> {
    let mut keep = vec![true; infos.len()];
    for i in 0..infos.len() {
        let is_empty_field =
            matches!(infos[i].token, Token::Field(_)) && infos[i].natural_width == 0;
        if !is_empty_field {
            continue;
        }
        // Prefer dropping a following whitespace literal, falling back to a
        // preceding one. Skip over zero-width style tokens when scanning, so
        // `{empty_field}#[fg=red] ` still drops the joiner space.
        if let Some(j) = next_kept_neighbor(&infos, &keep, i, true)
            && is_whitespace_literal(&infos[j])
        {
            keep[j] = false;
        } else if let Some(j) = next_kept_neighbor(&infos, &keep, i, false)
            && is_whitespace_literal(&infos[j])
        {
            keep[j] = false;
        }
    }
    infos
        .into_iter()
        .zip(keep)
        .filter_map(|(info, k)| if k { Some(info) } else { None })
        .collect()
}

/// Find the next still-kept non-style neighbor in the given direction.
fn next_kept_neighbor(
    infos: &[TokenInfo],
    keep: &[bool],
    from: usize,
    forward: bool,
) -> Option<usize> {
    if forward {
        let mut j = from + 1;
        while j < infos.len() {
            if keep[j] && !matches!(infos[j].token, Token::Style(_)) {
                return Some(j);
            }
            j += 1;
        }
        None
    } else {
        if from == 0 {
            return None;
        }
        let mut j = from - 1;
        loop {
            if keep[j] && !matches!(infos[j].token, Token::Style(_)) {
                return Some(j);
            }
            if j == 0 {
                return None;
            }
            j -= 1;
        }
    }
}

fn is_git_segment(id: TokenId) -> bool {
    matches!(
        id,
        TokenId::GitStats | TokenId::GitCommitted | TokenId::GitUncommitted | TokenId::GitRebase
    )
}

fn is_pr_segment(id: TokenId) -> bool {
    id == TokenId::PrChecks
}

fn is_whitespace_literal(info: &TokenInfo) -> bool {
    if let Token::Literal(s) = &info.token {
        !s.is_empty() && s.chars().all(|c| c.is_whitespace())
    } else {
        false
    }
}

fn render_common_token(
    info: &TokenInfo,
    ctx: &RowContext,
    user_style: &mut Style,
    spans: &mut Vec<Span<'static>>,
    used_width: &mut usize,
) -> bool {
    match &info.token {
        Token::Literal(s) => {
            spans.push(styled_span(s.clone(), Style::default(), *user_style, ctx));
            *used_width += info.natural_width;
            false
        }
        Token::Fill => false,
        Token::Style(directive) => {
            *user_style = apply_tmux_directives(*user_style, directive, Style::default());
            false
        }
        Token::Field(_) => true,
    }
}

fn render_with_layout(
    ctx: &RowContext,
    left: &[TokenInfo],
    right: &[TokenInfo],
    width: usize,
    mut available: usize,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut used_width = 0;
    let mut first_flex_assigned = false;
    let mut slack: usize = 0;
    // User-supplied tmux-style overlay accumulated as we scan tokens. Stale
    // rows ignore this overlay so state signaling stays authoritative.
    let mut user_style = Style::default();

    // Render left segment
    for info in left {
        if render_common_token(info, ctx, &mut user_style, &mut spans, &mut used_width) {
            let Token::Field(id) = info.token else {
                continue;
            };
            if info.is_flex && !first_flex_assigned {
                // First flex token: truncate if natural exceeds slack, otherwise
                // render at natural width and emit the leftover as a fill-space
                // span between left and right segments (handled after the loop).
                let allocated = available;
                let target_width = info.natural_width.min(allocated);
                let rendered_width =
                    render_field(&mut spans, ctx, id, target_width, allocated, user_style);
                used_width += rendered_width;
                if rendered_width < allocated {
                    slack = allocated - rendered_width;
                }

                first_flex_assigned = true;
                available = 0;
            } else {
                // Non-flex or subsequent flex: render at natural width
                let max_w = width.saturating_sub(used_width);
                used_width += render_field(
                    &mut spans,
                    ctx,
                    id,
                    info.natural_width.min(max_w),
                    max_w,
                    user_style,
                );
            }
        }
    }

    // Slack between left and right segments (where {fill} sat). If a flex
    // token absorbed part of the budget that's already in `slack`; otherwise
    // the entire `available` budget is unused and goes here.
    let fill_width = if first_flex_assigned {
        slack
    } else {
        available
    };
    if fill_width > 0 {
        spans.push(styled_span(
            " ".repeat(fill_width),
            Style::default(),
            user_style,
            ctx,
        ));
        used_width += fill_width;
    }

    // Render right segment
    for info in right {
        if render_common_token(info, ctx, &mut user_style, &mut spans, &mut used_width) {
            let Token::Field(id) = info.token else {
                continue;
            };
            let max_w = width.saturating_sub(used_width);
            used_width += render_field(
                &mut spans,
                ctx,
                id,
                info.natural_width.min(max_w),
                max_w,
                user_style,
            );
        }
    }

    // Fill any remaining width with spaces so the line reaches `width`
    // (important for background coloring in selected rows)
    if used_width < width {
        spans.push(styled_span(
            " ".repeat(width - used_width),
            Style::default(),
            user_style,
            ctx,
        ));
    }

    spans
}

fn render_field(
    spans: &mut Vec<Span<'static>>,
    ctx: &RowContext,
    id: TokenId,
    target_width: usize,
    max_width: usize,
    user_style: Style,
) -> usize {
    if target_width == 0 {
        return 0;
    }

    let rendered_width = if id == TokenId::StatusIcon {
        render_status_icon_spans(spans, ctx, target_width, user_style)
    } else if is_git_segment(id) {
        let (git_spans, git_width) = ctx.git_segment_spans(id, target_width);
        for (text, style) in git_spans {
            spans.push(styled_span(text, style, user_style, ctx));
        }
        git_width
    } else if is_pr_segment(id) {
        let (pr_spans, pr_width) = ctx.pr_check_spans(target_width);
        for (text, style) in pr_spans {
            spans.push(styled_span(text, style, user_style, ctx));
        }
        pr_width
    } else {
        let text = ctx.resolve(id);
        let rendered = if display_width(&text) > target_width {
            truncate_with_ellipsis(&text, target_width)
        } else {
            text
        };
        let rendered_width = display_width(&rendered);
        spans.push(styled_span(
            rendered,
            ctx.intrinsic_style(id),
            user_style,
            ctx,
        ));
        rendered_width
    };

    let target_width = target_width.min(max_width);
    if rendered_width < target_width {
        spans.push(styled_span(
            " ".repeat(target_width - rendered_width),
            Style::default(),
            user_style,
            ctx,
        ));
    }
    rendered_width.max(target_width)
}

fn render_status_icon_spans(
    spans: &mut Vec<Span<'static>>,
    ctx: &RowContext,
    max_width: usize,
    user_style: Style,
) -> usize {
    let mut width = 0;
    for (text, style) in &ctx.status_icon_spans {
        let remaining = max_width.saturating_sub(width);
        if remaining == 0 {
            break;
        }
        let rendered = if display_width(text) > remaining {
            truncate_with_ellipsis(text, remaining)
        } else {
            text.clone()
        };
        width += display_width(&rendered);
        spans.push(styled_span(rendered, *style, user_style, ctx));
    }
    width
}

/// Build a `Span` whose style is the intrinsic base patched by the user
/// overlay, except on stale rows where the user overlay is ignored.
fn styled_span(text: String, base: Style, user_style: Style, ctx: &RowContext) -> Span<'static> {
    if ctx.is_stale {
        Span::styled(text, base)
    } else {
        Span::styled(text, base.patch(user_style))
    }
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1))
        .sum()
}

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
        let char_width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
        if width + char_width + 1 > max_width {
            break;
        }
        out.push(c);
        width += char_width;
    }
    let trimmed = out.trim_end();
    let mut result = trimmed.to_string();
    result.push('\u{2026}');
    result
}

#[cfg(test)]
mod tests {
    use super::super::context::RowContext;
    use super::super::parser::{Token, TokenId};
    use super::*;
    use crate::git::GitStatus;
    use crate::github::{CheckState, CheckSummary, PrSummary};
    use crate::multiplexer::AgentPane;
    use crate::ui::theme::ThemePalette;
    use std::path::PathBuf;

    fn test_palette() -> &'static ThemePalette {
        use crate::config::{ThemeMode, ThemeScheme};
        Box::leak(Box::new(ThemePalette::for_scheme(
            ThemeScheme::Default,
            ThemeMode::Dark,
        )))
    }

    fn test_agent(name: &str) -> AgentPane {
        AgentPane {
            session: "session".to_string(),
            window_name: format!("wm-{}", name),
            pane_id: "%1".to_string(),
            window_id: "@1".to_string(),
            window_index: None,
            path: PathBuf::from(format!("/tmp/{}", name)),
            pane_title: None,
            status: None,
            status_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
        }
    }

    fn tokens(template: &str) -> Vec<Token> {
        super::super::parser::parse_line(template).expect("test template should parse")
    }

    fn make_context(agent: &AgentPane) -> RowContext<'_> {
        // Build a minimal RowContext manually for unit tests
        RowContext {
            agent,
            primary: "feature-auth".to_string(),
            secondary: "myproject".to_string(),
            pane_suffix: String::new(),
            elapsed: "5:23".to_string(),
            status_icon_spans: vec![("💤".to_string(), ratatui::style::Style::default())],
            status_color: ratatui::style::Color::Reset,
            pane_title: None,
            git_status: None,
            pr_summary: None,
            check_summary: None,
            is_stale: false,
            is_active: false,
            is_selected: false,
            palette: test_palette(),
            agent_icon: String::new(),
            agent_icon_color: None,
            agent_label: String::new(),
            idx: 0,
            spinner_frame: 0,
        }
    }

    #[test]
    fn render_line_with_fill() {
        let agent = test_agent("foo");
        let ctx = make_context(&agent);
        let text = render_text(&ctx, &tokens("{primary} {fill} {elapsed}"), 20);
        // primary = "feature-auth" (12 cols), elapsed = "5:23" (4 cols), 2 spaces, fill = 2
        // left gets 20 - 4 - 2 = 14; primary is 12 so padded by 2
        assert!(text.contains("feature-auth"));
        assert!(text.contains("5:23"));
    }

    #[test]
    fn render_line_narrow_truncates_flex() {
        let agent = test_agent("foo");
        let ctx = make_context(&agent);
        let text = render_text(&ctx, &tokens("{primary}{fill}{elapsed}"), 10);
        // elapsed = 4, available = 10 - 4 = 6, primary truncated to ~5 + ellipsis
        assert!(text.contains("5:23"));
        assert!(text.contains('…'));
    }

    #[test]
    fn render_line_drops_right_token_when_narrow() {
        let agent = test_agent("foo");
        let ctx = make_context(&agent);
        // Width of 4: right (5) + left fixed (1) > 4, so elapsed is dropped,
        // then primary is truncated to fit.
        let text = render_text(&ctx, &tokens("{primary} {fill} {elapsed}"), 4);
        // Elapsed should be dropped, primary truncated
        assert!(!text.contains("5:23"));
        assert!(text.contains('…'));
    }

    #[test]
    fn template_line_with_empty_field_is_not_blank() {
        let tokens = vec![
            Token::Field(TokenId::PaneTitle),
            Token::Literal(" ".to_string()),
            Token::Fill,
        ];
        assert!(!is_blank_template_line(&tokens));
    }

    #[test]
    fn template_line_with_literal_content_is_not_blank() {
        let tokens = vec![
            Token::Literal("▌ ".to_string()),
            Token::Field(TokenId::Primary),
        ];
        assert!(!is_blank_template_line(&tokens));
    }

    fn make_git_context<'a>(agent: &'a AgentPane, status: &'a GitStatus) -> RowContext<'a> {
        let mut ctx = make_context(agent);
        ctx.primary.clear();
        ctx.secondary.clear();
        ctx.elapsed.clear();
        ctx.status_icon_spans.clear();
        ctx.git_status = Some(status);
        ctx
    }

    fn render_text(ctx: &RowContext, tokens: &[Token], width: usize) -> String {
        render_line(ctx, tokens, width)
            .iter()
            .map(|s| s.content.clone())
            .collect()
    }

    fn render_text_with_options(
        ctx: &RowContext,
        tokens: &[Token],
        width: usize,
        options: &RenderOptions,
    ) -> String {
        render_line_with_options(ctx, tokens, width, options)
            .iter()
            .map(|s| s.content.clone())
            .collect()
    }

    fn test_pr() -> PrSummary {
        PrSummary {
            number: 123,
            title: "Add PR status".to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            checks: Some(CheckState::Failure {
                passed: 2,
                total: 3,
            }),
            check_meta: None,
            url: None,
        }
    }

    fn test_checks() -> CheckSummary {
        CheckSummary {
            state: CheckState::Failure {
                passed: 2,
                total: 3,
            },
            meta: None,
        }
    }

    fn successful_checks() -> CheckSummary {
        CheckSummary {
            state: CheckState::Success,
            meta: None,
        }
    }

    fn make_pr_context<'a>(
        agent: &'a AgentPane,
        pr: &'a PrSummary,
        checks: &'a CheckSummary,
    ) -> RowContext<'a> {
        let mut ctx = make_context(agent);
        ctx.pr_summary = Some(pr);
        ctx.check_summary = Some(checks);
        ctx
    }

    #[test]
    fn pr_checks_renders_as_single_composite_token() {
        let agent = test_agent("pr");
        let pr = test_pr();
        let checks = test_checks();
        let ctx = make_pr_context(&agent, &pr, &checks);
        let text = render_text(&ctx, &[Token::Field(TokenId::PrChecks)], 20);
        assert!(!text.trim().is_empty(), "{text:?}");
    }

    #[test]
    fn pr_checks_render_without_pr_identity() {
        let agent = test_agent("branch");
        let checks = test_checks();
        let mut ctx = make_context(&agent);
        ctx.check_summary = Some(&checks);

        let text = render_text(&ctx, &tokens("{pr_number} {pr_checks}"), 20);

        assert!(ctx.pr_summary.is_none());
        assert!(text.contains("2/3"), "{text:?}");
    }

    #[test]
    fn main_branch_only_renders_failed_checks() {
        let agent = test_agent("main");
        let status = GitStatus {
            branch: Some("main".to_string()),
            ..Default::default()
        };
        let success = successful_checks();
        let failure = test_checks();
        let mut ctx = make_git_context(&agent, &status);

        ctx.check_summary = Some(&success);
        assert!(
            render_text(&ctx, &tokens("{pr_checks}"), 20)
                .trim()
                .is_empty(),
            "successful main checks should be hidden"
        );

        ctx.check_summary = Some(&failure);
        assert!(
            render_text(&ctx, &tokens("{pr_checks}"), 20).contains("2/3"),
            "failed main checks should be visible"
        );
    }

    #[test]
    fn pr_checks_self_fits_when_narrow() {
        let agent = test_agent("pr");
        let pr = test_pr();
        let checks = test_checks();
        let ctx = make_pr_context(&agent, &pr, &checks);
        let wide = render_text(&ctx, &[Token::Field(TokenId::PrChecks)], 20);
        let narrow = render_text(&ctx, &[Token::Field(TokenId::PrChecks)], 1);
        assert!(wide.contains("2/3"), "{wide:?}");
        assert!(!narrow.contains("2/3"), "{narrow:?}");
        assert_eq!(display_width(narrow.trim()), 1, "{narrow:?}");
    }

    #[test]
    fn empty_pr_tokens_collapse_joiner_space() {
        let agent = test_agent("pr");
        let ctx = make_context(&agent);
        let text = render_text(&ctx, &tokens("{primary} {pr_number} {pr_checks}"), 30);
        assert_eq!(text.trim_end(), "feature-auth");
    }

    #[test]
    fn split_git_tokens_join_with_literal_space() {
        let agent = test_agent("g");
        let status = GitStatus {
            lines_added: 10,
            lines_removed: 5,
            uncommitted_added: 3,
            uncommitted_removed: 1,
            is_dirty: true,
            ..Default::default()
        };
        let ctx = make_git_context(&agent, &status);
        let text = render_text(&ctx, &tokens("{git_committed} {git_uncommitted}"), 40);
        assert!(text.contains("+10 -5"), "missing committed: {:?}", text);
        assert!(text.contains("+3 -1"), "missing uncommitted: {:?}", text);
    }

    #[test]
    fn split_git_tokens_collapse_space_when_one_empty() {
        let agent = test_agent("g");
        // Only uncommitted (committed is zero)
        let status = GitStatus {
            uncommitted_added: 3,
            uncommitted_removed: 1,
            is_dirty: true,
            ..Default::default()
        };
        let ctx = make_git_context(&agent, &status);
        let text = render_text(&ctx, &tokens("{git_committed} {git_uncommitted}"), 40);
        // Should not start with a leading space from the dropped committed token
        assert!(
            !text.starts_with("  "),
            "leading whitespace bleed: {:?}",
            text
        );
        assert!(text.contains("+3 -1"));
    }

    #[test]
    fn split_git_committed_self_fits_when_narrow() {
        let agent = test_agent("g");
        let status = GitStatus {
            lines_added: 1278,
            lines_removed: 400,
            ..Default::default()
        };
        let ctx = make_git_context(&agent, &status);
        // Natural: "+1278 -400" = 10 cols. Width 7 should fit "+1278" (5 cols) variant.
        let text = render_text(&ctx, &tokens("{git_committed}"), 7);
        assert!(text.contains("+1278"), "missing +1278: {:?}", text);
        assert!(!text.contains("-400"), "should drop -400: {:?}", text);
    }

    #[test]
    fn split_git_uncommitted_falls_back_to_icon_only() {
        let agent = test_agent("g");
        let status = GitStatus {
            uncommitted_added: 999,
            uncommitted_removed: 999,
            is_dirty: true,
            ..Default::default()
        };
        let ctx = make_git_context(&agent, &status);
        // Width 1: only icon (1 col) fits.
        let text = render_text(&ctx, &tokens("{git_uncommitted}"), 1);
        assert!(!text.contains('+'), "should drop numbers: {:?}", text);
        assert!(!text.contains('-'), "should drop numbers: {:?}", text);
    }

    #[test]
    fn split_git_token_renders_on_right_side_of_fill() {
        let agent = test_agent("g");
        let status = GitStatus {
            lines_added: 10,
            lines_removed: 5,
            ..Default::default()
        };
        let ctx = make_git_context(&agent, &status);
        let text = render_text(&ctx, &tokens("{fill}{git_committed}"), 40);
        assert!(text.contains("+10 -5"), "missing committed: {:?}", text);
    }

    #[test]
    fn git_token_template_line_is_not_blank_when_git_status_is_empty() {
        let tokens = vec![Token::Field(TokenId::GitCommitted)];
        assert!(!is_blank_template_line(&tokens));
    }

    #[test]
    fn git_token_template_line_is_not_blank_when_git_status_has_content() {
        let tokens = vec![Token::Field(TokenId::GitCommitted)];
        assert!(!is_blank_template_line(&tokens));
    }

    #[test]
    fn trailing_style_after_right_field_does_not_change_drop_behavior() {
        let agent = test_agent("foo");
        let ctx = make_context(&agent);
        let plain_text = render_text(&ctx, &tokens("{primary}{fill}{elapsed}"), 3);
        let styled_text = render_text(&ctx, &tokens("{primary}{fill}{elapsed}#[default]"), 3);
        assert_eq!(plain_text, styled_text);
    }

    #[test]
    fn style_token_is_zero_width_in_layout() {
        let agent = test_agent("foo");
        let ctx = make_context(&agent);
        // Same template with and without a style wrap should render identical
        // visible text at the same width.
        let plain_text = render_text(&ctx, &tokens("{primary}{fill}{elapsed}"), 30);
        let styled_text = render_text(
            &ctx,
            &tokens("#[fg=red]{primary}#[default]{fill}{elapsed}"),
            30,
        );
        assert_eq!(plain_text, styled_text);
    }

    #[test]
    fn user_fg_overrides_intrinsic_on_field() {
        use ratatui::style::Color;
        let agent = test_agent("foo");
        let ctx = make_context(&agent);
        let spans = render_line(&ctx, &tokens("#[fg=red]{primary}#[default]"), 20);
        let primary_span = spans
            .iter()
            .find(|s| s.content.contains("feature-auth"))
            .expect("primary rendered");
        assert_eq!(primary_span.style.fg, Some(Color::Red));
    }

    #[test]
    fn default_resets_user_overlay() {
        use ratatui::style::Color;
        let agent = test_agent("foo");
        let ctx = make_context(&agent);
        // "x" should be red, "y" should fall back to no-overlay (literals are
        // emitted with Style::default() by default).
        let spans = render_line(&ctx, &tokens("#[fg=red]x#[default]y"), 10);
        let x = spans.iter().find(|s| s.content == "x").unwrap();
        let y = spans.iter().find(|s| s.content == "y").unwrap();
        assert_eq!(x.style.fg, Some(Color::Red));
        assert_eq!(y.style.fg, None);
    }

    #[test]
    fn stale_row_ignores_user_overlay() {
        let agent = test_agent("foo");
        let mut ctx = make_context(&agent);
        ctx.is_stale = true;
        let plain = render_line(&ctx, &tokens("{primary}"), 20);
        let styled = render_line(&ctx, &tokens("#[fg=red,bold]{primary}"), 20);
        let plain_primary = plain
            .iter()
            .find(|s| s.content.contains("feature-auth"))
            .unwrap();
        let styled_primary = styled
            .iter()
            .find(|s| s.content.contains("feature-auth"))
            .unwrap();
        assert_eq!(plain_primary.style, styled_primary.style);
    }

    #[test]
    fn unclosed_style_renders_literally_and_counts_for_width() {
        let agent = test_agent("foo");
        let ctx = make_context(&agent);
        // Parser path: an unclosed `#[` becomes a literal. Round-trip the
        // template through the parser to exercise the real flow.
        use super::super::parser::parse_line;
        let tokens = parse_line("#[fg=red x").unwrap();
        let text = render_text(&ctx, &tokens, 20);
        assert!(text.contains("#[fg=red x"));
    }

    #[test]
    fn rendered_width_matches_requested_width_with_styles() {
        let agent = test_agent("foo");
        let ctx = make_context(&agent);
        let spans = render_line(
            &ctx,
            &tokens("#[fg=red]{primary}#[default]{fill}{elapsed}"),
            25,
        );
        let total_width: usize = spans.iter().map(|s| display_width(&s.content)).sum();
        assert_eq!(total_width, 25);
    }

    #[test]
    fn field_min_width_pads_status_icon_before_primary() {
        let agent = test_agent("foo");
        let mut ctx = make_context(&agent);
        ctx.status_icon_spans = vec![("✓".to_string(), Style::default())];
        let options = RenderOptions::default().with_field_min_width(TokenId::StatusIcon, 2);
        let text = render_text_with_options(&ctx, &tokens("{status_icon} {primary}"), 20, &options);
        assert!(text.starts_with("✓  feature-auth"), "{text:?}");
    }

    #[test]
    fn field_min_width_does_not_change_default_render_line() {
        let agent = test_agent("foo");
        let mut ctx = make_context(&agent);
        ctx.status_icon_spans = vec![("✓".to_string(), Style::default())];
        let text = render_text(&ctx, &tokens("{status_icon} {primary}"), 20);
        assert!(text.starts_with("✓ feature-auth"), "{text:?}");
    }

    #[test]
    fn field_min_width_pads_non_status_fields() {
        let agent = test_agent("foo");
        let mut ctx = make_context(&agent);
        ctx.agent_label = "CC".to_string();
        let options = RenderOptions::default().with_field_min_width(TokenId::AgentLabel, 5);
        let text = render_text_with_options(&ctx, &tokens("{agent_label}:{elapsed}"), 20, &options);
        assert!(text.starts_with("CC   :5:23"), "{text:?}");
    }

    #[test]
    fn field_min_width_pads_flex_fields() {
        let agent = test_agent("foo");
        let ctx = make_context(&agent);
        let options = RenderOptions::default().with_field_min_width(TokenId::Primary, 15);
        let text = render_text_with_options(&ctx, &tokens("{primary}:{elapsed}"), 20, &options);
        assert!(text.starts_with("feature-auth   :5:23"), "{text:?}");
    }

    #[test]
    fn field_min_width_respects_remaining_width() {
        let agent = test_agent("foo");
        let mut ctx = make_context(&agent);
        ctx.status_icon_spans = vec![("✓".to_string(), Style::default())];
        let options = RenderOptions::default().with_field_min_width(TokenId::StatusIcon, 2);
        let text = render_text_with_options(&ctx, &tokens("{status_icon}"), 1, &options);
        assert_eq!(display_width(&text), 1, "{text:?}");
    }

    #[test]
    fn styled_fill_padding_inherits_overlay() {
        use ratatui::style::Color;
        let agent = test_agent("foo");
        let ctx = make_context(&agent);
        let spans = render_line(&ctx, &tokens("#[bg=blue]{primary}{fill}{elapsed}"), 30);
        // Find the slack span (whitespace between primary and elapsed).
        let pad = spans
            .iter()
            .find(|s| !s.content.is_empty() && s.content.chars().all(|c| c == ' '))
            .expect("a padding span exists");
        assert_eq!(pad.style.bg, Some(Color::Blue));
    }

    #[test]
    fn collapse_empty_field_skips_intervening_style_token() {
        let agent = test_agent("foo");
        let mut ctx = make_context(&agent);
        ctx.pane_suffix.clear();
        let text = render_text(
            &ctx,
            &tokens("{primary}{pane_suffix}#[fg=red] {elapsed}"),
            30,
        );
        // No double space between primary and elapsed when pane_suffix is
        // empty: the whitespace literal sandwiched after the style token is
        // dropped.
        assert!(
            !text.contains("feature-auth  "),
            "joiner space leaked: {:?}",
            text
        );
    }

    #[test]
    fn blank_template_line_ignores_style_tokens() {
        let tokens = vec![
            Token::Style("fg=red".to_string()),
            Token::Style("default".to_string()),
        ];
        assert!(is_blank_template_line(&tokens));
    }

    #[test]
    fn template_line_with_only_whitespace_literal_is_blank() {
        let tokens = vec![Token::Literal("   ".to_string()), Token::Fill];
        assert!(is_blank_template_line(&tokens));
    }
}
