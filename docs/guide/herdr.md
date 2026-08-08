---
description: Use herdr as an alternative multiplexer backend
---

# herdr

::: warning Experimental
The herdr backend uses herdr's socket-backed CLI and is experimental.
:::

[herdr](https://herdr.dev) can be used as a workmux multiplexer backend. Set
`WORKMUX_BACKEND=herdr` when launching workmux. The backend maps workmux windows
to herdr tabs and workmux panes to herdr panes; it uses `herdr workspace`,
`herdr tab`, `herdr pane`, and `herdr agent` APIs rather than screen scraping.

## Requirements

- A running herdr server and a recent `herdr` CLI on `PATH`
- Run workmux from a herdr pane (or export `WORKMUX_BACKEND=herdr`)
- Unix-like OS for workmux's shell-startup handshake

## Limitations

- herdr's workspaces are not equivalent to tmux sessions, so workmux session
  mode is not supported; use normal window mode.
- Tab insertion ordering and fixed split sizes are controlled by herdr.
- The backend depends on the versioned JSON socket responses from the herdr CLI.

## Backend contract

The implementation satisfies workmux's `Multiplexer` contract: it creates,
focuses, lists and closes named tabs; splits and writes to panes; captures pane
output; and reconciles live pane metadata. This is the same contract used by
the tmux, WezTerm, kitty, and Zellij integrations.
