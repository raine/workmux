//! What the sidebar daemon reads from and does to a Rex server.
//!
//! Rex has no hooks, so the daemon observes every window's layout and places,
//! resizes and closes sidebar blocks itself. Sidebar blocks carry the label
//! [`SIDEBAR_LABEL`] and run the workmux binary directly, so the block's
//! process names it.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::SidebarPosition;
use crate::multiplexer::types::LivePaneInfo;

use super::{RexBackend, RexPane, SHELL_FLAVOR, lua_list, server_endpoint, util};

/// Label of every block that runs a workmux sidebar.
pub(crate) const SIDEBAR_LABEL: &str = "workmux-sidebar";

/// A terminal block's size as reported by its `size` call.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BlockSize {
    pub columns: u16,
    pub rows: u16,
    /// Sizes requested by attached clients. Empty when no client displays
    /// the block, in which case `columns` and `rows` are a placeholder.
    #[serde(default, deserialize_with = "lua_list")]
    pub desired: Vec<Value>,
}

impl BlockSize {
    /// Whether a client displays the block, so its size is real.
    pub(crate) fn is_measured(&self) -> bool {
        !self.desired.is_empty()
    }
}

/// One node of a window's tiled layout. Rects are fractions of the window:
/// `[x, y, width, height]`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LayoutNode {
    pub rect: [f64; 4],
    pub block_id: Option<String>,
    pub split: Option<Box<LayoutSplit>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LayoutSplit {
    pub split_id: String,
    /// `horizontal` puts `before` left of `after`, `vertical` above it.
    pub direction: String,
    /// The share of the split that `before` takes.
    pub ratio: f64,
    pub before: LayoutNode,
    pub after: LayoutNode,
}

impl LayoutSplit {
    pub(crate) fn is_horizontal(&self) -> bool {
        self.direction == "horizontal"
    }
}

impl LayoutNode {
    /// The first block in layout order: the top-left one.
    pub(crate) fn first_block(&self) -> Option<&str> {
        match (&self.block_id, &self.split) {
            (Some(block_id), _) => Some(block_id),
            (None, Some(split)) => split.before.first_block(),
            (None, None) => None,
        }
    }

    /// The ID that addresses this whole subtree as a split anchor.
    pub(crate) fn anchor_id(&self) -> Option<&str> {
        match (&self.block_id, &self.split) {
            (Some(block_id), _) => Some(block_id),
            (None, Some(split)) => Some(&split.split_id),
            (None, None) => None,
        }
    }

    /// Every block ID in layout order.
    pub(crate) fn block_ids(&self) -> Vec<&str> {
        let mut ids = Vec::new();
        self.collect_blocks(&mut ids);
        ids
    }

    fn collect_blocks<'a>(&'a self, ids: &mut Vec<&'a str>) {
        if let Some(block_id) = &self.block_id {
            ids.push(block_id);
        }
        if let Some(split) = &self.split {
            split.before.collect_blocks(ids);
            split.after.collect_blocks(ids);
        }
    }

    /// The rect of a block in this subtree.
    pub(crate) fn block_rect(&self, block_id: &str) -> Option<[f64; 4]> {
        if self.block_id.as_deref() == Some(block_id) {
            return Some(self.rect);
        }
        let split = self.split.as_ref()?;
        split
            .before
            .block_rect(block_id)
            .or_else(|| split.after.block_rect(block_id))
    }
}

/// A window's tiled layout as reported by the snapshot script.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RexWindowLayout {
    pub session_id: String,
    pub window_id: String,
    #[serde(default)]
    pub active: bool,
    pub layout: Option<LayoutNode>,
}

/// A block as the sidebar daemon sees it.
#[derive(Debug, Clone)]
pub(crate) struct RexSidebarBlock {
    pub block_id: String,
    pub floating: bool,
    pub label: Option<String>,
    /// Name of the process the terminal started.
    pub child_name: Option<String>,
    pub size: Option<BlockSize>,
}

