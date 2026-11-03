"""
Tests for `workmux send` command.

Tests error paths and argument validation. Happy-path tests (sending text to
a live agent pane) require a reconciled agent with matching backend/instance,
which is set up via set-window-status.
"""

import json
import time
from pathlib import Path
from typing import cast

import pytest

from .conftest import (
    MuxEnvironment,
    TmuxEnvironment,
    make_env_script,
    poll_until,
    run_workmux_add,
    run_workmux_command,
    write_workmux_config,
)
from .support.agent_state import (
    ActiveAgent,
    build_status_cmd,
    list_agent_state_files,
    start_active_agent,
    stabilize_tmux_agent,
    stabilize_tmux_pane,
)
from .support.codex import CODEX_SIMULATOR
from .support.fake_agent import FAKE_AGENT


def start_agent_behind(
    env: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch: str,
    status: str,
) -> ActiveAgent:
    """[`start_active_agent`], keeping the window that runs commands in
    front so they do not reach the agent's input."""
    command_window = env.tmux(["display-message", "-p", "#{window_id}"]).stdout.strip()
    agent = start_active_agent(env, workmux_exe_path, repo_path, branch, status=status)
    env.select_window(command_window)
    return agent


def run_fake_agent(
    env: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch: str,
    mode: str,
    tmp_path: Path,
) -> ActiveAgent:
    """An agent in `branch` that is done and acts on input by `mode`."""
    agent = start_agent_behind(env, workmux_exe_path, repo_path, branch, "done")
    script = tmp_path / "fake_agent.py"
    script.write_text(FAKE_AGENT)
    ready = tmp_path / f"{branch}-ready"
    command = f"exec python3 {script} {workmux_exe_path} {mode} {ready}"
    env.send_keys(
        agent.window,
        make_env_script(env, command, {"XDG_STATE_HOME": env.env["XDG_STATE_HOME"]}),
    )
    assert poll_until(ready.exists, timeout=10), "fake agent did not start"
    # A done state from this second would hold the send back to the next.
    time.sleep(1)
    return agent


def pane_text(env: MuxEnvironment, window: str) -> str:
    return env.capture_pane(window) or ""


def test_send_error_worktree_not_found(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Send fails with agent_not_found when worktree doesn't exist."""
    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "send nonexistent hello",
        expect_fail=True,
    )
    assert result.exit_code == 5
    assert "agent_not_found" in result.stderr


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
    assert result.exit_code == 5
    assert "agent_not_found: No agent running" in result.stderr


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
    agent = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-send-text",
        status="done",
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "send feature-send-text hello-from-send",
    )
    assert result.exit_code == 0
    assert poll_until(lambda: "hello-from-send" in pane_text(env, agent.window))


def test_send_refuses_an_agent_waiting_on_a_dialog(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Enter would answer the dialog, so nothing is typed."""
    env = mux_server
    agent = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-send-dialog",
        status="waiting",
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "send feature-send-dialog must-not-answer",
        expect_fail=True,
    )
    assert result.exit_code == 3
    assert "agent_waiting" in result.stderr
    assert "must-not-answer" not in pane_text(env, agent.window)


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
        status="done",
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
def test_send_to_a_worktree_with_several_agents_needs_a_pane_id(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """A handle naming several agents is refused, listing their pane IDs,
    each of which targets its agent."""
    env = cast(TmuxEnvironment, mux_server)
    agent = start_agent_behind(
        env, workmux_exe_path, mux_repo_path, "feature-send-two", "done"
    )
    second = env.tmux(
        [
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            agent.window,
            "-c",
            str(agent.worktree),
        ]
    ).stdout.strip()
    stabilize_tmux_agent(env, agent)
    env.send_keys(second, build_status_cmd(env, workmux_exe_path, "done"))
    assert poll_until(lambda: len(list_agent_state_files(env)) == 2)
    stabilize_tmux_pane(env, second)
    panes = env.tmux(
        ["list-panes", "-t", agent.window, "-F", "#{pane_id}"]
    ).stdout.split()
    assert second in panes and len(panes) == 2

    def agents() -> list:
        listed = run_workmux_command(
            env, workmux_exe_path, mux_repo_path, "status --json feature-send-two"
        )
        return json.loads(listed.stdout)["agents"]

    assert poll_until(lambda: len(agents()) == 2, timeout=10), agents()

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "send feature-send-two to-whom",
        expect_fail=True,
    )
    assert result.exit_code == 4
    assert "ambiguous_target" in result.stderr
    for pane in panes:
        assert pane in result.stderr

    run_workmux_command(
        env, workmux_exe_path, mux_repo_path, f"send {second} to-second"
    )
    assert poll_until(lambda: "to-second" in pane_text(env, second))
    captured = run_workmux_command(
        env, workmux_exe_path, mux_repo_path, f"capture {second}"
    )
    assert "to-second" in captured.stdout


@pytest.mark.tmux_only
def test_send_wait_confirms_the_turn_and_waits_for_it_to_settle(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    tmp_path: Path,
):
    env = cast(TmuxEnvironment, mux_server)
    run_fake_agent(
        env, workmux_exe_path, mux_repo_path, "feature-wait-turn", "turns", tmp_path
    )

    started = time.time()
    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "send --wait feature-wait-turn first",
        timeout=20,
    )
    assert "feature-wait-turn: done" in result.stderr
    assert time.time() - started >= 3, "returned the previous turn's done"

    time.sleep(1)
    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "send --wait --until working feature-wait-turn second",
        timeout=20,
    )
    assert "feature-wait-turn: working" in result.stderr


@pytest.mark.tmux_only
def test_send_then_wait_waits_for_the_turn_the_send_started(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    tmp_path: Path,
):
    """The previous turn's done does not satisfy a `wait` right after a send."""
    env = cast(TmuxEnvironment, mux_server)
    run_fake_agent(
        env, workmux_exe_path, mux_repo_path, "feature-wait-fence", "turns", tmp_path
    )

    run_workmux_command(
        env, workmux_exe_path, mux_repo_path, "send feature-wait-fence task"
    )
    started = time.time()
    run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "wait feature-wait-fence --status done --timeout 20",
        timeout=25,
    )
    assert time.time() - started >= 2, "wait returned the previous turn's done"


