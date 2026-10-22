use anyhow::{Context, Result, anyhow, bail};
use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use std::collections::HashSet;
use tracing::debug;

use crate::config::{Config, SidebarHeight, SidebarPosition, SidebarWidth};
use crate::multiplexer::Multiplexer;
use crate::multiplexer::zellij::{ZellijBackend, ZellijSidebarPane, ZellijSidebarTab};

const SIDEBAR_PANE_NAME: &str = "workmux-sidebar";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PaneRect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl PaneRect {
    fn center(self) -> (f64, f64) {
        (
            f64::from(self.x) + f64::from(self.width) / 2.0,
            f64::from(self.y) + f64::from(self.height) / 2.0,
        )
    }

    fn area(self) -> u32 {
        u32::from(self.width) * u32::from(self.height)
    }
}

fn is_sidebar_pane(title: &str) -> bool {
    title == SIDEBAR_PANE_NAME
}

fn backend(instance_id: &str) -> ZellijBackend {
    ZellijBackend::for_session(instance_id)
}

fn tab_extent(tab: &ZellijSidebarTab, position: SidebarPosition) -> Option<u16> {
    let bounds = tab
        .panes
        .iter()
        .filter(|pane| !pane.is_floating)
        .filter_map(|pane| match position {
            SidebarPosition::Left => Some((pane.pane_x?, pane.pane_x? + pane.pane_columns?)),
            SidebarPosition::Top => Some((pane.pane_y?, pane.pane_y? + pane.pane_rows?)),
        });
    let (minimum, maximum) =
        bounds.fold(None::<(u16, u16)>, |bounds, (start, end)| match bounds {
            Some((minimum, maximum)) => Some((minimum.min(start), maximum.max(end))),
            None => Some((start, end)),
        })?;
    Some(maximum.saturating_sub(minimum))
}

fn pane_rect(pane: &ZellijSidebarPane) -> Option<PaneRect> {
    Some(PaneRect {
        x: pane.pane_x?,
        y: pane.pane_y?,
        width: pane.pane_columns?,
        height: pane.pane_rows?,
    })
}

fn bounds(rects: impl Iterator<Item = PaneRect>) -> Option<PaneRect> {
    rects.fold(None, |bounds, rect| match bounds {
        Some(bounds) => {
            let right = (bounds.x + bounds.width).max(rect.x + rect.width);
            let bottom = (bounds.y + bounds.height).max(rect.y + rect.height);
            let x = bounds.x.min(rect.x);
            let y = bounds.y.min(rect.y);
            Some(PaneRect {
                x,
                y,
                width: right - x,
                height: bottom - y,
            })
        }
        None => Some(rect),
    })
}

fn slot_cost(
    original: PaneRect,
    original_bounds: PaneRect,
    slot: PaneRect,
    slot_bounds: PaneRect,
) -> f64 {
    let (original_center_x, original_center_y) = original.center();
    let expected_center_x = f64::from(slot_bounds.x)
        + (original_center_x - f64::from(original_bounds.x)) * f64::from(slot_bounds.width)
            / f64::from(original_bounds.width);
    let expected_center_y = f64::from(slot_bounds.y)
        + (original_center_y - f64::from(original_bounds.y)) * f64::from(slot_bounds.height)
            / f64::from(original_bounds.height);
    let expected_width =
        f64::from(original.width) * f64::from(slot_bounds.width) / f64::from(original_bounds.width);
    let expected_height = f64::from(original.height) * f64::from(slot_bounds.height)
        / f64::from(original_bounds.height);
    let (slot_center_x, slot_center_y) = slot.center();
    (slot_center_x - expected_center_x).powi(2)
        + (slot_center_y - expected_center_y).powi(2)
        + (f64::from(slot.width) - expected_width).powi(2)
        + (f64::from(slot.height) - expected_height).powi(2)
}

fn target_slots(
    original: &std::collections::HashMap<String, PaneRect>,
    current: &std::collections::HashMap<String, PaneRect>,
) -> Result<std::collections::HashMap<String, PaneRect>> {
    let aligned = |panes: &std::collections::HashMap<String, PaneRect>| {
        panes.values().next().map(|first| {
            (
                panes
                    .values()
                    .all(|rect| rect.y == first.y && rect.height == first.height),
                panes
                    .values()
                    .all(|rect| rect.x == first.x && rect.width == first.width),
            )
        })
    };
    if let (Some((original_row, original_column)), Some((current_row, current_column))) =
        (aligned(original), aligned(current))
        && ((original_row && current_row) || (original_column && current_column))
        && original.len() == current.len()
    {
        // Reapplying bare slots can equalize unequal widths. Preserve order, not nearest centers.
        let mut panes: Vec<_> = original.iter().collect();
        panes.sort_by_key(|(_, rect)| (rect.x, rect.y));
        let mut slots: Vec<_> = current.values().copied().collect();
        slots.sort_by_key(|rect| (rect.x, rect.y));
        return Ok(panes
            .into_iter()
            .zip(slots)
            .map(|((id, _), slot)| (id.clone(), slot))
            .collect());
    }
    let original_bounds =
        bounds(original.values().copied()).ok_or_else(|| anyhow!("no original pane geometry"))?;
    let slot_bounds =
        bounds(current.values().copied()).ok_or_else(|| anyhow!("no current pane geometry"))?;
    let mut candidates = Vec::new();
    for (pane_id, original_rect) in original {
        for slot in current.values() {
            candidates.push((
                slot_cost(*original_rect, original_bounds, *slot, slot_bounds),
                pane_id.clone(),
                *slot,
            ));
        }
    }
    candidates.sort_by(|left, right| left.0.total_cmp(&right.0));

    let mut assigned_panes = HashSet::new();
    let mut assigned_slots = HashSet::new();
    let mut targets = std::collections::HashMap::new();
    for (_, pane_id, slot) in candidates {
        if assigned_panes.contains(&pane_id) || assigned_slots.contains(&slot) {
            continue;
        }
        assigned_panes.insert(pane_id.clone());
        assigned_slots.insert(slot);
        targets.insert(pane_id, slot);
    }
    if targets.len() != original.len() {
        bail!("could not map all retained Zellij panes to their original slots");
    }
    Ok(targets)
}

