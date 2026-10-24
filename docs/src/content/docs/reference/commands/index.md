---
title: "CLI reference"
description: Complete reference for all workmux commands
---

## Commands overview

| Command                                           | Description                                         |
| ------------------------------------------------- | --------------------------------------------------- |
| [`add`](/reference/commands/add/)                 | Create a new worktree and tmux window               |
| [`merge`](/reference/commands/merge/)             | Merge a branch and clean up everything              |
| [`rebase`](/reference/commands/rebase/)           | Rebase a worktree branch onto its base branch       |
| [`remove`](/reference/commands/remove/)           | Remove worktrees without merging                    |
| [`rename`](/reference/commands/rename/)           | Rename a worktree, its tmux window, and branch      |
| [`list`](/reference/commands/list/)               | List all worktrees with status                      |
| [`status`](/reference/commands/status/)           | Query tracked agent status                          |
| [`open`](/reference/commands/open/)               | Open a tmux window for an existing worktree         |
| [`close`](/reference/commands/close/)             | Close a worktree's tmux window (keeps worktree)     |
| [`resurrect`](/reference/commands/resurrect/)     | Restore worktree windows after a crash              |
| [`sync-files`](/reference/commands/sync-files/)   | Re-apply file operations to existing worktrees      |
| [`path`](/reference/commands/path/)               | Get the filesystem path of a worktree               |
| [`dashboard`](/reference/commands/dashboard/)     | TUI dashboard for monitoring agents                 |
| [`sidebar`](/reference/commands/sidebar/)         | Live agent status sidebar in tmux                   |
| [`reap-agents`](/reference/commands/reap-agents/) | Exit tracked agent processes older than a threshold |
| [`config edit`](/reference/commands/config/)      | Edit the global configuration file                  |
| [`init`](/reference/commands/init/)               | Generate configuration file                         |
| [`claude prune`](/reference/commands/claude/)     | Clean up stale Claude Code entries                  |
| [`remote`](/reference/commands/remote/)           | Set up and manage remote hosts that run agents      |
| [`completions`](/reference/commands/completions/) | Generate shell completions                          |
| [`docs`](/reference/commands/docs/)               | Show detailed documentation                         |
| [`update`](/reference/commands/update/)           | Update workmux to the latest version                |
| [`last-done`](/reference/commands/last-done/)     | Switch to the most recently completed agent         |
