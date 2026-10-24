"""Remote agent status over the loopback transport: the node's agent state
reaches the local attach pane, views and commands.

A fake agent on the node reports through `workmux set-window-status`, as
real agent hooks do, and is driven through a control file."""

import json
import subprocess
import time
from pathlib import Path
from typing import List, Optional

import pytest
import yaml

from ..conftest import (
    TmuxEnvironment,
    get_window_name,
    poll_until,
    wait_for_pane_output,
)
from ..support.codex import CODEX_SIMULATOR
from ..test_popup_navigation import attached_client
from .conftest import LOOPBACK, Node, git, local_records, wm

pytestmark = pytest.mark.tmux_only


class FakeAgent:
    """A script that applies statuses written to its control file."""

    def __init__(self, directory: Path):
        self.control = directory / "agent-control"
        self.path = directory / "fake-agent"
        self.path.write_text(
            f"""#!/bin/sh
control={self.control}
echo FAKE-AGENT-READY
while :; do
  if [ -f "$control" ]; then
    command=$(cat "$control")
    rm -f "$control"
    case $command in
      exit) exit 0 ;;
      *) workmux set-window-status "$command" </dev/null && echo "applied-$command" ;;
    esac
  fi
  sleep 0.1
done
"""
        )
        self.path.chmod(0o755)

    def do(self, command: str) -> None:
        tmp = self.control.with_suffix(".tmp")
        tmp.write_text(command)
        tmp.rename(self.control)


def setup_agent_project(repo: Path, agent: FakeAgent) -> None:
    project = {
        "nerdfont": False,
        "agent": str(agent.path),
        "panes": [{"command": "<agent>"}],
    }
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "configure a fake agent")


def local_states(env: TmuxEnvironment) -> List[dict]:
    directory = env.home_path / ".local/state/workmux/agents"
    if not directory.is_dir():
        return []
    return [json.loads(path.read_text()) for path in directory.glob("*.json")]


def remote_state(env: TmuxEnvironment) -> Optional[dict]:
    states = [state for state in local_states(env) if state.get("remote")]
    return states[0] if len(states) == 1 else None


def observation_path(env: TmuxEnvironment) -> Path:
    return env.home_path / ".local/state/workmux/remote/lo/observed.v2.json"


def observed_since(env: TmuxEnvironment, since: float) -> bool:
    """Whether a connected observation arrived after `since`."""
    observed = json.loads(observation_path(env).read_text())
    return observed["connected"] and observed["last_seen"] > since


def status_rows(env: TmuxEnvironment, exe: Path, repo: Path, *args: str) -> List[dict]:
    result = wm(env, exe, repo, " ".join(["status", "--json", *args]))
    return [row for row in json.loads(result.stdout)["agents"] if row.get("remote")]


