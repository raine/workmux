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

1. Committing any staged changes
2. Rebasing onto the base branch
3. Running `workmux merge` to merge and clean up

## Step 0: Detect VCS

Determine the VCS backend:
- If `.jj/` directory exists at or above the current directory → jj mode
- Otherwise → git mode

For jj mode, adapt the steps below:
- Step 1: Use `jj describe` instead of `git commit`
- Step 2: Use `jj rebase -d <base>` instead of `git rebase`
- For conflicts: Use `jj resolve` and inspect with `jj diff`

## Step 1: Commit

If there are staged changes, commit them. Use lowercase, imperative mood, no conventional commit prefixes. Skip if nothing is staged.

## Step 2: Rebase

Get the base branch from git config:

```
git config --local --get "branch.$(git branch --show-current).workmux-base"
```

If no base branch is configured, default to "main".

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
