# Nix Installation and Configuration

## Installation Methods

### Nix Profile

Quick installation for current user:

```bash
nix profile install github:raine/workmux
```

### Nix Shell (Temporary)

Try workmux without installing:

```bash
nix shell github:raine/workmux
workmux --help
```

### NixOS/Home Manager

Add to your `flake.nix`:

```nix
{
  inputs.workmux.url = "github:raine/workmux";

  # For NixOS system packages:
  environment.systemPackages = [ inputs.workmux.packages.${system}.default ];

  # For home-manager:
  home.packages = [ inputs.workmux.packages.${system}.default ];
}
```

## Home Manager Module (Declarative Configuration)

The workmux flake provides a home-manager module for declarative configuration of workmux.

### Basic Setup

Add the module to your home-manager configuration:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager.url = "github:nix-community/home-manager";
    workmux.url = "github:raine/workmux";
  };

  outputs = { nixpkgs, home-manager, workmux, ... }: {
    homeConfigurations.youruser = home-manager.lib.homeManagerConfiguration {
      # ... other config ...

      modules = [
        workmux.homeManagerModules.default
        {
          programs.workmux = {
            enable = true;
          };
        }
      ];
    };
  };
}
```

### Configuration Options

The module provides these options under `programs.workmux`:

#### Basic Options

- **`enable`** (boolean): Enable workmux
- **`package`** (package): The workmux package to use (default: from flake)
- **`windowPrefix`** (string): Prefix for tmux window names (default: `"wm-"`)
- **`worktreePrefix`** (string or null): Prefix for worktree directories

#### Pane Layout

- **`panes`** (list of pane configs): Define tmux pane layout
  ```nix
  panes = [
    { command = "nvim ."; focus = true; }
    { split = "horizontal"; }
  ];
  ```

#### Setup Commands

- **`postCreate`** (list of strings): Commands to run after creating a worktree
  ```nix
  postCreate = ["npm install" "cp .env.example .env"];
  ```

#### File Operations

- **`files.copy`** (list of strings): Glob patterns for files to copy
- **`files.symlink`** (list of strings): Glob patterns for files to symlink
  ```nix
  files = {
    symlink = ["node_modules" ".env"];
    copy = [".env.local"];
  };
  ```

#### AI Agent Integration

- **`agent`** (string or null): Default AI agent command
  - Supported: `"claude"`, `"codex"`, `"opencode"`, `"gemini"`
- **`statusFormat`** (boolean): Auto-configure tmux status format (default: `true`)
- **`statusIcons`**: Custom icons for agent status
  ```nix
  statusIcons = {
    working = "🤖";
    waiting = "⏳";
    done = "✅";
  };
  ```

#### Git Integration

- **`mergeStrategy`** (enum or null): Default merge strategy
  - Values: `"merge"`, `"rebase"`, `"squash"`
- **`mainBranch`** (string or null): Target branch for merging (auto-detected if null)

#### Shell Integration

- **`enableBashIntegration`** (boolean): Enable Bash completions (default: `true`)
- **`enableZshIntegration`** (boolean): Enable Zsh completions (default: `true`)
- **`enableFishIntegration`** (boolean): Enable Fish completions (default: `true`)

#### Advanced

- **`extraConfig`** (attribute set): Additional YAML config not covered by other options
  ```nix
  extraConfig = {
    pre_merge = ["npm test"];
    post_merge = ["echo 'Merged!'"];
  };
  ```

### Complete Example

```nix
{
  programs.workmux = {
    enable = true;

    # Window naming
    windowPrefix = "work-";

    # AI agent configuration
    agent = "claude";
    statusFormat = true;

    # Pane layout
    panes = [
      { command = "nvim ."; focus = true; }
      { split = "horizontal"; command = "npm run dev"; }
    ];

    # Setup automation
    postCreate = [
      "npm install"
      "cp .env.example .env"
    ];

    # File management
    files = {
      symlink = ["node_modules"];
      copy = [".env.local"];
    };

    # Git workflow
    mergeStrategy = "squash";
    mainBranch = "main";

    # Additional hooks
    extraConfig = {
      pre_merge = ["npm test" "npm run lint"];
      post_create = ["git submodule update --init"];
    };
  };
}
```

## Building from Source

Clone the repository and build:

```bash
git clone https://github.com/raine/workmux
cd workmux
nix build
./result/bin/workmux --version
```

## Development

Enter the development shell:

```bash
nix develop
cargo build
cargo test
```

## Testing

Run the integration tests:

```bash
nix build .#checks.$(nix eval --impure --raw --expr 'builtins.currentSystem').workmux-test
```

Or run all checks:

```bash
nix flake check
```
