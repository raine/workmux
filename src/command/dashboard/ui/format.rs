//! Formatting helpers for dashboard UI rendering.

use ratatui::style::{Color, Modifier, Style};

use crate::vcs::VcsStatus;
use crate::github::{CheckState, PrSummary};
use crate::nerdfont;

use super::super::spinner::SPINNER_FRAMES;
use super::theme::ThemePalette;

/// Format git status for the Git column: base branch, diff stats, then indicators
/// Format: "→branch +N -M 󰏫 +X -Y 󰀪 ↑A ↓B"
/// When there are uncommitted changes that differ from total, branch totals are dimmed
pub fn format_git_status(
    status: Option<&VcsStatus>,
    spinner_frame: u8,
    palette: &ThemePalette,
) -> Vec<(String, Style)> {
    let icons = nerdfont::git_icons();

    if let Some(status) = status {
        let mut spans: Vec<(String, Style)> = Vec::new();
        let has_uncommitted =
            status.uncommitted_added > 0 || status.uncommitted_removed > 0 || status.is_dirty;

        // Check if uncommitted equals total (all changes are uncommitted, nothing committed yet)
        let all_uncommitted = status.uncommitted_added == status.lines_added
            && status.uncommitted_removed == status.lines_removed;

        // Base branch (dimmed) - only show if not default (main/master)
        if !status.base_branch.is_empty()
            && status.base_branch != "main"
            && status.base_branch != "master"
        {
            spans.push((
                format!("→{}", status.base_branch),
                Style::default().fg(palette.dimmed),
            ));
        }

        // Always dim branch totals (historical), always bright uncommitted (active work)
        // - Clean: dim branch totals only
        // - All uncommitted: icon + bright uncommitted only
        // - Mixed: dim branch totals + icon + bright uncommitted
        if has_uncommitted && all_uncommitted {
            // All changes are uncommitted - show icon + bright numbers only
            if !spans.is_empty() {
                spans.push((" ".to_string(), Style::default()));
            }
            spans.push((icons.diff.to_string(), Style::default().fg(Color::Magenta)));

            if status.uncommitted_added > 0 {
                spans.push((" ".to_string(), Style::default()));
                spans.push((
                    format!("+{}", status.uncommitted_added),
                    Style::default().fg(Color::Green),
                ));
            }
            if status.uncommitted_removed > 0 {
                spans.push((" ".to_string(), Style::default()));
                spans.push((
                    format!("-{}", status.uncommitted_removed),
                    Style::default().fg(Color::Red),
                ));
            }
        } else {
            // Either clean or mixed - show dim branch totals
            if status.lines_added > 0 {
                if !spans.is_empty() {
                    spans.push((" ".to_string(), Style::default()));
                }
                spans.push((
                    format!("+{}", status.lines_added),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::DIM),
                ));
            }
            if status.lines_removed > 0 {
                if !spans.is_empty() {
                    spans.push((" ".to_string(), Style::default()));
                }
                spans.push((
                    format!("-{}", status.lines_removed),
                    Style::default().fg(Color::Red).add_modifier(Modifier::DIM),
                ));
            }

            // If there are uncommitted changes, show icon + bright uncommitted
            if has_uncommitted {
                if !spans.is_empty() {
                    spans.push((" ".to_string(), Style::default()));
                }
                spans.push((icons.diff.to_string(), Style::default().fg(Color::Magenta)));

                if status.uncommitted_added > 0 {
                    spans.push((" ".to_string(), Style::default()));
                    spans.push((
                        format!("+{}", status.uncommitted_added),
                        Style::default().fg(Color::Green),
                    ));
                }
                if status.uncommitted_removed > 0 {
                    spans.push((" ".to_string(), Style::default()));
                    spans.push((
                        format!("-{}", status.uncommitted_removed),
                        Style::default().fg(Color::Red),
                    ));
                }
            }
        }

        // Conflict indicator
        if status.has_conflict {
            if !spans.is_empty() {
                spans.push((" ".to_string(), Style::default()));
            }
            spans.push((icons.conflict.to_string(), Style::default().fg(Color::Red)));
        }

        // Ahead/behind upstream
        if status.ahead > 0 {
            if !spans.is_empty() {
                spans.push((" ".to_string(), Style::default()));
            }
            spans.push((
                format!("↑{}", status.ahead),
                Style::default().fg(Color::Blue),
            ));
        }
        if status.behind > 0 {
            if !spans.is_empty() {
                spans.push((" ".to_string(), Style::default()));
            }
            spans.push((
                format!("↓{}", status.behind),
                Style::default().fg(Color::Yellow),
            ));
        }

        if spans.is_empty() {
            vec![("-".to_string(), Style::default().fg(palette.dimmed))]
        } else {
            spans
        }
    } else {
        // No status yet - show spinner
        let frame = SPINNER_FRAMES[spinner_frame as usize % SPINNER_FRAMES.len()];
        vec![(frame.to_string(), Style::default().fg(palette.dimmed))]
    }
}

/// Format PR status as styled spans for dashboard display
pub fn format_pr_status(
    pr: Option<&PrSummary>,
    show_check_counts: bool,
    palette: &ThemePalette,
) -> Vec<(String, Style)> {
    match pr {
        Some(pr) => {
            let icons = nerdfont::pr_icons();
            let (icon, color) = if pr.is_draft {
                (icons.draft, palette.dimmed)
            } else {
                match pr.state.as_str() {
                    "OPEN" => (icons.open, Color::Green),
                    "MERGED" => (icons.merged, Color::Magenta),
                    "CLOSED" => (icons.closed, Color::Red),
                    _ => ("?", palette.dimmed),
                }
            };
            let mut spans = vec![
                (format!("#{} ", pr.number), Style::default().fg(color)),
                (icon.to_string(), Style::default().fg(color)),
            ];

            // Append check status if present
            if let Some(ref checks) = pr.checks {
                let check_icons = nerdfont::check_icons();
                let (check_icon, check_color, counts) = match checks {
                    CheckState::Success => (check_icons.success, Color::Green, None),
                    CheckState::Failure { passed, total } => {
                        (check_icons.failure, Color::Red, Some((*passed, *total)))
                    }
                    CheckState::Pending { passed, total } => {
                        (check_icons.pending, Color::Yellow, Some((*passed, *total)))
                    }
                };

                spans.push((" ".to_string(), Style::default()));
                spans.push((check_icon.to_string(), Style::default().fg(check_color)));

                if show_check_counts && let Some((passed, total)) = counts {
                    spans.push((
                        format!(" {}/{}", passed, total),
                        Style::default().fg(check_color),
                    ));
                }
            }

            spans
        }
        None => vec![("-".to_string(), Style::default().fg(palette.dimmed))],
    }
}
