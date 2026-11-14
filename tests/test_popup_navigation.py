"""Navigation checks with real PTY clients and keybinding-launched popups."""

import fcntl
import os
import pty
import select
import shlex
import struct
import subprocess
import termios
import threading
from contextlib import contextmanager
from pathlib import Path

import pytest

from .conftest import (
    TmuxEnvironment,
    get_session_name,
    poll_until,
    write_workmux_config,
)
from .test_workmux_add.conftest import add_branch_and_get_worktree

pytestmark = pytest.mark.tmux_only


@contextmanager
def attached_client(env: TmuxEnvironment):
    existing = set(
        env.tmux(["list-clients", "-F", "#{client_name}"]).stdout.splitlines()
    )
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    client_env = dict(env.env, TERM="xterm-256color")
    client_env.pop("TMUX", None)
    client_env.pop("TMUX_PANE", None)
    process = subprocess.Popen(
        ["tmux", "-S", str(env.socket_path), "attach-session", "-t", "test"],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=client_env,
    )
    os.close(slave)
    stop = threading.Event()

    def drain():
        while not stop.is_set():
            if select.select([master], [], [], 0.1)[0]:
                try:
                    if not os.read(master, 65536):
                        break
                except OSError:
                    break

    thread = threading.Thread(target=drain)
    thread.start()
    try:

        def new_clients():
            return (
                set(
                    env.tmux(
                        ["list-clients", "-F", "#{client_name}"]
                    ).stdout.splitlines()
                )
                - existing
            )

        assert poll_until(lambda: bool(new_clients()))
        client = new_clients().pop()
        yield master, client
    finally:
        process.kill()
        process.wait(timeout=5)
        stop.set()
        thread.join(timeout=5)
        os.close(master)


@pytest.mark.parametrize("operation", ["remove", "close"])
@pytest.mark.parametrize("popup", [False, True], ids=["pane", "popup"])
@pytest.mark.parametrize("explicit", [False, True], ids=["inferred", "explicit"])
@pytest.mark.parametrize(
    "current", [False, True], ids=["other-target", "current-target"]
)
def test_current_session_navigation(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    tmp_path: Path,
    operation: str,
    popup: bool,
    explicit: bool,
    current: bool,
):
    env = mux_server
    # Popup jobs inherit the server environment, unlike spawned panes.
    env.tmux(["set-environment", "-gu", "TMUX_PANE"])
    write_workmux_config(repo_path, panes=[{"command": "/bin/sh"}])
    branch = "popup-navigation"
    worktree = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch, extra_args="--session --background"
    )
    source = get_session_name(branch)
    preserved_file = worktree / "untracked.txt"
    if operation == "close":
        preserved_file.write_text("preserve me\n")
    env.tmux(["set-option", "-g", "detach-on-destroy", "on"])
    evidence = tmp_path / "pane-environment"
    command = shlex.join([str(workmux_exe_path), operation])
    if explicit:
        command += " " + branch
    if operation == "remove":
        command += " -f"
    command = (
        f"cd {shlex.quote(str(worktree))}; "
        f"printf '%s' \"${{TMUX_PANE-unset}}\" > {shlex.quote(str(evidence))}; "
        + command
    )

    with attached_client(env) as (master, client):
        if current:
            env.tmux(["switch-client", "-c", client, "-t", source])
        if popup:
            env.tmux(
                ["bind-key", "r", "display-popup", "-d", str(worktree), "-E", command]
            )
            os.write(master, b"\x02r")
        else:
            env.send_keys(f"={source if current else 'test'}:", command)
        assert poll_until(lambda: evidence.exists() and bool(evidence.read_text()))
        assert (evidence.read_text() == "unset") == popup
        assert poll_until(
            lambda: env.tmux(
                ["has-session", "-t", f"={source}"], check=False
            ).returncode
            != 0,
            timeout=10,
        )
        clients = env.tmux(
            ["list-clients", "-F", "#{client_name}\t#{session_name}"]
        ).stdout.splitlines()
        assert f"{client}\ttest" in clients, clients
        if operation == "remove":
            assert poll_until(lambda: not worktree.exists(), timeout=10)
        else:
            assert worktree.exists()
            assert (worktree / ".git").exists()
            assert preserved_file.read_text() == "preserve me\n"
            env.run_command(
                ["git", "show-ref", "--verify", f"refs/heads/{branch}"], cwd=repo_path
            )


@pytest.mark.parametrize("operation", ["remove", "close"])
def test_popup_current_window_navigation(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    tmp_path: Path,
    operation: str,
):
    env = mux_server
    env.tmux(["set-environment", "-gu", "TMUX_PANE"])
    write_workmux_config(repo_path, panes=[{"command": "/bin/sh"}])
    branch = "popup-window"
    worktree = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch, extra_args="--background"
    )
    target = f"test:={get_session_name(branch)}"
    window_id = env.tmux(
        ["display-message", "-p", "-t", target, "#{window_id}"]
    ).stdout.strip()
    state = tmp_path / "popup-state"
    command = (
        f"cd {shlex.quote(str(worktree))}; "
        f"export XDG_STATE_HOME={shlex.quote(str(state))} RUST_LOG=workmux=debug; "
        + shlex.join([str(workmux_exe_path), operation, branch])
        + (" -f" if operation == "remove" else "")
    )
    with attached_client(env) as (master, client):
        env.tmux(["select-window", "-t", target])
        env.tmux(["bind-key", "r", "display-popup", "-d", str(worktree), "-E", command])
        os.write(master, b"\x02r")
        assert poll_until(lambda: window_id not in env.list_window_ids(), timeout=10)
        clients = env.tmux(
            ["list-clients", "-F", "#{client_name}\t#{session_name}"]
        ).stdout.splitlines()
        assert f"{client}\ttest" in clients
        if operation == "remove":
            assert poll_until(lambda: not worktree.exists(), timeout=10)
            log = (state / "workmux" / "workmux.log").read_text()
            assert "running_inside_target=true" in log
            assert "deferred_cleanup=true" in log
        else:
            assert worktree.exists()
