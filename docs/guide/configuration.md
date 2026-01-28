---
description: Configure workmux with global defaults and project-specific settings
---

# Configuration

workmux uses a two-level configuration system:

- **Global** (`~/.config/workmux/config.yaml`): Personal defaults for all projects
- **Project** (`.workmux.yaml`): Project-specific overrides

Project settings override global settings. When you run workmux from a subdirectory, it walks upward to find the nearest `.workmux.yaml`, allowing nested configs for monorepos. See [Monorepos](./monorepos.md#nested-configuration) for details. For `post_create` and file operation lists (`files.copy`, `files.symlink`), you can use `"<global>"` to include global values alongside project-specific ones. Other settings like `panes` are replaced entirely when defined in the project config.

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

| Option           | Description                                          | Default                 |
| ---------------- | ---------------------------------------------------- | ----------------------- |
| `main_branch`    | Branch to merge into                                 | Auto-detected           |
| `worktree_dir`   | Directory for worktrees (absolute or relative)       | `<project>__worktrees/` |
| `nerdfont`       | Enable nerdfont icons (prompted on first run)        | Prompted                |
| `window_prefix`  | Override tmux window/session prefix                  | Icon or `wm-`           |
| `agent`          | Default agent for `<agent>` placeholder              | `claude`                |
| `merge_strategy` | Default merge strategy (`merge`, `rebase`, `squash`) | `merge`                 |
| `target`         | Tmux target type (`window` or `session`)             | `window`                |

### Naming options

| Option            | Description                                 | Default |
| ----------------- | ------------------------------------------- | ------- |
| `worktree_naming` | How to derive names from branches           | `full`  |
| `worktree_prefix` | Prefix for worktree directories and windows | none    |

`worktree_naming` strategies:

- `full`: Use the full branch name (slashes become dashes)
- `basename`: Use only the part after the last `/` (e.g., `prj-123/feature` → `feature`)

### Panes

Define your tmux pane layout with the `panes` array:

```yaml
panes:
  - command: <agent>
    focus: true
  - command: npm run dev
    split: horizontal
    size: 15
```

Each pane supports:

| Option       | Description                                         | Default |
| ------------ | --------------------------------------------------- | ------- |
| `command`    | Command to run (use `<agent>` for configured agent) | Shell   |
| `focus`      | Whether this pane receives focus                    | `false` |
| `split`      | Split direction (`horizontal` or `vertical`)        | —       |
| `size`       | Absolute size in lines/cells                        | 50%     |
| `percentage` | Size as percentage (1-100)                          | 50%     |

::: tip
The `<agent>` placeholder must be the entire command value to be substituted. To add extra flags, either include them in the `agent` config (e.g., `agent: "claude --verbose"`) or use the literal command name (e.g., `command: "claude --verbose"`).
:::

### File operations

Copy or symlink files into new worktrees:

```yaml
files:
  copy:
    - .env
  symlink:
    - node_modules
    - .pnpm-store
```

Both `copy` and `symlink` accept glob patterns.

### Lifecycle hooks

Run commands at specific points in the worktree lifecycle. All hooks run with the **worktree directory** as the working directory (or the nested config directory for [nested configs](./monorepos.md#nested-configuration)) and receive environment variables: `WM_HANDLE`, `WM_WORKTREE_PATH`, `WM_PROJECT_ROOT`, `WM_CONFIG_DIR`.

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

Set `status_format: false` to disable automatic tmux format modification.

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
