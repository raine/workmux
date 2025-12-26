{
  config,
  lib,
  pkgs,
  ...
}:
with lib; let
  cfg = config.programs.workmux;
  yamlFormat = pkgs.formats.yaml {};

  paneType = types.submodule {
    options = {
      command = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Command to run in this pane";
      };
      focus = mkOption {
        type = types.bool;
        default = false;
        description = "Whether to focus this pane";
      };
      split = mkOption {
        type = types.nullOr (types.enum ["horizontal" "vertical"]);
        default = null;
        description = "Split direction for this pane";
      };
      size = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "Size in lines/columns for this pane";
      };
      percentage = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "Percentage of space for this pane";
      };
    };
  };

  filesType = types.submodule {
    options = {
      copy = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Glob patterns for files/directories to copy into new worktrees";
        example = [".env" "node_modules"];
      };
      symlink = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Glob patterns for files/directories to symlink";
        example = ["node_modules" ".env.local"];
      };
    };
  };

  statusIconsType = types.submodule {
    options = {
      working = mkOption {
        type = types.str;
        default = "🤖";
        description = "Icon for working agent status";
      };
      waiting = mkOption {
        type = types.str;
        default = "⏳";
        description = "Icon for waiting agent status";
      };
      done = mkOption {
        type = types.str;
        default = "✅";
        description = "Icon for done agent status";
      };
    };
  };
in {
  options.programs.workmux = {
    enable = mkEnableOption "workmux - parallel development in tmux with git worktrees";

    package = mkOption {
      type = types.package;
      default = pkgs.workmux or (throw "workmux package not found in pkgs. Add the workmux flake to your inputs and overlay it.");
      defaultText = literalExpression "pkgs.workmux";
      description = "The workmux package to use";
    };

    windowPrefix = mkOption {
      type = types.str;
      default = "wm-";
      description = "Prefix for tmux window names";
      example = "work-";
    };

    worktreePrefix = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Prefix prepended to worktree directory and window names";
      example = "wt-";
    };

    worktreeDir = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Directory where worktrees are created";
      example = "..";
    };

    panes = mkOption {
      type = types.listOf paneType;
      default = [];
      example = literalExpression ''
        [
          { command = "nvim ."; focus = true; }
          { split = "horizontal"; }
        ]
      '';
      description = ''
        Pane layout configuration. Defines how tmux panes are created and what
        commands run in them when a new worktree is opened.
      '';
    };

    postCreate = mkOption {
      type = types.listOf types.str;
      default = [];
      example = ["npm install" "cp .env.example .env"];
      description = ''
        Commands executed after worktree creation but before opening the tmux window.
        Useful for setup tasks like installing dependencies or copying config files.
      '';
    };

    files = mkOption {
      type = filesType;
      default = {};
      example = literalExpression ''
        {
          symlink = ["node_modules" ".env"];
          copy = [".env.local"];
        }
      '';
      description = "File operations to perform when creating new worktrees";
    };

    agent = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "claude";
      description = ''
        Default agent command for AI-assisted development.
        Supported values: "claude", "codex", "opencode", "gemini"
      '';
    };

    mergeStrategy = mkOption {
      type = types.nullOr (types.enum ["merge" "rebase" "squash"]);
      default = null;
      example = "squash";
      description = "Default merge strategy when merging branches";
    };

    mainBranch = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "main";
      description = ''
        Target branch for merging. If not specified, workmux will auto-detect
        by looking for common branch names (main, master, develop).
      '';
    };

    statusFormat = mkOption {
      type = types.bool;
      default = true;
      description = "Auto-configure tmux status format to show agent status";
    };

    statusIcons = mkOption {
      type = statusIconsType;
      default = {};
      example = literalExpression ''
        {
          working = "⚙️";
          waiting = "⏸️";
          done = "✓";
        }
      '';
      description = "Custom emoji/icons for agent status display in tmux";
    };

    extraConfig = mkOption {
      type = types.attrs;
      default = {};
      example = literalExpression ''
        {
          pre_merge = ["npm test"];
          post_merge = ["echo 'Merged successfully'"];
        }
      '';
      description = ''
        Additional configuration options to merge into config.yaml.
        Use this for options not directly exposed as module options.
      '';
    };

    enableBashIntegration = mkOption {
      type = types.bool;
      default = true;
      description = "Whether to enable Bash integration (shell completions)";
    };

    enableZshIntegration = mkOption {
      type = types.bool;
      default = true;
      description = "Whether to enable Zsh integration (shell completions)";
    };

    enableFishIntegration = mkOption {
      type = types.bool;
      default = true;
      description = "Whether to enable Fish integration (shell completions)";
    };
  };

  config = mkIf cfg.enable {
    home.packages = [cfg.package];

    # Generate config file if any non-default options are set
    xdg.configFile."workmux/config.yaml" = mkIf (
      cfg.windowPrefix != "wm-"
      || cfg.worktreePrefix != null
      || cfg.worktreeDir != null
      || cfg.panes != []
      || cfg.postCreate != []
      || cfg.files.copy != []
      || cfg.files.symlink != []
      || cfg.agent != null
      || cfg.mergeStrategy != null
      || cfg.mainBranch != null
      || cfg.statusFormat != true
      || cfg.statusIcons != {}
      || cfg.extraConfig != {}
    ) {
      source = let
        configData = filterAttrs (n: v: v != null && v != {} && v != []) {
          window_prefix = cfg.windowPrefix;
          worktree_prefix = cfg.worktreePrefix;
          worktree_dir = cfg.worktreeDir;
          panes =
            if cfg.panes != []
            then map (pane: filterAttrs (n: v: v != null && v != false) pane) cfg.panes
            else null;
          post_create = if cfg.postCreate != [] then cfg.postCreate else null;
          files =
            if (cfg.files.copy != [] || cfg.files.symlink != [])
            then filterAttrs (n: v: v != []) cfg.files
            else null;
          agent = cfg.agent;
          merge_strategy = cfg.mergeStrategy;
          main_branch = cfg.mainBranch;
          status_format = cfg.statusFormat;
          status_icons = if cfg.statusIcons != {} then cfg.statusIcons else null;
        };
      in
        yamlFormat.generate "workmux-config" (configData // cfg.extraConfig);
    };

    # Shell completions
    programs.bash.initExtra = mkIf cfg.enableBashIntegration ''
      # workmux shell completion
      if command -v workmux &> /dev/null; then
        eval "$(workmux completions bash)"
      fi
    '';

    programs.zsh.initExtra = mkIf cfg.enableZshIntegration ''
      # workmux shell completion
      if command -v workmux &> /dev/null; then
        eval "$(workmux completions zsh)"
      fi
    '';

    programs.fish.interactiveShellInit = mkIf cfg.enableFishIntegration ''
      # workmux shell completion
      if command -v workmux &> /dev/null
        workmux completions fish | source
      end
    '';
  };
}
