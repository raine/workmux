"""Shell lifecycle over real SSH in host and Docker modes."""

import shlex
import uuid

import pytest
import yaml

from ..conftest import get_scripts_dir, get_window_name, poll_until
from .conftest import agent_dirs_env, commit_project
from .test_shell import pane_text
from . import test_ssh_workflows as ssh_tests

pytestmark = ssh_tests.pytestmark
ssh_workspace = ssh_tests.ssh_workspace


def open_shell(ws, window, args, banner):
    script = get_scripts_dir(ws.env) / f"review-shell-{uuid.uuid4().hex[:8]}.sh"
    exports = {
        **agent_dirs_env(ws.env.home_path),
        "WORKMUX_REMOTE_LOOPBACK": "0",
        "PATH": ws.env.env["PATH"],
        "HOME": ws.env.env["HOME"],
        "SHELL": "/bin/sh",
    }
    script.write_text(
        "\n".join(
            [
                *(
                    f"export {key}={shlex.quote(value)}"
                    for key, value in exports.items()
                ),
                f"cd {shlex.quote(str(ws.repo))}",
                f"{shlex.quote(str(ws.exe))} shell {args}",
                'echo "LOCAL-BACK-$?"',
                "exec sleep 600",
            ]
        )
        + "\n"
    )
    before = set(ws.tmux("list-sessions", "-F", "#{session_name}").split())
    pane = ws.env.tmux(
        ["split-window", "-t", window, "-P", "-F", "#{pane_id}", f"/bin/sh {script}"]
    ).stdout.strip()
    assert poll_until(lambda: banner in pane_text(ws.env, pane), timeout=60), pane_text(
        ws.env, pane
    )
    after = set(ws.tmux("list-sessions", "-F", "#{session_name}").split())
    [session] = list(after - before)
    assert poll_until(
        lambda: bool(ws.tmux("list-clients", "-t", session, "-F", "#{client_pid}")),
        timeout=20,
    )
    return pane, session


def send(ws, pane, command, expected):
    ws.env.tmux(["send-keys", "-t", pane, command, "Enter"])
    assert poll_until(lambda: expected in pane_text(ws.env, pane), timeout=20), (
        pane_text(ws.env, pane)
    )


