//! Data types for the dashboard application state.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::git::GitStatus;
use crate::github::{PrListEntry, PrSummary};
use crate::workflow::types::WorktreeInfo;

use super::super::diff::DiffView;

/// Unified event type for the dashboard event loop.
/// All background threads and the input thread send events through a single channel.
pub enum AppEvent {
    /// Terminal input event (from dedicated input thread)
    Terminal(crossterm::event::Event),
    /// Git status update for a worktree path
    GitStatus(PathBuf, GitStatus),
    /// PR status update for a repo root
    PrStatus(PathBuf, HashMap<String, PrSummary>),
    /// Full worktree list from background fetch
    WorktreeList(Vec<WorktreeInfo>),
    /// Git log preview for a worktree path
    WorktreeLog(PathBuf, String),
    /// Result of a background add-worktree operation
    AddWorktreeResult(Result<String, String>),
    /// Result of fetching open PRs for the add-worktree modal
    AddWorktreePrList(u64, Result<Vec<PrListEntry>, String>),
    /// Progress update during background sweep (current, total, handle)
    SweepProgressUpdate(usize, usize, String),
    /// Sweep operation completed
    SweepComplete(Result<(), String>),
}

use clap::ValueEnum;

/// Which tab is active in the dashboard
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DashboardTab {
    #[default]
    Agents,
    Worktrees,
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
