"""Guarded preferred navigation with native session relocation as fallback."""

import os
import shlex
import sys
from pathlib import Path

import pytest

from .conftest import (
    TmuxEnvironment,
    get_session_name,
    poll_until,
    write_workmux_config,
)
from .test_popup_navigation import attached_client
from .test_workmux_add.conftest import add_branch_and_get_worktree

pytestmark = pytest.mark.tmux_only


def setup_target(env, binary, repo):
    env.tmux(["set-environment", "-gu", "TMUX_PANE"])
    env.tmux(["set-option", "-g", "detach-on-destroy", "on"])
    write_workmux_config(repo, panes=[{"command": "/bin/sh"}])
    worktree = add_branch_and_get_worktree(
        env, binary, repo, "adversarial", extra_args="--session --background"
    )
    return worktree, get_session_name("adversarial")


def assert_resources(env, repo, worktree, operation):
    if operation == "remove":
        assert poll_until(lambda: not worktree.exists(), timeout=10)
        assert poll_until(
            lambda: env.run_command(
                ["git", "show-ref", "--verify", "refs/heads/adversarial"],
                cwd=repo,
                check=False,
            ).returncode
            != 0,
            timeout=10,
        )
    else:
        assert (worktree / ".git").exists()
        env.run_command(
            ["git", "show-ref", "--verify", "refs/heads/adversarial"], cwd=repo
        )


def clients(env):
    return dict(
        line.split("\t")
        for line in env.tmux(
            ["list-clients", "-F", "#{client_name}\t#{session_name}"]
        ).stdout.splitlines()
    )


def gone(env, source):
    return env.tmux(["has-session", "-t", "=" + source], check=False).returncode != 0


def popup(env, master, cwd, command):
    env.tmux(["bind-key", "r", "display-popup", "-d", str(cwd), "-E", command])
    os.write(master, b"\x02r")


@pytest.mark.parametrize("operation", ["remove", "close"])
@pytest.mark.parametrize(
    "other_active", [False, True], ids=["invoker-active", "bystander-active"]
)
def test_two_clients(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    tmp_path: Path,
    operation,
    other_active,
):
    env = mux_server
    worktree, source = setup_target(env, workmux_exe_path, repo_path)
    env.tmux(["new-session", "-d", "-s", "bystander"])
    ready = tmp_path / "ready"
    ack = tmp_path / "bystander-ack"
    resolved = tmp_path / "resolved"
    command = (
        f"touch {shlex.quote(str(ready))}; tmux wait-for launch; tmux display-message -p '#{{session_name}} #{{pane_id}}' > {shlex.quote(str(resolved))}; "
        + shlex.join([str(workmux_exe_path), operation, "adversarial"])
        + (" -f" if operation == "remove" else "")
    )
    with (
        attached_client(env) as (master, client),
        attached_client(env) as (other_master, other),
    ):
        env.tmux(["switch-client", "-c", other, "-t", "bystander"])
        env.tmux(["switch-client", "-c", client, "-t", source])
        popup(env, master, worktree, command)
        assert poll_until(ready.exists)
        if other_active:
            os.write(other_master, f"touch {shlex.quote(str(ack))}\r".encode())
            assert poll_until(ack.exists)
        env.tmux(["wait-for", "-S", "launch"])
        assert poll_until(lambda: gone(env, source), timeout=10)
        assert_resources(env, repo_path, worktree, operation)
        actual = clients(env)
        print("popup fallback resolved:", resolved.read_text().strip())
        print(
            operation,
            other_active,
            "client map",
            actual,
            "invoker",
            client,
            "bystander",
            other,
        )
        assert actual.get(other) == "bystander", actual
        assert actual.get(client) == "test", actual


@pytest.mark.parametrize("operation", ["remove", "close"])
def test_focus_changes_after_schedule(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    tmp_path: Path,
    operation,
):
    env = mux_server
    worktree, source = setup_target(env, workmux_exe_path, repo_path)
    env.tmux(["new-session", "-d", "-s", "chosen"])
    shimdir = tmp_path / "shim"
    shimdir.mkdir()
    marker = tmp_path / "scheduled"
    import shutil

    real_tmux = shutil.which("tmux")
    assert real_tmux is not None
    shim = shimdir / "tmux"
    shim.write_text(
        f"#!{sys.executable}\nimport os, sys\nfrom pathlib import Path\na=sys.argv[1:]\nif a[:2] == ['run-shell', '-b'] and 'kill-session' in a[-1]:\n    a[-1] = 'touch ' + {shlex.quote(str(marker))!r} + '; ' + {shlex.quote(real_tmux)!r} + ' wait-for release; ' + a[-1]\nos.execv({real_tmux!r}, [{real_tmux!r}] + a)\n"
    )
    shim.chmod(0o755)
    command = (
        f"export PATH={shlex.quote(str(shimdir))}:$PATH; "
        + shlex.join([str(workmux_exe_path), operation, "adversarial"])
        + (" -f" if operation == "remove" else "")
    )
    with attached_client(env) as (master, client):
        env.tmux(["switch-client", "-c", client, "-t", source])
        popup(env, master, worktree, command)
        assert poll_until(marker.exists, timeout=10)
        env.tmux(["switch-client", "-c", client, "-t", "chosen"])
        assert clients(env).get(client) == "chosen"
        print(operation, "before releasing delayed script", clients(env))
        env.tmux(["wait-for", "-S", "release"])
        assert poll_until(lambda: gone(env, source), timeout=10)
        assert_resources(env, repo_path, worktree, operation)
        actual = clients(env)
        print(operation, "focus race", actual, "invoker", client)
        assert actual.get(client) == "chosen", actual


