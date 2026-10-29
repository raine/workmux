//! Data types for the dashboard application state.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use crate::git::GitStatus;
use crate::github::{BranchSummary, PrListEntry};
use crate::workflow::types::WorktreeInfo;

use super::super::diff::DiffView;

/// Unified event type for the dashboard event loop.
/// All background threads and the input thread send events through a single channel.
pub enum AppEvent {
    /// Terminal input event (from dedicated input thread)
    Terminal(crossterm::event::Event),
    /// Git status update for a worktree path
    GitStatus(PathBuf, GitStatus),
    /// GitHub status update for a repo root
    GithubStatus(PathBuf, HashMap<String, BranchSummary>),
    /// Full worktree list from background fetch
    WorktreeList(Vec<WorktreeInfo>),
    /// Git log preview for a worktree path
    WorktreeLog(PathBuf, String),
    /// Result of a confirmed background removal.
    RemoveWorktreeResult(
        PathBuf,
        Result<crate::workflow::types::RemoveResult, String>,
    ),
    /// Result of a background add-worktree operation
    AddWorktreeResult(Result<String, String>),
    /// Result of fetching open PRs for the add-worktree modal
    AddWorktreePrList(u64, Result<Vec<PrListEntry>, String>),
    /// Progress update during background sweep (current, total, handle)
    SweepProgressUpdate(usize, usize, String),
    /// Sweep operation completed
    SweepComplete(Result<SweepOutcome, String>),
    /// Capture of a remote agent's pane fetched from its node, by row pane id
    RemotePreview(String, Result<String, String>),
    /// Diff of a remote workspace fetched from its node: workspace id and
    /// whether it is the branch diff
    RemoteDiff(
        String,
        bool,
        Result<crate::remote::agents::CachedDiff, String>,
    ),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepOutcome {
    pub completed: usize,
    pub scheduled: usize,
}

use clap::ValueEnum;
use ratatui::layout::Rect;

/// Which tab is active in the dashboard
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DashboardTab {
    #[default]
    Agents,
    Worktrees,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseRowId {
    Agent(String),
    Worktree(PathBuf),
}

#[derive(Debug, Clone)]
pub struct MouseRowClick {
    pub id: MouseRowId,
    pub at: Instant,
}

/// Current view mode of the dashboard
#[derive(Debug, Default, PartialEq)]
pub enum ViewMode {
    #[default]
    Dashboard,
    Diff(Box<DiffView>),
}

/// A candidate worktree for bulk sweep cleanup.
pub struct SweepCandidate {
    pub handle: String,
    pub path: PathBuf,
    pub reason: SweepReason,
    pub is_dirty: bool,
    pub selected: bool,
}

/// Why a worktree is a sweep candidate.
#[derive(Clone)]
pub enum SweepReason {
    PrMerged,
    PrClosed,
    UpstreamGone,
    MergedLocally,
}

impl SweepReason {
    pub fn label(&self) -> &'static str {
        match self {
            SweepReason::PrMerged => "PR merged",
            SweepReason::PrClosed => "PR closed",
            SweepReason::UpstreamGone => "upstream gone",
            SweepReason::MergedLocally => "merged locally",
        }
    }
}

/// State for the bulk sweep modal.
pub struct SweepState {
    pub candidates: Vec<SweepCandidate>,
    pub cursor: usize,
}

/// Progress state for a background sweep operation.
pub struct SweepProgress {
    pub total: usize,
    pub current: usize,
    pub handle: String,
}

/// An entry in the project picker.
pub struct ProjectEntry {
    pub name: String,
    pub path: PathBuf,
}

/// State for the project picker modal.
pub struct ProjectPicker {
    pub projects: Vec<ProjectEntry>,
    pub cursor: usize,
    pub filter: String,
    pub current_name: Option<String>,
}

impl ProjectPicker {
    /// Return indices into `projects` that match the current filter.
    pub fn filtered(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.projects.len()).collect();
        }
        let lower = self.filter.to_lowercase();
        self.projects
            .iter()
            .enumerate()
            .filter(|(_, p)| p.name.to_lowercase().contains(&lower))
            .map(|(i, _)| i)
            .collect()
    }
}

