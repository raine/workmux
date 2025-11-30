# Changelog

<!-- skipped: v0.1.27 -->
<!-- skipped: v0.1.25 -->
<!-- skipped: v0.1.8 -->

## Unreleased

- Fixed worktree path calculation when running `add` from inside an existing
  worktree, which previously created nested paths instead of sibling worktrees

## v0.1.33 (2025-11-30)

- Added support for GitHub fork branch format (`user:branch`) in `add` command,
  allowing direct checkout of fork branches copied from GitHub's UI

## v0.1.32 (2025-11-30)

- Added OpenCode agent support: prompts are now automatically passed using the
  `-p` flag when using `--prompt-file` or `--prompt-editor` with
  `--agent opencode`

## v0.1.31 (2025-11-29)

- Added `path` command to get the filesystem path of a worktree by branch name
- Added `--name` flag to `add` command for explicit worktree directory and tmux
  window naming
- Added `worktree_naming` config option to control how worktree names are
  derived from branches (`full` or `basename`)
- Added `worktree_prefix` config option to add a prefix to all worktree
  directory names
- Added `merge_strategy` config option to set default merge behavior (merge,
  rebase, or squash)

## v0.1.30 (2025-11-27)

- Added nushell support for pane startup commands
- Improved reliability of pane command execution across different shells

## v0.1.29 (2025-11-26)

- Shell completions now suggest git branch names when using the `add` command

## v0.1.28 (2025-11-26)

- Shell completions now dynamically suggest branch names when pressing TAB for
  `open`, `merge`, and `remove` commands (bash, zsh, fish)

## v0.1.26 (2025-11-25)

- Added `--pr` flag to checkout a GitHub pull request directly into a new
  worktree
- Fixed version managers (nvm, pnpm, mise, etc.) being shadowed by stale PATH
  entries when running pane commands
- Improved list output with cleaner table formatting and relative paths
- Fixed duplicate command announcement when running merge workflow

## v0.1.24 (2025-11-22)

- Fixed "can't find pane: 0" errors when using `pane-base-index 1` in tmux
  configuration
- Merge conflicts now abort cleanly, keeping your main worktree in a usable
  state with guidance on how to resolve

## v0.1.23 (2025-11-22)

- Added `--keep` flag to merge command to merge without cleaning up the
  worktree, useful for verifying the merge before removing the branch
- Fixed a bug where multi-agent worktrees had incorrect agent configuration for
  worktrees after the first one
- After closing a worktree (merge or remove), the terminal now navigates back to
  the main worktree instead of staying in the deleted directory

## v0.1.22 (2025-11-21)

- Added YAML frontmatter support in prompt files for defining variable matrices
  (`foreach`), making it easier to specify multi-worktree generation without CLI
  flags
- Added `size` and `percentage` options for pane configuration to control pane
  dimensions when splitting
- Fixed prompt editor temporary file now using `.md` extension for better editor
  syntax highlighting
- Fixed Gemini agent startup issues

## v0.1.21 (2025-11-18)

- Switched templating engine from Tera to MiniJinja (Jinja2-compatible) for
  branch names and prompts. Existing templates should work unchanged.

## v0.1.20 (2025-11-18)

- Fixed prompts starting with a dash (e.g. "- foo") being incorrectly
  interpreted as CLI flags
- The `rm` command now automatically uses the correct base branch that was used
  when the worktree was created, instead of defaulting to the main branch

## v0.1.19 (2025-11-17)

- Added `--with-changes` flag to `add` command: move uncommitted changes from
  your current worktree to a new one, useful when you've started working on the
  wrong branch
- Added `--patch` flag: interactively select which changes to move when using
  `--with-changes`
- Added `--include-untracked` (`-u`) flag: include untracked files when moving
  changes

## v0.1.18 (2025-11-17)

- New branches now default to branching from your currently checked out branch
  instead of the main branch's remote tracking branch
- Removed the `--from-current` flag (no longer needed since this is now the
  default behavior)

## v0.1.17 (2025-11-17)

- Added multi-agent workflows: create multiple worktrees from a single command
  using `-a agent1 -a agent2`, `-n count`, or `--foreach` matrix options
- Added background mode (`-b`, `--background`) to create worktrees without
  switching to them
