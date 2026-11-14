//! Background thread spawning for git and GitHub status fetches.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::git;

use super::App;
use super::types::AppEvent;

/// Group branches by repository identity while retaining a checkout for gh.
fn group_pr_requests<'a>(
    requests: impl IntoIterator<Item = (&'a PathBuf, &'a PathBuf, &'a str)>,
) -> HashMap<PathBuf, (PathBuf, Vec<String>)> {
    let mut grouped: HashMap<PathBuf, (PathBuf, Vec<String>)> = HashMap::new();
    for (key, path, branch) in requests {
        let (_, branches) = grouped
            .entry(key.clone())
            .or_insert_with(|| (path.clone(), Vec::new()));
        branches.push(branch.to_string());
    }
    for (_, branches) in grouped.values_mut() {
        branches.sort();
        branches.dedup();
    }
    grouped
}

impl App {
    /// Spawn a background thread to fetch git status for all agent worktrees
    pub(super) fn spawn_git_status_fetch(&self) {
        // Skip if a fetch is already in progress (prevents thread pile-up)
        if self
            .is_git_fetching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let tx = self.event_tx.clone();
        let is_fetching = self.is_git_fetching.clone();
        let main_branch = self.config.main_branch.clone();
        // Include both agent paths and worktree paths so the worktree view gets git status too
        let mut paths: Vec<PathBuf> = self
            .all_agents
            .iter()
            .filter(|a| a.remote.is_none())
            .map(|a| a.path.clone())
            .collect();
        for wt in &self.worktrees {
            if !paths.contains(&wt.path) {
                paths.push(wt.path.clone());
            }
        }

        std::thread::spawn(move || {
            // Reset flag when thread completes (even on panic)
            struct ResetFlag(Arc<AtomicBool>);
            impl Drop for ResetFlag {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _reset = ResetFlag(is_fetching);

            for path in paths {
                let status = git::get_git_status(&path, main_branch.as_deref());
                let _ = tx.send(AppEvent::GitStatus(path, status));
            }
        });
    }

    /// Spawn a background thread to fetch GitHub status for all repos.
    /// Returns true if a fetch was started, false if one is already in progress.
    pub(super) fn spawn_pr_status_fetch(&self) -> bool {
        // Skip if already fetching
        if self
            .is_pr_fetching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }

        // Use unfiltered snapshots so filtering cannot omit branches from a fetch.
        let worktree_requests = self
            .all_worktrees
            .iter()
            .map(|wt| (&wt.repo_key, &wt.path, wt.branch.as_str()));
        let agent_requests = self.all_agents.iter().filter_map(|agent| {
            let branch = self.git_statuses.get(&agent.path)?.branch.as_deref()?;
            let key = self.repo_keys.get(&agent.path)?;
            Some((key, &agent.path, branch))
        });
        let repo_branches = group_pr_requests(worktree_requests.chain(agent_requests));

        if repo_branches.is_empty() {
            self.is_pr_fetching.store(false, Ordering::SeqCst);
            // Return false so the caller doesn't reset the timer — no fetch
            // was actually started, and we want to retry once git statuses
            // or worktree data arrive.
            return false;
        }

        let tx = self.event_tx.clone();
        let is_fetching = self.is_pr_fetching.clone();

        let priority_path = self
            .worktree_project_override
            .as_ref()
            .map(|(_, p)| p.clone())
            .or_else(|| self.current_worktree.clone());

        std::thread::spawn(move || {
            struct ResetFlag(Arc<AtomicBool>);
            impl Drop for ResetFlag {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _reset = ResetFlag(is_fetching);

            let priority_repo = priority_path
                .as_deref()
                .and_then(|path| git::get_repository_key_in(Some(path)).ok());

            // Sort repos so the priority repo (current project) is fetched first
            let mut repos: VecDeque<_> = repo_branches.into_iter().collect();
            if let Some(ref priority) = priority_repo {
                repos
                    .make_contiguous()
                    .sort_by_key(|(repo, _)| repo != priority);
            }

            // Fetch repos in parallel with bounded concurrency
            let queue = Arc::new(Mutex::new(repos));
            let workers = queue.lock().unwrap().len().min(4);

            std::thread::scope(|s| {
                for _ in 0..workers {
                    let queue = Arc::clone(&queue);
                    let tx = tx.clone();
                    s.spawn(move || {
                        loop {
                            let Some((repo_key, (checkout, branches))) =
                                queue.lock().unwrap().pop_front()
                            else {
                                break;
                            };
                            match crate::github::list_branch_summaries(&checkout, &branches) {
                                Ok(summaries) => {
                                    let _ = tx.send(AppEvent::GithubStatus(repo_key, summaries));
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Failed to fetch GitHub status for {:?}: {}",
                                        checkout,
                                        e
                                    );
                                }
                            }
                        }
                    });
                }
            });
        });

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_use_stable_keys_and_checkout_paths_without_main_rows() {
        let key = PathBuf::from("/project/.git");
        let first = PathBuf::from("/project/worktrees/branch-1");
        let second = PathBuf::from("/project/worktrees/branch-2");
        let agent = second.join("src");
        let grouped = group_pr_requests([
            (&key, &first, "branch-1"),
            (&key, &second, "branch-2"),
            (&key, &agent, "branch-2"),
        ]);
        assert_eq!(grouped.len(), 1);
        assert_eq!(
            grouped[&key],
            (first, vec!["branch-1".into(), "branch-2".into()])
        );
        let refreshed = group_pr_requests([(&key, &second, "branch-2")]);
        assert_eq!(refreshed[&key], (second, vec!["branch-2".into()]));
    }

    #[test]
    fn same_named_repositories_have_separate_requests() {
        let key_a = PathBuf::from("/a/project/.git");
        let key_b = PathBuf::from("/b/project/.git");
        let checkout_a = PathBuf::from("/a/project/worktrees/feature");
        let checkout_b = PathBuf::from("/b/project/worktrees/feature");
        let grouped = group_pr_requests([
            (&key_a, &checkout_a, "feature"),
            (&key_b, &checkout_b, "feature"),
        ]);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[&key_a].0, checkout_a);
        assert_eq!(grouped[&key_b].0, checkout_b);
    }
}
