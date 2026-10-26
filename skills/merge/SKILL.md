---
name: merge
description: Commit, rebase, and merge the current branch.
disable-model-invocation: true
allowed-tools: Read, Bash, Glob, Grep
---

<!-- Customize the commit style and rebase behavior to match your workflow. -->

**Arguments:** `$ARGUMENTS`

Check the arguments for flags:

- `--keep`, `-k` → pass `--keep` to `workmux merge` (keeps the worktree and tmux window after merging)
- `--no-verify`, `-n` → pass `--no-verify` to `workmux merge`

Strip all flags from arguments.

Commit, rebase, and merge the current branch.

This command finishes work on the current branch by:

1. Committing all uncommitted changes
2. Rebasing onto the base branch
3. Running `workmux merge` to merge and clean up

## Step 1: Commit

Check for staged, unstaged, and untracked changes with `git status --porcelain`. If
there are changes, stage all of them with `git add -A`, review the staged diff,
and commit. Use lowercase, imperative mood, no conventional commit prefixes.
Skip if the working tree is clean.

## Step 2: Rebase

Get the base branch from git config:

```
git config --local --get "branch.$(git branch --show-current).workmux-base"
```

If no base branch is configured, default to "main".

If the base branch doesn't exist locally (`git rev-parse --verify --quiet
<base-branch>` prints nothing), skip this step. This is the case in a workspace
on a remote server, where `workmux merge --rebase` rebases on the local machine.

Rebase onto the local base branch (do NOT fetch from origin first):

```
git rebase <base-branch>
```

IMPORTANT: Do NOT run `git fetch`. Do NOT rebase onto `origin/<branch>`. Only rebase onto the local branch name (e.g., `git rebase main`, not `git rebase origin/main`).

If conflicts occur:

- BEFORE resolving any conflict, understand what changes were made to each
  conflicting file in the base branch
- For each conflicting file, run `git log -p -n 3 <base-branch> -- <file>` to
  see recent changes to that file in the base branch
- The goal is to preserve BOTH the changes from the base branch AND our branch's
  changes
- After resolving each conflict, stage the file and continue with
  `git rebase --continue`
- If a conflict is too complex or unclear, ask for guidance before proceeding

## Step 3: Merge

Run: `workmux merge --rebase --notification [--keep] [--no-verify]`

Include `--keep` only if the `--keep` flag was passed in arguments.
Include `--no-verify` only if the `--no-verify` flag was passed in arguments.

This will merge the branch into the base branch and clean up the worktree and
tmux window (unless `--keep` is used).
