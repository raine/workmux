---
title: "Jujutsu (jj)"
description: Create, merge, and clean up jj workspaces with workmux
---

workmux supports jj 0.46 or newer in **colocated repositories**. Install `jj` on your `PATH`. To convert an existing non-colocated jj repository, run:

```bash
jj git colocation enable
```

Older jj versions and non-colocated repositories produce an error rather than falling back to Git operations. Start with an initial commit and a main bookmark; an empty repository is not supported.

## Workspaces and bookmarks

A jj workspace is a workmux worktree. A workmux branch is a jj bookmark, and the workspace name is the workmux handle. Normal window setup, hooks, and file copying still apply.

jj snapshots untracked files into the working-copy commit, so files copied into a workspace by `files.copy` become part of its changes unless they are ignored. Symlinks created by `files.symlink` to ignored paths, such as `node_modules`, are added to the repository's `info/exclude` so they stay out of commits.

```bash
workmux add feature-auth --base main
```

This creates a colocated workspace with its working-copy commit on top of `main`. The new `feature-auth` bookmark starts on the **base commit**, not the working-copy commit. Using an existing bookmark creates a workspace on top of that bookmark.

Before merging or non-forced removal, workmux moves the workspace's bookmark to the workspace's latest commit. That is the top of the commits stacked above `@`, for example after `jj edit` of an earlier commit. Without such commits, it is `@` when `@` has both changes and a description; otherwise, `@-`. If several stacks branch off above `@`, workmux asks you to abandon or rebase all but one. Removal only advances a bookmark along its existing history. Merge rejects a bookmark on unrelated history and tells you how to reposition it.

## Choosing a base

Without `--base`, `workmux add` uses:

- Inside a workmux workspace: that workspace's bookmark.
- Elsewhere: the single bookmark on `@`. If none is found and `@` has no description, it checks `@-` instead. Bookmarks belonging to workmux workspaces are excluded from this lookup.

If there is no suitable bookmark or more than one, workmux asks for `--base` and lists candidates when available. It does not silently choose the main branch.

```bash
workmux add feature-auth --base main
workmux add follow-up --base feature-auth
```

When the base belongs to another workmux workspace, workmux first advances that bookmark to include the workspace's latest work.

## Uncommitted changes

jj records file changes in the working-copy commit automatically. For merging, workmux treats a working-copy commit **with changes but no description** as uncommitted. Describe it before merging:

```bash
jj describe -m "Add authentication"
workmux merge
```

You can also use `jj commit`. An empty working-copy commit is not included in the merge, even if it has a description.

`--ignore-uncommitted` merges through `@-`, excluding the undescribed changes. Normal cleanup then removes the workspace, so those changes are not retained in its files. `--keep` also permits undescribed changes but keeps the workspace available.

## Merging

`workmux merge` targets the saved base bookmark if it still exists locally, otherwise the configured main branch. Use `--into <bookmark>` to choose another existing target. When the target is another workmux workspace's bookmark, workmux first advances it to that workspace's latest work, and afterwards moves that workspace's empty, undescribed working-copy commit onto the result. If the workspace has other work on top, workmux prints the `jj rebase` command to run there.

| Command                  | Behavior                                                                                                                                    |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `workmux merge`          | Fast-forwards the target when possible; otherwise creates a merge commit.                                                                   |
| `workmux merge --rebase` | Rebases the workspace's branch onto the target, then fast-forwards the target.                                                              |
| `workmux merge --squash` | Creates one commit on the target with the combined changes. Opens the jj description editor, prefilled with the source commit descriptions. |
| `workmux merge --keep`   | Integrates the work without removing the workspace, bookmark, or terminal window. Can be combined with either strategy.                     |

Pre-merge hooks run after bookmark preparation and before integration. If the main workspace is on an empty, undescribed working-copy commit directly above the old target, workmux moves it onto the updated target.

Merge refuses work that jj recorded with unresolved conflicts, a conflicted bookmark, or divergent changes, and explains how to fix it. Merge and squash conflicts leave the target unchanged. Rebase conflicts remain in the workspace for you to resolve. Follow the error's instructions, inspect `jj status`, resolve the conflicts, and retry.

A workspace whose commits were rewritten from another workspace is stale. workmux updates it before using it, snapshotting its files first. Edits made to a stale workspace before that update end up in a divergent commit; merge refuses until you keep one version.

After `--squash --keep`, rebase the kept workspace before continuing:

```bash
jj rebase -b @ -d main
```

Replace `main` with your merge target. To rebase without merging or cleanup, use `workmux rebase [name]`; it rebases the workspace onto its saved local base or the configured main branch.

## Cleanup

A successful merge removes the workspace directory, forgets the jj workspace, deletes its bookmark, and closes its managed terminal window unless you use `--keep`.

`workmux remove [name]` cleans up without merging. Its uncommitted-change check uses Git status, so describe and finish your working-copy commit with `jj new` before removal if needed. Use `--keep-branch` to preserve the bookmark while removing the workspace and window. `--force` bypasses confirmation, uncommitted-change protection, and bookmark advancement; use it only when you intend to discard the workspace.

## Unsupported features

- **Remote branch checkout and `--pr`**: Fetch with `jj git fetch`, then create a workspace with `workmux add feature-auth --base main@origin` instead.
- **`--with-changes`**: Create the workspace first, then move changes with `jj squash --from <revision> --into <workspace>@`.
- **`workmux rename`**: Renaming jj workspaces through workmux is not supported.
