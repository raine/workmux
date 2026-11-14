---
title: "direnv integration"
description: Automatically set up direnv in new worktrees
---

If your project uses [direnv](https://direnv.net/) for environment management, you can configure workmux to automatically set it up in new worktrees:

```yaml
# .workmux.yaml
post_create:
  - direnv allow

files:
  symlink:
    - .envrc
```

See also [Using direnv for port isolation](/guide/monorepos/#using-direnv) in monorepos.
