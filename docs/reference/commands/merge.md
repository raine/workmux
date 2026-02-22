---
description: Merge a branch and clean up the worktree, window, and local branch
---

# merge

Merges a branch into a target branch (main by default) and automatically cleans up all associated resources (worktree, tmux window, and local branch).

```bash
workmux merge [branch-name] [flags]
```

::: tip When to use `merge` vs `remove`
`workmux merge` performs the git merge locally. Use it when you want to merge directly without a pull request.

If your workflow uses pull requests, the merge happens on the remote after review. In that case, use [`workmux remove`](remove.md) to clean up the worktree after your PR is merged.
:::

## Arguments

- `[branch-name]`: Optional name of the branch to merge. If omitted, automatically detects the current branch from the worktree you're in.

## Options

| Flag                   | Description                                                                                                                                                                                                                                              |
| ---------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--into <branch>`      | Merge into the specified branch instead of main. Useful for stacked PRs, git-flow workflows, or merging subtasks into a parent feature branch. If the target branch has its own worktree, the merge happens there; otherwise, the main worktree is used. |
| `--ignore-uncommitted` | Commit any staged changes before merging without opening an editor.                                                                                                                                                                                      |
| `--keep, -k`           | Keep the worktree, window, and branch after merging (skip cleanup). Useful when you want to verify the merge before cleaning up.                                                                                                                         |
| `--notification`       | Show a system notification on successful merge. Useful when delegating merge to an AI agent and you want to be notified when it completes.                                                                                                               |
| `--rebase`             | Rebase the feature branch onto the target before merging (creates a linear history via fast-forward merge). If conflicts occur, you'll need to resolve them manually and run `git rebase --continue`.                                                    |
| `--squash`             | Squash all commits from the feature branch into a single commit on the target. You'll be prompted to provide a commit message in your editor.                                                                                                            |

## Merge strategies

By default, `workmux merge` performs a standard merge commit (configurable via `merge_strategy`). You can override the configured behavior with these mutually exclusive flags:

- `--rebase`: Rebase the feature branch onto the target before merging (creates a linear history via fast-forward merge). If conflicts occur, you'll need to resolve them manually in the worktree and run `git rebase --continue`. For jj repos, this uses `jj rebase`.
- `--squash`: Squash all commits from the feature branch into a single commit on the target. You'll be prompted to provide a commit message in your editor. For jj repos, this uses `jj squash`.

If you don't want to have merge commits in your main branch, use the `rebase` merge strategy, which does `--rebase` by default.

```yaml
# ~/.config/workmux/config.yaml
merge_strategy: rebase
```

## What happens

1. Determines which branch to merge (specified branch or current branch if omitted)
2. Determines the target branch (`--into` or main branch from config)
3. Checks for uncommitted changes (errors if found, unless `--ignore-uncommitted` is used)
4. Commits staged changes if present (unless `--ignore-uncommitted` is used)
5. Merges your branch into the target using the selected strategy (default: merge commit)
6. Deletes the tmux window (including the one you're currently in if you ran this from a worktree) — skipped if `--keep` is used
7. Removes the worktree — skipped if `--keep` is used
8. Deletes the local branch — skipped if `--keep` is used

## Typical workflow

When you're done working in a worktree, simply run `workmux merge` from within that worktree's tmux window. The command will automatically detect which branch you're on, merge it into main, and close the current window as part of cleanup.

## Examples

```bash
# Merge branch into main (default: merge commit)
workmux merge user-auth

# Merge the current worktree you're in
# (run this from within the worktree's tmux window)
workmux merge

# Rebase onto main before merging for a linear history
workmux merge user-auth --rebase

# Squash all commits into a single commit
workmux merge user-auth --squash

# Merge but keep the worktree/window/branch to verify before cleanup
workmux merge user-auth --keep
# ... verify the merge in main ...
workmux remove user-auth  # clean up later when ready

# Merge into a different branch (stacked PRs)
workmux merge feature/subtask --into feature/parent
```
