---
title: "docs"
description: Display the README with terminal formatting
---

Displays the README with terminal formatting. Useful for quick reference without leaving the terminal.

```bash
workmux docs
```

When run interactively, renders markdown with colors and uses a pager (`less`). When piped (e.g., to an LLM), outputs raw markdown for clean context.

## Using with AI agents

You can ask an agent to read the docs and configure workmux for you:

```
> run `workmux docs` and configure workmux so that on the left pane
  there is claude as agent, and on the right side neovim and empty
  shell on top of each other

⏺ Bash(workmux docs)
  ⎿  <p align="center">
       <picture>
     … +923 lines

⏺ Write(.workmux.yaml)
  ⎿  Wrote 9 lines to .workmux.yaml

⏺ Created .workmux.yaml with the layout:
  - Left: claude agent (focused)
  - Right top: neovim
  - Right bottom: empty shell
```
