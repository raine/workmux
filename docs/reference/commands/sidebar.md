---
description: Toggle a live agent status sidebar in tmux
---

# sidebar

Toggles a live agent status sidebar on the left side of all tmux windows.
Shows all active agents across all sessions and projects with live status
updates.

```bash
workmux sidebar            # Toggle sidebar on/off (all sessions)
workmux sidebar --session  # Toggle sidebar for current session only
```

## What it shows

Each agent row displays:

- Status icon (working/waiting/done with spinner animation)
- Optional agent identity badge (Claude, Codex, OpenCode, Gemini, etc.)
- Project and worktree name (e.g. `myproject/fix-bug`)
- Elapsed time since last status change

## Keybindings

| Key     | Action                   |
| ------- | ------------------------ |
| `j`/`k` | Navigate up/down         |
| `Enter` | Jump to agent pane       |
| `g`/`G` | Jump to first/last       |
| `v`     | Toggle layout mode       |
| `z`     | Toggle sleeping on agent |
| `q`     | Quit sidebar             |

## Navigation commands

Switch between agents from any tmux pane, in the same order shown in the
sidebar:

| Command                    | Action                               |
| -------------------------- | ------------------------------------ |
| `workmux sidebar next`     | Switch to the next agent (wraps)     |
| `workmux sidebar prev`     | Switch to the previous agent (wraps) |
| `workmux sidebar jump <N>` | Jump to the Nth agent (1-indexed)    |

### Example tmux keybindings

```bash
# Alt+j / Alt+k to cycle agents (no prefix needed)
bind -n M-j run-shell "workmux sidebar next"
bind -n M-k run-shell "workmux sidebar prev"

# Alt+1..9 to jump directly
bind -n M-1 run-shell "workmux sidebar jump 1"
bind -n M-2 run-shell "workmux sidebar jump 2"
bind -n M-3 run-shell "workmux sidebar jump 3"
# ...

# Or with prefix key (avoids terminal conflicts)
bind C-j run-shell "workmux sidebar next"
bind C-k run-shell "workmux sidebar prev"
```

## Configuration

```yaml
sidebar:
  width: 40 # absolute columns (default: "10%", clamped 25-50)
  # width: "15%"  # or percentage of terminal width
  layout: tiles # "compact" or "tiles" (default)
  agent_identity: both # "off" (default), "icon", "label", or "both"
  agent_icons:
    claude: "✳"
    codex: "CX"
    opencode: "OC"
```

Explicit width values bypass the default 25-50 column clamp (minimum 10
columns). Layout preference can also be toggled at runtime with `v` and is
persisted across restarts.

Agent identity badges are detected from the command recorded by status tracking.
If the sidebar is too narrow, worktree names take priority and the identity badge
is hidden or shortened.

## How it works

When enabled, a background daemon polls tmux state every 2 seconds and pushes
snapshots to each sidebar pane over a Unix socket. The sidebar creates a narrow
tmux pane on the left side of every existing window using a full-height split.
A tmux hook (`after-new-window`) ensures newly created windows also get a
sidebar automatically.

Running `workmux sidebar` again disables the sidebar globally, killing all
sidebar panes, the daemon, and removing hooks.

### Session-scoped mode

By default, the sidebar appears in all tmux sessions. Use `--session` to scope
it to the current session only, leaving other sessions untouched:

```bash
workmux sidebar --session  # Enable in current session only
workmux sidebar --session  # Run again to disable
```

You can enable session-scoped sidebars in multiple sessions independently. Each
session can be toggled on/off without affecting others.

Session-scoped and global modes cannot be mixed. Attempting to enable one while
the other is active will show an error.

## Limitations

- tmux only (other backends are not supported yet)

## Example tmux binding

```bash
bind C-t run-shell "workmux sidebar"
```
