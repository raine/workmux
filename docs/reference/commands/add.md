---
description: Create git worktrees and tmux windows, with support for AI prompts and parallel generation
---

# add

Creates a new git worktree with a matching tmux window and switches you to it immediately. If the branch doesn't exist, it will be created automatically.

```bash
workmux add <branch-name> [flags]
```

## Arguments

- `<branch-name>`: Name of the branch to create or switch to, a remote branch reference (e.g., `origin/feature-branch`), or a GitHub fork reference (e.g., `user:branch`). Remote and fork references are automatically fetched and create a local branch with the derived name. Optional when using `--pr`.

## Options

| Flag                           | Description                                                                                                                                                                                                                                                             |
| ------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--base <branch\|commit\|tag>` | Specify a base branch, commit, or tag to branch from when creating a new branch. By default, new branches are created from the current branch you have checked out.                                                                                                     |
| `--pr <number>`                | Checkout a GitHub pull request by its number into a new worktree. Requires the `gh` command-line tool to be installed and authenticated. The local branch name defaults to the PR's head branch name, but can be overridden (e.g., `workmux add custom-name --pr 123`). |
| `-A, --auto-name`              | Generate branch name from prompt using LLM. See [Automatic branch name generation](#automatic-branch-name-generation).                                                                                                                                                  |
| `--name <name>`                | Override the worktree directory and tmux window name. By default, these are derived from the branch name (slugified). Cannot be used with multi-worktree generation (`--count`, `--foreach`, or multiple `--agent`).                                                    |
| `-b, --background`             | Create the tmux window in the background without switching to it. Useful with `--prompt-editor`.                                                                                                                                                                        |
| `-w, --with-changes`           | Move uncommitted changes from the current worktree to the new worktree, then reset the original worktree to a clean state. Useful when you've started working on main and want to move your branches to a new worktree.                                                 |
| `--patch`                      | Interactively select which changes to move (requires `--with-changes`). Opens an interactive prompt for selecting hunks to stash.                                                                                                                                       |
| `-u, --include-untracked`      | Also move untracked files (requires `--with-changes`). By default, only staged and modified tracked files are moved.                                                                                                                                                    |
| `-p, --prompt <text>`          | Provide an inline prompt that will be automatically passed to AI agent panes.                                                                                                                                                                                           |
| `-P, --prompt-file <path>`     | Provide a path to a file whose contents will be used as the prompt.                                                                                                                                                                                                     |
| `-e, --prompt-editor`          | Open your `$EDITOR` (or `$VISUAL`) to write the prompt interactively.                                                                                                                                                                                                   |
| `-a, --agent <name>`           | The agent(s) to use for the worktree(s). Can be specified multiple times to generate a worktree for each agent. Overrides the `agent` from your config file.                                                                                                            |
| `-W, --wait`                   | Block until the created tmux window is closed. Useful for scripting when you want to wait for an agent to complete its work. The agent can signal completion by running `workmux remove --keep-branch`.                                                                 |
| `-S, --session`                | Create the worktree's window in its own tmux session instead of the current session. Useful for session-per-project workflows. Can also be set via `target: session` in config.                                                                                         |

## Skip options

These options allow you to skip expensive setup steps when they're not needed (e.g., for documentation-only changes):

| Flag                 | Description                                                           |
| -------------------- | --------------------------------------------------------------------- |
| `-H, --no-hooks`     | Skip running `post_create` commands                                   |
| `-F, --no-file-ops`  | Skip file copy/symlink operations (e.g., skip linking `node_modules`) |
| `-C, --no-pane-cmds` | Skip executing pane commands (panes open with plain shells instead)   |

## What happens

1. Determines the **handle** for the worktree by slugifying the branch name (e.g., `feature/auth` becomes `feature-auth`). This can be overridden with the `--name` flag.
2. Creates a git worktree at `<worktree_dir>/<handle>` (the `worktree_dir` is configurable and defaults to a sibling directory of your project)
3. Runs any configured file operations (copy/symlink)
4. Executes `post_create` commands if defined (runs before the tmux window/session opens, so keep them fast)
5. Creates a new tmux window named `<window_prefix><handle>` (e.g., `wm-feature-auth` with `window_prefix: wm-`). With `--session`, the window is created in its own dedicated session instead of the current session.
6. Sets up your configured tmux pane layout
7. Automatically switches your tmux client to the new window

## Examples

::: code-group

```bash [Basic usage]
# Create a new branch and worktree
workmux add user-auth

