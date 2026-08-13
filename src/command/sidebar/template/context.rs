//! Per-row context: pre-computed token values for a single agent row.

use ratatui::style::{Color, Modifier, Style};

use crate::agent_display::{extract_project_name, extract_worktree_name, sanitize_pane_title};
use crate::agent_identity::AgentKind;
use crate::git::GitStatus;
use crate::github::{CheckSummary, PrSummary};
use crate::multiplexer::agent::resolve_profile_for_display;
use crate::multiplexer::{AgentPane, AgentStatus};
use crate::ui::theme::ThemePalette;

use super::super::app::{ResolvedAgentIcons, SidebarApp};
use super::TokenId;

/// Pre-computed values for every piece of row metadata.
pub struct RowContext<'a> {
    pub agent: &'a AgentPane,
    /// Resolved primary label.
    pub primary: String,
    /// Resolved secondary label.
    pub secondary: String,
    /// Pane suffix like " (1)" when multiple agents share a window.
    pub pane_suffix: String,
    /// Compact elapsed string (e.g. "5:23", "2h", "1d").
    pub elapsed: String,
    /// Status icon parsed into styled spans.
    pub status_icon_spans: Vec<(String, Style)>,
    /// Foreground color extracted from status icon style.
    pub status_color: Color,
    /// Sanitized pane title, filtered against primary/secondary duplicates.
    pub pane_title: Option<String>,
    /// Git status for this agent's path.
    pub git_status: Option<&'a GitStatus>,
    /// PR summary for this agent's path.
    pub pr_summary: Option<&'a PrSummary>,
    /// GitHub check summary for this agent's path.
    pub check_summary: Option<&'a CheckSummary>,
    /// Row flags.
    pub is_stale: bool,
    pub is_active: bool,
    pub is_selected: bool,
    /// Theme palette for style resolution.
    pub palette: &'a ThemePalette,
    /// Pre-resolved agent icon string (empty when no profile matches).
    pub agent_icon: String,
    /// Pre-resolved foreground color for `{agent_icon}`. `None` means fall
    /// through to `palette.text`.
    pub agent_icon_color: Option<Color>,
    /// Pre-resolved agent label string (empty when no profile matches).
    pub agent_label: String,
    /// 0-based sidebar row index. Rendered as 1-based via the `idx` and
    /// `jump_key` tokens.
    pub idx: usize,
    /// Current spinner frame for animated PR checks.
    pub spinner_frame: u8,
}

impl<'a> RowContext<'a> {
    pub fn build(
        app: &'a SidebarApp,
        agent: &'a AgentPane,
        idx: usize,
        pane_suffixes: &[String],
        now_secs: u64,
        selected_idx: Option<usize>,
    ) -> Self {
        let (primary, secondary) = app.resolve_agent_labels(agent);
        let pane_suffix = pane_suffixes[idx].clone();

        let is_sleeping = app.sleeping_pane_ids.contains(&agent.pane_id);
        let is_interrupted = app.interrupted_pane_ids.contains(&agent.pane_id);
        let is_stale = should_dim_agent(
            app.dim_stale,
            is_agent_stale(
                agent.status_ts,
                agent.status,
                now_secs,
                app.stale_threshold_secs,
                is_sleeping,
                is_interrupted,
            ),
        );
        let is_active = app.host_agent_idx == Some(idx);
        let is_selected = selected_idx == Some(idx);

        let (status_icon_spans, status_icon_style) =
            super::super::ui::status_icon_and_style(app, agent.status, is_stale);
        let status_color = status_icon_style.fg.unwrap_or(Color::Reset);

        let elapsed = agent
            .status_ts
            .map(|ts| format_compact_elapsed(now_secs.saturating_sub(ts)))
            .unwrap_or_default();

        let pane_title = build_pane_title(agent, &primary, &secondary, app.window_prefix());
        let git_status = app.git_statuses.get(&agent.path);
        let pr_summary = app.pr_statuses.get(&agent.path);
        let check_summary = app.check_statuses.get(&agent.path);
        let kind =
            effective_agent_kind(agent.agent_kind.as_deref(), agent.agent_command.as_deref());
        let agent_icon = resolve_agent_icon(kind, &app.agent_icons);
        let agent_icon_color = resolve_agent_icon_color(kind, &app.agent_icons);
        let agent_label = resolve_agent_label(kind);

        Self {
            agent,
            primary,
            secondary,
            pane_suffix,
            elapsed,
            status_icon_spans,
            status_color,
            pane_title,
            git_status,
            pr_summary,
            check_summary,
            is_stale,
            is_active,
            is_selected,
            palette: &app.palette,
            agent_icon,
            agent_icon_color,
            agent_label,
            idx,
            spinner_frame: app.spinner_frame,
        }
    }

