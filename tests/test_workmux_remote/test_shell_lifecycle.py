"""Shell environment, reconnect, and cancellation regressions."""

import json
import subprocess

import pytest

from ..conftest import get_window_name, poll_until
from .conftest import LOOPBACK, agent_dirs_env, commit_project, wm
from .test_shell import pane_text, split_running_shell

pytestmark = pytest.mark.tmux_only


def setup(env, exe, repo, node, *, nested=False):
    config_dir = repo / "app" if nested else repo
    config_dir.mkdir(exist_ok=True)
    commit_project(
        config_dir, {"nerdfont": False, "panes": [{"command": "echo AGENT-READY"}]}
    )
    wm(env, exe, config_dir, "add feat --remote lo")
    [workspace] = node.workspaces()
    return workspace, get_window_name("feat")


def wait_shell(env, exe, window, node, args="feat"):
    before = set(node.sessions())
    pane = split_running_shell(env, exe, window, args)
    assert poll_until(lambda: bool(set(node.sessions()) - before), timeout=20)
    [session] = list(set(node.sessions()) - before)
    assert poll_until(lambda: "on the host" in pane_text(env, pane), timeout=20)
    assert poll_until(
        lambda: bool(
            node.tmux(
                "list-clients", "-t", session, "-F", "#{client_pid}"
            ).stdout.strip()
        ),
        timeout=20,
    )
    return pane, session


def test_host_shell_receives_workspace_environment(
    mux_server, workmux_exe_path, mux_repo_path, node
):
    commit_project(
        mux_repo_path, {"agent": "claude", "panes": [{"command": "echo AGENT-READY"}]}
    )
    wm(mux_server, workmux_exe_path, mux_repo_path, "add feat --remote lo")
    [ws] = node.workspaces()
    window = get_window_name("feat")
    directory = node.root / "secrets"
    directory.mkdir(exist_ok=True)
    (directory / "node.json").write_text(
        json.dumps({"CLAUDE_CODE_OAUTH_TOKEN": "review-token-value"})
    )
    pane, session = wait_shell(mux_server, workmux_exe_path, window, node)
    assert (
        "review-token-value"
        in node.tmux(
            "show-environment", "-t", session, "CLAUDE_CODE_OAUTH_TOKEN"
        ).stdout
    )
    mux_server.tmux(
        [
            "send-keys",
            "-t",
            pane,
            'printf "TOKEN=%s\\n" "${CLAUDE_CODE_OAUTH_TOKEN:-missing}"',
            "Enter",
        ]
    )
    assert poll_until(
        lambda: "TOKEN=review-token-value" in pane_text(mux_server, pane), timeout=5
    ), pane_text(mux_server, pane)


def test_shell_reconnect_and_gc(mux_server, workmux_exe_path, mux_repo_path, node):
    ws, window = setup(mux_server, workmux_exe_path, mux_repo_path, node)
    pane, session = wait_shell(mux_server, workmux_exe_path, window, node)
    mux_server.tmux(
        [
            "send-keys",
            "-t",
            pane,
            "export REVIEW_VALUE=retained; printf 'SET-%s\\n' \"$REVIEW_VALUE\"",
            "Enter",
        ]
    )
    assert poll_until(lambda: "SET-retained" in pane_text(mux_server, pane), timeout=5)
    client = node.tmux(
        "list-clients", "-t", session, "-F", "#{client_pid}"
    ).stdout.strip()
    import os
    import signal

    os.kill(int(client), signal.SIGKILL)
    assert poll_until(
        lambda: node.tmux(
            "list-clients", "-t", session, "-F", "#{client_pid}"
        ).stdout.strip()
        not in ("", client),
        timeout=30,
    )
    mux_server.tmux(
        ["send-keys", "-t", pane, "printf 'AFTER-%s\\n' \"$REVIEW_VALUE\"", "Enter"]
    )
    assert poll_until(
        lambda: "AFTER-retained" in pane_text(mux_server, pane), timeout=10
    ), pane_text(mux_server, pane)
    gc = wm(mux_server, workmux_exe_path, mux_repo_path, "remote gc lo --dry-run")
    assert session not in gc.stdout
    mux_server.tmux(["send-keys", "-t", pane, "exit", "Enter"])
    assert poll_until(lambda: "LOCAL-BACK-0" in pane_text(mux_server, pane), timeout=20)
    assert node.sessions() == [f"{ws['execution']['ex_id']}-0"]


def test_multiple_shells_are_independent(
    mux_server, workmux_exe_path, mux_repo_path, node
):
    ws, window = setup(mux_server, workmux_exe_path, mux_repo_path, node)
    pane1, session1 = wait_shell(mux_server, workmux_exe_path, window, node)
    pane2, session2 = wait_shell(mux_server, workmux_exe_path, window, node)
    assert session1 != session2
    mux_server.tmux(["send-keys", "-t", pane1, "exit", "Enter"])
    assert poll_until(lambda: session1 not in node.sessions(), timeout=20)
    assert session2 in node.sessions()
    mux_server.tmux(["send-keys", "-t", pane2, "echo STILL-$((6*7))", "Enter"])
    assert poll_until(lambda: "STILL-42" in pane_text(mux_server, pane2), timeout=10)
    mux_server.tmux(["kill-pane", "-t", pane2])
    assert poll_until(
        lambda: node.sessions() == [f"{ws['execution']['ex_id']}-0"], timeout=20
    )