/// State for the base branch picker modal.
pub struct BaseBranchPicker {
    pub branches: Vec<String>,
    pub cursor: usize,
    pub filter: String,
    /// Current base branch of the selected worktree (highlighted in picker)
    pub current_base: Option<String>,
    /// Branch name of the worktree being edited
    pub worktree_branch: String,
    /// Path to the worktree's repo (for running git commands)
    pub repo_path: PathBuf,
}

impl BaseBranchPicker {
    /// Return indices into `branches` that match the current filter.
    pub fn filtered(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.branches.len()).collect();
        }
        let lower = self.filter.to_lowercase();
        self.branches
            .iter()
            .enumerate()
            .filter(|(_, b)| b.to_lowercase().contains(&lower))
            .map(|(i, _)| i)
            .collect()
    }
}

/// Mode for the add-worktree modal.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum AddWorktreeMode {
    #[default]
    Branch,
    Pr,
}

/// Loading state for the PR list in the add-worktree modal.
pub enum PrListState {
    Loading,
    Loaded { prs: Vec<PrListEntry> },
    Error { message: String },
}

/// State for the add-worktree modal.
pub struct AddWorktreeState {
    /// All local branches (fetched once when modal opens).
    pub branches: Vec<String>,
    /// Branches that already have worktrees (cannot create another).
    pub occupied_branches: std::collections::HashSet<String>,
    /// Cursor position: 0 = "Create new", 1..N = filtered branch index.
    pub cursor: usize,
    /// Filter text (doubles as new branch name).
    pub filter: String,
    /// Original typed prefix preserved during Tab cycling (cleared on typing).
    pub tab_prefix: Option<String>,
    /// Base branch for new worktree creation.
    pub base_branch: String,
    /// Whether the base branch field is being edited (Ctrl+b toggles).
    pub editing_base: bool,
    /// Filter/input text for the base branch field.
    pub base_filter: String,
    /// Tab prefix for base branch cycling.
    pub base_tab_prefix: Option<String>,
    pub repo_path: PathBuf,
    /// Current mode: Branch picker or PR list.
    pub mode: AddWorktreeMode,
    /// PR list state (loaded async when switching to PR mode).
    pub pr_list: Option<PrListState>,
    /// Monotonic counter to discard stale PR list results.
    pub pr_request_counter: u64,
}

/// Outcome of cycling tab completion through branch name candidates.
pub struct BranchCompletion {
    pub branch: String,
    /// Picker cursor when the create row occupies index 0.
    pub picker_cursor: Option<usize>,
}

/// Cycle tab completion to the next fuzzy-matching branch name.
///
/// `exclude_branches` omits occupied branches from candidates (primary picker only).
pub fn cycle_branch_completion(
    branches: &[String],
    tab_prefix: &mut Option<String>,
    filter: &mut String,
    exclude_branches: Option<&HashSet<String>>,
) -> Option<BranchCompletion> {
    if tab_prefix.is_none() {
        *tab_prefix = Some(filter.clone());
    }

    let prefix = tab_prefix.as_deref().unwrap_or(filter.as_str());
    let lower = prefix.to_lowercase();

    let candidates: Vec<usize> = branches
        .iter()
        .enumerate()
        .filter(|(_, b)| {
            fuzzy_match(&lower, &b.to_lowercase())
                && exclude_branches.is_none_or(|ex| !ex.contains(*b))
        })
        .map(|(i, _)| i)
        .collect();
    if candidates.is_empty() {
        return None;
    }

    let current_pos = candidates.iter().position(|&idx| branches[idx] == *filter);

    let next = match current_pos {
        Some(pos) => (pos + 1) % candidates.len(),
        None => 0,
    };

    let branch = branches[candidates[next]].clone();
    Some(BranchCompletion {
        branch,
        picker_cursor: exclude_branches.map(|_| next + 1),
    })
}