    /// Resolve a token to its display string.
    pub fn resolve(&self, token: TokenId) -> String {
        match token {
            TokenId::Primary => self.primary.clone(),
            TokenId::Secondary => self.secondary.clone(),
            TokenId::Worktree => self.worktree_name(),
            TokenId::Project => self.project_name(),
            TokenId::Session => self.agent.session.clone(),
            TokenId::Window => self.agent.window_name.clone(),
            TokenId::WindowIndex => self
                .agent
                .window_index
                .map(|index| index.to_string())
                .unwrap_or_default(),
            TokenId::PaneTitle => self.pane_title.clone().unwrap_or_default(),
            TokenId::AgentLabel => self.agent_label.clone(),
            TokenId::StatusIcon => self
                .status_icon_spans
                .iter()
                .map(|(t, _)| t.clone())
                .collect(),
            TokenId::AgentIcon => self.agent_icon.clone(),
            TokenId::PaneSuffix => self.pane_suffix.clone(),
            TokenId::Elapsed => self.elapsed.clone(),
            TokenId::GitStats
            | TokenId::GitCommitted
            | TokenId::GitUncommitted
            | TokenId::GitRebase
            | TokenId::PrChecks => {
                // Span-rendered tokens: empty string at resolution time;
                // layout engine calls segment span helpers for rendering.
                String::new()
            }
            TokenId::GitAhead => self
                .git_status
                .filter(|s| s.has_upstream && s.ahead > 0)
                .map(|s| format!("\u{2191}{}", s.ahead))
                .unwrap_or_default(),
            TokenId::GitBehind => self
                .git_status
                .filter(|s| s.has_upstream && s.behind > 0)
                .map(|s| format!("\u{2193}{}", s.behind))
                .unwrap_or_default(),
            TokenId::GitDirty => match self.git_status {
                Some(s) if s.is_dirty => crate::nerdfont::git_icons().diff.to_string(),
                _ => String::new(),
            },
            TokenId::GitConflict => match self.git_status {
                Some(s) if s.has_conflict => crate::nerdfont::git_icons().conflict.to_string(),
                _ => String::new(),
            },
            TokenId::GitBranch => self
                .git_status
                .and_then(|s| s.branch.clone())
                .unwrap_or_default(),
            TokenId::PrNumber => self
                .pr_summary
                .map(|pr| format!("#{}", pr.number))
                .unwrap_or_default(),
            TokenId::StatusLabel => match self.agent.status {
                Some(AgentStatus::Working) => "Working".to_string(),
                Some(AgentStatus::Waiting) => "Waiting".to_string(),
                Some(AgentStatus::Done) => "Done".to_string(),
                None => String::new(),
            },
            TokenId::Idx => (self.idx + 1).to_string(),
            TokenId::JumpKey => {
                if self.idx < 9 {
                    format!("M-{}", self.idx + 1)
                } else {
                    String::new()
                }
            }
        }
    }

    /// Natural display width of a token's resolved text.
    pub fn natural_width(&self, token: TokenId) -> usize {
        match token {
            TokenId::StatusIcon => self
                .status_icon_spans
                .iter()
                .map(|(t, _)| display_width(t))
                .sum(),
            TokenId::AgentIcon => display_width(&self.agent_icon),
            TokenId::AgentLabel => display_width(&self.agent_label),
            TokenId::GitStats
            | TokenId::GitCommitted
            | TokenId::GitUncommitted
            | TokenId::GitRebase => {
                let (_, width) = self.git_segment_spans(token, usize::MAX);
                width
            }
            TokenId::PrChecks => {
                let (_, width) = self.pr_check_spans(usize::MAX);
                width
            }
            other => display_width(&self.resolve(other)),
        }
    }

    /// Render git stats with a given allocated width, returning styled spans and actual width.
    pub fn git_stats_spans(&self, allocated_width: usize) -> (Vec<(String, Style)>, usize) {
        match self.git_status {
            Some(status) => super::super::ui::format_sidebar_git_stats(
                Some(status),
                self.palette,
                self.is_stale,
                allocated_width,
            ),
            None => (Vec::new(), 0),
        }
    }

