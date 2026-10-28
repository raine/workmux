"""`workmux shell` over the loopback transport: a shell in a remote
workspace, relayed into a local split of its window."""

import os
import shlex
from pathlib import Path

import pytest

from ..conftest import (
    TmuxEnvironment,
    get_scripts_dir,
    get_window_name,
    poll_until,
)
from .conftest import LOOPBACK, Node, agent_dirs_env, commit_project, wm

pytestmark = pytest.mark.tmux_only


def split_running_shell(env: TmuxEnvironment, exe: Path, window: str, args: str):
    """Split `window` and run `workmux shell <args>` in the new pane, which
    prints LOCAL-BACK with the exit status when the command returns."""
    script = get_scripts_dir(env) / "shell.sh"
    exports = {
        **LOOPBACK,
        **agent_dirs_env(env.home_path),
        "PATH": env.env["PATH"],
        "HOME": env.env["HOME"],
        "SHELL": "/bin/sh",
    }
    lines = [f"export {key}={shlex.quote(value)}" for key, value in exports.items()]
    script.write_text(
        "\n".join(
            [
                *lines,
                f"{shlex.quote(str(exe))} shell {args}",
                'echo "LOCAL-BACK-$?"',
                "exec sleep 600",
            ]
        )
        + "\n"
    )
    result = env.tmux(
        ["split-window", "-t", window, "-P", "-F", "#{pane_id}", f"/bin/sh {script}"]
    )
    return result.stdout.strip()


def pane_text(env: TmuxEnvironment, pane: str) -> str:
    return env.tmux(["capture-pane", "-p", "-J", "-t", pane]).stdout


def test_shell_opens_in_the_workspace_and_leaves_the_agent_running(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    commit_project(
        mux_repo_path,
        {"nerdfont": False, "panes": [{"command": "echo AGENT-READY"}]},
    )
    wm(mux_server, workmux_exe_path, mux_repo_path, "add feat --remote lo")
    [workspace] = node.workspaces()
    ex_id = workspace["execution"]["ex_id"]
    worktree = workspace["worktree_path"]
    window = get_window_name("feat")
    assert node.sessions() == [f"{ex_id}-0"]

    # Without a name, a fresh split names the workspace by its window.
    pane = split_running_shell(mux_server, workmux_exe_path, window, "")
    assert poll_until(lambda: len(node.sessions()) == 2, timeout=20)
    [shell] = [s for s in node.sessions() if s != f"{ex_id}-0"]
    assert shell.startswith(f"{ex_id}-shell")
    assert poll_until(
        lambda: "on the host" in pane_text(mux_server, pane), timeout=20
    ), pane_text(mux_server, pane)
    mux_server.tmux(
        [
            "send-keys",
            "-t",
            pane,
            'printf "AT=%s TERM=%s COLOR=%s\\n" "$(pwd -P)" "$TERM" "$COLORTERM"',
            "Enter",
        ]
    )
    expected = f"AT={os.path.realpath(worktree)} TERM=tmux-256color COLOR=truecolor"
    assert poll_until(lambda: expected in pane_text(mux_server, pane), timeout=20), (
        pane_text(mux_server, pane)
    )

    # Exiting the shell returns to the local pane; the agent keeps running.
    mux_server.tmux(["send-keys", "-t", pane, "exit", "Enter"])
    assert poll_until(
        lambda: "LOCAL-BACK-0" in pane_text(mux_server, pane), timeout=20
    ), pane_text(mux_server, pane)
    assert node.sessions() == [f"{ex_id}-0"]
    mux_server.tmux(["kill-pane", "-t", pane])

    # A shell whose terminal goes away is closed on the node.
    pane = split_running_shell(mux_server, workmux_exe_path, window, "feat")
    assert poll_until(lambda: len(node.sessions()) == 2, timeout=20)
    mux_server.tmux(["kill-pane", "-t", pane])
    assert poll_until(lambda: node.sessions() == [f"{ex_id}-0"], timeout=20)

    # Removing the workspace ends its shells with its panes.
    pane = split_running_shell(mux_server, workmux_exe_path, window, "feat")
    assert poll_until(lambda: len(node.sessions()) == 2, timeout=20)
    wm(mux_server, workmux_exe_path, mux_repo_path, "remove --force feat")
    assert node.sessions() == []


def test_shell_names_what_it_cannot_open(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    wm(mux_server, workmux_exe_path, mux_repo_path, "add here")
    local = wm(
        mux_server, workmux_exe_path, mux_repo_path, "shell here", expect_fail=True
    )
    assert "'here' is a local worktree" in local.stderr
    assert 'cd "$(workmux path here)"' in local.stderr
    missing = wm(
        mux_server, workmux_exe_path, mux_repo_path, "shell nope", expect_fail=True
    )
    assert "no remote workspace named 'nope'" in missing.stderr
    bare = wm(mux_server, workmux_exe_path, mux_repo_path, "shell", expect_fail=True)
    assert "not in the window of a remote workspace" in bare.stderr
