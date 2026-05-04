---
description: Configure workmux with global defaults and project-specific settings
---

# Configuration

workmux uses a two-level configuration system:

- **Global** (`~/.config/workmux/config.yaml`): Personal defaults for all projects. Run `workmux config edit` to open it in your editor.
- **Project** (`.workmux.yaml`): Project-specific overrides

Project settings override global settings. When you run workmux from a subdirectory, it walks upward to find the nearest `.workmux.yaml`, allowing nested configs for monorepos. See [Monorepos](./monorepos.md#nested-configuration) for details. For `post_create` and file operation lists (`files.copy`, `files.symlink`), you can use `"<global>"` to include global values alongside project-specific ones. Other settings like `panes` are replaced entirely when defined in the project config.

### XDG Base Directory support

workmux respects the [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir/latest/):

| Purpose       | Environment variable | Default          |
| ------------- | -------------------- | ---------------- |
| Configuration | `XDG_CONFIG_HOME`    | `~/.config`      |
| Cache         | `XDG_CACHE_HOME`     | `~/.cache`       |
| State         | `XDG_STATE_HOME`     | `~/.local/state` |

All workmux files live under a `workmux/` subdirectory within these base directories. If you have an existing config at the default location and later set a custom `XDG_CONFIG_HOME`, workmux will fall back to reading from `~/.config/workmux/` if no config exists at the new location.

The host-side log lives at `$XDG_STATE_HOME/workmux/workmux.log` and includes sandbox network-proxy rejections — see [Debugging blocked requests](./sandbox/features.md#debugging-blocked-requests).

## Global configuration example

`~/.config/workmux/config.yaml`:

```yaml
nerdfont: true # Enable nerdfont icons (prompted on first run)
merge_strategy: rebase # Make workmux merge do rebase by default
agent: claude

panes:
  - command: <agent> # Start the configured agent (e.g., claude)
    focus: true
  - split: horizontal # Second pane with default shell
```

## Project configuration example

`.workmux.yaml`:

```yaml
post_create:
  - "<global>"
  - mise use

files:
  symlink:
    - "<global>" # Include global symlinks (node_modules)
    - .pnpm-store # Add project-specific symlink

panes:
  - command: pnpm install
    focus: true
  - command: <agent>
    split: horizontal
  - command: pnpm run dev
    split: vertical
```

For a real-world example, see [workmux's own `.workmux.yaml`](https://github.com/raine/workmux/blob/main/.workmux.yaml).

## Configuration options

Most options have sensible defaults. You only need to configure what you want to customize.

### Basic options

| Option             | Description                                                                         | Default                     |
| ------------------ | ----------------------------------------------------------------------------------- | --------------------------- |
| `main_branch`      | Branch to merge into                                                                | Auto-detected               |
| `base_branch`      | Default base branch for new worktrees (overridden by `--base`)                      | Current branch              |
| `worktree_dir`     | Directory for worktrees (absolute or relative). Supports `~` and `{project}`.       | `<project>__worktrees/`     |
| `nerdfont`         | Enable nerdfont icons (prompted on first run)                                       | Prompted                    |
| `window_prefix`    | Override tmux window/session prefix                                                 | Icon or `wm-`               |
| `agent`            | Default agent for `<agent>` placeholder                                             | `claude`                    |
| `agents`           | Named agent commands (global-only). See [named agents](/guide/agents#named-agents). | `{}`                        |
| `prompt_file_only` | Write prompt files without injecting into agent commands                            | `false`                     |
| `merge_strategy`   | Default merge strategy (`merge`, `rebase`, `squash`)                                | `merge`                     |
| `theme`            | Dashboard color scheme (see [themes](#themes))                                      | `default` (auto dark/light) |
| `mode`             | Tmux mode (`window` or `session`). See [session mode](/guide/session-mode).         | `window`                    |

### Themes

The dashboard supports 12 color schemes, each with dark and light variants. Dark/light mode is auto-detected from your terminal background.

Press `T` (shift+t) in the dashboard to cycle through schemes. The selection persists to your global config (`~/.config/workmux/config.yaml`).

Available schemes: `default`, `emberforge`, `glacier-signal`, `obsidian-pop`, `slate-garden`, `phosphor-arcade`, `lasergrid`, `mossfire`, `night-sorbet`, `graphite-code`, `festival-circuit`, `teal-drift`.

```yaml
# Just a scheme name (auto-detect dark/light)
theme: emberforge

# Force a specific mode
theme:
  scheme: emberforge
  mode: light
```

#### Custom colors

You can override individual palette colors using the `custom` block. Custom colors are applied on top of the base scheme, so you can start from any built-in theme and tweak specific colors. Values can be hex colors (`"#51afef"`), named colors (`red`, `cyan`), or terminal color indices (`42`):

```yaml
theme:
  custom:
    bg: "#282c34"
    fg: "#bbc2cf"
    accent: "#51afef"
    success: "#98be65"
    warning: "#ECBE7B"
    error: "#ff6c6b"
```

You can also combine custom colors with a specific scheme and mode:

```yaml
theme:
  scheme: emberforge
  mode: dark
  custom:
    accent: "#51afef"
    danger: "#ff6c6b"
```

**Shorthand aliases:** `bg` for `current_row_bg`, `fg` for `text`, `error` for `danger`.

**All palette fields:** `current_row_bg`, `highlight_row_bg`, `current_worktree_fg`, `dimmed`, `text`, `border`, `help_border`, `help_muted`, `header`, `keycap`, `info`, `success`, `warning`, `danger`, `accent`.

Custom colors persist when cycling themes with `T`.

### Naming options

| Option            | Description                                 | Default |
| ----------------- | ------------------------------------------- | ------- |
| `worktree_naming` | How to derive names from branches           | `full`  |
| `worktree_prefix` | Prefix for worktree directories and windows | none    |

`worktree_naming` strategies:

- `full`: Use the full branch name (slashes become dashes)
- `basename`: Use only the part after the last `/` (e.g., `prj-123/feature` → `feature`)

### Panes

Define your tmux pane layout with the `panes` array. For multiple windows in session mode, use [windows](#windows) instead (they are mutually exclusive).

```yaml
panes:
  - command: <agent>
    focus: true
  - command: npm run dev
    split: horizontal
    size: 15
```

Each pane supports:

| Option       | Description                                                          | Default |
| ------------ | -------------------------------------------------------------------- | ------- |
| `command`    | Command to run (see [agent placeholders](#agent-placeholders) below) | Shell   |
| `focus`      | Whether this pane receives focus                                     | `false` |
| `zoom`       | Zoom pane to fullscreen (implies `focus: true`)                      | `false` |
| `split`      | Split direction (`horizontal` or `vertical`)                         | ---     |
| `size`       | Absolute size in lines/cells                                         | 50%     |
| `percentage` | Size as percentage (1-100)                                           | 50%     |

#### Agent placeholders

- `<agent>`: resolves to the configured agent (from `agent` config or `--agent` flag)

Built-in agents (`claude`, `gemini`, `codex`, `opencode`, `kiro-cli`, `vibe`, `pi`) are auto-detected when used as literal commands and receive prompt injection automatically, without needing the `<agent>` placeholder or a matching `agent` config:

```yaml
panes:
  - command: "claude --dangerously-skip-permissions"
    focus: true
  - command: "codex --yolo"
    split: vertical
```

Each agent receives the prompt (via `-p`/`-P`/`-e`) using the correct format for that agent. Auto-detection matches the executable name regardless of flags or path.

### Named layouts

Define reusable pane arrangements in the `layouts` map and select one at add-time with `-l/--layout`:

```yaml
layouts:
  design:
    panes:
      - command: <agent>
        focus: true
      - command: <agent:codex>
        split: vertical
  review:
    panes:
      - command: <agent>
```

```bash
workmux add my-feature -l design
```

When `-l` is used, the layout's `panes` replace the top-level `panes` for that worktree. All other config (hooks, files, agent, etc.) comes from the top-level as usual. The `-l` flag cannot be combined with `--agent`.

### Windows

When using [session mode](/guide/session-mode), you can configure multiple windows per session using the `windows` array. This is mutually exclusive with the top-level `panes` config. See [multiple windows per session](/guide/session-mode#multiple-windows-per-session) for full details.

```yaml
mode: session
windows:
  - name: editor
    panes:
      - command: <agent>
        focus: true
      - split: horizontal
        size: 20
  - name: tests
    panes:
      - command: just test --watch
```

### File operations

New worktrees are clean checkouts with no gitignored files (`.env`, `node_modules`, etc.). Use `files` to automatically copy or symlink what each worktree needs:

```yaml
files:
  copy:
    - .env
  symlink:
    - .next/cache # Share build cache across worktrees
```

Both `copy` and `symlink` accept glob patterns.

To re-apply file operations to existing worktrees (e.g., after updating the config), use [`workmux sync-files`](/reference/commands/sync-files).

### Lifecycle hooks

Run commands at specific points in the worktree lifecycle, such as installing dependencies or running database migrations. All hooks run with the **worktree directory** as the working directory (or the nested config directory for [nested configs](./monorepos.md#nested-configuration)) and receive environment variables: `WM_HANDLE`, `WM_WORKTREE_PATH`, `WM_PROJECT_ROOT`, `WM_CONFIG_DIR`.

| Hook          | When it runs                                      | Additional env vars                  |
| ------------- | ------------------------------------------------- | ------------------------------------ |
| `post_create` | After worktree creation, before tmux window opens | —                                    |
| `pre_merge`   | Before merging (aborts on failure)                | `WM_BRANCH_NAME`, `WM_TARGET_BRANCH` |
| `pre_remove`  | Before worktree removal (aborts on failure)       | —                                    |

`WM_CONFIG_DIR` points to the directory containing the `.workmux.yaml` that was used, which may differ from `WM_WORKTREE_PATH` when using nested configs.

Example:

```yaml
post_create:
  - direnv allow

pre_merge:
  - just check
```

### Agent status icons

Customize the icons shown in tmux window names:

```yaml
status_icons:
  working: "🤖" # Agent is processing
  waiting: "💬" # Agent needs input (auto-clears on focus)
  done: "✅" # Agent finished (auto-clears on focus)
```

You can use tmux style codes for colored icons in both the tmux status bar and the dashboard:

```yaml
status_icons:
  done: "#[fg=#a6e3a1]󰄴#[fg=default]"
```

Supported tmux style attributes: `fg=`, `bg=`, `default`. Colors can be hex (`#a6e3a1`), named (`red`, `green`, etc.), or indexed (`colour196`).

Set `status_format: false` to disable automatic tmux format modification.

### Auto-name configuration

Configure LLM-based branch name generation for the `--auto-name` (`-A`) flag:

```yaml
auto_name:
  command: "claude -p" # Use a custom command instead of the inferred default
  model: "gemini-2.5-flash-lite"
  background: true
  system_prompt: "Generate a kebab-case git branch name."
```

The command used for branch name generation is resolved in this order:

1. `auto_name.command` is set: uses that command as-is
2. `agent` is a known agent (`claude`, `gemini`, `codex`, `opencode`, `kiro-cli`, `vibe`, `pi`): uses the agent's CLI with a fast/cheap model automatically
3. Neither: falls back to the `llm` CLI (requires installation)

To override back to `llm` when an agent is configured, set `auto_name.command: "llm"`.

| Option          | Description                                                      | Default                    |
| --------------- | ---------------------------------------------------------------- | -------------------------- |
| `command`       | Command for branch name generation (overrides agent profile)     | Agent profile or `llm` CLI |
| `model`         | LLM model to use with the `llm` CLI (ignored when `command` set) | `llm`'s default            |
| `background`    | Always run in background when using `--auto-name`                | `false`                    |
| `system_prompt` | Custom system prompt for branch name generation                  | Built-in prompt            |

See [`workmux add --auto-name`](../reference/commands/add.md#automatic-branch-name-generation) for usage details.

## Default behavior

- Worktrees are created in `<project>__worktrees` as a sibling directory to your project by default
- If no `panes` configuration is defined, workmux provides opinionated defaults:
  - For projects with a `CLAUDE.md` file: Opens the configured agent (see `agent` option) in the first pane, defaulting to `claude` if none is set.
  - For all other projects: Opens your default shell.
  - Both configurations include a second pane split horizontally
- `post_create` commands are optional and only run if you configure them

## Automatic setup with panes

Use the `panes` configuration to automate environment setup. Unlike `post_create` hooks which must finish before the tmux window opens, pane commands execute immediately _within_ the new window.

This can be used for:

- **Installing dependencies**: Run `npm install` or `cargo build` in a focused pane to monitor progress.
- **Starting services**: Launch dev servers, database containers, or file watchers automatically.
- **Running agents**: Initialize AI agents with specific context.

Since these run in standard tmux panes, you can interact with them (check logs, restart servers) just like a normal terminal session.

::: tip
Running dependency installation (like `pnpm install`) in a pane command rather than `post_create` has a key advantage: you get immediate access to the tmux window while installation runs in the background. With `post_create`, you'd have to wait for the install to complete before the window even opens. This also means AI agents can start working immediately in their pane while dependencies install in parallel.
:::

```yaml
panes:
  # Pane 1: Install dependencies, then start dev server
  - command: pnpm install && pnpm run dev

  # Pane 2: AI agent
  - command: <agent>
    split: horizontal
    focus: true
```
