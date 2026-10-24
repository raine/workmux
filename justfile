# Rust project checks

set positional-arguments
set shell := ["bash", "-euo", "pipefail", "-c"]

# List available commands
default:
    @just --list

# Run project checks through checkle
check:
    checkle run all

# Run check and fail if there are uncommitted changes for CI
check-ci: check
    #!/usr/bin/env bash
    set -euo pipefail
    if ! git diff --quiet || ! git diff --cached --quiet; then
        echo "Error: check caused uncommitted changes"
        echo "Run 'just check' locally and commit the results"
        git diff --stat
        exit 1
    fi

# Install shims into the Git hooks directory
install-hooks:
    scripts/install-git-hook-shims

# Check Rust and Python formatting through checkle
format: format-rust format-python

# Check Rust formatting through checkle
format-rust:
    checkle run format-rust-check

# Check Python formatting through checkle
format-python:
    checkle run format-python-check

# Check clippy through checkle
clippy:
    checkle run clippy

# Check the build through checkle
build:
    checkle --label build --mode cargo -- cargo build --all --message-format=json

# Install release binary globally from local source
install:
    cargo install --offline --path . --locked

# Install release binary globally from GitHub Actions
install-ci:
    scripts/install-ci

# Install release binary globally from GitHub releases
install-release:
    #!/usr/bin/env bash
    set -euo pipefail
    install_root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}"
    WORKMUX_INSTALL_DIR="$install_root/bin" bash scripts/install.sh

# Install debug binary globally via symlink
install-dev:
    cargo build && ln -sf $(pwd)/target/debug/workmux ~/.cargo/bin/workmux

# Build this tree on a Linux remote for `remotes.<name>.binary` (HANDLE: ~/src/HANDLE)
remote-dev-binary host handle="dev":
    scripts/remote-dev-binary "$1" "$2"

# Run unit tests through checkle
unit-tests:
    checkle run unit-tests

# Check Python tests with ruff through checkle
ruff-check:
    checkle run ruff-check

# Check Python tests with pyright through checkle
pyright:
    checkle run pyright

# Check docs pages through checkle
docs-check:
    checkle run docs-check

# Run the application
run *ARGS:
    cargo run -- "$@"

# Build and open the dashboard in a running CuaBot session
cua-dashboard session:
    scripts/cua-dashboard '{{session}}'

# Run integration tests in parallel
itest *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --all
    source tests/venv/bin/activate
    export WORKMUX_TEST=1
    quiet_flag=""
    [[ -n "${CLAUDECODE:-}" ]] && quiet_flag="-q"
    args=("$@")
    if [ $# -eq 0 ]; then
        args=(tests/ -n auto)
    fi
    pytest $quiet_flag "${args[@]}"

# Run unit tests by default, pass args to integration tests
test *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ $# -eq 0 ]; then
        checkle run unit-tests
    else
        just itest "$@"
    fi

# Run docs dev server
docs:
    cd docs && bun install && bun run dev -- --open

# Build documentation
docs-build:
    cd docs && bun install --frozen-lockfile && bun run build

# Format documentation files
format-docs:
    cd docs && bun run format

# Release a new patch version
release *ARGS:
    @just _release patch {{ARGS}}

# Internal release helper
_release bump *ARGS:
    @cargo-release --skip-publish {{bump}} {{ARGS}}
