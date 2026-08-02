---
title: "Customization"
description: Customize the sidebar with templates, styling, and per-agent icons
---

The sidebar renders each agent row from a small token-based template DSL. You
can override the built-in templates per layout mode:

```yaml
sidebar:
  templates:
    # Compact mode: a single line per agent.
    compact: "{status_icon} {primary} {pane_suffix} {fill} {elapsed}"

    # Tile mode: one string per visual line in the tile body.
    tiles:
      - "{primary} {pane_suffix} {fill} {elapsed}"
      - "{secondary} {fill} {git_stats}"
      - "{pane_title} {fill} {pr_checks}"

    # Horizontal mode: one string per visual line in each top bar chip.
    horizontal:
      - "{status_icon} {primary} {pane_suffix} {fill} {elapsed}"
      - "{secondary} {fill} {git_stats}"
      - "{pane_title} {fill} {pr_checks}"
```

The values shown above are also the built-in defaults, so leaving these keys
unset gives you the standard rendering. `top` is also accepted as an alias for
`horizontal`.

Templates can be set in either the global config or a project's `.workmux.yaml`.
Project values override global values. Changes are picked up live by running
sidebars without a restart.

## Tokens

| Token            | Description                                                                                                                                     |
| ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `{primary}`      | Primary identity label (worktree / window / session / project chain).                                                                           |
| `{secondary}`    | Secondary label from the same chain, with worktree appended if not already primary.                                                             |
| `{worktree}`     | Worktree directory name.                                                                                                                        |
| `{project}`      | Project name (parent of the worktree).                                                                                                          |
| `{session}`      | Tmux session name (blank for workmux-prefixed sessions).                                                                                        |
| `{window}`       | Tmux window name (blank for generic shell names like `zsh`, `bash`).                                                                            |
| `{window_index}` | Tmux window number as shown in the status bar (e.g. `3` for `3:wm-fix-auth`), usable with `prefix+N`. Empty on backends without window indexes. |
| `{pane_title}`   | Sanitized agent task title from the pane title.                                                                                                 |
| `{pane_suffix}`  | Disambiguator like `(1)`, `(2)` when multiple agents share a window. Empty otherwise.                                                           |
| `{status_icon}`  | Status indicator (working spinner, waiting, done, sleeping, etc.).                                                                              |
| `{agent_icon}`   | Per-agent icon based on the running agent's profile (see [Agent identity](#agent-identity)).                                                    |
| `{agent_label}`  | Capitalized agent name (e.g. `Claude`, `Codex`).                                                                                                |
| `{elapsed}`      | Elapsed time since the agent's last status change.                                                                                              |
| `{git_stats}`    | Composite git diff stats: committed (`+1278 -400`), pen icon, uncommitted (`+21`). Self-degrades to fit the space it gets.                      |
| `{git_branch}`   | Current branch name. Empty when detached HEAD or git status unavailable.                                                                        |
| `{pr_number}`    | Pull request number as `#123`. Empty when the current branch has no matching PR.                                                                |
| `{pr_checks}`    | GitHub check status for the current remote branch, with counts when space allows. Empty when no checks are known.                               |
| `{git_ahead}`    | Commits ahead of upstream as `↑N` when N greater than 0. Empty when 0 or no upstream.                                                           |
| `{git_behind}`   | Commits behind upstream as `↓N` when N greater than 0. Empty when 0 or no upstream.                                                             |
| `{git_dirty}`    | Diff glyph when the working tree is dirty. Empty when clean.                                                                                    |
| `{git_conflict}` | Conflict glyph when the worktree has merge conflicts. Empty otherwise.                                                                          |
| `{status_label}` | Display name for the agent status: `Working`, `Waiting`, `Done`, or empty when no status.                                                       |
| `{idx}`          | 1-based sidebar position (`1`, `2`, ...).                                                                                                       |
| `{jump_key}`     | The `M-1`..`M-9` chord label for the first nine rows. Empty for row 10 and beyond.                                                              |
| `{fill}`         | Layout marker that splits a line into a left and right segment. At most one per line.                                                           |

