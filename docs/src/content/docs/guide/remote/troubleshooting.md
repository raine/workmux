---
title: "Maintenance and troubleshooting"
description: Upgrade, clean up and retire remote servers, and fix common problems
---

## Upgrading workmux

After you upgrade workmux on the laptop, the next command that reaches a server installs the matching version there. Running agents keep the version they started with.

If you run workmux built from source, the server needs a Linux build of the same source: point [`remotes.<name>.binary`](/guide/configuration/#remotes) at it.

## Cleaning up a server

```bash
workmux remote gc devbox --dry-run   # list only
workmux remote gc devbox             # list, ask, remove
workmux remote gc devbox --force     # remove without asking, including unsaved work
```

`remote gc` lists what is on the server that no workspace on this laptop accounts for, and removes it after you confirm: workspaces the laptop has no record of (for example after `remove --force` while the server was unreachable), leftover worktrees, sessions and containers, and workmux versions no agent uses. It also forgets laptop records of workspaces the server no longer has.

A workspace with uncommitted changes or commits beyond its base is listed, but only removed with `--force`. Workspaces in the middle of starting, a merge or a pull are left alone. Without a terminal, nothing is removed unless you pass `--force`.

:::caution[One laptop per `path`]
`gc` treats every workspace this laptop doesn't know as unused. If two machines use the same server, give each its own [`path`](/guide/configuration/#remotes).
:::

## Retiring a server

1. Merge, pull or remove the workspaces you want to keep.
2. Run `workmux remote gc devbox --force`. This removes work that exists only on the server.
3. Delete the server's `path` directory, `~/.workmux-node` by default. workmux has no uninstall command.
4. Remove the server from `remotes`, and from any `placement` that names it.

If the server is already gone, `workmux remove --force <handle>` forgets its workspaces on the laptop.

## Common problems

**`ssh` fails or asks for a password.** Run `ssh <host>` yourself first. workmux never prompts, so the key must be in your SSH agent or have no passphrase, and the host key must already be in `known_hosts`. `remote doctor` names the failing step: authentication, host key, DNS, refused connection or timeout.

**Docker permission denied after adding the user to the `docker` group.** workmux reuses an SSH connection for up to ten minutes, which keeps the old group membership. Doctor's fix includes the command that closes the connection.

**Agents stop when you log out of the server.** Enable linger: `sudo loginctl enable-linger <user>`.

**An agent asks you to log in.** Run `workmux remote login <remote> <agent>`. `remote doctor` shows which agents have credentials on the server.

**A login fails because the agent is missing.** On the server directly, install the agent there. In Docker mode with a `sandbox.image` override, the image must contain the agent.

**No status icons for a remote agent.** Run `workmux remote setup <remote>` again and check doctor's `hooks` row. For agents on the server directly, OpenCode's hooks are installed once its CLI is on the login `PATH`. In Docker mode, they need the OpenCode sandbox image on the server, so run `setup` again after its first agent or login there.

**Codex asks to review hooks on the server.** Codex outside a sandbox asks once to trust workmux's status hooks. Attach to the agent and trust them; Codex asks again only when the hooks change.

**`workmux: command not found` in a remote pane.** Expected in Docker mode: run the command on the laptop. On the server directly, `workmux` is on every pane's `PATH`.

**`rm` or `merge` in a remote pane says the request was withdrawn.** The laptop isn't connected, or runs an older workmux. Run the command on the laptop instead.

**A merge requested from a remote pane failed.** The pane prints a `workmux remote logs` command. Run it on the laptop to see the full output.

**Protocol mismatch.** The server's workmux doesn't match your local one. This happens with builds from source; point `binary` at a Linux build of the same source.

**Something is left over on the server.** Run `workmux remote gc <remote> --dry-run`.

## Known limitations

- Remote agents need tmux in window mode. Session mode, `windows:` configs and other multiplexers aren't supported for them.
- Servers run Linux and use Docker for sandboxing; Podman, Lima and Apple Container aren't available there.
- The dashboard can't create remote agents; use `workmux add --remote`.
- Agents on a server can't push, read the laptop's clipboard or run host commands.
- `rm` and `merge` from a remote pane work only for agents running directly on the server.
- Ignored files, submodule contents and agent conversations don't come home with `merge` or `remote pull`.
- The environment snapshot is taken once per workspace.
- Imported credentials can drift apart from the laptop's copy. `omp` logins can't be imported, and doctor can't tell whether `omp` is logged in.
- The dashboard's diff view of a remote agent is fetched from the server and cached; patch mode is local-only.
- Operation logs aren't deleted automatically.
- A server's `path` is meant for one laptop.
