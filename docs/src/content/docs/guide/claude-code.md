---
title: "Claude Code"
description: Configure Claude Code permissions and settings for use with workmux worktrees
---

## Permissions

By default, Claude Code prompts for permission before running commands. There are several ways to handle this in worktrees:

### Share permissions across worktrees

To keep permission prompts but share granted permissions across worktrees:

```yaml
files:
  symlink:
    - .claude/settings.local.json
```

Add this to your global config (`~/.config/workmux/config.yaml`) or project's `.workmux.yaml`. Since this file contains user-specific permissions, also add it to `.gitignore`:

```
.claude/settings.local.json
```

### Skip permission prompts (yolo mode)

To skip prompts entirely, define a named agent with the flag:

```yaml
# ~/.config/workmux/config.yaml
agents:
  claude: "claude --dangerously-skip-permissions"
```

This shadows the built-in `claude` name so all workmux-created worktrees use the flag automatically, without affecting `claude` outside of workmux.

You can also use a separate name and reference it per-project:

```yaml
# ~/.config/workmux/config.yaml
agents:
  cc-yolo: "claude --dangerously-skip-permissions"

# .workmux.yaml (in projects that need it)
agent: cc-yolo
```

## Continuing a conversation in a worktree

Sometimes you want to continue a Claude conversation inside a worktree. Since worktrees are technically separate project directories, Claude Code treats them as different projects, so you cannot directly resume a conversation that started in another worktree (or the main tree).

[claude-history](https://github.com/raine/claude-history) solves this with its cross-project fork feature. Use `--global` mode to search conversations across all projects, then fork the one you want:

```sh
claude-history --global
```

Select a conversation and press `Ctrl+F` to fork it. When the conversation belongs to a different project than your current directory, claude-history automatically copies the session files into the current project and resumes there. The forked conversation then lives in the worktree as if it started there.

## Multiple Claude configurations (work/personal)

If you use separate Claude configurations for work and personal projects, define [named agents](/guide/agents/#named-agents) in your global config:

```yaml
# ~/.config/workmux/config.yaml
agents:
  cc-work: "env CLAUDE_CONFIG_DIR=~/.claude-work claude"
  cc-personal: "env CLAUDE_CONFIG_DIR=~/.claude-personal claude"
```

Then set the agent per project:

```yaml
# work project .workmux.yaml
agent: cc-work
```

Or use it directly: `workmux add feature -a cc-work`.

### Alternative: direnv

You can also use [`CLAUDE_CONFIG_DIR`](https://code.claude.com/docs/en/env-vars) with [direnv](https://direnv.net/) to switch configurations per directory. This affects `claude` everywhere, not just workmux:

```bash
# .envrc
export CLAUDE_CONFIG_DIR=~/.claude-work
```
