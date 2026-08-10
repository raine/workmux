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
  agent_columns:
    [number, project, worktree, git, pr, window, status, time, title]
  worktree_columns: [number, project, worktree, git, pr, mux, age, agent]
  close_on_jump: true
```

The `commit` and `merge` values are text sent to the agent's pane. Use the `!` prefix to run shell commands (supported by Claude, Gemini, and other agents).

## Defaults

| Option             | Default value                                               | Description                               |
| ------------------ | ----------------------------------------------------------- | ----------------------------------------- |
| `commit`           | `Commit staged changes with a descriptive message`          | Natural language prompt                   |
| `merge`            | `!workmux merge`                                            | Shell command via agent                   |
| `preview_size`     | `60`                                                        | Preview pane height as percentage (10-90) |
| `agent_columns`    | `[number, project, worktree, git, pr, status, time, title]` | Agents table columns, in display order    |
| `worktree_columns` | `[number, project, worktree, git, pr, mux, age, agent]`     | Worktree table columns, in display order  |
| `close_on_jump`    | `true`                                                      | Close the dashboard after a jump          |

## Staying open after a jump

Jumping to an agent or worktree closes the dashboard. This includes `Enter`, `1`-`9`, `Bksp`, command-palette jumps, and double-clicking a row. Set `close_on_jump: false` to keep the dashboard open while moving between agents and worktrees:

```yaml
dashboard:
  close_on_jump: false
```

The jump still updates the pane history used by `Bksp`. The `p` peek action always keeps the dashboard open and does not update pane history. Multiplexers such as Zellij that keep the dashboard open after jumps retain that behavior regardless of this setting.

## Columns

`agent_columns` sets which columns the agents table shows and in what order. For example, to read the title first and keep the elapsed time out of the way:

```yaml
dashboard:
  agent_columns:
    [number, window, status, title, project, worktree, git, pr, time]
```

| Column     | Content                                          |
| ---------- | ------------------------------------------------ |
| `number`   | Jump key of the row, shown under the `#` header  |
| `project`  | Project name                                     |
| `worktree` | Worktree name, with a pane number when it splits |
| `git`      | Branch state, staged and unstaged changes        |
| `pr`       | Pull request number and check status             |
| `window`   | Multiplexer window index                         |
| `status`   | Agent status icons                               |
| `time`     | Time since the last status change                |
| `title`    | Agent session title                              |

A column left out of the list is not rendered, so `agent_columns: [worktree, status, title]` gives a table of just those three. Repeating a column has no effect, and an empty list falls back to the default order.

Dropping `number` hides the jump key, and `1`-`9` still jump to the first nine rows. The `pr` column appears only while at least one agent has a pull request or checks to report, wherever it is placed in the list. The opt-in `window` column shows the tmux window index used by `prefix + <n>`; backends without window indexes leave the cell blank. A trailing `title` takes the width left over by the other columns; anywhere else it sizes to its content, and the leftover width sits at the right edge of the table.

### Worktree columns

`worktree_columns` controls the Worktrees tab independently of `agent_columns`. To hide Git status and give the other columns more room:

```yaml
dashboard:
  worktree_columns: [number, project, worktree, pr, mux, age, agent]
```

| Column     | Content                                         |
| ---------- | ----------------------------------------------- |
| `number`   | Jump key of the row, shown under the `#` header |
| `project`  | Project name                                    |
| `worktree` | Worktree name, with the branch when different   |
| `git`      | Branch state and uncommitted changes            |
| `pr`       | Pull request number and check status            |
| `mux`      | Whether a multiplexer window is open            |
| `age`      | Time since worktree creation                    |
| `agent`    | Agent status summary                            |

The list sets display order; omitted columns are hidden and duplicates are ignored. An absent or empty list uses the default order. Project configuration overrides the global list, and an explicit empty project list resets it to defaults.

A trailing `agent` fills the remaining width. Elsewhere it sizes to its content, leaving extra space at the right edge. The `pr` column is hidden until a worktree has a pull request or checks, except when it is the only configured column: like the agents table, it stays visible to avoid an empty table.

Hiding `number` does not disable `1`-`9` jumps. Hiding `git` does not disable Git status updates or remove the selected worktree's Git details from the preview.

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
