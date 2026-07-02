//! Tmux layout tree parser, serializer, and reflow logic.
//!
//! Tmux encodes window layouts as a string like:
//!   `checksum,WxH,X,Y{child1,child2,...}`
//!
//! Where:
//! - `{children}` = horizontal split (children side by side)
//! - `[children]` = vertical split (children stacked)
//! - `WxH,X,Y,pane_id` = leaf pane
//!
//! The checksum is a 4-char hex value (tmux's rotate-right-and-add algorithm).
//!
//! This module parses the layout string into a tree, allows surgical width
//! reflow after sidebar creation, and serializes back for `select-layout`.

use tracing::debug;

use crate::cmd::Cmd;
use crate::config::SidebarPosition;

/// A rectangle in the tmux layout coordinate system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    w: u16,
    h: u16,
    x: u16,
    y: u16,
}

/// A node in the tmux layout tree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LayoutNode {
    Leaf {
        rect: Rect,
        pane_id: u32,
    },
    HSplit {
        rect: Rect,
        children: Vec<LayoutNode>,
    },
    VSplit {
        rect: Rect,
        children: Vec<LayoutNode>,
    },
}

impl LayoutNode {
    fn rect(&self) -> &Rect {
        match self {
            LayoutNode::Leaf { rect, .. }
            | LayoutNode::HSplit { rect, .. }
            | LayoutNode::VSplit { rect, .. } => rect,
        }
    }

    fn rect_mut(&mut self) -> &mut Rect {
        match self {
            LayoutNode::Leaf { rect, .. }
            | LayoutNode::HSplit { rect, .. }
            | LayoutNode::VSplit { rect, .. } => rect,
        }
    }

    fn width(&self) -> u16 {
        self.rect().w
    }

    fn height(&self) -> u16 {
        self.rect().h
    }
}

// ── Parser ──────────────────────────────────────────────────────

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    fn expect(&mut self, ch: u8) -> Option<()> {
        if self.peek() == Some(ch) {
            self.advance();
            Some(())
        } else {
            None
        }
    }

    fn parse_num<T: std::str::FromStr>(&mut self) -> Option<T> {
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.advance();
        }
        if self.pos == start {
            return None;
        }
        std::str::from_utf8(&self.input[start..self.pos])
            .ok()?
            .parse()
            .ok()
    }

    /// Parse `WxH,X,Y` prefix shared by all node types.
    fn parse_rect(&mut self) -> Option<Rect> {
        let w = self.parse_num()?;
        self.expect(b'x')?;
        let h = self.parse_num()?;
        self.expect(b',')?;
        let x = self.parse_num()?;
        self.expect(b',')?;
        let y = self.parse_num()?;
        Some(Rect { w, h, x, y })
    }

    /// Parse a single layout node (leaf or split).
    fn parse_node(&mut self) -> Option<LayoutNode> {
        let rect = self.parse_rect()?;

        match self.peek() {
            Some(b'{') => {
                self.advance();
                let children = self.parse_children(b'}')?;
                Some(LayoutNode::HSplit { rect, children })
            }
            Some(b'[') => {
                self.advance();
                let children = self.parse_children(b']')?;
                Some(LayoutNode::VSplit { rect, children })
            }
            Some(b',') => {
                self.advance();
                let pane_id = self.parse_num()?;
                Some(LayoutNode::Leaf { rect, pane_id })
            }
            _ => None,
        }
    }

    /// Parse comma-separated children until the closing bracket.
    fn parse_children(&mut self, close: u8) -> Option<Vec<LayoutNode>> {
        let mut children = Vec::new();
        loop {
            children.push(self.parse_node()?);
            match self.peek() {
                Some(c) if c == close => {
                    self.advance();
                    return Some(children);
                }
                Some(b',') => {
                    self.advance();
                }
                _ => return None,
            }
        }
    }
}

/// Parse a tmux layout string (including checksum prefix) into a tree.
fn parse_layout(layout: &str) -> Option<LayoutNode> {
    // Skip "XXXX," checksum prefix
    let body = layout.get(5..)?;
    if layout.as_bytes().get(4).copied().is_none_or(|b| b != b',') {
        return None;
    }
    let mut parser = Parser::new(body);
    let node = parser.parse_node()?;
    // Ensure entire input was consumed
    if parser.pos == parser.input.len() {
        Some(node)
    } else {
        None
    }
}

// ── Serializer ──────────────────────────────────────────────────

/// Serialize a layout node back to tmux format (without checksum).
fn serialize_node(node: &LayoutNode) -> String {
    let mut out = String::new();
    write_node(node, &mut out);
    out
}

fn write_node(node: &LayoutNode, out: &mut String) {
    use std::fmt::Write;
    let r = node.rect();
    let _ = write!(out, "{}x{},{},{}", r.w, r.h, r.x, r.y);

    match node {
        LayoutNode::Leaf { pane_id, .. } => {
            let _ = write!(out, ",{}", pane_id);
        }
        LayoutNode::HSplit { children, .. } => {
            out.push('{');
            for (i, child) in children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_node(child, out);
            }
            out.push('}');
        }
        LayoutNode::VSplit { children, .. } => {
            out.push('[');
            for (i, child) in children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_node(child, out);
            }
            out.push(']');
        }
    }
}

