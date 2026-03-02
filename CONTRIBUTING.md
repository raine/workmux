# Contributing to workmux

Thanks for your interest in contributing! This guide covers development setup,
testing, and the PR process.

## Prerequisites

- Rust toolchain (stable)
- Python 3.10+ with a virtual environment at `tests/venv/`
- [just](https://github.com/casey/just) command runner
- tmux (required for tests)
- WezTerm (optional, for testing WezTerm backend)
- Zellij (optional, for testing Zellij backend)

## Development setup

```bash
# Clone and build
git clone https://github.com/raine/workmux.git
cd workmux
cargo build

# Set up Python test environment
python -m venv tests/venv
source tests/venv/bin/activate
pip install -r tests/requirements.txt

# Install dev binaries (workmux + devmux symlinks to ~/.cargo/bin)
just install-dev

# Optional: install Dioxus CLI used by subsecond hotpatch dev mode
cargo install dioxus-cli

# Start dashboard in hot-reload mode
workmux dev tui

# Optional: run a second dashboard process that connects to the same dev hotpatch server
workmux dev dashboard

# Or use the wrapper recipe for `workmux dev tui`
just dev-tui
```

## Running tests

Tests are written in Python using pytest and run against an isolated multiplexer
environment.

```bash
# Run all tests (tmux backend, parallel)
just test

# Run specific test file
just test tests/test_workmux_add/test_basic.py

# Run with verbose output (shows backend in test names)
just test tests/test_agent_state.py -vvv
```

### Testing different backends

By default, tests run against **tmux only**.

```bash
# Test with WezTerm (requires WezTerm to be running)
WORKMUX_TEST_BACKEND=wezterm just test

# Test both backends
just test --backend=tmux,wezterm

# Alternative: use pytest directly
just test --backend=wezterm tests/test_agent_state.py -vvv
```

When running with `-vvv`, test names show the backend:

```
test_state_file_has_correct_fields[wezterm] PASSED
test_state_file_has_correct_fields[tmux] PASSED
```

**Note:** WezTerm tests are slower due to GUI mux-server overhead and worker
throttling (8 workers vs unlimited for tmux).

### Marking tmux-only tests

Some tests only make sense for tmux. Use the marker:

```python
@pytest.mark.tmux_only
def test_tmux_specific_feature():
    ...
```

## Code quality

```bash
# Run all checks (format, clippy, build, tests)
just check

# Individual commands
just format      # Format Rust and Python
just clippy      # Lint Rust code
just ruff-check  # Lint Python tests
just pyright     # Type check Python tests
```

## Pull request guidelines

1. **Discuss first**: For large or complex changes, open an issue or discussion
   before starting work.

2. **Keep PRs focused**: One feature or fix per PR. Smaller PRs are easier to
   review.

3. **Run checks locally**: Before pushing, run `just check` to catch issues
   early.

4. **Test multiple backends**: If your change affects multiplexer interaction,
   test with other backends:

   ```bash
   just test --backend=tmux,wezterm
   ```

5. **Update docs**: If adding features, update relevant documentation in `docs/`
   or `README.md`.
