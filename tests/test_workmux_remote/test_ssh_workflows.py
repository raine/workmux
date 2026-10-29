"""Workspace workflows over real SSH, gated like test_remote_host."""

import hashlib
import json
import shlex
import uuid

import pytest
import yaml

from ..conftest import (
    get_window_name,
    get_worktree_path,
    poll_until,
    wait_for_pane_output,
)
from .conftest import commit_project, git, local_records, wm
from .test_remote_host import BINARY, HOST, pytestmark, remote_sh

pytestmark = [*pytestmark, pytest.mark.tmux_only]


class SshWorkspace:
    def __init__(self, env, exe, repo, root):
        self.env, self.exe, self.repo, self.root = env, exe, repo, root

    def run(self, command, **kwargs):
        return wm(
            self.env,
            self.exe,
            self.repo,
            command,
            extra_env={"WORKMUX_REMOTE_LOOPBACK": "0"},
            timeout=180,
            **kwargs,
        )

    def shell(self, command):
        result = remote_sh(command)
        assert result.returncode == 0, result.stdout + result.stderr
        return result.stdout.strip()

    def tmux(self, *args):
        return self.shell(
            shlex.join(["tmux", "-S", f"{self.root}/run/tmux.sock", *args])
        )

    def record(self, handle):
        return next(r for r in local_records(self.env) if r["handle"] == handle)

    def at(self, handle, command):
        path = self.record(handle)["remote_worktree"]
        return self.shell(f"cd {shlex.quote(path)} && {command}")

    def wait_attached(self, handle):
        session = self.record(handle)["execution"]["ex_id"] + "-0"
        assert poll_until(
            lambda: bool(
                self.tmux("list-clients", "-t", session, "-F", "#{client_pid}")
            ),
            timeout=30,
        )


@pytest.fixture
def ssh_workspace(mux_server, workmux_exe_path, mux_repo_path):
    assert HOST and BINARY
    result = remote_sh('printf %s "$HOME"')
    assert result.returncode == 0, result.stderr
    home = result.stdout
    root = f"{home}/.workmux-ssh-e2e-{uuid.uuid4().hex[:8]}"
    config_dir = mux_server.home_path / ".config/workmux"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.yaml").write_text(
        yaml.safe_dump(
            {
                "nerdfont": False,
                "remotes": {
                    "lo": {
                        "host": HOST,
                        "path": root,
                        "binary": BINARY,
                        "isolation": "none",
                    }
                },
            }
        )
    )
    workspace = SshWorkspace(mux_server, workmux_exe_path, mux_repo_path, root)
    try:
        yield workspace
    finally:
        for record in local_records(mux_server):
            workspace.run(f"remove --force {shlex.quote(record['handle'])}")
        remote_sh(
            f"tmux -S {shlex.quote(root + '/run/tmux.sock')} kill-server 2>/dev/null; "
            f"rm -rf -- {shlex.quote(root)}"
        )