    /// Render one git segment token (composite or split) with self-fitting.
    pub fn git_segment_spans(
        &self,
        token: TokenId,
        allocated_width: usize,
    ) -> (Vec<(String, Style)>, usize) {
        match token {
            TokenId::GitStats => self.git_stats_spans(allocated_width),
            TokenId::GitCommitted => super::super::ui::format_committed_spans(
                self.git_status,
                self.palette,
                self.is_stale,
                allocated_width,
            ),
            TokenId::GitUncommitted => super::super::ui::format_uncommitted_spans(
                self.git_status,
                self.palette,
                self.is_stale,
                allocated_width,
            ),
            TokenId::GitRebase => super::super::ui::format_rebase_spans(
                self.git_status,
                self.palette,
                self.is_stale,
                allocated_width,
            ),
            _ => (Vec::new(), 0),
        }
    }

    /// Render PR checks with a given allocated width, returning styled spans and actual width.
    pub fn pr_check_spans(&self, allocated_width: usize) -> (Vec<(String, Style)>, usize) {
        let branch = self.git_status.and_then(|status| status.branch.as_deref());
        let check_summary = self.check_summary.filter(|summary| {
            branch.is_none_or(|branch| summary.should_display_for_branch(branch))
        });
        super::super::ui::format_sidebar_check_status(
            check_summary,
            self.palette,
            self.is_stale,
            self.spinner_frame,
            allocated_width,
        )
    }

    /// Intrinsic style for a token (before state/selection post-pass).
    pub fn intrinsic_style(&self, token: TokenId) -> Style {
        if self.is_stale {
            return Style::default()
                .fg(self.palette.dimmed)
                .add_modifier(Modifier::DIM);
        }
        match token {
            TokenId::Primary if self.is_active => Style::default()
                .fg(self.palette.current_worktree_fg)
                .add_modifier(Modifier::BOLD),
            TokenId::Primary => Style::default().fg(self.palette.text),
            TokenId::Secondary => Style::default()
                .fg(self.palette.text)
                .add_modifier(Modifier::DIM),
            TokenId::PaneTitle => Style::default().fg(self.palette.dimmed),
            TokenId::PaneSuffix => Style::default().fg(self.palette.dimmed),
            TokenId::Elapsed => Style::default()
                .fg(self.palette.text)
                .add_modifier(Modifier::DIM),
            TokenId::AgentLabel => Style::default().fg(self.palette.text),
            TokenId::GitAhead => Style::default().fg(self.palette.success),
            TokenId::GitBehind => Style::default().fg(self.palette.danger),
            TokenId::GitDirty => Style::default().fg(self.palette.warning),
            TokenId::GitConflict => Style::default().fg(self.palette.danger),
            TokenId::GitBranch => Style::default().fg(self.palette.text),
            TokenId::PrNumber => self
                .pr_summary
                .map(|pr| crate::ui::pr_status::pr_state_icon_color(pr, self.palette).1)
                .map(|color| Style::default().fg(color))
                .unwrap_or_else(|| Style::default().fg(self.palette.text)),
            TokenId::StatusLabel => Style::default().fg(self.status_color),
            TokenId::Idx => Style::default().fg(self.palette.dimmed),
            TokenId::JumpKey => Style::default().fg(self.palette.dimmed),
            TokenId::WindowIndex => Style::default().fg(self.palette.dimmed),
            TokenId::AgentIcon => {
                let fg = self.agent_icon_color.unwrap_or(self.palette.text);
                Style::default().fg(fg)
            }
            _ => Style::default().fg(self.palette.text),
        }
    }

    fn worktree_name(&self) -> String {
        let (wt, _) = extract_worktree_name(
            &self.agent.session,
            &self.agent.window_name,
            "",
            &self.agent.path,
        );
        wt
    }

    fn project_name(&self) -> String {
        extract_project_name(&self.agent.path)
    }
}

fn resolve_agent_label(kind: Option<AgentKind>) -> String {
    match kind {
        Some(k) => k.default_label().to_string(),
        None => String::new(),
    }
}

fn resolve_agent_icon(kind: Option<AgentKind>, icons: &ResolvedAgentIcons) -> String {
    let Some(kind) = kind else {
        return String::new();
    };
    if let Some(icon) = icons.icons.get(kind.as_str()) {
        return icon.clone();
    }
    kind.default_icon().to_string()
}