fn move_direction(current: PaneRect, target: PaneRect) -> Option<&'static str> {
    if current == target {
        return None;
    }
    let (current_x, current_y) = current.center();
    let (target_x, target_y) = target.center();
    let dx = target_x - current_x;
    let dy = target_y - current_y;
    if dx.abs() >= dy.abs() {
        Some(if dx < 0.0 { "left" } else { "right" })
    } else {
        Some(if dy < 0.0 { "up" } else { "down" })
    }
}

fn sidebar_size(config: &Config, position: SidebarPosition, extent: u16) -> KdlValue {
    let maximum = extent.saturating_sub(1).max(1);
    let resolved = match position {
        SidebarPosition::Left => match config.sidebar.width {
            Some(SidebarWidth::Absolute(width)) => width,
            Some(SidebarWidth::Percent(percent)) => extent.saturating_mul(percent) / 100,
            None => super::resolve_width_for(config, extent, None),
        },
        SidebarPosition::Top => match config.sidebar.height {
            Some(SidebarHeight::Absolute(height)) => height,
            Some(SidebarHeight::Percent(percent)) => extent.saturating_mul(percent) / 100,
            None => super::resolve_height_for(config, extent, None),
        },
    };
    let resolved = resolved.clamp(1, maximum);
    // Zellij adds the separator column to a fixed-width bordered pane.
    let layout_size = match position {
        SidebarPosition::Left => resolved.saturating_sub(1).max(1),
        SidebarPosition::Top => resolved,
    };
    KdlValue::Integer(layout_size.into())
}

fn is_zellij_ui_pane(node: &KdlNode) -> bool {
    node.children().is_some_and(|children| {
        children.nodes().iter().any(|child| {
            child.name().value() == "plugin"
                && child
                    .get("location")
                    .and_then(KdlValue::as_string)
                    .is_some_and(|location| {
                        matches!(
                            location,
                            "zellij:tab-bar" | "zellij:status-bar" | "tab-bar" | "status-bar"
                        )
                    })
        })
    })
}

fn property(node: &mut KdlNode, name: &str, value: impl Into<KdlValue>) {
    node.insert(name, KdlEntry::new(value));
}

fn remove_property(node: &mut KdlNode, name: &str) {
    node.entries_mut().retain(|entry| {
        entry
            .name()
            .is_none_or(|entry_name| entry_name.value() != name)
    });
}

fn sidebar_pane(size: KdlValue, executable: &str, cwd: &str) -> KdlNode {
    let mut pane = KdlNode::new("pane");
    property(&mut pane, "name", SIDEBAR_PANE_NAME);
    property(&mut pane, "size", size);
    property(&mut pane, "command", "env");
    property(&mut pane, "cwd", cwd);
    property(&mut pane, "close_on_exit", true);

    let mut args = KdlNode::new("args");
    args.push("WORKMUX_BACKEND=zellij");
    args.push(executable);
    args.push("_sidebar-run");
    let mut children = KdlDocument::new();
    children.nodes_mut().push(args);
    pane.set_children(children);
    pane
}

fn active_dumped_tab(layout: &KdlNode, tab_position: u32) -> Option<KdlNode> {
    let tabs: Vec<&KdlNode> = layout
        .children()?
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "tab")
        .collect();
    tabs.iter()
        .find(|tab| tab.get("focus").and_then(KdlValue::as_bool) == Some(true))
        .or_else(|| tabs.get(tab_position as usize))
        .map(|tab| (*tab).clone())
}

fn remove_existing_sidebar(node: &mut KdlNode) {
    let is_pane = node.name().value() == "pane";
    let Some(children) = node.children_mut() else {
        return;
    };
    let mut removed_directly = false;
    let mut retained = Vec::new();
    for mut child in std::mem::take(children.nodes_mut()) {
        if child.get("name").and_then(KdlValue::as_string) == Some(SIDEBAR_PANE_NAME) {
            removed_directly = true;
        } else {
            remove_existing_sidebar(&mut child);
            retained.push(child);
        }
    }
    *children.nodes_mut() = retained;
    let replacement = (removed_directly
        && is_pane
        && children.nodes().len() == 1
        && children.nodes()[0].name().value() == "pane")
        .then(|| children.nodes()[0].clone());
    if let Some(replacement) = replacement {
        *node = replacement;
    }
}

struct RetainedContent<'a> {
    placeholder_name: &'a str,
    ui_plugin_aliases: HashSet<String>,
}

fn prepare_retained_content(node: &mut KdlNode, retained: &RetainedContent<'_>) {
    if node.name().value() != "pane"
        || node
            .children()
            .is_some_and(|children| children.get("plugin").is_some())
    {
        return;
    }
    for property in ["command", "cwd", "name", "close_on_exit", "start_suspended"] {
        remove_property(node, property);
    }
    if let Some(children) = node.children_mut() {
        children
            .nodes_mut()
            .retain(|child| matches!(child.name().value(), "pane" | "children"));
        for child in children.nodes_mut() {
            prepare_retained_content(child, retained);
        }
        if children.nodes().is_empty() {
            *node.children_mut() = None;
        }
    }
    if node.children().is_none() {
        let placeholder_name = retained.placeholder_name;
        // Bare slots can spawn unmarked shells when their cwd differs from the original invocation.
        // Mark every temporary slot so cleanup cannot mistake a user pane for a placeholder.
        property(node, "name", placeholder_name);
        property(node, "command", "env");
        property(node, "start_suspended", true);
        property(node, "close_on_exit", true);
        let mut args = KdlNode::new("args");
        args.push(format!("WORKMUX_SIDEBAR_PLACEHOLDER={placeholder_name}"));
        args.push("true");
        let mut children = KdlDocument::new();
        children.nodes_mut().push(args);
        node.set_children(children);
    }
}

