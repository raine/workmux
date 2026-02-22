---
description: Common worktree pitfalls and how workmux handles them
---

# Worktree caveats

While powerful, worktrees (git worktrees and jj workspaces) have nuances that are important to understand. workmux is designed to automate solutions to these, but awareness of the underlying mechanics helps.

> **Note:** Most caveats below apply to both git worktrees and jj workspaces. Sections that are git-specific are marked as such.

## Ignored files require configuration

When a new worktree is created (via `git worktree add` or `jj workspace add`), it's a clean checkout. Ignored files (e.g., `.env` files, `node_modules`, IDE configuration) will not exist in the new worktree by default. Your application will be broken in the new worktree until you manually create or link these necessary files.

This is a primary feature of workmux. Use the `files` section in your `.workmux.yaml` to automatically copy or symlink these files on creation:

```yaml
# .workmux.yaml
files:
  copy:
    - .env # Copy environment variables
  symlink:
    - .next/cache # Share Next.js build cache
```

::: warning
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

Worktrees isolate your filesystem, but they do not prevent merge conflicts. If you modify the same area of code on two different branches (in two different worktrees), you will still have a conflict when you merge one into the other. This applies to both git and jj.

The best practice is to work on logically separate features in parallel worktrees. When conflicts are unavoidable, use standard VCS tools to resolve them (`git` conflict resolution or `jj resolve`). You can also leverage an AI agent within the worktree to assist with the conflict resolution.

## Package manager considerations (pnpm, yarn)

Modern package managers like `pnpm` use a global store with symlinks to `node_modules`. Each worktree typically needs its own `pnpm install` to set up the correct dependency versions for that branch.

If your worktrees always have identical dependencies (e.g., working on multiple features from the same base), you could potentially symlink `node_modules` between worktrees. However, this breaks as soon as branches diverge in their dependencies, so it's generally safer to run a fresh install in each worktree.

::: info
In large monorepos, cleaning up `node_modules` during worktree removal can take significant time. workmux has a [special cleanup mechanism](https://github.com/raine/workmux/blob/main/src/scripts/cleanup_node_modules.sh) that moves `node_modules` to a temporary location and deletes it in the background, making the `remove` command return almost instantly.
:::

## Rust projects

Unlike `node_modules`, Rust's `target/` directory should **not** be symlinked between worktrees. Cargo locks the `target` directory during builds, so sharing it would block parallel builds and defeat the purpose of worktrees.

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

## Symlinks and `.gitignore` trailing slashes (git-specific)

If your `.gitignore` uses a trailing slash to ignore directories (e.g., `tests/venv/`), symlinks to that path in the created worktree will **not** be ignored and will show up in `git status`. This is because `venv/` only matches directories, not files (symlinks).

To ignore both directories and symlinks, remove the trailing slash:

```diff
- tests/venv/
+ tests/venv
```

## Local git ignores are not shared (git-specific)

The local git ignore file, `.git/info/exclude`, is specific to the main worktree's git directory and is not respected in other worktrees. Personal ignore patterns for your editor or temporary files may not apply in new worktrees, causing them to appear in `git status`.

For personal ignores, use a global git ignore file. For project-specific ignores that are safe to share with your team, add them to the project's main `.gitignore` file.
