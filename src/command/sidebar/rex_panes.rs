//! Sidebar placement on Rex.
//!
//! Rex has no hooks, so the sidebar daemon reconciles every window against
//! the scope on each observation: it places a sidebar where the scope wants
//! one, closes sidebars outside it, and keeps each sidebar at its size. A
//! sidebar wraps the window's whole tiled tree, so it is the `before` half
//! of the root split.
//!
//! A block labeled `workmux-sidebar` belongs to workmux only when its
//! process is this workmux binary or the daemon placed it. Other labeled
//! blocks are never touched.

use anyhow::{Context, Result, anyhow};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use crate::config::{Config, SidebarPosition};
use crate::multiplexer::LivePaneInfo;
use crate::multiplexer::rex::RexBackend;
use crate::multiplexer::rex::sidebar::{
    LastBlockRefused, LayoutNode, RexSidebarObservation, RexSidebarWindow, SIDEBAR_LABEL,
    exec_layout,
};

use super::session_state::{self, Placement, SidebarSessionState};

/// Columns a left sidebar always leaves for content, and rows for a top one.
const MIN_CONTENT_COLS: f64 = 10.0;
const MIN_CONTENT_ROWS: f64 = 3.0;

/// Window extents assumed before any client has displayed a window.
const ASSUMED_WINDOW_COLS: u16 = 200;
const ASSUMED_WINDOW_ROWS: u16 = 50;

/// The workmux binary, resolved so its file name matches the name Rex
/// reports for the process.
fn exe_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to locate the workmux binary")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

fn exe_name(exe: &std::path::Path) -> String {
    exe.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A labeled sidebar block in a window.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    block_id: String,
    owned: bool,
    /// Whether the block is the `before` half of the root split, split the
    /// way the position wants.
    at_edge: bool,
}

/// One window as the planner sees it.
#[derive(Debug, Clone)]
struct WindowView {
    /// The ID that addresses the window's whole tiled tree.
    anchor: Option<String>,
    candidates: Vec<Candidate>,
    content_blocks: Vec<String>,
    /// False when a labeled block's process could not be read, so ownership
    /// is unknown.
    complete: bool,
    topology: u64,
    /// The root split ratio and the first block after the sidebar, when an
    /// owned sidebar sits at the edge.
    root: Option<(f64, String)>,
    /// The window extent along the sidebar's axis, once a client displays it.
    window_cells: Option<u16>,
}

fn topology(window: &RexSidebarWindow) -> u64 {
    let mut ids: Vec<&str> = window.blocks.iter().map(|b| b.block_id.as_str()).collect();
    ids.sort_unstable();
    let mut hasher = DefaultHasher::new();
    ids.hash(&mut hasher);
    hasher.finish()
}

fn split_matches(horizontal: bool, position: SidebarPosition) -> bool {
    horizontal == (position == SidebarPosition::Left)
}

/// The window extent in cells, from a block's extent and its share of the
/// window along the same axis.
fn window_extent(block_cells: u16, share: f64) -> Option<u16> {
    if block_cells == 0 || share.is_nan() || share <= 0.0 {
        return None;
    }
    let cells = (f64::from(block_cells) / share).round();
    (cells >= 1.0 && cells <= f64::from(u16::MAX)).then_some(cells as u16)
}

/// Each displayed block's size stamp.
fn size_stamps(window: &RexSidebarWindow) -> BTreeMap<String, String> {
    window
        .blocks
        .iter()
        .filter_map(|block| {
            let size = block.size.as_ref().filter(|size| size.is_measured())?;
            Some((block.block_id.clone(), size.stamp()))
        })
        .collect()
}

/// The window extent along the sidebar's axis, from a block a client
/// displays at the current layout, preferring the sidebar's own block.
///
/// A hidden window keeps the sizes it was last shown at, so a block whose
/// stamp is in `stale` measures a layout that no longer exists.
fn measured_extent(
    window: &RexSidebarWindow,
    layout: &LayoutNode,
    position: SidebarPosition,
    stale: Option<&BTreeMap<String, String>>,
) -> Option<u16> {
    let (sidebars, others): (Vec<_>, Vec<_>) = window
        .blocks
        .iter()
        .filter(|b| !b.floating)
        .partition(|b| b.is_sidebar_labeled());
    sidebars.into_iter().chain(others).find_map(|block| {
        let size = block.size.as_ref().filter(|size| size.is_measured())?;
        if stale.and_then(|stale| stale.get(&block.block_id)) == Some(&size.stamp()) {
            return None;
        }
        let rect = layout.block_rect(&block.block_id)?;
        match position {
            SidebarPosition::Left => window_extent(size.columns, rect[2]),
            SidebarPosition::Top => window_extent(size.rows, rect[3]),
        }
    })
}

