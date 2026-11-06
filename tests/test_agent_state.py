"""
Tests for agent state management.

Verifies that:
1. workmux set-window-status creates agent state files
2. State files contain correct fields (pane_key, pane_pid, command, workdir, status)
3. State files contain pane identification info needed for reconciliation
4. Status updates overwrite existing state files (no duplicates)

Note: Reconciliation behavior (removing stale state files) is tested implicitly
through the dashboard TUI, which calls load_reconciled_agents(). These tests
verify the state files have the fields needed for reconciliation to work.
"""

import fcntl
import json
import os
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

import pytest

from .conftest import (
    MuxEnvironment,
    TmuxEnvironment,
    get_window_name,
    get_worktree_path,
    make_env_script,
    poll_until,
    run_workmux_add,
    wait_for_window_ready,
    write_workmux_config,
)
from .support.agent_state import (
    build_status_cmd,
    build_status_cmd_with_marker,
    get_agents_dir,
    list_agent_state_files,
    read_agent_state,
    start_active_agent,
)


# -----------------------------------------------------------------------------
# Tests
# -----------------------------------------------------------------------------


def test_set_window_status_creates_state_file(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies that workmux set-window-status creates an agent state file."""
    env = mux_server
    branch_name = "feature-state-test"
    window_name = get_window_name(branch_name)

    # Configure with a pane that starts a shell (no blocking command)
    # so we can send keys to it
    write_workmux_config(
        mux_repo_path,
        panes=[
            {"focus": True},  # Just a shell, no command
        ],
    )

    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    # Wait for window/shell to be ready
    wait_for_window_ready(env, window_name)

    # Send set-window-status command to the pane using tab title
    # This simulates what Claude hooks do when agent starts working
    status_cmd = build_status_cmd(env, workmux_exe_path, "working")
    env.send_keys(window_name, status_cmd)

    # Wait for state file to be created
    def state_file_exists():
        files = list_agent_state_files(env)
        return len(files) > 0

    assert poll_until(state_file_exists, timeout=5.0), (
        f"No agent state file created after set-window-status. "
        f"State dir: {get_agents_dir(env)}"
    )


def test_register_agent_creates_statusless_state(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature-register-agent-test"
    window_name = get_window_name(branch_name)

    write_workmux_config(mux_repo_path, panes=[{"focus": True}])
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)

    env.send_keys(window_name, build_status_cmd(env, workmux_exe_path, "working"))
    assert poll_until(lambda: len(list_agent_state_files(env)) == 1, timeout=5.0)

    register_cmd = make_env_script(
        env,
        f"{shlex.quote(str(workmux_exe_path))} register-agent",
        {"XDG_STATE_HOME": env.env["XDG_STATE_HOME"]},
    )
    registered_after = int(time.time())
    env.send_keys(window_name, register_cmd)

    def registration_is_persisted():
        files = list_agent_state_files(env)
        if len(files) != 1:
            return False
        state = read_agent_state(files[0])
        return (
            state["status"] is None
            and state["status_ts"] is None
            and state["activity_ts"] >= registered_after
        )

    assert poll_until(registration_is_persisted, timeout=5.0)


def test_set_window_status_disabled_by_env(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature-disabled-status-test"
    window_name = get_window_name(branch_name)

    write_workmux_config(
        mux_repo_path,
        panes=[
            {"focus": True},
        ],
    )

    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)

    marker_path = env.tmp_path / "disabled-status-finished"
    status_cmd = build_status_cmd_with_marker(
        env,
        workmux_exe_path,
        "working",
        marker_path,
        {"WORKMUX_DISABLE_SET_WINDOW_STATUS": "1"},
    )
    env.send_keys(window_name, status_cmd)

    assert poll_until(lambda: marker_path.exists(), timeout=5.0)
    assert list_agent_state_files(env) == []


@pytest.mark.tmux_only
def test_done_status_acknowledges_focused_pane_and_waits_for_background_focus(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature-focused-done-status"
    window_name = get_window_name(branch_name)
    initial_window_id = env.mux_command(
        ["display-message", "-p", "#{window_id}"]
    ).stdout.strip()

    write_workmux_config(mux_repo_path, panes=[{"focus": True}])
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)
    pane_id = env.mux_command(
        ["display-message", "-p", "-t", window_name, "#{pane_id}"]
    ).stdout.strip()
    env.mux_command(["set-option", "-g", "focus-events", "on"])

    client = subprocess.Popen(
        [
            "tmux",
            "-S",
            str(env.socket_path),
            "-C",
            "attach-session",
            "-t",
            "test",
        ],
        env=env.env,
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    def window_status() -> str:
        return env.mux_command(
            ["show-options", "-wqv", "-t", pane_id, "@workmux_status"],
            check=False,
        ).stdout.strip()

    def pane_status() -> str:
        return env.mux_command(
            ["show-options", "-pqv", "-t", pane_id, "@workmux_pane_status"],
            check=False,
        ).stdout.strip()

    try:
        assert poll_until(
            lambda: (
                env.mux_command(
                    ["display-message", "-p", "-t", window_name, "#{session_attached}"]
                ).stdout.strip()
                == "1"
            ),
            timeout=5.0,
        )
        env.select_window(window_name)

        focused_marker = env.tmp_path / "focused-done-finished"
        env.send_keys(
            window_name,
            build_status_cmd_with_marker(env, workmux_exe_path, "done", focused_marker),
        )
        assert poll_until(lambda: focused_marker.exists(), timeout=5.0)
        assert poll_until(
            lambda: window_status() == "" and pane_status() == "", timeout=5.0
        )

        env.mux_command(["select-window", "-t", initial_window_id])
        background_marker = env.tmp_path / "background-done-finished"
        env.send_keys(
            window_name,
            build_status_cmd_with_marker(
                env, workmux_exe_path, "done", background_marker
            ),
        )
        assert poll_until(lambda: background_marker.exists(), timeout=5.0)
        assert window_status() != ""
        assert pane_status() != ""

        env.select_window(window_name)
        assert poll_until(
            lambda: window_status() == "" and pane_status() == "", timeout=5.0
        )
    finally:
        client.terminate()
        client.wait(timeout=5)


@pytest.mark.tmux_only
def test_set_window_status_without_tmux_env_uses_process_ancestry(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """A hook descending from a pane can recover its exact pane without tmux env."""
    env = mux_server
    branch_name = "feature-status-no-tmux-env"
    window_name = get_window_name(branch_name)

    write_workmux_config(
        mux_repo_path,
        panes=[
            {"focus": True},
        ],
    )

    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)

    real_tmux = shutil.which("tmux", path=os.environ.get("PATH", ""))
    assert real_tmux is not None, "tmux binary not found"
    tmux_wrapper = env.fake_bin_dir / "tmux"
    env.install_script(
        tmux_wrapper,
        "#!/bin/sh\n"
        f'exec {shlex.quote(real_tmux)} -S {shlex.quote(str(env.socket_path))} "$@"\n',
    )

    marker_path = env.tmp_path / "status-no-tmux-env-finished"
    release_fifo = env.tmp_path / "status-no-tmux-env-release"
    os.mkfifo(release_fifo)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    hook_input = json.dumps({"session_id": "session-without-tmux-env"})
    command = (
        "unset TMUX TMUX_PANE; "
        f"cd {shlex.quote(str(worktree_path))} && "
        f"printf %s {shlex.quote(hook_input)} | "
        f"{shlex.quote(str(workmux_exe_path))} set-window-status working; "
        f"touch {shlex.quote(str(marker_path))}; "
        f"read _ < {shlex.quote(str(release_fifo))}"
    )
    status_cmd = make_env_script(
        env,
        command,
        {
            "HOME": str(env.home_path),
            "PATH": env.env["PATH"],
            "XDG_STATE_HOME": env.env["XDG_STATE_HOME"],
            "WORKMUX_BACKEND": "tmux",
        },
    )
    env.send_keys(window_name, status_cmd)

    assert poll_until(lambda: marker_path.exists(), timeout=5.0)
    assert poll_until(lambda: len(list_agent_state_files(env)) > 0, timeout=5.0), (
        "State file not created without TMUX/TMUX_PANE"
    )

    state = read_agent_state(list_agent_state_files(env)[0])
    assert state["pane_key"]["backend"] == "tmux"
    assert state["pane_key"]["instance"] == str(env.socket_path)
    assert state["workdir"] == str(worktree_path)
    assert state["status"] == "working"
    assert state["agent_session_id"] == "session-without-tmux-env"

    host_env = env.env.copy()
    host_env.pop("TMUX", None)
    host_env.pop("TMUX_PANE", None)
    host_env["WORKMUX_BACKEND"] = "tmux"
    result = subprocess.run(
        [str(workmux_exe_path), "set-window-status", "done"],
        cwd=worktree_path,
        env=host_env,
        input=hook_input,
        text=True,
        capture_output=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert read_agent_state(list_agent_state_files(env)[0])["status"] == "done"
    release_fifo.write_text("release\n")


@pytest.mark.tmux_only
def test_background_continuation_requires_transcript_lineage(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature-status-background-lineage"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path, panes=[{"focus": True}])
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)

    real_tmux = shutil.which("tmux", path=os.environ.get("PATH", ""))
    assert real_tmux is not None, "tmux binary not found"
    tmux_wrapper = env.fake_bin_dir / "tmux"
    env.install_script(
        tmux_wrapper,
        "#!/bin/sh\n"
        f'exec {shlex.quote(real_tmux)} -S {shlex.quote(str(env.socket_path))} "$@"\n',
    )

    transcripts = env.tmp_path / "transcripts"
    transcripts.mkdir()
    old_session = "session-old"
    new_session = "session-new"
    unrelated_session = "session-unrelated"
    old_transcript = transcripts / f"{old_session}.jsonl"
    new_transcript = transcripts / f"{new_session}.jsonl"
    unrelated_transcript = transcripts / f"{unrelated_session}.jsonl"
    old_transcript.write_text(
        json.dumps(
            {
                "type": "continued-in",
                "sessionId": old_session,
                "continuedInSessionId": new_session,
            }
        )
        + "\n"
    )
    new_transcript.write_text("")
    unrelated_transcript.write_text("")

    old_input = json.dumps(
        {"session_id": old_session, "transcript_path": str(old_transcript)}
    )
    new_input = json.dumps(
        {"session_id": new_session, "transcript_path": str(new_transcript)}
    )
    unrelated_input = json.dumps(
        {
            "session_id": unrelated_session,
            "transcript_path": str(unrelated_transcript),
        }
    )
    marker_path = env.tmp_path / "background-lineage-finished"
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    command = (
        "unset TMUX TMUX_PANE; "
        f"cd {shlex.quote(str(worktree_path))} && "
        f"printf %s {shlex.quote(old_input)} | "
        f"{shlex.quote(str(workmux_exe_path))} set-window-status working && "
        f"printf %s {shlex.quote(new_input)} | "
        f"CLAUDE_JOB_DIR={shlex.quote(str(env.tmp_path / 'job-new'))} "
        f"{shlex.quote(str(workmux_exe_path))} register-agent && "
        f"printf %s {shlex.quote(new_input)} | "
        f"CLAUDE_JOB_DIR={shlex.quote(str(env.tmp_path / 'job-new'))} "
        f"{shlex.quote(str(workmux_exe_path))} set-window-status done && "
        f"printf %s {shlex.quote(unrelated_input)} | "
        f"CLAUDE_JOB_DIR={shlex.quote(str(env.tmp_path / 'job-unrelated'))} "
        f"{shlex.quote(str(workmux_exe_path))} set-window-status waiting; "
        f"touch {shlex.quote(str(marker_path))}"
    )
    status_cmd = make_env_script(
        env,
        command,
        {
            "HOME": str(env.home_path),
            "PATH": env.env["PATH"],
            "XDG_STATE_HOME": env.env["XDG_STATE_HOME"],
            "WORKMUX_BACKEND": "tmux",
        },
    )
    env.send_keys(window_name, status_cmd)

    assert poll_until(lambda: marker_path.exists(), timeout=5.0)
    state_files = list_agent_state_files(env)
    assert len(state_files) == 1
    state = read_agent_state(state_files[0])
    assert state["status"] == "done"
    assert state["agent_session_id"] == new_session


@pytest.mark.tmux_only
def test_set_window_status_without_pane_identity_refuses_cwd_match(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature-status-refuse-cwd"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path, panes=[{"focus": True}])
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)

    real_tmux = shutil.which("tmux", path=os.environ.get("PATH", ""))
    assert real_tmux is not None, "tmux binary not found"
    tmux_wrapper = env.fake_bin_dir / "tmux"
    env.install_script(
        tmux_wrapper,
        "#!/bin/sh\n"
        f'exec {shlex.quote(real_tmux)} -S {shlex.quote(str(env.socket_path))} "$@"\n',
    )

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    host_env = env.env.copy()
    host_env.pop("TMUX", None)
    host_env.pop("TMUX_PANE", None)
    host_env["WORKMUX_BACKEND"] = "tmux"
    result = subprocess.run(
        [str(workmux_exe_path), "set-window-status", "working"],
        cwd=worktree_path,
        env=host_env,
        input=json.dumps({"session_id": "unbound-session"}),
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr
    assert list_agent_state_files(env) == []


@pytest.mark.tmux_only
def test_set_window_status_accepts_explicit_tmux_target(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature-status-explicit-target"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path, panes=[{"focus": True}])
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)

    pane_id = env.tmux(
        ["list-panes", "-t", window_name, "-F", "#{pane_id}"]
    ).stdout.strip()
    target_env = env.env.copy()
    target_env.pop("TMUX", None)
    target_env.pop("TMUX_PANE", None)
    target_env.update(
        {
            "WORKMUX_STATUS_BACKEND": "tmux",
            "WORKMUX_STATUS_INSTANCE": str(env.socket_path),
            "WORKMUX_STATUS_PANE_ID": pane_id,
        }
    )
    result = subprocess.run(
        [str(workmux_exe_path), "set-window-status", "working"],
        cwd=get_worktree_path(mux_repo_path, branch_name),
        env=target_env,
        input="{}",
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr
    state = read_agent_state(list_agent_state_files(env)[0])
    assert state["pane_key"] == {
        "backend": "tmux",
        "instance": str(env.socket_path),
        "pane_id": pane_id,
    }
    assert state["status"] == "working"


@pytest.mark.tmux_only
def test_set_window_status_clear_clears_explicit_target_state(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """An explicit clear must reset the persisted status for exactly that pane."""
    env = mux_server
    branch_name = "feature-status-explicit-clear"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path, panes=[{"focus": True}])
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)

    pane_id = env.tmux(
        ["list-panes", "-t", window_name, "-F", "#{pane_id}"]
    ).stdout.strip()
    target_env = env.env.copy()
    target_env.pop("TMUX", None)
    target_env.pop("TMUX_PANE", None)
    target_env.update(
        {
            "WORKMUX_STATUS_BACKEND": "tmux",
            "WORKMUX_STATUS_INSTANCE": str(env.socket_path),
            "WORKMUX_STATUS_PANE_ID": pane_id,
        }
    )

    def run_status(status: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(workmux_exe_path), "set-window-status", status],
            cwd=get_worktree_path(mux_repo_path, branch_name),
            env=target_env,
            input="{}",
            text=True,
            capture_output=True,
            check=False,
        )

    result = run_status("working")
    assert result.returncode == 0, result.stderr
    assert poll_until(lambda: len(list_agent_state_files(env)) == 1, timeout=5.0)

    before = read_agent_state(list_agent_state_files(env)[0])
    assert before["status"] == "working"
    assert before["pane_pid"] > 0

    result = run_status("clear")
    assert result.returncode == 0, result.stderr

    def status_cleared() -> bool:
        state = read_agent_state(list_agent_state_files(env)[0])
        return state["status"] is None and state["status_ts"] is None

    assert poll_until(status_cleared, timeout=5.0), "persisted status was not cleared"

    after = read_agent_state(list_agent_state_files(env)[0])
    assert after["pane_key"] == before["pane_key"]
    assert after["pane_pid"] == before["pane_pid"]
    assert after["command"] == before["command"]
    assert after["workdir"] == before["workdir"]


def test_state_file_has_correct_fields(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies that agent state files contain the expected fields."""
    env = mux_server
    branch_name = "feature-fields-test"
    window_name = get_window_name(branch_name)

    write_workmux_config(
        mux_repo_path,
        panes=[
            {"focus": True},  # Just a shell
        ],
    )

    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)

    # Trigger state file creation using tab title
    status_cmd = build_status_cmd(env, workmux_exe_path, "working")
    env.send_keys(window_name, status_cmd)

    def state_file_exists():
        return len(list_agent_state_files(env)) > 0

    assert poll_until(state_file_exists, timeout=5.0), "State file not created"

    # Read and verify state file contents
    state_files = list_agent_state_files(env)
    state = read_agent_state(state_files[0])

    # Check required fields exist
    assert "pane_key" in state, "Missing pane_key field"
    assert "pane_pid" in state, "Missing pane_pid field"
    assert "command" in state, "Missing command field"
    assert "workdir" in state, "Missing workdir field"
    assert "status" in state, "Missing status field"

    # Check pane_key structure
    pane_key = state["pane_key"]
    assert "backend" in pane_key, "Missing backend in pane_key"
    assert "instance" in pane_key, "Missing instance in pane_key"
    assert "pane_id" in pane_key, "Missing pane_id in pane_key"

    # Verify values are sensible
    assert pane_key["backend"] == env.backend_name
    assert state["pane_pid"] > 0, "pane_pid should be positive"
    assert state["status"] == "working", (
        f"Expected status 'working', got '{state['status']}'"
    )
    # Command could be "workmux" (if captured during set-window-status) or the shell
    assert state["command"], "command should not be empty"


