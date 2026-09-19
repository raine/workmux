---
title: "sidebar"
description: Control a live agent status sidebar in tmux
---

Controls a live agent status sidebar on the left or top edge of tmux windows. By default, each sidebar pane shows active agents across all tmux
sessions with live status updates. Use `workmux sidebar filter session` to show
only agents in the current tmux session.

```bash
workmux sidebar                         # Toggle sidebar on/off (all sessions)
workmux sidebar on                      # Ensure the global sidebar is running
workmux sidebar off                     # Ensure the global sidebar is stopped
workmux sidebar --session on            # Ensure it is running in this session
workmux sidebar --session off           # Ensure it is stopped in this session
workmux sidebar on --position top       # Enable with a top sidebar
workmux sidebar on --width 40           # Set left sidebar width in columns
workmux sidebar on --height 3           # Set top sidebar height in rows
```

## What it shows

Each agent row displays:

- Status icon (working/waiting/done with spinner animation)
- Project and worktree name (e.g. `myproject/fix-bug`)
- Elapsed time since last status change
- Git diff stats, plus optional pull request number and check status in custom templates

## Keybindings

| Key     | Action                    |
| ------- | ------------------------- |
| `j`/`k` | Navigate up/down          |
| `Enter` | Jump to agent pane        |
| `g`/`G` | Jump to first/last        |
| `v`     | Toggle layout mode        |
| `f`     | Toggle session filter     |
| `z`     | Toggle sleeping on agent  |
| `t`     | Toggle grouping           |
| `h`/`l` | Unfold/fold current group |
| `s`     | Toggle the current fold   |
| `S`     | Fold/unfold every group   |
| `?`     | Show these keys           |
| `q`     | Open quit confirmation    |

## Mouse support

With tmux mouse mode enabled (`set -g mouse on`), click an agent row or top-bar
chip to jump to its pane, or scroll to navigate the list.

## Navigation commands

Switch between agents from any tmux pane, in the same order shown in the
sidebar. Navigation uses the same filter mode as the sidebar view, so it cycles
through all sessions by default:

| Command                         | Action                                               |
| ------------------------------- | ---------------------------------------------------- |
| `workmux sidebar next`          | Switch to the next agent (wraps)                     |
| `workmux sidebar prev`          | Switch to the previous agent (wraps)                 |
| `workmux sidebar jump <N>`      | Jump to the Nth agent (1-indexed)                    |
| `workmux sidebar filter`        | Toggle session filter (none/session)                 |
| `workmux sidebar filter <MODE>` | Set filter mode: `none`/`all` or `session`/`project` |
| `workmux sidebar group`         | Toggle grouping between grouped and flat             |
| `workmux sidebar group <MODE>`  | Set grouping: `none`/`off`, `project` or `session`   |
| `workmux sidebar group --clear` | Drop the runtime grouping and follow config again    |

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
  position: left # "left" (default) or "top"
  width: 40 # left width in columns (default: "10%", clamped 25-50)
  # width: "15%"
  layout: tiles # left only: "compact" or "tiles" (default)
  dim_stale: true # dim agents whose activity state is older than one hour
  group_by: project # "project" or "session"; unset keeps one flat list
  collapse_stale: true # while grouped, fold stale agents behind a toggle
```

For a horizontal top bar:

```yaml
sidebar:
  position: top
  height: 3 # top height in rows
  horizontal:
    item_width: 24 # horizontal chip width in columns (default, clamped 12-80)
  templates:
    horizontal:
      - "{status_icon} {primary} {pane_suffix} {fill} {elapsed}"
      - "{secondary} {fill} {git_stats}"
      - "{pane_title}"
```

Explicit width values bypass the default 25-50 column clamp (minimum 10
columns). Layout preference can also be toggled at runtime with `v` and is
persisted across restarts. Height only applies to `position: top`; set it as a
row count for the number of horizontal lines you want to show. The top bar uses a
horizontal chip layout, so `v` has no effect there. Horizontal templates render
as many configured lines as the current height allows. `horizontal.item_width`
controls each chip width and is clamped between 12 and 80 columns. Position
changes take effect when the sidebar is enabled. Use
`workmux sidebar on --position top` or `--position left` to override the
configured placement. Width and height also accept percentages, such as
`workmux sidebar on --width 15%`. Repeating `on` with appearance options applies
them without creating duplicate panes. The `off` action rejects position and
dimension flags because they have no effect while stopping the sidebar.

## How it works

When enabled, a background daemon polls tmux state every 2 seconds and pushes
snapshots to each sidebar pane over a Unix socket. The sidebar creates a tmux
pane on the configured edge of every existing window. A tmux hook
(`after-new-window`) ensures newly created windows also get a sidebar
automatically.

Running `workmux sidebar` again disables the sidebar globally, killing all
sidebar panes, the daemon, and removing hooks. `workmux sidebar on` and
`workmux sidebar off` provide idempotent alternatives for scripts and tmux
configuration.

### Session-scoped mode

By default, the sidebar appears in all tmux sessions. Use `--session` to scope
it to the current session only, leaving other sessions untouched:

```bash
workmux sidebar --session on   # Ensure it is enabled in the current session
workmux sidebar --session off  # Ensure it is disabled in the current session
```

You can enable session-scoped sidebars in multiple sessions independently. Each
session can be toggled or explicitly enabled and disabled without affecting
others.

If the global sidebar is already active, `workmux sidebar --session` hides the
sidebar in the current tmux session only. Run it again to show the sidebar in
that session again while other sessions remain globally managed.

Starting global mode still replaces any session-scoped sidebars.

## Limitations

- tmux only (other backends are not supported yet)

## Example tmux binding

```bash
bind C-t run-shell "workmux sidebar"
```