`{git_ahead}` and `{git_behind}` already include the arrow prefix, so do not
wrap them with another `↑` / `↓` literal in your template, otherwise a stray
glyph would remain when the count is zero. The same applies to `{git_dirty}`
and `{git_conflict}`, which are self-contained glyph indicators.

`{pr_number}` is populated from the current git branch's matching GitHub pull
request. It renders empty on `main`/`master`, when the branch has no PR, when git
status is unavailable, or while PR data has not been fetched yet.

`{pr_checks}` is populated from the matching pull request's head commit when a PR
exists, then from the remote branch tip when the branch has no PR check data.
On `main` and `master`, only failing checks render. Local-only branches and
commits that have not been pushed render empty. Check status shows an icon and,
when space allows, passed/total counts for pending or failing checks.

Unknown tokens or unbalanced braces cause that template section to be rejected
and the previous valid section (or the built-in default) is kept. The sidebar
also shows a visible warning such as `template error: unknown token 'pr_status'
at column 1 in tiles[0]` until the template is fixed.

## Layout

`{fill}` is the only layout marker. Tokens before it form the left segment and
tokens after it form the right segment. The leftmost flex token in the left
segment absorbs ellipsis-truncation when there isn't enough room. Flex tokens
are: `{primary}`, `{secondary}`, `{worktree}`, `{project}`, `{session}`,
`{window}`, `{pane_title}`. Other tokens always render at their natural width.

When a line has more slack than the flex token needs, the leftover is emitted as
spaces between the left and right segments, so right-segment tokens like
`{elapsed}` line up against the right edge.

When a token resolves to an empty string, one adjacent literal whitespace is
dropped automatically. This means `{primary} {pane_suffix} {fill} {elapsed}`
renders cleanly whether or not `{pane_suffix}` is empty.

In tile mode, the stripe, status icon column, and a 1-column right margin are
drawn as chrome by the renderer. Templates only control the body area, so line
alignment between tiles is automatic. A tile line containing a field still
renders as a row when that field is empty, preserving tile height for templates
like the default `{pane_title}` row. Use a blank string in the `tiles` list to
skip that row entirely.

In horizontal mode, each configured `horizontal` line renders inside every chip.
The bar shows as many lines as its current height permits, so height 1 uses only
the first line, height 2 also shows the secondary label and git stats, and height
3 also shows the pane title.

## Escaping

Use `{{` for a literal `{` and `}}` for a literal `}`.

## Styling

