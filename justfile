# Rust project checks

set positional-arguments
set shell := ["bash", "-euo", "pipefail", "-c"]

# List available commands
default:
    @just --list

# Run format, clippy-fix, build, unit tests, and integration tests
check: parallel-checks clippy test

# Run format, clippy-fix, build, unit tests, ruff, and pyright in parallel
[parallel]
parallel-checks: format clippy-fix build unit-tests ruff-check pyright

# Format Rust and Python files
format:
    cargo fmt --all
    ruff format tests --quiet

# Run clippy and fail on any warnings
clippy:
    cargo clippy -- -D clippy::all

# Auto-fix clippy warnings
clippy-fix:
    cargo clippy --fix --allow-dirty -- -W clippy::all

# Build the project
build:
    cargo build --all

# Run unit tests
unit-tests:
    cargo test --bin workmux

# Run ruff linter on Python tests
ruff-check:
    ruff check tests --fix

# Run pyright type checker on Python tests
pyright:
    #!/usr/bin/env bash
    set -euo pipefail
    source tests/venv/bin/activate
    pyright tests

# Run the application
run *ARGS:
    cargo run -- "$@"

# Run Python tests in parallel (depends on build)
test *ARGS: build
    #!/usr/bin/env bash
    set -euo pipefail
    source tests/venv/bin/activate
    if [ $# -eq 0 ]; then
        pytest tests/ -n auto
    else
        pytest "$@"
    fi

# Release a new patch version
release-patch:
    @just _release patch

# Internal release helper
_release bump:
    @python3 scripts/release.py {{bump}}
