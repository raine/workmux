---
title: "last-done"
description: Switch to the most recently completed or waiting agent
---

Switches to the agent that most recently completed its task or is waiting for user input. Repeated invocations cycle through all done/waiting agents in reverse chronological order (most recent first). The cycle resets when a new agent finishes, so you always see the latest first.

This is a hidden command (not shown in `--help`), typically invoked via a tmux keybinding.

```bash
workmux last-done
```

## How it works

1. Finds all agents with "done" or "waiting" status.
2. Sorts them by most recent status change first.
3. Switches to the most recent one.
4. On repeated invocations, advances to the next oldest agent in the list.
5. When a new agent finishes (the most recent timestamp changes), the cycle resets back to the newest.

## Tmux keybinding

Add to `~/.tmux.conf` for quick access:

```bash
bind l run-shell "workmux last-done"
```

Then press `prefix + l` to jump to the last completed or waiting agent. Press again to cycle to the next oldest.

## Examples

```bash
# Jump to the most recently completed/waiting agent
workmux last-done

# Press again to cycle to the next oldest
workmux last-done
```

## Related

- [Status tracking](/guide/status-tracking/) explains how agent status is detected and displayed
