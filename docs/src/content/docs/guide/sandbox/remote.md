---
title: "Remote hosts"
description: Run individual agents on an SSH-reachable Linux host while others stay local
---

workmux can run an agent on a Linux machine you reach over SSH, such as a home server, a Proxmox VM or a cloud VM, while your other agents stay on the laptop. Each remote agent still gets a normal tmux window on the laptop, and shows up in the dashboard, the sidebar, `workmux status` and `workmux list` like a local one.

The agent itself runs on the remote host, with the same sandbox setting as a local agent. It keeps running while the laptop sleeps, loses its network or closes the window. When you come back, the window reconnects by itself. When the work is done, `workmux merge` brings it home and merges it on the laptop.

Use a remote host when:

- agents need more CPU, memory or disk than the laptop should give them
- agents should keep working while the laptop is closed
- you want to run more agents in parallel than the laptop handles comfortably

## How it works

workmux installs its own binary on the remote host and drives it over SSH. That remote side is called the **node**, and everything it owns lives under one directory, the **node root**.

- `workmux add --remote` pushes the base commit from the laptop to a repository on the node, creates the worktree there, and starts every configured pane as a session of the node's own tmux server. Unpushed local commits are included. The node never fetches from your Git hosting.
- In Docker mode, the agent pane runs the same sandbox supervisor as a local sandboxed agent: the container, the RPC bridge and, if configured, the network proxy all run on the node.
- The laptop window has one pane per remote pane. Each one relays its remote session over SSH, so the agent outlives the relay.
- One SSH connection per remote streams agent status, titles and Git status to the laptop.

## Where remote agents run

Remote isolation follows the effective `sandbox.enabled`: the project's setting overrides the global setting, and the default is off. An explicit `remotes.<name>.isolation` overrides that rule in either direction.

| `sandbox.enabled` | `remotes.<name>.isolation` | Result       |
| ----------------- | -------------------------- | ------------ |
| off               | unset                      | runs on host |
| on                | unset                      | Docker       |
| any               | `none`                     | runs on host |
| any               | `container`                | Docker       |

**On host:** install each agent CLI on the SSH user's login `PATH`. workmux does not install agents or fall back to Docker. A missing agent command stops `add --remote` before creating workspace state, and stops `remote login` before starting login. No `sandbox.*` settings are sent or reported as unapplied in this mode.

**In Docker:** install Docker and allow the SSH user to use it without `sudo`. Agent CLIs come from the sandbox image; they do not need to be installed on the host.

Run `remote setup`, `remote doctor`, and `remote login` from the same project directory as `add` to use the same effective settings. Outside a project, they use global settings. The add summary and doctor show both the mode and the setting that decided it.

Each workspace keeps the isolation recorded when it was created, even if settings change later. Sibling workspaces spawned by a remote agent inherit its isolation. Logins use the same remote agent directories in both modes.

## Host requirements

- Linux on x86-64 or ARM64.
- SSH access that works without a prompt, because workmux connects with `BatchMode=yes`. Use a key loaded in your SSH agent or an unencrypted key, and accept the host key once by running `ssh <host>` yourself. Aliases from `~/.ssh/config`, including `ProxyJump`, `Port` and `User`, are honoured.
- Git and tmux 3.3 or newer.
- systemd linger enabled for the SSH user (`sudo loginctl enable-linger <user>`), so agents survive the end of the SSH session.
- For Docker isolation: Docker, usable by the SSH user without `sudo` (membership of the `docker` group). Podman is not supported on remote hosts.
- Network access to the APIs your agents use, and in Docker mode to the container registry (`ghcr.io`).

The remote host does not need access to GitHub: the laptop downloads the workmux release, verifies its checksum and uploads it.

## Setup

### 1. Configure the remote

Add the remote to your **global** config:

```yaml
# ~/.config/workmux/config.yaml (global config only)
remotes:
  devbox: # remote name used on the command line
    host: devbox # ssh destination: an alias from ~/.ssh/config or user@host
```

