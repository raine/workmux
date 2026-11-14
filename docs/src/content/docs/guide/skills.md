---
title: "Skills"
description: Use skills to streamline workmux workflows
---

[Claude Code skills](https://code.claude.com/docs/en/skills) extend what Claude can do. Create a `SKILL.md` file with instructions, and Claude adds it to its toolkit. Claude uses skills when relevant, or you can invoke one directly with `/skill-name`.

:::tip
This documentation uses Claude Code's skill support as example, but other agents implement similar features. For example, [OpenCode skills](https://opencode.ai/docs/skills/). Adapt to your favorite agent as needed.
:::

## Using with workmux

Skills unlock the full potential of workmux. While you can run workmux commands directly, skills let agents handle the complete workflow - committing with context-aware messages, resolving conflicts intelligently, and delegating tasks to parallel worktrees.

- [**`/workmux`**](#workmux) - Teach the agent how to use workmux
- [**`/merge`**](#merge) - Commit, rebase, and merge the current branch
- [**`/rebase`**](#rebase) - Rebase with flexible target and smart conflict resolution
- [**`/worktree`**](#worktree) - Delegate tasks to parallel worktree agents
- [**`/coordinator`**](#coordinator) - Orchestrate multiple agents with full lifecycle control
- [**`/open-pr`**](#open-pr) - Write a PR description using conversation context

You can trigger `/merge` from the [dashboard](/guide/dashboard/configuration/) using the `m` keybinding:

```yaml
dashboard:
  merge: "/merge"
```

## Installation

Run `workmux setup` to install all skills automatically:

```bash
workmux setup --skills
```

This detects installed Claude Code, OpenCode, Pi, and Oh My Pi agents and copies skills to the right location. The setup wizard also offers skill installation on first run.

You can also copy skills manually from [`skills/`](https://github.com/raine/workmux/tree/main/skills) to your skills directory:

**Claude Code**: `~/.claude/skills/` (or project `.claude/skills/`). If `CLAUDE_CONFIG_DIR` is set, `workmux setup --skills` installs to `$CLAUDE_CONFIG_DIR/skills/` instead.

**OpenCode**: `~/.config/opencode/skills/`.

**Pi**: `~/.pi/agent/skills/`.

**Oh My Pi**: `~/.omp/agent/skills/`.

## `/workmux`

Teaches the agent how to use the workmux CLI. Invoke `/workmux` to give the agent knowledge of workmux commands, configuration, and concepts. The agent can then use workmux to manage worktrees, check status, and interact with other agents.

[**View skill ->**](https://github.com/raine/workmux/tree/main/skills/workmux/SKILL.md)

This is a reference skill, not an action skill. It loads workmux documentation into the agent's context so it can use workmux commands as needed. For specific workflows, the agent is directed to the dedicated skills below.

## `/merge`

Handles the complete merge workflow:

1. Commit staged changes using a specific commit style
2. Rebase onto the base branch with smart conflict resolution
3. Run `workmux merge` to merge, clean up, and send a notification when complete

[**View skill →**](https://github.com/raine/workmux/tree/main/skills/merge/SKILL.md)

Instead of just running `workmux merge`, this skill:

- Commits staged changes first - the agent has full context on the work done and can write a meaningful commit message
- Reviews base branch changes before resolving conflicts - the agent understands both sides and can merge intelligently
- Asks for guidance on complex conflicts

## `/rebase`

Rebases with flexible target selection and smart conflict resolution.

[**View skill →**](https://github.com/raine/workmux/tree/main/skills/rebase/SKILL.md)

Usage: `/rebase`, `/rebase origin`, `/rebase origin/develop`, `/rebase feature-branch`

See [Resolve merge conflicts with Claude Code](https://raine.dev/blog/resolve-conflicts-with-claude/) for more on this approach.

## `/worktree`

Delegates tasks to parallel worktree agents. A main agent on the main branch can act as a coordinator: planning work and delegating tasks to worktree agents.

[**View skill →**](https://github.com/raine/workmux/tree/main/skills/worktree/SKILL.md)

See the [blog post on delegating tasks](https://raine.dev/blog/git-worktrees-parallel-agents/) for a detailed walkthrough.

Usage:

```bash
> /worktree Implement user authentication
> /worktree Fix the race condition in handler.go
> /worktree Add dark mode, Implement caching  # multiple tasks
```

### Customization

You can customize the skill to add additional instructions for worktree agents. For example, to have agents review their changes with a subagent before finishing, or run `workmux merge` after completing their task.

## `/coordinator`

Orchestrates the full lifecycle of multiple worktree agents: spawning, monitoring, communicating, and merging. Unlike `/worktree` which dispatches tasks and returns, `/coordinator` turns the agent into a persistent orchestrator that manages agents through completion.

[**View skill ->**](https://github.com/raine/workmux/tree/main/skills/coordinator/SKILL.md)

The coordinator agent does not implement tasks itself. It writes prompt files, spawns worktree agents, monitors their status, sends follow-up instructions, and triggers merges.

### Key commands used

| Command                    | Purpose                                         |
| -------------------------- | ----------------------------------------------- |
| `workmux add -b -P <file>` | Spawn an agent in the background                |
| `workmux status`           | Check agent statuses                            |
| `workmux wait`             | Block until agents reach a target status        |
| `workmux capture`          | Read terminal output from an agent              |
| `workmux send`             | Send instructions or skill commands to an agent |
| `workmux run`              | Run shell commands in an agent's worktree       |

### Cross-project agent communication

Agent commands (`send`, `capture`, `status`, `wait`, `run`) can target agents in other projects, not just the current git repository. If a worktree name is not found locally, workmux searches all active agents globally.

```bash
# From any project, send to an agent in another project
workmux send other-project-worktree "run the tests"

# Use project:handle syntax to disambiguate when names collide
workmux send myproject:docs-update "also add the API reference"

# Check status of agents across projects
workmux status myproject:feature-auth
```

Lifecycle commands (`add`, `open`, `merge`, `remove`, `close`) remain scoped to the current repository.

### Fan-out / fan-in pattern

The typical coordinator workflow:

1. Write prompt files with full context for each task
2. Spawn all agents in the background
3. Confirm agents started with `workmux wait --status working`
4. Wait for completion with `workmux wait`
5. Review results with `workmux capture`
6. Merge one at a time by sending `/merge` to each agent sequentially

### When to use `/coordinator` vs `/worktree`

- **`/worktree`**: fire and forget. Spawn agents and return control to you. Good for delegating tasks you will review later yourself.
- **`/coordinator`**: full automation. The agent manages the entire lifecycle, including waiting, reviewing output, sending follow-ups, and merging. Good for multi-step plans where tasks depend on each other.

## `/open-pr`

Writes a PR description using the conversation context and opens the PR creation page in browser. This is the recommended way to finish work in repos that use pull requests.

[**View skill →**](https://github.com/raine/workmux/tree/main/skills/open-pr/SKILL.md)

The skill is opinionated: it opens the PR creation page in your browser rather than creating the PR directly. This lets you review and edit the description before submitting.

The agent knows what it built and why, so it can write a PR description that captures that context.
