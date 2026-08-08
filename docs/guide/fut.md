---
description: Use fut as an alternative multiplexer backend
---

# fut

::: warning Experimental and macOS-only
fut currently runs only on macOS. The workmux backend is experimental.
:::

[fut](https://github.com/adihex/fut) is a project-oriented terminal multiplexer
that workmux can target with `WORKMUX_BACKEND=fut`. Its hierarchy is
**session → workspace → tab → pane**; workmux treats tabs as named windows and
panes as its pane targets.

## Requirements

- macOS and a running fut daemon
- `fut` on `PATH`
- Run workmux from fut, or export `WORKMUX_BACKEND=fut`

## How it works

The backend uses fut's structured socket API: `fut list --json` to reconcile
resources, `fut open` for project locations, `fut tab new` for named workmux
windows, `fut pane new` for splits, and terminal reporting/attachment APIs for
pane activity. All noninteractive calls request JSON where fut supports it.

## Limitations

- **macOS only** until fut supports other platforms.
- fut owns layout and does not provide tmux-compatible insertion order or fixed
  split dimensions.
- Session mode is not exposed by this initial backend; normal workmux window
  mode maps directly to fut tabs.

## Backend contract

This integration implements workmux's `Multiplexer` contract: named window
lifecycle, pane creation and input, preview capture, focus, and live pane
reconciliation. It follows the same interface used by tmux, WezTerm, kitty,
and Zellij.
