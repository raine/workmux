# Integration tests for workmux
#
# Run with: nix build .#checks.<system>.workmux-test
# Or: nix flake check
{
  pkgs,
  workmux,
}:
pkgs.runCommand "workmux-integration-test"
{
  buildInputs = [
    workmux
    pkgs.git
    pkgs.tmux
  ];
} ''
  set -euo pipefail

  # Set HOME to TMPDIR to allow workmux to create log directory
  export HOME=$TMPDIR

  echo "==> Testing workmux binary..."

  # Test that workmux exists and shows version
  if ! workmux --version; then
    echo "ERROR: workmux --version failed"
    exit 1
  fi

  # Test that help command works
  if ! workmux --help > /dev/null; then
    echo "ERROR: workmux --help failed"
    exit 1
  fi

  # Test that all subcommands are available
  for cmd in add open merge remove list path init claude completions; do
    if ! workmux help "$cmd" > /dev/null 2>&1; then
      echo "ERROR: workmux help $cmd failed"
      exit 1
    fi
  done

  echo "==> Testing shell completions..."

  # Verify bash completion
  if [ ! -f "${workmux}/share/bash-completion/completions/workmux.bash" ]; then
    echo "ERROR: bash completion not found"
    exit 1
  fi

  # Verify fish completion
  if [ ! -f "${workmux}/share/fish/vendor_completions.d/workmux.fish" ]; then
    echo "ERROR: fish completion not found"
    exit 1
  fi

  # Verify zsh completion
  if [ ! -f "${workmux}/share/zsh/site-functions/_workmux" ]; then
    echo "ERROR: zsh completion not found"
    exit 1
  fi

  # Test that completions can be generated
  for shell in bash fish zsh; do
    if ! workmux completions "$shell" > /dev/null; then
      echo "ERROR: workmux completions $shell failed"
      exit 1
    fi
  done

  echo "==> Testing workmux init..."

  # Test init command (generates example config)
  mkdir -p "$TMPDIR/test-init"
  cd "$TMPDIR/test-init"

  if ! workmux init; then
    echo "ERROR: workmux init failed"
    exit 1
  fi

  if [ ! -f ".workmux.yaml" ]; then
    echo "ERROR: .workmux.yaml not created by init"
    exit 1
  fi

  echo "==> All tests passed!"

  touch $out
''
