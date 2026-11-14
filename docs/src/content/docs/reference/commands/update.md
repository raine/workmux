---
title: "update"
description: Update workmux to the latest version
---

Updates workmux to the latest version by downloading the prebuilt binary from GitHub Releases.

```bash
workmux update
```

## What happens

1. Checks the latest release version from the GitHub API
2. Compares with the currently installed version
3. Downloads the correct binary for your OS and architecture
4. Verifies the SHA-256 checksum
5. Atomically replaces the current binary (with rollback on failure)

If workmux is already up to date, it reports this and exits.

## Homebrew installs

If workmux was installed via Homebrew, the command will detect this and instruct you to use `brew upgrade` instead:

```bash
brew upgrade workmux
```

## Supported platforms

The command downloads prebuilt binaries for:

- macOS (Apple Silicon / Intel)
- Linux (x86_64 / ARM64)

## Automatic update check

Workmux periodically checks for new versions in the background. When an update is available, a one-line notice is printed to stderr:

```
Update available: workmux v0.1.124 -> v0.1.125 (run `workmux update`)
```

The check runs at most once every 24 hours via a detached background process and never slows down your commands. The notice is shown at most once per day and only in interactive terminals.

### Disabling

To disable the automatic update check, either:

- Set `auto_update_check: false` in the global config (`~/.config/workmux/config.yaml`)
- Set the environment variable `WORKMUX_NO_UPDATE_CHECK=1`

## Requirements

- `curl` must be available in PATH (used for downloading)
- `tar` must be available in PATH (used for extraction)
- `sha256sum` or `shasum` must be available (used for checksum verification)
- Write permission to the directory containing the workmux binary
