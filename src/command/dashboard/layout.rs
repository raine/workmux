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

/// Place the remote servers tip just left of `? Help` when the footer has
/// room for it after the key hints, which take `hints_width` columns.
pub(super) fn remote_tip_area(footer: Rect, hints_width: u16) -> Option<Rect> {
    let needed = hints_width
        .saturating_add(1)
        .saturating_add(REMOTE_TIP_WIDTH)
        .saturating_add(FOOTER_HELP_WIDTH);
    (footer.width >= needed).then(|| {
        Rect::new(
            footer.right() - FOOTER_HELP_WIDTH - REMOTE_TIP_WIDTH,
            footer.y,
            REMOTE_TIP_WIDTH,
            1,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_tip_sits_left_of_help_when_it_fits() {
        let footer = Rect::new(2, 30, 160, 1);
        let tip = remote_tip_area(footer, 100).unwrap();
        assert_eq!(tip, Rect::new(2 + 160 - 7 - 39, 30, 39, 1));
        assert_eq!(tip.right(), footer.right() - FOOTER_HELP_WIDTH);
        // One column of space separates the tip from the last hint.
        assert!(remote_tip_area(footer, 113).is_some());
        assert!(remote_tip_area(footer, 114).is_none());
    }

    #[test]
    fn remote_tip_is_dropped_on_narrow_footers() {
        let hints = 115;
        let fits = hints + 1 + REMOTE_TIP_WIDTH + FOOTER_HELP_WIDTH;
        assert!(remote_tip_area(Rect::new(0, 0, fits, 1), hints).is_some());
        assert!(remote_tip_area(Rect::new(0, 0, fits - 1, 1), hints).is_none());
        assert!(remote_tip_area(Rect::new(0, 0, 80, 1), hints).is_none());
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
