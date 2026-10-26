---
title: "Setting up a server"
description: Prepare a Linux server, add it to your config and choose how its agents run
---

## Requirements

- Linux on x86-64 or ARM64.
- SSH that works without a prompt. workmux never asks for a password or to accept a host key, so use a key loaded in your SSH agent or one without a passphrase, and run `ssh <host>` once yourself. Aliases from `~/.ssh/config`, including `ProxyJump`, `Port` and `User`, work.
- Git and tmux 3.3 or newer.
- systemd linger for the SSH user, so agents survive the end of the SSH session:

  ```bash
  sudo loginctl enable-linger <user>
  ```

- Network access to the APIs your agents use.

The server doesn't need access to GitHub: the local machine downloads the workmux release, verifies it and uploads it. What else the server needs depends on [where agents run](#server-or-docker).

## Add the server to your config

```yaml
# ~/.config/workmux/config.yaml (global config only)
remotes:
  devbox: # the name you use on the command line
    host: devbox # an ~/.ssh/config alias or user@host
```

Names may contain letters, digits, `.`, `_` and `-`, and must start with a letter or digit. `local` is reserved.

`remotes` only works in your global config. A project's `.workmux.yaml` can't add a server, so a repository can't send your code anywhere; it can only choose among your servers with [`placement`](#choosing-where-new-agents-go). The other keys, `path`, `isolation` and `binary`, are in the [configuration reference](/guide/configuration/#remotes).

## Server or Docker

Remote agents are sandboxed exactly when the same agent would be sandboxed locally. `remotes.<name>.isolation` overrides that for one server, in either direction.

| `sandbox.enabled` | `remotes.<name>.isolation` | Agents run    |
| ----------------- | -------------------------- | ------------- |
| off (default)     | unset                      | on the server |
| on                | unset                      | in Docker     |
| any               | `none`                     | on the server |
| any               | `container`                | in Docker     |

The two modes need different things on the server and behave differently in a few places:

|                                           | On the server                                     | In Docker                                          |
| ----------------------------------------- | ------------------------------------------------- | -------------------------------------------------- |
| Agent CLIs                                | Install each one on the SSH user's login `PATH`   | Come from the sandbox image                        |
| Also needs                                | Your project's tools, if hooks or agents use them | Docker, usable without `sudo`; access to `ghcr.io` |
| Sandbox                                   | None, with the agent's normal permission prompts  | The [container sandbox](/guide/sandbox/container/) |
| `workmux rm` and `merge` in a remote pane | Ask the local machine to do it                    | Agents can ask the local machine, without hooks    |
| `sandbox.env` and `env_passthrough`       | Not used                                          | Used                                               |

workmux doesn't install agents on the server and doesn't fall back to Docker: if the agent's CLI is missing on the server, `add --remote` stops before creating anything. Podman isn't supported on servers.

:::danger[No sandbox on the server]
Agents running directly on the server have the SSH user's full access to it. Only use this mode on a machine you'd let the agent use freely.
:::

Run `remote setup`, `remote doctor` and `remote login` from the same project directory as `add`, because the project's `sandbox.enabled` decides the mode. Outside a project they use your global settings. `add` and `doctor` print the mode and the setting that chose it.

Each workspace keeps the mode it was created with, even if your settings change later.

## Install and check

```bash
workmux remote setup devbox
```

Installs the workmux that matches your local version on the server, plus workmux's [status hooks](/guide/status-tracking/) for the supported agents. It is safe to run again. `add --remote` and `remote login` install workmux on first use too, but running `setup` first shows problems early.

```bash
workmux remote doctor devbox
```

Checks SSH, the server's architecture, workmux, Git, tmux, linger, agent CLIs or Docker, status hooks and logins, and prints a fix for each failing check. In Docker mode it also pulls the sandbox image, so the first run can take a while. See [`remote doctor`](/reference/commands/remote/#remote-doctor) for every check.

```bash
workmux remote
```

Lists your servers and whether each one is reachable and set up.

Then [log your agents in](/guide/remote/logins/).

## Choosing where new agents go

Agents are created locally unless you ask otherwise:

- `workmux add <branch> --remote [<name>]` creates the agent on a server. The name is optional when `placement` names a server or only one is configured.
- `workmux add <branch> --local` creates it locally, overriding `placement`.
- `placement` sets the default: `local` (the default) or a server name.

```yaml
# ~/.config/workmux/config.yaml or .workmux.yaml
placement: devbox
```

A project's `placement` wins over the global one, so a project can send its agents to a server or keep them local. If `placement` names a server that isn't configured, `add` fails and suggests `--local`.

Where a workspace lives is decided when it is created. Later commands such as `open`, `merge` and `remove` find it by its handle. The dashboard creates local agents only, and refuses when `placement` names a server.

## What the server uses from your config

A remote workspace runs a copy of the local machine's effective config, taken when the workspace is created. The server's own workmux config and the worktree's `.workmux.yaml` are never read. `add --remote` lists every setting it didn't apply.

- **Applied:** `panes`, `agent`, `agents`, `post_create`, `pre_remove`, `files`, the status icons and format, and `hook_shell` unless it is an absolute local path.
- **Applied in Docker mode:** `sandbox.image`, `target`, `network`, `toolchain`, `env`, `env_passthrough`, and `container.cpus`, `memory`, `group_add`, `cap_add`, `security_opt` and `excluded_files`. The sandbox is always Docker there.
- **Not applied**, because they name local resources: Lima, other container runtimes, `rpc_host`, `extra_mounts`, `agent_config_dir`, `host_commands`, `container.devices` and `container.oci_runtime`.

`post_create` and `pre_remove` hooks run on the server, in the remote worktree. If `add --remote` fails partway, run it again with the same arguments: it resumes and doesn't repeat hooks that already finished.

`files.copy` and `files.symlink` sources are sent from the local machine, up to 64 MiB per workspace. A larger source is skipped with a warning. Symlink sources are sent once per project and then reused; they aren't kept in sync with the local machine.

## Security

Treat the server as an extension of your local machine: the SSH user runs every agent and holds their logins. Use a server or VM dedicated to agents, and treat what agents produce there as untrusted, as with a [local sandbox](/guide/sandbox/#security-model).

- **No host commands.** [Host command proxying](/guide/sandbox/features/#host-command-proxying) is off on servers. In Docker mode, agents can only run what their container has.
- **No Git credentials.** The server has no `origin`, and workmux connects without SSH agent or X11 forwarding. Anything else you give agents, such as tokens in `sandbox.env`, is up to you.
- **Local actions stay local.** A remote pane or agent can ask the local machine to remove or merge its own workspace, and nothing else; hook output from that work stays on the local machine. A request from a container runs no hooks. `workmux close` and image paste fail with a message saying what to run on the local machine.
- **Hooks run outside the container.** `post_create` and `pre_remove` run on the server as the SSH user, as they run on the local machine for local agents. They come from your local config.
- **Firewall the Docker bridge** if the server routes or forwards it to the outside. The sandbox's RPC bridge and proxy listen on the bridge address.

workmux marks remote worktrees as trusted in Claude and Codex, so they start without asking whether to trust the folder. Folders you marked untrusted stay untrusted. A trusted folder lets the agent load the repository's own agent settings, such as `.codex/config.toml`.
