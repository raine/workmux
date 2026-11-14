---
title: "Git worktree caveats"
description: Common git worktree pitfalls and how workmux handles them
---

While powerful, git worktrees have nuances that are important to understand. workmux is designed to automate solutions to these, but awareness of the underlying mechanics helps.

## Gitignored files require configuration

When `git worktree add` creates a new working directory, it's a clean checkout. Files listed in your `.gitignore` (e.g., `.env` files, `node_modules`, IDE configuration) will not exist in the new worktree by default. Your application will be broken in the new worktree until you manually create or link these necessary files.

This is a primary feature of workmux. Use the `files` section in your `.workmux.yaml` to automatically copy or symlink these files on creation:

```yaml
# .workmux.yaml
files:
  copy:
    - .env # Copy environment variables
  symlink:
    - .next/cache # Share Next.js build cache
```

:::caution
Symlinking `node_modules` can be efficient but only works if all worktrees share identical dependencies. If different branches have different dependency versions, each worktree needs its own installation.
:::

For dependency installation, consider using a pane command instead of `post_create` hooks - this runs the install in the background without blocking the worktree and window creation:

```yaml
panes:
  - command: npm install
    focus: true
  - split: horizontal
```

## Conflicts

Worktrees isolate your filesystem, but they do not prevent merge conflicts. If you modify the same area of code on two different branches (in two different worktrees), you will still have a conflict when you merge one into the other.

The best practice is to work on logically separate features in parallel worktrees. When conflicts are unavoidable, use standard git tools to resolve them. You can also leverage an AI agent within the worktree to assist with the conflict resolution.

## Package manager considerations (pnpm, yarn)

Modern package managers like `pnpm` use a global store with symlinks to `node_modules`. Each worktree typically needs its own `pnpm install` to set up the correct dependency versions for that branch.

If your worktrees always have identical dependencies (e.g., working on multiple features from the same base), you could potentially symlink `node_modules` between worktrees. However, this breaks as soon as branches diverge in their dependencies, so it's generally safer to run a fresh install in each worktree.

:::note
In large monorepos, cleaning up `node_modules` during worktree removal can take significant time. workmux has a [special cleanup mechanism](https://github.com/raine/workmux/blob/main/src/scripts/cleanup_node_modules.sh) that moves `node_modules` to a temporary location and deletes it in the background, making the `remove` command return almost instantly. It is enabled
automatically when a lockfile (`pnpm-lock.yaml`, `package-lock.json`, or
`yarn.lock`) exists at the project root or in a direct subdirectory. You can also
request it explicitly with `"<cleanup-node-modules>"` in `pre_remove`.
:::

## Rust projects

Rust's `target/` directory should **not** be symlinked between worktrees. Cargo locks the `target` directory during builds, so sharing it would block parallel builds and defeat the purpose of worktrees.

Instead, use [sccache](https://github.com/mozilla/sccache) to share compiled dependencies across worktrees:

```bash
brew install sccache
```

Add to `~/.cargo/config.toml`:

```toml
[build]
rustc-wrapper = "sccache"
```

This caches compiled dependencies globally, so new worktrees benefit from cached artifacts without any lock contention.

## Symlinks and `.gitignore` trailing slashes

If your `.gitignore` uses a trailing slash to ignore directories (e.g., `tests/venv/`), symlinks to that path in the created worktree will **not** be ignored and will show up in `git status`. This is because `venv/` only matches directories, not files (symlinks).

To ignore both directories and symlinks, remove the trailing slash:

```diff
- tests/venv/
+ tests/venv
```
