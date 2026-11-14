---
name: rebase
description: Rebase the current branch with smart conflict resolution.
disable-model-invocation: true
allowed-tools: Read, Bash, Glob, Grep
---

<!-- Customize conflict resolution strategy to match your preferences. -->

Rebase the current branch.

Arguments: $ARGUMENTS

Behavior:

- No arguments: rebase on the current branch's workmux base branch
  (`git config branch.<current>.workmux-base`), falling back to local main when
  none is configured
- "origin": fetch origin, rebase on origin/main
- "origin/branch": fetch origin, rebase on origin/branch
- "branch": rebase on local branch (use "main" to force a rebase on local main)

Steps:

1. Check for local changes with `git status --porcelain`:
   - If the working tree has staged, unstaged, or untracked changes, stash them
     with `git stash push --include-untracked -m "workmux rebase"`.
   - Remember whether this command created a stash. Existing stash entries must
     remain untouched.
   - If stashing fails, stop before fetching or rebasing.
2. Parse arguments:
   - No args → target is the current branch's workmux base branch
     (`git config --get branch.$(git branch --show-current).workmux-base`); if
     that is empty, target is "main". No fetch.
   - Contains "/" (e.g., "origin/develop") → split into remote and branch, fetch
     remote, target is remote/branch
   - Just "origin" → fetch origin, target is "origin/main"
   - Anything else → target is that branch name, no fetch
3. If fetching, run: `git fetch <remote>`. If fetching or target resolution
   fails before the rebase begins, restore the stash created in step 1 before
   stopping.
4. Run: `git rebase <target>`
5. If conflicts occur, handle them carefully (see below)
6. Continue until rebase is complete
7. If step 1 created a stash, restore it with `git stash pop --index`:
   - Restore the stash only after the rebase succeeds.
   - If restoration conflicts, preserve the stash, report the conflicts, and
     leave the affected files for manual resolution.

Handling conflicts:

- BEFORE resolving any conflict, understand what changes were made to each
  conflicting file in the target branch
- For each conflicting file, run `git log -p -n 3 <target> -- <file>` to see
  recent changes to that file in the target branch
- The goal is to preserve BOTH the changes from the target branch AND our
  branch's changes
- After resolving each conflict, stage the file and continue with
  `git rebase --continue`
- If a conflict is too complex or unclear, ask for guidance before proceeding
