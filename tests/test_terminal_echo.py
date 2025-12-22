"""Regression test for PR #17 - terminal echo preservation after pane handshake."""

from pathlib import Path

from .conftest import (
    TmuxEnvironment,
    get_window_name,
    run_workmux_add,
    wait_for_pane_output,
    write_workmux_config,
)


def test_terminal_echo_is_enabled_after_handshake(
    isolated_tmux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
):
    """
    Ensures that when workmux sets up a pane with a command (using the handshake),
    it restores terminal echo before exec-ing the shell.

    The bug: PaneHandshake runs `stty -echo` before signaling readiness, then
    execs into the shell without restoring echo. Bash doesn't auto-fix this
    (unlike zsh's ZLE), leaving typed input invisible.

    The fix: Add `stty echo` before the exec.
    """
    env = isolated_tmux_server
    branch_name = "test-echo"
    window_name = get_window_name(branch_name)

    # Force bash - zsh auto-fixes terminal state via ZLE, masking the bug
    env.tmux(["set-option", "-g", "default-shell", "/bin/bash"])

    # Pane with a command triggers PaneHandshake code path
    write_workmux_config(
        repo_path,
        panes=[{"command": "echo 'Ready'", "focus": True}],
    )

    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Wait for the command to complete - shell is now interactive
    wait_for_pane_output(env, window_name, "Ready")

    # Send text WITHOUT pressing Enter
    # If echo is disabled, this text will NOT appear in the pane capture
    marker = "__echo_check_abc123__"
    env.tmux(["send-keys", "-t", window_name, marker])

    # Verify the typed text is visible (echo is enabled)
    wait_for_pane_output(env, window_name, marker, timeout=2.0)