@pytest.mark.tmux_only
def test_send_wait_reports_an_agent_that_never_acts(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    tmp_path: Path,
):
    env = cast(TmuxEnvironment, mux_server)
    run_fake_agent(
        env, workmux_exe_path, mux_repo_path, "feature-wait-stall", "silent", tmp_path
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "send --wait feature-wait-stall swallowed",
        expect_fail=True,
        timeout=20,
    )
    assert result.exit_code == 6
    assert "send_stalled" in result.stderr


@pytest.mark.tmux_only
def test_send_wait_reports_an_agent_that_exits(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    tmp_path: Path,
):
    env = cast(TmuxEnvironment, mux_server)
    run_fake_agent(
        env, workmux_exe_path, mux_repo_path, "feature-wait-exit", "exit", tmp_path
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "send --wait feature-wait-exit bye",
        expect_fail=True,
        timeout=20,
    )
    assert result.exit_code == 7
    assert "agent_gone" in result.stderr


@pytest.mark.tmux_only
def test_send_wait_times_out_on_a_turn_that_does_not_end(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = cast(TmuxEnvironment, mux_server)
    agent = start_agent_behind(
        env, workmux_exe_path, mux_repo_path, "feature-wait-long", "working"
    )
    stabilize_tmux_agent(env, agent)

    started = time.time()
    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "send --wait --timeout 2 feature-wait-long more",
        expect_fail=True,
        timeout=20,
    )
    assert result.exit_code == 124
    assert "timeout" in result.stderr
    assert time.time() - started < 10


def test_send_wait_needs_a_reported_status(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """`--wait` cannot observe an agent without a status; plain send can
    still type into it."""
    env = mux_server
    agent = start_active_agent(
        env, workmux_exe_path, mux_repo_path, "feature-wait-unknown", status="done"
    )
    env.send_keys(agent.window, build_status_cmd(env, workmux_exe_path, "clear"))

    def statuses() -> list:
        listed = run_workmux_command(
            env, workmux_exe_path, mux_repo_path, "status --json feature-wait-unknown"
        )
        return [agent["status"] for agent in json.loads(listed.stdout)["agents"]]

    assert poll_until(lambda: statuses() == ["-"], timeout=10), statuses()

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "send --wait feature-wait-unknown unobservable",
        expect_fail=True,
    )
    assert result.exit_code == 8
    assert "agent_status_unknown" in result.stderr
    assert "unobservable" not in pane_text(env, agent.window)

    run_workmux_command(
        env, workmux_exe_path, mux_repo_path, "send feature-wait-unknown hookless"
    )


@pytest.mark.tmux_only
def test_send_codex_prompt_uses_bracketed_paste(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    tmp_path: Path,
):
    """Codex prompts submit through bracketed paste instead of a typed burst."""
    env = cast(TmuxEnvironment, mux_server)
    command_window = env.tmux(["display-message", "-p", "#{window_id}"]).stdout.strip()
    agent = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-send-codex",
        status="done",
    )
    write_workmux_config(mux_repo_path, agent="codex", panes=[{"focus": True}])

    ready_file = tmp_path / "codex-simulator-ready"
    submitted_file = tmp_path / "codex-simulator-submitted"
    input_file = tmp_path / "codex-simulator-input"
    simulator = tmp_path / "codex_simulator.py"
    simulator.write_text(CODEX_SIMULATOR)

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
