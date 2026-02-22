---
description: A workflow tool for managing git worktrees and tmux windows as isolated development environments for AI agents
---

# What is workmux?

workmux is a giga opinionated zero-friction workflow tool for managing [git worktrees](https://git-scm.com/docs/git-worktree) (or [jj workspaces](https://jj-vcs.github.io/jj/latest/working-copy/#workspaces)) and tmux windows as isolated development environments. Also supports [kitty](/guide/kitty), [WezTerm](/guide/wezterm), and [Zellij](/guide/zellij) (experimental). Perfect for running multiple AI agents in parallel without conflict.

**Philosophy:** Do one thing well, then compose. Your terminal handles windowing and layout, git handles branches and worktrees, your agent executes, and workmux ties it all together.

::: tip New to workmux?
Read the [introduction blog post](https://raine.dev/blog/introduction-to-workmux/) for a quick overview.
:::

## Why workmux?

**Parallel workflows.** Work on multiple features or hotfixes at the same time,
each with its own AI agent. No stashing, no branch switching, no conflicts.

**One window per task.** A natural mental model. Each has its own terminal
state, editor session, dev server, and AI agent. Context switching is switching tabs.

**Automated setup.** New worktrees start broken (no `.env`, no `node_modules`, no dev server). workmux can copy config files, symlink dependencies, and run install commands on creation. Configure once, reuse everywhere.

**One-command cleanup.** `workmux merge` handles the full lifecycle: merge the branch, delete the worktree, close the tmux window, remove the local branch. Or go next level and use the [`/merge` skill](/guide/skills#merge) to let your agent commit, rebase, and merge autonomously.

**Terminal workflow.** Build on your familiar terminal setup instead of yet another agentic GUI that won't exist next year. If you don't have one yet, [tmux is worth picking up](https://raine.dev/blog/my-tmux-setup/). Also supports [Kitty](/guide/kitty), [WezTerm](/guide/wezterm), and [Zellij](/guide/zellij).

<div class="terminal-window">
  <div class="terminal-header">
    <div class="window-controls">
      <span class="control red"></span>
      <span class="control yellow"></span>
      <span class="control green"></span>
    </div>
    <div class="window-title">Terminal</div>
  </div>
  <div class="screenshot-container">
    <img src="/tmux-screenshot.webp" alt="tmux with multiple worktrees">
    <span class="callout callout-worktrees">Worktrees</span>
  </div>
</div>

<style>
.terminal-window {
  background: #1e1e1e;
  border-radius: 10px;
  box-shadow: 0 20px 50px -10px rgba(0,0,0,0.3), 0 0 0 1px rgba(255,255,255,0.1);
  overflow: hidden;
  margin: 1.5rem 0;
}
.terminal-header {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 28px;
  background: #2d2d2d;
  position: relative;
}
.window-controls {
  position: absolute;
  left: 10px;
  display: flex;
  gap: 6px;
}
.control {
  width: 10px;
  height: 10px;
  border-radius: 50%;
}
.control.red { background-color: #ff5f56; }
.control.yellow { background-color: #ffbd2e; }
.control.green { background-color: #27c93f; }
.window-title {
  font-family: var(--vp-font-family-mono);
  font-size: 0.75rem;
  color: rgba(255, 255, 255, 0.4);
}
.screenshot-container {
  position: relative;
}
.screenshot-container img {
  display: block;
  width: 100%;
}
.callout {
  position: absolute;
  font-size: 0.75rem;
  font-weight: 600;
  color: #fff;
  background: rgba(0, 0, 0, 0.7);
  padding: 0.125rem 0.5rem;
  border-radius: 8px;
  border: 1px solid rgba(255, 255, 255, 0.2);
  pointer-events: none;
  white-space: nowrap;
}
.callout-worktrees {
  bottom: 8%;
  left: calc(50% - 3px);
  transform: translateX(-50%);
}
.callout-worktrees::before {
  content: '';
  position: absolute;
  top: 100%;
  left: 50%;
  transform: translateX(-50%);
  border: 7px solid transparent;
  border-top-color: rgba(255, 255, 255, 0.2);
}
.callout-worktrees::after {
  content: '';
  position: absolute;
  top: 100%;
  left: 50%;
  transform: translateX(-50%);
  border: 6px solid transparent;
  border-top-color: rgba(0, 0, 0, 0.7);
}
</style>

## Features

- Create worktrees (git) or workspaces (jj) with matching tmux windows (or kitty/WezTerm/Zellij tabs) in a single command (`add`)
- Merge branches and clean up everything (worktree, tmux window, branches) in one command (`merge`)
- [Dashboard](/guide/dashboard/) for monitoring agents, reviewing changes, and sending commands
- [Delegate tasks to worktree agents](/guide/skills#-worktree) with a `/worktree` skill
- [Display Claude agent status in tmux window names](/guide/status-tracking)
- Automatically set up your preferred tmux pane layout (editor, shell, watchers, etc.)
- Run post-creation hooks (install dependencies, setup database, etc.)
- Copy or symlink configuration files (`.env`, `node_modules`) into new worktrees
- [Sandbox agents](/guide/sandbox/) in containers or VMs for enhanced security
- [Automatic branch name generation](/reference/commands/add#automatic-branch-name-generation) from prompts using LLM
- Shell completions

## Before and after

workmux turns a multi-step manual workflow into simple commands, making parallel development workflows practical.

### Without workmux

```bash
# 1. Manually create the worktree and environment
git worktree add ../worktrees/user-auth -b user-auth
cd ../worktrees/user-auth
cp ../../project/.env.example .env
ln -s ../../project/node_modules .
npm install
# ... and other setup steps

# 2. Manually create and configure the tmux window
tmux new-window -n user-auth
tmux split-window -h 'npm run dev'
tmux send-keys -t 0 'claude' C-m
# ... repeat for every pane in your desired layout

# 3. When done, manually merge and clean everything up
cd ../../project
git switch main && git pull
git merge --no-ff user-auth
tmux kill-window -t user-auth
git worktree remove ../worktrees/user-auth
git branch -d user-auth
```

### With workmux

```bash
# Create the environment
workmux add user-auth

# ... work on the feature ...

# Merge and clean up
workmux merge
```

## Why git worktrees?

[Git worktrees](https://git-scm.com/docs/git-worktree) let you have multiple branches checked out at once in the same repository, each in a separate directory. This provides two main advantages over a standard single-directory setup:

- **Painless context switching**: Switch between tasks just by changing directories (`cd ../other-branch`). There's no need to `git stash` or make temporary commits. Your work-in-progress, editor state, and command history remain isolated and intact for each branch.

- **True parallel development**: Work on multiple branches simultaneously without interference. You can run builds, install dependencies (`npm install`), or run tests in one worktree while actively coding in another. This isolation is perfect for running multiple AI agents in parallel on different tasks.

In a standard Git setup, switching branches disrupts your flow by requiring a clean working tree. Worktrees remove this friction. `workmux` automates the entire process and pairs each worktree with a dedicated tmux window, creating fully isolated development environments.

**Using jj?** workmux also supports [jj (Jujutsu)](https://jj-vcs.github.io/jj/) natively. jj workspaces provide the same parallel development benefits. workmux auto-detects your VCS backend.

## Requirements

- Git 2.5+ (for worktree support) or [jj](https://jj-vcs.github.io/jj/) (Jujutsu)
- tmux (or [WezTerm](/guide/wezterm), [kitty](/guide/kitty), or [Zellij](/guide/zellij))

## Inspiration and related tools

workmux is inspired by [wtp](https://github.com/satococoa/wtp), an excellent git worktree management tool. While wtp streamlines worktree creation and setup, workmux takes this further by tightly coupling worktrees with tmux window management.

For managing multiple AI agents in parallel, tools like [claude-squad](https://github.com/smtg-ai/claude-squad) and [vibe-kanban](https://github.com/BloopAI/vibe-kanban/) offer dedicated interfaces, like a TUI or kanban board. In contrast, workmux adheres to its philosophy that **tmux is the interface**, providing a native tmux experience for managing parallel workflows without requiring a separate interface to learn.

## Related projects

- [tmux-tools](https://github.com/raine/tmux-tools) — Collection of tmux utilities including file picker, smart sessions, and more
- [tmux-file-picker](https://github.com/raine/tmux-file-picker) — Pop up fzf in tmux to quickly insert file paths, perfect for AI coding assistants
- [tmux-bro](https://github.com/raine/tmux-bro) — Smart tmux session manager that sets up project-specific sessions automatically
- [claude-history](https://github.com/raine/claude-history) — Search and view Claude Code conversation history with fzf
- [consult-llm-mcp](https://github.com/raine/consult-llm-mcp) — MCP server that lets Claude Code consult stronger AI models (o3, Gemini, GPT-5.1 Codex)
