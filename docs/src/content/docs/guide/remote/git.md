---
title: "Bringing work home"
description: Merge, rebase and move remote workspaces back to the local machine
---

The local machine stays the source of truth for your repository. Work goes to the server as a commit when a workspace is created, and comes back to the local machine with `merge` or `remote pull`.

## Merging

```bash
workmux merge my-feature
```

This copies the workspace's branch, staged and unstaged changes and untracked files into a local worktree, runs your normal local merge there, then removes the remote workspace, its agent and its branch. `merge_strategy`, `--rebase`, `--squash`, `--into` and `--keep` work as for a local worktree, and `pre_merge` hooks run on the local machine.

### Preparing the merge worktree

The local worktree gets your `files` and `post_create` hooks, but has no panes, so pane commands never run there. Files ignored by Git, such as `node_modules`, don't come from the server either. If `pre_merge` needs dependencies, install them in `post_create` or share them with `files`:

```yaml
post_create:
  - cd docs && bun install
pre_merge:
  - just check
```

Keep setup hooks repeatable: they run for every fresh merge attempt. If setup fails, the merge stops and the remote workspace stays.

| Flag          | Skips                                                   |
| ------------- | ------------------------------------------------------- |
| `--no-verify` | `pre_merge` hooks                                       |
| `--no-hooks`  | all hooks, including `post_create`; `files` still apply |

### Uncommitted work, conflicts and cleanup

- Unstaged changes and untracked files on the server refuse the merge unless you pass `--ignore-uncommitted` or `--keep`. With `--ignore-uncommitted`, they aren't merged and are removed with the workspace.
- On a conflict, resolve it in the local worktree and run `workmux merge my-feature` again.
- If the agent changed the workspace after its work was copied, the remote workspace is kept and the merge says so.
- If the server can't be reached after the merge, its cleanup is left pending. The next command that reaches the server finishes it without merging again.
- With `--keep` (or `merge_keep: true`), the remote workspace and agent stay, and the remote branch moves to the merged result.

## Operation logs

A merge or removal [requested from a remote pane](/guide/remote/usage/#remove-or-merge-from-a-remote-pane) runs in the background on the local machine. Its output, including hook output, is saved on the local machine:

```bash
workmux remote logs devbox                # list log files
workmux remote logs devbox r0123456789ab   # print one request's output
```

The pane prints the exact command for its request. Logs are private files under `~/.local/state/workmux/remote/<remote>/operation-logs/`, readable without a connection to the server. They may contain sensitive hook output and aren't deleted automatically. Hook output never goes to the server.

## Rebasing

```bash
workmux rebase my-feature
```

Sends the local machine's base branch to the server and rebases the workspace there, committing as your local Git identity. The agent keeps running. Conflicts stay on the server: run `workmux open my-feature` and resolve them in the remote worktree.

## Moving a workspace to the local machine

```bash
workmux remote pull my-feature
```

Stops the remote agent, brings the branch, staged and unstaged changes and untracked files home as a local worktree, opens its window and removes the remote workspace. The window runs your configured panes, so a fresh local agent starts there; the remote agent's conversation isn't carried over. Pass `--no-hooks` to skip the local `post_create` hooks.

## What doesn't come home

Merge and `remote pull` carry commits, staged and unstaged changes, and untracked files. They don't carry:

- files ignored by Git, such as build output or `.env` files edited on the server
- the contents of submodules, only the recorded commit
- agent state, such as conversations and caches

An untracked nested Git repository, an unfinished merge, rebase or cherry-pick, or a detached HEAD in the remote worktree blocks the copy until you resolve it on the server.

## Pushing

Agents on the server can't push to your Git hosting: the server has no `origin` and no credentials for it. Bring the work home with `merge` or `remote pull`, and push from the local machine.
