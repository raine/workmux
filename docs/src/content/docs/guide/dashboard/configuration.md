---
title: "Configuration"
description: Customize dashboard commands and layout
---

The dashboard can be customized in your `.workmux.yaml`:

```yaml
dashboard:
  commit: "Commit staged changes with a descriptive message"
  merge: "!workmux merge"
  preview_size: 60
  columns: [window, status, time, title]
```

The `commit` and `merge` values are text sent to the agent's pane. Use the `!` prefix to run shell commands (supported by Claude, Gemini, and other agents).

## Defaults

| Option         | Default value                                      | Description                               |
| -------------- | -------------------------------------------------- | ----------------------------------------- |
| `commit`       | `Commit staged changes with a descriptive message` | Natural language prompt                   |
| `merge`        | `!workmux merge`                                   | Shell command via agent                   |
| `preview_size` | `60`                                               | Preview pane height as percentage (10-90) |
| `columns`      | `[status, time, title]`                            | Order of the trailing table columns       |

## Columns

`columns` sets the order of the columns that follow the fixed `#`, `Project`, `Worktree`, `Git` and optional `PR` ones. For example, to read the title first and keep the elapsed time out of the way:

```yaml
dashboard:
  columns: [status, title, time]
```

Any of `window`, `status`, `time` and `title` may be listed, in any order. `window` is not shown by default and prints the multiplexer window index, the same number `prefix + <n>` uses in tmux, which makes the table line up with the status bar. Backends that do not number their windows leave the cell blank.

Any of them A column left out of the list is not rendered, so `columns: [status, title]` drops the elapsed time entirely. Repeating a column has no effect, and an empty list falls back to the default order.

## Preview size

The `preview_size` option controls the height of the preview pane as a percentage of the terminal height. A higher value means more space for the preview and less for the table.

You can also adjust the preview size interactively with `+`/`-` keys. These adjustments persist across dashboard sessions via tmux variables.

The CLI flag `--preview-size` (`-P`) overrides both the config and saved preference for that session.

## Examples

```yaml
# Use Claude skill for merge (see skills guide)
dashboard:
  merge: "/merge"

# Custom shell commands
dashboard:
  merge: "!workmux merge --rebase --notification"

# Natural language prompts
dashboard:
  commit: "Create a commit with a conventional commit message"
  merge: "Rebase onto main and run workmux merge"
```

## Using skills

For complex workflows, [skills](/guide/skills/) are more powerful than simple prompts or shell commands. A skill can encode detailed, multi-step instructions that the agent follows intelligently.

```yaml
dashboard:
  merge: "/merge"
```

See the [skills guide](/guide/skills/) for the `/merge` skill you can copy.
