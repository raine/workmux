"""Sidebar placement on private Rex servers.

Rex runs only on macOS, so these tests skip wherever `rex` is missing.
"""

import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from collections.abc import Iterator
from pathlib import Path
from typing import Any, Optional

import pytest

from .conftest import RexEnvironment, poll_until

SIDEBAR_LABEL = "workmux-sidebar"


class RexSidebar:
    def __init__(self, env: RexEnvironment, exe: Path):
        self.env = env
        self.exe = exe

    def workmux(
        self, *args: str, session: Optional[str] = None, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        env = dict(self.env.env)
        env["WORKMUX_BACKEND"] = "rex"
        if session is not None:
            env["REX_SESSION"] = session
        result = subprocess.run(
            [str(self.exe), *args],
            cwd=self.env.tmp_path,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        if check and result.returncode != 0:
            pytest.fail(f"workmux {' '.join(args)} failed:\n{result.stderr}")
        return result

    def sidebar(self, *args: str, session: Optional[str] = None) -> None:
        self.workmux("sidebar", *args, session=session)

    def rex(self, *args: str) -> str:
        return self.env.mux_command(list(args)).stdout

    @property
    def session(self) -> str:
        assert self.env.session_id is not None
        return self.env.session_id

    def api(
        self, method: str, args: dict[str, Any], session: Optional[str] = None
    ) -> Any:
        command = ["api", "call"]
        if session is not None:
            command += ["-s", session]
        out = self.rex(*command, method, json.dumps(args)).strip()
        return json.loads(out) if out else None

    def new_session(self, label: str) -> str:
        created = json.loads(
            self.rex(
                "new",
                label,
                "-c",
                str(self.env.tmp_path),
                "--shell=none",
                "--json",
                "--",
                "/bin/sleep",
                "3600",
            )
        )
        return created["session_id"]

    def new_window(self, session: str, *command: str) -> str:
        command = command or ("/bin/sleep", "3600")
        created = json.loads(
            self.rex(
                "window",
                "new",
                "-s",
                session,
                "--focus=false",
                "--shell=none",
                "--json",
                "--",
                *command,
            )
        )
        return created["window_id"]

    def sessions(self) -> list[str]:
        listed = self.api("session.list", {})
        return [s["session_id"] for s in listed.get("sessions") or []]

    def windows(self) -> list[dict[str, Any]]:
        """Every window with its session, tiled layout and blocks."""
        windows = []
        for session in self.sessions():
            view = self.api("session.view", {"session_id": session})
            for window in view.get("windows") or []:
                tiled = next(
                    (
                        layer
                        for layer in window.get("layers") or []
                        if layer["kind"] == "tiled"
                    ),
                    None,
                )
                windows.append(
                    {
                        "session_id": session,
                        "window_id": window["window_id"],
                        "focused": window.get("focused_block_id"),
                        "layout": tiled["layout"] if tiled else None,
                        "blocks": tiled["blocks"] if tiled else [],
                    }
                )
        return windows

    def window(self, window_id: str) -> Optional[dict[str, Any]]:
        return next((w for w in self.windows() if w["window_id"] == window_id), None)

    def open_window(self, window_id: str) -> dict[str, Any]:
        window = self.window(window_id)
        assert window is not None, f"window {window_id} is gone"
        return window

    @staticmethod
    def sidebars(window: dict[str, Any]) -> list[str]:
        return [
            b["block_id"] for b in window["blocks"] if b.get("label") == SIDEBAR_LABEL
        ]

    @staticmethod
    def root_sidebar(window: dict[str, Any]) -> Optional[str]:
        """The sidebar wrapping the window's tiled tree, if there is one."""
        split = (window["layout"] or {}).get("split")
        if not split or split["direction"] != "horizontal":
            return None
        block = split["before"].get("block_id")
        return block if block in RexSidebar.sidebars(window) else None

    @staticmethod
    def sidebar_block(window: dict[str, Any]) -> str:
        block = RexSidebar.root_sidebar(window)
        assert block is not None, f"no sidebar in {window['window_id']}"
        return block

    def wait_for(self, predicate, timeout: float = 10.0, message: str = "") -> None:
        if not poll_until(predicate, timeout=timeout):
            pytest.fail(f"timed out: {message}\n{json.dumps(self.windows(), indent=1)}")

    def each_window_has_one_sidebar(
        self, window_ids: Optional[set[str]] = None
    ) -> bool:
        windows = [
            w
            for w in self.windows()
            if window_ids is None or w["window_id"] in window_ids
        ]
        return all(
            len(self.sidebars(w)) == 1 and self.root_sidebar(w) is not None
            for w in windows
        )

    def no_sidebars(self) -> bool:
        return all(not self.sidebars(w) for w in self.windows())

    def state_file(self) -> Path:
        files = list(Path(self.env.env["TMPDIR"]).glob("workmux-sidebar-state-*.json"))
        assert len(files) == 1, files
        return files[0]

    def state(self) -> dict[str, Any]:
        return json.loads(self.state_file().read_text())

    def daemon_pid(self) -> Optional[int]:
        return self.state().get("daemon_pid")

    def running_daemon(self) -> int:
        pid = self.daemon_pid()
        assert pid is not None and pid_alive(pid), pid
        return pid


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


@pytest.fixture
def rex_sidebar(tmp_path: Path, workmux_exe_path: Path) -> Iterator[RexSidebar]:
    if sys.platform != "darwin":
        pytest.skip("Rex runs only on macOS")
    from .conftest import skip_if_backend_unavailable

    skip_if_backend_unavailable("rex")
    env = RexEnvironment(tmp_path)
    # The daemon's socket lives in TMPDIR, which must stay under the macOS
    # limit for Unix socket paths
    short_tmp = tempfile.mkdtemp(prefix="wmx_")
    env.env["TMPDIR"] = short_tmp
    env.start_server()
    harness = RexSidebar(env, workmux_exe_path)
    try:
        yield harness
    finally:
        try:
            harness.workmux("sidebar", "off", check=False)
        finally:
            env.stop_server()
            shutil.rmtree(short_tmp, ignore_errors=True)


def test_on_wraps_every_window_and_keeps_focus(rex_sidebar: RexSidebar):
    s = rex_sidebar
    session = s.session
    background = s.new_window(session)
    other = s.new_session("other")
    focused = {w["window_id"]: w["focused"] for w in s.windows()}

    s.sidebar("on")

    windows = s.windows()
    assert len(windows) == 3
    assert s.each_window_has_one_sidebar()
    for window in windows:
        assert window["focused"] == focused[window["window_id"]]
        assert 0.05 < window["layout"]["split"]["ratio"] < 0.5
    assert {w["session_id"] for w in windows} == {session, other}
    assert background in {w["window_id"] for w in windows}


def test_sidebar_runs_the_workmux_tui(rex_sidebar: RexSidebar):
    s = rex_sidebar
    s.sidebar("on")
    window = s.windows()[0]
    sidebar = s.sidebar_block(window)

    def shows_tui() -> bool:
        out = s.env.mux_command(
            ["capture", "-s", window["session_id"], "-b", sidebar], check=False
        ).stdout
        return "No agents" in out

    s.wait_for(shows_tui, message="sidebar TUI did not render")
    proc = json.loads(
        s.rex(
            "block",
            "call",
            "-s",
            window["session_id"],
            "-b",
            sidebar,
            "process",
        )
    )
    assert proc["child"]["name"] == "workmux"


def test_new_windows_and_sessions_get_a_sidebar(rex_sidebar: RexSidebar):
    s = rex_sidebar
    s.sidebar("on")
    window = s.new_window(s.session)
    s.new_session("later")

    s.wait_for(s.each_window_has_one_sidebar, message="no sidebar in new windows")
    assert len(s.windows()) == 3
    assert s.root_sidebar(s.open_window(window))


def test_off_removes_sidebars_and_keeps_content_ratios(rex_sidebar: RexSidebar):
    s = rex_sidebar
    session = s.session
    s.sidebar("on")
    window = s.windows()[0]
    content = window["layout"]["split"]["after"]["block_id"]
    s.api(
        "session.new_split",
        {
            "session_id": session,
            "anchor_id": content,
            "direction": "vertical",
            "side": "after",
            "ratio": 0.3,
            "layout": {
                "block": {
                    "flavor": "com.superlogical.terminal.shell",
                    "options": {"command": ["/bin/sleep", "3600"]},
                }
            },
        },
    )
    s.wait_for(s.each_window_has_one_sidebar, message="sidebar moved after a split")
    daemon = s.running_daemon()

    s.sidebar("off")

    assert s.no_sidebars()
    layout = s.open_window(window["window_id"])["layout"]
    assert layout["split"]["direction"] == "vertical"
    assert layout["split"]["ratio"] == pytest.approx(0.3)
    s.wait_for(lambda: not pid_alive(daemon), message="daemon did not exit")


def test_closed_sidebar_stays_closed_until_on(rex_sidebar: RexSidebar):
    s = rex_sidebar
    s.new_window(s.session)
    s.sidebar("on")
    first, second = s.windows()
    s.api(
        "block.close",
        {"block_id": s.root_sidebar(first), "keep_last": True},
        session=first["session_id"],
    )

    time.sleep(5)
    assert not s.sidebars(s.open_window(first["window_id"]))
    assert s.root_sidebar(s.open_window(second["window_id"]))

    s.sidebar("on")
    assert s.each_window_has_one_sidebar()


def test_session_scope_and_optout(rex_sidebar: RexSidebar):
    s = rex_sidebar
    main = s.session
    other = s.new_session("other")

    s.sidebar("--session", "on", session=main)
    by_session = {w["session_id"]: w for w in s.windows()}
    assert s.root_sidebar(by_session[main])
    assert not s.sidebars(by_session[other])

    s.sidebar("on")
    s.sidebar("--session", "off", session=other)
    by_session = {w["session_id"]: w for w in s.windows()}
    assert s.root_sidebar(by_session[main])
    assert not s.sidebars(by_session[other])
    s.new_window(other)
    time.sleep(4)
    assert all(not s.sidebars(w) for w in s.windows() if w["session_id"] == other)

    s.sidebar("--session", session=other)
    assert s.each_window_has_one_sidebar()


def test_session_filter_jump_skips_agents_without_a_live_pane(
    rex_sidebar: RexSidebar,
):
    s = rex_sidebar
    main = s.session
    other = s.new_session("other")
    blocks = {w["session_id"]: w["blocks"][0]["block_id"] for w in s.windows()}

    s.sidebar("filter", "session", session=main)
    # No daemon runs, so the published agent order stays as written.
    state = s.state()
    state["ordered_agents"] = ["gone-block", blocks[other], blocks[main]]
    s.state_file().write_text(json.dumps(state))

    jumped = s.workmux("sidebar", "jump", "1", session=main, check=False)
    assert jumped.returncode == 0, jumped.stderr
    out_of_range = s.workmux("sidebar", "jump", "2", session=main, check=False)
    assert out_of_range.returncode != 0
    assert "out of range (1-1)" in out_of_range.stderr


def test_window_closes_when_its_content_exits(rex_sidebar: RexSidebar):
    s = rex_sidebar
    s.sidebar("on")
    window = s.new_window(s.session, "/bin/sleep", "6")
    s.wait_for(
        lambda: s.root_sidebar(s.open_window(window)) is not None, message="no sidebar"
    )

    s.wait_for(
        lambda: s.window(window) is None,
        timeout=20,
        message="window with only a sidebar stayed open",
    )


def test_restarted_daemon_adopts_sidebars(rex_sidebar: RexSidebar):
    s = rex_sidebar
    s.new_window(s.session)
    s.sidebar("on")
    before = sorted(b for w in s.windows() for b in s.sidebars(w))
    daemon = s.running_daemon()
    os.kill(daemon, signal.SIGKILL)
    s.wait_for(lambda: not pid_alive(daemon), message="daemon survived SIGKILL")

    s.sidebar("on")

    assert sorted(b for w in s.windows() for b in s.sidebars(w)) == before
    assert s.running_daemon() != daemon


def test_concurrent_on_places_one_sidebar_per_window(rex_sidebar: RexSidebar):
    s = rex_sidebar
    s.new_window(s.session)
    s.new_session("other")
    env = dict(s.env.env, WORKMUX_BACKEND="rex")
    runs = [
        subprocess.Popen(
            [str(s.exe), "sidebar", "on"],
            cwd=s.env.tmp_path,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        for _ in range(3)
    ]
    for run in runs:
        _, stderr = run.communicate(timeout=30)
        assert run.returncode == 0, stderr

    time.sleep(3)
    assert s.each_window_has_one_sidebar()
    daemons = subprocess.run(
        ["pgrep", "-f", f"{s.exe} _sidebar-daemon"],
        capture_output=True,
        text=True,
        check=False,
    ).stdout.split()
    alive = [
        pid
        for pid in daemons
        if f"REX_SERVER={s.env.server_endpoint}"
        in subprocess.run(
            ["ps", "eww", "-o", "command=", "-p", pid],
            capture_output=True,
            text=True,
            check=False,
        ).stdout
    ]
    assert len(alive) == 1, alive


def test_quitting_a_sidebar_turns_it_off(rex_sidebar: RexSidebar):
    s = rex_sidebar
    s.new_window(s.session)
    s.sidebar("on")
    window = s.windows()[0]
    sidebar = s.sidebar_block(window)
    s.wait_for(
        lambda: "No agents"
        in s.env.mux_command(
            ["capture", "-s", window["session_id"], "-b", sidebar], check=False
        ).stdout,
        message="sidebar TUI did not render",
    )

    # Quitting asks for confirmation
    s.rex("send", "-s", window["session_id"], "-b", sidebar, "q")
    time.sleep(0.3)
    s.rex("send", "-s", window["session_id"], "-b", sidebar, "y")

    s.wait_for(s.no_sidebars, message="sidebars stayed after quitting one")
    assert not s.state().get("enabled")


def event_followers(s: RexSidebar) -> list[str]:
    return subprocess.run(
        ["pgrep", "-f", f"{s.env.server_endpoint} --autostart=false events"],
        capture_output=True,
        text=True,
        check=False,
    ).stdout.split()


def test_events_place_sidebars_before_the_next_poll(rex_sidebar: RexSidebar):
    s = rex_sidebar
    s.sidebar("on")
    s.wait_for(lambda: len(event_followers(s)) == 1, message="no event follower")
    for _ in range(3):
        window = s.new_window(s.session)
        started = time.monotonic()
        s.wait_for(
            lambda: s.root_sidebar(s.open_window(window)) is not None,
            message="no sidebar in the new window",
        )
        assert time.monotonic() - started < 1.5

    # A dropped stream reconnects
    for pid in event_followers(s):
        os.kill(int(pid), signal.SIGKILL)
    s.wait_for(
        lambda: len(event_followers(s)) == 1, message="follower did not reconnect"
    )

    s.sidebar("off")
    s.wait_for(lambda: not event_followers(s), message="follower outlived the daemon")


def test_views_toggle_from_the_cli_and_the_sidebar(rex_sidebar: RexSidebar):
    s = rex_sidebar
    config = s.env.home_path / ".config/workmux/config.yaml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text(
        "sidebar:\n  git_status: false\n  views:\n"
        "    - name: Nowhere\n      sessions: [absent]\n"
        "    - name: Mine\n      other: true\n"
    )
    s.sidebar("on")
    window = s.windows()[0]

    def view_names() -> set[str]:
        return {view["name"] for view in s.state().get("window_views", {}).values()}

    s.wait_for(lambda: view_names() == {"Mine"}, message="no window view published")

    s.sidebar("view", "off")
    assert s.state()["views_enabled"] is False
    s.wait_for(lambda: view_names() == {"All"}, message="views stayed on")

    sidebar = s.sidebar_block(window)

    def footer_shows(view: str) -> bool:
        return (
            f"[view: {view}]"
            in s.env.mux_command(
                ["capture", "-s", window["session_id"], "-b", sidebar], check=False
            ).stdout
        )

    s.wait_for(lambda: footer_shows("All"), message="sidebar does not show All")
    s.rex("send", "-s", window["session_id"], "-b", sidebar, "V")
    s.wait_for(lambda: footer_shows("Mine"), message="V did not turn views back on")
    assert s.state()["views_enabled"] is True