# Use an existing branch
workmux add existing-work

# Create a new branch from a specific base
workmux add hotfix --base production

# Create a worktree from a remote branch (creates local branch "user-auth-pr")
workmux add origin/user-auth-pr

# Remote branches with slashes work too (creates local branch "feature/foo")
workmux add origin/feature/foo

# Create a worktree in the background without switching to it
workmux add feature/parallel-task --background

# Use a custom name for the worktree directory and tmux window
workmux add feature/long-descriptive-branch-name --name short
```

```bash [Pull requests & forks]
# Checkout PR #123. The local branch will be named after the PR's branch.
workmux add --pr 123

# Checkout PR #456 with a custom local branch name
workmux add fix/api-bug --pr 456

# Checkout a fork branch using GitHub's owner:branch format (copy from GitHub UI)
workmux add someuser:feature-branch
```

```bash [Moving changes]
# Move uncommitted changes to a new worktree (including untracked files)
workmux add feature/new-thing --with-changes -u

# Move only staged/modified files (not untracked files)
workmux add fix/bug --with-changes

# Interactively select which changes to move
workmux add feature/partial --with-changes --patch
```

```bash [AI agent prompts]
# Create a worktree with an inline prompt for AI agents
workmux add feature/ai --prompt "Implement user authentication with OAuth"

# Override the default agent for a specific worktree
workmux add feature/testing -a gemini

# Create a worktree with a prompt from a file
workmux add feature/refactor --prompt-file task-description.md

# Open your editor to write a prompt interactively
workmux add feature/new-api --prompt-editor
```

```bash [Skip setup steps]
# Skip expensive setup for documentation-only changes
workmux add docs-update --no-hooks --no-file-ops --no-pane-cmds

# Skip just the file operations (e.g., you don't need node_modules)
workmux add quick-fix --no-file-ops
```

```bash [Scripting with --wait]
# Block until the agent completes and closes the window
workmux add feature/api --wait -p "Implement the REST API, then run: workmux remove --keep-branch"

# Use in a script to run sequential agent tasks
for task in task1.md task2.md task3.md; do
  workmux add "task-$(basename $task .md)" --wait -P "$task"
done
```

```bash [Session mode]
# Create a worktree in its own tmux session (instead of the current session)
workmux add feature/isolated --session

# Create in a new session without switching to it
workmux add feature/background-task --session --background

# Session mode works with all other flags
workmux add feature/ai-session --session -p "Implement the new API"
```

:::

## AI agent integration

When you provide a prompt via `--prompt`, `--prompt-file`, or `--prompt-editor`, workmux automatically injects the prompt into panes running the configured agent command (e.g., `claude`, `codex`, `opencode`, `gemini`, or whatever you've set via the `agent` config or `--agent` flag) without requiring any `.workmux.yaml` changes:

- Panes with a command matching the configured agent are automatically started with the given prompt.
- You can keep your `.workmux.yaml` pane configuration simple (e.g., `panes: [{ command: "<agent>" }]`) and let workmux handle prompt injection at runtime.

This means you can launch AI agents with task-specific prompts without modifying your project configuration for each task.

## Automatic branch name generation

The `--auto-name` (`-A`) flag generates a branch name from your prompt using an LLM via the [`llm`](https://llm.datasette.io/) CLI tool.

### Usage

```bash
# Opens editor for prompt, generates branch name
workmux add -A

# With inline prompt
workmux add -A -p "Add OAuth authentication"

