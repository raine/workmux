---
description: Recommended patterns for starting worktrees and delegating tasks to agents
---

# Workflows

Common patterns for working with workmux and AI agents.

## Starting work

### From the terminal

When starting a new task from scratch, use `workmux add -A` (`--auto-name`):

```bash
workmux add -A
```

This opens your `$EDITOR` where you describe the task. After saving, workmux generates a branch name from your prompt and creates the worktree with the prompt passed to the agent.

It's essentially a streamlined version of `workmux add <branch-name>`, then waiting for the agent to start, then typing the prompt. But you write the prompt first and skip thinking of a branch name.

::: tip
The `-A` flag requires the [`llm`](https://llm.datasette.io/) CLI tool to be installed and configured. See [Automatic branch name generation](/reference/commands/add#automatic-branch-name-generation) for setup.

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

When you're already working with an agent and want to spin off a task into a separate worktree, use the [`/worktree` skill](/guide/skills#-worktree). The agent has context on what you've discussed, so it can write a detailed prompt for the new worktree agent.

```
> /worktree Implement the caching layer we discussed
```

The main agent writes a prompt file with all the relevant context and runs `workmux add` to create the worktree. This is useful when:

- The agent already understands the task from your conversation
- You want to parallelize work while continuing in the main window
- You're delegating multiple related tasks from a plan

This pattern naturally leads to a **coordinator agent** workflow: an agent on the main branch that plans work and delegates tasks to worktree agents via `/worktree`. The coordinator stays on main and doesn't write code itself; it breaks down a larger goal into parallel tasks and spins up worktree agents to handle each one.

See [Skills](/guide/skills#-worktree) for the skill setup.

### Coordinating multiple agents

For multi-step plans where you want the agent to manage the full lifecycle (spawning, monitoring, and merging), use the [`/coordinator` skill](/guide/skills#-coordinator).

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

See [Skills](/guide/skills#-coordinator) for more details on the coordinator pattern.

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

See [Skills](/guide/skills) for the skill setup.

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

> For jj users, push with `jj git push` instead.


Once your PR is merged on GitHub, use `workmux remove` to clean up:

```bash
# Remove a specific worktree
workmux remove feature-123

# Or clean up all worktrees whose remote branches were deleted
workmux rm --gone
```

The `--gone` flag is particularly useful - it automatically finds worktrees whose upstream branches no longer exist (because the PR was merged and the branch was deleted on GitHub) and removes them. For jj repos, workmux detects gone branches via `jj git fetch` and bookmark tracking.
