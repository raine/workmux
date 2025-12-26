{
  description = "Parallel development in tmux with git worktrees";

  inputs = {
    # Using nixos-unstable for latest Rust toolchain and dependencies.
    # While nixpkgs 25.11+ has Rust 1.91.1 (sufficient for workmux's requirements),
    # unstable is preferred for standalone tool flakes to get faster updates and
    # maintain compatibility with the latest Rust ecosystem.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    {
      # Home Manager module
      homeManagerModules.default = import ./nix/home-manager-module.nix;
      homeManagerModules.workmux = self.homeManagerModules.default;
    }
    // flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
      in {
        packages = {
          default = self.packages.${system}.workmux;

          workmux = pkgs.rustPlatform.buildRustPackage {
            pname = "workmux";
            version = self.shortRev or self.dirtyShortRev or "dev";

            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = with pkgs; [
              pkg-config
              installShellFiles
            ];

            postInstall = ''
              # Set HOME to avoid log directory creation errors during completion generation
              export HOME=$TMPDIR
              installShellCompletion --cmd workmux \
                --bash <($out/bin/workmux completions bash) \
                --fish <($out/bin/workmux completions fish) \
                --zsh <($out/bin/workmux completions zsh)
            '';

            meta = with pkgs.lib; {
              description = "Parallel development in tmux with git worktrees";
              longDescription = ''
                Workmux combines git worktrees with tmux window management to streamline
                parallel development. It creates isolated workspaces where each branch
                gets its own directory and tmux window, eliminating friction when juggling
                multiple features simultaneously.
              '';
              homepage = "https://github.com/raine/workmux";
              license = licenses.mit;
              maintainers = [];
              mainProgram = "workmux";
            };
          };
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            cargo
            rustc
            rust-analyzer
            rustfmt
            clippy
            pkg-config
          ];

          RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
        };

        # Integration tests
        checks.workmux-test = pkgs.callPackage ./nix/test.nix {
          workmux = self.packages.${system}.workmux;
        };

        # Formatter for nix files
        formatter = pkgs.alejandra;
      }
    );
}
