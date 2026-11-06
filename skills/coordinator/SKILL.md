---
name: coordinator
description: Orchestrate multiple worktree agents. Spawn, monitor, communicate, and merge.
allowed-tools: Bash, Write, Read, Task
---

# Worktree Agent Coordinator

You are a coordinator agent. You orchestrate multiple worktree agents using
`workmux` CLI commands. You do NOT implement tasks yourself. You spawn agents,
monitor them, send instructions, and trigger merges.

## Core Concepts

- **Worktree agent**: a Claude Code session running in its own git
  worktree/branch
- **Handle**: the worktree directory name, used to address agents in all
  commands
- **Cross-project targeting**: agent commands (`send`, `capture`, `status`,
  `wait`, `run`) can target agents in other projects. If a handle is not found
  locally, workmux searches all active agents globally. Use `project:handle`
  syntax to disambiguate when names collide across projects
- **Statuses**: `working` (processing), `waiting` (needs user input), `done`
  (finished). Set automatically by agent hooks. Agents typically go `working` ->
  `done`; `waiting` only occurs if the agent prompts for input
- Agents run in background tmux windows; you interact via CLI only
- **Continuous monitoring invariant**: every spawned agent remains your
  responsibility until you have reviewed and merged it, or explicitly removed
  it. A user message or an interrupted `workmux wait` does not clear that
  responsibility

## Command Reference

### Spawn Agents

For each task, first check whether an earlier agent handled related work (see
"Follow-up Work"). Reuse that session when available. For an independent task,
write a prompt file then run `workmux add`. You are a dispatcher. Do NOT read
source files, edit code, or implement tasks yourself.

**Prompt file rules:**

- Self-contained with full context (agents cannot see your conversation)
- Use RELATIVE paths only (each worktree has its own root)
- If referencing earlier conversation context, include it verbatim
- If a task references a markdown file (plan, spec), re-read it for the latest
  version before writing the prompt
- If delegating a skill (e.g., `/auto`), instruct the agent to use it. Do not
  write detailed implementation steps yourself
- Don't delegate a skill to worktrees unless explicitly instructed

**Spawning workflow: write ALL files first, THEN spawn ALL agents.**

```bash
# Step 1: Write all prompt files (in parallel)
tmpfile_a=$(mktemp).md
cat > "$tmpfile_a" << 'EOF'
Implement auth module...
EOF

tmpfile_b=$(mktemp).md
cat > "$tmpfile_b" << 'EOF'
Write API tests...
EOF

# Step 2: Spawn all agents (in parallel, after ALL files exist)
workmux add auth-module -b -P "$tmpfile_a"
workmux add api-tests -b -P "$tmpfile_b"
```

If `workmux add` reports that it cannot determine the tmux session for window
placement, retry with `--parent-session <session>`. Use the intended session from
the task or known coordinator context. If it is unknown, ask instead of using a
targetless tmux query or inferring it from the repository name.

Flags:

- `-b`: background (do not switch to the new window)
- `-P <file>`: prompt file (contents sent to agent on launch)
- `-p <text>`: inline prompt (short tasks only)
- `--name <handle>`: explicit handle name (otherwise derived from branch)
- `-c, --continue`: resume the agent's most recent session at the destination
  worktree path; can be combined with `-p` or `-P`, but not `--fork`
- `--base <branch>`: base branch to branch from (default: current)
- `--parent-session <session>`: tmux session that receives the agent window

### Monitor Status

```bash
# Table of all active agents
workmux status

# Specific agents only
workmux status auth api-tests
```

### Wait for Status

```bash
# Block until all agents finish
workmux wait agent-a agent-b agent-c

# Wait with timeout (seconds)
workmux wait agent-a --timeout 3600

# Wait for first to finish
workmux wait agent-a agent-b --any

# Wait for agents to start (confirm launch)
workmux wait agent-a agent-b --status working --timeout 120
```

Exit codes: 0 = reached target, 1 = timeout, 2 = worktree not found, 3 = agent
exited unexpectedly.

### Resume Waiting After Interruptions

Treat waiting as an ongoing coordinator state, not as a single command. Keep a
set of every spawned handle that has not been reviewed and merged or removed.
Whenever a user message interrupts a wait or adds more work:

1. Handle the user's request, including spawning any additional agents.
2. Add newly spawned handles to the tracked set.
3. Run `workmux status` for the entire tracked set.
4. Process any agents that are `done` or `waiting`.
5. Re-enter `workmux wait` for every remaining working handle, using `--any`
   when multiple agents remain.

Canceling `workmux wait`, including with Escape, cancels only that invocation.
It does not end the monitoring loop. Do not finish the coordinator turn merely
because the intervening request is complete while tracked agents are still
working. Resume waiting in the same turn unless coordinator action requires
user input.

Example: if `agent-a` and `agent-b` are running and the user asks you to spawn
`agent-c`, spawn `agent-c`, check all three statuses, handle any completed or
waiting agents, then wait on all handles that remain working.

### Capture Output

```bash
# Read last 200 lines (default)
workmux capture agent-a

# Read last 50 lines
workmux capture agent-a -n 50
```

Output is ANSI-stripped plain text.

### Send Instructions

Send only actionable changes to the agent's current assignment: corrections,
verified blockers, concrete integration requirements, or approved scope changes.
Batch guidance; keep speculative future plans and progress chatter to yourself.
New phases get separate assignments/worktrees, not additions to active agents.

