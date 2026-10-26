---
title: "open"
description: Open or switch to a tmux window for an existing worktree
---

Opens or switches to a tmux window for a pre-existing git worktree. If the window already exists, switches to it. If not, creates a new window with the configured pane layout and environment. Accepts multiple names to open several worktrees at once.

```bash
workmux open [name...] [flags]
```

## Arguments

- `[name...]`: One or more worktree names, matching the worktree directory or handle. The tmux target can differ when a worktree uses `--target-name` or `--parent-session`. Optional with `--new` when run from inside a worktree.

## Options

| Flag                       | Description                                                                                                                                                                                                                                                                                                              |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `-n, --new`                | Force opening in a new window even if one already exists. Creates a duplicate window with a suffix (e.g., `-2`, `-3`). Useful for having multiple terminal views into the same worktree. In tmux, the duplicate appears immediately to the right of the window where the command runs. Cannot be used with session mode. |
| `--mode <window\|session>` | Override the multiplexer mode for this command. `session` persists the mode change for subsequent opens. `window` converts a session-mode worktree back to window mode. Session mode is only supported with tmux.                                                                                                        |
| `-s, --session`            | Shorthand for `--mode session`. Persists the mode change for subsequent opens. Cannot be combined with `--mode`.                                                                                                                                                                                                         |
| `--target-name <name>`     | Override the workmux-managed tmux target name for this command. In window mode, creates or selects window `<window_prefix><name>`. In session mode, creates or selects session `<window_prefix><name>`. Cannot be used with multiple worktree names.                                                                     |
| `--parent-session <name>`  | Window mode only. Creates the workmux-managed window inside the named tmux session without applying `window_prefix` to that parent session. Cannot be used with session mode or multiple worktree names.                                                                                                                 |
| `--config <path>`          | Use an alternate config file for this invocation. Still merges with global config. Useful for per-command config overrides like `workmux open feat/my-branch --config /path/to/workmux.session.yaml`.                                                                                                                    |
| `--run-hooks`              | Re-runs the `post_create` commands (these block window creation).                                                                                                                                                                                                                                                        |
| `--force-files`            | Re-applies file copy/symlink operations. Useful for restoring a deleted `.env` file.                                                                                                                                                                                                                                     |
| `-p, --prompt <text>`      | Provide an inline prompt for AI agent panes.                                                                                                                                                                                                                                                                             |
| `-P, --prompt-file <path>` | Provide a path to a file containing the prompt.                                                                                                                                                                                                                                                                          |
| `-c, --continue`           | Resume the agent's most recent conversation in this worktree. Injects the appropriate flag for the configured agent (e.g., `--continue` for Claude, `--resume` for Gemini).                                                                                                                                              |
| `-e, --prompt-editor`      | Open your editor to write the prompt interactively.                                                                                                                                                                                                                                                                      |
| `--prompt-file-only`       | Write the prompt file to the worktree without injecting it into agent commands.                                                                                                                                                                                                                                          |

## What happens

1. Verifies that a worktree with `<name>` exists.
2. If the target exists and `--new` is not set, switches to it.
3. Otherwise, creates a new tmux window or session. In tmux, duplicate windows appear immediately to the right of the window identified by `$TMUX_PANE`. `--target-name` can override the managed target, and `--parent-session` can choose the parent session for a window-mode target.
4. (If specified) Runs file operations and `post_create` hooks.
5. Sets up your configured tmux pane layout.
6. Automatically switches your tmux client to the new window.

In tmux window mode without `--parent-session`, `$TMUX_PANE` identifies the
calling pane and its parent session. When `$TMUX_PANE` is absent or stale,
workmux uses live pane working directories when their closest matches identify
one session. A tmux server with one session is the final fallback. Remaining
ambiguity requires `--parent-session <name>`. The working directory selects the
Git repository and only provides conservative evidence for tmux placement.

## Remote workspaces

For a workspace on a [remote server](/guide/remote/), `open` reopens the local window and reattaches it to the remote session. The agent was never stopped by `close`, so nothing restarts. If the remote session has ended, `open` starts the workspace's panes again. After a remote host reboot, agents resume their most recent conversation, and `open` says whether the agent continued its conversation or started a new one.

Prompts, `--continue`, `--new`, `--run-hooks`, `--force-files`, `--target-name`, `--parent-session` and session mode aren't available for remote workspaces. With `mode: session` in the config, pass `--mode window` to open a remote workspace.

## Examples

```bash
# Open or switch to a window for an existing worktree
workmux open user-auth

# Force open a second window for the same worktree (creates user-auth-2)
workmux open user-auth --new

# Open a new window for the current worktree (run from within the worktree)
workmux open --new

# Open in session mode (converts from window mode if needed)
workmux open user-auth --mode session

# Convert a session-mode worktree back to a window
workmux open user-auth --mode window

# Recreate a closed worktree with a custom window target
workmux open user-auth --target-name review-auth

# Recreate a window-mode worktree inside a named tmux session
workmux open user-auth --parent-session prs --target-name review-auth

# Recreate a worktree as a custom-named session
workmux open user-auth --mode session --target-name review-auth

# Resume the agent's last conversation
workmux open user-auth --continue

# Resume and send a follow-up prompt
workmux open user-auth --continue -p "Continue implementing the login flow"

# Open and re-run dependency installation
workmux open user-auth --run-hooks

# Open and restore configuration files
workmux open user-auth --force-files

# Open multiple worktrees at once
workmux open user-auth api-refactor bugfix-login
```