def start_workmux(
    env: TmuxEnvironment, exe: Path, repo: Path, *args: str
) -> "subprocess.Popen[str]":
    """Run workmux in the background, outside tmux."""
    process_env = {**env.env, **LOOPBACK}
    process_env.pop("TMUX", None)
    return subprocess.Popen(
        [str(exe), *args],
        cwd=repo,
        env=process_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def window_status(env: TmuxEnvironment, window: str) -> str:
    result = env.tmux(
        ["show-options", "-wqv", "-t", window, "@workmux_status"], check=False
    )
    return result.stdout.strip()


def test_remote_status_reaches_the_attach_pane(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    repo = mux_repo_path
    agent = FakeAgent(tmp_path)
    setup_agent_project(repo, agent)
    window = get_window_name("st")
    result = wm(mux_server, workmux_exe_path, repo, "add st --remote lo")
    assert "Successfully created remote workspace" in result.stdout, result.stderr
    wait_for_pane_output(mux_server, window, "FAKE-AGENT-READY", timeout=20)
    [record] = local_records(mux_server)

    # The relay registers its pane before any status arrives.
    assert poll_until(lambda: remote_state(mux_server) is not None, timeout=10)
    state = remote_state(mux_server)
    assert state is not None
    assert state["remote"]["ws_id"] == record["ws_id"]
    assert state["status"] is None

    agent.do("working")
    assert poll_until(
        lambda: (remote_state(mux_server) or {}).get("status") == "working",
        timeout=10,
    )
    assert window_status(mux_server, window) != ""

    agent.do("done")
    assert poll_until(
        lambda: (remote_state(mux_server) or {}).get("status") == "done",
        timeout=10,
    )
    observed = json.loads(observation_path(mux_server).read_text())
    assert observed["connected"] is True


def test_commands_reach_remote_agents_with_or_without_a_window(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    repo = mux_repo_path
    exe = workmux_exe_path
    agent = FakeAgent(tmp_path)
    setup_agent_project(repo, agent)
    window = get_window_name("cmd")
    wm(mux_server, exe, repo, "add cmd --remote lo")
    wait_for_pane_output(mux_server, window, "FAKE-AGENT-READY", timeout=20)
    agent.do("working")
    wait_for_pane_output(mux_server, window, "applied-working", timeout=10)

    # Attached: the row is the local attach pane.
    assert poll_until(
        lambda: [r["status"] for r in status_rows(mux_server, exe, repo)]
        == ["working"],
        timeout=10,
    )
    [row] = status_rows(mux_server, exe, repo, "cmd")
    assert row["remote"]["name"] == "lo"
    assert row["remote"]["attached"] is True
    assert row["remote"]["connected"] is True
    assert row["worktree"] == "cmd"
    assert row["pane_id"].startswith("%") and "remote" not in row["pane_id"]
    table = wm(mux_server, exe, repo, "status")
    assert "cmd@lo" in table.stdout

    # Closing the window keeps the agent listed, now through the node.
    wm(mux_server, exe, repo, "close cmd")
    assert remote_state(mux_server) is None
    [row] = status_rows(mux_server, exe, repo)
    assert row["status"] == "working"
    assert row["remote"]["attached"] is False
    assert row["pane_id"].startswith("%remote-")

    # list reports the agent and the node's git state from the observation.
    [workspace] = node.workspaces()
    worktree = Path(workspace["worktree_path"])
    (worktree / "node.txt").write_text("node\n")
    git(worktree, "add", "node.txt")
    git(worktree, "-c", "user.name=A", "-c", "user.email=a@x", "commit", "-m", "n")
    (worktree / "dirty.txt").write_text("dirty\n")

    def listed() -> dict:
        rows = json.loads(wm(mux_server, exe, repo, "list --json").stdout)
        [listed] = [row for row in rows if row.get("remote") == "lo"]
        return listed

    assert poll_until(lambda: listed()["has_uncommitted_changes"], timeout=15)
    assert listed()["agent_statuses"] == ["working"]
    [line] = [
        line
        for line in wm(mux_server, exe, repo, "list").stdout.splitlines()
        if "remote:lo" in line
    ]
    assert "working" in line and "●" in line, line

    # send and capture go through the node.
    wm(mux_server, exe, repo, "send cmd remote-hello-42")
    assert poll_until(
        lambda: "remote-hello-42" in wm(mux_server, exe, repo, "capture cmd").stdout,
        timeout=10,
    )

    # wait sees transitions on the node.
    waiting = start_workmux(mux_server, exe, repo, "wait", "cmd", "--timeout", "30")
    agent.do("done")
    _, stderr = waiting.communicate(timeout=40)
    assert waiting.returncode == 0, stderr
    assert "cmd: done" in stderr

    # An agent that exits while waited on is reported.
    agent.do("working")
    assert poll_until(
        lambda: [r["status"] for r in status_rows(mux_server, exe, repo)]
        == ["working"],
        timeout=10,
    )
    started = time.time()
    waiting = start_workmux(mux_server, exe, repo, "wait", "cmd", "--timeout", "30")
    assert poll_until(lambda: observed_since(mux_server, started), timeout=15)
    # Let one poll see the agent running.
    time.sleep(3)
    agent.do("exit")
    _, stderr = waiting.communicate(timeout=40)
    assert waiting.returncode == 3, stderr
    assert "agent exited unexpectedly" in stderr
    assert status_rows(mux_server, exe, repo) == []


def test_send_reaches_a_remote_agent_the_config_picks(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    """Without `-a`, the configured agent decides how input is typed."""
    repo = mux_repo_path
    exe = workmux_exe_path
    simulator = tmp_path / "codex_simulator.py"
    simulator.write_text(CODEX_SIMULATOR)
    ready = tmp_path / "codex-ready"
    submitted = tmp_path / "codex-submitted"
    received = tmp_path / "codex-input"
    # Under an interpreter, the pane does not look like Codex.
    mux_server.install_script(
        mux_server.fake_bin_dir / "codex",
        f"#!/bin/sh\nexec python3 {simulator} workmux {ready} {submitted} {received}\n",
    )
    project = {"nerdfont": False, "agent": "codex", "panes": [{"command": "<agent>"}]}
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "configure codex")

    wm(mux_server, exe, repo, "add cx --remote lo")
    assert poll_until(ready.exists, timeout=20), "Codex simulator did not start"
    assert poll_until(
        lambda: [r["status"] for r in status_rows(mux_server, exe, repo)]
        == ["waiting"],
        timeout=10,
    )
    prompt = "continue with deterministic delivery"
    wm(mux_server, exe, repo, f"send cx '{prompt}'")
    assert poll_until(submitted.exists, timeout=10), (
        f"prompt was not pasted; input={received.read_bytes()!r}"
    )
    assert submitted.read_text() == prompt


def test_views_list_remote_agents_beside_local_ones(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    env = mux_server
    repo = mux_repo_path
    exe = workmux_exe_path
    # Unix sidebar sockets must fit macOS sockaddr_un, unlike pytest paths.
    env.env["TMPDIR"] = "/tmp"
    env.env.update(LOOPBACK)
    for name, value in [("TMPDIR", "/tmp"), *LOOPBACK.items()]:
        env.tmux(["set-environment", "-g", name, value])
    config = env.home_path / ".config/workmux/config.yaml"
    config.write_text(config.read_text() + "sidebar:\n  git_status: false\n")

    agent = FakeAgent(tmp_path)
    setup_agent_project(repo, agent)
    window = get_window_name("vw")
    wm(env, exe, repo, "add vw --remote lo")
    wait_for_pane_output(env, window, "FAKE-AGENT-READY", timeout=20)
    agent.do("working")
    wait_for_pane_output(env, window, "applied-working", timeout=10)

    # A local agent beside it, in the runner pane.
    runner_pane = env.runner_pane_id
    assert runner_pane is not None
    env.env["TMUX"] = f"{env.socket_path},0,0"
    env.env["TMUX_PANE"] = runner_pane
    env.run_command([str(exe), "set-window-status", "done"], cwd=repo)

    env.tmux(["new-window", "-d", "-n", "dash", "-c", str(repo), f"{exe} dashboard"])

    def dashboard() -> str:
        return env.tmux(["capture-pane", "-p", "-t", "dash"]).stdout

    def row(screen: str, text: str) -> str:
        return next((line for line in screen.splitlines() if text in line), "")

    assert poll_until(
        lambda: "vw@lo" in dashboard() and "main" in dashboard(), timeout=20
    ), dashboard()

    # Without a window, the dashboard keeps the row through its own observer
    # and previews the agent from the node.
    wm(env, exe, repo, "close vw")
    assert poll_until(lambda: "vw@lo" in dashboard(), timeout=10), dashboard()
    env.tmux(["send-keys", "-t", "dash", "/", "vw@lo", "Enter"])
    assert poll_until(lambda: "applied-working" in dashboard(), timeout=20), dashboard()

    # The node stops answering: the row stays, disconnected, then recovers.
    bin_dir = node.root / "bin"
    hidden = node.root / "bin-hidden"
    bin_dir.rename(hidden)
    # The running stream keeps the path it was started with.
    subprocess.run(["pkill", "-f", f"{bin_dir}/.* node watch"], check=False)
    try:
        assert poll_until(
            lambda: "disconnected" in row(dashboard(), "vw@lo"), timeout=30
        ), dashboard()
    finally:
        hidden.rename(bin_dir)
    assert poll_until(
        lambda: "vw@lo" in dashboard()
        and "disconnected" not in row(dashboard(), "vw@lo"),
        timeout=40,
    ), dashboard()

    # The sidebar lists both.
    with attached_client(env):
        result = env.run_command(
            [str(exe), "sidebar", "on", "--width", "40"], cwd=repo, check=False
        )
        assert result.returncode == 0, result.stderr
        panes = env.tmux(
            ["list-panes", "-t", runner_pane, "-F", "#{pane_id}"]
        ).stdout.split()
        sidebar = next(pane for pane in panes if pane != runner_pane)

        def screen() -> str:
            return env.tmux(["capture-pane", "-p", "-t", sidebar]).stdout

        assert poll_until(lambda: "vw@lo" in screen(), timeout=20), screen()

    # Enter on the detached row opens a window relaying the running agent.
    env.tmux(["send-keys", "-t", "dash", "Enter"])
    assert poll_until(lambda: window in env.list_windows(), timeout=20)
    wait_for_pane_output(env, window, "applied-working", timeout=20)


def test_resurrect_reattaches_without_restarting_the_agent(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    env = mux_server
    repo = mux_repo_path
    exe = workmux_exe_path
    agent = FakeAgent(tmp_path)
    setup_agent_project(repo, agent)
    window = get_window_name("rs")
    wm(env, exe, repo, "add rs --remote lo")
    wait_for_pane_output(env, window, "FAKE-AGENT-READY", timeout=20)
    agent.do("working")
    wait_for_pane_output(env, window, "applied-working", timeout=10)
    assert poll_until(
        lambda: (remote_state(env) or {}).get("status") == "working", timeout=10
    )
    sessions = node.sessions()
    [session] = sessions

    def agent_pid() -> str:
        return node.tmux(
            "display-message", "-p", "-t", f"={session}", "#{pane_pid}"
        ).stdout.strip()

    pid = agent_pid()

    # The local tmux server goes away; the remote agent keeps running.
    env.stop_server()
    no_tmux = tmp_path / "no-tmux"
    no_tmux.mkdir()
    process_env = {**env.env, **LOOPBACK, "TMUX_TMPDIR": str(no_tmux)}
    status = subprocess.run(
        [str(exe), "status", "--json", "rs"],
        cwd=repo,
        env=process_env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert status.returncode == 0, status.stderr
    [row] = json.loads(status.stdout)["agents"]
    assert row["status"] == "working"
    assert row["remote"]["attached"] is False
    env.start_server()
    result = wm(env, exe, repo, "resurrect")
    assert "rs@lo" in result.stdout and "restoring" in result.stdout, result.stdout
    assert window in env.list_windows()
    wait_for_pane_output(env, window, "applied-working", timeout=20)
    assert node.sessions() == sessions
    assert agent_pid() == pid
    assert poll_until(
        lambda: (remote_state(env) or {}).get("status") == "working", timeout=10
    )
    again = wm(env, exe, repo, "resurrect")
    assert "restoring" not in again.stdout, again.stdout

    # A closed window stays closed across a restart.
    wm(env, exe, repo, "close rs")
    env.stop_server()
    env.start_server()
    result = wm(env, exe, repo, "resurrect")
    assert "rs@lo" not in result.stdout, result.stdout
    assert window not in env.list_windows()
    assert agent_pid() == pid

    # Also when it was reopened by hand after a restart, which files the
    # earlier server's pane state away for resurrection.
    wm(env, exe, repo, "open rs")
    assert poll_until(lambda: remote_state(env) is not None, timeout=10)
    env.stop_server()
    env.start_server()
    wm(env, exe, repo, "open rs")
    wait_for_pane_output(env, window, "applied-working", timeout=20)
    wm(env, exe, repo, "close rs")
    env.stop_server()
    env.start_server()
    result = wm(env, exe, repo, "resurrect")
    assert "rs@lo" not in result.stdout, result.stdout
    assert agent_pid() == pid


def test_an_older_observer_does_not_hide_the_node(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    repo = mux_repo_path
    exe = workmux_exe_path
    agent = FakeAgent(tmp_path)
    setup_agent_project(repo, agent)
    window = get_window_name("mix")
    wm(mux_server, exe, repo, "add mix --remote lo")
    wait_for_pane_output(mux_server, window, "FAKE-AGENT-READY", timeout=20)
    wm(mux_server, exe, repo, "close mix")

    # An older build observes the remote: it holds its lock and keeps writing
    # a connected observation in its own format that lists no agents.
    directory = mux_server.home_path / ".local/state/workmux/remote/lo"
    legacy = directory / "observed.json"
    holder = subprocess.Popen(
        [
            "python3",
            "-c",
            "import fcntl, json, sys, time\n"
            "lock = open(sys.argv[1], 'a')\n"
            "fcntl.flock(lock, fcntl.LOCK_EX)\n"
            "while True:\n"
            "    data = {'connected': True, 'last_seen': int(time.time()),"
            " 'skew': 0, 'workspaces': [], 'retired': 1}\n"
            "    open(sys.argv[2], 'w').write(json.dumps(data))\n"
            "    time.sleep(0.5)\n",
            str(directory / "observer.lock"),
            str(legacy),
        ]
    )
    try:
        assert poll_until(legacy.exists, timeout=5)
        agent.do("working")
        assert poll_until(
            lambda: [r["status"] for r in status_rows(mux_server, exe, repo)]
            == ["working"],
            timeout=15,
        )
        [row] = status_rows(mux_server, exe, repo)
        assert row["remote"]["connected"] is True
    finally:
        holder.kill()
        holder.wait()
