---
title: "Working with remote agents"
description: Day-to-day commands, remote panes, disconnects and reboots for agents on a remote server
---

## Commands on remote workspaces

Remote workspaces use the usual commands. Run them from the project on the local machine and name the workspace by its handle.

| Command                                                                        | On a remote workspace                                                                                       |
| ------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------- |
| [`close`](/reference/commands/close/)                                          | Closes the local window only. The agent keeps running.                                                      |
| [`open`](/reference/commands/open/)                                            | Reattaches to the running agent, or starts the panes again if they ended.                                   |
| [`resurrect`](/reference/commands/resurrect/)                                  | Reopens windows of agents still running after the local machine's tmux restarted, without restarting them.  |
| `status`, `wait`, `send`, `capture`                                            | Work whether or not the window is open. Agents are labelled `handle@remote`.                                |
| [`list`](/reference/commands/list/)                                            | Shows remote workspaces with their server.                                                                  |
| [`remove`](/reference/commands/remove/)                                        | Stops the agent and removes the remote worktree, branch and window, with the same checks as a local remove. |
| [`merge`](/guide/remote/git/#merging), [`rebase`](/guide/remote/git/#rebasing) | See [bringing work home](/guide/remote/git/).                                                               |
| `rename`, `path`, `run`, `sync-files`                                          | Not supported.                                                                                              |

`remove --all` and `remove --gone` only remove local worktrees. A remote branch such as `origin/feature` can't be the new workspace's branch: fetch it and pass it with `--base`. Some other `add` options aren't available either; see [`add`](/reference/commands/add/#remote-workspaces).

## Sending prompts

`workmux send` and `wait` work as they do locally. An agent running directly on the server can only receive `send` once it has reported its first status, so give the first prompt with `add -p` or type it into the window. In Docker mode, `send` works as soon as the agent's container is running.

## Panes on the server

Every pane in the window is a session on the server. Non-agent panes are shells on the server, or in the container with `sandbox.target: all`.

- **On the server directly:** `workmux` works in every pane, including the shell left after the agent exits. It is the server's copy, which knows the pane's workspace.
- **In Docker:** shell panes on the server have no `workmux`. Run workmux commands for the workspace on the local machine. The agent's container has `workmux`, for [`merge` and `rm`](#remove-or-merge-from-a-remote-pane) and [starting agents](#agents-that-start-agents).

## Remove or merge from a remote pane

In a workspace running directly on the server, type the usual commands in any of its panes. An agent can run them too, including in Docker:

```bash
workmux merge
workmux rm
```

The local machine does the work, exactly as if you had run `workmux merge my-feature` or `workmux rm my-feature` there: it uses the local machine's config, hooks and base branch, and closes the window when it is done. Meanwhile the pane shows the local machine's progress:

```
Merging 'my-feature' on the local machine…
Started on the local machine; follow with `workmux remote logs devbox r0123456789ab`
Bringing the work to the local machine… (1s)
Preparing a local copy for the merge (files, post_create)…
Running pre_merge hooks locally… (42s)
```

The [log](/guide/remote/git/#operation-logs) holds the full hook output.

- The local machine must be awake and connected. If it doesn't pick the request up within about 15 seconds, the request is withdrawn and the command fails with "the local machine isn't connected".
- Only the workspace's own handle works: no name, `.`, or the handle itself.
- Nothing can ask questions, so these are refused with a hint: removing a workspace with unmerged commits without `--force`, merging staged changes that need a commit message (commit them first), squash merges, `rm --all`, `--gone` and `--keep-branch`. `close` is refused too, because the window is on the local machine.
- If the merge fails, the workspace stays and the pane prints why.
- In Docker mode, these requests run no hooks: no `post_create`, `pre_merge` or `pre_remove`, and no Git hooks, as for a [local sandbox](/guide/sandbox/#security-model). Hooks would run outside the container on code the agent wrote. Run `workmux merge <handle>` on the local machine when you want them.

An agent can finish its own task this way, for example with `-p "… when the tests pass, run: workmux merge"` or the bundled [`/merge` skill](/guide/skills/).

## When the local machine sleeps or disconnects

The agent, its container and its tmux session run on the server, so they don't depend on the local machine.

- The agent keeps working, and its status is recorded on the server.
- The window shows a status line with the server's name and the reason, reconnects by itself, and offers keys to retry now or close the pane.
- Keystrokes typed while disconnected are dropped, never delivered late.
- The dashboard, sidebar and `workmux status` keep showing the agent's last known state, marked disconnected with when it was last seen.

Short interruptions pass without any visible change. After an authentication or host key failure, or when the session was attached from somewhere else, the window doesn't retry by itself: fix the cause and press the retry key.

## Agents that start agents

An agent on a server can start a sibling the same way a local agent does (see [spawning agents](/guide/workflows/#from-an-ongoing-agent-session)). The sibling is created on the same server, even while the local machine is away:

```bash
# run by the agent on the server
workmux add parser-tests -b -P task.md
```

- The agent must name a new branch. Generated names and existing branches are refused.
- The sibling starts from the tip of the agent's branch, with the same mode, config, files and environment. `post_create` hooks don't run.
- Its window opens next to the parent's the next time the local machine connects. If it doesn't, run `workmux list` from the project's repository, or `workmux open <branch>`.

## When the server reboots

A reboot stops the agents and containers, but worktrees, branches and records survive. The dashboard, `status` and `list` show the agent as exited because the server rebooted. To start it again:

```bash
workmux open my-feature
```

The agent starts with its continue flag, so agents that keep their conversations on disk pick up where they left off. `open` says whether it continued. The original prompt isn't sent again, and nothing restarts automatically at boot.
