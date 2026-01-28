---
description: Open or switch to a tmux window for an existing worktree
---

# open

Opens or switches to a tmux window for a pre-existing git worktree. If the window already exists, switches to it. If not, creates a new window with the configured pane layout and environment.

```bash
workmux open [name] [flags]
```

## Arguments

- `[name]`: Worktree name (the directory name, which is also the tmux window name without the prefix). Optional with `--new` when run from inside a worktree.

## Options

| Flag                       | Description                                                                                                                                                                              |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `-n, --new`                | Force opening in a new window even if one already exists. Creates a duplicate window with a suffix (e.g., `-2`, `-3`). Useful for having multiple terminal views into the same worktree. |
| `--run-hooks`              | Re-runs the `post_create` commands (these block window creation).                                                                                                                        |
| `--force-files`            | Re-applies file copy/symlink operations. Useful for restoring a deleted `.env` file.                                                                                                     |
| `-p, --prompt <text>`      | Provide an inline prompt for AI agent panes.                                                                                                                                             |
| `-P, --prompt-file <path>` | Provide a path to a file containing the prompt.                                                                                                                                          |
| `-e, --prompt-editor`      | Open your editor to write the prompt interactively.                                                                                                                                      |

## What happens

1. Verifies that a worktree with `<name>` exists.
2. If a tmux window exists and `--new` is not set, switches to it.
3. Otherwise, creates a new tmux window (with suffix if duplicating). If the worktree was originally created with `--session`, the window is recreated in its own session.
4. (If specified) Runs file operations and `post_create` hooks.
5. Sets up your configured tmux pane layout.
6. Automatically switches your tmux client to the new window.

## Examples

```bash
# Open or switch to a window for an existing worktree
workmux open user-auth

# Force open a second window for the same worktree (creates user-auth-2)
workmux open user-auth --new

# Open a new window for the current worktree (run from within the worktree)
workmux open --new

# Open with a prompt for AI agents
workmux open user-auth -p "Continue implementing the login flow"

# Open and re-run dependency installation
workmux open user-auth --run-hooks

# Open and restore configuration files
workmux open user-auth --force-files
```
