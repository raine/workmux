---
title: "resurrect"
description: Restore worktree windows after a tmux or computer crash
---

Restores worktree windows after a tmux or computer crash. Uses persisted agent state files to detect which worktrees had active agents before the crash, then reopens them with each agent's resume arguments.

```bash
workmux resurrect [--dry-run]
```

## Options

- `--dry-run`: Show what would be restored without actually doing it.

## How it works

1. Reads agent state files from `~/.local/state/workmux/agents/`
2. Filters to the current multiplexer backend and instance
3. Matches each state file's working directory to a git worktree in the current repo
4. Skips worktrees that are already open, no longer exist, or are the main worktree
5. Opens each matched worktree with the resume arguments for the remembered agent
6. Uses agent-specific behavior, such as Codex's resume subcommand with its last-session flag
7. Cleans up consumed stale state files

Agent state from known earlier multiplexer boots is compacted to one recovery
candidate per exact working directory. Unrestored worktrees remain recoverable
across repeated restarts without retaining duplicate records for every boot.
Current-boot records, records without a reliable boot identity, malformed files,
and state owned by another multiplexer instance or backend are preserved. Each
instance compacts its own history when workmux observes that instance with a
reliable boot identity.

## Remote workspaces

Agents on [remote hosts](/guide/sandbox/remote/) kept running while the local tmux was down, so `resurrect` reopens their windows and reattaches them without restarting anything. A remote agent whose session ended, for example because the remote host rebooted, is skipped: run `workmux open <handle>` to start it again.

## Examples

```bash
# Preview what would be restored
workmux resurrect --dry-run

# Restore all worktrees that had agents running
workmux resurrect
```

## Example output

```
  continue-flag        -> restoring
  dashboard-fix        -> skipping (already open)
  auth-refactor        -> restoring
  (2 state file(s) from other projects ignored)

✓ Restored 2 worktree(s): continue-flag, auth-refactor
```