# With prompt file
workmux add -A -P task-spec.md
```

### Requirements

Install the `llm` CLI tool:

```bash
pipx install llm
```

Configure a model (e.g., OpenAI):

```bash
llm keys set openai
# Or use a local model
llm install llm-ollama
```

### Configuration

Optionally specify a model and/or custom system prompt in `.workmux.yaml`:

```yaml
auto_name:
  model: "gemini-2.5-flash-lite"
  system_prompt: |
    Generate a concise git branch name based on the task description.

    Rules:
    - Use kebab-case (lowercase with hyphens)
    - Keep it short: 1-3 words, max 4 if necessary
    - Focus on the core task/feature, not implementation details
    - No prefixes like feat/, fix/, chore/

    Examples of good branch names:
    - "Add dark mode toggle" → dark-mode
    - "Fix the search results not showing" → fix-search
    - "Refactor the authentication module" → auth-refactor
    - "Add CSV export to reports" → export-csv
    - "Shell completion is broken" → shell-completion

    Output ONLY the branch name, nothing else.
```

If `model` is not configured, uses `llm`'s default model.

Recommended models for fast, cheap branch name generation:

- `gemini-2.5-flash-lite` (recommended)
- `gpt-5-nano`

## Parallel workflows & multi-worktree generation

workmux can generate multiple worktrees from a single `add` command, which is ideal for running parallel experiments or delegating tasks to multiple AI agents. This is controlled by four mutually exclusive modes:

- (`-a`, `--agent`): Create a worktree for each specified agent.
- (`-n`, `--count`): Create a specific number of worktrees.
- (`--foreach`): Create worktrees based on a matrix of variables.
- **stdin**: Pipe input lines to create worktrees with templated prompts.

When using any of these modes, branch names are generated from a template, and prompts can be templated with variables.

### Multi-worktree options

| Flag                           | Description                                                                                                                                                                                                                                                                                     |
| ------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `-a, --agent <name>`           | When used multiple times, creates one worktree for each agent.                                                                                                                                                                                                                                  |
| `-n, --count <number>`         | Creates `<number>` worktree instances. Can be combined with a single `--agent` flag to apply that agent to all instances.                                                                                                                                                                       |
| `--foreach <matrix>`           | Creates worktrees from a variable matrix string. The format is `"var1:valA,valB;var2:valX,valY"`. All value lists must have the same length. Values are paired by index position (zip, not Cartesian product): the first value of each variable goes together, the second with the second, etc. |
| `--branch-template <template>` | A [MiniJinja](https://docs.rs/minijinja/latest/minijinja/) (Jinja2-compatible) template for generating branch names. Available variables: `{{ base_name }}`, `{{ agent }}`, `{{ num }}`, `{{ index }}`, `{{ input }}` (stdin), and any variables from `--foreach`.                              |
| `--max-concurrent <number>`    | Limits how many worktrees run simultaneously. When set, workmux creates up to `<number>` worktrees, then waits for any window to close before starting the next. Requires agents to close windows when done (e.g., via prompt instruction to run `workmux remove --keep-branch`).               |

### Prompt templating

When generating multiple worktrees, any prompt provided via `-p`, `-P`, or `-e` is treated as a MiniJinja template. You can use variables from your generation mode to create unique prompts for each agent or instance.

### Variable matrices in prompt files

Instead of passing `--foreach` on the command line, you can specify the variable matrix directly in your prompt file using YAML frontmatter. This is more convenient for complex matrices and keeps the variables close to the prompt that uses them.

**Format:**

Create a prompt file with YAML frontmatter at the top, separated by `---`:

**Example 1:** `mobile-task.md`

```markdown
---
foreach:
  platform: [iOS, Android]
  lang: [swift, kotlin]
---

Build a {{ platform }} app using {{ lang }}. Implement user authentication and data persistence.
```

```bash
workmux add mobile-app --prompt-file mobile-task.md
# Generates worktrees: mobile-app-ios-swift, mobile-app-android-kotlin
```

**Example 2:** `agent-task.md` (using `agent` as a foreach variable)

```markdown
---
foreach:
  agent: [claude, gemini]
---

