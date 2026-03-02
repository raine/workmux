# Rust project checks

set positional-arguments
set shell := ["bash", "-euo", "pipefail", "-c"]

# List available commands
default:
    @just --list

# Run all checks via three parallel pipelines
[parallel]
check: _rust-pipeline _python-pipeline docs-check

# Run check and fail if there are uncommitted changes (for CI)
check-ci: check
    #!/usr/bin/env bash
    set -euo pipefail
    if ! git diff --quiet || ! git diff --cached --quiet; then
        echo "Error: check caused uncommitted changes"
        echo "Run 'just check' locally and commit the results"
        git diff --stat
        exit 1
    fi

# Rust: format → clippy-fix → clippy → test (sequential)
_rust-pipeline: format-rust clippy-fix clippy unit-tests

# Python: format → lint → typecheck (sequential)
_python-pipeline: format-python ruff-check pyright

# Format Rust and Python files
format: format-rust format-python

# Format Rust files
format-rust:
    cargo fmt --all

# Format Python test files
format-python:
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

# Install debug binary and dev hot-reload launcher via symlink
install-dev:
    cargo build && ln -sf $(pwd)/target/debug/workmux ~/.cargo/bin/workmux && chmod +x $(pwd)/scripts/devmux && ln -sf $(pwd)/scripts/devmux ~/.cargo/bin/devmux

# Run unit tests
unit-tests:
    cargo test --bin workmux --quiet

# Run ruff linter on Python tests
ruff-check:
    ruff check tests --fix

# Run pyright type checker on Python tests
pyright:
    #!/usr/bin/env bash
    set -euo pipefail
    source tests/venv/bin/activate
    pyright tests

# Check that all docs pages have meta descriptions
docs-check:
    #!/usr/bin/env bash
    set -euo pipefail
    missing=()
    while IFS= read -r file; do
        if ! head -20 "$file" | grep -q '^description:'; then
            missing+=("$file")
        fi
    done < <(find docs -name "*.md" -not -path "*/node_modules/*" -not -path "docs/README.md")
    if [ ${#missing[@]} -gt 0 ]; then
        echo "Missing meta description in:"
        printf '  %s\n' "${missing[@]}"
        exit 1
    fi
    echo "All docs have descriptions"

# Run the application
run *ARGS:
    cargo run -- "$@"

# Start dashboard with subsecond hot-reload dev server
dev-tui *ARGS:
    scripts/devmux "$@"

# Start a dashboard sidecar connected to local dev hotpatch server
dev-dashboard *ARGS:
    cargo run -- dev dashboard "$@"

# Run Python tests in parallel (depends on build)
test *ARGS: build
    #!/usr/bin/env bash
    set -euo pipefail
    source tests/venv/bin/activate
    export WORKMUX_TEST=1
    quiet_flag=""
    [[ -n "${CLAUDECODE:-}" ]] && quiet_flag="-q"
    if [ $# -eq 0 ]; then
        pytest tests/ -n auto $quiet_flag
    else
        pytest $quiet_flag "$@"
    fi

# Run docs dev server
docs:
    cd docs && npm install && npm run dev -- --open

# Format documentation files
format-docs:
    cd docs && npm run format

# Release a new patch version
release *ARGS:
    @just _release patch {{ARGS}}

# Internal release helper
_release bump *ARGS:
    @cargo-release {{bump}} {{ARGS}}
