---
title: "Alternatives"
description: How workmux sandboxing compares to Claude Code's built-in sandbox
---

## Claude Code's built-in sandbox

:::note[Scope]
Based on testing Claude Code v2.1.39 on macOS with sandbox auto-allow mode (February 2026). Behavior may change in future releases. See [Claude Code's sandbox documentation](https://docs.anthropic.com/en/docs/claude-code/sandboxing) for the latest details.
:::

Claude Code has a native sandbox that uses OS-level primitives (Seatbelt on macOS, bubblewrap on Linux) to restrict bash commands. It is a useful guardrail, but it operates on a fundamentally different security model than container/VM sandbox.

**Claude Code** uses _process-level restriction_. It wraps the `bash` tool process with OS sandbox rules while the agent itself runs directly on your host.

**workmux sandbox** uses Docker/Podman containers or Lima VMs for _environment isolation_. The entire agent runs inside a separate container or VM. Host files that are not explicitly mounted do not exist inside the sandbox.

### Comparison

|                          | workmux sandbox                                                                   | Claude Code sandbox                                                                                                                                                                                                                                      |
| ------------------------ | --------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Isolation**            | Full environment (container/VM). The entire agent runs in a separate OS instance. | Process restriction (Seatbelt/bwrap). Only the Bash tool is sandboxed by the OS; Read, Edit, Write, and other tools run in the unsandboxed parent process.                                                                                               |
| **Sensitive files**      | Not present in the guest. `~/.ssh`, `~/.aws`, `~/.gnupg` simply do not exist.     | Readable by default. Only `.env*` files are denied. Protection for `~/.ssh` and similar paths relies on model-level refusal, not OS enforcement.                                                                                                         |
| **Unsandboxed fallback** | No automatic retry path outside the sandbox.                                      | Failed sandboxed commands can be retried unsandboxed by default (`dangerouslyDisableSandbox`). Retries go through the normal permission flow, but in auto-allow mode the prompt is easy to miss. Can be disabled with `allowUnsandboxedCommands: false`. |
| **Hooks**                | Run inside the sandbox.                                                           | Run in the unsandboxed Node.js parent process with full host access.                                                                                                                                                                                     |

Claude's sandbox can be hardened (`allowUnsandboxedCommands: false` and custom deny rules), but the scope remains Bash-only. If your threat model requires that a compromised agent cannot touch host secrets, a container or VM boundary is a stronger guarantee.