@pytest.mark.parametrize("operation", ["remove", "close"])
def test_no_previous_session(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path, operation
):
    env = mux_server
    worktree, source = setup_target(env, workmux_exe_path, repo_path)
    # The fixture client attaches directly to its session named test.
    env.tmux(["rename-session", "-t", "test", "available"])
    env.tmux(["rename-session", "-t", source, "test"])
    with attached_client(env) as (master, client):
        # Restore the managed name without giving the client previous-session history.
        env.tmux(["rename-session", "-t", "test", source])
        command = shlex.join([str(workmux_exe_path), operation, "adversarial"]) + (
            " -f" if operation == "remove" else ""
        )
        popup(env, master, worktree, command)
        assert poll_until(lambda: gone(env, source), timeout=10)
        assert_resources(env, repo_path, worktree, operation)
        actual = clients(env)
        print(operation, "no previous session", actual, "invoker", client)
        assert actual.get(client) == "available", actual


@pytest.mark.parametrize("local_option", [None, "on", "off"])
def test_failed_close_restores_session_option(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    tmp_path: Path,
    local_option,
):
    import shutil
    import subprocess

    env = mux_server
    worktree, source = setup_target(env, workmux_exe_path, repo_path)
    if local_option is not None:
        env.tmux(["set-option", "-t", source, "detach-on-destroy", local_option])
    original = env.tmux(
        ["show-options", "-qv", "-t", source, "detach-on-destroy"]
    ).stdout
    shimdir = tmp_path / "fail-kill"
    shimdir.mkdir()
    real_tmux = shutil.which("tmux")
    assert real_tmux is not None
    shim = shimdir / "tmux"
    shim.write_text(
        f"#!{sys.executable}\nimport os, sys\na=sys.argv[1:]\n"
        "if 'kill-session' in a:\n"
        "    i=a.index('kill-session')\n"
        "    a[i+2]='$999999999'\n"
        f"os.execv({real_tmux!r}, [{real_tmux!r}] + a)\n"
    )
    shim.chmod(0o755)
    pid = env.tmux(["display-message", "-p", "#{pid}"]).stdout.strip()
    command_env = dict(env.env, TMUX=f"{env.socket_path},{pid},0")
    command_env.pop("TMUX_PANE", None)
    command_env["PATH"] = f"{shimdir}:{command_env['PATH']}"
    result = subprocess.run(
        [str(workmux_exe_path), "close", "adversarial"],
        cwd=repo_path,
        env=command_env,
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0, result
    assert not gone(env, source)
    assert worktree.exists()
    assert (
        env.tmux(["show-options", "-qv", "-t", source, "detach-on-destroy"]).stdout
        == original
    )
    assert (
        env.tmux(["show-options", "-gqv", "detach-on-destroy"]).stdout.strip() == "on"
    )


@pytest.mark.parametrize("operation", ["remove", "close"])
def test_all_source_clients_relocated(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    operation,
):
    env = mux_server
    worktree, source = setup_target(env, workmux_exe_path, repo_path)
    command = shlex.join([str(workmux_exe_path), operation, "adversarial"]) + (
        " -f" if operation == "remove" else ""
    )
    with attached_client(env) as (master, client), attached_client(env) as (_, other):
        env.tmux(["switch-client", "-c", client, "-t", source])
        env.tmux(["switch-client", "-c", other, "-t", source])
        popup(env, master, worktree, command)
        assert poll_until(lambda: gone(env, source), timeout=10)
        assert_resources(env, repo_path, worktree, operation)
        actual = clients(env)
        assert actual.get(client) == "test", actual
        assert actual.get(other) == "test", actual
        assert (
            env.tmux(["show-options", "-gqv", "detach-on-destroy"]).stdout.strip()
            == "on"
        )
        assert (
            env.tmux(["show-options", "-qv", "-t", "test", "detach-on-destroy"]).stdout
            == ""
        )


@pytest.mark.parametrize("operation", ["remove", "close"])
def test_previous_session_beats_unrelated_idle_session(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path, operation
):
    env = mux_server
    worktree, source = setup_target(env, workmux_exe_path, repo_path)
    env.tmux(["new-session", "-d", "-s", "idle"])
    with attached_client(env) as (master, client):
        env.tmux(["switch-client", "-c", client, "-t", source])
        # Both older and newer unrelated sessions exist. Neither should win
        # over this client's own previous session.
        env.tmux(["new-session", "-d", "-s", "newer-agent"])
        command = shlex.join([str(workmux_exe_path), operation, "adversarial"]) + (
            " -f" if operation == "remove" else ""
        )
        popup(env, master, worktree, command)
        assert poll_until(lambda: gone(env, source), timeout=10)
        assert clients(env).get(client) == "test"
        assert_resources(env, repo_path, worktree, operation)


@pytest.mark.parametrize("explicit", [False, True], ids=["stored-base", "into"])
def test_merge_returns_to_parent_session(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path, explicit
):
    env = mux_server
    env.tmux(["set-environment", "-gu", "TMUX_PANE"])
    write_workmux_config(repo_path, panes=[{"command": "/bin/sh"}], env=env)
    parent = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, "parent", extra_args="--session --background"
    )
    child = add_branch_and_get_worktree(
        env,
        workmux_exe_path,
        repo_path,
        "child",
        extra_args="--session --background --base parent",
    )
    (child / "result.txt").write_text("child result\n")
    env.run_command(["git", "add", "result.txt"], cwd=child)
    env.run_command(["git", "commit", "-m", "add child result"], cwd=child)
    source = get_session_name("child")
    with attached_client(env) as (master, client):
        # The previous session is test, not the merge destination.
        env.tmux(["switch-client", "-c", client, "-t", source])
        env.tmux(["new-session", "-d", "-s", "unrelated-agent"])
        command = shlex.join([str(workmux_exe_path), "merge", "child"]) + (
            " --into parent" if explicit else ""
        )
        popup(env, master, child, command)
        assert poll_until(lambda: gone(env, source), timeout=10)
        assert clients(env).get(client) == get_session_name("parent")
        assert poll_until(lambda: not child.exists(), timeout=10)
        assert (parent / "result.txt").read_text() == "child result\n"