def test_ssh_lifecycle_reconnect_and_remote_merge(ssh_workspace):
    ws = ssh_workspace
    repo = ws.repo
    agent = ws.root + "/test-agent"
    script = (
        '#!/bin/sh\necho SSH-READY\nwhile IFS= read -r line; do eval "$line"; done\n'
    )
    ws.shell(
        f"mkdir -p {shlex.quote(ws.root)} && printf %s {shlex.quote(script)} > {shlex.quote(agent)} && chmod 755 {shlex.quote(agent)}"
    )
    (repo / ".gitignore").write_text("seed.bin\nlocal-only\n")
    git(repo, "add", ".gitignore")
    commit_project(
        repo,
        {
            "agent": agent,
            "panes": [
                {"command": "<agent>"},
                {"command": "echo SECOND-READY", "split": "horizontal"},
            ],
            "remote_files": {"copy": ["seed.bin"]},
            "files": {"copy": ["local-only"]},
            "pre_merge": ["echo LIVE-SSH-MERGE-HOOK"],
        },
    )
    (repo / "seed.bin").write_bytes(bytes(range(256)) * 1024)
    (repo / "local-only").write_text("local")
    head = git(repo, "rev-parse", "HEAD")
    ws.run("add live --remote lo")
    assert ws.at("live", "git rev-parse HEAD") == head
    assert ws.at("live", "git remote") == ""
    assert (
        ws.at("live", "sha256sum seed.bin").split()[0]
        == hashlib.sha256((repo / "seed.bin").read_bytes()).hexdigest()
    )
    ws.at("live", "test ! -e local-only")
    assert not get_worktree_path(repo, "live").exists()
    window = get_window_name("live")
    record = ws.record("live")
    session = record["execution"]["ex_id"] + "-0"
    assert poll_until(
        lambda: "SSH-READY"
        in ws.tmux("capture-pane", "-p", "-J", "-S", "-", "-t", session),
        timeout=30,
    )
    assert poll_until(
        lambda: bool(ws.tmux("list-clients", "-t", session, "-F", "#{client_pid}")),
        timeout=30,
    )
    ws.env.tmux(["send-keys", "-t", window, "echo LIVE-$((6*7))", "Enter"])
    wait_for_pane_output(ws.env, window, "LIVE-42", timeout=30)
    ws.env.tmux(
        [
            "send-keys",
            "-t",
            window,
            "workmux set-window-status working && echo STATUS-REPORTED",
            "Enter",
        ]
    )
    wait_for_pane_output(ws.env, window, "STATUS-REPORTED", timeout=30)

    def statuses():
        return json.loads(ws.run("status --json live").stdout)["agents"]

    assert poll_until(
        lambda: any(r["status"] == "working" for r in statuses()), timeout=30
    )
    before = ws.tmux("list-sessions", "-F", "#{session_name}")
    ws.run("close live")
    assert ws.tmux("list-sessions", "-F", "#{session_name}") == before
    assert any(
        r["status"] == "working" and not r["remote"]["attached"] for r in statuses()
    )
    ws.run("wait live --status working --timeout 10")
    ws.run("send live 'echo SENT-$((8*8))'")
    assert poll_until(lambda: "SENT-64" in ws.run("capture live").stdout, timeout=30)
    ws.run("open live")
    wait_for_pane_output(ws.env, window, "LIVE-42", timeout=30)
    assert poll_until(
        lambda: bool(ws.tmux("list-clients", "-t", session, "-F", "#{client_pid}")),
        timeout=30,
    )
    pid = ws.tmux("list-clients", "-t", session, "-F", "#{client_pid}")
    ws.shell(f"kill -9 {int(pid)}")
    assert poll_until(
        lambda: ws.tmux("list-clients", "-t", session, "-F", "#{client_pid}")
        not in ("", pid),
        timeout=30,
    )
    ws.env.tmux(["send-keys", "-t", window, "echo RECONNECTED-$((7*7))", "Enter"])
    wait_for_pane_output(ws.env, window, "RECONNECTED-49", timeout=30)
    rows = json.loads(ws.run("list --json").stdout)
    assert any(r.get("remote") == "lo" and r["handle"] == "live" for r in rows)
    ws.at(
        "live",
        "printf 'remote work\\n' > result.txt; git add result.txt; "
        "git -c user.name=Test -c user.email=test@example.com commit -m result",
    )
    ws.env.tmux(["send-keys", "-t", window, "workmux merge", "Enter"])
    assert poll_until(lambda: (repo / "result.txt").exists(), timeout=60)
    assert poll_until(lambda: not local_records(ws.env), timeout=60)
    assert (repo / "result.txt").read_text() == "remote work\n"
    logs = ws.run("remote logs lo").stdout
    assert logs.strip()
    log_dir = ws.env.home_path / ".local/state/workmux/remote/lo/operation-logs"
    assert any("LIVE-SSH-MERGE-HOOK" in p.read_text() for p in log_dir.glob("*"))
    assert window not in ws.env.list_windows()


