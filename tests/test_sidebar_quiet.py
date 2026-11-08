"""Quiet presentation through the real tmux daemon and sidebar."""

from pathlib import Path

import pytest

from .conftest import TmuxEnvironment, poll_until
from .test_popup_navigation import attached_client

pytestmark = pytest.mark.tmux_only


@pytest.mark.parametrize("nerdfont", [False, True])
def test_quiet_sidebar_hides_working_timer(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, nerdfont: bool
):
    env = mux_server
    # Unix sidebar sockets must fit macOS sockaddr_un.
    env.env["TMPDIR"] = "/tmp"
    env.tmux(["set-environment", "-g", "TMPDIR", "/tmp"])
    env.env["TMUX"] = f"{env.socket_path},0,0"
    runner = env.runner_pane_id
    assert runner is not None
    env.env["TMUX_PANE"] = runner
    config = env.home_path / ".config/workmux/config.yaml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text(
        f"nerdfont: {str(nerdfont).lower()}\n"
        "stale_after: 1h\n"
        "sidebar:\n"
        "  git_status: false\n"
        "  dim_stale: false\n"
        "  layout: compact\n"
        "  templates:\n"
        '    compact: "{status_icon} {status_label} timer={elapsed}"\n'
    )

    def cli(*args: str):
        return env.run_command([str(workmux_exe_path), *args])

    with attached_client(env):
        cli("sidebar", "on", "--width", "40")
        try:
            sidebar = next(
                line.split()[0]
                for line in env.tmux(
                    ["list-panes", "-t", "test:", "-F", "#{pane_id} #{@workmux_role}"]
                ).stdout.splitlines()
                if line.endswith(" sidebar")
            )

            def screen():
                return env.tmux(["capture-pane", "-p", "-t", sidebar]).stdout

            cli("set-window-status", "working")
            assert poll_until(lambda: "Working timer=0:" in screen(), timeout=10)
            assert poll_until(lambda: "Quiet" in screen(), timeout=25), screen()
            quiet_row = next(
                line.strip() for line in screen().splitlines() if "Quiet" in line
            )
            assert quiet_row == "○ Quiet timer="

            # Quiet is presentation only, not successful completion or status clearing.
            state = cli("status", "--json").stdout
            assert '"status": "working"' in state
            cli("set-window-status", "working")
            assert poll_until(lambda: "Working" in screen() and "Quiet" not in screen())
        finally:
            cli("sidebar", "off")
