"""Per-window sidebar views through the real CLI, daemon and TUI."""

import json
from pathlib import Path

import pytest

from .conftest import TmuxEnvironment, poll_until
from .test_popup_navigation import attached_client

pytestmark = pytest.mark.tmux_only


@pytest.mark.parametrize("position", ["left", "top"])
def test_sidebar_view_toggle_and_config_reload(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path, position: str
):
    env = mux_server
    # Unix sidebar sockets must fit macOS sockaddr_un, unlike pytest paths.
    env.env["TMPDIR"] = "/tmp"
    env.tmux(["set-environment", "-g", "TMPDIR", "/tmp"])
    config = env.home_path / ".config/workmux/config.yaml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text(
        "sidebar:\n  git_status: false\n  views:\n"
        "    - name: Named sessions\n      sessions: [test, foo, bar, xyz]\n"
        f"    - name: Code\n      project_dirs: [{repo_path.parent}]\n"
        "    - name: Empty\n      sessions: [absent]\n"
        "    - name: Other\n      other: true\n"
    )
    env.env["TMUX"] = f"{env.socket_path},0,0"
    runner_pane = env.runner_pane_id
    assert runner_pane is not None
    env.env["TMUX_PANE"] = runner_pane

    def cli(*args: str, check: bool = True):
        result = env.run_command(
            [str(workmux_exe_path), "sidebar", *args], cwd=repo_path, check=False
        )
        if check:
            assert result.returncode == 0, result.stderr
        return result

    assert cli("view", "Named sessions", check=False).returncode != 0
    assert cli("view", "--clear", check=False).returncode != 0
    cli("view", "off")
    assert (
        env.tmux(
            ["show-option", "-gqv", "@workmux_sidebar_views_enabled"]
        ).stdout.strip()
        == "off"
    )
    cli("view")
    assert (
        env.tmux(
            ["show-option", "-gqv", "@workmux_sidebar_views_enabled"]
        ).stdout.strip()
        == "on"
    )

    with attached_client(env):
        cli("on", "--position", position, "--width", "40", "--height", "4")
        try:

            def inventory():
                raw = env.tmux(
                    [
                        "show-option",
                        "-wqv",
                        "-t",
                        runner_pane,
                        "@workmux_sidebar_scope",
                    ]
                ).stdout
                if not raw.strip():
                    return env.tmux(
                        ["show-option", "-gqv", "@workmux_sidebar_agents"]
                    ).stdout.split()
                return json.loads(raw)["pane_ids"]

            panes = env.tmux(
                ["list-panes", "-t", "test:", "-F", "#{pane_id}"]
            ).stdout.splitlines()
            sidebar = next(pane for pane in panes if pane != runner_pane)

            def screen():
                return env.tmux(["capture-pane", "-p", "-t", sidebar]).stdout

            assert poll_until(
                lambda: "[view: Named sessions]" in screen().splitlines()[-1]
                and "No matching agents" in screen(),
                timeout=10,
            )
            env.run_command(
                [str(workmux_exe_path), "set-window-status", "working"], cwd=repo_path
            )
            assert poll_until(lambda: runner_pane in inventory(), timeout=10)
            cli("view", "off")
            assert poll_until(lambda: "[view: All]" in screen() and bool(inventory()))
            env.tmux(["select-pane", "-t", sidebar])
            env.tmux(["send-keys", "-t", sidebar, "V"])
            assert poll_until(lambda: "[view: Named sessions]" in screen())

            # Automatic selection follows renamed and removed rules on reload.
            config.write_text(
                config.read_text().replace("name: Named sessions", "name: Renamed")
            )
            assert poll_until(lambda: "[view: Renamed]" in screen(), timeout=10)
            config.write_text(
                config.read_text().replace(
                    "sessions: [test, foo, bar, xyz]", "sessions: [foo]"
                )
            )
            assert poll_until(lambda: "[view: Code]" in screen(), timeout=10)
            config.write_text("sidebar:\n  git_status: false\n")
            assert poll_until(
                lambda: "[view:" not in screen() and bool(inventory()), timeout=10
            )
            cli("view", "off")
            assert "[view:" not in screen()
        finally:
            cli("off")


