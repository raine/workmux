---
description: Remove worktrees, tmux windows, and branches without merging
---

# remove

Removes worktrees, tmux windows, and branches without merging (unless you keep the branches). Useful for abandoning work or cleaning up experimental branches. Supports removing multiple worktrees in a single command. Alias: `rm`

```bash
workmux remove [name]... [flags]
```

## Arguments

- `[name]...`: One or more worktree names (the directory names). Defaults to current directory name if omitted.

## Options

| Flag                | Description                                                                                                                                                                      |
| ------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--all`             | Remove all worktrees at once (except the main worktree). Prompts for confirmation unless `--force` is used. Safely skips worktrees with uncommitted changes or unmerged commits. |
| `--gone`            | Remove worktrees whose upstream remote branch has been deleted (e.g., after a PR is merged on GitHub). Automatically runs `git fetch --prune` (or `jj git fetch` for jj repos) first. |
| `--force, -f`       | Skip confirmation prompt and ignore uncommitted changes.                                                                                                                         |
| `--keep-branch, -k` | Remove only the worktree and tmux window while keeping the local branch.                                                                                                         |

## Examples

```bash
# Remove the current worktree (run from within the worktree)
workmux remove

# Remove a specific worktree with confirmation if unmerged
workmux remove experiment

# Remove multiple worktrees at once
workmux rm feature-a feature-b feature-c

# Remove multiple worktrees with force (no confirmation)
workmux rm -f old-work stale-branch

# Use the alias
workmux rm old-work

# Remove worktree/window but keep the branch
workmux remove --keep-branch experiment

# Force remove without prompts
workmux rm -f experiment

# Remove worktrees whose remote branches were deleted (e.g., after PR merge)
workmux rm --gone

# Force remove all gone worktrees (no confirmation)
workmux rm --gone -f

# Remove all worktrees at once
workmux rm --all
```
