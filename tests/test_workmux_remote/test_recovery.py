"""Recovery of remote executions over the loopback transport: an execution
the host rebooted under shows as ended and `workmux open` resumes its
agent's conversation; Claude never asks to trust node worktrees."""

import json
from pathlib import Path

import pytest

from ..conftest import (
    TmuxEnvironment,
    get_window_name,
    poll_until,
    wait_for_pane_output,
)
from .conftest import Node, commit_project, local_records, wm

pytestmark = pytest.mark.tmux_only


def setup_claude_project(repo: Path, bin_dir: Path, log: Path) -> None:
    """A fake `claude` that logs its arguments and stays up."""
    bin_dir.mkdir()
    claude = bin_dir / "claude"
    claude.write_text(
        f"""#!/bin/sh
echo "ARGS:$*" >> {log}
echo FAKE-CLAUDE-READY "$*"
while :; do sleep 1; done
"""
    )
    claude.chmod(0o755)
    commit_project(
        repo,
        {"nerdfont": False, "agent": str(claude), "panes": [{"command": "<agent>"}]},
    )


def remote_status(env: TmuxEnvironment, exe: Path, repo: Path) -> list:
    status = json.loads(wm(env, exe, repo, "status --json").stdout)
    return [agent for agent in status["agents"] if agent.get("remote")]


def test_reboot_resumes_and_claude_trusts_worktrees(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    repo = mux_repo_path
    log = tmp_path / "claude-args.log"
    setup_claude_project(repo, tmp_path / "bin", log)
    claude_json = mux_server.home_path / ".claude" / ".claude.json"
    claude_json.parent.mkdir(parents=True, exist_ok=True)
    claude_json.write_text(json.dumps({"numStartups": 7, "projects": {"/x": {"a": 1}}}))
    inode = claude_json.stat().st_ino

    added = wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    assert "Successfully created remote workspace" in added.stdout, added.stderr
    window = get_window_name("feat")
    wait_for_pane_output(mux_server, window, "FAKE-CLAUDE-READY", timeout=20)
    assert log.read_text().splitlines() == ["ARGS:"]

    # The node marked its worktrees as trusted, in place, keeping the rest.
    config = json.loads(claude_json.read_text())
    assert (
        config["projects"][str(node.root / "projects")]["hasTrustDialogAccepted"]
        is True
    )
    assert config["numStartups"] == 7
    assert config["projects"]["/x"] == {"a": 1}
    assert claude_json.stat().st_ino == inode

    # A reboot: the node's tmux server is gone and the kernel boot differs.
    [workspace] = node.workspaces()
    path = node.root / "state" / "workspaces" / f"{workspace['ws_id']}.json"
    data = json.loads(path.read_text())
    assert data["execution"]["boot_id"]
    data["execution"]["boot_id"] = "boot-before-reboot"
    path.write_text(json.dumps(data))
    node.tmux("kill-server")

    assert poll_until(
        lambda: [
            a["remote"].get("ended")
            for a in remote_status(mux_server, workmux_exe_path, repo)
        ]
        == ["host_reboot"],
        timeout=30,
    ), remote_status(mux_server, workmux_exe_path, repo)
    text = wm(mux_server, workmux_exe_path, repo, "status").stdout
    assert "exited (host rebooted)" in text, text
    listing = wm(mux_server, workmux_exe_path, repo, "list").stdout
    assert "exited (host rebooted)" in listing, listing

    # `open` replaces the stale window and resumes the conversation.
    [before] = local_records(mux_server)
    opened = wm(mux_server, workmux_exe_path, repo, "open feat")
    assert (
        "Resumed 'feat' on 'lo' after the host rebooted; the agent continues its "
        "last conversation" in opened.stdout
    ), opened.stderr
    [after] = local_records(mux_server)
    assert after["execution"]["ex_id"] != before["execution"]["ex_id"]
    assert poll_until(lambda: len(log.read_text().splitlines()) == 2, timeout=20)
    assert log.read_text().splitlines()[1] == "ARGS:--continue"
    wait_for_pane_output(mux_server, window, "FAKE-CLAUDE-READY --continue", timeout=20)
    assert poll_until(
        lambda: [
            a["remote"].get("ended")
            for a in remote_status(mux_server, workmux_exe_path, repo)
        ]
        == [None],
        timeout=30,
    )

    # A resume interrupted before its agents started still continues.
    data = json.loads(path.read_text())
    assert data["execution"]["continues"] is True
    data["execution"]["state"] = "starting"
    path.write_text(json.dumps(data))
    node.tmux("kill-server")
    wm(mux_server, workmux_exe_path, repo, "open feat")
    assert poll_until(lambda: len(log.read_text().splitlines()) == 3, timeout=20)
    assert log.read_text().splitlines()[2] == "ARGS:--continue"
    wait_for_pane_output(mux_server, window, "FAKE-CLAUDE-READY --continue", timeout=20)

    # A session that dies without a reboot starts fresh.
    node.tmux("kill-server")
    opened = wm(mux_server, workmux_exe_path, repo, "open feat")
    assert "Started remote workspace 'feat' on 'lo'" in opened.stdout, opened.stderr
    assert poll_until(lambda: len(log.read_text().splitlines()) == 4, timeout=20), (
        opened
    )
    assert log.read_text().splitlines()[3] == "ARGS:"
