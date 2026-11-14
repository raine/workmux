---
title: "WezTerm"
description: Use WezTerm as an alternative multiplexer backend
---

:::caution[Experimental]
The WezTerm backend is new and experimental. Expect rough edges and potential issues.
:::

[WezTerm](https://wezterm.org/) can be used as an alternative to tmux. Detected automatically via `$WEZTERM_PANE`.

## Differences from tmux

| Feature              | tmux                 | WezTerm           |
| -------------------- | -------------------- | ----------------- |
| Agent status in tabs | Yes (window names)   | Dashboard only    |
| Tab ordering         | Insert after current | Appends to end    |
| Scope                | tmux session         | WezTerm workspace |

- **Tab ordering**: New tabs appear at the end of the tab bar (no "insert after" support like tmux)
- **Workspace isolation**: workmux operates within the current WezTerm workspace (analogous to tmux sessions). Tabs in other workspaces are not affected.
- **Exit detection**: Uses title heuristics to detect when agents exit

## Requirements

- WezTerm with CLI enabled (`wezterm cli` must work)
- Unix-like OS (named pipes for handshakes)
- Windows is **not supported**
- **Required WezTerm configuration** (see below)

## Required WezTerm configuration

workmux relies on WezTerm's environment variables (`WEZTERM_PANE`, `WEZTERM_UNIX_SOCKET`) being consistent across all panes. This requires connecting to the mux server on startup.

Add this to your `wezterm.lua`:

```lua
local config = wezterm.config_builder()

-- REQUIRED: Connect to unix mux server on startup
-- This ensures WEZTERM_UNIX_SOCKET is consistent across all panes
config.default_gui_startup_args = { 'connect', 'unix' }

-- REQUIRED: Configure unix_domains for the mux server
config.unix_domains = {
    { name = 'unix' },
}
```

Additionally, if you have custom keybindings for creating tabs, ensure they use `CurrentPaneDomain`:

```lua
-- CORRECT: Uses the current pane's domain (mux server)
{ key = 't', mods = 'SUPER', action = act.SpawnTab('CurrentPaneDomain') },

-- WRONG: This spawns in the GUI domain, breaking workmux
-- { key = 't', mods = 'SUPER', action = act.SpawnTab({ DomainName = 'local' }) },
```

Without this configuration, panes created via keybindings may connect to a different socket than panes created by workmux, causing state inconsistencies.

## Cross-workspace navigation

The dashboard can show agents from all workspaces with `--all` (or pressing `a`). However, WezTerm's CLI cannot directly switch workspaces. To enable jumping to tabs in other workspaces, add this to your `wezterm.lua`:

```lua
local wezterm = require("wezterm")

wezterm.on("user-var-changed", function(window, pane, name, value)
    if name == "workmux-switch-pane" then
        local data = wezterm.json_parse(value)
        -- Switch to the target workspace
        window:perform_action(
            wezterm.action.SwitchToWorkspace({ name = data.workspace }),
            pane
        )
        -- Find and activate the tab by title (stable across mux contexts)
        wezterm.time.call_after(0.1, function()
            for _, win in ipairs(wezterm.mux.all_windows()) do
                for _, tab in ipairs(win:tabs()) do
                    if tab:get_title() == data.tab_title then
                        tab:activate()
                        local panes = tab:panes()
                        if #panes > 0 then panes[1]:activate() end
                        return
                    end
                end
            end
        end)
    end
end)
```

Without this configuration, the dashboard can display agents from all workspaces but jumping to panes in other workspaces will not work.

## Known limitations

- Windows is not supported (requires Unix-specific features)
- Cross-workspace jumping requires Lua config (see above)
- Some edge cases may not be as thoroughly tested as the tmux backend
- Agent status icons do not appear in tab titles

## Credits

Thanks to [@JeremyBYU](https://github.com/JeremyBYU) for contributing WezTerm support.
