{
  description = "Parallel development in tmux with git worktrees";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      forAllSystems = f: nixpkgs.lib.genAttrs
        [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ]
        (system: f nixpkgs.legacyPackages.${system});
    in {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "workmux";
          version = self.shortRev or self.dirtyShortRev or "dev";
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "crossterm-0.29.0" = "sha256-rfAaqGylDaxx3bjmofifnzSh7Hmh21BzHp5fS/w2Z6I=";
            };
          };
          nativeBuildInputs = [ pkgs.installShellFiles pkgs.git ];
          postInstall = ''
            export HOME=$TMPDIR
            installShellCompletion --cmd workmux \
              --bash <($out/bin/workmux completions bash) \
              --fish <($out/bin/workmux completions fish) \
              --nushell <($out/bin/workmux completions nushell) \
              --zsh <($out/bin/workmux completions zsh)
          '';
        };
      });
    };
}