The other keys (`path`, `isolation` and `binary`) and their defaults are listed under [remotes](/guide/configuration/#remotes) in the configuration reference. See [where remote agents run](#where-remote-agents-run) to choose host execution or Docker.

Remote names may contain letters, digits, `.`, `_` and `-`, and must start with a letter or digit. `local` is reserved.

`remotes` is global-only: a `remotes` block in a project's `.workmux.yaml` is ignored with a warning, so a repository cannot point workmux at a host. See [placement](#choosing-where-agents-run) for what a project can set.

### 2. Install the node

```bash
workmux remote setup devbox
```

This uploads the workmux binary that matches your local version into the node root and installs workmux's [status hooks](/guide/status-tracking/) for the supported agents in the remote user's agent directories. OpenCode's hook layout depends on its version. In host mode, its hooks are skipped until its CLI is installed on the login `PATH`; an installed CLI that fails is an error. In Docker mode, its hooks need the OpenCode sandbox image on the remote, so run `setup` again after its first agent or login there. Running `setup` again is safe: it only installs what is missing. Commands that need the node, such as `add --remote` and `remote login`, also install it on first use, but running `setup` first shows any problem early.

### 3. Check the host

```bash
workmux remote doctor devbox
```

Doctor checks SSH, architecture, the installed node, Git, tmux, linger, status hooks and logins. In host mode it checks agent CLIs on the login `PATH`: a missing default agent (`agent:`) fails, while missing other configured agents produce one warning. In Docker mode it checks Docker and pulls the sandbox image, so the first run can take a while; an image that cannot be pulled passes when it was built on the remote. Doctor prints the resolved mode and a fix for each failing row, and exits non-zero when any check fails.

```bash
workmux remote
```

lists every configured remote with its host and whether it is reachable and set up.

### 4. Log your agents in

Remote agents can't use the laptop's logins. See [logins and credentials](#logins-and-credentials).

### 5. Start an agent

```bash
workmux add my-feature --remote -p "Implement the parser"
```

The window opens on the laptop as usual. `workmux list` shows the workspace with its remote, and `workmux status` shows the agent as `my-feature@devbox`.

## Choosing where agents run

Agents are local unless you ask otherwise:

- `workmux add <branch> --remote [<name>]` creates the agent on a remote. The name is optional when `placement` names a remote or exactly one remote is configured.
- `workmux add <branch> --local` creates the agent locally, overriding `placement`.
- `placement` in config sets the default: `local` (the default) or the name of a remote.

```yaml
# ~/.config/workmux/config.yaml or .workmux.yaml
placement: devbox
```

`placement` can also be set in a project's `.workmux.yaml`, where it wins over the global value, so a project can send its agents to a remote or keep them local. It can only name a remote from your global config. When no remotes are configured, `placement` is ignored. When remotes are configured and `placement` names an unknown one, `add` fails and suggests `--local`.

:::caution[Put the branch before a bare `--remote`]
`--remote` takes an optional value, so in `workmux add --remote my-feature` the branch is read as the remote name. Write `workmux add my-feature --remote`, or name the remote: `workmux add --remote devbox my-feature`.
:::

Placement is decided once, when the workspace is created. Every later command (`open`, `merge`, `remove`, …) finds the workspace by its handle and knows where it lives.

The dashboard creates local agents only. If `placement` selects a remote, the dashboard's create actions refuse and point you at `workmux add --remote` or `--local`.

## Logins and credentials

Remote agents read their logins from the remote user's default agent directories (`~/.claude`, `~/.codex`, `~/.gemini`, `~/.local/share/opencode`, `~/.pi/agent`, `~/.omp/agent`), the same directories the remote containers mount. A login is shared by every workspace and every node root of that user, so you log each agent in once per remote host.

### Log in on the remote

```bash
workmux remote login devbox <agent> [-- <extra login arguments>]
```

This runs the agent's own login flow on the remote, attached to your terminal. With `container` isolation it runs inside the agent's sandbox image with the same credential mounts as the agents. In host mode it runs on the host, so the agent must be installed there.

| Agent      | Login that runs       | Notes                                                                                                        |
| ---------- | --------------------- | ------------------------------------------------------------------------------------------------------------ |
| `claude`   | `claude auth login`   | Open the printed URL on the laptop and paste the code back. See also [`--setup-token`](#claude-setup-token). |
| `codex`    | `codex login`         | The browser returns through a forwarded port. Without a browser, pass `-- --device-auth`.                    |
| `gemini`   | `gemini`              | Trust the folder, choose "Sign in with Google", open the URL and paste the code back. Leave with `/quit`.    |
| `opencode` | `opencode auth login` | Pick a provider; browser logins return through a forwarded port.                                             |
| `pi`       | `pi`                  | Type `/login` and pick a provider. Leave with `/quit`.                                                       |
| `omp`      | `omp login`           | Pick a provider, or name one: `-- anthropic`.                                                                |

Logins whose browser flow calls back to `localhost` get those ports forwarded from the laptop with `ssh -L`, so you can open the URL in the laptop's browser. Naming a provider in the extra arguments forwards only that provider's port. A port that is already in use on the laptop is skipped with a warning.

Some login screens take over Ctrl-C. Leave them with the agent's own exit command, or press Ctrl-P Ctrl-Q to leave a container login.

:::note
With a `sandbox.image` override, logins run in that image too, so it must contain the agent you log in.
:::

### Import the laptop's credentials

```bash
workmux remote login devbox codex --import
```

`--import` copies the agent's credential files from the laptop once, with mode `0600`, and nothing else from its directory. It reads the agent directory local sandboxes use, honouring `sandbox.agent_config_dir`.

- This is a one-time copy. Agents rotate their refresh tokens as they use them, so once either machine refreshes, the other copy can stop working. Import again or use `remote login` when that happens.
- On macOS, Claude Code keeps its login in the Keychain, which workmux does not read. `--import` only finds a credentials file written by a sandboxed Claude. Use `remote login` or `--setup-token` instead.
- `omp` keeps its logins in a database with its other state, so it can't be imported.

### Claude setup token

```bash
workmux remote login devbox claude --setup-token
```

This runs `claude setup-token` on the remote, then asks you to paste the token it printed. The input is hidden. The token is stored in the node root's secrets with mode `0600` and given to every Claude agent on that node root as `CLAUDE_CODE_OAUTH_TOKEN`, including agents running on the host. Press Enter without a token to keep the current one.

### Environment variables

[`sandbox.env_passthrough`](/guide/sandbox/container/#configuration) values are read from the laptop's environment when you run `workmux add --remote`, because the laptop may be asleep when the agent needs them. `sandbox.env` values are added after them and win on a name conflict. The values are kept in the node root's secrets with mode `0600` and reach the container without appearing in its command line.

- The snapshot is taken once per workspace. Changing a value on the laptop does not update existing workspaces; create a new one.
- Variables that are unset on the laptop, and values with a line break or NUL, are not sent. `add` names them in its report.
- In host mode, `sandbox.env` and `sandbox.env_passthrough` are not used.

`remote doctor` shows one row per configured agent with the credential files it finds, and points at `remote login` when they are missing.

## What runs on the remote

The node never reads the remote user's workmux config or the worktree's `.workmux.yaml`. Each workspace runs a frozen copy of the laptop's effective config, made when the workspace is created. It carries the settings that make sense on another machine: `panes`, `agent`, `agents`, `post_create`, `pre_remove`, `hook_shell`, the status icons and format, and the sandbox settings below.

`add --remote` reports each setting it did not apply. With `container` isolation:

- The sandbox is always on, whatever `sandbox.enabled` says on the laptop, and always uses Docker.
- Kept: `sandbox.image`, `target`, `network`, `toolchain`, and `container.cpus`, `memory`, `group_add`, `cap_add`, `security_opt` and `excluded_files`.
- Not applied: the Lima backend and Lima images, a non-Docker `container.runtime`, `rpc_host`, `extra_mounts`, `agent_config_dir`, `host_commands`, unsandboxed host exec, `container.devices` and `container.oci_runtime`, since they name laptop resources.

`files.copy` sources and `files.symlink` sources are uploaded from the laptop in a compressed binary stream, up to 64 MiB of uncompressed file data per workspace. Symlink sources are seeded once per project on the remote and are not kept in sync with the laptop. Symlink sources already present on the remote are reused without uploading them again; only missing symlink sources count toward the transfer limit. Uploads are validated before files are installed or hooks run. `post_create` and `pre_remove` hooks run on the remote host, in the remote worktree. A failed `add --remote` can be re-run with the same arguments: it resumes the workspace and does not repeat hooks that already finished.

Non-agent panes are shells on the remote host (or in the container with `sandbox.target: all`). The laptop's `workmux` is not available in them, so run workmux commands for remote workspaces from a local shell, naming the handle.

## Lifecycle

Remote workspaces use the usual commands. Name them by handle; the commands find them from the project's repository.

- [`workmux add --remote`](/reference/commands/add/) creates the workspace, runs its hooks and starts its panes.
- [`workmux close`](/reference/commands/close/) closes the laptop window only, the current one when run without a name inside it. The agent keeps running on the remote.
- [`workmux open`](/reference/commands/open/) reattaches to the running agent, or starts the workspace's panes again if they have ended.
- [`workmux remove`](/reference/commands/remove/) stops the agent and removes the remote worktree, branch and window, with the same checks as a local remove (see [Git](#git)).
- [`workmux resurrect`](/reference/commands/resurrect/) reopens the windows of remote agents that are still running after the laptop's tmux server restarted. The agents are not restarted.
- `workmux status`, `wait`, `send` and `capture` work on remote agents whether or not their window is open. `send` and `capture` always go through the remote.
- After `send`, `wait` waits for the turn the text started, though the agent may report it a few seconds later: the agent's earlier `done` doesn't count. Sent to an agent that is still working, the text may only run after the current turn, and `wait` ends when that turn does. A `wait` with nothing sent returns at once for an agent that is already done, and so does one more than two minutes after a `send` that started no turn.
- With container isolation, `send` reaches an agent that hasn't reported a status yet, such as Codex before its first turn after `add` without a prompt, or after `open` resumed it. Until the agent's container is running, `send` refuses; an agent whose container started less than five seconds ago gets the rest of that time to start first. In host mode, `send` needs the agent's first status: until then nothing tells the agent apart from another program in its pane, so pass the first prompt with `add -p` or type it into the window.

`workmux list` shows remote workspaces with their remote, and `list --json` adds a `remote` field. `workmux status` labels remote agents `handle@remote`, and `status --json` adds a `remote` object with the remote's name and host, whether a window is attached, whether the remote is connected and when it was last seen.

Some `add` options aren't available for remote agents; see [`add`](/reference/commands/add/#remote-workspaces). A remote branch reference such as `origin/feature` is refused too: fetch it and pass a local branch name with `--base`. `rename`, `path`, `run` and `sync-files` don't work on remote workspaces, and `remove --all` and `remove --gone` only remove local worktrees.

## Laptop sleep and disconnects

The agent, its container and its tmux session run entirely on the remote, so they don't depend on the laptop. While the laptop sleeps or is offline:

- The agent keeps working. Its status, title and Git status are recorded on the node.
- The relay pane in the laptop window shows a status line at the bottom with the remote's name and the reason, and retries with backoff (up to 30 seconds between attempts). It offers keys to retry now or close the pane.
- Keystrokes typed while the pane is disconnected are dropped, never delivered late.
- The dashboard, sidebar and `workmux status` keep listing the agent with its last known state, marked as disconnected with the time it was last seen. A remote counts as disconnected once it hasn't answered for about 15 seconds.

Interruptions shorter than about 15 seconds pass without any visible change. When the connection comes back, the relay reattaches, and the views catch up with the node's current state within a few seconds. Transitions that happened in between are not replayed.

A relay doesn't retry on its own after an authentication or host key failure, or when the remote session was attached from somewhere else. Retry with the key the status line shows once the cause is fixed. When the remote session ends, its pane closes.

## Git

The laptop stays the source of truth for your repository: work goes to the remote as a pushed base commit and comes back the same way, over the SSH connection.

### Merging

```bash
workmux merge my-feature
```

Merging a remote workspace brings its work home and runs the normal local merge:

1. The remote exports the branch, the index and the worktree.
2. The laptop fetches it into a local worktree at the usual path, with the branch, staged changes and files exactly as on the remote.
3. The laptop applies `files.copy` and `files.symlink` and runs `post_create` hooks, as for a headless local add. The exported files and index are restored afterwards so setup cannot replace the remote's work.
4. The local merge runs there: staged changes are committed with your editor, `pre_merge` hooks run on the laptop, and `merge_strategy`, `--rebase`, `--squash`, `--into` and `--keep` behave as for a local worktree.
5. The remote workspace, its agent and its branch are removed.

The merge worktree has no panes: pane commands never run, and ignored dependencies such as `node_modules` are not fetched from the remote. Put setup needed by `pre_merge` in `post_create` (for example, `cd docs && bun install`) or use `files` to share suitable laptop files. Keep setup hooks repeatable: they run for each fresh merge attempt, including `--keep`, but not when resuming a conflict in an existing copy. A setup failure aborts the merge without removing the remote workspace.

`--no-hooks` skips setup and lifecycle hooks, but still applies `files`. `--no-verify` skips `pre_merge`, not `post_create`. Ordinary `remove`, including `--keep-branch`, does not create or provision a local worktree.

A merge started on the laptop prints hook output in that terminal. A merge or removal requested from a remote pane runs in a detached laptop worker. Its stdout and stderr are retained on the laptop, including failed setup and hook output:

```bash
workmux remote logs devbox                # list log paths
workmux remote logs devbox r0123456789ab   # print one request's output
```

While the worker runs, the remote pane shows its stages (bringing the work to the laptop, setting up the merge worktree, running `pre_merge` hooks, removing the workspace) with the time spent on the current one, and the first line names the request's log command. A refusal includes the same command. Only stage names, a categorised error, the remote name and the request ID go back to the node, never hook output or laptop paths. Logs are private files under `$XDG_STATE_HOME/workmux/remote/<remote>/operation-logs/` (by default `~/.local/state/workmux/remote/<remote>/operation-logs/`). They survive workspace removal, need no SSH connection to read, and may contain sensitive hook output. Delete old log files when no longer needed; they are not automatically pruned.

As locally, unstaged changes and untracked files on the remote refuse the merge unless you pass `--ignore-uncommitted` or `--keep`. With `--ignore-uncommitted`, those changes are not merged and are removed with the workspace.

The agent keeps running until the remote workspace is removed. If it changed the workspace after the export, the remote side is kept, and the merge says so. If the merge hits a conflict, resolve it in the local worktree and run `workmux merge my-feature` again. If the laptop can't reach the remote after merging, the merge is recorded as done with the remote cleanup pending. The next `merge`, `remove`, `open`, `add`, `rebase` or `remote pull` that reaches that remote finishes the cleanup, and never merges a second time.

With `--keep` (or `merge_keep: true`), the remote workspace and its agent stay, and the remote branch moves to the merged result. If the remote changed in the meantime or can't be reached, its branch is left as it was, and the merge says so.

### Rebasing

```bash
workmux rebase my-feature
```

This pushes the laptop's base branch to the remote and rebases the workspace there, committing as your laptop Git identity. The agent keeps running. Conflicts stay on the remote: run `workmux open my-feature` and resolve them in the remote worktree.

### Bringing a workspace home

```bash
workmux remote pull my-feature
```

`remote pull` moves the workspace to the laptop: it stops the remote agent, brings the branch, the index and the worktree home as a local worktree, opens its window and removes the remote workspace. The local window runs your configured panes, so a fresh local agent starts there. The remote agent's conversation is not carried over. Pass `--no-hooks` to skip the local `post_create` hooks.

### What doesn't come home

Merge and `remote pull` carry commits, staged and unstaged changes, and untracked files. They don't carry:

- files ignored by Git, such as build output or `.env` files edited on the remote
- the contents of submodules (only the recorded commit)
- agent state, such as conversations and caches

An untracked nested Git repository, an unfinished merge, rebase or cherry-pick, or a detached HEAD in the remote worktree blocks the export until you resolve it on the remote.

### Pushing

The node has no Git credentials and no `origin`, and workmux never forwards your SSH agent, so agents on the remote can't push to your Git hosting. Bring the work home with `merge` or `remote pull` and push from the laptop.

## Agents that start agents

An agent on a remote host can start a sibling the same way a local sandboxed agent does, with `workmux add` inside its sandbox (see [spawning agents](/guide/workflows/#from-an-ongoing-agent-session)). The sibling is created on the same remote even while the laptop is away:

```bash
# run by the agent, inside its sandbox on the remote
workmux add parser-tests -b -P task.md
```

- The agent must name a new branch: generated names and existing branches are refused.
- The sibling starts from the tip of the agent's branch, with the same project, isolation, frozen config, files and environment snapshot. No `post_create` hooks run.
- The next time the laptop talks to the remote, its window opens next to the parent's window. Without `-b`, it takes focus if this happens within a minute of the spawn.

A sibling's window opens on its own when the laptop has a workspace of the same project. Otherwise run `workmux list` or `workmux status` from the project's repository to pick it up. These only ask remotes where the laptop has a workspace of that project; when it has none left there, run `workmux remote` or `workmux open <branch>` instead.

## When the remote host reboots

A reboot stops the remote tmux server and containers, but the worktrees, branches and records survive. After the host is back, the dashboard, `status` and `list` show the agent as exited because the host rebooted. Run:

```bash
workmux open my-feature
```

to start the workspace again. The agent starts with its continue flag (as `workmux open --continue` does locally), so agents that keep their conversations on disk pick up where they left off; `open` says whether the agent continued its conversation. The original prompt is not sent again. workmux doesn't restart agents automatically at boot.

## Cleaning up a remote

```bash
workmux remote gc devbox --dry-run   # list only
workmux remote gc devbox             # list, ask, remove
workmux remote gc devbox --force     # remove without asking, including unsaved work
```

`remote gc` lists what the node root holds that no workspace on this laptop accounts for, and removes it after you confirm:

- workspaces this laptop has no record of, for example after `remove --force` while the remote was unreachable
- worktrees, tmux sessions, containers, sandbox supervisors, refs and state files that belong to no workspace
- node binaries that no running agent uses
- the laptop's records of workspaces the remote no longer has

A workspace with uncommitted changes or commits beyond its base is listed but only removed with `--force`. Sibling workspaces that are still starting, files younger than ten minutes and workspaces in the middle of a merge or pull are left alone. Without a terminal, `gc` removes nothing unless you pass `--force`.

:::caution[One laptop per node root]
gc treats every workspace this laptop has no record of as unused. If two machines share a remote, give each its own `path`.
:::

## Security model

The remote host is an extension of your laptop: the SSH user you configure owns the node root, runs every agent and holds the agents' logins. Use a host or VM dedicated to agents, and treat what agents produce there as untrusted, as with a [local sandbox](/guide/sandbox/#security-model).

With `container` isolation, agents run in the same container sandbox as locally, with these differences:

- **No host commands.** [Host command proxying](/guide/sandbox/features/#host-command-proxying) and unsandboxed host exec are off on remotes; agents can only run what their container has.
- **Laptop actions are refused.** Inside a remote sandbox, `workmux merge`, `workmux close` and clipboard reads (image paste) fail with a message saying what to run on the laptop instead. Merging always starts on the laptop.
- **Private listeners.** The RPC bridge and the network proxy listen on the Docker bridge address of the host, not on all interfaces, and every RPC call needs the per-sandbox token. A host that routes or forwards the Docker bridge network to the outside needs a firewall.
- **No Git credentials.** The node has no `origin` and no credentials for it, and workmux connects without SSH agent or X11 forwarding. Anything else you give agents, such as tokens in `sandbox.env`, is up to you.
- **Global-only hosts.** Only your global config can define remotes. A repository can choose `placement` among them, but cannot add a host.

`post_create` and `pre_remove` hooks run on the remote host as the SSH user, outside the container, as they run on the laptop for local agents. They come from the laptop's config at creation time, not from the remote worktree.

In host mode, there is no sandbox: agents run directly as the SSH user with that user's access to the host, and their permission prompts are not disabled. Only use it on a machine you'd let the agent use freely.

The node creates every remote worktree, so it marks them as trusted before an agent starts, and Claude and Codex don't stop on their "trust this folder?" question. It adds `<path>/projects` to Claude's trusted folders, and each project's node repository (`<path>/projects/<id>/repo`, which Codex uses for all of the project's worktrees) to `[projects]` in Codex's `config.toml`. The entry goes into the config the agent reads: the remote user's agent directory, which containers mount, or the directory `CLAUDE_CONFIG_DIR` or `CODEX_HOME` points to in host mode. A folder you marked untrusted stays untrusted. Trusted folders let the agent load the repository's own agent settings, such as a `.codex/config.toml`.

## Troubleshooting

**`ssh` fails or asks for a password.** Run `ssh <host>` yourself first. workmux never prompts, so the key must be in your SSH agent or unencrypted, and the host key must already be in `known_hosts`. `remote doctor` names the failing step: authentication, host key, DNS, refused connection or timeout.

**Docker permission denied after adding the user to the `docker` group.** workmux reuses an SSH connection for up to ten minutes, which keeps the old group membership. Doctor's fix includes the command that closes the connection.

**Agents stop when you log out of the host.** Enable linger: `sudo loginctl enable-linger <user>`.

**An agent asks you to log in.** Run `workmux remote login <remote> <agent>`. `remote doctor` shows which agents have credentials on the remote.

**A login fails because the agent is missing.** With a `sandbox.image` override, logins run in that image, which may not contain the agent. In host mode, install the agent on the host.

**No status icons for a remote agent.** Run `workmux remote setup <remote>` again to install the status hooks, and check doctor's `hooks` row. OpenCode's hooks need the OpenCode sandbox image on the remote.

**Codex asks to review hooks in host mode.** Sandboxed Codex runs hooks without review (see [Agent flags](/guide/sandbox/features/#agent-flags)), but on the host Codex asks once for workmux's status hooks. Attach to the agent and trust them; Codex asks again only when the hooks change.

**Protocol mismatch.** The node runs a binary that matches your local workmux: the release of the same version, or the `binary` you configured, identified by its content. After an upgrade, the next remote command installs the new version next to the old one, and running agents keep the binary they started with. A mismatch means the local workmux is not the release it claims to be (for example, built from source), or `binary` was built from different source. Point `binary` at a Linux build of the same source.

**Something is left over on the remote.** Run `workmux remote gc <remote> --dry-run`.

## Known limitations

- Remote agents need tmux in window mode. Session mode, `windows:` configs and other multiplexers are not supported for them.
- Remote hosts run Linux and use Docker; Podman, Lima and Apple Container are not available there.
- The dashboard can't create remote agents; use `workmux add --remote`.
- Agents on the remote can't push, read the laptop's clipboard or run host commands.
- Ignored files, submodule contents and agent conversations don't come home with `merge` or `remote pull`.
- The environment snapshot is taken once per workspace.
- Imported credentials can drift apart from the laptop's copy. `omp` logins can't be imported, and doctor can't tell whether `omp` is logged in.
- The dashboard's diff view of a remote agent is fetched from the remote and cached; patch mode is local-only.
- A node root is meant for one laptop (see [cleaning up](#cleaning-up-a-remote)).