@pytest.mark.parametrize("operation", ["remove", "close"])
@pytest.mark.parametrize("event", ["moved", "disconnected", "shadowed"])
def test_focus_changes_after_client_enumeration(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    tmp_path: Path,
    operation,
    event,
):
    import shutil

    env = mux_server
    worktree, source = setup_target(env, workmux_exe_path, repo_path)
    source_id = env.tmux(
        ["display-message", "-p", "-t", source, "#{session_id}"]
    ).stdout.strip()
    env.tmux(["new-session", "-d", "-s", "chosen"])
    shimdir = tmp_path / "enumeration-shim"
    shimdir.mkdir()
    marker = tmp_path / "enumerated"
    real_tmux = shutil.which("tmux")
    assert real_tmux is not None
    shim = shimdir / "tmux"
    shim.write_text(
        f"#!{sys.executable}\n"
        "import os, subprocess, sys\nfrom pathlib import Path\n"
        "a = sys.argv[1:]\n"
        "if a[:2] == ['run-shell', '-b'] and 'kill-session' in a[-1]:\n"
        f"    a[-1] = 'export PATH=' + {shlex.quote(str(shimdir))!r} + ':$PATH; ' + a[-1]\n"
        f"if a[:3] == ['list-clients', '-t', {source_id!r}]:\n"
        f"    result = subprocess.run([{real_tmux!r}] + a, capture_output=True)\n"
        f"    Path({str(marker)!r}).touch()\n"
        f"    subprocess.run([{real_tmux!r}, 'wait-for', 'release-enumeration'], check=True)\n"
        "    sys.stdout.buffer.write(result.stdout)\n    sys.exit(result.returncode)\n"
        f"os.execv({real_tmux!r}, [{real_tmux!r}] + a)\n"
    )
    shim.chmod(0o755)
    command = (
        f"export PATH={shlex.quote(str(shimdir))}:$PATH; "
        + shlex.join([str(workmux_exe_path), operation, "adversarial"])
        + (" -f" if operation == "remove" else "")
    )
    with attached_client(env) as (master, client), attached_client(env) as (_, other):
        env.tmux(["switch-client", "-c", client, "-t", source])
        popup(env, master, worktree, command)
        assert poll_until(marker.exists, timeout=10)
        if event == "disconnected":
            env.tmux(["detach-client", "-t", client])
        else:
            if event == "shadowed":
                env.tmux(["rename-session", "-t", source_id, client])
            env.tmux(["switch-client", "-c", client, "-t", "chosen"])
        env.tmux(["wait-for", "-S", "release-enumeration"])
        assert poll_until(
            lambda: env.tmux(["has-session", "-t", source_id], check=False).returncode
            != 0,
            timeout=10,
        )
        actual = clients(env)
        assert actual.get(other) == "test", actual
        if event == "disconnected":
            assert client not in actual
        else:
            assert actual.get(client) == "chosen", actual
        assert_resources(env, repo_path, worktree, operation)
