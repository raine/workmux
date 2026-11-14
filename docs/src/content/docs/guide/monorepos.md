---
title: "Monorepos"
description: Per-project configs and port isolation for monorepos with multiple services
---

Tips for using workmux with monorepos containing multiple services.

## Nested configuration

Place a `.workmux.yaml` in any subdirectory to configure that project independently. When you run workmux from a subdirectory, it walks upward to find the nearest config:

```
monorepo/
├── .workmux.yaml          # Root config (used from monorepo/)
├── backend/
│   ├── .workmux.yaml      # Backend config (used from backend/)
│   └── src/
└── frontend/
    ├── .workmux.yaml      # Frontend config (used from frontend/)
    └── src/
```

```bash
cd monorepo/backend
workmux add api-feature    # Uses backend/.workmux.yaml
```

When using a nested config:

- **Working directory**: The tmux window opens in the subdirectory (e.g., `backend/`) within the new worktree, not the worktree root
- **File operations**: `files.copy` and `files.symlink` paths are relative to the config directory
- **Hooks**: Run with the subdirectory as the working directory

### Example nested config

```yaml
# backend/.workmux.yaml
agent: claude

files:
  copy:
    - .env # Copies backend/.env to worktree's backend/.env

post_create:
  - cargo build # Runs in worktree's backend/ directory
```

### Environment variables

Hooks receive `WM_CONFIG_DIR` pointing to the config directory in the new worktree:

```yaml
# backend/.workmux.yaml
post_create:
  - echo "Project root: $WM_PROJECT_ROOT" # /path/to/main-worktree
  - echo "Config dir: $WM_CONFIG_DIR" # /path/to/worktree/backend
  - echo "Worktree root: $WM_WORKTREE_PATH" # /path/to/worktree
```

### No inheritance

Nested configs completely replace the root config - they are not merged. If you need shared settings across projects, use the global config (`~/.config/workmux/config.yaml`) with the `<global>` placeholder.

## Port isolation

When running multiple services (API, web app, database) in a monorepo, each worktree needs unique ports to avoid conflicts. For example, if your `.env` has hardcoded ports like `API_PORT=3001` and `VITE_PORT=3000`, running two worktrees simultaneously would fail because both would try to bind to the same ports.

One strategy is to generate a `.env.local` file with unique ports for each worktree. Many frameworks (Vite, Next.js, CRA) automatically load `.env.local` and merge it with `.env`, with `.env.local` taking precedence.

### Example

Create a script at `scripts/worktree-env`:

```bash
#!/usr/bin/env bash
set -euo pipefail

port_in_use() {
  lsof -nP -iTCP:"$1" -sTCP:LISTEN &>/dev/null
}

find_port() {
  local port=$1
  while port_in_use "$port"; do
    ((port++))
  done
  echo "$port"
}

# Hash the handle to get a deterministic port offset (0-99)
hash=$(echo -n "$WM_HANDLE" | md5 | cut -c1-4)
offset=$((16#$hash % 100))

# Find available ports starting from the hash-based offset
api_port=$(find_port $((3001 + offset * 10)))
vite_port=$(find_port $((3000 + offset * 10)))

# Generate .env.local with port overrides
cat >.env.local <<EOF
API_PORT=$api_port
VITE_PORT=$vite_port
VITE_PUBLIC_API_URL=http://localhost:$api_port
EOF

echo "Created .env.local with ports: API=$api_port, VITE=$vite_port"
```

Configure workmux to copy `.env` and generate `.env.local`:

```yaml
# .workmux.yaml
files:
  copy:
    - .env # Copy secrets (DATABASE_URL, API keys, etc.)

post_create:
  - ./scripts/worktree-env # Generate .env.local with unique ports
```

### Plain Node.js

For Node.js without framework support, load both files with later overriding earlier:

```json
{
  "scripts": {
    "api": "node --env-file=.env --env-file=.env.local api/server.js",
    "web": "node --env-file=.env --env-file=.env.local web/server.js"
  }
}
```

### Using direnv

You can also use [direnv](https://direnv.net/) to load the generated `.env.local`:

```bash
# .envrc
dotenv
dotenv_if_exists .env.local
```

Use the same `worktree-env` script to generate `.env.local`. When you enter the directory, direnv automatically loads `.env` and `.env.local`, with the latter taking precedence.

```yaml
# .workmux.yaml
files:
  copy:
    - .envrc
    - .env

post_create:
  - ./scripts/worktree-env
```

### How it works

The worktree handle is hashed to get a deterministic starting port, so `feature-auth` always starts at the same offset. If that port is taken, `lsof` finds the next available one.

```
$ workmux add feature-auth
Running setup commands...
Created .env.local with ports: API=3471, VITE=3470
✓ Setup complete
✓ Successfully created worktree and tmux window for 'feature-auth'
```
