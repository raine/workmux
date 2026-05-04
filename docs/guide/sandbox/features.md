---
description: Features shared across container and Lima sandbox backends
---

# Shared features

These features work with both the container and Lima sandbox backends.

## Extra mounts

The `extra_mounts` option lets you mount additional host directories into the sandbox. Mounts are read-only by default for security.

`extra_mounts` is a **global-only** setting. If set in a project's `.workmux.yaml`, it is ignored and a warning is logged. This prevents a malicious repository from mounting arbitrary host paths into the sandbox.

Each entry can be a simple path string (read-only, mirrored into the guest at the same path) or a detailed spec with `host_path`, optional `guest_path`, and optional `writable` flag.

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  extra_mounts:
    # Simple: read-only, same path in guest
    - ~/Screenshots

    # Detailed: writable with custom guest path
    - host_path: ~/shared-data
      guest_path: /mnt/shared
      writable: true
```

Paths starting with `~` are expanded to the user's home directory. When `guest_path` is omitted, the expanded host path is used as the guest mount point.

**Note:** For the Lima backend, mount changes only take effect when the VM is created. To apply changes to an existing VM, recreate it with `workmux sandbox prune`.

**Note:** Apple Container only supports directory mounts. Individual file paths in `extra_mounts` will fail with Apple Container.

## Host command proxying

The `host_commands` option lets agents inside the sandbox run specific commands on the host machine. It's useful for project toolchain commands (build tools, task runners, linters) that are available on the host but would be slow or complex to install inside the sandbox. Running builds on the host is also faster since both backends use virtualization on macOS, and filesystem I/O through mount sharing adds overhead for build-heavy workloads.

::: warning Evaluate your threat model
Host command proxying is primarily a convenience feature that exists so you don't have to install your entire build toolchain inside each container or VM. It should not necessarily be expected to provide airtight confinement.

Any allowed command can execute code from project files. For example, an agent could write a malicious `justfile` and run `just`. The filesystem sandbox blocks access to host secrets and restricts writes, but proxied commands still have network access and run on the host. If your threat model requires strict isolation with no host execution, don't enable `host_commands`.
:::

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  host_commands: ['just', 'cargo', 'npm']
```

`host_commands` is only read from your global config. If set in a project's `.workmux.yaml`, it is ignored and a warning is logged. This ensures that only you control which commands get host access, not the projects you clone.

When configured, workmux creates shim scripts inside the sandbox that transparently forward these commands to the host via RPC. The host runs them in the project's toolchain environment (Devbox/Nix if available), streams stdout/stderr back to the sandbox in real-time, and returns the exit code.

Some commands are built-in and always available as host-exec shims without configuration (e.g., `afplay` for sound notifications). Only commands listed in `host_commands` or built-in are allowed; there is no wildcard or auto-discovery.

For Lima VMs: This is complementary to the toolchain integration (`toolchain: auto`). The toolchain wraps the _agent command_ itself (e.g., `claude`), while `host_commands` lets the agent invoke _other_ tools that exist on the host. For example, an agent running inside the VM could run `just check` and the command would execute on the host with full access to the project's Devbox environment.

### Security model

Host-exec applies several layers of defense to limit what a compromised agent inside the sandbox can do:

- **Allowed commands**: Only commands explicitly listed in `host_commands` (or built-in) can be executed. This is enforced on the host side.
- **Strict command names**: Command names must match `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`. No path separators, shell metacharacters, or special names (`.`, `..`) are accepted.
- **No shell injection**: When toolchain wrapping is active (devbox/nix), command arguments are passed as positional parameters to bash (`"$@"`), never interpolated into a shell string. Without toolchain wrapping, commands are executed directly via the OS with no shell involved.
- **Environment isolation**: Child processes run with a sanitized environment. Only essential variables (`PATH`, `HOME`, `TERM`, etc.) are passed through. Host secrets like API keys are not inherited. `PATH` is normalized to absolute entries only to prevent relative-path hijacking.
- **Filesystem sandbox**: On macOS, child processes run under `sandbox-exec` (Seatbelt), which denies access to sensitive directories (including `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`, `~/.docker`, `~/.claude`, `~/.config/gh`, `~/.password-store`, keychains, browser data) and credential files (including `~/.gitconfig`, `~/.vault-token`, shell histories), and denies writes to `$HOME` except toolchain caches (`.cache`, `.cargo`, `.rustup`, `.npm`). On Linux, `bwrap` (Bubblewrap) provides similar isolation with a read-only root filesystem, tmpfs over secret directories, and a writable worktree bind mount. If `bwrap` is not installed on Linux, host-exec commands are refused (fail closed).
- **Global-only config**: `host_commands` is only read from global config (`~/.config/workmux/config.yaml`). Project-level `.workmux.yaml` cannot set it. A warning is logged if it tries.
- **Global-only RPC host**: `rpc_host` is only read from global config. A malicious project config cannot redirect RPC traffic to attacker infrastructure.
- **Worktree-locked**: All commands execute with the project worktree as the working directory.