// Leaves lose their sizes: percentages from a dump rarely sum to 100 and Zellij then rejects the layout.
fn bare_slots(node: &KdlNode) -> KdlNode {
    let mut slot = KdlNode::new("pane");
    let Some(children) = node.children().filter(|children| {
        children
            .nodes()
            .iter()
            .any(|child| child.name().value() == "pane")
    }) else {
        return slot;
    };
    for property_name in ["split_direction", "size"] {
        if let Some(value) = node.get(property_name) {
            property(&mut slot, property_name, value.clone());
        }
    }
    let mut slot_children = KdlDocument::new();
    for child in children.nodes() {
        if child.name().value() == "pane" {
            slot_children.nodes_mut().push(bare_slots(child));
        }
    }
    slot.set_children(slot_children);
    slot
}

fn bare_content_container(content: &[KdlNode], split_direction: Option<KdlValue>) -> KdlNode {
    let mut slots: Vec<KdlNode> = content.iter().map(bare_slots).collect();
    if slots.len() == 1 {
        let mut node = slots.remove(0);
        remove_property(&mut node, "size");
        return node;
    }
    let mut container = KdlNode::new("pane");
    if let Some(direction) = split_direction {
        property(&mut container, "split_direction", direction);
    }
    let mut children = KdlDocument::new();
    children.nodes_mut().append(&mut slots);
    container.set_children(children);
    container
}

fn content_container(
    mut content: Vec<KdlNode>,
    split_direction: Option<KdlValue>,
    retained: &RetainedContent<'_>,
) -> KdlNode {
    for node in &mut content {
        prepare_retained_content(node, retained);
    }
    if content.len() == 1 {
        let mut node = content.remove(0);
        remove_property(&mut node, "size");
        return node;
    }
    let mut container = KdlNode::new("pane");
    if let Some(direction) = split_direction {
        property(&mut container, "split_direction", direction);
    }
    let mut children = KdlDocument::new();
    children.nodes_mut().append(&mut content);
    container.set_children(children);
    container
}

#[cfg(test)]
fn layout_with_sidebar(
    dumped_layout: &str,
    tab_position: u32,
    position: SidebarPosition,
    size: KdlValue,
    executable: &str,
    cwd: &str,
    retained: &RetainedContent<'_>,
) -> Result<String> {
    layout_for_tab(
        dumped_layout,
        tab_position,
        Some((position, sidebar_pane(size, executable, cwd))),
        retained,
    )
}

fn layout_for_tab(
    dumped_layout: &str,
    tab_position: u32,
    sidebar: Option<(SidebarPosition, KdlNode)>,
    retained: &RetainedContent<'_>,
) -> Result<String> {
    let document =
        KdlDocument::parse_v1(dumped_layout).context("failed to parse dumped Zellij layout")?;
    let layout = document
        .get("layout")
        .ok_or_else(|| anyhow!("dumped Zellij layout has no layout node"))?;
    let mut tab = active_dumped_tab(layout, tab_position)
        .ok_or_else(|| anyhow!("dumped Zellij layout has no target tab"))?;
    remove_existing_sidebar(&mut tab);
    let original_direction = tab.get("split_direction").cloned();
    let original_children = tab
        .children_mut()
        .take()
        .ok_or_else(|| anyhow!("dumped Zellij tab has no panes"))?
        .nodes()
        .to_vec();
    let mut content = Vec::new();
    for node in &original_children {
        if node.name().value() == "pane" && !is_zellij_ui_pane(node) {
            content.push(node.clone());
        }
    }
    if content.is_empty() {
        bail!("dumped Zellij tab has no terminal content panes");
    }

    let mut swap_wrapper = bare_content_container(&content, original_direction.clone());
    let mut wrapper = content_container(content, original_direction, retained);
    if let Some((position, sidebar)) = sidebar {
        let wrap = |content: KdlNode, sidebar: KdlNode| {
            let mut node = KdlNode::new("pane");
            property(
                &mut node,
                "split_direction",
                match position {
                    SidebarPosition::Left => "vertical",
                    SidebarPosition::Top => "horizontal",
                },
            );
            let mut children = KdlDocument::new();
            children.nodes_mut().extend([sidebar, content]);
            node.set_children(children);
            node
        };
        wrapper = wrap(wrapper, sidebar.clone());
        swap_wrapper = wrap(swap_wrapper, sidebar);
    }

    let mut replacement_children = KdlDocument::new();
    let mut swap_children = KdlDocument::new();
    let mut inserted = false;
    for mut node in original_children {
        if node.name().value() == "floating_panes" {
            // Floating and tiled panes are matched independently by Zellij.
            if let Some(children) = node.children_mut() {
                for pane in children.nodes_mut() {
                    prepare_retained_content(pane, retained);
                }
            }
            replacement_children.nodes_mut().push(node);
        } else if is_zellij_ui_pane(&node) {
            // dump-layout expands aliases, but Zellij matches plugins by their original invocation.
            if let Some(plugin) = node
                .children_mut()
                .as_mut()
                .and_then(|c| c.get_mut("plugin"))
                && let Some(alias) = plugin
                    .get("location")
                    .and_then(KdlValue::as_string)
                    .and_then(|location| location.strip_prefix("zellij:"))
                    .filter(|alias| retained.ui_plugin_aliases.contains(*alias))
                    .map(str::to_owned)
            {
                property(plugin, "location", alias);
            }
            swap_children.nodes_mut().push(node.clone());
            replacement_children.nodes_mut().push(node);
        } else if node.name().value() != "pane" {
            replacement_children.nodes_mut().push(node);
        } else if !inserted {
            replacement_children.nodes_mut().push(wrapper.clone());
            swap_children.nodes_mut().push(swap_wrapper.clone());
            inserted = true;
        }
    }
    tab.set_children(replacement_children);

    let mut root = KdlNode::new("layout");
    let mut root_children = KdlDocument::new();
    root_children.nodes_mut().push(tab);
    // Re-seat retained panes only after every temporary slot has been removed.
    let mut swap_tab = KdlNode::new("tab");
    swap_tab.set_children(swap_children);
    let mut swap_layout = KdlNode::new("swap_tiled_layout");
    property(&mut swap_layout, "name", SIDEBAR_PANE_NAME);
    let mut swap_layout_children = KdlDocument::new();
    swap_layout_children.nodes_mut().push(swap_tab);
    swap_layout.set_children(swap_layout_children);
    root_children.nodes_mut().push(swap_layout);
    root.set_children(root_children);
    let mut output = KdlDocument::new();
    output.nodes_mut().push(root);
    output.autoformat_no_comments();
    output.ensure_v1();
    Ok(output.to_string())
}