/// Compute tmux's layout checksum (rotate-right-and-add).
fn layout_checksum(layout: &str) -> u16 {
    let mut csum: u16 = 0;
    for &b in layout.as_bytes() {
        csum = (csum >> 1) | ((csum & 1) << 15);
        csum = csum.wrapping_add(b as u16);
    }
    csum
}

/// Serialize a layout tree into a full tmux layout string with checksum.
fn serialize_layout(root: &LayoutNode) -> String {
    let body = serialize_node(root);
    let checksum = layout_checksum(&body);
    format!("{:04x},{}", checksum, body)
}

// ── Reflow ──────────────────────────────────────────────────────

/// Distribute `available` length among items proportionally to their old lengths.
/// The last item gets the remainder to avoid rounding gaps.
fn proportional_lengths(old_lengths: &[u16], available: u16) -> Vec<u16> {
    let old_total: u16 = old_lengths.iter().sum();
    if old_total == 0 || old_lengths.is_empty() {
        return vec![0; old_lengths.len()];
    }
    let mut remaining = available;
    let last = old_lengths.len() - 1;
    old_lengths
        .iter()
        .enumerate()
        .map(|(i, &old_len)| {
            if i == last {
                remaining
            } else {
                let scaled = (old_len as f64 * available as f64 / old_total as f64).round() as u16;
                let scaled = scaled.min(remaining);
                remaining = remaining.saturating_sub(scaled);
                scaled
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

impl Axis {
    fn for_position(position: SidebarPosition) -> Self {
        match position {
            SidebarPosition::Left => Self::Horizontal,
            SidebarPosition::Top => Self::Vertical,
        }
    }

    fn root_matches(self, node: &LayoutNode) -> bool {
        matches!(
            (self, node),
            (Axis::Horizontal, LayoutNode::HSplit { .. })
                | (Axis::Vertical, LayoutNode::VSplit { .. })
        )
    }
}

fn node_extent(node: &LayoutNode, axis: Axis) -> u16 {
    match axis {
        Axis::Horizontal => node.width(),
        Axis::Vertical => node.height(),
    }
}

fn rect_extent(rect: &Rect, axis: Axis) -> u16 {
    match axis {
        Axis::Horizontal => rect.w,
        Axis::Vertical => rect.h,
    }
}

fn rect_pos(rect: &Rect, axis: Axis) -> u16 {
    match axis {
        Axis::Horizontal => rect.x,
        Axis::Vertical => rect.y,
    }
}

/// Recursively scale a subtree along one axis, preserving internal proportions.
fn scale_axis(node: &mut LayoutNode, axis: Axis, new_len: u16, new_pos: u16) {
    let rect = node.rect_mut();
    match axis {
        Axis::Horizontal => {
            rect.w = new_len;
            rect.x = new_pos;
        }
        Axis::Vertical => {
            rect.h = new_len;
            rect.y = new_pos;
        }
    }

    match (axis, node) {
        (Axis::Horizontal, LayoutNode::HSplit { children, .. })
        | (Axis::Vertical, LayoutNode::VSplit { children, .. }) => {
            let seps = children.len().saturating_sub(1) as u16;
            let old_lengths: Vec<u16> = children.iter().map(|c| node_extent(c, axis)).collect();
            let new_lengths = proportional_lengths(&old_lengths, new_len.saturating_sub(seps));

            let mut pos = new_pos;
            for (child, child_len) in children.iter_mut().zip(new_lengths) {
                scale_axis(child, axis, child_len, pos);
                pos = pos.saturating_add(child_len).saturating_add(1);
            }
        }
        (Axis::Horizontal, LayoutNode::VSplit { children, .. })
        | (Axis::Vertical, LayoutNode::HSplit { children, .. }) => {
            for child in children {
                scale_axis(child, axis, new_len, new_pos);
            }
        }
        (_, LayoutNode::Leaf { .. }) => {}
    }
}

#[cfg(test)]
fn scale_width(node: &mut LayoutNode, new_w: u16, new_x: u16) {
    scale_axis(node, Axis::Horizontal, new_w, new_x);
}

#[cfg(test)]
fn scale_height(node: &mut LayoutNode, new_h: u16, new_y: u16) {
    scale_axis(node, Axis::Vertical, new_h, new_y);
}

/// Rebalance the window layout after a sidebar pane was added.
///
/// After `split-window -hbf`, the root is an HSplit with 2 children:
/// `{sidebar_leaf, content_tree}`. The sidebar stole width only from
/// the first pane it was split from, leaving the rest of the content
/// tree lopsided. This function scales the content subtree to fill
/// the remaining space proportionally, then applies the fixed layout
/// atomically via `select-layout`.
pub(super) fn reflow_after_sidebar_add(
    window_id: &str,
    sidebar_pane_id: &str,
    position: SidebarPosition,
    sidebar_size: u16,
) {
    reflow_after_sidebar_add_to_window_extent(
        window_id,
        sidebar_pane_id,
        position,
        sidebar_size,
        None,
    );
}

pub(super) fn reflow_after_sidebar_add_to_window_extent(
    window_id: &str,
    sidebar_pane_id: &str,
    position: SidebarPosition,
    sidebar_size: u16,
    window_extent: Option<u16>,
) {
    let layout_str = match Cmd::new("tmux")
        .args(&["display-message", "-t", window_id, "-p", "#{window_layout}"])
        .run_and_capture_stdout()
    {
        Ok(s) => s.trim().to_string(),
        Err(_) => return,
    };

    debug!(
        window_id,
        sidebar_pane_id,
        sidebar_size,
        position = ?position,
        layout = layout_str.as_str(),
        "reflow: starting"
    );

    let mut root = match parse_layout(&layout_str) {
        Some(node) => node,
        None => {
            debug!(
                layout = layout_str.as_str(),
                "reflow: failed to parse layout"
            );
            return;
        }
    };

    let axis = Axis::for_position(position);
    if !axis.root_matches(&root) {
        debug!(position = ?position, "reflow: root split does not match sidebar position, skipping");
        return;
    }

    let (rect, children) = match &mut root {
        LayoutNode::HSplit { rect, children } | LayoutNode::VSplit { rect, children } => {
            (rect, children)
        }
        LayoutNode::Leaf { .. } => return,
    };

    let sidebar_num: u32 = sidebar_pane_id
        .strip_prefix('%')
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let sidebar_idx = children.iter().position(
        |child| matches!(child, LayoutNode::Leaf { pane_id, .. } if *pane_id == sidebar_num),
    );

    let Some(sidebar_idx) = sidebar_idx else {
        debug!(
            sidebar_pane_id,
            "reflow: sidebar pane not found among {} root children",
            children.len()
        );
        return;
    };

    debug!(
        sidebar_idx,
        root_children = children.len(),
        "reflow: found sidebar"
    );

    if let Some(extent) = window_extent {
        match axis {
            Axis::Horizontal => rect.w = extent,
            Axis::Vertical => rect.h = extent,
        }
    }

    let root_pos = rect_pos(rect, axis);
    match axis {
        Axis::Horizontal => {
            children[sidebar_idx].rect_mut().w = sidebar_size;
            children[sidebar_idx].rect_mut().x = root_pos;
        }
        Axis::Vertical => {
            children[sidebar_idx].rect_mut().h = sidebar_size;
            children[sidebar_idx].rect_mut().y = root_pos;
        }
    }

    let window_len = rect_extent(rect, axis);
    let num_content = children.len() - 1;
    let total_seps = (children.len() as u16).saturating_sub(1);
    let available = window_len
        .saturating_sub(sidebar_size)
        .saturating_sub(total_seps);

    let content_indices: Vec<usize> = (0..children.len()).filter(|&i| i != sidebar_idx).collect();
    let old_lengths: Vec<u16> = content_indices
        .iter()
        .map(|&i| node_extent(&children[i], axis))
        .collect();

    debug!(
        window_len,
        available, num_content, "reflow: scaling content"
    );

    if available == 0 {
        return;
    }

    let new_lengths = proportional_lengths(&old_lengths, available);
    let mut pos = root_pos.saturating_add(sidebar_size).saturating_add(1);

    for (&idx, new_len) in content_indices.iter().zip(new_lengths) {
        scale_axis(&mut children[idx], axis, new_len, pos);
        pos = pos.saturating_add(new_len).saturating_add(1);
    }

    // Apply the rebalanced layout
    let new_layout = serialize_layout(&root);
    debug!(
        window_id,
        old = layout_str.as_str(),
        new = new_layout.as_str(),
        "reflow: applying"
    );

    let _ = Cmd::new("tmux")
        .args(&["select-layout", "-t", window_id, &new_layout])
        .run();
}

// ── Sidebar removal ────────────────────────────────────────────

fn prune_split_children(
    rect: Rect,
    children: Vec<LayoutNode>,
    target: u32,
    rebuild: impl FnOnce(Rect, Vec<LayoutNode>) -> LayoutNode,
) -> Option<LayoutNode> {
    let mut children: Vec<_> = children
        .into_iter()
        .filter_map(|child| prune_pane(child, target))
        .collect();
    match children.len() {
        0 => None,
        1 => Some(children.remove(0)),
        _ => Some(rebuild(rect, children)),
    }
}

/// Remove a pane from the layout tree by its numeric ID.
/// Collapses any split that ends up with a single child.
fn prune_pane(node: LayoutNode, target: u32) -> Option<LayoutNode> {
    match node {
        LayoutNode::Leaf { pane_id, .. } if pane_id == target => None,
        leaf @ LayoutNode::Leaf { .. } => Some(leaf),
        LayoutNode::HSplit { rect, children } => {
            prune_split_children(rect, children, target, |rect, children| {
                LayoutNode::HSplit { rect, children }
            })
        }
        LayoutNode::VSplit { rect, children } => {
            prune_split_children(rect, children, target, |rect, children| {
                LayoutNode::VSplit { rect, children }
            })
        }
    }
}

/// Compute the layout string for a window after removing the sidebar pane.
///
/// Reads the current live layout, prunes the sidebar node from the tree,
/// and scales the remaining content to fill the full window width. This
/// preserves whatever pane arrangement the user created while the sidebar
/// was open, unlike the old save/restore approach which used a stale snapshot.
pub(super) fn layout_after_sidebar_remove(
    window_id: &str,
    sidebar_pane_id: &str,
    position: SidebarPosition,
) -> Option<String> {
    let layout_str = Cmd::new("tmux")
        .args(&["display-message", "-t", window_id, "-p", "#{window_layout}"])
        .run_and_capture_stdout()
        .ok()?;
    let layout_str = layout_str.trim();

    debug!(
        window_id,
        sidebar_pane_id,
        layout = layout_str,
        "layout_after_sidebar_remove: starting"
    );

    let root = parse_layout(layout_str)?;
    let window_rect = *root.rect();

    let sidebar_num: u32 = sidebar_pane_id
        .strip_prefix('%')
        .and_then(|s| s.parse().ok())?;

    let axis = Axis::for_position(position);
    let mut content = prune_pane(root, sidebar_num)?;
    scale_axis(
        &mut content,
        axis,
        rect_extent(&window_rect, axis),
        rect_pos(&window_rect, axis),
    );

    let result = serialize_layout(&content);
    debug!(
        window_id,
        old = layout_str,
        new = result.as_str(),
        "layout_after_sidebar_remove: computed"
    );

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: u16, h: u16, x: u16, y: u16) -> Rect {
        Rect { w, h, x, y }
    }

    fn leaf(w: u16, h: u16, x: u16, y: u16, pane_id: u32) -> LayoutNode {
        LayoutNode::Leaf {
            rect: rect(w, h, x, y),
            pane_id,
        }
    }

    fn hsplit(w: u16, h: u16, x: u16, y: u16, children: Vec<LayoutNode>) -> LayoutNode {
        LayoutNode::HSplit {
            rect: rect(w, h, x, y),
            children,
        }
    }

    fn vsplit(w: u16, h: u16, x: u16, y: u16, children: Vec<LayoutNode>) -> LayoutNode {
        LayoutNode::VSplit {
            rect: rect(w, h, x, y),
            children,
        }
    }

    #[test]
    fn test_parse_single_pane() {
        let layout = "1234,80x24,0,0,42";
        let node = parse_layout(layout).unwrap();
        assert_eq!(
            node,
            LayoutNode::Leaf {
                rect: Rect {
                    w: 80,
                    h: 24,
                    x: 0,
                    y: 0
                },
                pane_id: 42,
            }
        );
    }

    #[test]
    fn test_parse_hsplit() {
        // Two panes side by side: 40+39=79, +1 separator = 80
        let layout = "abcd,80x24,0,0{40x24,0,0,1,39x24,41,0,2}";
        let node = parse_layout(layout).unwrap();
        match node {
            LayoutNode::HSplit { rect, children } => {
                assert_eq!(rect.w, 80);
                assert_eq!(children.len(), 2);
                assert_eq!(children[0].width(), 40);
                assert_eq!(children[1].width(), 39);
            }
            _ => panic!("expected HSplit"),
        }
    }

    #[test]
    fn test_parse_vsplit() {
        // Two panes stacked: 12+11=23, +1 separator = 24
        let layout = "abcd,80x24,0,0[80x12,0,0,1,80x11,0,13,2]";
        let node = parse_layout(layout).unwrap();
        match node {
            LayoutNode::VSplit { rect, children } => {
                assert_eq!(rect.h, 24);
                assert_eq!(children.len(), 2);
                assert_eq!(children[0].rect().h, 12);
                assert_eq!(children[1].rect().h, 11);
            }
            _ => panic!("expected VSplit"),
        }
    }

    #[test]
    fn test_parse_nested() {
        // Real layout from tmux: HSplit containing [VSplit, Leaf]
        let layout = "123a,186x44,0,0{93x44,0,0[93x22,0,0,1189,93x21,0,23,1394],92x44,94,0,1387}";
        let node = parse_layout(layout).unwrap();
        match node {
            LayoutNode::HSplit { children, .. } => {
                assert_eq!(children.len(), 2);
                // First child is a VSplit
                match &children[0] {
                    LayoutNode::VSplit { children: vc, .. } => {
                        assert_eq!(vc.len(), 2);
                    }
                    _ => panic!("expected VSplit as first child"),
                }
                // Second child is a Leaf
                assert!(matches!(
                    &children[1],
                    LayoutNode::Leaf { pane_id: 1387, .. }
                ));
            }
            _ => panic!("expected HSplit"),
        }
    }

    /// Parse and serialize every real layout, verify exact roundtrip.
    #[test]
    fn test_roundtrip_real_layouts() {
        // Real layouts captured from a live tmux session
        let layouts = [
            // HSplit with nested VSplit
            "9d0a,373x79,0,0{205x79,0,0[205x39,0,0,1070,205x39,0,40,1073],167x79,206,0,1072}",
            // Simple HSplit (2 panes)
            "6804,373x79,0,0{205x79,0,0,1075,167x79,206,0,1077}",
            "1bd3,373x79,0,0{242x79,0,0,510,130x79,243,0,532}",
            "f6ce,373x79,0,0{211x79,0,0,509,161x79,212,0,986}",
            "7e05,373x79,0,0{205x79,0,0,988,167x79,206,0,989}",
            "37ce,373x79,0,0{221x79,0,0,521,151x79,222,0,528}",
            // HSplit where second child is a VSplit
            "c6bc,373x79,0,0{212x79,0,0,634,160x79,213,0[160x20,213,0,636,160x58,213,21,637]}",
            // Single pane
            "f64a,373x79,0,0,640",
            "f652,373x79,0,0,648",
            "f651,373x79,0,0,666",
            // Smaller terminal layouts
            "ec38,373x79,0,0,67",
            // Complex: HSplit with two nested VSplits
            "0e04,186x44,0,0{100x44,0,0[100x21,0,0,980,100x22,0,22,817],85x44,101,0[85x21,101,0,985,85x22,101,22,1169]}",
            // HSplit with leaf + nested VSplit
            "f910,186x44,0,0{91x44,0,0,350,94x44,92,0[94x21,92,0,512,94x22,92,22,1074]}",
            "123a,186x44,0,0{93x44,0,0[93x22,0,0,1189,93x21,0,23,1394],92x44,94,0,1387}",
            // 3 children at root (sidebar + 2 content panes)
            "7fb5,186x44,0,0{25x44,0,0,1395,91x44,26,0,1196,68x44,118,0,1307}",
            "184f,186x44,0,0,1396",
        ];

        for layout in layouts {
            let node =
                parse_layout(layout).unwrap_or_else(|| panic!("failed to parse: {}", layout));
            let result = serialize_layout(&node);
            assert_eq!(result, layout, "roundtrip failed");
        }
    }

    #[test]
    fn test_checksum_known_values() {
        // Verify checksum against multiple real tmux layouts
        let cases = [
            (
                "373x79,0,0{205x79,0,0[205x39,0,0,1070,205x39,0,40,1073],167x79,206,0,1072}",
                "9d0a",
            ),
            ("373x79,0,0{205x79,0,0,1075,167x79,206,0,1077}", "6804"),
            ("373x79,0,0,640", "f64a"),
            (
                "186x44,0,0{93x44,0,0[93x22,0,0,1189,93x21,0,23,1394],92x44,94,0,1387}",
                "123a",
            ),
        ];
        for (body, expected_hex) in cases {
            let expected = u16::from_str_radix(expected_hex, 16).unwrap();
            assert_eq!(
                layout_checksum(body),
                expected,
                "checksum mismatch for: {}",
                body
            );
        }
    }

    #[test]
    fn test_scale_width_leaf() {
        let mut node = LayoutNode::Leaf {
            rect: Rect {
                w: 100,
                h: 50,
                x: 0,
                y: 0,
            },
            pane_id: 1,
        };
        scale_width(&mut node, 80, 10);
        assert_eq!(node.rect().w, 80);
        assert_eq!(node.rect().x, 10);
        assert_eq!(node.rect().h, 50); // height unchanged
    }

    #[test]
    fn test_scale_width_hsplit_proportional() {
        // HSplit 200 wide: children 100+99=199, 1 separator
        let mut node = LayoutNode::HSplit {
            rect: Rect {
                w: 200,
                h: 50,
                x: 0,
                y: 0,
            },
            children: vec![
                LayoutNode::Leaf {
                    rect: Rect {
                        w: 100,
                        h: 50,
                        x: 0,
                        y: 0,
                    },
                    pane_id: 1,
                },
                LayoutNode::Leaf {
                    rect: Rect {
                        w: 99,
                        h: 50,
                        x: 101,
                        y: 0,
                    },
                    pane_id: 2,
                },
            ],
        };

        // Scale to 150 wide, starting at x=30
        scale_width(&mut node, 150, 30);

        assert_eq!(node.rect().w, 150);
        assert_eq!(node.rect().x, 30);
        // Available for children: 150 - 1 separator = 149
        // Old total: 199. Scale factor = 149/199
        // Child 1: round(100 * 149/199) = round(74.87) = 75
        // Child 2: 149 - 75 = 74
        let children = match &node {
            LayoutNode::HSplit { children, .. } => children,
            _ => panic!(),
        };
        assert_eq!(children[0].width(), 75);
        assert_eq!(children[1].width(), 74);
        // x positions: child 0 at 30, child 1 at 30+75+1=106
        assert_eq!(children[0].rect().x, 30);
        assert_eq!(children[1].rect().x, 106);
    }

    #[test]
    fn test_scale_width_vsplit() {
        let mut node = vsplit(
            100,
            50,
            0,
            0,
            vec![leaf(100, 24, 0, 0, 1), leaf(100, 25, 0, 25, 2)],
        );

        scale_width(&mut node, 80, 20);

        // Both children should get the same new width
        let children = match &node {
            LayoutNode::VSplit { children, .. } => children,
            _ => panic!(),
        };
        assert_eq!(children[0].width(), 80);
        assert_eq!(children[1].width(), 80);
        assert_eq!(children[0].rect().x, 20);
        assert_eq!(children[1].rect().x, 20);
        // Heights unchanged
        assert_eq!(children[0].rect().h, 24);
        assert_eq!(children[1].rect().h, 25);
    }

    #[test]
    fn test_scale_height_vsplit_proportional() {
        let mut node = vsplit(
            100,
            50,
            0,
            0,
            vec![leaf(100, 24, 0, 0, 1), leaf(100, 25, 0, 25, 2)],
        );

        scale_height(&mut node, 40, 5);

        let children = match &node {
            LayoutNode::VSplit { children, .. } => children,
            _ => panic!(),
        };
        assert_eq!(children[0].rect().h + children[1].rect().h + 1, 40);
        assert_eq!(children[0].rect().y, 5);
        assert_eq!(children[1].rect().y, 25);
        assert_eq!(children[0].width(), 100);
        assert_eq!(children[1].width(), 100);
    }

    #[test]
    fn test_scale_height_hsplit() {
        let mut node = LayoutNode::HSplit {
            rect: Rect {
                w: 100,
                h: 50,
                x: 0,
                y: 0,
            },
            children: vec![
                LayoutNode::Leaf {
                    rect: Rect {
                        w: 49,
                        h: 50,
                        x: 0,
                        y: 0,
                    },
                    pane_id: 1,
                },
                LayoutNode::Leaf {
                    rect: Rect {
                        w: 50,
                        h: 50,
                        x: 50,
                        y: 0,
                    },
                    pane_id: 2,
                },
            ],
        };

        scale_height(&mut node, 40, 5);

        let children = match &node {
            LayoutNode::HSplit { children, .. } => children,
            _ => panic!(),
        };
        assert_eq!(children[0].rect().h, 40);
        assert_eq!(children[1].rect().h, 40);
        assert_eq!(children[0].rect().y, 5);
        assert_eq!(children[1].rect().y, 5);
        assert_eq!(children[0].width(), 49);
        assert_eq!(children[1].width(), 50);
    }

    /// Simulate what reflow_after_sidebar_add does: sidebar + content VSplit.
    #[test]
    fn test_scale_sidebar_plus_vsplit_content() {
        // Layout after split-window -hbf: {sidebar(35), content_vsplit(150)}
        // Window is 186 wide: 35 + 1 separator + 150 = 186
        let layout =
            "0000,186x44,0,0{35x44,0,0,999,150x44,36,0[150x22,36,0,1189,150x21,36,23,1394]}";
        let mut root = parse_layout(layout).unwrap();

        if let LayoutNode::HSplit { children, .. } = &mut root {
            // Scale content to fill remaining: 186 - 35 - 1 = 150 (already correct here)
            let content_w = 186u16.saturating_sub(35).saturating_sub(1);
            scale_width(&mut children[1], content_w, 36);

            assert_eq!(children[1].width(), content_w);
            // All VSplit children should have the same width
            if let LayoutNode::VSplit { children: vc, .. } = &children[1] {
                assert_eq!(vc[0].width(), content_w);
                assert_eq!(vc[1].width(), content_w);
            }
        }
    }

    /// Test reflow where root HSplit has 2 children (sidebar nested old root).
    #[test]
    fn test_reflow_two_children() {
        // Window=200, sidebar=30, content is nested HSplit{60, 79}
        let layout = "0000,200x50,0,0{30x50,0,0,100,169x50,31,0{60x50,31,0,101,79x50,92,0,102}}";
        let mut root = parse_layout(layout).unwrap();

        if let LayoutNode::HSplit { children, .. } = &mut root {
            assert_eq!(children.len(), 2);

            let content_w = 200u16 - 30 - 1; // 169
            scale_width(&mut children[1], content_w, 31);

            assert_eq!(children[1].width(), content_w);
            if let LayoutNode::HSplit {
                children: content, ..
            } = &children[1]
            {
                let sum: u16 = content.iter().map(|c| c.width()).sum();
                assert_eq!(sum, 168); // 169 - 1 separator
                // Proportions roughly preserved (60:79 -> ~73:95)
                assert!(content[0].width() > 70 && content[0].width() < 76);
                assert!(content[1].width() > 92 && content[1].width() < 98);
            }
        }
    }

    /// Test reflow where sidebar is inserted as sibling in existing root HSplit
    /// (3 children: sidebar + 2 original panes). This is the case that was
    /// previously broken because we only handled 2 children.
    #[test]
    fn test_reflow_three_children_at_root() {
        // Original: {100x44[vsplit], 85x44} (186 wide, 2 children + 1 sep)
        // After split-window -hbf, sidebar inserted as 3rd sibling:
        // {25x44(sidebar), 75x44[vsplit], 85x44} - left shrunk, right untouched
        let layout =
            "0000,186x44,0,0{25x44,0,0,999,75x44,26,0[75x22,26,0,1,75x21,26,23,2],85x44,102,0,3}";
        let mut root = parse_layout(layout).unwrap();

        if let LayoutNode::HSplit { rect, children } = &mut root {
            assert_eq!(children.len(), 3);

            let sidebar_width: u16 = 25;
            children[0].rect_mut().w = sidebar_width;
            children[0].rect_mut().x = 0;

            // Available: 186 - 25 - 2 separators = 159
            let total_seps = 2u16;
            let available = rect.w - sidebar_width - total_seps;
            assert_eq!(available, 159);

            // Old content widths: 75 + 85 = 160
            // Scale preserves proportions: 75:85 ratio in 159 cols
            let old_total: u16 = 75 + 85;
            let mut remaining = available;
            let mut cx = sidebar_width + 1;

            let scaled = (75.0 * available as f64 / old_total as f64).round() as u16;
            remaining -= scaled;
            scale_width(&mut children[1], scaled, cx);
            cx += scaled + 1;
            scale_width(&mut children[2], remaining, cx);

            // Content fills all available space
            let w1 = children[1].width();
            let w2 = children[2].width();
            assert_eq!(w1 + w2, available);

            // Proportions preserved: original ratio 75:85 (~0.88)
            let original_ratio = 75.0 / 85.0;
            let result_ratio = w1 as f64 / w2 as f64;
            assert!(
                (result_ratio - original_ratio).abs() < 0.05,
                "proportions not preserved: original={:.2}, result={:.2}",
                original_ratio,
                result_ratio
            );

            // VSplit children should have matching width
            if let LayoutNode::VSplit { children: vc, .. } = &children[1] {
                assert_eq!(vc[0].width(), w1);
                assert_eq!(vc[1].width(), w1);
            }
        }
    }

    // ── prune_pane / sidebar removal tests ─────────────────────

    #[test]
    fn test_prune_removes_target_leaf() {
        let node = LayoutNode::Leaf {
            rect: Rect {
                w: 80,
                h: 24,
                x: 0,
                y: 0,
            },
            pane_id: 42,
        };
        assert_eq!(prune_pane(node, 42), None);
    }

    #[test]
    fn test_prune_keeps_other_leaf() {
        let node = LayoutNode::Leaf {
            rect: Rect {
                w: 80,
                h: 24,
                x: 0,
                y: 0,
            },
            pane_id: 42,
        };
        assert!(prune_pane(node, 99).is_some());
    }

    #[test]
    fn test_prune_collapses_single_child_hsplit() {
        // HSplit{sidebar, content} -> content (collapsed)
        let node = hsplit(
            186,
            44,
            0,
            0,
            vec![leaf(25, 44, 0, 0, 999), leaf(160, 44, 26, 0, 100)],
        );
        let result = prune_pane(node, 999).unwrap();
        assert!(matches!(result, LayoutNode::Leaf { pane_id: 100, .. }));
    }

    #[test]
    fn test_prune_preserves_multi_child_hsplit() {
        // HSplit{sidebar, a, b} -> HSplit{a, b}
        let node = hsplit(
            186,
            44,
            0,
            0,
            vec![
                leaf(25, 44, 0, 0, 999),
                leaf(80, 44, 26, 0, 1),
                leaf(79, 44, 107, 0, 2),
            ],
        );
        let result = prune_pane(node, 999).unwrap();
        match result {
            LayoutNode::HSplit { children, .. } => {
                assert_eq!(children.len(), 2);
                assert!(matches!(&children[0], LayoutNode::Leaf { pane_id: 1, .. }));
                assert!(matches!(&children[1], LayoutNode::Leaf { pane_id: 2, .. }));
            }
            _ => panic!("expected HSplit"),
        }
    }

    /// Simulate the reported bug: sidebar + single pane, user splits content,
    /// then sidebar is removed. The two content panes should fill the window
    /// proportionally (near-equal since they were split evenly).
    #[test]
    fn test_remove_sidebar_two_content_panes_fill_window() {
        // State after: sidebar=25, content split into 80+79=159, +2 seps = 186
        let layout = "0000,186x44,0,0{25x44,0,0,999,80x44,26,0,100,79x44,107,0,101}";
        let root = parse_layout(layout).unwrap();
        let window_w = root.rect().w;

        let mut content = prune_pane(root, 999).unwrap();
        scale_width(&mut content, window_w, 0);

        // Should be an HSplit with 2 children filling 186 cols
        match &content {
            LayoutNode::HSplit { rect, children } => {
                assert_eq!(rect.w, 186);
                assert_eq!(children.len(), 2);
                let w0 = children[0].width();
                let w1 = children[1].width();
                // children_total + 1 separator = 186
                assert_eq!(w0 + w1 + 1, 186);
                // Near-equal split (within 1 col of each other)
                assert!(
                    (w0 as i32 - w1 as i32).abs() <= 1,
                    "panes should be near-equal: {} vs {}",
                    w0,
                    w1,
                );
            }
            _ => panic!("expected HSplit"),
        }
    }

    /// Remove sidebar when content is a VSplit (sidebar + vsplit{a,b}).
    #[test]
    fn test_remove_sidebar_vsplit_content_fills_window() {
        // HSplit{sidebar(25), vsplit(160){a,b}} -> vsplit at full 186 width
        let layout = "0000,186x44,0,0{25x44,0,0,999,160x44,26,0[160x22,26,0,100,160x21,26,23,101]}";
        let root = parse_layout(layout).unwrap();
        let window_w = root.rect().w;

        let mut content = prune_pane(root, 999).unwrap();
        scale_width(&mut content, window_w, 0);

        match &content {
            LayoutNode::VSplit { rect, children } => {
                assert_eq!(rect.w, 186);
                // Both children get the full width
                assert_eq!(children[0].width(), 186);
                assert_eq!(children[1].width(), 186);
            }
            _ => panic!("expected VSplit"),
        }
    }

    /// Remove sidebar from 3-sibling root: {sidebar, hsplit{a,b}, c}.
    /// Content should scale proportionally to fill the window.
    #[test]
    fn test_remove_sidebar_nested_content_proportional() {
        // sidebar=25, hsplit{60,50}=111, leaf=48, +2 seps = 186
        let layout =
            "0000,186x44,0,0{25x44,0,0,999,111x44,26,0{60x44,26,0,1,50x44,87,0,2},48x44,138,0,3}";
        let root = parse_layout(layout).unwrap();
        let window_w = root.rect().w;

        let mut content = prune_pane(root, 999).unwrap();
        scale_width(&mut content, window_w, 0);

        match &content {
            LayoutNode::HSplit { rect, children } => {
                assert_eq!(rect.w, 186);
                assert_eq!(children.len(), 2);
                // First child is the nested HSplit, second is the leaf
                let w0 = children[0].width();
                let w1 = children[1].width();
                // Total + 1 sep = 186
                assert_eq!(w0 + w1 + 1, 186);
                // Proportions preserved: original 111:48 ratio
                let orig_ratio = 111.0 / 48.0;
                let result_ratio = w0 as f64 / w1 as f64;
                assert!(
                    (result_ratio - orig_ratio).abs() < 0.1,
                    "proportions not preserved: original={:.2}, result={:.2}",
                    orig_ratio,
                    result_ratio,
                );
            }
            _ => panic!("expected HSplit"),
        }
    }

    /// Remove sidebar leaving only a single content pane (the original bug scenario
    /// step 1-2 without the user splitting).
    #[test]
    fn test_remove_sidebar_single_content_fills_window() {
        // HSplit{sidebar(25), content(160)} -> content at full 186
        let layout = "0000,186x44,0,0{25x44,0,0,999,160x44,26,0,100}";
        let root = parse_layout(layout).unwrap();
        let window_w = root.rect().w;

        let mut content = prune_pane(root, 999).unwrap();
        scale_width(&mut content, window_w, 0);

        match &content {
            LayoutNode::Leaf { rect, pane_id } => {
                assert_eq!(*pane_id, 100);
                assert_eq!(rect.w, 186);
                assert_eq!(rect.x, 0);
            }
            _ => panic!("expected Leaf"),
        }
    }

    #[test]
    fn test_top_sidebar_plus_hsplit_content() {
        let layout = "0000,186x44,0,0[186x3,0,0,999,186x40,0,4{93x40,0,4,1,92x40,94,4,2}]";
        let mut root = parse_layout(layout).unwrap();

        if let LayoutNode::VSplit { children, .. } = &mut root {
            let content_h = 44u16 - 3 - 1;
            scale_height(&mut children[1], content_h, 4);

            assert_eq!(children[1].height(), content_h);
            if let LayoutNode::HSplit { children: hc, .. } = &children[1] {
                assert_eq!(hc[0].rect().h, content_h);
                assert_eq!(hc[1].rect().h, content_h);
                assert_eq!(hc[0].rect().y, 4);
                assert_eq!(hc[1].rect().y, 4);
            } else {
                panic!("expected HSplit content");
            }
        } else {
            panic!("expected VSplit root");
        }
    }

    #[test]
    fn test_remove_top_sidebar_two_content_panes_fill_window() {
        let root = vsplit(
            186,
            44,
            0,
            0,
            vec![
                leaf(186, 3, 0, 0, 999),
                leaf(186, 20, 0, 4, 100),
                leaf(186, 19, 0, 25, 101),
            ],
        );
        let window_h = root.rect().h;

        let mut content = prune_pane(root, 999).unwrap();
        scale_height(&mut content, window_h, 0);

        match &content {
            LayoutNode::VSplit { rect, children } => {
                assert_eq!(rect.h, 44);
                assert_eq!(children.len(), 2);
                let h0 = children[0].height();
                let h1 = children[1].height();
                assert_eq!(h0 + h1 + 1, 44);
                assert!((h0 as i32 - h1 as i32).abs() <= 1);
                assert_eq!(children[0].rect().y, 0);
                assert_eq!(children[1].rect().y, h0 + 1);
            }
            _ => panic!("expected VSplit"),
        }
    }

    #[test]
    fn test_remove_top_sidebar_nested_content_fills_height() {
        let root = vsplit(
            186,
            44,
            0,
            0,
            vec![
                leaf(186, 3, 0, 0, 999),
                hsplit(
                    186,
                    18,
                    0,
                    4,
                    vec![leaf(93, 18, 0, 4, 1), leaf(92, 18, 94, 4, 2)],
                ),
                leaf(186, 21, 0, 23, 3),
            ],
        );
        let window_h = root.rect().h;

        let mut content = prune_pane(root, 999).unwrap();
        scale_height(&mut content, window_h, 0);

        match &content {
            LayoutNode::VSplit { rect, children } => {
                assert_eq!(rect.h, 44);
                assert_eq!(children.len(), 2);
                let h0 = children[0].height();
                let h1 = children[1].height();
                assert_eq!(h0 + h1 + 1, 44);
                if let LayoutNode::HSplit { children: hc, .. } = &children[0] {
                    assert_eq!(hc[0].height(), h0);
                    assert_eq!(hc[1].height(), h0);
                } else {
                    panic!("expected HSplit content");
                }
            }
            _ => panic!("expected VSplit"),
        }
    }
}