fn resolve_agent_icon_color(kind: Option<AgentKind>, icons: &ResolvedAgentIcons) -> Option<Color> {
    let kind = kind?;
    match icons.colors.get(kind.as_str()) {
        Some(Some(c)) => Some(*c),
        Some(None) => None, // explicit opt-out via `color: ''`
        None => kind.default_color(),
    }
}

/// Prefer the cached classification; fall back to today's stem-based resolver.
///
/// A malformed, hand-edited, or future-version state file with an unknown
/// `agent_kind` falls through to the command-based resolver instead of
/// shadowing a perfectly good `agent_command` with a meaningless icon/label.
fn effective_agent_kind(
    agent_kind: Option<&str>,
    agent_command: Option<&str>,
) -> Option<AgentKind> {
    if let Some(kind) = agent_kind.and_then(AgentKind::from_str) {
        return Some(kind);
    }
    AgentKind::from_str(resolve_profile_for_display(agent_command).name())
}

fn build_pane_title(
    agent: &AgentPane,
    primary: &str,
    secondary: &str,
    window_prefix: &str,
) -> Option<String> {
    let title_worktree = extract_worktree_name(
        &agent.session,
        &agent.window_name,
        window_prefix,
        &agent.path,
    )
    .0;
    let title_project = extract_project_name(&agent.path);
    sanitize_pane_title(agent.pane_title.as_deref(), &title_worktree, &title_project)
        .filter(|t| *t != primary && *t != secondary)
        .filter(|t| !is_hostname_title(t))
        .map(|s| s.to_string())
}

fn is_hostname_title(title: &str) -> bool {
    is_hostname_title_with(title, std::env::var("HOSTNAME").ok().as_deref())
}

fn is_hostname_title_with(title: &str, hostname: Option<&str>) -> bool {
    hostname.is_some_and(|hostname| !hostname.is_empty() && title == hostname)
}

fn is_agent_stale(
    status_ts: Option<u64>,
    status: Option<AgentStatus>,
    now_secs: u64,
    stale_threshold_secs: u64,
    is_sleeping: bool,
    is_interrupted: bool,
) -> bool {
    if is_sleeping {
        return true;
    }

    if !is_interrupted
        && matches!(
            status,
            Some(AgentStatus::Working) | Some(AgentStatus::Waiting)
        )
    {
        return false;
    }

    status_ts
        .map(|ts| now_secs.saturating_sub(ts) > stale_threshold_secs)
        .unwrap_or(true)
}

fn should_dim_agent(dim_stale: bool, is_stale: bool) -> bool {
    dim_stale && is_stale
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1))
        .sum()
}