@pytest.mark.parametrize("container", [False, True])
def test_real_ssh_shell(ssh_workspace, container):
    ws = ssh_workspace
    handle = "shellreview-box" if container else "shellreview-host"
    if container:
        path = ws.env.home_path / ".config/workmux/config.yaml"
        config = yaml.safe_load(path.read_text())
        config["remotes"]["lo"]["isolation"] = "container"
        config["sandbox"] = {"env": {"SHELL_REVIEW_ENV": "container-value"}}
        path.write_text(yaml.safe_dump(config))
    commit_project(
        ws.repo,
        {
            "sandbox": {"enabled": container, "target": "all"},
            "panes": [{"command": "echo AGENT-READY; exec bash"}],
        },
    )
    ws.run(f"add {handle} --remote lo")
    ws.wait_attached(handle)
    window = get_window_name(handle)
    pane, session = open_shell(
        ws, window, handle, "in a new container" if container else "on the host"
    )
    send(
        ws,
        pane,
        'printf "CWD=%s COLOR=%s\\n" "$(pwd -P)" "$COLORTERM"',
        f"CWD={ws.record(handle)['remote_worktree']} COLOR=truecolor",
    )
    send(
        ws,
        pane,
        "if test -f /.dockerenv; then echo PLACE-box; else echo PLACE-host; fi",
        "PLACE-box" if container else "PLACE-host",
    )
    if container:
        send(ws, pane, 'printf "ENV=%s\\n" "$SHELL_REVIEW_ENV"', "ENV=container-value")
        hostpane, hostsession = open_shell(
            ws, window, f"{handle} --host", "on the host"
        )
        send(
            ws,
            hostpane,
            "if test -f /.dockerenv; then echo HOST-box; else echo HOST-native; fi",
            "HOST-native",
        )
        ws.env.tmux(["send-keys", "-t", hostpane, "exit", "Enter"])
        assert poll_until(
            lambda: "LOCAL-BACK-0" in pane_text(ws.env, hostpane), timeout=20
        )
        assert (
            hostsession not in ws.tmux("list-sessions", "-F", "#{session_name}").split()
        )
        ws.env.tmux(["kill-pane", "-t", hostpane])
    send(
        ws,
        pane,
        "export REVIEW_RECONNECT=kept; printf 'BEFORE-%s\\n' \"$REVIEW_RECONNECT\"",
        "BEFORE-kept",
    )
    client = ws.tmux("list-clients", "-t", session, "-F", "#{client_pid}")
    ws.shell(f"kill -9 {int(client)}")
    assert poll_until(
        lambda: ws.tmux("list-clients", "-t", session, "-F", "#{client_pid}")
        not in ("", client),
        timeout=30,
    )
    send(ws, pane, "printf 'AFTER-%s\\n' \"$REVIEW_RECONNECT\"", "AFTER-kept")
    gc = ws.run("remote gc lo --dry-run")
    assert session not in gc.stdout
    ws.env.tmux(["send-keys", "-t", pane, "exit", "Enter"])
    assert poll_until(lambda: "LOCAL-BACK-0" in pane_text(ws.env, pane), timeout=30), (
        pane_text(ws.env, pane)
    )
    assert session not in ws.tmux("list-sessions", "-F", "#{session_name}").split()
    if container:
        assert poll_until(
            lambda: len(ws.shell(f"docker ps -q --filter name=wm-{handle}-").split())
            == 1,
            timeout=30,
        )
    ws.env.tmux(["kill-pane", "-t", pane])
    # Closing a fully attached shell must clean up its own session/container.
    pane, session = open_shell(
        ws, window, handle, "in a new container" if container else "on the host"
    )
    ws.env.tmux(["kill-pane", "-t", pane])
    assert poll_until(
        lambda: session
        not in ws.tmux("list-sessions", "-F", "#{session_name}").split(),
        timeout=30,
    )
    if container:
        assert poll_until(
            lambda: len(ws.shell(f"docker ps -q --filter name=wm-{handle}-").split())
            == 1,
            timeout=30,
        )
    # Removal ends a live shell, and its containers, along with the agent.
    pane, session = open_shell(
        ws, window, handle, "in a new container" if container else "on the host"
    )
    ws.run(f"remove --force {handle}")
    assert (
        ws.shell(
            f"tmux -S {shlex.quote(ws.root + '/run/tmux.sock')} list-sessions 2>/dev/null || true"
        )
        == ""
    )
    if container:
        assert poll_until(
            lambda: ws.shell(f"docker ps -q --filter name=wm-{handle}-") == "",
            timeout=30,
        )


def test_gc_preserves_container_shell_after_agent_ends(ssh_workspace):
    ws = ssh_workspace
    handle = "shellreview-gc"
    path = ws.env.home_path / ".config/workmux/config.yaml"
    config = yaml.safe_load(path.read_text())
    config["remotes"]["lo"]["isolation"] = "container"
    path.write_text(yaml.safe_dump(config))
    commit_project(
        ws.repo,
        {
            "sandbox": {"enabled": True, "target": "all"},
            "panes": [{"command": "echo AGENT-READY; exec bash"}],
        },
    )
    ws.run(f"add {handle} --remote lo")
    ws.wait_attached(handle)
    pane, session = open_shell(
        ws, get_window_name(handle), handle, "in a new container"
    )
    ex_id = ws.record(handle)["execution"]["ex_id"]
    ws.tmux("kill-session", "-t", f"={ex_id}-0")
    assert poll_until(
        lambda: len(ws.shell(f"docker ps -q --filter name=wm-{handle}-").split()) == 1,
        timeout=30,
    )
    send(ws, pane, "echo LIVE-$((6*7))", "LIVE-42")
    gc = ws.run("remote gc lo --dry-run")
    assert (
        f"container wm-{handle}-" not in gc.stdout
        and "sandbox supervisor" not in gc.stdout
    ), gc.stdout
    ws.run("remote gc lo --force")
    send(ws, pane, "echo SURVIVED-$((6*7))", "SURVIVED-42")