def test_state_file_contains_pane_info(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies that state file contains valid pane identification info.

    This tests that the state file has the information needed for reconciliation
    to detect stale panes (pane_id, pane_pid, command).
    """
    env = mux_server
    branch_name = "feature-pane-info-test"
    window_name = get_window_name(branch_name)

    write_workmux_config(
        mux_repo_path,
        panes=[
            {"focus": True},  # Just a shell
        ],
    )

    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)

    # Create state file
    marker_path = env.tmp_path / "pane-info-status-finished"
    status_cmd = build_status_cmd_with_marker(
        env,
        workmux_exe_path,
        "working",
        marker_path,
    )
    env.send_keys(window_name, status_cmd)

    assert poll_until(lambda: marker_path.exists(), timeout=5.0)

    def state_file_exists():
        return len(list_agent_state_files(env)) > 0

    assert poll_until(state_file_exists, timeout=5.0), "State file not created"

    # Read state file and verify reconciliation-relevant fields
    state_files = list_agent_state_files(env)
    state = read_agent_state(state_files[0])

    # Verify pane_key has all required fields for pane identification
    pane_key = state["pane_key"]
    assert pane_key["pane_id"], "pane_id should be set"

    # Verify we have PID and command for stale detection
    assert state["pane_pid"] > 0, (
        "pane_pid should be positive (for PID-based stale detection)"
    )
    assert state["command"], "command should be set (for command-change detection)"

    # Verify workdir is set (useful for context)
    assert state["workdir"], "workdir should be set"


def test_same_status_update_preserves_state_for_same_process(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Repeated status updates retain the status age and stored title."""
    env = mux_server
    agent = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-same-process-state",
        status="working",
    )
    state_file = list_agent_state_files(env)[0]
    state = read_agent_state(state_file)
    state["status_ts"] = 1
    state["activity_ts"] = 2
    state["updated_ts"] = 1
    state["pane_title"] = "same-process title"
    state_file.write_text(json.dumps(state))

    marker = env.tmp_path / "same-process-status-finished"
    env.send_keys(
        agent.window,
        build_status_cmd_with_marker(env, workmux_exe_path, "working", marker),
    )
    assert poll_until(lambda: marker.exists(), timeout=5.0)

    updated = read_agent_state(state_file)
    assert updated["status_ts"] == 1
    assert updated["activity_ts"] == 2
    assert updated["pane_title"] == "same-process title"
    assert updated["updated_ts"] > 1


