"""Exercise sidebar commands through isolated Zellij CLI test doubles."""

import json
import os
import socket
import subprocess
import sys
import tempfile
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import pytest

from .support.executable import install_script


@dataclass
class SidebarHarness:
    root: Path
    executable: Path
    env: dict[str, str]
    state_path: Path

    def run(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(self.executable), "sidebar", *args],
            cwd=self.root,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=10,
            check=True,
        )

    def run_raw(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(self.executable), *args],
            cwd=self.root,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=10,
            check=True,
        )

    def model(self) -> dict[str, Any]:
        return json.loads((self.root / "mux.json").read_text())

    def save_model(self, model: dict[str, Any]) -> None:
        (self.root / "mux.json").write_text(json.dumps(model))

    def state(self) -> dict[str, Any]:
        return json.loads(self.state_path.read_text())


@pytest.fixture
def sidebar(tmp_path: Path, workmux_exe_path: Path) -> Iterator[SidebarHarness]:
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (tmp_path / ".workmux.yaml").write_text("sidebar:\n  group_by: project\n")
    env = os.environ | {
        "HOME": str(tmp_path),
        "XDG_CONFIG_HOME": str(config_dir),
        "XDG_STATE_HOME": str(tmp_path / "state"),
        "TMPDIR": str(tmp_path),
        "WORKMUX_BACKEND": "zellij",
        "WORKMUX_TEST": "1",
        "ZELLIJ": "0",
        "ZELLIJ_SESSION_NAME": "sidebar-test",
        "ZELLIJ_PANE_ID": "1",
        "PATH": f"{bin_dir}:{os.environ['PATH']}",
        "SIDEBAR_TEST_ROOT": str(tmp_path),
    }
    subprocess.run(["git", "init", "-q", str(tmp_path)], env=env, check=True)
    install_script(
        bin_dir / "tmux",
        "#!/bin/sh\n"
        'printf "%s\\n" "$*" >> "$SIDEBAR_TEST_ROOT/tmux-calls"\n'
        'case "$*" in\n'
        "  *group_by*) echo none ;;\n"
        "  *filter*) echo session ;;\n"
        "  *position*) echo top ;;\n"
        "esac\n",
    )
    install_script(
        bin_dir / "zellij",
        f"#!{sys.executable}\n"
        + r"""
import json
import os
from pathlib import Path
import re
import sys

root = Path(os.environ["SIDEBAR_TEST_ROOT"])
path = root / "mux.json"
model = json.loads(path.read_text())
args = sys.argv[1:]
assert args[:3] == ["--session", "sidebar-test", "action"], args
args = args[3:]
model["calls"].append(args)
action = args[0]
if action == "list-panes":
    print(json.dumps([
        pane for pane in model["panes"]
        if "--all" in args or pane.get("is_selectable", True)
    ]))
elif action == "list-tabs":
    print(json.dumps(model.get("tabs", [{"tab_id": 0, "position": 0, "name": "one", "active": True}])))
elif action == "current-tab-info":
    print("name: one\nid: 0\nposition: 0")
elif action == "dump-layout":
    print(model["layout"])
elif action == "override-layout":
    layout = args[args.index("--layout-string") + 1]
    model["applied_layout"] = layout
    marker = re.search(r'name="(workmux-sidebar-placeholder-[^"]+)"', layout)
    active_id = next(
        (tab["tab_id"] for tab in model.get("tabs", []) if tab["active"]), 0
    )
    target = next(pane for pane in model["panes"] if pane["tab_id"] == active_id)
    first_id = max(9, 1 + max(pane["id"] for pane in model["panes"]))
    for pane_id, title, pane_command in [
        (first_id, "concurrent editor", "zsh"),
        (
            first_id + 1,
            "Pane #3",
            f'env WORKMUX_SIDEBAR_PLACEHOLDER={marker[1]} true' if marker else "env true",
        ),
        (first_id + 2, "workmux-sidebar", "env WORKMUX_BACKEND=zellij workmux _sidebar-run"),
    ]:
        model["panes"].append(
            target
            | {
                "id": pane_id,
                "title": title,
                "pane_command": pane_command,
                "is_focused": False,
            }
        )
elif action == "close-pane":
    pane_id = int(args[args.index("--pane-id") + 1].removeprefix("terminal_"))
    model["closed"].append(pane_id)
    model["panes"] = [pane for pane in model["panes"] if pane["id"] != pane_id]
elif action == "focus-pane-id" and model.get("focus_error"):
    path.write_text(json.dumps(model))
    print(model["focus_error"], file=sys.stderr)
    sys.exit(2)
elif action == "go-to-tab-by-id":
    tab_id = int(args[1])
    for tab in model.get("tabs", []):
        tab["active"] = tab["tab_id"] == tab_id
elif action in ("focus-pane-id", "change-floating-pane-coordinates", "next-swap-layout"):
    pass
else:
    raise AssertionError(args)
path.write_text(json.dumps(model))
""",
    )
    key = 0xCBF29CE484222325
    for byte in b"sidebar-test":
        key = ((key ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    state_path = tmp_path / f"workmux-sidebar-state-{key:016x}.json"
    harness = SidebarHarness(tmp_path, workmux_exe_path, env, state_path)
    harness.save_model(
        {
            "panes": [
                {
                    "id": 1,
                    "is_plugin": False,
                    "is_focused": True,
                    "terminal_command": "bash",
                    "title": "editor",
                    "tab_id": 0,
                    "tab_name": "one",
                    "tab_position": 0,
                    "pane_x": 0,
                    "pane_y": 1,
                    "pane_columns": 160,
                    "pane_rows": 40,
                }
            ],
            "layout": 'layout { tab name="one" focus=true { pane; }; }',
            "calls": [],
            "closed": [],
        }
    )
    # Keep socket paths short on macOS and prevent a real daemon from starting.
    with (
        tempfile.TemporaryDirectory(prefix="wm-sidebar-", dir="/tmp") as runtime_dir,
        socket.socket(socket.AF_UNIX) as listener,
    ):
        env["TMPDIR"] = runtime_dir
        harness.state_path = Path(runtime_dir) / state_path.name
        listener.bind(str(Path(runtime_dir) / f"workmux-sidebar-{key:016x}.sock"))
        listener.listen()
        yield harness


def test_group_toggle_ignores_tmux_override(sidebar: SidebarHarness):
    sidebar.run("group", "--clear")
    sidebar.run("group")
    assert sidebar.state()["group_by"] == "none"
    assert not (sidebar.root / "tmux-calls").exists()


def test_filter_toggle_ignores_tmux_override(sidebar: SidebarHarness):
    sidebar.run("filter")
    assert sidebar.state()["filter"] == "session"
    assert not (sidebar.root / "tmux-calls").exists()


@pytest.mark.parametrize("action", [("jump", "1"), ("next",), ("prev",)])
def test_navigation_focuses_pane_by_id(
    sidebar: SidebarHarness, action: tuple[str, ...]
):
    sidebar.state_path.write_text(json.dumps({"ordered_agents": ["terminal_1"]}))
    sidebar.run(*action)
    assert ["focus-pane-id", "terminal_1"] in sidebar.model()["calls"]


def test_layout_cleanup_preserves_concurrently_opened_panes(sidebar: SidebarHarness):
    sidebar.run("on")
    model = sidebar.model()
    assert 9 not in model["closed"]
    assert 10 in model["closed"]
    assert 1 not in model["closed"]


@pytest.mark.parametrize("use_alias", [True, False])
def test_layout_preserves_builtin_plugin_invocations(
    sidebar: SidebarHarness, use_alias: bool
):
    model = sidebar.model()
    for pane_id, name in [(2, "tab-bar"), (3, "status-bar")]:
        model["panes"].append(
            model["panes"][0]
            | {
                "id": pane_id,
                "is_plugin": True,
                "is_selectable": False,
                "title": f"custom {name}",
                "plugin_url": name if use_alias else f"zellij:{name}",
            }
        )
    model["layout"] = """
        layout {
            tab focus=true {
                pane size=1 borderless=true { plugin location="zellij:tab-bar"; }
                pane
                pane size=1 borderless=true { plugin location="zellij:status-bar"; }
            }
        }
    """
    sidebar.save_model(model)
    sidebar.run("on")
    layout = sidebar.model()["applied_layout"]
    for name in ["tab-bar", "status-bar"]:
        location = name if use_alias else f"zellij:{name}"
        assert f'plugin location="{location}"' in layout


def test_navigation_to_already_focused_pane_succeeds(sidebar: SidebarHarness):
    model = sidebar.model()
    model["focus_error"] = "Pane Terminal(1) is already focused"
    sidebar.save_model(model)
    sidebar.state_path.write_text(json.dumps({"ordered_agents": ["terminal_1"]}))
    sidebar.run("jump", "1")


def test_navigation_propagates_focus_failure(sidebar: SidebarHarness):
    model = sidebar.model()
    model["focus_error"] = "Target pane was closed"
    sidebar.save_model(model)
    sidebar.state_path.write_text(json.dumps({"ordered_agents": ["terminal_1"]}))
    with pytest.raises(subprocess.CalledProcessError) as error:
        sidebar.run("jump", "1")
    assert "Failed to focus zellij pane" in error.value.stderr


def test_floating_panes_keep_their_coordinates(sidebar: SidebarHarness):
    model = sidebar.model()
    model["panes"].append(
        model["panes"][0]
        | {
            "id": 7,
            "title": "floating editor",
            "is_floating": True,
            "pane_x": 10,
            "pane_y": 8,
            "pane_columns": 40,
            "pane_rows": 15,
        }
    )
    sidebar.save_model(model)
    sidebar.run("on")
    model = sidebar.model()
    assert 7 not in model["closed"]
    assert [
        "change-floating-pane-coordinates",
        "--pane-id",
        "terminal_7",
        "--x",
        "10",
        "--y",
        "8",
        "--width",
        "40",
        "--height",
        "15",
    ] in model["calls"]


def test_replacement_closes_previous_sidebar(sidebar: SidebarHarness):
    model = sidebar.model()
    model["panes"].append(model["panes"][0] | {"id": 8, "title": "workmux-sidebar"})
    model["layout"] = """
        layout {
            tab name="one" focus=true {
                pane split_direction="vertical" {
                    pane name="workmux-sidebar" command="/old/workmux" cwd="/other" size=24
                    pane
                }
            }
        }
    """
    sidebar.save_model(model)
    sidebar.run("on")
    model = sidebar.model()
    assert 8 in model["closed"]
    assert sum(pane["title"] == "workmux-sidebar" for pane in model["panes"]) == 1


def test_background_sync_does_not_select_tabs(sidebar: SidebarHarness):
    sidebar.run("on")
    model = sidebar.model()
    model["tabs"] = [
        {"tab_id": 0, "position": 0, "name": "one", "active": False},
        {"tab_id": 1, "position": 1, "name": "manual", "active": True},
    ]
    model["panes"].append(
        model["panes"][0]
        | {
            "id": 20,
            "title": "manual shell",
            "tab_id": 1,
            "tab_name": "manual",
            "tab_position": 1,
        }
    )
    model["calls"] = []
    sidebar.save_model(model)

    sidebar.run_raw("_sidebar-sync")

    assert not any(call[0] == "go-to-tab-by-id" for call in sidebar.model()["calls"])