fn close_generated_terminal_placeholders(
    mux: &ZellijBackend,
    tab: &ZellijSidebarTab,
    original_pane_ids: &HashSet<String>,
    placeholder_name: &str,
) -> Result<()> {
    for pane in &tab.panes {
        let is_placeholder = pane.title == placeholder_name
            || pane
                .command
                .as_deref()
                .is_some_and(|command| command.contains(placeholder_name));
        if !original_pane_ids.contains(&pane.pane_id) && is_placeholder {
            mux.close_pane_by_id(&pane.pane_id)?;
        }
    }
    Ok(())
}

struct SidebarUpdate<'a> {
    tab: &'a ZellijSidebarTab,
    placeholder_name: String,
    focus_pane_id: &'a str,
    select_tab: bool,
    slot_count: usize,
}

fn restore_pane_slots(
    instance_id: &str,
    tab_id: u32,
    original: &std::collections::HashMap<String, PaneRect>,
    panes: &[ZellijSidebarPane],
) -> Result<()> {
    if original.len() < 2 {
        return Ok(());
    }
    let mux = backend(instance_id);
    let geometry = |panes: &[ZellijSidebarPane]| -> std::collections::HashMap<String, PaneRect> {
        panes
            .iter()
            .filter(|pane| original.contains_key(&pane.pane_id))
            .filter_map(|pane| pane_rect(pane).map(|rect| (pane.pane_id.clone(), rect)))
            .collect()
    };
    let mut current = geometry(panes);
    if current.len() != original.len() {
        bail!("Zellij did not report geometry for all retained panes in tab {tab_id}");
    }
    let targets = target_slots(original, &current)?;
    let mut ordered_targets: Vec<_> = targets.iter().collect();
    let first = ordered_targets[0].1;
    let aligned = targets
        .values()
        .all(|rect| rect.y == first.y && rect.height == first.height)
        || targets
            .values()
            .all(|rect| rect.x == first.x && rect.width == first.width);
    if aligned {
        // Finish the leading slots first; otherwise fixing a later slot can undo the previous move.
        ordered_targets.sort_by_key(|(_, rect)| (rect.x, rect.y));
    } else {
        ordered_targets.sort_by_key(|(_, rect)| (std::cmp::Reverse(rect.area()), rect.x, rect.y));
    }

    for _ in 0..original.len().saturating_mul(original.len()).max(1) {
        let Some((pane_id, target, rect, direction)) =
            ordered_targets.iter().find_map(|(pane_id, target)| {
                let rect = current.get(*pane_id)?;
                move_direction(*rect, **target)
                    .map(|direction| ((*pane_id).clone(), **target, *rect, direction))
            })
        else {
            return Ok(());
        };
        mux.move_pane_by_id(&pane_id, direction)?;
        current = geometry(&mux.sidebar_tab_panes(tab_id)?);
        if current.len() != original.len() {
            bail!("Zellij lost retained panes while restoring pane order in tab {tab_id}");
        }
        if current.get(&pane_id) == Some(&rect) {
            bail!(
                "Zellij could not move pane {pane_id} {direction} toward its original slot {target:?}"
            );
        }
    }
    bail!("Zellij pane order did not converge after layout override")
}

fn create_sidebar_in_tab(
    instance_id: &str,
    tab: &ZellijSidebarTab,
    position: SidebarPosition,
    config: &Config,
    replace_existing: bool,
    select_tab: bool,
) -> Result<()> {
    if let Some(update) = apply_tab_layout(
        instance_id,
        tab,
        Some((position, config)),
        replace_existing,
        select_tab,
    )? {
        finish_sidebar_updates(instance_id, &[update])?;
    }
    Ok(())
}

fn apply_tab_layout<'a>(
    instance_id: &str,
    tab: &'a ZellijSidebarTab,
    sidebar: Option<(SidebarPosition, &Config)>,
    replace_existing: bool,
    select_tab: bool,
) -> Result<Option<SidebarUpdate<'a>>> {
    let mux = backend(instance_id);
    let has_sidebar = tab.panes.iter().any(|pane| is_sidebar_pane(&pane.title));
    if (sidebar.is_some() && has_sidebar && !replace_existing)
        || (sidebar.is_none() && !has_sidebar)
    {
        return Ok(None);
    }
    let Some(target) = tab
        .panes
        .iter()
        .find(|pane| !is_sidebar_pane(&pane.title) && pane.is_focused)
        .or_else(|| tab.panes.iter().find(|pane| !is_sidebar_pane(&pane.title)))
    else {
        return Ok(None);
    };
    let sidebar = if let Some((position, config)) = sidebar {
        let extent = tab_extent(tab, position)
            .ok_or_else(|| anyhow!("Zellij tab {} has no pane geometry", tab.tab_id))?;
        let size = sidebar_size(config, position, extent);
        let executable = std::env::current_exe()?;
        let executable = executable
            .to_str()
            .ok_or_else(|| anyhow!("exe path not UTF-8"))?;
        let cwd = std::env::current_dir()?;
        let cwd = cwd
            .to_str()
            .ok_or_else(|| anyhow!("current directory path not UTF-8"))?;
        Some((position, sidebar_pane(size, executable, cwd)))
    } else {
        None
    };
    debug!(tab_id = tab.tab_id, "overriding Zellij tab layout");
    if select_tab {
        mux.go_to_tab_by_id(tab.tab_id)?;
    } else if !tab.active {
        return Ok(None);
    }
    let dumped = mux.dump_layout()?;
    let placeholder_name = format!("workmux-sidebar-placeholder-{:016x}", getrandom::u64()?);
    let layout = layout_for_tab(
        &dumped,
        tab.position,
        sidebar,
        &RetainedContent {
            placeholder_name: &placeholder_name,
            ui_plugin_aliases: tab.ui_plugin_aliases.clone(),
        },
    )?;
    for pane in tab.panes.iter().filter(|pane| is_sidebar_pane(&pane.title)) {
        mux.close_pane_by_id(&pane.pane_id)?;
    }
    mux.override_active_tab_layout(&layout)
        .with_context(|| format!("failed to update sidebar in Zellij tab {}", tab.tab_id))?;
    Ok(Some(SidebarUpdate {
        tab,
        placeholder_name,
        focus_pane_id: &target.pane_id,
        select_tab,
        slot_count: layout.matches("WORKMUX_SIDEBAR_PLACEHOLDER=").count(),
    }))
}

