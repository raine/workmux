---
title: "Configuration"
description: Customize dashboard commands and layout
---

The dashboard can be customized in your `.workmux.yaml`:

```yaml
dashboard:
  commit: "Commit staged changes with a descriptive message"
  merge: "!workmux merge"
  layout: stacked
  preview_size: 60
  agent_columns:
    [number, project, worktree, git, pr, window, status, time, title]
  worktree_columns: [number, project, worktree, git, pr, mux, age, agent]
  close_on_jump: true
  input_exit_key: Esc
```

The `commit` and `merge` values are text sent to the agent's pane. Use the `!` prefix to run shell commands (supported by Claude, Gemini, and other agents).

## Defaults

| Option             | Default value                                                        | Description                                                                   |
| ------------------ | -------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| `commit`           | `Commit staged changes with a descriptive message`                   | Natural language prompt                                                       |
| `merge`            | `!workmux merge`                                                     | Shell command via agent                                                       |
| `layout`           | `stacked`                                                            | `stacked` (table above preview) or `side_by_side` (preview left, table right) |
| `preview_size`     | `60`                                                                 | Preview height or width as percentage (10-90)                                 |
| `agent_columns`    | `[number, project, worktree, machine, git, pr, status, time, title]` | Agents table columns, in display order                                        |
| `worktree_columns` | `[number, project, worktree, git, pr, mux, age, agent]`              | Worktree table columns, in display order                                      |
| `close_on_jump`    | `true`                                                               | Close the dashboard after a jump                                              |
| `input_exit_key`   | `Esc`                                                                | Key to exit input mode (`Esc` or `Ctrl+<letter>`)                             |

## Input mode exit key

`input_exit_key` accepts `Esc` (the default) or `Ctrl+<letter>` (`a`–`z`), case-insensitively. For example, to exit input mode with `Ctrl+q` and send `Esc` to the agent:

```yaml
dashboard:
  input_exit_key: Ctrl+q
```

The exit key is consumed by the dashboard, not sent to the agent. Escape bindings in other dashboard modes are unchanged. Project configuration overrides the global setting.

Choose a key your terminal delivers distinctly. Some Ctrl-letter keys are indistinguishable from Tab, Enter, or Backspace, and terminal flow control can intercept `Ctrl+q` or `Ctrl+s`.

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

| Column      | Content                                          |
| ----------- | ------------------------------------------------ |
| `number`    | Jump key of the row, shown under the `#` header  |
| `project`   | Project name                                     |
| `worktree`  | Worktree name, with a pane number when it splits |
| `machine`   | Remote server the agent runs on, or `local`      |
| `git`       | Branch state, staged and unstaged changes        |
| `pr`        | Pull request number and check status             |
| `pr_title`  | Pull request title                               |
| `pr_issues` | Issues closed by the pull request                |
| `window`    | Multiplexer window index                         |
| `status`    | Agent status icons                               |
| `time`      | Time since the last status change                |
| `title`     | Agent session title                              |
| `prompt`    | Latest prompt you sent to the agent              |

A column left out of the list is not rendered, so `agent_columns: [worktree, status, title]` gives a table of just those three. Repeating a column has no effect, and an empty list falls back to the default order.

Dropping `number` hides the jump key, and `1`-`9` still jump to the first nine rows. The `pr` column appears only while at least one agent has a pull request or checks to report, wherever it is placed in the list. Likewise, `machine` appears only while the table lists at least one [remote agent](/guide/remote/), showing the remote's name from `remotes:` and `local` for agents on this machine. The opt-in `pr_title` and `pr_issues` columns stay visible and leave the cell blank when the branch has no pull request or closing issues. `pr_issues` uses GitHub's closing issue references, not arbitrary issue mentions in pull request text. Same-repository issues appear as `#123`; cross-repository issues include the repository, such as `owner/repo#123`. The opt-in `window` column shows the tmux window index used by `prefix + <n>`; backends without window indexes leave the cell blank. A trailing `title`, `prompt`, `pr_title`, or `pr_issues` takes the width left over by the other columns; anywhere else it sizes to its content, and the leftover width sits at the right edge of the table.

#### Prompt column

The opt-in `prompt` column shows the last prompt you sent to each agent, on one line and cut with `…` to fit. Status hooks store the prompt only while the `prompt` column or a [`{prompt}` sidebar token](/guide/sidebar/customization/#tokens) is configured, evaluated with the agent's project configuration. Removing both stops capture, and each agent's stored prompt is deleted at its next status update.

Prompts come from Claude Code, Codex, Gemini CLI, Copilot CLI, Grok, Kimi Code, OpenCode, pi, and oh-my-pi. Prompts sent to subagents and messages the agent injects itself are ignored. A prompt with no text, such as an image alone, keeps the previous prompt. A prompt passed when the agent starts, such as `workmux add -p`, may not be shown until you send the next one. OpenCode, pi, and oh-my-pi report prompts once `workmux setup` updates their status plugin.

### Worktree columns

`worktree_columns` controls the Worktrees tab independently of `agent_columns`. To hide Git status and give the other columns more room:

```yaml
dashboard:
  worktree_columns: [number, project, worktree, pr, mux, age, agent]
```

| Column      | Content                                         |
| ----------- | ----------------------------------------------- |
| `number`    | Jump key of the row, shown under the `#` header |
| `project`   | Project name                                    |
| `worktree`  | Worktree name, with the branch when different   |
| `git`       | Branch state and uncommitted changes            |
| `pr`        | Pull request number and check status            |
| `pr_title`  | Pull request title                              |
| `pr_issues` | Issues closed by the pull request               |
| `mux`       | Whether a multiplexer window is open            |
| `age`       | Time since worktree creation                    |
| `agent`     | Agent status summary                            |

The list sets display order; omitted columns are hidden and duplicates are ignored. An absent or empty list uses the default order. Project configuration overrides the global list, and an explicit empty project list resets it to defaults.

A trailing `agent`, `pr_title`, or `pr_issues` fills the remaining width. Elsewhere it sizes to its content, leaving extra space at the right edge. The `pr` column is hidden until a worktree has a pull request or checks, except when it is the only configured column: like the agents table, it stays visible to avoid an empty table. The opt-in PR metadata columns use the same closing-reference and blank-cell behavior as the Agents table.

Hiding `number` does not disable `1`-`9` jumps. Hiding `git` does not disable Git status updates or remove the selected worktree's Git details from the preview.

## Layout

The default `stacked` layout places the table above the preview. To give the preview the full content height, with the table on the right:

```yaml
dashboard:
  layout: side_by_side
```

The layout applies to both the Agents and Worktrees tabs. The tab header and footer remain above and below the content. Backends without preview support show only the table regardless of this setting.

Side-by-side leaves less width for each pane. On narrow terminals, use the stacked layout or reduce the table's configured columns. Captured agent output is clipped to the preview width, not rewrapped.

## Preview size

The `preview_size` option controls the preview as a percentage of the content area: height in `stacked` layout, width in `side_by_side` layout. A higher value means more space for the preview and less for the table.

You can also adjust the preview size interactively with `+`/`-` keys. These adjustments persist across dashboard sessions in the workmux state directory.

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