def test_nested_shell_directory(mux_server, workmux_exe_path, mux_repo_path, node):
    ws, window = setup(mux_server, workmux_exe_path, mux_repo_path, node, nested=True)
    pane, _ = wait_shell(mux_server, workmux_exe_path, window, node)
    mux_server.tmux(
        ["send-keys", "-t", pane, 'printf "CWD=%s\\n" "$(pwd -P)"', "Enter"]
    )
    import os

    expected = "CWD=" + os.path.realpath(ws["worktree_path"] + "/app")
    assert poll_until(lambda: expected in pane_text(mux_server, pane), timeout=10), (
        pane_text(mux_server, pane)
    )


def test_noninteractive_refuses_before_creation(
    mux_server, workmux_exe_path, mux_repo_path, node
):
    setup(mux_server, workmux_exe_path, mux_repo_path, node)
    before = node.sessions()
    result = subprocess.run(
        [str(workmux_exe_path), "shell", "feat"],
        cwd=mux_repo_path,
        env={**mux_server.env, **LOOPBACK, **agent_dirs_env(mux_server.home_path)},
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0
    assert "needs a terminal" in result.stderr
    assert node.sessions() == before


def test_shell_close_cannot_kill_agent(
    mux_server, workmux_exe_path, mux_repo_path, node
):
    ws, _ = setup(mux_server, workmux_exe_path, mux_repo_path, node)
    result = subprocess.run(
        [str(workmux_exe_path), "node", "shell", "--json"],
        input=json.dumps(
            {
                "action": "close",
                "ws_id": ws["ws_id"],
                "ex_id": ws["execution"]["ex_id"],
                "slot": "0",
            }
        ),
        env={**mux_server.env, "WORKMUX_NODE_ROOT": str(node.root)},
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0
    assert "not a shell session" in result.stdout + result.stderr
    assert node.sessions() == [f"{ws['execution']['ex_id']}-0"]


@pytest.mark.parametrize("response", ["delayed", "lost"])
def test_closing_during_open_does_not_leak_shell(
    mux_server, workmux_exe_path, mux_repo_path, node, response
):
    import sys
    import yaml

    wrapper = mux_server.home_path / "delayed-shell-node"
    wrapper.write_text(
        f"#!{sys.executable}\n"
        "import json, os, subprocess, sys, time\n"
        f"binary = {str(workmux_exe_path)!r}\n"
        "if sys.argv[1:3] != ['node', 'shell']:\n"
        "    os.execv(binary, [binary, *sys.argv[1:]])\n"
        "raw = sys.stdin.buffer.read()\n"
        "result = subprocess.run([binary, *sys.argv[1:]], input=raw, capture_output=True)\n"
        "if json.loads(raw).get('action') == 'open':\n"
        f"    if {response!r} == 'lost': sys.exit(75)\n"
        "    time.sleep(3)\n"
        "sys.stdout.buffer.write(result.stdout)\n"
        "sys.stderr.buffer.write(result.stderr)\n"
        "sys.exit(result.returncode)\n"
    )
    wrapper.chmod(0o755)
    config_path = mux_server.home_path / ".config/workmux/config.yaml"
    config = yaml.safe_load(config_path.read_text())
    config["remotes"]["lo"]["binary"] = str(wrapper)
    config_path.write_text(yaml.safe_dump(config))
    ws, window = setup(mux_server, workmux_exe_path, mux_repo_path, node)
    pane = split_running_shell(mux_server, workmux_exe_path, window, "feat")
    if response == "delayed":
        assert poll_until(lambda: len(node.sessions()) == 2, timeout=20)
        mux_server.tmux(["kill-pane", "-t", pane])
    else:
        assert poll_until(
            lambda: "LOCAL-BACK-1" in pane_text(mux_server, pane), timeout=20
        ), pane_text(mux_server, pane)
    assert poll_until(
        lambda: node.sessions() == [f"{ws['execution']['ex_id']}-0"], timeout=10
    ), node.sessions()


def test_cancel_before_open_prevents_late_creation(
    mux_server, workmux_exe_path, mux_repo_path, node
):
    ws, _ = setup(mux_server, workmux_exe_path, mux_repo_path, node)
    identity = {
        "ws_id": ws["ws_id"],
        "ex_id": ws["execution"]["ex_id"],
        "slot": "shell12345",
    }

    def call(action):
        return subprocess.run(
            [str(workmux_exe_path), "node", "shell", "--json"],
            input=json.dumps({"action": action, **identity}),
            env={**mux_server.env, "WORKMUX_NODE_ROOT": str(node.root)},
            capture_output=True,
            text=True,
        )

    closed = call("close")
    assert closed.returncode == 0, closed.stdout + closed.stderr
    opened = call("open")
    assert opened.returncode != 0
    assert "this shell request was canceled" in opened.stdout + opened.stderr
    assert node.sessions() == [f"{ws['execution']['ex_id']}-0"]
