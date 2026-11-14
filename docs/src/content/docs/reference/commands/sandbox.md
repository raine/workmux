---
title: "sandbox"
description: Manage sandbox backends and tooling
---

Commands for managing sandbox functionality across container (Docker/Podman/Apple Container) and Lima backends.

## Container commands

### sandbox build

Build the sandbox container image locally (two-stage: base + agent).

```bash
workmux sandbox build
```

Builds the image locally for the configured agent. This is an alternative to using the pre-built image from `ghcr.io/raine/workmux-sandbox`. Most users should use `workmux sandbox pull` instead.

### sandbox pull

Pull the latest sandbox image from the container registry.

```bash
workmux sandbox pull
```

Pulls the pre-built image for the configured agent from `ghcr.io/raine/workmux-sandbox:{agent}`. This is the recommended way to get and update the sandbox image.

### sandbox init-dockerfile

Export a customizable Dockerfile for building your own sandbox image.

```bash
workmux sandbox init-dockerfile [--force]
```

**Options:**

- `--force` - Overwrite existing Dockerfile

Creates a `Dockerfile.sandbox` in the current directory with the base system setup (Debian, git, workmux) and agent-specific installation (e.g., Claude Code) combined into a single file.

## Lima commands

### sandbox stop

Stop Lima VMs to free resources.

```bash
# Interactive mode - show list and select VM
workmux sandbox stop

# Stop specific VM
workmux sandbox stop <vm-name>

# Stop all workmux VMs
workmux sandbox stop --all

# Skip confirmation prompt
workmux sandbox stop --all --yes
```

**Arguments:**

- `<vm-name>` - Name of the VM to stop (optional, conflicts with `--all`)

**Options:**

- `--all` - Stop all workmux VMs (those starting with `wm-` prefix)
- `-y, --yes` - Skip confirmation prompt

This command helps you stop running Lima VMs created by workmux to free up system resources. When run without arguments, it shows an interactive list of running workmux VMs for you to choose from. The command will ask for confirmation before stopping any VMs unless `--yes` is provided.

**Notes:**

- This command only works with Lima backend and requires `limactl` to be installed
- Only running VMs are shown in interactive mode
- If a specified VM is already stopped, the command reports this and exits successfully
- Non-interactive environments (pipes, scripts) require `--all` or a specific VM name

### sandbox prune

Delete unused Lima VMs to reclaim disk space.

```bash
# Interactive - show VMs and confirm deletion
workmux sandbox prune

# Skip confirmation and delete all workmux VMs
workmux sandbox prune --force
```

**Options:**

- `-f, --force` - Skip confirmation and delete all workmux VMs

Lists all workmux Lima VMs (those starting with `wm-` prefix) with their size, age, and last accessed time, then prompts for confirmation before deleting them. Requires `limactl` to be installed.

## General commands

### sandbox agent

Run the configured agent inside a sandbox with full RPC support. Unlike `shell`, this starts an RPC server so the agent can call workmux commands (e.g., `workmux add` to spawn sub-agents).

```bash
# Run the configured agent (from config or defaults to claude)
workmux sandbox agent

# Run a specific command instead
workmux sandbox agent -- claude -p "coordinate these tasks"
```

**Options:**

- `<command...>` - Command to run instead of the configured agent

This command runs a sandboxed agent in the current directory. It delegates to the same supervisor process used by `workmux sandbox run`, which handles RPC server setup, sandbox dispatch (Lima or container), environment variables, and cleanup.

The key difference from `sandbox shell` is that this starts an RPC server, enabling the guest to call `workmux add` from inside the sandbox. Guest-side `workmux add` detects the sandbox environment and routes through SpawnAgent RPC to the host, where sub-agents are created normally (and sandboxed if the project config says so).

**Requirements:**

- Must be run from inside a git repository (sandbox needs git directories for mounts)
- Sandbox must be configured (image pulled or built)

**Use case:** Running a coordinator agent inside a sandbox so it can spawn sub-agents via `workmux add` while still being isolated from the host.