impl RexSidebarBlock {
    pub(crate) fn is_sidebar_labeled(&self) -> bool {
        self.label.as_deref() == Some(SIDEBAR_LABEL)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RexSidebarWindow {
    pub session_id: String,
    pub window_id: String,
    pub layout: Option<LayoutNode>,
    pub blocks: Vec<RexSidebarBlock>,
}

/// One read of the whole server for the sidebar daemon.
#[derive(Debug, Default)]
pub(crate) struct RexSidebarObservation {
    pub boot_id: Option<String>,
    /// Content panes. Sidebar blocks are left out, so they are never taken
    /// for agents.
    pub live_panes: HashMap<String, LivePaneInfo>,
    /// `(session label, window ID)` of every session's active window.
    pub active_windows: HashSet<(String, String)>,
    pub pane_window_ids: HashMap<String, String>,
    pub pane_window_indexes: HashMap<String, u32>,
    pub active_pane_ids: HashSet<String>,
    /// Tiled blocks per window, sidebars included.
    pub window_pane_counts: HashMap<String, usize>,
    pub windows: Vec<RexSidebarWindow>,
}

/// Rex refused to close a window's last block.
#[derive(Debug)]
pub(crate) struct LastBlockRefused;

impl std::fmt::Display for LastBlockRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Rex refused to close the last block in the window")
    }
}

impl std::error::Error for LastBlockRefused {}

/// A one-block layout in `cwd` that execs `argv` directly, so the block's
/// process reports `argv[0]` as its binary.
pub(crate) fn exec_layout(cwd: &Path, argv: &[String], label: Option<&str>) -> Value {
    let mut block = json!({
        "flavor": SHELL_FLAVOR,
        "options": { "cwd": cwd.to_string_lossy(), "command": argv },
    });
    if let Some(label) = label {
        block["label"] = json!(label);
    }
    json!({ "block": block })
}

impl super::RexSnapshot {
    fn sidebar_observation(self) -> RexSidebarObservation {
        let mut obs = RexSidebarObservation {
            boot_id: self.instance_id.clone().filter(|id| !id.is_empty()),
            ..Default::default()
        };
        let mut blocks: HashMap<(String, String), Vec<RexSidebarBlock>> = HashMap::new();
        for pane in &self.panes {
            blocks
                .entry((pane.session_id.clone(), pane.window_id.clone()))
                .or_default()
                .push(pane.sidebar_block());
            if !pane.floating {
                *obs.window_pane_counts
                    .entry(pane.window_id.clone())
                    .or_default() += 1;
            }
            if pane.label.as_deref() == Some(SIDEBAR_LABEL) {
                continue;
            }
            let (pane_id, info) = pane.live_pane_info();
            obs.pane_window_ids
                .insert(pane_id.clone(), pane.window_id.clone());
            obs.pane_window_indexes
                .insert(pane_id.clone(), pane.window_index);
            if pane.active {
                obs.active_pane_ids.insert(pane_id.clone());
            }
            obs.live_panes.insert(pane_id, info);
        }
        for window in self.windows {
            let session_label = self
                .sessions
                .iter()
                .find(|s| s.session_id == window.session_id)
                .map(|s| s.label.clone())
                .unwrap_or_default();
            if window.active {
                obs.active_windows
                    .insert((session_label, window.window_id.clone()));
            }
            obs.windows.push(RexSidebarWindow {
                blocks: blocks
                    .remove(&(window.session_id.clone(), window.window_id.clone()))
                    .unwrap_or_default(),
                session_id: window.session_id,
                window_id: window.window_id,
                layout: window.layout,
            });
        }
        obs
    }
}

impl RexPane {
    fn sidebar_block(&self) -> RexSidebarBlock {
        RexSidebarBlock {
            block_id: self.block_id.clone(),
            floating: self.floating,
            label: self.label.clone(),
            child_name: self.child_name.clone(),
            size: self.size.clone(),
        }
    }
}

impl RexBackend {
    /// Read every session, window, layout and sidebar block in one call.
    pub(crate) fn sidebar_observation(&self) -> Result<RexSidebarObservation> {
        Ok(self.snapshot()?.sidebar_observation())
    }

    /// Wrap the window's whole tiled tree, addressed by `anchor`, in a new
    /// split with the sidebar before it. Focus stays where it was. Returns
    /// the new block's ID.
    pub(crate) fn new_sidebar_split(
        &self,
        session_id: &str,
        anchor: &str,
        position: SidebarPosition,
        ratio: f64,
        layout: Value,
    ) -> Result<String> {
        let result = self.call_api(
            "session.new_split",
            json!({
                "session_id": session_id,
                "anchor_id": anchor,
                "direction": split_direction(position),
                "side": "before",
                "ratio": ratio,
                "focus": false,
                "layout": layout,
            }),
        )?;
        Self::first_block_id(&result)
    }

