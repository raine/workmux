---
title: "claude prune"
description: Remove stale Claude Code entries pointing to deleted worktrees
---

Removes stale entries from Claude config (`~/.claude.json`) that point to
deleted worktree directories. When you run Claude Code in worktrees, it stores
per-worktree settings in that file. Over time, as worktrees are merged or
deleted, it can accumulate entries for paths that no longer exist.

```bash
workmux claude prune
```

## What happens

1. Scans `~/.claude.json` for entries pointing to non-existent directories
2. Creates a backup at `~/.claude.json.bak` before making changes
3. Removes all stale entries
4. Reports the number of entries cleaned up

## Safety

- Only removes entries for absolute paths that don't exist
- Creates a backup before modifying the file
- Preserves all valid entries and relative paths

## Examples

```bash
# Clean up stale Claude Code entries
workmux claude prune
```

## Example output

```
  - Removing: /Users/user/project__worktrees/old-feature

✓ Created backup at ~/.claude.json.bak
✓ Removed 3 stale entries from ~/.claude.json
```
