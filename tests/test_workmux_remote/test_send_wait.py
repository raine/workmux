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
    and reports `done`. Lines starting with `fast` take no time; lines
    starting with `prompt` report `working` at once."""
    path = directory / "turn-agent"
    path.write_text(
        f"""#!/bin/sh
workmux set-window-status done </dev/null
echo TURN-AGENT-READY
n=0
while read -r line; do
  n=$((n + 1))
  case $line in fast*|prompt*) ;; *) sleep 4 ;; esac
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


def configure(repo: Path, agent: Path, panes: int = 1) -> None:
    lines = "  - command: <agent>\n" + "".join(
        "  - command: <agent>\n    split: horizontal\n" for _ in range(panes - 1)
    )
    (repo / ".workmux.yaml").write_text(
        f"nerdfont: false\nagent: {agent}\npanes:\n{lines}"
    )
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "configure an agent")


def test_send_wait_follows_a_remote_turn(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    repo = mux_repo_path
    exe = workmux_exe_path
    configure(repo, turn_agent(tmp_path))
    wm(mux_server, exe, repo, "add sw --remote lo")
    wait_for_pane_output(
        mux_server, get_window_name("sw"), "TURN-AGENT-READY", timeout=20
    )
    assert poll_until(lambda: statuses(mux_server, exe, repo) == ["done"], timeout=15)

    result = wm(mux_server, exe, repo, "send --wait sw prompt-1")
    assert (tmp_path / "turn-1").exists(), "returned the previous turn's done"
    assert "sw: done" in result.stderr

    result = wm(mux_server, exe, repo, "send --wait --until working sw prompt-2")
    assert "sw: working" in result.stderr
    assert not (tmp_path / "turn-2").exists()

    # A working agent's turn needs no new activity, so the timeout decides.
    result = wm(
        mux_server,
        exe,
        repo,
        "send --wait --until waiting --timeout 2 sw fast-3",
        expect_fail=True,
    )
    assert result.exit_code == 124, result.stderr


def test_send_refuses_a_remote_agent_waiting_on_a_dialog(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    repo = mux_repo_path
    exe = workmux_exe_path
    received = tmp_path / "received"
    agent = tmp_path / "dialog-agent"
    agent.write_text(
        "#!/bin/sh\n"
        "workmux set-window-status waiting </dev/null\n"
        "echo DIALOG-READY\n"
        f": > {received}\n"
        f'while read -r line; do echo "$line" >> {received}; done\n'
    )
    agent.chmod(0o755)
    configure(repo, agent)
    wm(mux_server, exe, repo, "add dlg --remote lo")
    wait_for_pane_output(mux_server, get_window_name("dlg"), "DIALOG-READY", timeout=20)
    assert poll_until(
        lambda: statuses(mux_server, exe, repo) == ["waiting"], timeout=15
    )

    result = wm(mux_server, exe, repo, "send dlg must-not-answer", expect_fail=True)
    assert result.exit_code == 3, result.stderr
    assert "agent_waiting" in result.stderr
    time.sleep(1)
    assert "must-not-answer" not in received.read_text()


def test_send_to_one_of_several_remote_agents_takes_its_pane_id(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    repo = mux_repo_path
    exe = workmux_exe_path
    configure(repo, turn_agent(tmp_path), panes=2)
    wm(mux_server, exe, repo, "add two --remote lo")
    assert poll_until(
        lambda: statuses(mux_server, exe, repo) == ["done", "done"], timeout=20
    )

    result = wm(mux_server, exe, repo, "send two fast-1", expect_fail=True)
    assert result.exit_code == 4, result.stderr
    assert "ambiguous_target" in result.stderr
    pane_ids = [row["pane_id"] for row in status_rows(mux_server, exe, repo)]
    for pane_id in pane_ids:
        assert pane_id in result.stderr
    assert not (tmp_path / "turn-1").exists()

    # The second agent's relay pane takes the prompt to that agent.
    rows = status_rows(mux_server, exe, repo)
    second = next(row for row in rows if row["remote"]["pane"] == 1)
    result = wm(mux_server, exe, repo, f"send --wait {second['pane_id']} fast-1")
    assert "done" in result.stderr
    captured = wm(mux_server, exe, repo, f"capture {second['pane_id']}")
    assert "fast-1" in captured.stdout
    first = next(row for row in rows if row["remote"]["pane"] == 0)
    captured = wm(mux_server, exe, repo, f"capture {first['pane_id']}")
    assert "fast-1" not in captured.stdout