@pytest.mark.tmux_only
def test_same_status_update_discards_state_for_recycled_pane(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """A pane process mismatch starts a fresh status age and title."""
    env = mux_server
    agent = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-recycled-pane-state",
        status="working",
    )
    state_file = list_agent_state_files(env)[0]
    state = read_agent_state(state_file)
    live_pid = state["pane_pid"]
    state["pane_pid"] = live_pid + 1
    state["status_ts"] = 1
    state["activity_ts"] = 1
    state["updated_ts"] = 1
    state["pane_title"] = "title from recycled pane"
    state_file.write_text(json.dumps(state))

    marker = env.tmp_path / "recycled-pane-status-finished"
    env.send_keys(
        agent.window,
        build_status_cmd_with_marker(env, workmux_exe_path, "working", marker),
    )
    assert poll_until(lambda: marker.exists(), timeout=5.0)

    updated = read_agent_state(state_file)
    assert updated["pane_pid"] == live_pid
    assert updated["status"] == "working"
    assert updated["status_ts"] > 1
    assert updated["activity_ts"] > 1
    assert updated["pane_title"] != "title from recycled pane"


@pytest.mark.tmux_only
def test_same_status_update_discards_state_from_prior_tmux_server(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """A tmux server lifecycle mismatch starts fresh pane state."""
    env = mux_server
    agent = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-prior-server-state",
        status="working",
    )
    state_file = list_agent_state_files(env)[0]
    state = read_agent_state(state_file)
    assert state["boot_id"]
    state["boot_id"] = "prior-server"
    state["status_ts"] = 1
    state["updated_ts"] = 1
    state["pane_title"] = "title from prior server"
    state_file.write_text(json.dumps(state))

    marker = env.tmp_path / "prior-server-status-finished"
    env.send_keys(
        agent.window,
        build_status_cmd_with_marker(env, workmux_exe_path, "working", marker),
    )
    assert poll_until(lambda: marker.exists(), timeout=5.0)

    updated = read_agent_state(state_file)
    assert updated["boot_id"] != "prior-server"
    assert updated["status"] == "working"
    assert updated["status_ts"] > 1
    assert updated["pane_title"] != "title from prior server"


