---
title: "sync-files"
description: Re-apply file operations (copy/symlink) to existing worktrees
---

Re-applies file operations (copy and symlink from the `files` config) to existing worktrees. Useful when you add new entries to the config or a symlink was accidentally deleted.

Unlike `open --force-files`, this command does not require tmux and works standalone from inside any worktree.

```bash
workmux sync-files [--all]
```

## Options

| Flag    | Description                                         |
| ------- | --------------------------------------------------- |
| `--all` | Sync all worktrees instead of just the current one. |

## Examples

```bash
# Sync files to the current worktree
workmux sync-files

# Sync files to all worktrees at once
workmux sync-files --all
```