Templates accept the same tmux-style `#[...]` directives that workmux uses
elsewhere (see [configuration](/guide/configuration/#agent-status-icons)). A
directive is stateful: it applies to all subsequent literals, fields, and
fill spaces on the same line until the next directive or `#[default]`. Each
line starts with no overlay.

```yaml
sidebar:
  templates:
    compact: "{status_icon} #[fg=cyan]{primary}#[default] {fill} {elapsed}"
    tiles:
      - "#[fg=cyan,bold]{primary}#[default] {pane_suffix} {fill} {elapsed}"
      - "{secondary} {fill} #[fg=green]{git_stats}#[default]"
      - "{pane_title} {fill} {pr_checks}"
```

Supported attributes are `fg=`, `bg=`, `bold`, `dim`, `italics`,
`underscore`, `reverse`, `strikethrough`, their negations (`nobold`,
`nodim`, `noitalics`, `nounderscore`, `noreverse`, `nostrikethrough`)
to remove a modifier inherited from the token's intrinsic style, plus
`default`/`none` to clear the overlay. Colors may be hex (`#a6e3a1`),
named (`red`, `green`, `cyan`, etc.), or indexed (`colour196`).

For example, `{elapsed}` renders dim by default for visual hierarchy.
To override:

```yaml
sidebar:
  templates:
    tiles:
      - "{primary} {pane_suffix} {fill} #[fg=cyan,nodim]{elapsed}"
```

Notes:

- Style codes are zero-width. They never affect `{fill}` alignment or
  ellipsis truncation.
- Stale rows ignore template styling so the dim state remains visually
  authoritative.
- The active row's bold and the selected row's background highlight are
  preserved; user `fg` patches over the active foreground while keeping
  bold. A user `bg=` will hide the selection highlight on the styled span.
- An unclosed `#[...` is rendered as literal text; malformed directives
  inside `#[...]` are silently ignored.

## Agent identity

Adding `{agent_icon}` or `{agent_label}` to a template surfaces which agent is
running in each pane. Identity is detected from the stored agent command via
the same profile system used elsewhere in workmux.

Default icons:

- `claude` → `CC`
- `codex` → `CX`
- `opencode` → `OC`
- `gemini` → `G`
- `pi` → `π`
- `omp` → `⌥`
- `kiro-cli` → `K`
- `vibe` → `V`
- `copilot` → `CP`
- `grok` → `GK`

Unknown agents render an empty icon.

Default colors are brand accents: Claude orange, Codex teal, Gemini blue,
Copilot purple, Grok blue, Vibe orange, Pi sage, OMP red/orange, OpenCode blue. Stale rows still dim
and selected rows still take the highlight background; the icon color sits
on top of those.

Override icons or colors per agent under `sidebar.agent_icons`. Each value
is either a bare string (icon only, default color stays) or an object with
`icon` and `color`. Color values use the same format as `theme.custom`:
hex (`'#ff8c00'`), named ANSI (`red`, `yellow`, `lightgreen`), or indexed
(`'214'`).

```yaml
sidebar:
  agent_icons:
    # Bare string: icon only, default brand color stays
    vibe: V

    # Override color only
    gemini:
      color: cyan

    # Override both
    claude:
      icon: CC
      color: "#ff8c00"

    # Disable the default color (use palette text color)
    codex:
      color: ""
```

A project-level `agent_icons` map merges into the global one per agent
key. Setting `claude:` in a project replaces only the global `claude`
entry; other agents defined in the global config are preserved. To
clear an inherited override entirely for one agent, set its value to
`null` (e.g. `claude: ~`).

## Status icons

The `{status_icon}` token renders the working spinner, waiting indicator,
done check, and sleeping indicator. Defaults depend on whether you have
[nerdfont](/guide/configuration/) enabled:

| State    | Default (no nerdfont) | Default (nerdfont) |
| -------- | --------------------- | ------------------ |
| Working  | braille spinner       | braille spinner    |
| Waiting  | 💬                    | nf-fa-comment      |
| Done     | ✅                    | nf-md-check_circle |
| Sleeping | 💤                    | nf-md-sleep        |

Override per state under top-level `status_icons`. Any value set here
wins over both the emoji and nerdfont defaults, so you can mix and match.
Setting `working` also replaces the braille spinner with a static icon:

```yaml
status_icons:
  working: "🤖"
  waiting: "💬"
  done: "✅"
```

## Examples

Show only the worktree and elapsed time per agent in compact mode:

```yaml
sidebar:
  layout: compact
  templates:
    compact: "{status_icon} {worktree} {fill} {elapsed}"
```

Add the agent icon next to the primary label in tile mode:

```yaml
sidebar:
  templates:
    tiles:
      - "{agent_icon} {primary} {pane_suffix} {fill} {elapsed}"
      - "{secondary} {fill} {git_stats}"
      - "{pane_title} {fill} {pr_checks}"
```

Configure only two tile lines and show git stats inline on line one:

```yaml
sidebar:
  templates:
    tiles:
      - "{primary} {fill} {git_stats}"
      - "{secondary} {fill} {elapsed}"
```

Show pull request number and GitHub check status in tile mode:

```yaml
sidebar:
  templates:
    tiles:
      - "{primary} {pane_suffix} {fill} {elapsed}"
      - "{secondary} {fill} {git_stats}"
      - "{pane_title} {fill} {pr_number} {pr_checks}"
```
