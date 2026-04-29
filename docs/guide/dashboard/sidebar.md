---
description: A persistent agent status sidebar for tmux windows
---

# Sidebar

The sidebar provides an always-visible agent overview pinned to the left edge of
every tmux window. Unlike the dashboard (which is a full-screen TUI you open on
demand), the sidebar stays on screen while you work.

<div style="display: flex; justify-content: center; margin: 1.5rem 0;">
  <img src="/sidebar.webp" alt="workmux sidebar" style="border-radius: 4px;">
</div>

## Setup

::: warning Prerequisites
The sidebar requires [status tracking hooks](/guide/status-tracking) to be
configured and tmux as the backend.
:::

Toggle the sidebar with:

```bash
workmux sidebar            # All sessions (default)
workmux sidebar --session  # Current session only
```

By default, the sidebar appears in all existing and newly created tmux windows
across all sessions. Use `--session` to scope it to the current session only,
leaving other sessions untouched. Running the command again disables it.

Optionally, add a tmux binding for quick access:

```bash
bind C-t run-shell "workmux sidebar"
```

## What it shows

Each agent is displayed as a tile showing:

- Status icon with spinner animation (working/waiting/done)
- Optional agent identity badge (Claude, Codex, OpenCode, Gemini, etc.)
- Worktree name and elapsed time since last status change
- Project name and git diff stats (committed + uncommitted lines)
- Agent task description

## Configuration

Configure the sidebar in your global `~/.config/workmux/config.yaml` or project
`.workmux.yaml`:

```yaml
sidebar:
  # Width: absolute columns or percentage of terminal width
  width: 40 # absolute columns
  # width: "15%"  # percentage of terminal width

  # Layout mode: "compact" or "tiles" (default)
  layout: tiles

  # Agent identity badge: "off" (default), "icon", "label", or "both"
  agent_identity: both

  # Optional custom identity icons
  agent_icons:
    claude: "✳"
    codex: "CX"
    opencode: "OC"
```

Width defaults to 10% of terminal width, clamped between 25 and 50 columns.
When set explicitly, the clamp is removed (minimum 10 columns).

Agent identity badges are detected from the command recorded by status tracking.
The sidebar currently recognizes Claude, Codex, OpenCode, Gemini, Copilot, Pi,
Kiro, and Vibe agents. When space is tight, the sidebar preserves the worktree
name first and hides or shortens the identity badge.

## Layout modes

The sidebar supports two layout modes, toggled with `v`:

- **Tiles** (default): variable-height cards with status stripe
- **Compact**: single line per agent

Your preference is persisted across tmux restarts.

## Mouse support

Click an agent tile to jump to its pane, or scroll to navigate the list. Requires
`set -g mouse on` in your `~/.tmux.conf`.

## Keybindings

| Key     | Action                   |
| ------- | ------------------------ |
| `j`/`k` | Navigate up/down         |
| `Enter` | Jump to agent pane       |
| `g`/`G` | Jump to first/last       |
| `v`     | Toggle layout mode       |
| `z`     | Toggle sleeping on agent |
| `q`     | Quit sidebar             |

### Sleeping agents

Press `z` to manually mark an agent as sleeping. Sleeping agents show a 💤 icon
with dimmed colors and are pushed to the bottom of the list, regardless of their
actual status. This is useful for temporarily deprioritizing agents you don't
need to monitor. Press `z` again to wake them up.

## Agent navigation hotkeys

You can switch between agents from any tmux pane using subcommands. These work
in the same order shown in the sidebar:

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
```

## How it works

The sidebar is a bit of a hack on top of tmux's pane system, but it works quite
well. It uses a daemon + client architecture with event-driven rendering:

1. **Toggle on** (`workmux sidebar`): creates a narrow tmux pane on the left
   side of every window using a full-height split, starts a background daemon,
   and installs tmux hooks.

2. **Daemon**: a single headless process that polls tmux state every 2 seconds
   (or immediately when signaled via SIGUSR1). It reads agent state from the
   filesystem, queries tmux for pane geometry and active windows, then pushes
   snapshots to all connected sidebar clients over a Unix socket.

3. **Clients**: every tmux window gets its own sidebar pane running a separate
   `workmux _sidebar-run` process. Each process connects to the shared daemon
   socket, receives snapshots via a background reader thread, and renders
   independently. The main thread blocks on a channel, only waking when new
   data arrives or a spinner tick is needed. Rendering is skipped entirely for
   inactive windows. This event-driven design keeps CPU usage near zero when
   idle.

4. **Hooks**: tmux hooks handle lifecycle events:
   - `after-new-window` / `after-new-session`: automatically adds a sidebar pane
     to newly created windows
   - `window-resized`: reflows the layout tree to keep the sidebar at the
     correct width and content panes proportionally balanced
   - `after-select-window` / `client-session-changed` / `after-kill-pane`:
     signals the daemon for an immediate refresh

5. **Layout reflow**: when the sidebar is added or the terminal is resized, a
   layout tree parser reads the tmux `#{window_layout}` string, scales the
   content subtree proportionally, and applies the result atomically via
   `select-layout`. This preserves existing pane proportions (e.g. a 70/30 split
   stays 70/30).

6. **Toggle off**: kills all sidebar panes, reflows content panes to fill the
   freed space, stops the daemon, and removes hooks.

## Resource usage

Because tmux has no concept of a pane that persists across all windows, each
window runs its own `_sidebar-run` process. Each one uses roughly 15 MB of
resident memory, and the shared daemon (`_sidebar-daemon`) uses about 16 MB. With
five agents running, total memory footprint is around 90 MB. CPU usage is near
zero when idle thanks to the event-driven architecture.