    /// Move the border between the sidebar and the content next to it so the
    /// sidebar takes `ratio` of the window.
    pub(crate) fn resize_sidebar(
        &self,
        session_id: &str,
        position: SidebarPosition,
        sidebar: &str,
        neighbor: &str,
        ratio: f64,
    ) -> Result<()> {
        self.call_api(
            "session.resize_pane",
            json!({
                "session_id": session_id,
                "direction": split_direction(position),
                "before_block_id": sidebar,
                "after_block_id": neighbor,
                "ratio": ratio,
            }),
        )
        .map(|_| ())
    }

    /// Close a block. With `keep_last`, closing a window's only block fails
    /// with [`LastBlockRefused`] instead of closing the window.
    pub(crate) fn close_block(
        &self,
        session_id: &str,
        block_id: &str,
        keep_last: bool,
    ) -> Result<()> {
        let args = json!({ "block_id": block_id, "keep_last": keep_last });
        match self.call_api_in(Some(session_id), "block.close", args) {
            Ok(_) => Ok(()),
            Err(error) if is_last_block_refusal(&error) => Err(LastBlockRefused.into()),
            Err(error) => {
                Err(error).with_context(|| format!("Failed to close Rex block {block_id}"))
            }
        }
    }

    /// The last `lines` lines of a block's screen, from a known session.
    pub(crate) fn capture_block(
        &self,
        session_id: &str,
        block_id: &str,
        lines: u16,
    ) -> Option<String> {
        // --trim drops the blank rows under the last line of output
        let output = self
            .rex_cmd()
            .args(&["capture", "-s", session_id, "-b", block_id, "--trim"])
            .run_and_capture_stdout()
            .ok()?;
        Some(util::tail_lines(&output, lines))
    }
}

/// Whether a server event can change where sidebars belong: a session or
/// window coming or going, a layout change, or a block changing hands.
/// Events from clients connecting, including the daemon's own CLI calls,
/// don't.
fn wakes(line: &str) -> bool {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    match event["type"].as_str() {
        Some("session_created" | "session_destroyed") => true,
        Some("session_event") => matches!(
            event["payload"]["event"]["type"].as_str(),
            Some("session_view_changed" | "block_created" | "block_closed" | "block_owner_changed")
        ),
        _ => false,
    }
}

/// Follows the server's event stream and calls back on events that can
/// change where sidebars belong. The stream is a hint: callers still poll.
pub(crate) struct EventFollower {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
}

impl EventFollower {
    const MIN_BACKOFF: Duration = Duration::from_millis(500);
    const MAX_BACKOFF: Duration = Duration::from_secs(5);

    pub(crate) fn spawn(wake: impl Fn() + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let thread = {
            let stop = stop.clone();
            let child = child.clone();
            std::thread::spawn(move || Self::run(&stop, &child, &wake))
        };
        Self {
            stop,
            child,
            thread: Some(thread),
        }
    }

