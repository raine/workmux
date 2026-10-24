"""`send` followed at once by `wait` on a remote agent waits for the turn
the send started, though the agent reports it later than `send` returns
and the laptop sees it later still."""

import time
from pathlib import Path

import pytest

from ..conftest import (
    TmuxEnvironment,
    get_window_name,
    poll_until,
    wait_for_pane_output,
)
from .conftest import Node, git, wm
from .test_status import start_workmux, status_rows

pytestmark = pytest.mark.tmux_only


def turn_agent(directory: Path) -> Path:
    """An agent that is done, then runs one turn per line typed into it:
    after a delay it reports `working`, a while later it writes `turn-<n>`
    and reports `done`. Lines starting with `fast` take no time."""
    path = directory / "turn-agent"
    path.write_text(
        f"""#!/bin/sh
workmux set-window-status done </dev/null
echo TURN-AGENT-READY
n=0
while read -r line; do
  n=$((n + 1))
  case $line in fast*) ;; *) sleep 4 ;; esac
  workmux set-window-status working </dev/null
  case $line in fast*) ;; *) sleep 4 ;; esac
  echo "$line" > {directory}/turn-$n
  workmux set-window-status done </dev/null
done
"""
    )
    path.chmod(0o755)
    return path


def statuses(env: TmuxEnvironment, exe: Path, repo: Path) -> list:
    return [row["status"] for row in status_rows(env, exe, repo)]


def test_wait_after_send_waits_for_the_turn_it_started(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    repo = mux_repo_path
    exe = workmux_exe_path
    agent = turn_agent(tmp_path)
    (repo / ".workmux.yaml").write_text(
        f"nerdfont: false\nagent: {agent}\npanes:\n  - command: <agent>\n"
    )
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "configure a turn agent")
    wm(mux_server, exe, repo, "add sw --remote lo")
    wait_for_pane_output(
        mux_server, get_window_name("sw"), "TURN-AGENT-READY", timeout=20
    )
    assert poll_until(lambda: statuses(mux_server, exe, repo) == ["done"], timeout=15)

    # Nobody sent: an agent that is done is done.
    started = time.time()
    wm(mux_server, exe, repo, "wait sw --timeout 20")
    assert time.time() - started < 4

    # The turn reports `working` 4 s after the prompt, long after `send`
    # returned and the observation still shows the previous `done`.
    wm(mux_server, exe, repo, "send sw slow-1")
    wm(mux_server, exe, repo, "wait sw --timeout 30")
    assert (tmp_path / "turn-1").exists(), "wait returned the previous turn's done"

    # A turn right after a `done` reports in a later second than it, so it
    # is told apart and the wait still ends.
    wm(mux_server, exe, repo, "send sw fast-2")
    wm(mux_server, exe, repo, "wait sw --timeout 30")
    assert (tmp_path / "turn-2").exists()

    # Sent to a working agent, `wait` ends with the turn already running,
    # not with the `done` before it.
    wm(mux_server, exe, repo, "send sw slow-3")
    assert poll_until(
        lambda: statuses(mux_server, exe, repo) == ["working"], timeout=15
    )
    waiting = start_workmux(mux_server, exe, repo, "wait", "sw", "--timeout", "30")
    wm(mux_server, exe, repo, "send sw fast-4")
    _, stderr = waiting.communicate(timeout=40)
    assert waiting.returncode == 0, stderr
    assert (tmp_path / "turn-3").exists()
    assert poll_until((tmp_path / "turn-4").exists, timeout=10)

    # `--status working` after a send is the turn's working state.
    wm(mux_server, exe, repo, "send sw slow-5")
    wm(mux_server, exe, repo, "wait sw --status working --timeout 30")
    assert not (tmp_path / "turn-5").exists()
    wm(mux_server, exe, repo, "wait sw --timeout 30")
    assert (tmp_path / "turn-5").exists()


def test_send_refuses_an_agent_that_has_not_reported_without_a_container(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    """Without a container the agent is the pane's own program: nothing but
    its hook says it still runs, so `send` waits for one."""
    repo = mux_repo_path
    exe = workmux_exe_path
    agent = tmp_path / "silent-agent"
    agent.write_text("#!/bin/sh\necho SILENT-READY\nexec cat > /dev/null\n")
    agent.chmod(0o755)
    (repo / ".workmux.yaml").write_text(
        f"nerdfont: false\nagent: {agent}\npanes:\n  - command: <agent>\n"
    )
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "configure a silent agent")
    wm(mux_server, exe, repo, "add quiet --remote lo")
    wait_for_pane_output(
        mux_server, get_window_name("quiet"), "SILENT-READY", timeout=20
    )
    result = wm(mux_server, exe, repo, "send quiet hello", expect_fail=True)
    assert "no agent is running in workspace 'quiet'" in result.stderr
