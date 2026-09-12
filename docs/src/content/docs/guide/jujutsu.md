---
title: "Jujutsu (jj)"
description: What workmux supports in a jj repository today, and which commands are still git-only
---

workmux has experimental support for [Jujutsu](https://jj-vcs.github.io/jj/) repositories. It is detected automatically: if the directory you run workmux in resolves to a jj repository (`jj git init`, with or without `--colocate`), workmux drives `jj` instead of `git`.

The vocabulary maps like this:

| git            | jj                                       |
| -------------- | ---------------------------------------- |
| worktree       | workspace (`jj workspace add/forget`)    |
| branch         | bookmark (`jj bookmark create/delete`)   |
| `HEAD`         | `@`, the working-copy commit             |
| `origin/main`  | `main@origin`                            |

Per-worktree metadata that workmux keeps in `git config --local` for a git repository is instead stored in a TOML file at `.jj/workmux/metadata.toml` inside the primary workspace. It lives under `.jj/` on purpose: jj snapshots the working copy into `@` on almost every command, so anything workmux wrote at the workspace root would be committed into your own history.

## Known limitations

**jj support currently covers `workmux add` and `workmux remove` end to end. Nothing else does.**

These commands do not yet recognize jj repositories and behave as if the repository were git-only:

- `workmux open`
- `workmux merge`
- `workmux close`
- `workmux rename`
- `workmux list`
- `workmux status`
- `workmux dashboard`
- `workmux sidebar`
- `workmux rebase`
- `workmux set-base`
- `workmux resurrect`
- pull-request handling (`workmux add --pr`, the dashboard's PR status and actions)

The failure mode differs by repository layout, and the colocated one is the confusing case:

- **jj-only repository** (`jj git init --no-colocate`, no `.git` at the workspace root): these commands shell out to `git` and fail, usually with a `not a git repository` error. Noisy, but unambiguous.
- **Colocated repository** (`jj git init --colocate`): `git` commands succeed, so these commands silently operate on the **git view** of the repository while `workmux add` and `workmux remove` operate on the **jj view**. Since jj keeps git's `HEAD` detached and only materializes bookmarks as git branches when it syncs them, the two views can disagree — a workspace workmux created through jj may not appear as a git worktree, `workmux list` may show nothing, and a branch these commands report may not be where `@` actually is. No error is raised.

`workmux sandbox agent` is the exception: it bails with an explicit error in a non-git repository rather than degrading, because it mounts the repository's git directories into the container.

Until the rest of the surface is migrated, the supported workflow in a jj repository is `workmux add` to create a workspace and `workmux remove` to tear it down, with everything in between done through `jj` directly.
