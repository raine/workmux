---
title: "Nix"
description: Installing and configuring workmux with Nix and Home Manager
---

Requires [Nix with flakes enabled](https://nixos.wiki/wiki/Flakes).

## Quick start

Run workmux without installing:

```bash
nix run github:raine/workmux -- --help
```

## Flake input

```nix
inputs.workmux.url = "github:raine/workmux";
```

## Installation

### Home Manager

Install the package and write the config file directly:

```nix
{ inputs, pkgs, ... }:

{
  home.packages = [ inputs.workmux.packages.${pkgs.system}.default ];

  xdg.configFile."workmux/config.yaml".text = ''
    merge_strategy: rebase
    agent: claude
    panes:
      - command: <agent>
        focus: true
      - split: horizontal
  '';
}
```

See [Configuration](/guide/configuration/) for all config options.

### NixOS

```nix
{ inputs, pkgs, ... }:

{
  environment.systemPackages = [
    inputs.workmux.packages.${pkgs.system}.default
  ];
}
```

## Shell completions

The flake automatically installs completions for Bash, Zsh, and Fish. Do not add the manual `eval "$(workmux completions ...)"` lines from the [Installation](/guide/installation/#shell-completions) guide.
