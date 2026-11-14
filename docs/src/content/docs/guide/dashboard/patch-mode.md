---
title: "Patch mode"
description: Selectively stage individual hunks like git add -p
---

Patch mode (`a` from WIP diff) allows staging individual hunks like `git add -p`. This is useful for selectively staging parts of an agent's work.

When [delta](https://github.com/dandavison/delta) is installed, hunks are rendered with syntax highlighting for better readability.

## Keybindings

| Key       | Action                           |
| --------- | -------------------------------- |
| `y`       | Stage current hunk               |
| `n`       | Skip current hunk                |
| `u`       | Undo last staged hunk            |
| `s`       | Split hunk (if splittable)       |
| `o`       | Comment on hunk (sends to agent) |
| `j`/`k`   | Navigate to next/previous hunk   |
| `:`       | Open command palette             |
| `q`/`Esc` | Exit patch mode                  |
| `Ctrl+c`  | Quit dashboard                   |

## Staging hunks

Press `y` to stage the current hunk (adds it to the git index) and advance to the next. Press `n` to skip without staging. The counter in the header shows your progress through all hunks (e.g., `[3/10]`).

After staging or skipping all hunks, the diff refreshes to show any remaining unstaged changes.

## Splitting hunks

Press `s` to split the current hunk into smaller pieces. This works when there are context lines (unchanged lines) between separate changes within a hunk. If the hunk cannot be split further, nothing happens.

## Undo

Press `u` to undo the last staged hunk. This uses `git apply --cached --reverse` to unstage it. You can undo multiple times to unstage several hunks.

## Commenting on hunks

Press `o` to enter comment mode. Type your message and press `Enter` to send it to the agent. The comment includes:

- File path and line number
- The diff hunk as context (in a code block)
- Your comment text

Press `Esc` to cancel without sending.

This is useful for giving the agent feedback about specific changes, like "This function should handle the error case" or "Can you add a test for this?"
