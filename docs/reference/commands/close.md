---
description: Close a tmux window without removing the worktree or branch
---

# close

Closes the tmux window for a worktree without removing the worktree or branch. This is useful when you want to temporarily close a window to reduce clutter or free resources, but plan to return to the work later.

```bash
workmux close [name]
```

## Arguments

- `[name]`: Optional worktree name (the directory name). Defaults to current directory if omitted.

## Examples

```bash
# Close the window for a specific worktree
workmux close user-auth

# Close the current worktree's window (run from within the worktree)
workmux close
```

To reopen the window later, use [`workmux open`](./open).

::: tip
You can also use tmux's native kill-window command (default: `prefix + &`) to close a worktree's window with the same effect. For worktrees created with `--session`, this closes the entire session.
:::