@pytest.mark.parametrize("position", ["left", "top"])
def test_windows_choose_their_own_view(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, position: str
):
    env = mux_server
    env.env["TMPDIR"] = "/tmp"
    env.tmux(["set-environment", "-g", "TMPDIR", "/tmp"])
    env.env["TMUX"] = f"{env.socket_path},0,0"
    personal = env.home_path / "personal"
    work = env.home_path / "work/project"
    personal.mkdir(parents=True)
    work.mkdir(parents=True)
    config = env.home_path / ".config/workmux/config.yaml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text(
        "sidebar:\n  git_status: false\n  group_by: project\n  views:\n"
        "    - name: Personal\n      other: true\n"
        "    - name: Work\n      project_dirs: [~/work]\n"
    )
    assert env.runner_pane_id is not None
    personal_pane = env.runner_pane_id
    work_pane = env.tmux(
        [
            "new-window",
            "-d",
            "-t",
            "test:",
            "-n",
            "work",
            "-c",
            str(work),
            "-P",
            "-F",
            "#{pane_id}",
        ]
    ).stdout.strip()

    def command(pane: str, *args: str):
        env.env["TMUX_PANE"] = pane
        result = env.run_command(
            [str(workmux_exe_path), *args],
            check=False,
            cwd=work if pane == work_pane else personal,
        )
        assert result.returncode == 0, result.stderr
        return result

    def scope(pane: str):
        value = env.tmux(
            ["show-option", "-wqv", "-t", pane, "@workmux_sidebar_scope"]
        ).stdout
        return json.loads(value) if value.strip() else {}

    for pane in [personal_pane, work_pane]:
        command(pane, "set-window-status", "working")

    with attached_client(env) as (_, client):
        command(
            personal_pane,
            "sidebar",
            "on",
            "--position",
            position,
            "--width",
            "40",
            "--height",
            "4",
        )
        try:

            def sidebar_for(pane: str):
                lines = env.tmux(
                    ["list-panes", "-t", pane, "-F", "#{pane_id}\t#{@workmux_role}"]
                ).stdout.splitlines()
                return next(
                    line.split("\t")[0] for line in lines if line.endswith("\tsidebar")
                )

            personal_sidebar = sidebar_for(personal_pane)
            work_sidebar = sidebar_for(work_pane)

            def screen(sidebar: str):
                return env.tmux(["capture-pane", "-p", "-t", sidebar]).stdout

            assert poll_until(
                lambda: scope(personal_pane).get("name") == "Personal"
                and scope(work_pane).get("name") == "Work",
                timeout=10,
            )
            assert scope(personal_pane)["pane_ids"] == [personal_pane]
            assert scope(work_pane)["pane_ids"] == [work_pane]
            assert poll_until(
                lambda: "[view: Personal]" in screen(personal_sidebar).splitlines()[-1]
                and "[view: Work]" in screen(work_sidebar).splitlines()[-1]
            )

            # Switching windows or focusing a sidebar must not change either view.
            for pane in [work_pane, personal_pane, work_sidebar, personal_sidebar]:
                env.tmux(["switch-client", "-c", client, "-t", pane])
                assert scope(personal_pane)["name"] == "Personal"
                assert scope(work_pane)["name"] == "Work"

            command(personal_pane, "sidebar", "next")
            assert poll_until(
                lambda: env.tmux(["list-clients", "-F", "#{pane_id}"]).stdout.strip()
                == personal_pane
            )
            command(work_pane, "sidebar", "next")
            assert poll_until(
                lambda: env.tmux(["list-clients", "-F", "#{pane_id}"]).stdout.strip()
                == work_pane
            )

            # V toggles rules globally, not a shared named view.
            env.tmux(["select-pane", "-t", personal_sidebar])
            env.tmux(["send-keys", "-t", personal_sidebar, "V"])
            assert poll_until(
                lambda: scope(personal_pane).get("name") == "All"
                and scope(work_pane).get("name") == "All"
            )
            assert set(scope(personal_pane)["pane_ids"]) == {personal_pane, work_pane}
            assert set(scope(work_pane)["pane_ids"]) == {personal_pane, work_pane}
            assert poll_until(
                lambda: "[view: All]" in screen(personal_sidebar).splitlines()[-1]
                and "[view: All]" in screen(work_sidebar).splitlines()[-1]
            )

            env.tmux(["select-pane", "-t", work_sidebar])
            env.tmux(["send-keys", "-t", work_sidebar, "V"])
            assert poll_until(
                lambda: scope(work_pane).get("name") == "Work"
                and scope(personal_pane).get("name") == "Personal"
            )

            # Explicit CLI on/off controls the same global mode.
            command(work_pane, "sidebar", "view", "off")
            assert poll_until(
                lambda: scope(work_pane).get("name") == "All"
                and scope(personal_pane).get("name") == "All"
            )
            command(personal_pane, "sidebar", "view", "on")
            assert poll_until(
                lambda: scope(work_pane).get("name") == "Work"
                and scope(personal_pane).get("name") == "Personal"
            )

            if position == "left":
                env.tmux(
                    [
                        "set-option",
                        "-g",
                        "@workmux_sleeping_panes",
                        f"{personal_pane} {work_pane}",
                    ]
                )
                assert poll_until(
                    lambda: "STALE" in screen(work_sidebar).splitlines()[0]
                    and "[view: Work]" in screen(work_sidebar).splitlines()[-1]
                )
        finally:
            command(personal_pane, "sidebar", "off")
