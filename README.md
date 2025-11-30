<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="meta/logo-dark.svg">
    <img src="meta/logo.svg" alt="workmux icon" width="300">
  </picture>
</p>

<p align="center">
  <strong>Parallel development in tmux with git worktrees</strong>
</p>

<p align="center">
  <a href="#why-git-worktrees">Why?</a> ·
  <a href="#installation">Install</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#commands">Commands</a> ·
  <a href="CHANGELOG.md">Changelog</a>
</p>

---

Giga opinionated zero-friction workflow tool for managing
[git worktrees](https://git-scm.com/docs/git-worktree) and tmux windows as
isolated development environments. Perfect for running multiple AI agents in
parallel without conflict.

![workmux demo](https://raw.githubusercontent.com/raine/workmux/refs/heads/main/meta/workmux-demo.gif)

## Philosophy

- **One worktree, one tmux window**: Each git worktree gets its own dedicated,
  pre-configured tmux window.
- **Frictionless**: Multi-step workflows are reduced to simple commands.
- **Configuration as code**: Define your tmux layout and setup steps in
  `.workmux.yaml`.

The core principle is that **tmux is the interface**. If you already live in
tmux, you shouldn't need to learn a new TUI app or separate interface to manage
your work. With workmux, managing parallel development tasks, or multiple AI
agents, is as simple as managing tmux windows.

## Features

- Create git worktrees with matching tmux windows in a single command (`add`)
- Automatically set up your preferred pane layout (editor, shell, watchers,
  etc.)
- Run post-creation hooks (install dependencies, setup database, etc.)
- Copy or symlink configuration files (`.env`, `node_modules`) into new
  worktrees
- Merge branches and clean up everything (worktree, tmux window, branches) in
  one command (`merge`)
- List all worktrees with their tmux and merge status
- Bootstrap projects with an initial configuration file (`init`)
- Dynamic shell completions for branch names

## Hype

> "I've been using (and loving) workmux which brings together tmux, git
> worktrees, and CLI agents into an opinionated workflow."  
> — @Coolin96 [🔗](https://news.ycombinator.com/item?id=46029809)

> "Thank you so much for your work with workmux! It's a tool I've been wanting
> to exist for a long time."  
> — @rstacruz [🔗](https://github.com/raine/workmux/issues/2)

## Installation

### Homebrew (macOS/Linux)

```bash
brew install raine/workmux/workmux
```

### Cargo

Requires Rust. Install via [rustup](https://rustup.rs/) if you don't have it.

```bash
cargo install workmux
```

## Quick start

1. **Initialize configuration (optional)**:

   ```bash
   workmux init
   ```

   This creates a `.workmux.yaml` file to customize your workflow (pane layouts,
   setup commands, file operations, etc.). workmux works out of the box with
   sensible defaults, so this step is optional.

2. **Create a new worktree and tmux window**:

   ```bash
   workmux add new-feature
   ```

   This will:
   - Create a git worktree at
     `<project_root>/../<project_name>__worktrees/new-feature`
   - Create a tmux window named `wm-new-feature` (the prefix is configurable)
   - Automatically switch your tmux client to the new window

3. **Do your thing**

4. **When done, merge and clean up**:

   ```bash
   # Run in the worktree window
   workmux merge
   ```

Merges your branch into main and cleans up everything (tmux window, worktree,
and local branch).

## Configuration

workmux uses a two-level configuration system:

- **Global** (`~/.config/workmux/config.yaml`): Personal defaults for all
  projects
- **Project** (`.workmux.yaml`): Project-specific overrides

Project settings override global settings. For `post_create` and file operation
lists (`files.copy`, `files.symlink`), you can use `"<global>"` to include
global values alongside project-specific ones. Other settings like `panes` are
replaced entirely when defined in the project config.

### Global configuration example

`~/.config/workmux/config.yaml`:

```yaml
window_prefix: wm-

panes:
  - command: nvim .
    focus: true
  # Just a default shell (command omitted)
  - split: horizontal

post_create:
  - mise install

files:
  symlink:
    - node_modules

agent: claude
```

### Project configuration example

`.workmux.yaml`:

```yaml
post_create:
  - '<global>'
  - mise use

files:
  symlink:
    - '<global>' # Include global symlinks (node_modules)
    - .pnpm-store # Add project-specific symlink

panes:
  - command: pnpm install
    focus: true
  - command: <agent>
    split: horizontal
  - command: pnpm run dev
    split: vertical
```

For a real-world example, see
[workmux's own `.workmux.yaml`](https://github.com/raine/workmux/blob/main/.workmux.yaml).

### Configuration options

- `main_branch`: Branch to merge into (optional, auto-detected from remote or
  checks for `main`/`master`)
- `worktree_dir`: Custom directory for worktrees (absolute or relative to repo
  root)
- `window_prefix`: Prefix for tmux window names (default: `wm-`)
- `worktree_naming`: Strategy for deriving worktree/window names from branch
  names
  - `full` (default): Use the full branch name (slashes become dashes)
  - `basename`: Use only the part after the last `/` (e.g., `prj-123/feature` →
    `feature`)
- `worktree_prefix`: Prefix prepended to worktree directory and window names.
  Note: This stacks with `window_prefix`, so a worktree with
  `worktree_prefix: web-` and default `window_prefix: wm-` creates windows like
  `wm-web-feature`.
- `panes`: Array of pane configurations
  - `command`: Optional command to run when the pane is created. Use this for
    long-running setup like dependency installs so output is visible in tmux. If
    omitted, the pane starts with your default shell. Use `<agent>` to use the
    configured agent.
  - `focus`: Whether this pane should receive focus (default: false)
  - `split`: How to split from previous pane (`horizontal` or `vertical`)
  - `size`: Optional absolute size in lines (for vertical splits) or cells (for
    horizontal splits). Mutually exclusive with `percentage`. If neither is
    specified, tmux splits 50/50.
  - `percentage`: Optional size as a percentage (1-100) of the available space.
    Mutually exclusive with `size`. If neither is specified, tmux splits 50/50.
- `post_create`: Commands to run after worktree creation but before the tmux
  window opens. These block window creation, so keep them short (e.g., copying
  config files).
- `files`: File operations to perform on worktree creation
  - `copy`: List of glob patterns for files/directories to copy
  - `symlink`: List of glob patterns for files/directories to symlink
- `agent`: The default agent command to use for `<agent>` in pane commands
  (e.g., `claude`, `gemini`). This can be overridden by the `--agent` flag.
  Default: `claude`.
- `merge_strategy`: Default strategy for `workmux merge` (`merge`, `rebase`, or
  `squash`). CLI flags (`--rebase`, `--squash`) always override this setting.
  Default: `merge`.

#### Default behavior

- Worktrees are created in `<project>__worktrees` as a sibling directory to your
  project by default
- If no `panes` configuration is defined, workmux provides opinionated defaults:
  - For projects with a `CLAUDE.md` file: Opens the configured agent (see
    `agent` option) in the first pane, defaulting to `claude` if none is set.
  - For all other projects: Opens your default shell.
  - Both configurations include a second pane split horizontally
- `post_create` commands are optional and only run if you configure them

### Directory structure

Here's how workmux organizes your worktrees by default:

```
~/projects/
├── my-project/               <-- Main project directory
│   ├── src/
│   ├── package.json
│   └── .workmux.yaml
│
└── my-project__worktrees/    <-- Worktrees created by workmux
    ├── feature-A/            <-- Isolated workspace for 'feature-A' branch
    │   ├── src/
    │   └── package.json
    │
    └── bugfix-B/             <-- Isolated workspace for 'bugfix-B' branch
        ├── src/
        └── package.json
```

Each worktree is a separate working directory for a different branch, all
sharing the same git repository. This allows you to work on multiple branches
simultaneously without conflicts.

You can customize the worktree directory location using the `worktree_dir`
configuration option (see [Configuration options](#configuration-options)).

### Shell alias (recommended)

For faster typing, alias `workmux` to `wm`:

```bash
alias wm='workmux'
```

## Commands

- [`add`](#workmux-add-branch-name) - Create a new worktree and tmux window
- [`merge`](#workmux-merge-branch-name) - Merge a branch and clean up everything
- [`remove`](#workmux-remove-branch-name) - Remove a worktree without merging
- [`list`](#workmux-list) - List all worktrees with status
- [`init`](#workmux-init) - Generate configuration file
- [`open`](#workmux-open-branch-name) - Open a tmux window for an existing
  worktree
- [`path`](#workmux-path-branch-name) - Get the filesystem path of a worktree
- [`claude prune`](#workmux-claude-prune) - Clean up stale Claude Code entries
- [`completions`](#workmux-completions-shell) - Generate shell completions

### `workmux add <branch-name>`

Creates a new git worktree with a matching tmux window and switches you to it
immediately. If the branch doesn't exist, it will be created automatically.

- `<branch-name>`: Name of the branch to create or switch to, a remote branch
  reference (e.g., `origin/feature-branch`), or a GitHub fork reference (e.g.,
  `user:branch`). Remote and fork references are automatically fetched and
  create a local branch with the derived name. Optional when using `--pr`.

#### Options

- `--base <branch|commit|tag>`: Specify a base branch, commit, or tag to branch
  from when creating a new branch. By default, new branches are created from the
  current branch you have checked out.
- `--pr <number>`: Checkout a GitHub pull request by its number into a new
  worktree.
  - Requires the `gh` command-line tool to be installed and authenticated.
  - The local branch name defaults to the PR's head branch name, but can be
    overridden (e.g., `workmux add custom-name --pr 123`).
- `--name <name>`: Override the worktree directory and tmux window name. By
  default, these are derived from the branch name (slugified). Cannot be used
  with multi-worktree generation (`--count`, `--foreach`, or multiple
  `--agent`).
- `-b, --background`: Create the tmux window in the background without switching
  to it. Useful with `--prompt-editor`.
- `-w, --with-changes`: Move uncommitted changes from the current worktree to
  the new worktree, then reset the original worktree to a clean state. Useful
  when you've started working on main and want to move your branches to a new
  worktree.
- `--patch`: Interactively select which changes to move (requires
  `--with-changes`). Opens an interactive prompt for selecting hunks to stash.
- `-u, --include-untracked`: Also move untracked files (requires
  `--with-changes`). By default, only staged and modified tracked files are
  moved.
- `-p, --prompt <text>`: Provide an inline prompt that will be automatically
  passed to AI agent panes.
- `-P, --prompt-file <path>`: Provide a path to a file whose contents will be
  used as the prompt.
- `-e, --prompt-editor`: Open your `$EDITOR` (or `$VISUAL`) to write the prompt
  interactively.
- `-a, --agent <name>`: The agent(s) to use for the worktree(s). Can be
  specified multiple times to generate a worktree for each agent. Overrides the
  `agent` from your config file.

#### Skip options

These options allow you to skip expensive setup steps when they're not needed
(e.g., for documentation-only changes):

- `-H, --no-hooks`: Skip running `post_create` commands
- `-F, --no-file-ops`: Skip file copy/symlink operations (e.g., skip linking
  `node_modules`)
- `-C, --no-pane-cmds`: Skip executing pane commands (panes open with plain
  shells instead)

#### What happens

1. Determines the **handle** for the worktree by slugifying the branch name
   (e.g., `feature/auth` becomes `feature-auth`). This can be overridden with
   the `--name` flag.
2. Creates a git worktree at `<worktree_dir>/<handle>` (the `worktree_dir` is
   configurable and defaults to a sibling directory of your project)
3. Runs any configured file operations (copy/symlink)
4. Executes `post_create` commands if defined (runs before the tmux window
   opens, so keep them fast)
5. Creates a new tmux window named `<window_prefix><handle>` (e.g.,
   `wm-feature-auth`)
6. Sets up your configured tmux pane layout
7. Automatically switches your tmux client to the new window

#### Examples

##### Basic usage

```bash
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

##### Checking out pull requests and fork branches

```bash
# Checkout PR #123. The local branch will be named after the PR's branch.
workmux add --pr 123

# Checkout PR #456 with a custom local branch name
workmux add fix/api-bug --pr 456

# Checkout a fork branch using GitHub's owner:branch format (copy from GitHub UI)
workmux add someuser:feature-branch
```

##### Moving changes to a new worktree

```bash
# Move uncommitted changes to a new worktree (including untracked files)
workmux add feature/new-thing --with-changes -u

# Move only staged/modified files (not untracked files)
workmux add fix/bug --with-changes

# Interactively select which changes to move
workmux add feature/partial --with-changes --patch
```

##### AI agent prompts

```bash
# Create a worktree with an inline prompt for AI agents
workmux add feature/ai --prompt "Implement user authentication with OAuth"

# Override the default agent for a specific worktree
workmux add feature/testing -a gemini

# Create a worktree with a prompt from a file
workmux add feature/refactor --prompt-file task-description.md

# Open your editor to write a prompt interactively
workmux add feature/new-api --prompt-editor
```

##### Skipping setup steps

```bash
# Skip expensive setup for documentation-only changes
workmux add docs-update --no-hooks --no-file-ops --no-pane-cmds

# Skip just the file operations (e.g., you don't need node_modules)
workmux add quick-fix --no-file-ops
```

#### AI agent integration

When you provide a prompt via `--prompt`, `--prompt-file`, or `--prompt-editor`,
workmux automatically injects the prompt into panes running the configured agent
command (e.g., `claude`, `gemini`, or whatever you've set via the `agent` config
or `--agent` flag) without requiring any `.workmux.yaml` changes:

- Panes with a command matching the configured agent are automatically started
  with the given prompt.
- You can keep your `.workmux.yaml` pane configuration simple (e.g.,
  `panes: [{ command: "<agent>" }]`) and let workmux handle prompt injection at
  runtime.

This means you can launch AI agents with task-specific prompts without modifying
your project configuration for each task.

#### Parallel workflows & multi-worktree generation

workmux can generate multiple worktrees from a single `add` command, which is
ideal for running parallel experiments or delegating tasks to multiple AI
agents. This is controlled by three mutually exclusive modes:

- (`-a`, `--agent`): Create a worktree for each specified agent.
- (`-n`, `--count`): Create a specific number of worktrees.
- (`--foreach`): Create worktrees based on a matrix of variables.

When using any of these modes, branch names are generated from a template, and
prompts can be templated with variables.

##### Multi-worktree options

- `-a, --agent <name>`: When used multiple times, creates one worktree for each
  agent.
- `-n, --count <number>`: Creates `<number>` worktree instances. Can be combined
  with a single `--agent` flag to apply that agent to all instances.
- `--foreach <matrix>`: Creates worktrees from a variable matrix string. The
  format is `"var1:valA,valB;var2:valX,valY"`. All value lists must have the
  same length. Values are paired by index position (zip, not Cartesian product):
  the first value of each variable goes together, the second with the second,
  etc.
- `--branch-template <template>`: A
  [MiniJinja](https://docs.rs/minijinja/latest/minijinja/) (Jinja2-compatible)
  template for generating branch names.
  - Available variables: `{{ base_name }}`, `{{ agent }}`, `{{ num }}`, and any
    variables from `--foreach`.
  - Default:
    `{{ base_name }}{% if agent %}-{{ agent | slugify }}{% endif %}{% for key, value in foreach_vars %}-{{ value | slugify }}{% endfor %}{% if num %}-{{ num }}{% endif %}`

##### Prompt templating

When generating multiple worktrees, any prompt provided via `-p`, `-P`, or `-e`
is treated as a MiniJinja template. You can use variables from your generation
mode to create unique prompts for each agent or instance.

##### Variable matrices in prompt files

Instead of passing `--foreach` on the command line, you can specify the variable
matrix directly in your prompt file using YAML frontmatter. This is more
convenient for complex matrices and keeps the variables close to the prompt that
uses them.

**Format:**

Create a prompt file with YAML frontmatter at the top, separated by `---`:

**Example 1:** `mobile-task.md`

```markdown
---
foreach:
  platform: [iOS, Android]
  lang: [swift, kotlin]
---

Build a {{ platform }} app using {{ lang }}. Implement user authentication and
data persistence.
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

- Variables from the frontmatter are available in both the prompt template and
  the branch name template
- All value lists must have the same length, and values are paired by index
  position (same zip behavior as `--foreach`)
- CLI `--foreach` overrides frontmatter with a warning if both are present
- Works with both `--prompt-file` and `--prompt-editor`

##### Examples

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
```

---

### `workmux merge [branch-name]`

Merges a branch into the main branch and automatically cleans up all associated
resources (worktree, tmux window, and local branch).

- `[branch-name]`: Optional name of the branch to merge. If omitted,
  automatically detects the current branch from the worktree you're in.

#### Options

- `--ignore-uncommitted`: Commit any staged changes before merging without
  opening an editor
- `--delete-remote`, `-r`: Also delete the remote branch after a successful
  merge
- `--keep`, `-k`: Keep the worktree, window, and branch after merging (skip
  cleanup). Useful when you want to verify the merge before cleaning up.

#### Merge strategies

By default, `workmux merge` performs a standard merge commit (configurable via
`merge_strategy`). You can override the configured behavior with these mutually
exclusive flags:

- `--rebase`: Rebase the feature branch onto main before merging (creates a
  linear history via fast-forward merge). If conflicts occur, you'll need to
  resolve them manually in the worktree and run `git rebase --continue`.
- `--squash`: Squash all commits from the feature branch into a single commit on
  main. You'll be prompted to provide a commit message in your editor.

#### What happens

1. Determines which branch to merge (specified branch or current branch if
   omitted)
2. Checks for uncommitted changes (errors if found, unless
   `--ignore-uncommitted` is used)
3. Commits staged changes if present (unless `--ignore-uncommitted` is used)
4. Merges your branch into main using the selected strategy (default: merge
   commit)
5. Deletes the tmux window (including the one you're currently in if you ran
   this from a worktree) — skipped if `--keep` is used
6. Removes the worktree — skipped if `--keep` is used
7. Deletes the local branch — skipped if `--keep` is used

#### Typical workflow

When you're done working in a worktree, simply run `workmux merge` from within
that worktree's tmux window. The command will automatically detect which branch
you're on, merge it into main, and close the current window as part of cleanup.

#### Examples

```bash
# Merge branch from main branch (default: merge commit)
workmux merge user-auth

# Merge the current worktree you're in
# (run this from within the worktree's tmux window)
workmux merge

# Rebase onto main before merging for a linear history
workmux merge user-auth --rebase

# Squash all commits into a single commit
workmux merge user-auth --squash

# Merge and also delete the remote branch
workmux merge user-auth --delete-remote

# Merge but keep the worktree/window/branch to verify before cleanup
workmux merge user-auth --keep
# ... verify the merge in main ...
workmux remove user-auth  # clean up later when ready
```

---

### `workmux remove <branch-name>` (alias: `rm`)

Removes a worktree, tmux window, and branch without merging (unless you keep the
branch). Useful for abandoning work or cleaning up experimental branches.

- `<branch-name>`: Name of the branch to remove.

#### Options

- `--force`, `-f`: Skip confirmation prompt and ignore uncommitted changes
- `--delete-remote`, `-r`: Also delete the remote branch
- `--keep-branch`, `-k`: Remove only the worktree and tmux window while keeping
  the local branch (incompatible with `--delete-remote`)

#### Examples

```bash
# Remove with confirmation if unmerged
workmux remove experiment

# Use the alias
workmux rm old-work

# Remove worktree/window but keep the branch
workmux remove --keep-branch experiment

# Force remove without prompts
workmux rm -f experiment

# Force remove and delete remote branch
workmux rm -f -r old-work
```

---

### `workmux list` (alias: `ls`)

Lists all git worktrees with their tmux window status and merge status.

#### Examples

```bash
# List all worktrees
workmux list
```

#### Example output

```
BRANCH      TMUX    UNMERGED    PATH
------      ----    --------    ----
main        -       -           ~/project
user-auth   ✓       -           ~/project__worktrees/user-auth
bug-fix     ✓       ●           ~/project__worktrees/bug-fix
```

#### Key

- `✓` in TMUX column = tmux window exists for this worktree
- `●` in UNMERGED column = branch has commits not merged into main
- `-` = not applicable

---

### `workmux init`

Generates `.workmux.yaml` with example configuration and `"<global>"`
placeholder usage.

#### Examples

```bash
workmux init
```

---

### `workmux open <branch-name>`

Opens a new tmux window for a pre-existing git worktree, setting up the
configured pane layout and environment. This is useful any time you closed the
tmux window for a worktree you are still working on.

- `<branch-name>`: Name of the branch that has an existing worktree.

#### Options

- `--run-hooks`: Re-runs the `post_create` commands (these block window
  creation).
- `--force-files`: Re-applies file copy/symlink operations. Useful for restoring
  a deleted `.env` file.

#### What happens

1. Verifies that a worktree for `<branch-name>` exists and a tmux window does
   not.
2. Creates a new tmux window named after the branch.
3. (If specified) Runs file operations and `post_create` hooks.
4. Sets up your configured tmux pane layout.
5. Automatically switches your tmux client to the new window.

#### Examples

```bash
# Open a window for an existing worktree
workmux open user-auth

# Open and re-run dependency installation
workmux open user-auth --run-hooks

# Open and restore configuration files
workmux open user-auth --force-files
```

---

### `workmux path <branch-name>`

Prints the filesystem path of an existing worktree. Useful for scripting or
quickly navigating to a worktree directory.

- `<branch-name>`: Name of the branch that has an existing worktree.

#### Examples

```bash
# Get the path of a worktree
workmux path user-auth
# Output: /Users/you/project__worktrees/user-auth

# Use in scripts or with cd
cd "$(workmux path user-auth)"

# Copy a file to a worktree
cp config.json "$(workmux path feature-branch)/"
```

---

### `workmux claude prune`

Removes stale entries from Claude config (`~/.claude.json`) that point to
deleted worktree directories. When you run Claude Code in worktrees, it stores
per-worktree settings in that file. Over time, as worktrees are merged or
deleted, it can accumulate entries for paths that no longer exist.

#### What happens

1. Scans `~/.claude.json` for entries pointing to non-existent directories
2. Creates a backup at `~/.claude.json.bak` before making changes
3. Removes all stale entries
4. Reports the number of entries cleaned up

#### Safety

- Only removes entries for absolute paths that don't exist
- Creates a backup before modifying the file
- Preserves all valid entries and relative paths

#### Examples

```bash
# Clean up stale Claude Code entries
workmux claude prune
```

#### Example output

```
  - Removing: /Users/user/project__worktrees/old-feature

✓ Created backup at ~/.claude.json.bak
✓ Removed 3 stale entries from ~/.claude.json
```

---

### `workmux completions <shell>`

Generates shell completion script for the specified shell. Completions provide
tab-completion for commands and dynamic branch name suggestions.

- `<shell>`: Shell type: `bash`, `zsh`, or `fish`.

#### Examples

```bash
# Generate completions for zsh
workmux completions zsh
```

See the [Shell Completions](#shell-completions) section for installation
instructions.

## Workflow example

Here's a complete workflow:

```bash
# Start a new feature
workmux add user-auth

# Work on your feature...
# (tmux automatically sets up your configured panes and environment)

# When ready, merge and clean up
workmux merge user-auth

# Start another feature
workmux add api-endpoint

# List all active worktrees
workmux list
```

## Why workmux?

workmux turns a multi-step manual workflow into two simple commands, making
parallel development workflows practical.

### Without workmux

```bash
# 1. Manually create the worktree and environment
git worktree add ../worktrees/user-auth -b user-auth
cd ../worktrees/user-auth
cp ../../project/.env.example .env
ln -s ../../project/node_modules .
npm install
# ... and other setup steps

# 2. Manually create and configure the tmux window
tmux new-window -n user-auth
tmux split-window -h 'npm run dev'
tmux send-keys -t 0 'claude' C-m
# ... repeat for every pane in your desired layout

# 3. When done, manually merge and clean everything up
cd ../../project
git switch main && git pull
git merge --no-ff user-auth
tmux kill-window -t user-auth
git worktree remove ../worktrees/user-auth
git branch -d user-auth
```

### With workmux

```bash
# Create the environment
workmux add user-auth

# ... work on the feature ...

# Merge and clean up
workmux merge
```

### The parallel AI workflow (with workmux)

Delegate multiple complex tasks to AI agents and let them work at the same time.
This workflow is cumbersome to manage manually.

```bash
# Task 1: Refactor the user model (for Agent 1)
workmux add refactor/user-model

# Task 2: Build a new API endpoint (for Agent 2, in parallel)
workmux add feature/new-api

# ... Command agents work simultaneously in their isolated environments ...

# Merge each task as it's completed
workmux merge refactor/user-model
workmux merge feature/new-api
```

## Why git worktrees?

[Git worktrees](https://git-scm.com/docs/git-worktree) let you have multiple
branches checked out at once in the same repository, each in a separate
directory. This provides two main advantages over a standard single-directory
setup:

- **Painless context switching**: Switch between tasks just by changing
  directories (`cd ../other-branch`). There's no need to `git stash` or make
  temporary commits. Your work-in-progress, editor state, and command history
  remain isolated and intact for each branch.

- **True parallel development**: Work on multiple branches simultaneously
  without interference. You can run builds, install dependencies
  (`npm install`), or run tests in one worktree while actively coding in
  another. This isolation is perfect for running multiple AI agents in parallel
  on different tasks.

In a standard Git setup, switching branches disrupts your flow by requiring a
clean working tree. Worktrees remove this friction. `workmux` automates the
entire process and pairs each worktree with a dedicated tmux window, creating
fully isolated development environments. See [Why workmux?](#why-workmux) for
how workmux streamlines this workflow.

## Git worktree caveats

While powerful, git worktrees have nuances that are important to understand.
workmux is designed to automate solutions to these, but awareness of the
underlying mechanics helps.

- [Gitignored files require configuration](#gitignored-files-require-configuration)
- [Conflicts](#conflicts)
- [Package manager considerations (pnpm, yarn)](#package-manager-considerations-pnpm-yarn)
- [Build directories (Rust `target`, etc.)](#build-directories-rust-target-etc)
- [Local git ignores (`.git/info/exclude`) are not shared](#local-git-ignores-gitinfoexclude-are-not-shared)

### Gitignored files require configuration

When `git worktree add` creates a new working directory, it's a clean checkout.
Files listed in your `.gitignore` (e.g., `.env` files, `node_modules`, IDE
configuration) will not exist in the new worktree by default. Your application
will be broken in the new worktree until you manually create or link these
necessary files.

This is a primary feature of workmux. Use the `files` section in your
`.workmux.yaml` to automatically copy or symlink these files on creation:

```yaml
# .workmux.yaml
files:
  copy:
    - .env # Copy environment variables
  symlink:
    - .next/cache # Share Next.js build cache
```

Note: Symlinking `node_modules` can be efficient but only works if all worktrees
share identical dependencies. If different branches have different dependency
versions, each worktree needs its own installation. For dependency installation,
consider using a pane command instead of `post_create` hooks - this runs the
install in the background without blocking the worktree and window creation:

```yaml
panes:
  - command: npm install
    focus: true
  - split: horizontal
```

### Conflicts

Worktrees isolate your filesystem, but they do not prevent merge conflicts. If
you modify the area of code on two different branches (in two different
worktrees), you will still have a conflict when you merge one into the other.

The best practice is to work on logically separate features in parallel
worktrees. When conflicts are unavoidable, use standard git tools to resolve
them. You can also leverage an AI agent within the worktree to assist with the
conflict resolution.

### Package manager considerations (pnpm, yarn)

Modern package managers like `pnpm` use a global store with symlinks to
`node_modules`. Each worktree typically needs its own `pnpm install` to set up
the correct dependency versions for that branch.

If your worktrees always have identical dependencies (e.g., working on multiple
features from the same base), you could potentially symlink `node_modules`
between worktrees. However, this breaks as soon as branches diverge in their
dependencies, so it's generally safer to run a fresh install in each worktree.

Note: In large monorepos, cleaning up `node_modules` during worktree removal can
take significant time. workmux has a
[special cleanup mechanism](https://github.com/raine/workmux/blob/main/src/config.rs#L12)
that moves `node_modules` to a temporary location and deletes it in the
background, making the `remove` command return almost instantly.

### Build directories (Rust `target`, etc.)

For compiled languages, symlinking build directories can save disk space and
speed up builds:

```yaml
# .workmux.yaml
files:
  symlink:
    - target
```

### Local git ignores (`.git/info/exclude`) are not shared

The local git ignore file, `.git/info/exclude`, is specific to the main
worktree's git directory and is not respected in other worktrees. Personal
ignore patterns for your editor or temporary files may not apply in new
worktrees, causing them to appear in `git status`.

For personal ignores, use a global git ignore file. For project-specific ignores
that are safe to share with your team, add them to the project's main
`.gitignore` file.

## Tips

### Closing tmux windows

You can close workmux-managed tmux windows using tmux's standard `kill-window`
command (e.g., `<prefix> &` or `tmux kill-window -t <window-name>`). This will
properly terminate all processes running in the window's panes. The git worktree
will remain on disk, and you can reopen a window for it anytime with:

```bash
workmux open <branch-name>
```

However, it's recommended to use `workmux merge` or `workmux remove` for cleanup
instead, as these commands clean up both the tmux window and the git worktree
together. Use `workmux list` to see which worktrees have detached tmux windows.

## Shell completions

To enable tab completions for commands and branch names, add the following to
your shell's configuration file.

For **bash**, add to your `.bashrc`:

```bash
eval "$(workmux completions bash)"
```

For **zsh**, add to your `.zshrc`:

```bash
eval "$(workmux completions zsh)"
```

For **fish**, add to your `config.fish`:

```bash
workmux completions fish | source
```

## Requirements

- Rust (for building)
- Git 2.5+ (for worktree support)
- tmux

## Inspiration and related tools

workmux is inspired by [wtp](https://github.com/satococoa/wtp), an excellent git
worktree management tool. While wtp streamlines worktree creation and setup,
workmux takes this further by tightly coupling worktrees with tmux window
management.

For managing multiple AI agents in parallel, tools like
[claude-squad](https://github.com/smtg-ai/claude-squad) and
[vibe-kanban](https://github.com/BloopAI/vibe-kanban/) offer dedicated
interfaces, like a TUI or kanban board. In contrast, workmux adheres to its
philosophy that **tmux is the interface**, providing a native tmux experience
for managing parallel workflows without requiring a separate interface to learn.

## Related projects

- [tmux-file-picker](https://github.com/raine/tmux-file-picker) — Pop up fzf in
  tmux to quickly insert file paths, perfect for AI coding assistants
- [tmux-bro](https://github.com/raine/tmux-bro) — Smart tmux session manager
  that sets up project-specific sessions automatically
- [claude-history](https://github.com/raine/claude-history) — Search and view
  Claude Code conversation history with fzf
- [consult-llm-mcp](https://github.com/raine/consult-llm-mcp) — MCP server that
  lets Claude Code consult stronger AI models (o3, Gemini, GPT-5.1 Codex)