**Known limitations**:

- Allowlisted commands that read project files (build tools like `just`, `cargo`, `make`) effectively act as code interpreters. A compromised agent can write a malicious `justfile` and then invoke `just`. The filesystem sandbox mitigates this by blocking access to host secrets and restricting writes, but the child process still has network access (required for package managers).
- `sandbox-exec` is deprecated on macOS but remains functional. Apple has not announced a replacement for CLI tools.
- On Linux, `bwrap` must be installed separately (`apt install bubblewrap`). Without it, host-exec commands are refused.
- Setting `sandbox.dangerously_allow_unsandboxed_host_exec: true` in your global config skips the filesystem sandbox entirely on both macOS and Linux. Only environment sanitization is applied. This is a global-only setting; project config cannot enable it.

## Sound notifications

Claude Code hooks often use `afplay` to play notification sounds (e.g., when an agent finishes). Since `afplay` is a macOS-only binary, it doesn't exist inside the Linux guest. workmux includes `afplay` as a built-in host-exec shim that forwards sound playback to the host. This works with both Lima and container backends.

This is transparent: when a hook runs `afplay /System/Library/Sounds/Glass.aiff` inside the sandbox, the shim runs `afplay` on the host via the host-exec RPC mechanism. No configuration is needed.

## Clipboard proxy

Image pasting via Ctrl+V works inside the sandbox. workmux provides built-in shims for `wl-paste` and `xclip` that transparently proxy clipboard reads to the host. No configuration is needed.

Currently only `image/png` is supported. Text clipboard works natively through the terminal and does not need proxying.

## Git identity

The sandbox does not mount your `~/.gitconfig` because it may contain credential helpers, shell aliases, or other sensitive configuration. Instead, workmux automatically extracts your `user.name` and `user.email` from the host's git config and injects them into the sandbox via environment variables (`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_*`/`GIT_CONFIG_VALUE_*`).

This means git commits inside the sandbox use your identity without exposing the rest of your git config. The extraction respects all git config scopes (system, global, conditional includes) by running from the worktree directory, so directory-specific identities work correctly.

No configuration is needed. If the host has no `user.name` or `user.email` configured, the injection is silently skipped.

## Credentials

Both sandbox backends mount agent-specific credential directories from the host. The mounted directory depends on the configured `agent`:

| Agent      | Host directory             | Container mount               | Lima mount                     |
| ---------- | -------------------------- | ----------------------------- | ------------------------------ |
| `claude`   | `~/.claude/`               | `/tmp/.claude/`               | `$HOME/.claude/`               |
| `gemini`   | `~/.gemini/`               | `/tmp/.gemini/`               | `$HOME/.gemini/`               |
| `codex`    | `~/.codex/`                | `/home/user/.codex/`          | `$HOME/.codex/`                |
| `opencode` | `~/.local/share/opencode/` | `/tmp/.local/share/opencode/` | `$HOME/.local/share/opencode/` |

OpenCode's global config directory (`~/.config/opencode/`) is also mounted read-only, providing access to `opencode.json`, plugins, and global MCP definitions.

Key behaviors:

