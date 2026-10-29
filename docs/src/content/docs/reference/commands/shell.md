---
title: "shell"
description: Open a shell in a remote workspace from the local terminal
---

Opens an interactive shell in a workspace on a [remote server](/guide/remote/) and runs it in the current terminal. It starts in the workspace's working directory, and you don't need the server's SSH address or the worktree path.

```bash
workmux shell [name] [--host]
```

## Arguments

- `[name]`: Remote workspace handle or branch, or `project:handle`, as for [`send`](/guide/remote/usage/#commands-on-remote-workspaces). When omitted, it uses the remote workspace whose window the command runs in.

## Options

| Flag     | Description                                                                           |
| -------- | ------------------------------------------------------------------------------------- |
| `--host` | In a workspace whose agent runs in Docker, run the shell on the server, not in Docker |

## Where the shell runs

- **Agent on the server directly** (`isolation: none`): the shell runs on the server, with the server's `workmux` and the workspace's environment, like the window's other panes.
- **Agent in Docker:** the shell runs in a new container of the workspace, with the same image, mounts and environment as the agent's. This does not depend on `sandbox.target`. `--host` runs the shell on the server instead. It is the only way to get a server shell, and that shell has no `workmux`.

The first line in the shell says which of these you got.

## Opening it next to the agent

A tmux split of a remote workspace's window starts a shell on the local machine. Run `workmux shell` in that split to replace it with a shell on the server:

```bash
# in a new split of the remote window
workmux shell
```

You can also bind a key to open a split running the remote shell. Add a binding like this to your tmux config:

```bash
bind S split-window -h "workmux shell"
```

The split closes when the shell exits.

## Ending the shell

- Exiting the shell ends it on the server, and `workmux shell` returns. The agent and its workspace keep running.
- When the connection drops, the pane shows a status line and reconnects to the same shell, as the window's other panes do.
- Closing the pane, or pressing `q` while it is disconnected, also ends the shell on the server. If the server can't be reached at that moment, the shell keeps running there until the workspace's agent is restarted or the workspace is removed.
- Commands that stop the agent, such as `remove` and `merge`, end its shells too. So does `workmux open` when it restarts the agent after a server reboot.

## Errors

- A local worktree name is refused. Use `cd "$(workmux path <name>)"` for local worktrees.
- If the server can't be reached, `shell` fails at once and names the server.
- A workspace that never started on the server is refused with a hint to run `workmux open <name>`.
