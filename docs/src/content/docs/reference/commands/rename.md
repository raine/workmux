---
title: "rename"
description: Rename a worktree, its tmux window/session, and optionally the branch
---

Renames a worktree's directory, its tmux window or session, and the per-worktree
workmux metadata stored in git config. Optionally also renames the underlying
git branch.

```bash
workmux rename [old-name] <new-name> [--branch]
```

## Arguments

- `[old-name]`: Optional current worktree name (the directory name). Defaults to the current worktree when run from inside one.
- `<new-name>`: The new handle. This becomes the worktree directory name and the tmux window/session base name.

## Options

| Flag             | Description                                                                                               |
| ---------------- | --------------------------------------------------------------------------------------------------------- |
| `--branch`, `-b` | Also rename the underlying git branch to match `<new-name>`. Fails if the worktree is on a detached HEAD. |

## What gets renamed

1. The worktree directory (`git worktree move`)
2. The tmux window or session (matching duplicates like `wm-feature-2` are renamed preserving their `-N` suffix)
3. `workmux.worktree.<handle>.*` git config entries (e.g. the stored window/session mode)
4. Agent state files in `$XDG_STATE_HOME/workmux/agents/*.json` (updates `workdir`, `window_name`, `session_name`)
5. Sandbox container marker directory, if present
6. The local git branch, only when `--branch` is passed

## Examples

```bash
# Rename a worktree from inside it
workmux rename feature-new

# Rename a specific worktree by name
workmux rename feature-old feature-new

# Also rename the branch to match
workmux rename feature-old feature-new --branch
```

## Notes

- The main worktree cannot be renamed.
- Rename is non-destructive: uncommitted changes and untracked files survive.
- Submodules are not handled specially; `git worktree move` will error out if the worktree contains submodules.
- Any shell already running inside the old directory will have a stale `$PWD` after the rename. Run `cd` (or `cd <new-path>`) to refresh.
- Collisions are rejected up front: if the new path, tmux target, or branch already exists, the command aborts before making any changes.