- Gemini, Codex, and OpenCode store credentials in files. If you've authenticated on the host, the sandbox automatically has access.
- Claude stores auth in macOS Keychain, which isn't accessible from containers or Linux VMs. You need to authenticate Claude separately inside the sandbox.
- Authentication done inside the sandbox writes back to the host directory. Credentials persist across sandbox recreations.
- The credential mount is determined by the `agent` setting. Switching agents requires recreating the sandbox (Lima) or starting a new container.

The container backend also uses a separate config file for Claude, mounted to `/tmp/.claude.json` inside the container. Docker/Podman use `~/.claude-sandbox.json` (file mount); Apple Container uses `~/.claude-sandbox-config/claude.json` (directory mount, since Apple Container only supports directory mounts).

### Custom config directory

By default, each agent's standard config directory is mounted into the sandbox (see table above). To use a separate directory, keeping sandbox config isolated from the host:

```yaml
# ~/.config/workmux/config.yaml
sandbox:
  agent_config_dir: ~/sandbox-config/{agent}
```

The `{agent}` placeholder is replaced with the active agent name (e.g. `claude`, `gemini`). The directory is auto-created if it doesn't exist.

This is useful when you want different MCP servers, project configs, or settings for sandboxed sessions without affecting your host configuration. `agent_config_dir` is a **global-only** setting.

## Coordinator agents

::: info What is a coordinator agent?
A coordinator agent sits on the main branch, plans work, and delegates tasks to worktree agents via `/worktree`. See [Workflows](/guide/workflows#from-an-ongoing-agent-session) for more on this pattern.
:::

Coordinator agents can run inside a sandbox using `workmux sandbox agent`. When the coordinator calls `workmux add` from inside the sandbox, the command is automatically routed through RPC to the host, where sub-agents are created normally (and sandboxed if the project config enables it).

Alternatively, coordinators can run on the host (unsandboxed) and only sandbox leaf agents.

## RPC protocol

The supervisor and guest communicate via JSON-lines over TCP. Each request is a single JSON object on one line.

**Supported requests:**

- `SetStatus` - updates the tmux pane status icon (working/waiting/done/clear)
- `SetTitle` - renames the tmux window
- `Heartbeat` - health check, returns Ok
- `SpawnAgent` - runs `workmux add` on the host to create a new worktree and pane
- `Exec` - runs a command on the host and streams stdout/stderr back (used by host-exec shims, including built-in `afplay`)
- `Merge` - runs `workmux merge` on the host with all flags forwarded
- `ClipboardRead` - reads the host clipboard and writes image data to the shared worktree filesystem (used by `wl-paste`/`xclip` shims)

Requests are authenticated with a per-session token passed via the `WM_RPC_TOKEN` environment variable.

## Troubleshooting

### Agent can't find credentials

Claude stores auth in macOS Keychain, so it must authenticate separately inside containers and VMs. Other agents (Gemini, Codex, OpenCode) use file-based credentials that are shared with the host automatically.

If credentials are missing, start a shell in the sandbox with `workmux sandbox shell` and run the agent to trigger authentication. Credentials written inside the sandbox persist to the host.

### Debugging blocked requests

Network proxy rejections are logged on the host at `$XDG_STATE_HOME/workmux/workmux.log`, or `~/.local/state/workmux/workmux.log` by default.

To watch rejections while reproducing an issue:

```bash
tail -f ~/.local/state/workmux/workmux.log | grep rejected
```

Rejected entries include the denied hostname. Add any expected hosts to your sandbox's `network.domains` list and retry.

## Installing local builds

During development, the macOS host binary cannot run inside Linux containers or VMs. Use `install-dev` to cross-compile and install your local workmux build:

```bash
# First time: install prerequisites
rustup target add aarch64-unknown-linux-gnu
brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu

# Cross-compile and install into containers and running VMs
workmux sandbox install-dev

# After code changes, rebuild and reinstall
workmux sandbox install-dev

# Use --release for optimized builds
workmux sandbox install-dev --release

# Skip rebuild if binary hasn't changed
workmux sandbox install-dev --skip-build
```

For containers, this builds a thin overlay image (`FROM <image>` + `COPY workmux`) on top of the configured sandbox image, replacing it in-place. For Lima VMs, the binary is installed to `~/.local/bin/workmux` inside each running VM.