@pytest.mark.tmux_only
@pytest.mark.parametrize("command", ["register-agent", "set-window-status working"])
def test_persistence_combines_tmux_pane_and_boot_queries(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    command: str,
):
    """Persistence obtains live pane and server identity in one tmux query."""
    env = mux_server
    agent = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-combined-snapshot",
        status="done",
    )
    real_tmux = shutil.which("tmux", path=os.environ.get("PATH", ""))
    assert real_tmux is not None
    wrapper_dir = env.tmp_path / "snapshot-query-bin"
    wrapper_dir.mkdir()
    calls = env.tmp_path / "snapshot-query-calls"
    captured = env.tmp_path / "snapshot-query-captured"
    release = env.tmp_path / "snapshot-query-release"
    returned = env.tmp_path / "snapshot-query-returned"
    env.install_script(
        wrapper_dir / "tmux",
        f"#!{sys.executable}\n"
        "import os, subprocess, sys, time\n"
        "from pathlib import Path\n"
        "args = sys.argv[1:]\n"
        "if any('#{start_time}:#{pid}' in arg for arg in args):\n"
        "    kind = 'combined' if any('#{pane_pid}' in arg for arg in args) else 'boot_only'\n"
        f"    with open({str(calls)!r}, 'a') as log:\n"
        "        log.write(kind + '\\n')\n"
        f"    result = subprocess.run([{real_tmux!r}, *args], capture_output=True)\n"
        f"    Path({str(captured)!r}).touch()\n"
        "    deadline = time.monotonic() + 15\n"
        f"    while not Path({str(release)!r}).exists():\n"
        "        if time.monotonic() > deadline: sys.exit(2)\n"
        "        time.sleep(0.01)\n"
        "    sys.stdout.buffer.write(result.stdout)\n"
        "    sys.stdout.buffer.flush()\n"
        f"    Path({str(returned)!r}).touch()\n"
        "    sys.exit(result.returncode)\n"
        f"os.execv({real_tmux!r}, [{real_tmux!r}, *args])\n",
    )
    initial = read_agent_state(list_agent_state_files(env)[0])
    target_env = env.env.copy()
    target_env.update(
        {
            "PATH": f"{wrapper_dir}:{env.env['PATH']}",
            "WORKMUX_STATUS_BACKEND": "tmux",
            "WORKMUX_STATUS_INSTANCE": str(env.socket_path),
            "WORKMUX_STATUS_PANE_ID": initial["pane_key"]["pane_id"],
        }
    )
    child = subprocess.Popen(
        [str(workmux_exe_path), *command.split()],
        cwd=agent.worktree,
        env=target_env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert poll_until(captured.exists, timeout=5)
        lock_path = Path(env.env["XDG_STATE_HOME"]) / "workmux" / "agent-state.lock"
        with lock_path.open("r+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            release.touch()
            assert poll_until(returned.exists, timeout=5)
            # Cross a clock tick with the query returned and the OS lock held.
            query_second = int(time.time())
            assert poll_until(lambda: int(time.time()) > query_second, timeout=3)
            accepted_after = int(time.time())
            fcntl.flock(lock, fcntl.LOCK_UN)
        stdout, stderr = child.communicate(timeout=5)
        assert child.returncode == 0, (stdout, stderr)
    finally:
        release.touch()
        try:
            child.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.communicate(timeout=5)
    assert calls.read_text().splitlines() == ["combined"]
    state = read_agent_state(list_agent_state_files(env)[0])
    boot = env.tmux(["display-message", "-p", "#{start_time}:#{pid}"])
    assert state["boot_id"] == boot.stdout.strip()
    assert state["pane_pid"] > 0
    assert state["status"] == (None if command == "register-agent" else "working")
    assert state["updated_ts"] >= accepted_after
    if command == "register-agent":
        assert state["activity_ts"] >= accepted_after
    else:
        assert state["status_ts"] >= accepted_after
        assert state["activity_ts"] >= accepted_after


@pytest.mark.tmux_only
@pytest.mark.parametrize("command", ["register-agent", "set-window-status working"])
@pytest.mark.parametrize("new_server", [False, True])
def test_persistence_retries_snapshot_without_holding_state_lock(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    command: str,
    new_server: bool,
):
    """A blocked observation neither blocks writers nor compacts their newer state."""
    env = mux_server
    agent = start_active_agent(
        env, workmux_exe_path, mux_repo_path, "feature-snapshot-race", status="working"
    )
    with (agent.worktree / ".workmux.yaml").open("a") as config:
        config.write("\ndashboard:\n  agent_columns: [title, prompt]\n")
    state_file = list_agent_state_files(env)[0]
    initial = read_agent_state(state_file)
    pane_id = initial["pane_key"]["pane_id"]
    other_pane = env.tmux(
        ["new-session", "-d", "-s", "other-writer", "-P", "-F", "#{pane_id}"]
    ).stdout.strip()
    real_tmux = shutil.which("tmux", path=os.environ.get("PATH", ""))
    assert real_tmux is not None
    wrapper_dir = env.tmp_path / "snapshot-race-bin"
    wrapper_dir.mkdir()
    captured = env.tmp_path / "snapshot-captured"
    release = env.tmp_path / "snapshot-release"
    calls = env.tmp_path / "snapshot-calls"
    newer_boot = "1900000000:4242" if new_server else initial["boot_id"]
    env.install_script(
        wrapper_dir / "tmux",
        f"#!{sys.executable}\n"
        "import os, subprocess, sys, time\n"
        "from pathlib import Path\n"
        "args = sys.argv[1:]\n"
        "if any('#{start_time}:#{pid}' in arg and '#{pane_pid}' in arg for arg in args):\n"
        f"    captured, release = Path({str(captured)!r}), Path({str(release)!r})\n"
        "    first = os.environ.get('HOLD_SNAPSHOT') == '1' and not captured.exists()\n"
        "    if os.environ.get('HOLD_SNAPSHOT') == '0':\n"
        "        args = [arg.replace('#{pane_title}', 'concurrent title') for arg in args]\n"
        "    if not first:\n"
        f"        args = [arg.replace('#{{start_time}}:#{{pid}}', {newer_boot!r}) for arg in args]\n"
        f"    result = subprocess.run([{real_tmux!r}, *args], capture_output=True)\n"
        f"    with open({str(calls)!r}, 'a') as log:\n"
        "        log.write(('held' if first else 'fresh') + '\\n')\n"
        "    if first:\n"
        "        captured.touch()\n"
        "        deadline = time.monotonic() + 20\n"
        "        while not release.exists():\n"
        "            if time.monotonic() > deadline: sys.exit(2)\n"
        "            time.sleep(0.01)\n"
        "    sys.stdout.buffer.write(result.stdout)\n"
        "    sys.exit(result.returncode)\n"
        f"os.execv({real_tmux!r}, [{real_tmux!r}, *args])\n",
    )
    target_env = env.env.copy()
    target_env.update(
        {
            "WORKMUX_STATUS_BACKEND": "tmux",
            "WORKMUX_STATUS_INSTANCE": str(env.socket_path),
            "WORKMUX_STATUS_PANE_ID": pane_id,
            "PATH": f"{wrapper_dir}:{env.env['PATH']}",
            "HOLD_SNAPSHOT": "1",
        }
    )
    child = subprocess.Popen(
        [str(workmux_exe_path), *command.split()],
        cwd=agent.worktree,
        env=target_env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert child.stdin is not None
    child.stdin.write("{}")
    child.stdin.close()
    child.stdin = None
    try:
        assert poll_until(captured.exists, timeout=5), child.poll()
        lock_path = Path(env.env["XDG_STATE_HOME"]) / "workmux" / "agent-state.lock"
        # A separate process must be able to take the OS lock during the query.
        subprocess.run(
            [
                sys.executable,
                "-c",
                "import fcntl, sys; "
                "f = open(sys.argv[1], 'r+'); "
                "fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)",
                str(lock_path),
            ],
            check=True,
            timeout=5,
        )
        writer_env = {**target_env, "HOLD_SNAPSHOT": "0"}
        for target in [other_pane, pane_id]:
            result = subprocess.run(
                [
                    str(workmux_exe_path),
                    *(
                        ["register-agent"]
                        if target == pane_id
                        else ["set-window-status", "working"]
                    ),
                ],
                cwd=agent.worktree,
                env={**writer_env, "WORKMUX_STATUS_PANE_ID": target},
                input=json.dumps(
                    {
                        "session_id": "latest-session",
                        "initialPrompt": "latest prompt",
                        "prompt": "latest prompt",
                    }
                ),
                capture_output=True,
                text=True,
                timeout=5,
            )
            assert result.returncode == 0, result.stderr
        latest = read_agent_state(state_file)
        assert latest["boot_id"] == newer_boot
        assert latest["prompt"] == "latest prompt"
        assert latest["agent_session_id"] == "latest-session"
        assert latest["pane_title"] == "concurrent title"
        release.touch()
        stdout, stderr = child.communicate(timeout=10)
        assert child.returncode == 0, (stdout, stderr)
        # Two intervening writes plus the rejected writer's fresh observation.
        assert calls.read_text().splitlines() == ["held", "fresh", "fresh", "fresh"]
        states = [read_agent_state(path) for path in list_agent_state_files(env)]
        assert {state["pane_key"]["pane_id"] for state in states} == {
            pane_id,
            other_pane,
        }
        assert all(state["boot_id"] == newer_boot for state in states)
        updated = read_agent_state(state_file)
        if command == "register-agent":
            assert updated["status"] is None
            assert updated.get("prompt") is None
            assert updated.get("agent_session_id") is None
            assert updated["pane_title"] != "concurrent title"
        else:
            for field in [
                "prompt",
                "agent_session_id",
                "pane_title",
            ]:
                assert updated[field] == latest[field]
        recovery = lock_path.parent / "agent-recovery"
        for path in recovery.rglob("*.json"):
            assert all(
                entry["state"]["boot_id"] != newer_boot
                for entry in json.loads(path.read_text())["entries"]
            )
    finally:
        release.touch()
        try:
            child.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.communicate(timeout=5)


@pytest.mark.tmux_only
@pytest.mark.parametrize("boot_response", ["error", "", "unknown", "1700000000:"])
def test_status_update_preserves_state_when_tmux_boot_id_is_unavailable(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    boot_response: str,
):
    """A failed or incomplete snapshot leaves valid pane state intact."""
    env = mux_server
    agent = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-boot-id-unavailable",
        status="working",
    )
    state_file = list_agent_state_files(env)[0]
    state = read_agent_state(state_file)
    state["status_ts"] = 1
    state["updated_ts"] = 1
    state["pane_title"] = "preserved during query failure"
    state_file.write_text(json.dumps(state))

    real_tmux = shutil.which("tmux", path=os.environ.get("PATH", ""))
    assert real_tmux is not None, "tmux binary not found"
    wrapper_dir = env.tmp_path / "boot-id-failure-bin"
    wrapper_dir.mkdir()
    tmux_wrapper = wrapper_dir / "tmux"
    env.install_script(
        tmux_wrapper,
        f"#!{sys.executable}\n"
        "import os, sys\n"
        "args = sys.argv[1:]\n"
        f"response = {boot_response!r}\n"
        "if any('#{start_time}:#{pid}' in arg for arg in args):\n"
        "    if response == 'error':\n"
        "        sys.exit(1)\n"
        "    args = [arg.replace('#{start_time}:#{pid}', response) for arg in args]\n"
        f"os.execv({real_tmux!r}, [{real_tmux!r}, *args])\n",
    )

    marker = env.tmp_path / "boot-id-failure-status-finished"
    env.send_keys(
        agent.window,
        build_status_cmd_with_marker(
            env,
            workmux_exe_path,
            "working",
            marker,
            {"PATH": f"{wrapper_dir}:{env.env['PATH']}"},
        ),
    )
    assert poll_until(lambda: marker.exists(), timeout=5.0), env.capture_pane(
        agent.window
    )

    assert read_agent_state(state_file) == state


@pytest.mark.tmux_only
def test_status_update_preserves_state_when_tmux_pane_pid_is_unavailable(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """An unparseable live PID leaves valid pane state intact."""
    env = mux_server
    agent = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-pane-pid-unavailable",
        status="working",
    )
    state_file = list_agent_state_files(env)[0]
    state = read_agent_state(state_file)
    state["status_ts"] = 1
    state["updated_ts"] = 1
    state["pane_title"] = "preserved during missing PID"
    state_file.write_text(json.dumps(state))

    real_tmux = shutil.which("tmux", path=os.environ.get("PATH", ""))
    assert real_tmux is not None, "tmux binary not found"
    wrapper_dir = env.tmp_path / "pane-pid-failure-bin"
    wrapper_dir.mkdir()
    tmux_wrapper = wrapper_dir / "tmux"
    env.install_script(
        tmux_wrapper,
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        "if [[ ${1:-} == display-message && ${2:-} == -t "
        "&& ${4:-} == -p && ${5:-} == *'#{pane_pid}'* ]]; then\n"
        "  format=${5//'#{pane_pid}'/not-a-pid}\n"
        f'  exec {shlex.quote(real_tmux)} "$1" "$2" "$3" "$4" "$format"\n'
        "fi\n"
        f'exec {shlex.quote(real_tmux)} "$@"\n',
    )

    marker = env.tmp_path / "pane-pid-failure-status-finished"
    env.send_keys(
        agent.window,
        build_status_cmd_with_marker(
            env,
            workmux_exe_path,
            "working",
            marker,
            {"PATH": f"{wrapper_dir}:{env.env['PATH']}"},
        ),
    )
    assert poll_until(lambda: marker.exists(), timeout=5.0), env.capture_pane(
        agent.window
    )

    assert read_agent_state(state_file) == state


def test_status_update_overwrites_state(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies that calling set-window-status again updates the existing state.

    This ensures the state file is updated (not duplicated) when status changes.
    """
    env = mux_server
    branch_name = "feature-status-update-test"
    window_name = get_window_name(branch_name)

    write_workmux_config(
        mux_repo_path,
        panes=[
            {"focus": True},  # Just a shell
        ],
    )

    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)

    # Create initial state with "working" status
    status_cmd = build_status_cmd(env, workmux_exe_path, "working")
    env.send_keys(window_name, status_cmd)

    def state_file_exists():
        return len(list_agent_state_files(env)) > 0

    assert poll_until(state_file_exists, timeout=5.0), "State file not created"

    # Verify initial status
    state_files = list_agent_state_files(env)
    assert len(state_files) == 1, f"Expected 1 state file, got {len(state_files)}"
    state = read_agent_state(state_files[0])
    assert state["status"] == "working", f"Expected 'working', got '{state['status']}'"

    # Update to "done" status
    status_cmd = build_status_cmd(env, workmux_exe_path, "done")
    env.send_keys(window_name, status_cmd)

    # Poll for status to be updated (more reliable than fixed sleep under load)
    def status_is_done():
        files = list_agent_state_files(env)
        if not files:
            return False
        try:
            state = read_agent_state(files[0])
            return state.get("status") == "done"
        except json.JSONDecodeError:
            # File might be partially written, keep polling
            return False

    assert poll_until(status_is_done, timeout=5.0), "Status was not updated to 'done'"

    # Should still be exactly 1 state file (updated, not duplicated)
    state_files = list_agent_state_files(env)
    assert len(state_files) == 1, (
        f"Expected 1 state file after update, got {len(state_files)}"
    )