/// Background worktree creation job for the add-worktree modal.
pub enum AddWorktreeJob {
    CreateBranch {
        name: String,
        base_branch: Option<String>,
    },
    CheckoutPr {
        pr_number: u32,
    },
}

/// Fuzzy subsequence match: every character in `query` must appear in `target` in order.
/// "mergefail" matches "merge-fail-delete" because m-e-r-g-e-f-a-i-l appear in sequence.
pub fn fuzzy_match(query: &str, target: &str) -> bool {
    let mut target_chars = target.chars();
    for qc in query.chars() {
        if !target_chars.any(|tc| tc == qc) {
            return false;
        }
    }
    true
}

impl AddWorktreeState {
    /// Return indices into `branches` that match the current filter.
    /// Uses fuzzy subsequence matching. Available branches appear first, occupied last.
    pub fn filtered(&self) -> Vec<usize> {
        let text = self.tab_prefix.as_deref().unwrap_or(&self.filter);
        let matches: Vec<usize> = if text.is_empty() {
            (0..self.branches.len()).collect()
        } else {
            let lower = text.to_lowercase();
            self.branches
                .iter()
                .enumerate()
                .filter(|(_, b)| fuzzy_match(&lower, &b.to_lowercase()))
                .map(|(i, _)| i)
                .collect()
        };

        // Sort: available branches first, occupied last
        let mut available: Vec<usize> = Vec::new();
        let mut occupied: Vec<usize> = Vec::new();
        for idx in matches {
            if self.occupied_branches.contains(&self.branches[idx]) {
                occupied.push(idx);
            } else {
                available.push(idx);
            }
        }
        available.extend(occupied);
        available
    }

    /// Number of selectable (non-occupied) entries in `filtered()`.
    /// Since `filtered()` places available branches before occupied ones,
    /// this is the count of leading non-occupied entries.
    pub fn selectable_count(&self) -> usize {
        let filtered = self.filtered();
        filtered
            .iter()
            .take_while(|&&idx| !self.occupied_branches.contains(&self.branches[idx]))
            .count()
    }

    /// If the filter text looks like a PR number, return it.
    /// Matches "#123" or bare "123" (only digits).
    pub fn detected_pr_number(&self) -> Option<u32> {
        let text = self.filter.trim();
        let digits = text.strip_prefix('#').unwrap_or(text);
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            digits.parse().ok()
        } else {
            None
        }
    }

    /// Return indices into the PR list that match the current filter.
    pub fn filtered_prs(&self) -> Vec<usize> {
        let prs = match &self.pr_list {
            Some(PrListState::Loaded { prs, .. }) => prs,
            _ => return Vec::new(),
        };
        if self.filter.is_empty() {
            return (0..prs.len()).collect();
        }
        let lower = self.filter.to_lowercase();
        prs.iter()
            .enumerate()
            .filter(|(_, pr)| {
                fuzzy_match(&lower, &pr.title.to_lowercase())
                    || fuzzy_match(&lower, &pr.head_ref_name.to_lowercase())
                    || pr.number.to_string().contains(&lower)
                    || fuzzy_match(&lower, &pr.author.to_lowercase())
            })
            .map(|(i, _)| i)
            .collect()
    }
}

/// A command entry in the command palette.
pub struct PaletteCommand {
    /// Human-readable label shown in the palette
    pub label: &'static str,
    /// Key hint shown to the right (e.g. "d", "Ctrl+u")
    pub key_hint: &'static str,
    /// The action to dispatch when selected
    pub action: super::super::actions::Action,
}

/// State for the command palette modal.
pub struct CommandPaletteState {
    /// Available commands for the current context
    pub commands: Vec<PaletteCommand>,
    /// Filter text typed by the user
    pub filter: String,
    /// Cursor position in the filtered list
    pub cursor: usize,
}

