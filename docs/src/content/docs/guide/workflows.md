---
title: "Workflows"
description: Recommended patterns for starting worktrees and delegating tasks to agents
---

Common patterns for working with workmux and AI agents.

## Starting work

### From the terminal

When starting a new task from scratch, use `workmux add -A` (`--auto-name`):

```bash
workmux add -A
```

This opens your `$EDITOR` where you describe the task. After saving, workmux generates a branch name from your prompt and creates the worktree with the prompt passed to the agent.

It's essentially a streamlined version of `workmux add <branch-name>`, then waiting for the agent to start, then typing the prompt. But you write the prompt first and skip thinking of a branch name.

:::tip
The `-A` flag requires the [`llm`](https://llm.datasette.io/) CLI tool to be installed and configured. See [Automatic branch name generation](/reference/commands/add/#automatic-branch-name-generation) for setup.

Combine with `-b` (`--background`) to launch the worktree without switching to it.
:::

You can also pass the prompt inline or from a file:

```bash
# Inline prompt
workmux add -A -p "Add pagination to the /users endpoint"

# From a file
workmux add -A -P task-spec.md
```

### From an ongoing agent session

When you're already working with an agent and want to spin off a task into a separate worktree, use the [`/worktree` skill](/guide/skills/#worktree). The agent has context on what you've discussed, so it can write a detailed prompt for the new worktree agent.

```
> /worktree Implement the caching layer we discussed
```

The main agent writes a prompt file with all the relevant context and runs `workmux add` to create the worktree. This is useful when:

- The agent already understands the task from your conversation
- You want to parallelize work while continuing in the main window
- You're delegating multiple related tasks from a plan

Add `--fork` to pass the current conversation to the new worktree agent, so it can resume with full context instead of starting from a written prompt alone:

```
> /worktree --fork Implement the caching layer we discussed
```

This pattern naturally leads to a **coordinator agent** workflow: an agent on the main branch that plans work and delegates tasks to worktree agents via `/worktree`. The coordinator stays on main and doesn't write code itself; it breaks down a larger goal into parallel tasks and spins up worktree agents to handle each one.

See [Skills](/guide/skills/#worktree) for the skill setup.

### Forking conversations

When you want a new worktree agent to pick up where the current conversation left off, use `--fork` (Claude Code, Codex, and Pi):

```bash
workmux add -A --fork
```

This takes the most recent conversation from the current worktree and launches the agent in the new worktree resuming it, so it has full context of what was discussed. Useful when:

- You want to branch off a conversation to explore an alternative approach
- The current agent has built up context you want to preserve in a new worktree
- You're splitting a large task and want each worktree to start with shared context

To fork a specific session (not the most recent), use `--fork=<session-id>` with the session UUID or a prefix:

```bash
workmux add my-branch --fork=abc123
```

Supports Claude Code, Codex, and Pi conversations. The parent conversation is left as it was, so forking never disturbs the worktree you started from.

Claude Code has no cross-project resume, so workmux copies the conversation files into the new worktree's project directory and launches `claude --resume <session-id>`. Codex forks natively, so workmux resolves which session to fork and launches `codex fork -C <worktree> <session-id>`, which keeps the forked conversation in the new worktree. Pi also forks natively: workmux launches `pi --fork <session-path>` in the new worktree. Pi must support the `--fork` CLI option.

Pi session discovery uses `~/.pi/agent/sessions` (or `$PI_CODING_AGENT_DIR/sessions`) and matches the session’s recorded working directory to the source worktree root. For a custom session store, export `PI_CODING_AGENT_SESSION_DIR` when running workmux; Pi’s `--session-dir` flag and `sessionDir` setting are not read by workmux.

### Coordinating multiple agents

For multi-step plans where you want the agent to manage the full lifecycle (spawning, monitoring, and merging), use the [`/coordinator` skill](/guide/skills/#coordinator).

```
> /coordinator Break down the auth refactor into parallel tasks:
  1. Extract session logic into its own module
  2. Add OAuth provider support
  3. Write integration tests for the new auth flow
```

The coordinator agent writes prompt files for each task, spawns worktree agents in the background, waits for them to finish, reviews their output, and merges results sequentially. You stay hands-off while it runs.

This is useful when:

- You have a plan with multiple independent tasks
- Tasks should be merged in a specific order
- You want the agent to send follow-up instructions based on results
- You want full automation without checking in on each agent manually

See [Skills](/guide/skills/#coordinator) for more details on the coordinator pattern.

## Finishing work

How you finish depends on whether you merge locally or use pull requests.

### Direct merge

When you want to merge directly without a pull request, use `/merge` to commit, rebase, and merge in one step:

```
> /merge
```

This slash command handles the full workflow: committing staged changes, rebasing onto main, resolving conflicts if needed, and running `workmux merge` to clean up.

If you need to sync with main before you're ready to merge (e.g., to pick up changes from other merged branches), use `/rebase`:

```
> /rebase
```

The CLI equivalent is `workmux rebase`, which rebases the current or named worktree branch onto its saved base branch (or main when no local base exists).

See [Skills](/guide/skills/) for the skill setup.

### PR-based

If your team uses pull requests for code review, the merge happens on the remote after review. Push your branch and clean up after the PR is merged.

After committing your changes, push and create a PR. If you're working with an agent, consider using a slash command like `/open-pr` that can write the PR description using the conversation context:

```
> /open-pr
```

See [`skills/open-pr`](https://github.com/raine/workmux/tree/main/skills/open-pr/SKILL.md) for an example skill you can adapt.

Or manually:

```bash
git push -u origin feature-123
gh pr create
```

Once your PR is merged on GitHub, use `workmux remove` to clean up:

```bash
# Remove a specific worktree
workmux remove feature-123

# Or clean up all worktrees whose remote branches were deleted
workmux rm --gone
```

The `--gone` flag is particularly useful - it automatically finds worktrees whose upstream branches no longer exist (because the PR was merged and the branch was deleted on GitHub) and removes them.