fn format_compact_elapsed(secs: u64) -> String {
    if secs < 3600 {
        format!("{}:{:02}", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(agent_kind: Option<&str>, agent_command: Option<&str>) -> Option<AgentKind> {
        effective_agent_kind(agent_kind, agent_command)
    }

    #[test]
    fn missing_status_timestamp_is_stale() {
        assert!(is_agent_stale(None, None, 100, 60, false, false));
    }

    #[test]
    fn active_statuses_are_not_stale_without_timestamp() {
        assert!(!is_agent_stale(
            None,
            Some(AgentStatus::Working),
            100,
            60,
            false,
            false
        ));
        assert!(!is_agent_stale(
            None,
            Some(AgentStatus::Waiting),
            100,
            60,
            false,
            false
        ));
    }

    #[test]
    fn stale_rendering_can_be_disabled() {
        assert!(should_dim_agent(true, true));
        assert!(!should_dim_agent(false, true));
        assert!(!should_dim_agent(true, false));
    }

    #[test]
    fn sleeping_agent_is_stale_even_with_active_status() {
        assert!(is_agent_stale(
            Some(100),
            Some(AgentStatus::Working),
            100,
            60,
            true,
            false
        ));
    }

    #[test]
    fn cached_kind_resolves_label_without_command() {
        // Command is a version string the stem-based resolver can't classify;
        // the cached kind must drive label/icon.
        assert_eq!(
            resolve_agent_label(kind(Some("claude"), Some("2.1.118"))),
            "Claude"
        );
    }

    #[test]
    fn cached_kind_renders_friendly_kiro_label() {
        assert_eq!(resolve_agent_label(kind(Some("kiro-cli"), None)), "Kiro");
    }

    #[test]
    fn cached_kind_renders_friendly_opencode_label() {
        assert_eq!(
            resolve_agent_label(kind(Some("opencode"), None)),
            "OpenCode"
        );
    }

    #[test]
    fn unknown_cached_kind_falls_back_to_command() {
        // Defensive: malformed cache must not shadow a valid agent_command.
        let icons = ResolvedAgentIcons::default();
        assert_eq!(
            resolve_agent_label(kind(Some("not-a-profile"), Some("claude"))),
            "Claude"
        );
        assert_eq!(
            resolve_agent_icon(kind(Some("not-a-profile"), Some("claude")), &icons),
            "CC"
        );
    }

    #[test]
    fn no_cache_falls_back_to_today_behavior() {
        let icons = ResolvedAgentIcons::default();
        assert_eq!(resolve_agent_label(kind(None, Some("gemini"))), "Gemini");
        assert_eq!(resolve_agent_icon(kind(None, Some("gemini")), &icons), "G");
    }

    #[test]
    fn antigravity_uses_curved_default_icon() {
        let icons = ResolvedAgentIcons::default();
        assert_eq!(resolve_agent_label(kind(None, Some("agy"))), "Antigravity");
        assert_eq!(resolve_agent_icon(kind(None, Some("agy")), &icons), "⋂");
    }

    #[test]
    fn hostname_pane_title_is_noise() {
        assert!(is_hostname_title_with("framework", Some("framework")));
        assert!(!is_hostname_title_with("framework", Some("other")));
        assert!(!is_hostname_title_with("framework", None));
        assert!(!is_hostname_title_with("framework", Some("")));
    }

    #[test]
    fn custom_icon_override_still_honored_with_cached_kind() {
        let mut icons = ResolvedAgentIcons::default();
        icons.icons.insert("claude".to_string(), "X".to_string());
        assert_eq!(
            resolve_agent_icon(kind(Some("claude"), Some("2.1.118")), &icons),
            "X"
        );
    }

    use crate::command::sidebar::app::TemplateError;
    use crate::config::{ThemeMode, ThemeScheme};
    use crate::github::{CheckState, PrSummary};
    use crate::multiplexer::AgentPane;
    use std::path::PathBuf;

    fn test_palette() -> &'static ThemePalette {
        Box::leak(Box::new(ThemePalette::for_scheme(
            ThemeScheme::Default,
            ThemeMode::Dark,
        )))
    }

    fn test_agent() -> AgentPane {
        AgentPane {
            session: "s".to_string(),
            window_name: "w".to_string(),
            pane_id: "%1".to_string(),
            window_id: "@1".to_string(),
            window_index: None,
            path: PathBuf::from("/tmp/x"),
            pane_title: None,
            status: None,
            status_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
        }
    }

    #[test]
    fn row_context_keeps_stale_done_agent_colored_when_dimming_is_disabled() {
        let mut app = SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        });
        app.dim_stale = false;

        let mut agent = test_agent();
        agent.status = Some(AgentStatus::Done);
        agent.status_ts = Some(1);
        let pane_suffixes = vec![String::new()];
        let ctx = RowContext::build(&app, &agent, 0, &pane_suffixes, 7200, None);

        assert!(!ctx.is_stale);
        assert_eq!(ctx.status_color, app.palette.success);
    }

    fn make_context<'a>(
        agent: &'a AgentPane,
        git: Option<&'a GitStatus>,
        idx: usize,
    ) -> RowContext<'a> {
        make_context_with_pr(agent, git, None, None, idx)
    }

    fn make_context_with_pr<'a>(
        agent: &'a AgentPane,
        git: Option<&'a GitStatus>,
        pr: Option<&'a PrSummary>,
        checks: Option<&'a CheckSummary>,
        idx: usize,
    ) -> RowContext<'a> {
        RowContext {
            agent,
            primary: String::new(),
            secondary: String::new(),
            pane_suffix: String::new(),
            elapsed: String::new(),
            status_icon_spans: vec![],
            status_color: Color::Reset,
            pane_title: None,
            git_status: git,
            pr_summary: pr,
            check_summary: checks,
            is_stale: false,
            is_active: false,
            is_selected: false,
            palette: test_palette(),
            agent_icon: String::new(),
            agent_icon_color: None,
            agent_label: String::new(),
            idx,
            spinner_frame: 0,
        }
    }

    #[test]
    fn interrupted_active_agent_can_become_stale() {
        assert!(is_agent_stale(
            Some(100),
            Some(AgentStatus::Working),
            500,
            60,
            false,
            true
        ));
        assert!(!is_agent_stale(
            Some(480),
            Some(AgentStatus::Working),
            500,
            60,
            false,
            true
        ));
    }

    #[test]
    fn resolve_idx_is_one_based() {
        let agent = test_agent();
        let ctx0 = make_context(&agent, None, 0);
        let ctx9 = make_context(&agent, None, 9);
        assert_eq!(ctx0.resolve(TokenId::Idx), "1");
        assert_eq!(ctx9.resolve(TokenId::Idx), "10");
    }

    #[test]
    fn resolve_window_index_empty_when_unset() {
        let mut agent = test_agent();
        assert_eq!(
            make_context(&agent, None, 0).resolve(TokenId::WindowIndex),
            ""
        );
        agent.window_index = Some(3);
        assert_eq!(
            make_context(&agent, None, 0).resolve(TokenId::WindowIndex),
            "3"
        );
    }

    #[test]
    fn resolve_jump_key_caps_at_nine() {
        let agent = test_agent();
        assert_eq!(
            make_context(&agent, None, 0).resolve(TokenId::JumpKey),
            "M-1"
        );
        assert_eq!(
            make_context(&agent, None, 8).resolve(TokenId::JumpKey),
            "M-9"
        );
        assert_eq!(make_context(&agent, None, 9).resolve(TokenId::JumpKey), "");
    }

    #[test]
    fn resolve_status_label_capitalised() {
        let mut agent = test_agent();
        agent.status = Some(AgentStatus::Working);
        assert_eq!(
            make_context(&agent, None, 0).resolve(TokenId::StatusLabel),
            "Working"
        );
        agent.status = Some(AgentStatus::Waiting);
        assert_eq!(
            make_context(&agent, None, 0).resolve(TokenId::StatusLabel),
            "Waiting"
        );
        agent.status = Some(AgentStatus::Done);
        assert_eq!(
            make_context(&agent, None, 0).resolve(TokenId::StatusLabel),
            "Done"
        );
        agent.status = None;
        assert_eq!(
            make_context(&agent, None, 0).resolve(TokenId::StatusLabel),
            ""
        );
    }

    #[test]
    fn resolve_git_ahead_behind() {
        let agent = test_agent();
        // No git status -> empty.
        assert_eq!(make_context(&agent, None, 0).resolve(TokenId::GitAhead), "");
        assert_eq!(
            make_context(&agent, None, 0).resolve(TokenId::GitBehind),
            ""
        );

        let status = GitStatus {
            has_upstream: true,
            ahead: 3,
            behind: 0,
            ..Default::default()
        };
        let ctx = make_context(&agent, Some(&status), 0);
        assert_eq!(ctx.resolve(TokenId::GitAhead), "\u{2191}3");
        assert_eq!(ctx.resolve(TokenId::GitBehind), "");

        let status = GitStatus {
            has_upstream: true,
            ahead: 0,
            behind: 5,
            ..Default::default()
        };
        let ctx = make_context(&agent, Some(&status), 0);
        assert_eq!(ctx.resolve(TokenId::GitAhead), "");
        assert_eq!(ctx.resolve(TokenId::GitBehind), "\u{2193}5");

        // No upstream: even nonzero counts collapse to empty.
        let status = GitStatus {
            has_upstream: false,
            ahead: 3,
            behind: 5,
            ..Default::default()
        };
        let ctx = make_context(&agent, Some(&status), 0);
        assert_eq!(ctx.resolve(TokenId::GitAhead), "");
        assert_eq!(ctx.resolve(TokenId::GitBehind), "");
    }

    #[test]
    fn resolve_git_dirty_conflict_glyphs() {
        let agent = test_agent();
        let icons = crate::nerdfont::git_icons();

        let clean = GitStatus::default();
        let ctx = make_context(&agent, Some(&clean), 0);
        assert_eq!(ctx.resolve(TokenId::GitDirty), "");
        assert_eq!(ctx.resolve(TokenId::GitConflict), "");

        let dirty = GitStatus {
            is_dirty: true,
            has_conflict: true,
            ..Default::default()
        };
        let ctx = make_context(&agent, Some(&dirty), 0);
        assert_eq!(ctx.resolve(TokenId::GitDirty), icons.diff);
        assert_eq!(ctx.resolve(TokenId::GitConflict), icons.conflict);
    }

    #[test]
    fn resolve_git_branch() {
        let agent = test_agent();
        let status = GitStatus {
            branch: Some("feature/x".to_string()),
            ..Default::default()
        };
        assert_eq!(
            make_context(&agent, Some(&status), 0).resolve(TokenId::GitBranch),
            "feature/x"
        );
        // Detached HEAD -> empty.
        let detached = GitStatus::default();
        assert_eq!(
            make_context(&agent, Some(&detached), 0).resolve(TokenId::GitBranch),
            ""
        );
        // No git status -> empty.
        assert_eq!(
            make_context(&agent, None, 0).resolve(TokenId::GitBranch),
            ""
        );
    }

    #[test]
    fn resolve_pr_number_and_status() {
        let agent = test_agent();
        let pr = PrSummary {
            number: 123,
            title: "Add thing".to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            checks: Some(CheckState::Success),
            check_meta: None,
            url: None,
        };
        let checks = CheckSummary {
            state: CheckState::Success,
            meta: None,
        };
        let ctx = make_context_with_pr(&agent, None, Some(&pr), Some(&checks), 0);
        assert_eq!(ctx.resolve(TokenId::PrNumber), "#123");
        assert_eq!(ctx.resolve(TokenId::PrChecks), "");
        assert!(ctx.natural_width(TokenId::PrChecks) > 0);
    }

    #[test]
    fn resolve_pr_tokens_empty_without_pr() {
        let agent = test_agent();
        let ctx = make_context(&agent, None, 0);
        assert_eq!(ctx.resolve(TokenId::PrNumber), "");
        assert_eq!(ctx.natural_width(TokenId::PrChecks), 0);
    }

    #[test]
    fn intrinsic_style_assigns_palette_colors() {
        let agent = test_agent();
        let ctx = make_context(&agent, None, 0);
        let palette = ctx.palette;
        assert_eq!(
            ctx.intrinsic_style(TokenId::GitAhead).fg,
            Some(palette.success)
        );
        assert_eq!(
            ctx.intrinsic_style(TokenId::GitBehind).fg,
            Some(palette.danger)
        );
        assert_eq!(
            ctx.intrinsic_style(TokenId::GitConflict).fg,
            Some(palette.danger)
        );
        assert_eq!(
            ctx.intrinsic_style(TokenId::GitDirty).fg,
            Some(palette.warning)
        );
        assert_eq!(ctx.intrinsic_style(TokenId::Idx).fg, Some(palette.dimmed));
        assert_eq!(
            ctx.intrinsic_style(TokenId::JumpKey).fg,
            Some(palette.dimmed)
        );
    }

    #[test]
    fn intrinsic_style_dims_when_stale() {
        let agent = test_agent();
        let mut ctx = make_context(&agent, None, 0);
        ctx.is_stale = true;
        let palette = ctx.palette;
        assert_eq!(
            ctx.intrinsic_style(TokenId::GitAhead).fg,
            Some(palette.dimmed)
        );
        assert_eq!(
            ctx.intrinsic_style(TokenId::StatusLabel).fg,
            Some(palette.dimmed)
        );
    }

    #[test]
    fn default_color_for_claude_is_brand_orange() {
        let icons = ResolvedAgentIcons::default();
        assert_eq!(
            resolve_agent_icon_color(kind(Some("claude"), None), &icons),
            Some(Color::Rgb(0xd9, 0x77, 0x57))
        );
    }

    #[test]
    fn user_color_override_wins_over_default() {
        let mut icons = ResolvedAgentIcons::default();
        icons
            .colors
            .insert("claude".to_string(), Some(Color::Rgb(0, 255, 0)));
        assert_eq!(
            resolve_agent_icon_color(kind(Some("claude"), None), &icons),
            Some(Color::Rgb(0, 255, 0))
        );
    }

    #[test]
    fn explicit_empty_color_disables_default() {
        let mut icons = ResolvedAgentIcons::default();
        icons.colors.insert("claude".to_string(), None);
        assert_eq!(
            resolve_agent_icon_color(kind(Some("claude"), None), &icons),
            None
        );
    }

    #[test]
    fn unknown_agent_has_no_color() {
        let icons = ResolvedAgentIcons::default();
        assert_eq!(resolve_agent_icon_color(None, &icons), None);
    }
}