def test_ssh_rebase_and_pull_preserve_work(ssh_workspace):
    ws = ssh_workspace
    repo = ws.repo
    (repo / "tracked.txt").write_text("base\n")
    git(repo, "add", "tracked.txt")
    commit_project(repo, {"panes": [{"command": "echo PULL-READY"}]})
    ws.run("add pullme --remote lo")
    ws.at(
        "pullme",
        "printf 'committed\\n' > commit.txt; git add commit.txt; "
        "git -c user.name=Test -c user.email=test@example.com commit -m work",
    )
    (repo / "main.txt").write_text("base advanced\n")
    git(repo, "add", "main.txt")
    git(repo, "commit", "-m", "advance base")
    ws.run("rebase pullme")
    assert ws.at("pullme", "cat main.txt") == "base advanced"
    ws.at(
        "pullme",
        "printf 'staged\\n' > staged.txt; git add staged.txt; "
        "printf 'unstaged\\n' > tracked.txt; printf 'untracked\\n' > new.txt",
    )
    refused = ws.run("remove pullme", expect_fail=True, stdin_input="")
    assert "uncommitted" in refused.stderr
    remote_head = ws.at("pullme", "git rev-parse HEAD")
    ws.run("remote pull pullme")
    local = get_worktree_path(repo, "pullme")
    assert git(local, "rev-parse", "HEAD") == remote_head
    assert git(local, "diff", "--cached", "--name-only") == "staged.txt"
    assert git(local, "diff", "--name-only") == "tracked.txt"
    assert (local / "new.txt").read_text() == "untracked\n"
    assert (local / "tracked.txt").read_text() == "unstaged\n"
    assert local_records(ws.env) == []
    ws.run("remove --force pullme")
    gc = ws.run("remote gc lo --dry-run")
    assert gc.exit_code == 0


def test_ssh_container_isolation_survives_config_change(ssh_workspace):
    ws = ssh_workspace
    config_path = ws.env.home_path / ".config/workmux/config.yaml"
    config = yaml.safe_load(config_path.read_text())
    del config["remotes"]["lo"]["isolation"]
    config_path.write_text(yaml.safe_dump(config))
    commit_project(
        ws.repo,
        {
            "sandbox": {"enabled": True, "target": "all"},
            "panes": [
                {
                    "command": "test -f /.dockerenv && echo CONTAINER-READY && printf 'COLOR=%s\\n' \"$COLORTERM\"; exec bash"
                }
            ],
        },
    )
    added = ws.run("add boxed --remote lo")
    assert "Runs: in Docker" in added.stdout
    window = get_window_name("boxed")
    wait_for_pane_output(ws.env, window, "CONTAINER-READY", timeout=60)
    wait_for_pane_output(ws.env, window, "COLOR=truecolor", timeout=30)
    ws.env.tmux(
        [
            "send-keys",
            "-t",
            window,
            "test -f /.dockerenv && printf 'container work\\n' > box.txt",
            "Enter",
        ]
    )
    assert poll_until(
        lambda: ws.at("boxed", "cat box.txt 2>/dev/null || true") == "container work",
        timeout=30,
    )
    ws.run("close boxed")
    commit_project(ws.repo, {"sandbox": {"enabled": False}})
    ws.run("open boxed")
    ws.wait_attached("boxed")
    ws.env.tmux(
        [
            "send-keys",
            "-t",
            window,
            "test -f /.dockerenv && echo STILL-$((10+2))-BOXED",
            "Enter",
        ]
    )
    wait_for_pane_output(ws.env, window, "STILL-12-BOXED", timeout=30)
    ws.run("remote pull boxed")
    assert (
        get_worktree_path(ws.repo, "boxed") / "box.txt"
    ).read_text() == "container work\n"
    ws.run("remove --force boxed")
