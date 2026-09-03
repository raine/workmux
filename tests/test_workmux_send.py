"""
Tests for `workmux send` command.

Tests error paths and argument validation. Happy-path tests (sending text to
a live agent pane) require a reconciled agent with matching backend/instance,
which is set up via set-window-status.
"""

from pathlib import Path

import pytest

from .conftest import (
    MuxEnvironment,
    poll_until,
    run_workmux_add,
    run_workmux_command,
    write_workmux_config,
)
from .support.agent_state import start_active_agent


def test_send_error_worktree_not_found(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Send fails with helpful error when worktree doesn't exist."""
    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "send nonexistent hello",
        expect_fail=True,
    )
    assert result.exit_code != 0


def test_send_error_no_agent_in_worktree(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Send fails when worktree exists but no agent is running."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-no-agent")

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "send feature-no-agent hello",
        expect_fail=True,
    )
    assert "No agent running" in result.stderr


def test_send_error_text_and_file_conflict(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Send fails when both text and --file are provided (clap conflict)."""
    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "send some-wt hello --file /tmp/foo.txt",
        expect_fail=True,
    )
    assert result.exit_code != 0


def test_send_inline_text_to_agent(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Send delivers inline text to a running agent's pane."""
    env = mux_server
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-send-text",
        status="waiting",
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "send feature-send-text hello-from-send",
    )
    assert result.exit_code == 0


def test_send_from_file_to_agent(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Send delivers file content to a running agent's pane."""
    env = mux_server
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-send-file",
        status="waiting",
    )

    prompt_file = Path("/tmp/wm_prompt.txt")
    prompt_file.write_text("hello-from-file\n")

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"send feature-send-file --file {prompt_file}",
    )
    assert result.exit_code == 0


@pytest.mark.tmux_only
def test_send_codex_prompt_uses_bracketed_paste(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    tmp_path: Path,
):
    """Codex prompts submit through bracketed paste instead of a typed burst."""
    env = mux_server
    command_window = env.get_current_window()
    assert command_window is not None
    agent = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-send-codex",
        status="waiting",
    )
    write_workmux_config(mux_repo_path, agent="codex", panes=[{"focus": True}])

    ready_file = tmp_path / "codex-simulator-ready"
    submitted_file = tmp_path / "codex-simulator-submitted"
    input_file = tmp_path / "codex-simulator-input"
    simulator = tmp_path / "codex_simulator.py"
    simulator.write_text(
        """
import os
from pathlib import Path
import subprocess
import sys
import termios
import time
import tty

workmux, ready_path, submitted_path, input_path = sys.argv[1:]
subprocess.run([workmux, "set-window-status", "waiting"], check=True)
fd = sys.stdin.fileno()
old_settings = termios.tcgetattr(fd)
tty.setraw(fd)
os.write(sys.stdout.fileno(), b"\\x1b[?2004h")
time.sleep(0.1)
Path(ready_path).touch()

begin = b"\\x1b[200~"
end = b"\\x1b[201~"
buffer = bytearray()
prompt = bytearray()
in_paste = False
paste_completed = False
try:
    while True:
        received = os.read(fd, 4096)
        with Path(input_path).open("ab") as input_log:
            input_log.write(received)
        buffer.extend(received)
        while buffer:
            if (len(buffer) < len(begin) and begin.startswith(buffer)) or (
                len(buffer) < len(end) and end.startswith(buffer)
            ):
                break
            if buffer.startswith(begin):
                del buffer[:len(begin)]
                in_paste = True
                continue
            if buffer.startswith(end):
                del buffer[:len(end)]
                in_paste = False
                paste_completed = True
                continue
            byte = buffer.pop(0)
            if byte == 13:
                if paste_completed and not in_paste:
                    Path(submitted_path).write_bytes(prompt)
                    raise SystemExit(0)
                prompt.append(10)
            else:
                prompt.append(byte)
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old_settings)
""".strip()
        + "\n"
    )

    env.send_keys(
        agent.window,
        f"exec python3 {simulator} {workmux_exe_path} {ready_file} {submitted_file} {input_file}",
    )
    assert poll_until(ready_file.exists), "Codex simulator did not become ready"
    env.select_window(command_window)

    prompt = "continue with deterministic delivery"
    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"send feature-send-codex '{prompt}'",
    )

    assert result.exit_code == 0
    assert poll_until(submitted_file.exists), (
        f"prompt remained in the simulated composer; input={input_file.read_bytes()!r}"
    )
    assert submitted_file.read_text() == prompt