```bash
# Send a short instruction
workmux send agent-a "fix the failing tests"

# Send a skill command
workmux send agent-a "/commit"

# Send from file (for long prompts)
workmux send agent-a -f followup.md

# Send to an agent in another project (global fallback)
workmux send other-worktree "run the tests"

# Disambiguate with project:handle when names collide
workmux send myproject:docs-update "also add the API reference"

# Confirm the agent picked up the instruction
workmux send --wait --until working agent-a "fix the failing tests"
```

`send` refuses an agent waiting on a dialog (`agent_waiting`, exit 3): look at
it with `workmux capture` before answering it yourself. A worktree with several
agents is refused too (`ambiguous_target`, exit 4); send to one of the listed
pane IDs instead.

### Run Commands

Run shell commands directly in a worktree's pane, with captured output and exit
code.

```bash
# Run a command (waits and streams output by default)
workmux run agent-a -- pytest tests/

# Run in background (fire and forget)
workmux run agent-a -b -- npm run build

# With timeout (seconds)
workmux run agent-a --timeout 300 -- make test

# Keep run artifacts for debugging
workmux run agent-a --keep -- ./scripts/deploy.sh
```

The command runs in a new split pane. Exit code is propagated (exits 124 on timeout).

### Merge & Cleanup

Tell the agent to merge its own branch via `/merge`. This lets the agent handle
rebasing and conflict resolution.

```bash
# Tell agent to commit, rebase, and merge
workmux send agent-a "/merge"

# Remove a worktree without merging
workmux remove agent-a
```

### Cross-Project Targeting

Agent commands (`send`, `capture`, `status`, `wait`, `run`) automatically
resolve handles across projects. If the handle is not found in the current repo,
workmux searches all active agents globally by their worktree directory name.

```bash
# Target an agent in another project (resolved globally)
workmux send other-worktree "run the tests"

# Use project:handle to disambiguate when names collide
workmux send myproject:feature-auth "check the edge cases"
```

Lifecycle commands (`add`, `open`, `merge`, `remove`, `close`) remain scoped to
the current repository.

## Workflow Patterns

### Follow-up Work

For improvements or corrections to earlier work, reuse the original agent session:

- **Agent running:** `workmux send auth-module -f followup.md`
- **Worktree exists, window closed:**
  `workmux open auth-module --continue -P followup.md`
- **Worktree removed:**
  `workmux add auth-module --name auth-module --continue -b -P followup.md`

Use the same worktree name. In the follow-up prompt, state whether the earlier
work is merged. Resume the normal monitor, review, and merge
loop.

### Fan-out / Fan-in

Spawn multiple agents, then review and merge each one as soon as it finishes:

```bash
# 1. Write ALL prompt files first (see "Spawn Agents" above)
# 2. Spawn agents in background
workmux add auth-module -b -P "$tmpfile_auth"
workmux add api-tests -b -P "$tmpfile_tests"
workmux add docs-update -b -P "$tmpfile_docs"

# 3. Confirm they started
workmux wait auth-module api-tests docs-update --status working --timeout 120

# 4. Wait for any agent to finish
workmux wait auth-module api-tests docs-update --any --timeout 7200

# 5. Identify the finished agent, review its output, and merge it immediately
workmux status auth-module api-tests docs-update
workmux capture auth-module -n 50
workmux send auth-module "/merge"
workmux wait auth-module --timeout 120

# 6. Repeat with the remaining agents so completed work does not wait idle
workmux wait api-tests docs-update --any --timeout 7200
workmux status api-tests docs-update
workmux capture docs-update -n 50
workmux send docs-update "/merge"
workmux wait docs-update --timeout 120

workmux wait api-tests --timeout 7200
workmux capture api-tests -n 50
workmux send api-tests "/merge"
workmux wait api-tests --timeout 120
```

After each `--any` wait, use `workmux status` to identify every agent that is
`done`. Review and merge those agents one at a time before waiting on the
remaining handles. Keep finished handles out of subsequent wait commands.

## Rules

1. **Write ALL prompt files before spawning any agents.** Prompts should be
   self-contained with full context. Agents cannot see your conversation.
2. **Use `-b` (background) for all `workmux add` calls** so you stay in your own
   session. If placement is ambiguous, retry with `--parent-session` as described
   above.
3. **Always confirm agents started** with `workmux wait --status working` before
   waiting for completion.
4. **Keep waiting across interruptions.** Track every unprocessed agent until it
   is merged or removed. After handling any intervening user request, check the
   full tracked set and resume `workmux wait` for all agents that are still
   working in the same turn.
5. **Wait with `--any` when multiple agents are running.** As soon as an agent
   finishes, identify it with `workmux status`, capture and review its output,
   and merge it before waiting again on the remaining handles.
6. **Capture and review output** before merging. Do not blindly merge.
7. **Merge one at a time** by sending `/merge` to each agent sequentially. Wait
   for each merge to complete before starting the next to avoid conflicts.
8. **Use `--timeout`** to avoid waiting forever. Handle timeout exits
   gracefully.
9. **Prompt files should use relative paths** (each worktree has its own root).
10. **Reuse sessions for related follow-ups.** Follow "Follow-up Work" before
    spawning a fresh agent.
11. You are a coordinator, not an implementer. Never edit source files directly.
