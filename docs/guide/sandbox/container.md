---
description: Run agents in isolated containers using Docker, Podman, or Apple Container
---

# Container backend

The container sandbox runs agents in isolated containers using Docker, Podman, or Apple Container, providing lightweight, ephemeral environments that reset after every session.

## Setup

### 1. Install a container runtime

```bash
# macOS
brew install --cask docker          # Docker Desktop
# or
brew install --cask orbstack        # OrbStack (Docker-compatible)
# or
brew install podman                 # Podman
```

On macOS 26+ with Apple Silicon, you can also use [Apple Container](https://github.com/apple/container). When installed, it is auto-detected and preferred over Docker/Podman.

### 2. Enable sandbox in config

Add to your global or project config:

```yaml
# ~/.config/workmux/config.yaml or .workmux.yaml
sandbox:
  enabled: true
```

The pre-built image (`ghcr.io/raine/workmux-sandbox:{agent}`) is pulled automatically on first run based on your configured agent. No manual build step is needed, but possible if required (see [custom images](#custom-images)).

To pull the latest image explicitly:

```bash
workmux sandbox pull
```

## Configuration

| Option                    | Default                                 | Description                                                                                                                                                                                                                                                                                                                                                               |
| ------------------------- | --------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `enabled`                 | `false`                                 | Enable container sandboxing                                                                                                                                                                                                                                                                                                                                               |
| `container.runtime`       | auto-detect                             | Container runtime: `docker`, `podman`, or `apple-container`. Auto-detected from PATH when not set. On macOS, prefers Apple Container (`container`) over Docker/Podman.                                                                                                                                                                                                    |
| `container.memory`        | `16G` (Apple Container) / none (others) | Memory limit for the container. Apple Container VMs default to 1 GB which is too low, so workmux sets `16G` by default. This is a ceiling, not an upfront allocation. Works with any runtime when explicitly set.                                                                                                                                                         |
| `container.cpus`          | none                                    | CPU count for the container. Only passed when explicitly set. Apple Container defaults to 4 CPUs which is sufficient for most workloads.                                                                                                                                                                                                                                  |
| `container.devices`       | `[]`                                    | Host device nodes exposed to the sandbox (e.g. `/dev/kvm`, `/dev/ttyUSB0`). Passed to the runtime as `--device`. Docker and Podman only; Apple Container rejects. **Global config only.**                                                                                                                                                                                 |
| `container.group_add`     | `[]`                                    | Supplementary groups added to the sandboxed process (e.g. `dialout`, `video`, or numeric GIDs). Passed to the runtime as `--group-add`. Docker and Podman only; Apple Container rejects. **Global config only.**                                                                                                                                                          |
| `container.cap_add`       | `[]`                                    | Extra Linux capabilities granted to the container, passed verbatim as `--cap-add` (e.g. `ALL` or `SYS_ADMIN`). Docker and Podman only; Apple Container ignores it. Permissive values weaken host-kernel container isolation. **Global config only.**                                                                                                                     |
| `container.security_opt`  | `[]`                                    | Security options passed verbatim as `--security-opt` (e.g. `seccomp=unconfined` or `no-new-privileges`). Docker and Podman only; Apple Container ignores it. Options can strengthen or weaken confinement. **Global config only.**                                                                                                                                      |
| `container.oci_runtime`   | none (engine default)                   | OCI runtime to run the container under, passed as Docker or local Podman `--runtime`. Unset omits `--runtime`, deferring to the engine's configured default. Set to e.g. `kata` for stronger VM-based isolation. Remote Podman rejects this flag; Apple Container ignores it. **Global config only.**                                                                   |
| `target`                  | `agent`                                 | Which panes to sandbox: `agent` or `all`                                                                                                                                                                                                                                                                                                                                  |
| `image`                   | `ghcr.io/raine/workmux-sandbox:{agent}` | Container image name (auto-resolved from configured agent).                                                                                                                                                                                                                                                                                                               |
| `rpc_host`                | auto                                    | Override hostname for guest-to-host RPC. Defaults to `host.docker.internal` (Docker), `host.containers.internal` (Podman), or `192.168.64.1` (Apple Container). **Global config only.**                                                                                                                                                                                   |
| `env_passthrough`         | `[]`                                    | Environment variables to pass through. **Global config only.**                                                                                                                                                                                                                                                                                                            |
| `env`                     | `{}`                                    | Environment variables to set with explicit values (unlike `env_passthrough` which reads from host). **Global config only.**                                                                                                                                                                                                                                               |
| `extra_mounts`            | `[]`                                    | Additional host paths to mount (see [shared features](./features#extra-mounts)). **Global config only.**                                                                                                                                                                                                                                                                  |
| `agent_config_dir`        | per-agent default                       | Custom host directory for agent config. Supports `{agent}` placeholder. Overrides default mounts (e.g. `~/.claude/`). Auto-created if missing. **Global config only.**                                                                                                                                                                                                    |
| `network.policy`          | `allow`                                 | Network restriction policy: `allow` (no restrictions) or `deny` (block all except allowed domains). See [network restrictions](#network-restrictions). **Global config only.**                                                                                                                                                                                            |
| `network.allowed_domains` | `[]`                                    | Allowed outbound HTTPS domains when policy is `deny`. Supports exact matches, `*.` wildcard prefixes, and exact-host private destination opt-in. **Global config only.**                                                                                                                                                                                                  |

### Example configurations

**Minimal:**

```yaml
sandbox:
  enabled: true
```

**With Podman and custom env:**

```yaml
sandbox:
  enabled: true
  image: my-sandbox:latest
  env_passthrough:
    - GITHUB_TOKEN
    - ANTHROPIC_API_KEY
  env:
    GH_TOKEN: ghp_xxxxxxxxxxxx
  container:
    runtime: podman
```

**With Apple Container (macOS 26+):**

```yaml
sandbox:
  enabled: true
  container:
    runtime: apple-container
    # memory: 16G   # default, adjust if needed
    # cpus: 8       # optional, Apple Container defaults to 4
```

**With host hardware (USB serial, GPU, etc.):**

```yaml
sandbox:
  enabled: true
  container:
    runtime: docker
    devices:
      - /dev/kvm
      - /dev/bus/usb/001/002:/dev/bus/usb/001/002:rwm
      # structured form is also supported:
      - host_path: /dev/ttyUSB0
        guest_path: /dev/ttyUSB0
        permissions: rw
    group_add:
      - dialout # access to serial devices
      - "46" # numeric GID if the name differs between host and container
```

Notes on hardware access:

- Both `devices` and `group_add` are **global config only**. A project's
  `.workmux.yaml` cannot opt into them; this prevents a repository from
  silently widening the sandbox.
- Apple Container does not support `--device` or `--group-add`. Set
  `container.runtime: docker` (or `podman`) if you need hardware access.
- Group names (e.g. `dialout`) must resolve inside the container image.
  When the host and image disagree on GIDs (common across distros), use
  numeric GIDs.
- In network-deny mode, workmux drops privileges inside the container via
  `setpriv` (replacing `gosu`), which preserves the configured supplementary
  groups. This means hardware access works in both `allow` and `deny`
  network modes.
- Rootless Podman may require additional configuration to access host
  devices; see Podman's own documentation on `keep-groups` and subgid
  mapping if you hit permission errors.

**Hide specific files from the container:**

```yaml
sandbox:
  enabled: true
  container:
    excluded_files:
      - .env
      - .env.local
```

Listed paths are relative to the worktree root. Each one is shadowed by a read-only `/dev/null` bind mount, so agents running inside the container cannot read the host file without having to restructure the project. Files reachable via workmux's main-worktree mount (including symlinks from the current worktree into it) are masked at both paths. Absolute paths and entries containing `..` components are rejected; files that don't exist on disk are skipped with a warning.

**Security: global-only.** `excluded_files` is ignored when set in a project's `.workmux.yaml`; it must be configured in your global config (`~/.config/workmux/config.yaml`). This prevents a malicious repo from deleting protections via its own config.

**Note:** `excluded_files` relies on file-level bind mounts, which Apple Container does not support (it only accepts directory mounts). When the runtime is Apple Container and `excluded_files` is set, workmux fails fast rather than silently leaving secrets readable. Use Docker or Podman if you need this feature.

**Run under a VM-based OCI runtime (stronger isolation):**

```yaml
sandbox:
  enabled: true
  container:
    runtime: docker
    oci_runtime: kata
```

`oci_runtime` is passed straight through to Docker or local Podman as `--runtime`,
so the container runs under whatever OCI runtime you name instead of the engine's
configured default. Pointing it at a VM-based runtime such as
[Kata Containers](https://katacontainers.io/) gives each sandbox a hardware-VM
boundary while reusing the exact same container image, mounts, user mapping, and
RPC path as the normal container backend. Leaving it unset keeps the engine's
default behavior.

The named runtime must already be installed on the host and registered with your
container engine (for Docker, listed under `docker info`'s runtimes). Setting it
up is out of scope here. See the runtime's own documentation.

Notes:

- **Global config only.** Like `devices`/`group_add`, `oci_runtime` is ignored
  when set in a project's `.workmux.yaml`. It changes the isolation boundary, so
  a repository must not be able to select (or downgrade) it via its own config.
- **Kata needs Docker.** Kata Containers does not officially support Podman as a
  container manager ([Kata limitations](https://github.com/kata-containers/kata-containers/blob/main/docs/Limitations.md)),
  so use `runtime: docker` for the `kata` example above.
- **Podman support is local-only.** Other OCI runtimes may work with local Podman
  on Linux. Remote Podman clients, including all macOS and Windows builds, do
  not expose `podman run --runtime`. Workmux rejects `oci_runtime` on non-Linux
  hosts and when `CONTAINER_HOST` or `CONTAINER_CONNECTION` selects a remote
  service. Other remote Podman configurations are unsupported and may return
  `unknown flag: --runtime` directly from Podman.
- **Apple Container ignores it.** Apple Container manages its own VM and has no
  `--runtime` concept, so a configured `oci_runtime` is silently omitted there
  (as are `cap_add`/`security_opt`). This lets one global config target Kata on a
  Docker host and still run on macOS without erroring.
- **Network-deny mode** ([below](#network-restrictions)) configures `iptables`
  inside the container, which needs a real in-guest kernel. It works under a
  VM-based runtime like Kata, but may not under a userspace-kernel runtime such
  as gVisor.

**Docker-in-Docker under Kata:**

```yaml
sandbox:
  enabled: true
  container:
    runtime: docker
    oci_runtime: kata
    cap_add: [ALL]
    security_opt:
      - seccomp=unconfined
      - apparmor=unconfined
      - systempaths=unconfined
```

`cap_add` and `security_opt` are global-only passthroughs for Docker and Podman.
They support an inner Docker daemon under Kata without `--privileged`, which
injects host device nodes that conflict with devices in the guest. The values
above grant every Linux capability and disable several container-level security
controls, while Kata's guest VM remains the host isolation boundary.

These fields are independent of `oci_runtime`. Without a VM-based OCI runtime,
permissive values apply to a host-kernel container and weaken the sandbox's
isolation from the host. `security_opt` values such as `no-new-privileges` can
instead strengthen confinement. Apple Container silently omits both fields.

**Sandbox all panes (not just agent):**

```yaml
sandbox:
  enabled: true
  target: all
```

## How it works

When you run `workmux add feature-x`, the agent command is wrapped:

```bash
# Without sandbox:
claude -- "$(cat .workmux/PROMPT-feature-x.md)"

# With sandbox (Docker example):
docker run --rm -it \
  --user 501:20 \
  --env HOME=/tmp \
  --mount type=bind,source=/path/to/worktree,target=/path/to/worktree \
  --mount type=bind,source=/path/to/main/.git,target=/path/to/main/.git \
  --mount type=bind,source=/path/to/main,target=/path/to/main \
  --mount type=bind,source=~/.claude-sandbox.json,target=/tmp/.claude.json \
  --mount type=bind,source=~/.claude,target=/tmp/.claude \
  --workdir /path/to/worktree \
  workmux-sandbox:claude \
  sh -c 'claude -- "$(cat .workmux/PROMPT-feature-x.md)"'
```

The exact flags vary by runtime (e.g., Podman adds `--userns=keep-id`, Apple Container uses directory mounts instead of file mounts).

### What's mounted

| Mount                    | Access      | Purpose                                                       |
| ------------------------ | ----------- | ------------------------------------------------------------- |
| Worktree directory       | read-write  | Source code                                                   |
| Main worktree            | read-write  | Symlink resolution (e.g., CLAUDE.md)                          |
| Main `.git`              | read-write  | Git operations                                                |
| Agent credentials        | read-write  | Auth and settings (see [Credentials](./features#credentials)) |
| `extra_mounts` entries   | read-only\* | User-configured paths                                         |
| `excluded_files` entries | masked      | Shadowed with `/dev/null` so sensitive files are unreadable   |

\* Extra mounts are read-only by default. Set `writable: true` to allow writes.

For Claude specifically, a separate config file is mounted to `/tmp/.claude.json`. Docker/Podman mount `~/.claude-sandbox.json` directly; Apple Container mounts the `~/.claude-sandbox-config/` directory (since it only supports directory mounts).

### Networking

By default, containers have unrestricted network access. To restrict outbound connections to only approved domains, configure [network restrictions](#network-restrictions). When enabled, all outbound HTTPS is routed through a host-resident proxy that enforces a domain allowlist, and iptables rules inside the container block any direct connections.

### Debugging with `sandbox shell`

Start an interactive shell inside a container for debugging:

```bash
# Start a new container with the same mounts
workmux sandbox shell

# Exec into the currently running container for this worktree
workmux sandbox shell --exec
```

The `--exec` flag attaches to an existing running container instead of starting a new one. This is useful for inspecting the state of a running agent's environment.

## Network restrictions

Network restrictions block outbound connections from sandboxed containers, only allowing traffic to domains you explicitly whitelist. This prevents agents from accessing your local network, exfiltrating data to unauthorized services, or making unintended API calls.

### Configuration

Add to global config (`~/.config/workmux/config.yaml`):

```yaml
sandbox:
  enabled: true
  network:
    policy: deny
    allowed_domains:
      # Claude Code (adjust for your agent)
      - "api.anthropic.com"
      - "platform.claude.com"
```

`network` is a global-only setting. If set in a project's `.workmux.yaml`, it is ignored and a warning is logged. This ensures that project config cannot weaken network restrictions set by the user.

Domain entries support exact matches (`github.com`) and wildcard prefixes (`*.github.com`). Wildcards match subdomains only, not the base domain itself (e.g., `*.github.com` matches `api.github.com` but not `github.com`).

By default, allowed domains that resolve to private/internal IP ranges are still rejected. To reach a trusted VPN-hosted mirror, opt in for one exact host with object syntax:

```yaml
sandbox:
  enabled: true
  network:
    policy: deny
    allowed_domains:
      - api.anthropic.com
      - host: artifactory.example.com
        allow_private_ips: true
```

`allow_private_ips: true` permits RFC1918, CGNAT (`100.64.0.0/10`), and IPv6 ULA destinations for that exact host. It does not allow loopback, link-local, unspecified, multicast, or broadcast destinations. IP literals and wildcard private opt-ins like `host: "*.example.com"` are rejected.

### How it works

Two layers enforce the restrictions:

1. **iptables firewall** inside the container blocks all direct outbound connections, forcing traffic through a host-resident proxy.
2. **CONNECT proxy** on the host checks each domain against the allowlist and rejects connections to private/internal IPs.

This means agents cannot bypass restrictions by ignoring proxy environment variables.

Only HTTPS (port 443) to allowed domains gets through. The proxy also rejects connections to private/internal IP ranges by default, so allowed domains cannot be used to reach local network services unless an exact host is explicitly configured with `allow_private_ips: true`. Loopback, link-local, unspecified, multicast, and broadcast destinations are always blocked. Non-HTTPS protocols like `git+ssh` are blocked; use HTTPS git remotes instead. IPv6 is blocked in direct container egress to prevent bypassing the IPv4 firewall.

### Known limitations

- **Non-HTTP protocols**: Protocols like `git+ssh` are blocked. Use HTTPS git remotes (`git clone https://...`) instead of SSH (`git clone git@...`).
- **Podman rootless**: Network restrictions require `CAP_NET_ADMIN` for iptables. On rootless Podman, this may require additional configuration depending on your setup.

## Custom images

To add tools or customize the sandbox environment, export the Dockerfile and modify it:

```bash
workmux sandbox init-dockerfile        # creates Dockerfile.sandbox
vim Dockerfile.sandbox                 # customize
docker build -t my-sandbox -f Dockerfile.sandbox .
```

To build the default image locally instead of pulling from the registry:

```bash
workmux sandbox build
```

Then set the image in your config:

```yaml
sandbox:
  enabled: true
  image: my-sandbox
```

## Security: hooks in sandbox

Pre-merge and pre-remove hooks are always skipped for RPC-triggered merges (`--no-verify --no-hooks` is forced by the host). This prevents a compromised guest from injecting malicious hooks via `.workmux.yaml` and triggering them on the host. Similarly, `SpawnAgent` RPC forces `--no-hooks` to skip post-create hooks.
