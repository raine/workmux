"""Sidebar lifecycle notifications through real tmux hooks."""

import subprocess
import sys
from pathlib import Path

import pytest

from .conftest import TmuxEnvironment, poll_until
from .test_popup_navigation import attached_client

pytestmark = pytest.mark.tmux_only


@pytest.mark.parametrize("remaining_content", [False, True], ids=["last", "split"])
def test_natural_pane_exit_wakes_sidebar_daemon(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, remaining_content: bool
):
    env = mux_server
    # Unix sidebar sockets must fit macOS sockaddr_un, unlike pytest paths.
    env.env["TMPDIR"] = "/tmp"
    env.tmux(["set-environment", "-g", "TMPDIR", "/tmp"])
    env.env["TMUX"] = f"{env.socket_path},0,0"
    assert env.runner_pane_id is not None
    env.env["TMUX_PANE"] = env.runner_pane_id
    config = env.home_path / ".config/workmux/config.yaml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text("sidebar:\n  git_status: false\n")
    window, content = env.tmux(
        [
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{window_id} #{pane_id}",
            "/bin/sh",
        ]
    ).stdout.split()
    survivor = None
    if remaining_content:
        survivor = env.tmux(
            ["split-window", "-d", "-t", content, "-P", "-F", "#{pane_id}"]
        ).stdout.strip()

    def cli(action: str):
        env.run_command([str(workmux_exe_path), "sidebar", action])

    def pane_ids():
        return env.tmux(["list-panes", "-a", "-F", "#{pane_id}"]).stdout.split()

    with attached_client(env):
        cli("on")
        daemon_pid = env.tmux(
            ["show-option", "-gqv", "@workmux_sidebar_daemon_pid"]
        ).stdout.strip()
        try:
            sidebar = next(
                line.split()[0]
                for line in env.tmux(
                    ["list-panes", "-t", window, "-F", "#{pane_id} #{@workmux_role}"]
                ).stdout.splitlines()
                if line.endswith(" sidebar")
            )
            assert poll_until(
                lambda: (
                    "No agents"
                    in env.tmux(["capture-pane", "-p", "-t", sidebar]).stdout
                ),
                timeout=10,
            )

            # Forward one actual hook signal to the daemon. Waiting for the signal,
            # rather than timing the window closure, cannot pass via fallback polling.
            with subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    (
                        "import os, signal, sys; "
                        "signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGUSR1}); "
                        "print('ready', flush=True); "
                        "signal.sigwait({signal.SIGUSR1}); "
                        "os.kill(int(sys.argv[1]), signal.SIGUSR1)"
                    ),
                    daemon_pid,
                ],
                stdout=subprocess.PIPE,
                text=True,
            ) as relay:
                try:
                    assert relay.stdout is not None
                    assert relay.stdout.readline().strip() == "ready"
                    env.tmux(
                        [
                            "set-option",
                            "-g",
                            "@workmux_sidebar_daemon_pid",
                            str(relay.pid),
                        ]
                    )
                    # The affected window is in the background: do not mistake the
                    # active pane/window for the one whose content is exiting.
                    env.send_keys(content, "exit")
                    assert relay.wait(timeout=5) == 0
                finally:
                    env.tmux(
                        ["set-option", "-g", "@workmux_sidebar_daemon_pid", daemon_pid]
                    )
                    if relay.poll() is None:
                        relay.kill()

            assert poll_until(lambda: content not in pane_ids())
            if survivor is None:
                assert poll_until(lambda: window not in env.list_window_ids())
            else:
                assert {sidebar, survivor}.issubset(pane_ids())
            assert env.runner_pane_id in pane_ids()
        finally:
            cli("off")
        assert "pane-exited[98]" not in env.tmux(["show-hooks", "-g"]).stdout