impl CommandPaletteState {
    pub fn move_next(&mut self) {
        let count = self.filtered().len();
        if count > 0 {
            self.cursor = (self.cursor + 1).min(count - 1);
        }
    }

    pub fn move_previous(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn push_filter_char(&mut self, c: char) {
        self.filter.push(c);
        self.cursor = 0;
    }

    pub fn delete_char(&mut self) {
        self.filter.pop();
        self.cursor = 0;
    }

    pub fn delete_word(&mut self) {
        let trimmed = self.filter.trim_end();
        if let Some(pos) = trimmed.rfind(' ') {
            self.filter.truncate(pos + 1);
        } else {
            self.filter.clear();
        }
        self.cursor = 0;
    }

    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.cursor = 0;
    }

    pub fn selected_action(&self) -> Option<super::super::actions::Action> {
        self.filtered()
            .get(self.cursor)
            .map(|&idx| self.commands[idx].action.clone())
    }

    /// Return indices into `commands` that match the current filter, sorted by relevance.
    pub fn filtered(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.commands.len()).collect();
        }
        let query = self.filter.to_lowercase();
        let mut scored: Vec<(usize, i32)> = self
            .commands
            .iter()
            .enumerate()
            .filter_map(|(i, cmd)| {
                let target = cmd.label.to_lowercase();
                fuzzy_score(&query, &target).map(|score| (i, score))
            })
            .collect();
        scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(i, _)| i).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowContextKind {
    Agent,
    Worktree,
}

pub struct RowContextMenu {
    pub kind: RowContextKind,
    pub commands: Vec<PaletteCommand>,
    pub cursor: usize,
    pub column: u16,
    pub row: u16,
}

impl RowContextMenu {
    pub fn move_next(&mut self) {
        if !self.commands.is_empty() {
            self.cursor = (self.cursor + 1).min(self.commands.len() - 1);
        }
    }

    pub fn move_previous(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn selected_action(&self) -> Option<super::super::actions::Action> {
        self.commands
            .get(self.cursor)
            .map(|command| command.action.clone())
    }

    pub fn action_for_hint(&self, hint: &str) -> Option<super::super::actions::Action> {
        self.commands
            .iter()
            .find(|command| command.key_hint == hint)
            .map(|command| command.action.clone())
    }

    pub fn area(&self, terminal: Rect) -> Rect {
        let content_width = self
            .commands
            .iter()
            .map(|command| 4 + command.label.len() + command.key_hint.len())
            .max()
            .unwrap_or(20);
        let width = (content_width as u16 + 2).clamp(24, 48).min(terminal.width);
        let height = (self.commands.len() as u16 + 2).min(terminal.height);
        let x = self
            .column
            .min(terminal.right().saturating_sub(width))
            .max(terminal.x);
        let y = self
            .row
            .min(terminal.bottom().saturating_sub(height))
            .max(terminal.y);
        Rect::new(x, y, width, height)
    }

    pub fn item_at(&self, terminal: Rect, column: u16, row: u16) -> Option<usize> {
        let area = self.area(terminal);
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        if column < inner.x || column >= inner.right() || row < inner.y || row >= inner.bottom() {
            return None;
        }
        let index = usize::from(row - inner.y);
        (index < self.commands.len()).then_some(index)
    }
}

/// Score a fuzzy match: higher is better, None means no match.
/// Exact prefix > word-boundary match > substring > subsequence.
fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    // Exact prefix match
    if target.starts_with(query) {
        return Some(1000 + query.len() as i32);
    }
    // Word start match (query matches start of a word in target)
    let words: Vec<&str> = target.split_whitespace().collect();
    for word in &words {
        if word.starts_with(query) {
            return Some(800 + query.len() as i32);
        }
    }
    // Substring match
    if target.contains(query) {
        return Some(500 + query.len() as i32);
    }
    // Fuzzy subsequence match with scoring
    let mut target_chars = target.chars().peekable();
    let mut score = 0i32;
    let mut matched = 0;
    let mut prev_matched = false;
    for qc in query.chars() {
        let mut found = false;
        for tc in target_chars.by_ref() {
            if tc == qc {
                matched += 1;
                // Bonus for consecutive matches
                if prev_matched {
                    score += 5;
                }
                prev_matched = true;
                found = true;
                break;
            }
            prev_matched = false;
        }
        if !found {
            return None;
        }
    }
    if matched == query.len() {
        Some(score + matched as i32)
    } else {
        None
    }
}