fn window_view(
    window: &RexSidebarWindow,
    position: SidebarPosition,
    exe_name: &str,
    placed: Option<&Placement>,
) -> WindowView {
    let layout = window.layout.as_ref();
    let tiled: Vec<&str> = layout.map(LayoutNode::block_ids).unwrap_or_default();
    let root = layout.and_then(|layout| layout.split.as_deref());
    let edge_block = root
        .filter(|split| split_matches(split.is_horizontal(), position))
        .and_then(|split| split.before.block_id.as_deref());
    let mut complete = true;
    let candidates: Vec<Candidate> = window
        .blocks
        .iter()
        .filter(|block| block.is_sidebar_labeled())
        .map(|block| {
            let placed_here = placed.is_some_and(|p| p.block_id == block.block_id);
            if block.child_name.is_none() && !placed_here {
                complete = false;
            }
            Candidate {
                block_id: block.block_id.clone(),
                owned: placed_here || block.child_name.as_deref() == Some(exe_name),
                at_edge: !block.floating && edge_block == Some(block.block_id.as_str()),
            }
        })
        .collect();
    let content_blocks = window
        .blocks
        .iter()
        .filter(|block| !block.is_sidebar_labeled() && tiled.contains(&block.block_id.as_str()))
        .map(|block| block.block_id.clone())
        .collect();
    let root = root
        .filter(|_| candidates.iter().any(|c| c.owned && c.at_edge))
        .and_then(|split| Some((split.ratio, split.after.first_block()?.to_string())));
    WindowView {
        anchor: layout.and_then(LayoutNode::anchor_id).map(str::to_string),
        candidates,
        content_blocks,
        complete,
        topology: topology(window),
        root,
        window_cells: layout.and_then(|layout| {
            measured_extent(window, layout, position, placed.map(|p| &p.stale_sizes))
        }),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Place {
        anchor: String,
    },
    Close {
        block_id: String,
    },
    /// Start tracking a sidebar the daemon did not place, such as one from
    /// before a daemon restart.
    Adopt {
        block_id: String,
    },
    /// The user closed the sidebar: leave the window alone until the
    /// sidebar is turned on again.
    Dismiss,
}

/// What a window needs so it has exactly the sidebars the scope wants.
fn plan(w: &WindowView, wanted: bool, placed: Option<&Placement>, dismissed: bool) -> Vec<Action> {
    // A window with nothing but a sidebar is closing: the sidebar quits
    // when it is the last block.
    if !w.complete || w.content_blocks.is_empty() {
        return Vec::new();
    }
    let owned: Vec<&Candidate> = w.candidates.iter().filter(|c| c.owned).collect();
    let close = |c: &&Candidate| Action::Close {
        block_id: c.block_id.clone(),
    };
    if !wanted {
        return owned.iter().map(close).collect();
    }
    if dismissed {
        return Vec::new();
    }
    let keep = owned.iter().find(|c| c.at_edge);
    let mut actions: Vec<Action> = owned
        .iter()
        .filter(|c| Some(c) != keep.as_ref())
        .map(close)
        .collect();
    match keep {
        Some(keep) => {
            if placed.is_none_or(|p| p.block_id != keep.block_id) {
                actions.push(Action::Adopt {
                    block_id: keep.block_id.clone(),
                });
            }
        }
        // Moved sidebars close first; the next pass places a new one.
        None if !actions.is_empty() => {}
        None if placed.is_some() => actions.push(Action::Dismiss),
        None => {
            if let Some(anchor) = &w.anchor {
                actions.push(Action::Place {
                    anchor: anchor.clone(),
                });
            }
        }
    }
    actions
}

/// The split fraction that gives the sidebar `cells` while leaving room for
/// content.
fn sidebar_ratio(cells: u16, window_cells: u16, position: SidebarPosition) -> f64 {
    let window = f64::from(window_cells.max(1));
    let min_content = match position {
        SidebarPosition::Left => MIN_CONTENT_COLS,
        SidebarPosition::Top => MIN_CONTENT_ROWS,
    };
    let max = ((window - min_content) / window).clamp(0.05, 0.95);
    (f64::from(cells.max(1)) / window).clamp(0.01, max)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SizeDecision {
    /// No client shows the window at its current layout yet.
    Wait,
    Keep,
    /// The blocks changed but the ratio did not, yet: the app may still
    /// rebalance the row.
    Settle,
    /// Move the border to this ratio.
    Enforce(f64),
    /// A ratio seen once. A second sighting makes it a drag.
    Candidate(f64),
    /// The user dragged the border: this extent becomes every sidebar's.
    Persist(u16),
}

/// How a placed sidebar's size should change, given the root ratio now and
/// the ratio its target extent needs.
fn size_decision(
    placed: &Placement,
    topology: u64,
    ratio_now: f64,
    window_cells: Option<u16>,
    target: f64,
    configured: bool,
) -> SizeDecision {
    // Without a client the window has no real size: keep what was placed.
    let Some(window_cells) = window_cells else {
        return SizeDecision::Wait;
    };
    let epsilon = 0.5 / f64::from(window_cells.max(1));
    let same = |a: f64, b: f64| (a - b).abs() < epsilon;
    let moved = !same(ratio_now, placed.applied_ratio);
    // Rex keeps ratios when the app resizes a window, so a new extent at the
    // same ratio is a window resize. The extent is measured through blocks
    // with padding, so it also shifts when the ratio does.
    if placed.applied_window_cells.is_none()
        || (!moved && placed.applied_window_cells != Some(window_cells))
    {
        return SizeDecision::Enforce(target);
    }
    if placed.topology != topology {
        return if moved {
            SizeDecision::Enforce(target)
        } else {
            SizeDecision::Settle
        };
    }
    if placed.settling {
        return if moved {
            SizeDecision::Enforce(target)
        } else {
            SizeDecision::Keep
        };
    }
    if !moved {
        return if same(target, placed.applied_ratio) {
            SizeDecision::Keep
        } else {
            SizeDecision::Enforce(target)
        };
    }
    match placed.drag_candidate {
        Some(seen) if same(seen, ratio_now) && configured => SizeDecision::Enforce(target),
        Some(seen) if same(seen, ratio_now) => {
            SizeDecision::Persist(((ratio_now * f64::from(window_cells)).round() as u16).max(1))
        }
        _ => SizeDecision::Candidate(ratio_now),
    }
}

/// The user's config with the sidebar size stored for this server.
fn sidebar_config(state: &SidebarSessionState) -> Config {
    let mut config = Config::load(None).unwrap_or_default();
    if state.width.is_some() {
        config.sidebar.width.clone_from(&state.width);
    }
    if state.height.is_some() {
        config.sidebar.height.clone_from(&state.height);
    }
    config
}

fn synced_extent(position: SidebarPosition) -> Option<u16> {
    let settings = crate::state::StateStore::new().ok()?.load_settings().ok()?;
    match position {
        SidebarPosition::Left => settings.sidebar_width,
        SidebarPosition::Top => settings.sidebar_height,
    }
    .filter(|cells| *cells > 0)
}

fn persist_extent(position: SidebarPosition, cells: u16) {
    if let Ok(store) = crate::state::StateStore::new()
        && let Ok(mut settings) = store.load_settings()
    {
        match position {
            SidebarPosition::Left => settings.sidebar_width = Some(cells),
            SidebarPosition::Top => settings.sidebar_height = Some(cells),
        }
        let _ = store.save_settings(&settings);
    }
}

/// Sizing inputs shared by every window in one pass.
struct Sizing {
    config: Config,
    position: SidebarPosition,
    synced: Option<u16>,
    configured: bool,
    /// An extent some displayed window has, for windows no client shows.
    fallback_cells: u16,
}

impl Sizing {
    fn new(state: &SidebarSessionState, views: &[WindowView]) -> Self {
        let config = sidebar_config(state);
        let position = state.prefs.position.unwrap_or_default();
        let configured = match position {
            SidebarPosition::Left => config.sidebar.width.is_some(),
            SidebarPosition::Top => config.sidebar.height.is_some(),
        };
        let fallback_cells =
            views
                .iter()
                .find_map(|view| view.window_cells)
                .unwrap_or(match position {
                    SidebarPosition::Left => ASSUMED_WINDOW_COLS,
                    SidebarPosition::Top => ASSUMED_WINDOW_ROWS,
                });
        Self {
            config,
            position,
            synced: synced_extent(position),
            configured,
            fallback_cells,
        }
    }

    fn target_ratio(&self, window_cells: Option<u16>) -> f64 {
        let window = window_cells.unwrap_or(self.fallback_cells);
        let cells = match self.position {
            SidebarPosition::Left => super::resolve_width_for(&self.config, window, self.synced),
            SidebarPosition::Top => super::resolve_height_for(&self.config, window, self.synced),
        };
        sidebar_ratio(cells, window, self.position)
    }
}

/// Rex's runtime records after one reconcile pass.
#[derive(Debug, Default)]
struct Records {
    placements: BTreeMap<String, Placement>,
    dismissed: BTreeSet<String>,
}

struct Executor<'a> {
    rex: &'a RexBackend,
    instance_id: &'a str,
    exe: PathBuf,
    live_panes: &'a HashMap<String, LivePaneInfo>,
}

impl Executor<'_> {
    fn launch_argv(&self) -> Vec<String> {
        // Rex terminals don't inherit the server address, and the sidebar
        // must reach this server and pick the Rex backend.
        vec![
            "/usr/bin/env".to_string(),
            "WORKMUX_BACKEND=rex".to_string(),
            format!("REX_SERVER={}", self.instance_id),
            self.exe.to_string_lossy().into_owned(),
            "_sidebar-run".to_string(),
        ]
    }

    fn place(
        &self,
        window: &RexSidebarWindow,
        view: &WindowView,
        anchor: &str,
        sizing: &Sizing,
    ) -> Result<Placement> {
        let cwd = view
            .content_blocks
            .iter()
            .find_map(|block| self.live_panes.get(block))
            .map(|info| info.working_dir.clone())
            .filter(|dir| dir.is_dir())
            .or_else(home::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        let ratio = sizing.target_ratio(view.window_cells);
        let layout = exec_layout(&cwd, &self.launch_argv(), Some(SIDEBAR_LABEL));
        let block_id = self.rex.new_sidebar_split(
            &window.session_id,
            anchor,
            sizing.position,
            ratio,
            layout,
        )?;
        let mut ids: Vec<&str> = window.blocks.iter().map(|b| b.block_id.as_str()).collect();
        ids.push(&block_id);
        ids.sort_unstable();
        let mut hasher = DefaultHasher::new();
        ids.hash(&mut hasher);
        Ok(Placement {
            block_id,
            applied_ratio: ratio,
            applied_window_cells: view.window_cells,
            topology: hasher.finish(),
            // The app may rebalance the new split once it shows the window.
            settling: true,
            drag_candidate: None,
            stale_sizes: size_stamps(window),
        })
    }

    fn close(&self, window: &RexSidebarWindow, block_id: &str) -> Result<()> {
        match self.rex.close_block(&window.session_id, block_id, true) {
            Err(error) if error.is::<LastBlockRefused>() => {
                // The content closed under us; the sidebar quits by itself.
                tracing::debug!(block_id, "sidebar is the last block, leaving it");
                Ok(())
            }
            result => result,
        }
    }

    fn resize(
        &self,
        window: &RexSidebarWindow,
        view: &WindowView,
        position: SidebarPosition,
        sidebar: &str,
        ratio: f64,
    ) -> Result<()> {
        let (_, neighbor) = view
            .root
            .as_ref()
            .ok_or_else(|| anyhow!("sidebar has no neighbor to resize against"))?;
        self.rex
            .resize_sidebar(&window.session_id, position, sidebar, neighbor, ratio)
    }
}

/// Bring every window in line with the scope. Callers hold the layout lock.
pub(super) fn reconcile(instance_id: &str, obs: &RexSidebarObservation) -> Result<()> {
    let state = session_state::read(instance_id)?;
    let position = state.prefs.position.unwrap_or_default();
    let exe = exe_path()?;
    let exe_name = exe_name(&exe);
    let same_boot = obs.boot_id.is_some() && state.boot_id == obs.boot_id;
    let (placements, dismissed) = if same_boot {
        (state.placements.clone(), state.dismissed.clone())
    } else {
        Default::default()
    };

    let views: Vec<WindowView> = obs
        .windows
        .iter()
        .map(|w| window_view(w, position, &exe_name, placements.get(&w.window_id)))
        .collect();
    let sizing = Sizing::new(&state, &views);
    let rex = RexBackend::new();
    let executor = Executor {
        rex: &rex,
        instance_id,
        exe,
        live_panes: &obs.live_panes,
    };

    let mut records = Records::default();
    for (window, view) in obs.windows.iter().zip(&views) {
        let wanted = state.covers_session(&window.session_id);
        let mut placed = placements.get(&window.window_id).cloned();
        let mut is_dismissed = dismissed.contains(&window.window_id);
        for action in plan(view, wanted, placed.as_ref(), is_dismissed) {
            match action {
                Action::Place { anchor } => match executor.place(window, view, &anchor, &sizing) {
                    Ok(placement) => placed = Some(placement),
                    Err(error) => {
                        tracing::warn!(window = %window.window_id, %error, "failed to place Rex sidebar")
                    }
                },
                Action::Close { block_id } => {
                    if let Err(error) = executor.close(window, &block_id) {
                        tracing::warn!(%block_id, %error, "failed to close Rex sidebar");
                    }
                    if placed.as_ref().is_some_and(|p| p.block_id == block_id) {
                        placed = None;
                    }
                }
                Action::Adopt { block_id } => {
                    let ratio = view.root.as_ref().map_or(0.0, |(ratio, _)| *ratio);
                    placed = Some(Placement {
                        block_id,
                        applied_ratio: ratio,
                        applied_window_cells: None,
                        topology: view.topology,
                        settling: false,
                        drag_candidate: None,
                        stale_sizes: BTreeMap::new(),
                    });
                }
                Action::Dismiss => {
                    tracing::info!(window = %window.window_id, "Rex sidebar closed by the user");
                    placed = None;
                    is_dismissed = true;
                }
            }
        }
        if wanted
            && let Some(placement) = placed.as_mut()
            && let Some((ratio_now, _)) = view.root.as_ref()
            && view
                .candidates
                .iter()
                .any(|c| c.block_id == placement.block_id)
        {
            resize_placed(&executor, window, view, *ratio_now, &sizing, placement);
        }
        if let Some(placement) = placed.filter(|_| wanted) {
            records
                .placements
                .insert(window.window_id.clone(), placement);
        }
        if is_dismissed && wanted {
            records.dismissed.insert(window.window_id.clone());
        }
    }

    session_state::update(instance_id, |current| {
        current.boot_id.clone_from(&obs.boot_id);
        current.placements = records.placements;
        current.dismissed = records.dismissed;
    })?;
    Ok(())
}

fn resize_placed(
    executor: &Executor<'_>,
    window: &RexSidebarWindow,
    view: &WindowView,
    ratio_now: f64,
    sizing: &Sizing,
    placement: &mut Placement,
) {
    let target = sizing.target_ratio(view.window_cells);
    let decision = size_decision(
        placement,
        view.topology,
        ratio_now,
        view.window_cells,
        target,
        sizing.configured,
    );
    tracing::debug!(
        window = %window.window_id,
        ratio_now,
        applied_ratio = placement.applied_ratio,
        window_cells = ?view.window_cells,
        applied_window_cells = ?placement.applied_window_cells,
        target,
        ?decision,
        "Rex sidebar size"
    );
    let topology_changed = placement.topology != view.topology;
    placement.topology = view.topology;
    match decision {
        SizeDecision::Wait => {}
        SizeDecision::Keep => {
            placement.settling = false;
            placement.drag_candidate = None;
        }
        SizeDecision::Settle => {
            placement.settling = true;
            placement.drag_candidate = None;
        }
        SizeDecision::Enforce(ratio) => {
            let block_id = placement.block_id.clone();
            match executor.resize(window, view, sizing.position, &block_id, ratio) {
                Ok(()) => {
                    placement.applied_ratio = ratio;
                    placement.applied_window_cells = view.window_cells;
                    placement.stale_sizes = size_stamps(window);
                }
                Err(error) => tracing::warn!(%block_id, %error, "failed to resize Rex sidebar"),
            }
            placement.settling = topology_changed;
            placement.drag_candidate = None;
        }
        SizeDecision::Candidate(ratio) => placement.drag_candidate = Some(ratio),
        SizeDecision::Persist(cells) => {
            tracing::info!(cells, "Rex sidebar resized by the user");
            persist_extent(sizing.position, cells);
            placement.applied_ratio = ratio_now;
            placement.applied_window_cells = view.window_cells;
            placement.drag_candidate = None;
        }
    }
}

/// Observe the server and reconcile under the layout lock, waiting for a
/// daemon pass in progress to finish.
pub(super) fn sync(instance_id: &str) -> Result<RexSidebarObservation> {
    let _lock = session_state::lock_layout(instance_id)?;
    let obs = RexBackend::new().sidebar_observation()?;
    reconcile(instance_id, &obs)?;
    Ok(obs)
}

/// The window holding the Rex block this process runs in.
pub(super) fn current_window(obs: &RexSidebarObservation) -> Option<&RexSidebarWindow> {
    let block = std::env::var("REX_BLOCK").ok()?;
    obs.windows
        .iter()
        .find(|w| w.blocks.iter().any(|b| b.block_id == block))
}

/// Whether the current window shows a workmux sidebar.
pub(super) fn current_window_has_sidebar(obs: &RexSidebarObservation) -> bool {
    let exe_name = exe_path().map(|exe| exe_name(&exe)).unwrap_or_default();
    current_window(obs).is_some_and(|window| {
        window
            .blocks
            .iter()
            .any(|b| b.is_sidebar_labeled() && b.child_name.as_deref() == Some(exe_name.as_str()))
    })
}

/// Forget dismissals in these sessions, or in all of them.
pub(super) fn clear_dismissals(
    state: &mut SidebarSessionState,
    obs: &RexSidebarObservation,
    session: Option<&str>,
) {
    match session {
        None => state.dismissed.clear(),
        Some(session) => {
            for window in obs.windows.iter().filter(|w| w.session_id == session) {
                state.dismissed.remove(&window.window_id);
            }
        }
    }
}

/// Whether the sidebar block is the only tiled block left in its window.
pub(super) fn sidebar_is_only_pane(window_id: &str, pane_id: &str) -> bool {
    RexBackend::new().sidebar_observation().is_ok_and(|obs| {
        obs.windows
            .iter()
            .find(|w| w.window_id == window_id)
            .and_then(|w| w.layout.as_ref())
            .is_some_and(|layout| layout.block_ids() == [pane_id])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(block_id: &str, owned: bool, at_edge: bool) -> Candidate {
        Candidate {
            block_id: block_id.to_string(),
            owned,
            at_edge,
        }
    }

    fn view(candidates: Vec<Candidate>) -> WindowView {
        WindowView {
            anchor: Some("split:root".to_string()),
            candidates,
            content_blocks: vec!["block:content".to_string()],
            complete: true,
            topology: 1,
            root: None,
            window_cells: None,
        }
    }

    fn placement(block_id: &str) -> Placement {
        Placement {
            block_id: block_id.to_string(),
            applied_ratio: 0.125,
            applied_window_cells: Some(200),
            topology: 1,
            settling: false,
            drag_candidate: None,
            stale_sizes: BTreeMap::new(),
        }
    }

    fn close(block_id: &str) -> Action {
        Action::Close {
            block_id: block_id.to_string(),
        }
    }

    #[test]
    fn plan_places_a_sidebar_in_a_window_without_one() {
        assert_eq!(
            plan(&view(vec![]), true, None, false),
            [Action::Place {
                anchor: "split:root".to_string()
            }]
        );
    }

    #[test]
    fn plan_keeps_a_placed_edge_sidebar() {
        let w = view(vec![candidate("block:s", true, true)]);
        assert!(plan(&w, true, Some(&placement("block:s")), false).is_empty());
    }

    #[test]
    fn plan_adopts_an_untracked_edge_sidebar() {
        let w = view(vec![candidate("block:s", true, true)]);
        assert_eq!(
            plan(&w, true, None, false),
            [Action::Adopt {
                block_id: "block:s".to_string()
            }]
        );
    }

    #[test]
    fn plan_closes_duplicates_and_keeps_the_edge_one() {
        let w = view(vec![
            candidate("block:a", true, false),
            candidate("block:b", true, true),
        ]);
        assert_eq!(
            plan(&w, true, Some(&placement("block:b")), false),
            [close("block:a")]
        );
    }

    #[test]
    fn plan_closes_a_moved_sidebar_without_dismissing() {
        let w = view(vec![candidate("block:s", true, false)]);
        assert_eq!(
            plan(&w, true, Some(&placement("block:s")), false),
            [close("block:s")]
        );
    }

    #[test]
    fn plan_dismisses_when_a_placed_sidebar_vanishes() {
        assert_eq!(
            plan(&view(vec![]), true, Some(&placement("block:s")), false),
            [Action::Dismiss]
        );
    }

    #[test]
    fn plan_leaves_dismissed_windows_alone() {
        assert!(plan(&view(vec![]), true, None, true).is_empty());
    }

    #[test]
    fn plan_out_of_scope_closes_only_owned_sidebars() {
        let w = view(vec![
            candidate("block:mine", true, true),
            candidate("block:other", false, false),
        ]);
        assert_eq!(plan(&w, false, None, false), [close("block:mine")]);
    }

    #[test]
    fn plan_waits_when_ownership_is_unknown() {
        let mut w = view(vec![]);
        w.complete = false;
        assert!(plan(&w, true, Some(&placement("block:s")), false).is_empty());
    }

    #[test]
    fn plan_leaves_windows_without_content() {
        let mut w = view(vec![candidate("block:s", true, true)]);
        w.content_blocks.clear();
        assert!(plan(&w, false, None, false).is_empty());
    }

    #[test]
    fn size_unmeasured_window_waits() {
        let mut p = placement("block:s");
        p.applied_window_cells = None;
        assert_eq!(
            size_decision(&p, 1, 0.3, None, 0.125, false),
            SizeDecision::Wait
        );
    }

    #[test]
    fn size_rebalance_of_a_new_sidebar_is_not_a_drag() {
        // The app splits a sidebar placed in the window it shows evenly.
        let mut p = placement("block:s");
        p.settling = true;
        assert_eq!(
            size_decision(&p, 1, 0.5, None, 0.125, false),
            SizeDecision::Wait
        );
        assert_eq!(
            size_decision(&p, 1, 0.5, Some(200), 0.125, false),
            SizeDecision::Enforce(0.125)
        );
    }

    #[test]
    fn size_first_measure_and_window_resize_enforce() {
        let mut p = placement("block:s");
        p.applied_window_cells = None;
        assert_eq!(
            size_decision(&p, 1, 0.15, Some(200), 0.125, false),
            SizeDecision::Enforce(0.125)
        );
        let p = placement("block:s");
        assert_eq!(
            size_decision(&p, 1, 0.125, Some(300), 0.1, false),
            SizeDecision::Enforce(0.1)
        );
    }

    #[test]
    fn size_topology_change_settles_then_enforces_a_late_balance() {
        let p = placement("block:s");
        assert_eq!(
            size_decision(&p, 2, 0.125, Some(200), 0.125, false),
            SizeDecision::Settle
        );
        let mut settling = placement("block:s");
        settling.settling = true;
        assert_eq!(
            size_decision(&settling, 1, 1.0 / 3.0, Some(200), 0.125, false),
            SizeDecision::Enforce(0.125)
        );
        assert_eq!(
            size_decision(&p, 2, 0.5, Some(200), 0.125, false),
            SizeDecision::Enforce(0.125)
        );
    }

    #[test]
    fn size_drag_persists_on_the_second_sighting() {
        let mut p = placement("block:s");
        assert_eq!(
            size_decision(&p, 1, 0.2, Some(200), 0.125, false),
            SizeDecision::Candidate(0.2)
        );
        p.drag_candidate = Some(0.2);
        assert_eq!(
            size_decision(&p, 1, 0.2, Some(200), 0.125, false),
            SizeDecision::Persist(40)
        );
        p.drag_candidate = Some(0.18);
        assert_eq!(
            size_decision(&p, 1, 0.2, Some(200), 0.125, false),
            SizeDecision::Candidate(0.2)
        );
    }

    #[test]
    fn size_drag_is_not_a_window_resize_when_the_extent_shifts() {
        // Block padding makes the measured extent move with the ratio.
        let mut p = placement("block:s");
        assert_eq!(
            size_decision(&p, 1, 0.2, Some(196), 0.125, false),
            SizeDecision::Candidate(0.2)
        );
        p.drag_candidate = Some(0.2);
        assert_eq!(
            size_decision(&p, 1, 0.2, Some(196), 0.125, false),
            SizeDecision::Persist(39)
        );
    }

    #[test]
    fn size_drag_snaps_back_when_the_width_is_configured() {
        let mut p = placement("block:s");
        p.drag_candidate = Some(0.2);
        assert_eq!(
            size_decision(&p, 1, 0.2, Some(200), 0.125, true),
            SizeDecision::Enforce(0.125)
        );
    }

    #[test]
    fn size_follows_a_new_target_and_keeps_an_applied_ratio() {
        let p = placement("block:s");
        assert_eq!(
            size_decision(&p, 1, 0.125, Some(200), 0.2, false),
            SizeDecision::Enforce(0.2)
        );
        assert_eq!(
            size_decision(&p, 1, 0.126, Some(200), 0.125, false),
            SizeDecision::Keep
        );
    }

    #[test]
    fn ratio_leaves_room_for_content() {
        assert_eq!(sidebar_ratio(25, 200, SidebarPosition::Left), 0.125);
        assert_eq!(sidebar_ratio(80, 50, SidebarPosition::Left), 0.8);
        assert_eq!(sidebar_ratio(3, 40, SidebarPosition::Top), 0.075);
    }

    #[test]
    fn extent_comes_from_a_block_and_its_share() {
        assert_eq!(window_extent(25, 0.125), Some(200));
        assert_eq!(window_extent(0, 0.5), None);
        assert_eq!(window_extent(10, 0.0), None);
    }

    mod observed {
        use super::*;
        use crate::multiplexer::rex::sidebar::{BlockSize, RexSidebarBlock};

        fn block(id: &str, label: &str, child: Option<&str>) -> RexSidebarBlock {
            RexSidebarBlock {
                block_id: id.to_string(),
                floating: false,
                label: Some(label.to_string()),
                child_name: child.map(str::to_string),
                size: None,
            }
        }

        fn window(layout: &str, blocks: Vec<RexSidebarBlock>) -> RexSidebarWindow {
            RexSidebarWindow {
                session_id: "session:1".to_string(),
                window_id: "window:1".to_string(),
                layout: Some(serde_json::from_str(layout).unwrap()),
                blocks,
            }
        }

        const WRAPPED: &str = r#"{"rect":[0,0,1,1],"split":{"split_id":"split:root","direction":"horizontal","ratio":0.125,
            "before":{"rect":[0,0,0.125,1],"block_id":"block:s"},
            "after":{"rect":[0.125,0,0.875,1],"block_id":"block:c"}}}"#;

        #[test]
        fn ownership_needs_the_workmux_process_or_a_placement() {
            let w = window(
                WRAPPED,
                vec![
                    block("block:s", SIDEBAR_LABEL, Some("bash")),
                    block("block:c", "shell", Some("zsh")),
                ],
            );
            let v = window_view(&w, SidebarPosition::Left, "workmux", None);
            assert!(!v.candidates[0].owned);
            assert!(v.root.is_none());
            let v = window_view(&w, SidebarPosition::Left, "bash", None);
            assert!(v.candidates[0].owned && v.candidates[0].at_edge);
            assert_eq!(v.root, Some((0.125, "block:c".to_string())));
            let v = window_view(
                &w,
                SidebarPosition::Left,
                "workmux",
                Some(&placement("block:s")),
            );
            assert!(v.candidates[0].owned);
        }

        #[test]
        fn a_left_sidebar_is_not_at_the_edge_for_top() {
            let w = window(
                WRAPPED,
                vec![
                    block("block:s", SIDEBAR_LABEL, Some("workmux")),
                    block("block:c", "shell", Some("zsh")),
                ],
            );
            let v = window_view(&w, SidebarPosition::Top, "workmux", None);
            assert!(v.candidates[0].owned && !v.candidates[0].at_edge);
            assert_eq!(plan(&v, true, None, false), [close("block:s")]);
        }

        #[test]
        fn unknown_process_marks_the_window_incomplete() {
            let w = window(
                WRAPPED,
                vec![
                    block("block:s", SIDEBAR_LABEL, None),
                    block("block:c", "shell", Some("zsh")),
                ],
            );
            assert!(!window_view(&w, SidebarPosition::Left, "workmux", None).complete);
        }

        #[test]
        fn the_window_extent_comes_from_a_displayed_block() {
            let mut content = block("block:c", "shell", Some("zsh"));
            content.size = Some(BlockSize {
                columns: 175,
                rows: 50,
                desired: vec![serde_json::json!({"client_id": "client:1"})],
            });
            let w = window(
                WRAPPED,
                vec![block("block:s", SIDEBAR_LABEL, Some("workmux")), content],
            );
            let v = window_view(&w, SidebarPosition::Left, "workmux", None);
            assert_eq!(v.window_cells, Some(200));
            assert_eq!(v.anchor.as_deref(), Some("split:root"));
            assert_eq!(v.content_blocks, ["block:c"]);
        }

        fn shown(columns: u16, at: &str) -> Option<BlockSize> {
            Some(BlockSize {
                columns,
                rows: 50,
                desired: vec![serde_json::json!({"client_id": "client:1", "updated_at": at})],
            })
        }

        #[test]
        fn sizes_from_before_a_resize_do_not_measure_the_window() {
            // A hidden window resized from 25 to 50 columns of 200 keeps
            // reporting the old sizes against the new layout.
            let mut sidebar = block("block:s", SIDEBAR_LABEL, Some("workmux"));
            sidebar.size = shown(25, "t1");
            let mut content = block("block:c", "shell", Some("zsh"));
            content.size = shown(175, "t1");
            let mut w = window(
                &WRAPPED.replace("0.125", "0.25").replace("0.875", "0.75"),
                vec![sidebar, content],
            );
            let mut p = placement("block:s");
            p.stale_sizes = size_stamps(&w);
            let v = window_view(&w, SidebarPosition::Left, "workmux", Some(&p));
            assert_eq!(v.window_cells, None);
            let decision = size_decision(&p, topology(&w), 0.25, v.window_cells, 0.25, false);
            assert_eq!(decision, SizeDecision::Wait);

            // Once a client shows the window, the sidebar's new size counts.
            w.blocks[0].size = shown(50, "t2");
            let v = window_view(&w, SidebarPosition::Left, "workmux", Some(&p));
            assert_eq!(v.window_cells, Some(200));
        }

        #[test]
        fn the_sidebar_block_measures_the_window_first() {
            let mut sidebar = block("block:s", SIDEBAR_LABEL, Some("workmux"));
            sidebar.size = shown(26, "t1");
            let mut content = block("block:c", "shell", Some("zsh"));
            content.size = shown(170, "t1");
            let w = window(WRAPPED, vec![content, sidebar]);
            let v = window_view(&w, SidebarPosition::Left, "workmux", None);
            assert_eq!(v.window_cells, Some(208));
        }

        #[test]
        fn convergence_after_placing() {
            let w = window(
                WRAPPED,
                vec![
                    block("block:s", "workmux-sidebar", Some("workmux")),
                    block("block:c", "shell", Some("zsh")),
                ],
            );
            let mut p = placement("block:s");
            p.topology = topology(&w);
            let v = window_view(&w, SidebarPosition::Left, "workmux", Some(&p));
            assert!(plan(&v, true, Some(&p), false).is_empty());
        }
    }
}
