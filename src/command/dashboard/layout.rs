//! Shared dashboard geometry for rendering and mouse hit-testing.

use ratatui::layout::{Constraint, Layout, Rect};

use crate::config::DashboardLayout;

pub(super) fn dashboard_outer_areas(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::vertical([
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .split(area)
}

/// Return the table and optional preview rectangles, independent of tab.
pub(super) fn dashboard_content_areas(
    content: Rect,
    supports_preview: bool,
    preview_size: u8,
    layout: DashboardLayout,
) -> (Rect, Option<Rect>) {
    if !supports_preview {
        return (content, None);
    }
    let preview_size = u16::from(preview_size);
    let table_size = 100u16.saturating_sub(preview_size);
    match layout {
        DashboardLayout::Stacked => {
            let chunks =
                Layout::vertical([Constraint::Fill(table_size), Constraint::Fill(preview_size)])
                    .split(content);
            (chunks[0], Some(chunks[1]))
        }
        DashboardLayout::SideBySide => {
            let chunks =
                Layout::horizontal([Constraint::Fill(preview_size), Constraint::Fill(table_size)])
                    .split(content);
            (chunks[1], Some(chunks[0]))
        }
    }
}

/// Width of the right-pinned `? Help` hint in the footer.
pub(super) const FOOTER_HELP_WIDTH: u16 = 7;

/// Width of the remote servers footer tip: `New: remote servers  g Guide  x Hide │ `.
pub(super) const REMOTE_TIP_WIDTH: u16 = 39;
/// Tip columns that open the guide: the feature name and `g Guide`.
pub(super) const REMOTE_TIP_GUIDE_COLUMNS: std::ops::Range<u16> = 5..28;
/// Tip columns that hide the tip: `x Hide`.
pub(super) const REMOTE_TIP_HIDE_COLUMNS: std::ops::Range<u16> = 30..36;

/// Agents footer layout while the remote servers tip is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoteTipFooter {
    /// Which key hints stay, by index into the agents footer.
    pub keep: Vec<bool>,
    /// The tip, just left of `? Help`.
    pub tip: Rect,
}

/// Fit the remote servers tip into the agents footer, whose key hints are
/// `hint_widths` columns wide. Lower-priority hints give way until the rest
/// fit before the tip; `None` when the footer is too narrow even with only
/// `q Quit` left.
pub(super) fn remote_tip_footer(footer: Rect, hint_widths: &[u16]) -> Option<RemoteTipFooter> {
    let available = footer
        .width
        .checked_sub(REMOTE_TIP_WIDTH + FOOTER_HELP_WIDTH + 1)?;
    let mut keep = vec![true; hint_widths.len()];
    let mut drops = agent_hint_drop_order(hint_widths.len()).into_iter();
    while footer_hints_width(hint_widths, &keep) > available {
        keep[drops.next()?] = false;
    }
    let tip = Rect::new(
        footer.right() - FOOTER_HELP_WIDTH - REMOTE_TIP_WIDTH,
        footer.y,
        REMOTE_TIP_WIDTH,
        1,
    );
    Some(RemoteTipFooter { keep, tip })
}

/// Agents footer hints the tip may displace, lowest priority first: Stale,
/// Scope, Sort, Jump, PR, Diff, Input, the active filter, then Worktrees.
/// `q Quit`, the last hint, always stays. The footer lists seven fixed hints,
/// the filter only while one is set, then Worktrees and Quit.
fn agent_hint_drop_order(hint_count: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..7.min(hint_count)).rev().collect();
    order.extend(7..hint_count.saturating_sub(1));
    order
}