fn finish_sidebar_updates(instance_id: &str, updates: &[SidebarUpdate<'_>]) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let mux = backend(instance_id);
    // Overrides acknowledge before spawning panes. Wait for every slot before cleanup.
    // Snapshots come from the cheap pane table, which lacks commands. Use the full
    // query only if a new pane cannot be identified by its title.
    let started = std::time::Instant::now();
    let tabs = loop {
        let cheap = started.elapsed() < std::time::Duration::from_secs(1);
        let tabs = if cheap {
            mux.sidebar_layout_tabs()?
        } else {
            mux.sidebar_tabs()?
        };
        let mut pending = Vec::new();
        let mut unidentified = false;
        for update in updates {
            let tab = tabs
                .iter()
                .find(|tab| tab.tab_id == update.tab.tab_id)
                .ok_or_else(|| anyhow!("Zellij tab {} disappeared", update.tab.tab_id))?;
            let is_slot = |pane: &ZellijSidebarPane| {
                pane.title == update.placeholder_name
                    || pane
                        .command
                        .as_deref()
                        .is_some_and(|command| command.contains(&update.placeholder_name))
            };
            if tab.panes.iter().filter(|pane| is_slot(pane)).count() < update.slot_count {
                pending.push(tab.tab_id);
            }
            unidentified |= cheap
                && tab.panes.iter().any(|pane| {
                    !is_slot(pane)
                        && !is_sidebar_pane(&pane.title)
                        && !update
                            .tab
                            .panes
                            .iter()
                            .any(|original| original.pane_id == pane.pane_id)
                });
        }
        if pending.is_empty() {
            break if unidentified {
                mux.sidebar_tabs()?
            } else {
                tabs
            };
        }
        if started.elapsed() >= std::time::Duration::from_secs(5) {
            bail!("Zellij did not finish creating layout slots in tabs {pending:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut errors = Vec::new();
    let mut ready = Vec::new();
    for update in updates {
        let result = (|| -> Result<()> {
            let tab = tabs
                .iter()
                .find(|tab| tab.tab_id == update.tab.tab_id)
                .ok_or_else(|| anyhow!("Zellij tab {} disappeared", update.tab.tab_id))?;
            let original_ids = update
                .tab
                .panes
                .iter()
                .map(|pane| pane.pane_id.clone())
                .collect();
            close_generated_terminal_placeholders(
                &mux,
                tab,
                &original_ids,
                &update.placeholder_name,
            )?;
            if update.select_tab {
                mux.go_to_tab_by_id(tab.tab_id)?;
            }
            mux.reapply_active_tab_layout()?;
            for pane in update.tab.panes.iter().filter(|pane| pane.is_floating) {
                if let Some(rect) = pane_rect(pane) {
                    mux.set_floating_pane_coordinates(
                        &pane.pane_id,
                        rect.x,
                        rect.y,
                        rect.width,
                        rect.height,
                    )?;
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => ready.push(update),
            Err(error) => errors.push(format!("tab {}: {error}", update.tab.tab_id)),
        }
    }
    let tabs = mux.sidebar_layout_tabs()?;
    for update in ready {
        let result = (|| -> Result<()> {
            let tab = tabs
                .iter()
                .find(|tab| tab.tab_id == update.tab.tab_id)
                .ok_or_else(|| anyhow!("Zellij tab {} disappeared", update.tab.tab_id))?;
            for pane in &tab.panes {
                // Zellij can apply temporary slot names to retained shells.
                if (pane.title == update.placeholder_name || is_sidebar_pane(&pane.title))
                    && let Some(original) = update
                        .tab
                        .panes
                        .iter()
                        .find(|original| original.pane_id == pane.pane_id)
                    && !is_sidebar_pane(&original.title)
                {
                    mux.set_pane_name(&pane.pane_id, &original.title)?;
                }
            }
            let original_rects = update
                .tab
                .panes
                .iter()
                .filter(|pane| !is_sidebar_pane(&pane.title) && !pane.is_floating)
                .filter_map(|pane| pane_rect(pane).map(|rect| (pane.pane_id.clone(), rect)))
                .collect();
            restore_pane_slots(instance_id, tab.tab_id, &original_rects, &tab.panes)?;
            mux.focus_pane_by_id(update.focus_pane_id)?;
            Ok(())
        })();
        if let Err(error) = result {
            errors.push(format!("tab {}: {error}", update.tab.tab_id));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

pub(super) fn create_sidebars_in_all_tabs(
    instance_id: &str,
    position: SidebarPosition,
    resize_existing: bool,
) -> Result<()> {
    let state = super::session_state::read(instance_id)?;
    let mut config = Config::load(None)?;
    config.sidebar.width = state.width;
    config.sidebar.height = state.height;
    update_all_tabs(instance_id, Some((position, &config)), resize_existing)
}

fn update_all_tabs(
    instance_id: &str,
    sidebar: Option<(SidebarPosition, &Config)>,
    replace_existing: bool,
) -> Result<()> {
    let _layout_lock = super::session_state::lock_layout(instance_id)?;
    let tabs = backend(instance_id).sidebar_tabs()?;
    let original_tab = tabs.iter().find(|tab| tab.active).map(|tab| tab.tab_id);
    let mut errors = Vec::new();
    let mut updates = Vec::new();
    for tab in &tabs {
        match apply_tab_layout(instance_id, tab, sidebar, replace_existing, true) {
            Ok(Some(update)) => updates.push(update),
            Ok(None) => {}
            Err(error) => errors.push(format!("tab {}: {error}", tab.tab_id)),
        }
    }
    if let Err(error) = finish_sidebar_updates(instance_id, &updates) {
        errors.push(error.to_string());
    }
    if let Some(tab_id) = original_tab
        && let Err(error) = backend(instance_id).go_to_tab_by_id(tab_id)
    {
        errors.push(format!("failed to restore active tab {tab_id}: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to update Zellij sidebars: {}",
            errors.join("; ")
        ))
    }
}

pub(super) fn create_sidebar_in_active_tab(
    instance_id: &str,
    position: SidebarPosition,
) -> Result<()> {
    let _layout_lock = super::session_state::lock_layout(instance_id)?;
    let state = super::session_state::read(instance_id)?;
    let mut config = Config::load(None)?;
    config.sidebar.width = state.width;
    config.sidebar.height = state.height;
    let Some(tab) = backend(instance_id)
        .sidebar_tabs()?
        .into_iter()
        .find(|tab| tab.active)
    else {
        return Ok(());
    };
    create_sidebar_in_tab(instance_id, &tab, position, &config, false, false)
}

pub(super) fn create_sidebar_in_active_tab_from_snapshot(
    instance_id: &str,
    position: SidebarPosition,
    tabs: &[ZellijSidebarTab],
) -> Result<()> {
    let Some(tab) = tabs.iter().find(|tab| tab.active) else {
        return Ok(());
    };
    if tab.panes.iter().any(|pane| is_sidebar_pane(&pane.title)) {
        return Ok(());
    }
    let Some(_layout_lock) = super::session_state::try_lock_layout(instance_id)? else {
        return Ok(());
    };
    let state = super::session_state::read(instance_id)?;
    if !state.enabled {
        return Ok(());
    }
    let Some(tab) = backend(instance_id)
        .sidebar_tabs()?
        .into_iter()
        .find(|current| current.tab_id == tab.tab_id && current.active)
    else {
        return Ok(());
    };
    let mut config = Config::load(None)?;
    config.sidebar.width = state.width;
    config.sidebar.height = state.height;
    create_sidebar_in_tab(instance_id, &tab, position, &config, false, false)
}

pub(super) fn current_tab_has_sidebar(instance_id: &str) -> Result<bool> {
    Ok(backend(instance_id)
        .sidebar_tabs()?
        .into_iter()
        .find(|tab| tab.active)
        .is_some_and(|tab| tab.panes.iter().any(|pane| is_sidebar_pane(&pane.title))))
}

pub(super) fn kill_all_sidebars(instance_id: &str) -> Result<()> {
    update_all_tabs(instance_id, None, true)
}

pub(super) fn sidebar_is_only_pane(instance_id: &str, window_id: &str, pane_id: &str) -> bool {
    backend(instance_id)
        .sidebar_layout_tabs()
        .is_ok_and(|tabs| {
            tabs.into_iter()
                .find(|tab| tab.tab_id.to_string() == window_id)
                .is_some_and(|tab| tab.panes.len() == 1 && tab.panes[0].pane_id == pane_id)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retained_content() -> RetainedContent<'static> {
        RetainedContent {
            placeholder_name: "workmux-sidebar-placeholder-test",
            ui_plugin_aliases: HashSet::new(),
        }
    }

    #[test]
    fn active_sidebar_snapshot_skips_another_zellij_query() {
        let tabs = [ZellijSidebarTab {
            tab_id: 1,
            position: 0,
            active: true,
            panes: vec![ZellijSidebarPane {
                pane_id: "terminal_1".to_string(),
                title: SIDEBAR_PANE_NAME.to_string(),
                command: None,
                is_focused: true,
                is_floating: false,
                pane_x: Some(0),
                pane_y: Some(0),
                pane_columns: Some(24),
                pane_rows: Some(40),
            }],
            ui_plugin_aliases: HashSet::new(),
        }];

        create_sidebar_in_active_tab_from_snapshot(
            "no-zellij-process-should-be-started",
            SidebarPosition::Left,
            &tabs,
        )
        .unwrap();
    }

    #[test]
    fn marks_both_command_and_shell_slots_for_cleanup() {
        let retained = retained_content();
        let output = layout_with_sidebar(
            "layout { tab { pane split_direction=\"vertical\" { pane; pane command=\"sleep\"; }; }; }",
            0,
            SidebarPosition::Left,
            KdlValue::Integer(24),
            "/tmp/workmux",
            "/tmp",
            &retained,
        )
        .unwrap();
        assert_eq!(
            output.matches("WORKMUX_SIDEBAR_PLACEHOLDER=").count(),
            2,
            "{output}"
        );
        assert!(output.contains("swap_tiled_layout"), "{output}");
    }

    #[test]
    fn sidebar_command_pins_backend_and_closes_on_exit() {
        let pane = sidebar_pane(KdlValue::Integer(24), "/tmp/workmux", "/tmp");
        assert_eq!(pane.get("close_on_exit"), Some(&KdlValue::Bool(true)));
        assert_eq!(
            pane.get("command").and_then(KdlValue::as_string),
            Some("env")
        );
        let args = pane.children().unwrap().get("args").unwrap();
        let args: Vec<_> = args
            .entries()
            .iter()
            .map(|entry| entry.value().as_string().unwrap())
            .collect();
        assert_eq!(
            args,
            ["WORKMUX_BACKEND=zellij", "/tmp/workmux", "_sidebar-run"]
        );
    }

    #[test]
    fn marks_relative_cwd_shell_slots_and_preserves_plugin_aliases() {
        let dumped = r#"
            layout {
                cwd "/original"
                tab {
                    pane size=1 borderless=true { plugin location="zellij:tab-bar"; }
                    pane cwd="sub"
                    pane size=1 borderless=true { plugin location="zellij:status-bar"; }
                }
            }
        "#;
        let output = layout_with_sidebar(
            dumped,
            0,
            SidebarPosition::Left,
            KdlValue::Integer(24),
            "/tmp/workmux",
            "/other",
            &RetainedContent {
                ui_plugin_aliases: HashSet::from(["tab-bar".into(), "status-bar".into()]),
                ..retained_content()
            },
        )
        .unwrap();
        assert!(output.contains("WORKMUX_SIDEBAR_PLACEHOLDER="), "{output}");
        assert!(!output.contains("cwd=\"sub\""), "{output}");
        assert!(!output.contains("cwd \"/original\""), "{output}");
        assert!(output.contains("plugin location=\"tab-bar\""), "{output}");
        assert!(
            output.contains("plugin location=\"status-bar\""),
            "{output}"
        );
        assert!(!output.contains("zellij:"), "{output}");
    }

    #[test]
    fn preserves_floating_panes_at_tab_level() {
        let dumped = r#"
            layout {
                tab focus=true {
                    pane
                    floating_panes {
                        pane x=5 y=3 width=40 height=10 command="bash" cwd="/tmp"
                    }
                }
            }
        "#;
        let output = layout_with_sidebar(
            dumped,
            0,
            SidebarPosition::Left,
            KdlValue::Integer(24),
            "/tmp/workmux",
            "/tmp",
            &retained_content(),
        )
        .unwrap();
        let document = KdlDocument::parse_v1(&output).unwrap();
        let tab = document
            .get("layout")
            .unwrap()
            .children()
            .unwrap()
            .get("tab")
            .unwrap();
        let floating = tab
            .children()
            .unwrap()
            .get("floating_panes")
            .expect(&output);
        let pane = &floating.children().unwrap().nodes()[0];
        assert_eq!(pane.get("x"), Some(&KdlValue::Integer(5)));
        assert_eq!(pane.get("width"), Some(&KdlValue::Integer(40)));
    }

    #[test]
    fn recognizes_only_the_reserved_pane_name() {
        assert!(is_sidebar_pane("workmux-sidebar"));
        assert!(!is_sidebar_pane("workmux sidebar"));
        assert!(!is_sidebar_pane("agent"));
    }

    #[test]
    fn fixed_sidebar_size_accounts_for_zellij_separator() {
        let mut config = Config::default();
        config.sidebar.width = Some(SidebarWidth::Absolute(24));
        config.sidebar.height = Some(SidebarHeight::Absolute(4));
        assert_eq!(
            sidebar_size(&config, SidebarPosition::Left, 80),
            KdlValue::Integer(23)
        );
        assert_eq!(
            sidebar_size(&config, SidebarPosition::Top, 22),
            KdlValue::Integer(4)
        );
    }

    #[test]
    fn maps_scaled_slots_back_to_original_panes() {
        let original = std::collections::HashMap::from([
            (
                "left".to_string(),
                PaneRect {
                    x: 0,
                    y: 1,
                    width: 40,
                    height: 22,
                },
            ),
            (
                "right-top".to_string(),
                PaneRect {
                    x: 40,
                    y: 1,
                    width: 40,
                    height: 11,
                },
            ),
            (
                "right-bottom".to_string(),
                PaneRect {
                    x: 40,
                    y: 12,
                    width: 40,
                    height: 11,
                },
            ),
        ]);
        let current = std::collections::HashMap::from([
            (
                "right-top".to_string(),
                PaneRect {
                    x: 24,
                    y: 1,
                    width: 28,
                    height: 22,
                },
            ),
            (
                "right-bottom".to_string(),
                PaneRect {
                    x: 52,
                    y: 1,
                    width: 28,
                    height: 11,
                },
            ),
            (
                "left".to_string(),
                PaneRect {
                    x: 52,
                    y: 12,
                    width: 28,
                    height: 11,
                },
            ),
        ]);
        let targets = target_slots(&original, &current).unwrap();
        assert_eq!(targets["left"], current["right-top"]);
        assert_eq!(targets["right-top"], current["right-bottom"]);
        assert_eq!(targets["right-bottom"], current["left"]);
    }

    #[test]
    fn maps_unequal_row_by_order_when_slots_are_equalized() {
        let original = std::collections::HashMap::from(
            [(0, 80), (80, 40), (120, 20), (140, 20)].map(|(x, width)| {
                (
                    x.to_string(),
                    PaneRect {
                        x,
                        y: 1,
                        width,
                        height: 43,
                    },
                )
            }),
        );
        let current = std::collections::HashMap::from(
            [(0, 126, 34), (80, 23, 35), (120, 58, 34), (140, 92, 34)].map(|(id, x, width)| {
                (
                    id.to_string(),
                    PaneRect {
                        x,
                        y: 1,
                        width,
                        height: 43,
                    },
                )
            }),
        );
        let targets = target_slots(&original, &current).unwrap();
        assert_eq!(targets["0"].x, 23);
        assert_eq!(targets["80"].x, 58);
        assert_eq!(targets["120"].x, 92);
        assert_eq!(targets["140"].x, 126);
    }

    #[test]
    fn chooses_direction_toward_target_slot() {
        let left = PaneRect {
            x: 24,
            y: 1,
            width: 28,
            height: 22,
        };
        let top_right = PaneRect {
            x: 52,
            y: 1,
            width: 28,
            height: 11,
        };
        let bottom_right = PaneRect {
            x: 52,
            y: 12,
            width: 28,
            height: 11,
        };
        assert_eq!(move_direction(bottom_right, left), Some("left"));
        assert_eq!(move_direction(bottom_right, top_right), Some("up"));
        assert_eq!(move_direction(left, left), None);
    }

    #[test]
    fn replacing_sidebar_preserves_existing_content_ratio() {
        let dumped = r#"
            layout {
                tab name="one" focus=true {
                    pane size=1 borderless=true {
                        plugin location="zellij:tab-bar"
                    }
                    pane split_direction="vertical" {
                        pane command="/tmp/workmux" name="workmux-sidebar" size=31 {
                            args "_sidebar-run"
                            start_suspended true
                        }
                        pane command="copilot" size="25%" {
                            args "--resume"
                            start_suspended true
                        }
                        pane command="/bin/zsh" size="75%" {
                            args "-l"
                            start_suspended true
                        }
                    }
                    pane size=1 borderless=true {
                        plugin location="zellij:status-bar"
                    }
                }
            }
        "#;

        let first_output = layout_with_sidebar(
            dumped,
            0,
            SidebarPosition::Left,
            KdlValue::Integer(30),
            "/tmp/workmux",
            "/tmp",
            &retained_content(),
        )
        .unwrap();
        let output = layout_with_sidebar(
            &first_output,
            0,
            SidebarPosition::Left,
            KdlValue::Integer(30),
            "/tmp/workmux",
            "/tmp",
            &retained_content(),
        )
        .unwrap();
        let document = KdlDocument::parse_v1(&output).unwrap();
        let tab = document
            .get("layout")
            .and_then(KdlNode::children)
            .and_then(|children| children.get("tab"))
            .unwrap();
        let wrapper = &tab.children().unwrap().nodes()[1];
        let panes = wrapper.children().unwrap().nodes();
        assert_eq!(
            panes
                .iter()
                .filter(|pane| {
                    pane.get("name").and_then(KdlValue::as_string) == Some(SIDEBAR_PANE_NAME)
                })
                .count(),
            1,
            "{output}"
        );
        let content_panes = panes[1].children().unwrap().nodes();
        assert_eq!(
            content_panes[0].get("size"),
            Some(&KdlValue::String("25%".into())),
            "{output}"
        );
        assert_eq!(
            content_panes[1].get("size"),
            Some(&KdlValue::String("75%".into())),
            "{output}"
        );
        let root_panes = tab.children().unwrap().nodes();
        assert_eq!(root_panes.len(), 3, "{output}");
        assert!(is_zellij_ui_pane(&root_panes[0]), "{output}");
        assert!(is_zellij_ui_pane(&root_panes[2]), "{output}");
        assert_eq!(root_panes[2].get("size"), Some(&KdlValue::Integer(1)));
        assert_eq!(root_panes[2].get("borderless"), Some(&KdlValue::Bool(true)));
    }

    #[test]
    fn wraps_nested_content_without_flattening_it() {
        let dumped = r#"
            layout {
                tab name="one" focus=true split_direction="horizontal" {
                    pane size=1 borderless=true {
                        plugin location="zellij:tab-bar"
                    }
                    pane split_direction="vertical" {
                        pane size="70%"
                        pane command="sleep" cwd="/tmp" size="30%" {
                            args "300"
                            start_suspended true
                        }
                    }
                    pane size=2 borderless=true {
                        plugin location="zellij:status-bar"
                    }
                }
            }
        "#;
        let output = layout_with_sidebar(
            dumped,
            0,
            SidebarPosition::Left,
            KdlValue::Integer(24),
            "/tmp/workmux",
            "/tmp",
            &retained_content(),
        )
        .unwrap();
        let document = KdlDocument::parse_v1(&output).unwrap();
        let tab = document
            .get("layout")
            .and_then(KdlNode::children)
            .and_then(|children| children.get("tab"))
            .unwrap();
        let children = tab.children().unwrap().nodes();
        assert_eq!(children.len(), 3);
        let wrapper = &children[1];
        assert_eq!(
            wrapper.get("split_direction").and_then(KdlValue::as_string),
            Some("vertical")
        );
        let wrapper_children = wrapper.children().unwrap().nodes();
        assert_eq!(
            wrapper_children[0].get("size"),
            Some(&KdlValue::Integer(24))
        );
        assert_eq!(
            wrapper_children[1]
                .get("split_direction")
                .and_then(KdlValue::as_string),
            Some("vertical")
        );
        assert_eq!(wrapper_children[1].children().unwrap().nodes().len(), 2);
        let retained_command = &wrapper_children[1].children().unwrap().nodes()[1];
        assert_eq!(
            retained_command
                .get("command")
                .and_then(KdlValue::as_string),
            Some("env"),
            "{output}"
        );
        assert!(retained_command.get("cwd").is_none(), "{output}");
        assert_eq!(
            retained_command.get("start_suspended"),
            Some(&KdlValue::Bool(true)),
            "{output}"
        );
        assert!(!output.contains("300"), "{output}");
    }

    #[test]
    fn selects_active_tab_when_names_are_duplicated() {
        let dumped = r#"
            layout {
                tab name="same" {
                    pane
                }
                tab name="same" focus=true {
                    pane split_direction="vertical" {
                        pane
                        pane
                    }
                }
            }
        "#;
        let output = layout_with_sidebar(
            dumped,
            0,
            SidebarPosition::Top,
            KdlValue::Integer(4),
            "/tmp/workmux",
            "/tmp",
            &retained_content(),
        )
        .unwrap();
        let document = KdlDocument::parse_v1(&output).unwrap();
        let tab = document
            .get("layout")
            .and_then(KdlNode::children)
            .and_then(|children| children.get("tab"))
            .unwrap();
        let wrapper = &tab.children().unwrap().nodes()[0];
        assert_eq!(
            wrapper.get("split_direction").and_then(KdlValue::as_string),
            Some("horizontal")
        );
        assert_eq!(
            wrapper.children().unwrap().nodes()[1]
                .get("split_direction")
                .and_then(KdlValue::as_string),
            Some("vertical")
        );
    }
}
