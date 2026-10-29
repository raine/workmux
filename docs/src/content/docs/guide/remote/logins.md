---
title: "Logins and secrets"
description: Log agents in on a remote server and give them environment variables
---

Remote agents can't use the local machine's logins. Their logins live in the SSH user's home directory on the server and are shared by every workspace there, in both modes, so you log each agent in once per server.

## Log in on the server

```bash
workmux remote login devbox <agent> [-- <extra login arguments>]
```

This runs the agent's own login flow on the server, attached to your terminal. In Docker mode it runs in the agent's sandbox image; on the server directly, the agent must be installed there.

| Agent      | Login that runs       | Notes                                                                                                                    |
| ---------- | --------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `claude`   | `claude auth login`   | Open the printed URL on the local machine and paste the code back. See also [`--setup-token`](#claude-long-lived-token). |
| `codex`    | `codex login`         | The browser returns through a forwarded port. Without a browser, pass `-- --device-auth`.                                |
| `gemini`   | `gemini`              | Trust the folder, choose "Sign in with Google", open the URL and paste the code back. Leave with `/quit`.                |
| `opencode` | `opencode auth login` | Pick a provider; browser logins return through a forwarded port.                                                         |
| `pi`       | `pi`                  | Type `/login` and pick a provider. Leave with `/quit`.                                                                   |
| `omp`      | `omp login`           | Pick a provider, or name one: `-- anthropic`.                                                                            |

When a login's browser flow calls back to `localhost`, workmux forwards that port from the local machine, so you can open the URL in the local machine's browser. Naming a provider in the extra arguments forwards only its port. A port already in use on the local machine is skipped with a warning.

Some login screens take over Ctrl-C. Leave them with the agent's own exit command, or press Ctrl-P Ctrl-Q to leave a container login.

:::note
With a `sandbox.image` override, Docker-mode logins run in that image, so it must contain the agent you log in.
:::

## Copy logins from the local machine

```bash
workmux remote login devbox codex --import
```

`--import` copies the agent's credential files from the local machine, and nothing else from its directory.

- It is a one-time copy. Agents rotate their refresh tokens as they use them, so once either machine refreshes, the other copy can stop working. Import again or log in on the server when that happens.
- On macOS, Claude Code keeps its login in the Keychain, which workmux doesn't read. Use `remote login` or `--setup-token` for Claude.
- `omp` keeps its logins in a database with its other state, so it can't be imported.

## Claude long-lived token

```bash
workmux remote login devbox claude --setup-token
```

Runs `claude setup-token` on the server, then asks you to paste the token it printed (the input is hidden). Every Claude agent on that server's `path` uses the token, in both modes. It is stored privately on the server. Press Enter without a token to keep the current one.

## Environment variables

In Docker mode, [`sandbox.env_passthrough`](/guide/sandbox/container/#configuration) values are read from the local machine's environment when you run `workmux add --remote`, because the local machine may be asleep when the agent needs them. `sandbox.env` values are added after them and win on a name conflict. They are stored privately on the server.

- They are read once per workspace. Changing a value on the local machine doesn't update existing workspaces; create a new one.
- Variables unset on the local machine, and values containing a line break or NUL, aren't sent. `add` names them.
- Agents running directly on the server don't get `sandbox.env` or `env_passthrough`.

## Check logins

`remote doctor` shows one row per configured agent with the credential files it finds, and points at `remote login` when they are missing. It can't tell whether a login has expired, or whether `omp` is logged in.