    fn run(stop: &AtomicBool, slot: &Mutex<Option<Child>>, wake: &dyn Fn()) {
        let mut backoff = Self::MIN_BACKOFF;
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            let mut command = Command::new("rex");
            if let Some(server) = server_endpoint() {
                command.args(["-S", server]);
            }
            let spawned = command
                .args(["--autostart=false", "events", "--all", "--json"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            match spawned {
                Ok(mut child) => {
                    let stdout = child.stdout.take();
                    *slot.lock().unwrap() = Some(child);
                    if !stop.load(Ordering::Relaxed)
                        && let Some(stdout) = stdout
                    {
                        for line in BufReader::new(stdout).lines() {
                            let Ok(line) = line else { break };
                            if wakes(&line) {
                                wake();
                            }
                        }
                    }
                    if let Some(mut child) = slot.lock().unwrap().take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
                Err(error) => tracing::debug!(%error, "failed to follow Rex events"),
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // Events may have been missed while disconnected
            wake();
            if started.elapsed() > Duration::from_secs(10) {
                backoff = Self::MIN_BACKOFF;
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Self::MAX_BACKOFF);
        }
    }

    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn split_direction(position: SidebarPosition) -> &'static str {
    match position {
        SidebarPosition::Left => "horizontal",
        SidebarPosition::Top => "vertical",
    }
}

fn is_last_block_refusal(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("cannot close the last block"))
}

#[cfg(test)]
mod tests {
    use super::super::RexSnapshot;
    use super::*;
    use anyhow::anyhow;

    const VIEW_WITH_SIDEBAR: &str = include_str!("../testdata/rex_snapshot_with_sidebar.json");

    #[test]
    fn observation_keeps_sidebars_out_of_live_panes() {
        let obs = RexSnapshot::parse(VIEW_WITH_SIDEBAR)
            .unwrap()
            .sidebar_observation();
        let sidebar = "block:01a1d6fa-d08f-7e38-9eb8-3a81b76b8549";
        assert_eq!(
            obs.boot_id.as_deref(),
            Some("f6e08034-7b7f-4680-ad5c-b7d29bf5d874")
        );
        assert!(!obs.live_panes.contains_key(sidebar));
        assert_eq!(obs.live_panes.len(), 2);
        assert_eq!(
            obs.window_pane_counts["window:01a1d6fa-8d5e-77d3-acd5-ec7cd2d12162"],
            3
        );
        assert!(obs.active_windows.contains(&(
            "t".to_string(),
            "window:01a1d6fa-8d5e-77d3-acd5-ec7cd2d12162".to_string()
        )));

        let window = &obs.windows[0];
        assert_eq!(window.blocks.len(), 3);
        let block = window
            .blocks
            .iter()
            .find(|b| b.block_id == sidebar)
            .unwrap();
        assert!(block.is_sidebar_labeled());
        assert_eq!(block.child_name.as_deref(), Some("workmux"));
        assert!(!block.size.as_ref().unwrap().is_measured());
    }

    #[test]
    fn layout_tree_addresses_blocks_and_splits() {
        let obs = RexSnapshot::parse(VIEW_WITH_SIDEBAR)
            .unwrap()
            .sidebar_observation();
        let layout = obs.windows[0].layout.as_ref().unwrap();
        let root = layout.split.as_ref().unwrap();
        assert!(root.is_horizontal());
        assert_eq!(root.ratio, 0.2);
        assert_eq!(
            layout.anchor_id(),
            Some("split:01a1d6fa-d0a1-771c-a49b-b685ebd364d1")
        );
        assert_eq!(
            layout.first_block(),
            Some("block:01a1d6fa-d08f-7e38-9eb8-3a81b76b8549")
        );
        assert_eq!(
            root.after.first_block(),
            Some("block:01a1d6fa-8d3f-705d-a500-ecc67300e3d3")
        );
        assert_eq!(layout.block_ids().len(), 3);
        assert_eq!(
            layout.block_rect("block:01a1d6fa-a5ba-7261-9901-b286c06cab79"),
            Some([0.2, 0.5, 0.8, 0.5])
        );
    }

    #[test]
    fn measured_size_needs_a_client() {
        let headless: BlockSize = serde_json::from_str(
            r#"{"columns":80,"rows":24,"cell_width_px":8,"cell_height_px":16,"owner":null,"desired":{}}"#,
        )
        .unwrap();
        assert!(!headless.is_measured());
        let shown: BlockSize = serde_json::from_str(
            r#"{"columns":416,"rows":111,"owner":"client:1","desired":[{"client_id":"client:1","columns":416,"rows":111}]}"#,
        )
        .unwrap();
        assert!(shown.is_measured());
        assert_eq!(shown.columns, 416);
    }

    #[test]
    fn last_block_refusal_is_recognized() {
        let error = anyhow!("rex: 400 cannot close the last block in window window:1")
            .context("Failed to call Rex API method block.close");
        assert!(is_last_block_refusal(&error));
        assert!(!is_last_block_refusal(&anyhow!("block not found")));
    }

    #[test]
    fn layout_and_session_events_wake_the_daemon() {
        let session_event = |kind: &str| {
            json!({
                "type": "session_event",
                "payload": {"session_id": "session:1", "event": {"type": kind, "payload": {}}},
            })
            .to_string()
        };
        assert!(wakes(r#"{"type":"session_created","payload":{}}"#));
        assert!(wakes(&session_event("session_view_changed")));
        assert!(wakes(&session_event("block_closed")));
        assert!(!wakes(&session_event("client_connected")));
        assert!(!wakes(&session_event("block_event")));
        assert!(!wakes(r#"{"type":"client_joined","payload":{}}"#));
        assert!(!wakes("not json"));
    }

    #[test]
    fn exec_layout_runs_argv_without_a_shell() {
        let layout = exec_layout(
            Path::new("/repo"),
            &["/usr/bin/env".to_string(), "workmux".to_string()],
            Some(SIDEBAR_LABEL),
        );
        assert_eq!(layout["block"]["label"], SIDEBAR_LABEL);
        assert_eq!(layout["block"]["options"]["command"][0], "/usr/bin/env");
        assert_eq!(layout["block"]["options"]["cwd"], "/repo");
    }
}