Implement the dashboard refactor using your preferred approach.
```

```bash
workmux add refactor --prompt-file agent-task.md
# Generates worktrees: refactor-claude, refactor-gemini
```

**Behavior:**

- Variables from the frontmatter are available in both the prompt template and the branch name template
- All value lists must have the same length, and values are paired by index position (same zip behavior as `--foreach`)
- CLI `--foreach` overrides frontmatter with a warning if both are present
- Works with both `--prompt-file` and `--prompt-editor`

### Stdin input

You can pipe input lines to `workmux add` to create multiple worktrees. Each line becomes available as the `{{ input }}` template variable in your prompt. This is useful for batch-processing tasks from external sources.

**Plain text:** Each line becomes `{{ input }}`

```bash
echo -e "api\nauth\ndatabase" | workmux add refactor -P task.md
# {{ input }} = "api", "auth", "database"
```

**JSON lines:** Each key becomes a template variable

```bash
gh repo list --json url,name --jq -c '.[]' | workmux add analyze \
  --branch-template '{{ base_name }}-{{ name }}' \
  -P prompt.md
# Line: {"url":"https://github.com/raine/workmux","name":"workmux"}
# Variables: {{ url }}, {{ name }}, {{ input }} (raw JSON line)
```

This lets you structure data upstream with `jq` and use meaningful branch names while keeping the full URL available in your prompt.

**Behavior:**

- Empty lines and whitespace-only lines are filtered out
- Stdin input cannot be combined with `--foreach` (mutually exclusive)
- JSON objects (lines starting with `{`) are parsed and each key becomes a variable
- `{{ input }}` always contains the raw line
- If JSON contains an `input` key, it overwrites the raw line value

### Examples

```bash
# Create one worktree for claude and one for gemini with a focused prompt
workmux add my-feature -a claude -a gemini -p "Implement the new search API integration"
# Generates worktrees: my-feature-claude, my-feature-gemini

# Create 2 instances of the default agent
workmux add my-feature -n 2 -p "Implement task #{{ num }} in TASKS.md"
# Generates worktrees: my-feature-1, my-feature-2

# Create worktrees from a variable matrix
workmux add my-feature --foreach "platform:iOS,Android" -p "Build for {{ platform }}"
# Generates worktrees: my-feature-ios, my-feature-android

# Create agent-specific worktrees via --foreach
workmux add my-feature --foreach "agent:claude,gemini" -p "Implement the dashboard refactor"
# Generates worktrees: my-feature-claude, my-feature-gemini

# Use frontmatter in a prompt file for cleaner syntax
# task.md contains:
# ---
# foreach:
#   env: [staging, production]
#   task: [smoke-tests, integration-tests]
# ---
# Run {{ task }} against the {{ env }} environment
workmux add testing --prompt-file task.md
# Generates worktrees: testing-staging-smoke-tests, testing-production-integration-tests

# Pipe input from stdin to create worktrees
# review.md contains: Review the {{ input }} module for security issues.
echo -e "auth\npayments\napi" | workmux add review -A -P review.md
# Generates worktrees with LLM-generated branch names for each module
```

### Recipe: Batch processing with worker pools

Combine stdin input, prompt templating, and concurrency limits to create a worker pool that processes items from an external command.

**Example: Generate test scaffolding for untested files**

```bash
# generate-tests.md contains:
# Read the file at {{ input }} and generate a test suite covering
# the exported functions. Focus on happy path and edge cases.
# When done, run: workmux remove --keep-branch

find src/utils -name "*.ts" ! -name "*.test.ts" | \
  workmux add add-tests \
    --branch-template '{{ base_name }}-{{ index }}' \
    --prompt-file generate-tests.md \
    --max-concurrent 3 \
    --background
```

- `find ...` lists files without tests (one per line) piped to stdin
- `--branch-template` uses `{{ index }}` for unique branch names
- `--prompt-file` uses `{{ input }}` to pass each file path to the agent
- `--max-concurrent 3` limits parallel agents to avoid rate limits
- `--background` runs without switching focus