/// Width of the rendered key hints: two leading spaces, then the kept hints
/// separated by ` │ `.
pub(super) fn footer_hints_width(widths: &[u16], keep: &[bool]) -> u16 {
    let kept: Vec<u16> = widths
        .iter()
        .zip(keep)
        .filter_map(|(width, keep)| keep.then_some(*width))
        .collect();
    2 + kept.iter().sum::<u16>() + 3 * kept.len().saturating_sub(1) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Widths of the agents footer hints, in footer order.
    const AGENT_HINTS: [(&str, u16); 9] = [
        ("i Input", 7),
        ("d Diff", 6),
        ("o PR", 4),
        ("1-9 Jump", 8),
        ("s Sort (priority)", 17),
        ("F Scope (all)", 13),
        ("f Stale (shown)", 15),
        ("Tab Worktrees", 13),
        ("q Quit", 6),
    ];

    fn kept(footer: &RemoteTipFooter) -> Vec<&'static str> {
        AGENT_HINTS
            .iter()
            .zip(&footer.keep)
            .filter_map(|((label, _), keep)| keep.then_some(*label))
            .collect()
    }

    fn fit(width: u16) -> Option<RemoteTipFooter> {
        let widths: Vec<u16> = AGENT_HINTS.iter().map(|(_, w)| *w).collect();
        remote_tip_footer(Rect::new(0, 39, width, 1), &widths)
    }

    #[test]
    fn wide_footers_keep_every_hint_beside_the_tip() {
        let footer = fit(170).unwrap();
        assert!(footer.keep.iter().all(|keep| *keep));
        assert_eq!(footer.tip, Rect::new(170 - 7 - 39, 39, 39, 1));
    }

    #[test]
    fn ordinary_widths_drop_low_priority_hints_for_the_tip() {
        assert_eq!(
            kept(&fit(120).unwrap()),
            [
                "i Input",
                "d Diff",
                "o PR",
                "1-9 Jump",
                "Tab Worktrees",
                "q Quit"
            ]
        );
        assert_eq!(
            kept(&fit(100).unwrap()),
            ["i Input", "d Diff", "o PR", "Tab Worktrees", "q Quit"]
        );
        assert_eq!(kept(&fit(80).unwrap()), ["Tab Worktrees", "q Quit"]);
        assert_eq!(fit(80).unwrap().tip, Rect::new(80 - 7 - 39, 39, 39, 1));
    }

    #[test]
    fn very_narrow_footers_omit_the_tip() {
        // `q Quit` alone takes 8 columns, then a space, the tip and help.
        let smallest = 8 + 1 + REMOTE_TIP_WIDTH + FOOTER_HELP_WIDTH;
        assert_eq!(kept(&fit(smallest).unwrap()), ["q Quit"]);
        assert_eq!(fit(smallest - 1), None);
        assert_eq!(fit(40), None);
        assert_eq!(fit(0), None);
    }

    #[test]
    fn active_filter_outranks_input_but_not_worktrees() {
        assert_eq!(agent_hint_drop_order(9), [6, 5, 4, 3, 2, 1, 0, 7]);
        assert_eq!(agent_hint_drop_order(10), [6, 5, 4, 3, 2, 1, 0, 7, 8]);
    }

    #[test]
    fn hints_width_counts_leading_space_and_separators() {
        assert_eq!(footer_hints_width(&[7, 6], &[true, true]), 2 + 7 + 3 + 6);
        assert_eq!(footer_hints_width(&[7, 6], &[false, true]), 2 + 6);
        assert_eq!(footer_hints_width(&[], &[]), 2);
    }

    #[test]
    fn stacked_preserves_table_above_preview() {
        let content = Rect::new(5, 7, 100, 40);
        let (table, preview) = dashboard_content_areas(content, true, 60, DashboardLayout::Stacked);
        assert_eq!(table, Rect::new(5, 7, 100, 16));
        assert_eq!(preview, Some(Rect::new(5, 23, 100, 24)));
    }

    #[test]
    fn side_by_side_places_full_height_preview_on_left() {
        let content = Rect::new(5, 7, 100, 40);
        let (table, preview) =
            dashboard_content_areas(content, true, 60, DashboardLayout::SideBySide);
        assert_eq!(table, Rect::new(65, 7, 40, 40));
        assert_eq!(preview, Some(Rect::new(5, 7, 60, 40)));
    }

    #[test]
    fn unsupported_preview_uses_all_content() {
        let content = Rect::new(5, 7, 100, 40);
        for layout in [DashboardLayout::Stacked, DashboardLayout::SideBySide] {
            assert_eq!(
                dashboard_content_areas(content, false, 60, layout),
                (content, None)
            );
        }
    }

    #[test]
    fn splits_fit_small_and_empty_areas() {
        for width in [0, 1, 2, 20, 80] {
            for height in [0, 1, 2, 24] {
                for size in [10, 60, 90] {
                    for layout in [DashboardLayout::Stacked, DashboardLayout::SideBySide] {
                        let content = Rect::new(5, 7, width, height);
                        let (table, preview) = dashboard_content_areas(content, true, size, layout);
                        let preview = preview.unwrap();
                        for area in [table, preview] {
                            assert!(area.x >= content.x && area.y >= content.y);
                            assert!(area.right() <= content.right());
                            assert!(area.bottom() <= content.bottom());
                        }
                        match layout {
                            DashboardLayout::Stacked => {
                                assert_eq!(table.bottom(), preview.y);
                                assert_eq!(table.height + preview.height, height);
                            }
                            DashboardLayout::SideBySide => {
                                assert_eq!(preview.right(), table.x);
                                assert_eq!(table.width + preview.width, width);
                            }
                        }
                    }
                }
            }
        }
    }
}