/// Plan for a pending worktree removal (shown in confirmation modal).
pub struct RemovePlan {
    pub handle: String,
    pub path: PathBuf,
    pub is_dirty: bool,
    pub is_unmerged: bool,
    pub keep_branch: bool,
    pub force_armed: bool,
}

/// Tracks active removals and suppresses completed rows in stale list snapshots.
#[derive(Default)]
pub struct WorktreeRemovals {
    pending: HashSet<PathBuf>,
    completed: HashSet<PathBuf>,
}

impl WorktreeRemovals {
    pub fn contains(&self, path: &std::path::Path) -> bool {
        self.pending.contains(path)
    }

    pub fn start(&mut self, path: PathBuf) -> bool {
        self.pending.insert(path)
    }

    pub fn finish(&mut self, path: &std::path::Path, removed: bool) {
        self.pending.remove(path);
        if removed {
            self.completed.insert(path.to_path_buf());
        }
    }

    pub fn filter_snapshot(&mut self, worktrees: &mut Vec<WorktreeInfo>) {
        self.completed
            .retain(|path| worktrees.iter().any(|wt| wt.path == *path));
        worktrees.retain(|wt| !self.completed.contains(&wt.path));
    }
}

#[cfg(test)]
mod removal_tests {
    use super::*;

    fn worktree(path: &str) -> WorktreeInfo {
        WorktreeInfo {
            repo_key: "/repo/.git".into(),
            handle: "feature".into(),
            branch: "feature".into(),
            path: path.into(),
            is_main: false,
            mode: crate::config::MuxMode::Window,
            has_mux_window: false,
            has_unmerged: false,
            pr_info: None,
            agent_status: None,
            created_at: None,
            base_branch: None,
        }
    }

    #[test]
    fn removal_tracks_paths_and_rejects_duplicates() {
        let mut state = WorktreeRemovals::default();
        let first = PathBuf::from("/repo-a/feature");
        let second = PathBuf::from("/repo-b/feature");
        assert!(state.start(first.clone()));
        assert!(!state.start(first.clone()));
        assert!(state.start(second.clone()));
        state.finish(&first, false);
        assert!(!state.contains(&first));
        assert!(state.contains(&second));
        assert!(state.start(first));
    }

    #[test]
    fn completed_removal_suppresses_stale_snapshots_until_absent() {
        let mut state = WorktreeRemovals::default();
        let wt = worktree("/repo/feature");
        state.start(wt.path.clone());
        state.finish(&wt.path, true);
        let mut stale = vec![wt.clone()];
        state.filter_snapshot(&mut stale);
        assert!(stale.is_empty());
        state.filter_snapshot(&mut Vec::new());
        let mut recreated = vec![wt];
        state.filter_snapshot(&mut recreated);
        assert_eq!(recreated.len(), 1);
    }

    #[test]
    fn failed_or_scheduled_removal_keeps_the_row() {
        let mut state = WorktreeRemovals::default();
        let wt = worktree("/repo/feature");
        state.start(wt.path.clone());
        state.finish(&wt.path, false);
        let mut snapshot = vec![wt];
        state.filter_snapshot(&mut snapshot);
        assert_eq!(snapshot.len(), 1);
        assert!(!state.contains(&snapshot[0].path));
    }
}