- Added support for prompt templating with variables like `{{ agent }}`,
  `{{ num }}`, and custom `--foreach` variables
- Added `--branch-template` option to customize generated branch names

## v0.1.16 (2025-11-16)

- Added `--prompt-editor` (`-e`) flag to write prompts using your `$EDITOR`
- Added configurable agent support with `--agent` (`-a`) flag and config option
- Added flags to skip setup steps: `--no-hooks`, `--no-file-ops`,
  `--no-pane-cmds`
- Defaulted to current branch as base for `workmux add` (errors on detached HEAD
  without explicit `--base`)
- Fixed aliases containing `<agent>` placeholder not resolving correctly

## v0.1.15 (2025-11-15)

- Added `--prompt` (`-p`) and `--prompt-file` (`-P`) options to `workmux add`
  for attaching a prompt to new worktrees
- Added `--keep-branch` (`-k`) option to `workmux remove` to preserve the local
  branch while removing the worktree and tmux window

## v0.1.14 (2025-11-14)

- Added `--base` option to specify a base branch, commit, or tag when creating a
  new worktree
- Added `--from-current` (`-c`) flag to use the current branch as the base,
  useful for stacking feature branches
- Added support for creating worktrees from remote branches (e.g.,
  `workmux add origin/feature-branch`)
- Added support for copying directories (not just files) in file operations

## v0.1.13 (2025-11-13)

- Fixed `merge` and `remove` commands failing when run from within the worktree
  being deleted
- Added safety check to prevent accidentally deleting a branch that's checked
  out in the main worktree
- Fixed pane startup commands not loading shell environment tools (like direnv,
  nvm, rbenv) before running

## v0.1.11 (2025-11-11)

- Added `pre_delete` hooks that run before worktree deletion, with automatic
  detection of Node.js projects to fast-delete `node_modules` directories in the
  background
- Pane commands now keep an interactive shell open after completion, and panes
  can be created without a command (just a shell)
- Added `target` option for panes to split from any existing pane, not just the
  most recent one
- Tmux panes now use login shells for consistent environment across all panes
- The `create` command now displays which base branch was used
- Improved validation for pane configurations with helpful error messages

## v0.1.10 (2025-11-09)

- Post-create hooks now run before the tmux window opens, so the new window
  appears ready to use instead of showing setup commands running

## v0.1.9 (2025-11-09)

- Fixed cleanup when removing a worktree from within its own tmux window

## v0.1.7 (2025-11-09)

- Fixed a race condition where cleaning up a worktree could fail if the tmux
  window hadn't fully closed yet

## v0.1.6 (2025-11-09)

- Automatically run `pnpm install` when creating worktrees in pnpm projects

## v0.1.5 (2025-11-08)

- Fixed global config to always load from `~/.config/workmux/` instead of
  platform-specific locations (e.g., `~/Library/Application Support/` on macOS)

## v0.1.4 (2025-11-07)

- Added `--from` flag to `add` command to specify which branch, commit, or tag
  to branch from
- Fixed `rm` command failing when run from within the worktree being removed
- New worktree branches no longer track a remote upstream by default

## v0.1.3 (2025-11-06)

- Added global configuration support with XDG compliance—you can now set shared
  defaults in `~/.config/workmux/config.yaml` that apply across all projects
- Project configs can inherit from global settings using `<global>` placeholder
  in lists
- After merging or removing a worktree, automatically switches to the main
  branch tmux window if it exists
- Fixed an issue where removing a worktree could fail if the current directory
  was inside that worktree

## v0.1.2 (2025-11-05)

- Fixed `prune` command to correctly parse Claude Code's config file structure

## v0.1.1 (2025-11-05)

Initial release.

- Added `open` command to switch to an existing worktree's tmux window
- Added `--rebase` and `--squash` merge strategies to the `merge` command
- Added `claude prune` command to clean up stale worktree entries from Claude's
  config
- Added configurable window name prefix via `window_prefix` setting
- Allowed `remove` command to work without arguments to remove the current
  branch
- Shell completion now works with command aliases
- Fixed merge command not cleaning up worktrees after merging
- Fixed worktree deletion issues when running from within the worktree
- Fixed new branches being incorrectly flagged as unmerged