### sandbox shell

Start an interactive shell in a sandbox. Uses the same mounts and environment as a normal worktree sandbox. Works with both container and Lima backends.

```bash
# Start a new shell (container backend starts a new container, Lima connects to existing VM)
workmux sandbox shell

# Run a specific command instead of bash
workmux sandbox shell -- <command...>

# Exec into an existing container (container backend only)
workmux sandbox shell --exec
```

**Options:**

- `-e, --exec` - Exec into an existing container for this worktree instead of starting a new one (container backend only)
- `<command...>` - Command to run instead of bash

**Backend behavior:**

- **Container:** Starts a fresh container with the same mounts and environment as a normal worktree sandbox. With `--exec`, attaches to an existing container instead.
- **Lima:** Connects to the Lima VM for the current worktree (creating it if needed). The `--exec` flag is not supported since Lima VMs are persistent and `shell` always connects to the existing VM.

### sandbox install-dev

Cross-compile and install workmux into container images and running Lima VMs for local development.

```bash
# Cross-compile and install into containers and running VMs
workmux sandbox install-dev

# Use release profile (slower build, faster binary)
workmux sandbox install-dev --release

# Skip compilation, copy existing binary
workmux sandbox install-dev --skip-build
```

**Options:**

- `--skip-build` - Skip cross-compilation and copy the previously built binary
- `--release` - Use release profile (default is debug for faster iteration)

This is a developer-only command for getting local workmux builds into sandbox environments. The host macOS binary cannot run inside Linux containers or VMs, so this command cross-compiles for the correct Linux architecture.

For container sandboxes, it builds a thin overlay image (`FROM <image>` + `COPY workmux`) on top of the configured sandbox image, replacing it in-place. For Lima VMs, it copies the binary into each running VM.

**Prerequisites:**

- Rust cross-compilation target: `rustup target add aarch64-unknown-linux-gnu`
- Cross-linker: `brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu`

The binary is installed to `~/.local/bin/workmux` inside the VM, which is already on PATH.

### sandbox run

Run a command inside a sandbox (internal, used by pane setup).

```bash
workmux sandbox run <worktree> -- <command...>
```

This is an internal command generated by `wrap_for_lima()` during pane setup. It runs the host-side supervisor process that:

1. Ensures the Lima VM is running
2. Starts a TCP RPC server on a random port
3. Runs the command inside the VM via `limactl shell`
4. Passes `WM_SANDBOX_GUEST=1`, `WM_RPC_HOST`, `WM_RPC_PORT`, and `WM_RPC_TOKEN` env vars to the guest
5. Exits with the agent command's exit code

The RPC server handles requests from the guest workmux binary:

- `SetStatus`: updates the tmux pane status icon
- `SetTitle`: renames the tmux window
- `Heartbeat`: health check
- `SpawnAgent`: runs `workmux add` on the host to create a new worktree

**Guest-side `workmux add`:** When `workmux add` runs inside a sandbox, it automatically detects the sandbox environment and routes through SpawnAgent RPC instead of trying to create worktrees locally (which would fail due to missing tmux). This enables coordinator agents running in sandboxes to spawn sub-agents. Only a subset of `add` flags are supported over RPC; unsupported flags (`--base`, `--pr`, `--with-changes`, `--count`, `--foreach`, `--name`, `--agent`, `--wait`) are explicitly rejected with clear error messages.

## Quick Setup

```bash
# 1. Enable in config (~/.config/workmux/config.yaml or .workmux.yaml)
#    sandbox:
#      enabled: true

# The pre-built image is pulled automatically on first run.
# To pull it explicitly:
workmux sandbox pull
```

## Example

```bash
# Pull the latest sandbox image
workmux sandbox pull
# Output:
# Pulling image 'ghcr.io/raine/workmux-sandbox:claude'...
# Image 'ghcr.io/raine/workmux-sandbox:claude' is up to date.
```

## See also

- [Sandbox guide](/guide/sandbox/) for full setup instructions
