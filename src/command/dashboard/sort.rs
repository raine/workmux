//! Sort mode logic for the dashboard agent list.

use crate::state::StateStore;

/// Available sort modes for the agent list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    /// Sort by agent status importance (Waiting > Done > Working > Stale)
    #[default]
    Priority,
    /// Group agents by project name, then by status within each project
    Project,
    /// Sort by duration since last status change (newest first)
    Recency,
    /// Natural tmux order (by pane_id)
    Natural,
}

impl SortMode {
    /// Cycle to the next sort mode
    pub fn next(self) -> Self {
        match self {
            SortMode::Priority => SortMode::Project,
            SortMode::Project => SortMode::Recency,
            SortMode::Recency => SortMode::Natural,
            SortMode::Natural => SortMode::Priority,
        }
    }

    /// Get the display name for the sort mode
    pub fn label(&self) -> &'static str {
        match self {
            SortMode::Priority => "priority",
            SortMode::Project => "project",
            SortMode::Recency => "recency",
            SortMode::Natural => "natural",
        }
    }

    /// Convert to string for storage.
    fn as_str(&self) -> &'static str {
        match self {
            SortMode::Priority => "priority",
            SortMode::Project => "project",
            SortMode::Recency => "recency",
            SortMode::Natural => "natural",
        }
    }

    /// Parse from storage string.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "project" => SortMode::Project,
            "recency" => SortMode::Recency,
            "natural" => SortMode::Natural,
            _ => SortMode::Priority, // Default fallback
        }
    }

    /// Load sort mode from StateStore, falling back to `default` when unset.
    pub fn load_with_default(default: Self) -> Self {
        StateStore::new()
            .ok()
            .and_then(|store| store.load_settings().ok())
            .map(|s| Self::parse(&s.sort_mode))
            .unwrap_or(default)
    }

    /// Save sort mode to StateStore.
    pub fn save(&self) {
        if let Ok(store) = StateStore::new()
            && let Ok(mut settings) = store.load_settings()
        {
            settings.sort_mode = self.as_str().to_string();
            let _ = store.save_settings(&settings);
        }
    }
}

/// Available sort modes for the worktree list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorktreeSortMode {
    /// Git worktree list order
    #[default]
    Natural,
    /// Sort by creation time (newest first)
    Age,
}

impl WorktreeSortMode {
    pub fn next(self) -> Self {
        match self {
            WorktreeSortMode::Natural => WorktreeSortMode::Age,
            WorktreeSortMode::Age => WorktreeSortMode::Natural,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            WorktreeSortMode::Natural => "natural",
            WorktreeSortMode::Age => "age",
        }
    }

    fn as_str(&self) -> &'static str {
        self.label()
    }

    fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "age" => WorktreeSortMode::Age,
            _ => WorktreeSortMode::Natural,
        }
    }

    pub fn load() -> Self {
        StateStore::new()
            .ok()
            .and_then(|store| store.load_settings().ok())
            .and_then(|s| s.worktree_sort_mode)
            .map(|s| Self::from_str(&s))
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(store) = StateStore::new()
            && let Ok(mut settings) = store.load_settings()
        {
            settings.worktree_sort_mode = Some(self.as_str().to_string());
            let _ = store.save_settings(&settings);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SortMode;

    #[test]
    fn parse_known_modes() {
        assert_eq!(SortMode::parse("priority"), SortMode::Priority);
        assert_eq!(SortMode::parse("project"), SortMode::Project);
        assert_eq!(SortMode::parse("recency"), SortMode::Recency);
        assert_eq!(SortMode::parse("natural"), SortMode::Natural);
    }

    #[test]
    fn parse_is_case_insensitive_and_trims() {
        assert_eq!(SortMode::parse("  Recency "), SortMode::Recency);
    }

    #[test]
    fn parse_unknown_falls_back_to_priority() {
        assert_eq!(SortMode::parse(""), SortMode::Priority);
        assert_eq!(SortMode::parse("bogus"), SortMode::Priority);
    }
}
