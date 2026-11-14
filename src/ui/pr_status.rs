use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::github::{CheckMeta, CheckState, CheckSummary, PrSummary};
use crate::nerdfont;
use crate::ui::theme::ThemePalette;

pub const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Debug, Clone, Copy)]
pub struct PrStatusOptions {
    pub include_number: bool,
    pub show_check_counts: bool,
    pub none_placeholder: Option<&'static str>,
    pub is_stale: bool,
}

pub fn format_github_status(
    pr: Option<&PrSummary>,
    check_summary: Option<&CheckSummary>,
    options: PrStatusOptions,
    spinner_frame: u8,
    palette: &ThemePalette,
) -> Vec<(String, Style)> {
    let check_state = check_summary
        .map(|summary| &summary.state)
        .or_else(|| pr.and_then(|pr| pr.checks.as_ref()));
    let check_meta = check_summary
        .and_then(|summary| summary.meta.as_ref())
        .or_else(|| pr.and_then(|pr| pr.check_meta.as_ref()));

    if pr.is_none() && check_state.is_none() {
        return options
            .none_placeholder
            .map(|placeholder| vec![(placeholder.to_string(), Style::default().fg(palette.dimmed))])
            .unwrap_or_default();
    }

    let mut spans = Vec::new();
    if let Some(pr) = pr {
        let (icon, color) = pr_state_icon_color(pr, palette);
        let color = if options.is_stale {
            palette.dimmed
        } else {
            color
        };
        if options.include_number {
            spans.push((format!("#{} ", pr.number), Style::default().fg(color)));
        }
        spans.push((icon.to_string(), Style::default().fg(color)));
    }

    if let Some(checks) = check_state {
        let check_icons = nerdfont::check_icons();
        let (check_icon, check_color, counts) = match checks {
            CheckState::Success => (check_icons.success.to_string(), palette.success, None),
            CheckState::Failure { passed, total } => (
                check_icons.failure.to_string(),
                palette.danger,
                Some((*passed, *total)),
            ),
            CheckState::Pending { passed, total } => {
                let frame = SPINNER_FRAMES[spinner_frame as usize % SPINNER_FRAMES.len()];
                (frame.to_string(), palette.accent, Some((*passed, *total)))
            }
        };
        let check_color = if options.is_stale {
            palette.dimmed
        } else {
            check_color
        };

        if !spans.is_empty() {
            spans.push((" ".to_string(), Style::default()));
        }
        spans.push((check_icon, Style::default().fg(check_color)));

        if options.show_check_counts
            && let Some((passed, total)) = counts
        {
            spans.push((
                format!(" {}/{}", passed, total),
                Style::default().fg(check_color),
            ));
        }

        if let Some(time_str) = format_check_elapsed(checks, check_meta) {
            spans.push((
                format!(" {}", time_str),
                Style::default().fg(palette.dimmed),
            ));
        }
    }

    if options.is_stale {
        for (_, style) in &mut spans {
            *style = style.fg(palette.dimmed).add_modifier(Modifier::DIM);
        }
    }

    spans
}

pub fn pr_state_icon_color(
    pr: &PrSummary,
    palette: &ThemePalette,
) -> (&'static str, ratatui::style::Color) {
    let icons = nerdfont::pr_icons();
    if pr.is_draft {
        (icons.draft, palette.dimmed)
    } else {
        match pr.state.as_str() {
            "OPEN" => (icons.open, palette.success),
            "MERGED" => (icons.merged, palette.accent),
            "CLOSED" => (icons.closed, palette.danger),
            _ => ("?", palette.dimmed),
        }
    }
}

pub fn format_pr_details(
    pr: &PrSummary,
    spinner_frame: u8,
    palette: &ThemePalette,
) -> Vec<Span<'static>> {
    let Some(checks) = &pr.checks else {
        return vec![];
    };

    match checks {
        CheckState::Failure { .. } => {
            let Some(meta) = &pr.check_meta else {
                return vec![];
            };
            let Some(name) = &meta.failing_name else {
                return vec![];
            };
            let icon = nerdfont::check_icons().failure;
            vec![Span::styled(
                format!("{} {}", icon, name),
                Style::default().fg(palette.danger),
            )]
        }
        CheckState::Pending { .. } => match format_check_elapsed(checks, pr.check_meta.as_ref()) {
            Some(time_str) => {
                let frame = SPINNER_FRAMES[spinner_frame as usize % SPINNER_FRAMES.len()];
                vec![Span::styled(
                    format!("{} {}", frame, time_str),
                    Style::default().fg(palette.dimmed),
                )]
            }
            None => vec![],
        },
        CheckState::Success => vec![],
    }
}

fn format_check_elapsed(checks: &CheckState, meta: Option<&CheckMeta>) -> Option<String> {
    let meta = meta?;
    match checks {
        CheckState::Pending { .. } => {
            let start = meta.started_at?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            Some(format_compact_duration(now.saturating_sub(start)))
        }
        _ => None,
    }
}

fn format_compact_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> ThemePalette {
        ThemePalette::from_config(
            &crate::config::ThemeConfig::default(),
            crate::config::ThemeMode::Dark,
        )
    }

    fn options() -> PrStatusOptions {
        PrStatusOptions {
            include_number: true,
            show_check_counts: true,
            none_placeholder: Some("-"),
            is_stale: false,
        }
    }

    #[test]
    fn branch_checks_render_without_pr() {
        let checks = CheckSummary {
            state: CheckState::Failure {
                passed: 2,
                total: 3,
            },
            meta: None,
        };

        let spans = format_github_status(None, Some(&checks), options(), 0, &palette());
        let text: String = spans.into_iter().map(|(text, _)| text).collect();

        assert!(text.contains("2/3"), "{text:?}");
        assert!(!text.contains('#'), "{text:?}");
        assert_ne!(text, "-");
    }

    #[test]
    fn missing_pr_and_checks_use_placeholder() {
        let spans = format_github_status(None, None, options(), 0, &palette());
        let text: String = spans.into_iter().map(|(text, _)| text).collect();

        assert_eq!(text, "-");
    }
}
