"""Effective isolation, host prerequisites and recorded workspace modes."""

import os
import shutil
import subprocess
from pathlib import Path

import pytest
import yaml

from ..conftest import TmuxEnvironment, get_window_name, wait_for_pane_output
from .conftest import Node, RemoteCli, commit_project, local_records, wm, write_script


def follow_sandbox(env: TmuxEnvironment) -> None:
    path = env.home_path / ".config/workmux/config.yaml"
    config = yaml.safe_load(path.read_text())
    del config["remotes"]["lo"]["isolation"]
    path.write_text(yaml.dump(config))


@pytest.mark.tmux_only
def test_host_agent_and_missing_cli_before_state(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    follow_sandbox(mux_server)
    program = mux_server.home_path / "isolation-agent"
    shell = write_script(
        mux_server.home_path / "login-shell",
        f'export PATH={mux_server.home_path}:"$PATH"\n'
        'if [ "$1" = "-l" ]; then shift; fi\nexec /bin/sh "$@"\n',
    )
    login_env = {"SHELL": str(shell)}
    commit_project(
        mux_repo_path,
        {"agent": "isolation-agent", "panes": [{"command": "<agent>"}]},
    )
    missing = wm(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "add missing --remote lo",
        extra_env=login_env,
        expect_fail=True,
    )
    assert "is not installed on lo" in missing.stderr
    assert "remotes.lo.isolation: container" in missing.stderr
    assert not (node.root / "state").exists()
    assert not (node.root / "bin").exists()
    assert local_records(mux_server) == []

    write_script(program, "echo HOST-AGENT-READY\n")
    added = wm(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "add host --remote lo",
        extra_env=login_env,
    )
    assert "Runs: on host (sandbox.enabled is off)" in added.stdout
    wait_for_pane_output(mux_server, get_window_name("host"), "HOST-AGENT-READY")
    [workspace] = node.workspaces()
    assert workspace["isolation"] == "none"
    assert "sandbox." not in added.stderr


@pytest.mark.tmux_only
def test_project_sandbox_records_container_and_reopen_preserves_it(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    if not shutil.which("docker"):
        pytest.skip("requires Docker")
    docker = subprocess.run(["docker", "info"], capture_output=True)
    if docker.returncode:
        pytest.skip("requires a real Docker daemon")
    follow_sandbox(mux_server)
    commit_project(mux_repo_path, {"sandbox": {"enabled": True}})
    added = wm(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "add boxed --remote lo --no-pane-cmds",
    )
    assert "Runs: in Docker (sandbox.enabled is on)" in added.stdout
    [workspace] = node.workspaces()
    assert workspace["isolation"] == "container"
    wm(mux_server, workmux_exe_path, mux_repo_path, "close boxed")
    (mux_repo_path / ".workmux.yaml").write_text("sandbox:\n  enabled: false\n")
    wm(mux_server, workmux_exe_path, mux_repo_path, "open boxed")
    assert node.workspaces()[0]["isolation"] == "container"


@pytest.mark.parametrize("default_exists", [False, True])
def test_doctor_host_scope_and_login_preflight(
    loopback_cli: RemoteCli,
    workmux_exe_path: Path,
    default_exists: bool,
):
    cli = loopback_cli
    fake_bin = cli.home / "bin"
    fake_bin.mkdir()
    shell = write_script(
        cli.home / "login-shell",
        f'export PATH={fake_bin}:/usr/bin:/bin\nshift\nexec /bin/sh "$@"\n',
    )
    cli.env["SHELL"] = str(shell)
    if default_exists:
        write_script(fake_bin / "claude", "exit 0\n")
    cli.write_config(
        {
            "lo": {
                "host": "localhost",
                "path": "~/node",
                "binary": str(workmux_exe_path),
            }
        },
        agent="claude",
        agents={"claude": {"command": "claude"}, "other": {"command": "missing-other"}},
    )
    doctor = cli.run("remote", "doctor", "lo")
    assert "run on host (sandbox.enabled is off)" in doctor.stdout
    assert "default agent claude:" in doctor.stdout
    assert ("✓ agent " if default_exists else "✗ agent ") in doctor.stdout
    assert (
        "other configured agents not installed: other (missing-other)" in doctor.stdout
    )
    assert "claude (claude)" not in doctor.stdout
    assert "not needed for host agents" in doctor.stdout
    assert "image " not in doctor.stdout
    login = cli.run("remote", "login", "lo", "codex")
    assert "codex is not installed on lo" in login.stderr
    assert not (cli.home / "node").exists()


@pytest.mark.parametrize(
    "global_on,project_on,override,expected",
    [
        (False, None, None, "on host (sandbox.enabled is off)"),
        (True, None, None, "in Docker (sandbox.enabled is on)"),
        (True, False, None, "on host (sandbox.enabled is off)"),
        (False, True, None, "in Docker (sandbox.enabled is on)"),
        (True, True, "none", "on host (remotes.lo.isolation: none)"),
        (False, False, "container", "in Docker (remotes.lo.isolation: container)"),
    ],
)
def test_doctor_resolves_effective_project_and_global_config(
    loopback_cli: RemoteCli,
    workmux_exe_path: Path,
    global_on: bool,
    project_on: bool | None,
    override: str | None,
    expected: str,
):
    remote = {"host": "localhost", "path": "~/node", "binary": str(workmux_exe_path)}
    if override:
        remote["isolation"] = override
    loopback_cli.write_config({"lo": remote}, sandbox={"enabled": global_on})
    subprocess.run(["git", "init", "-q", str(loopback_cli.cwd)], check=True)
    if project_on is not None:
        (loopback_cli.cwd / ".workmux.yaml").write_text(
            yaml.dump({"sandbox": {"enabled": project_on}})
        )
    # Keep this config test from pulling images; runtime probes still execute.
    fake_bin = loopback_cli.home / "fake-bin"
    fake_bin.mkdir()
    write_script(fake_bin / "docker", "exit 1\n")
    loopback_cli.env["PATH"] = f"{fake_bin}:{os.environ['PATH']}"
    result = loopback_cli.run("remote", "doctor", "lo")
    assert f"run {expected}" in result.stdout
    assert not (loopback_cli.home / "node").exists()
