"""Remote agent lifecycle over the loopback transport: `add --remote`, the
attach relay in a real tmux pane, `close`, `open`, `list` and `remove`.

The node runs on this machine under a temporary root with `isolation: none`,
driven by the same scripts SSH would run on a remote."""

import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Dict, Iterator, List, Optional

import pytest
import yaml

from ..conftest import (
    TmuxEnvironment,
    assert_window_exists,
    assert_window_not_exists,
    get_window_name,
    get_worktree_path,
    poll_until,
    run_workmux_command,
    wait_for_pane_output,
)

pytestmark = pytest.mark.tmux_only

LOOPBACK = {"WORKMUX_REMOTE_LOOPBACK": "1"}


class Node:
    """The loopback node: its root, records and tmux server."""

    def __init__(self, root: Path):
        self.root = root

    def tmux(self, *args: str) -> subprocess.CompletedProcess[str]:
        socket = self.root / "run" / "tmux.sock"
        return subprocess.run(
            ["tmux", "-S", str(socket), *args], capture_output=True, text=True
        )

    def sessions(self) -> List[str]:
        result = self.tmux("list-sessions", "-F", "#{session_name}")
        return sorted(result.stdout.split()) if result.returncode == 0 else []

    def workspaces(self) -> List[dict]:
        directory = self.root / "state" / "workspaces"
        if not directory.is_dir():
            return []
        return [
            json.loads(path.read_text())
            for path in sorted(directory.glob("w*.json"))
            if not path.name.endswith(".config.json")
        ]


@pytest.fixture
def node(mux_server: TmuxEnvironment, workmux_exe_path: Path) -> Iterator[Node]:
    # A short root keeps the node's tmux socket under the root.
    root = Path(tempfile.mkdtemp(prefix="wmn-", dir="/tmp")).resolve()
    config = {
        "nerdfont": False,
        "remotes": {
            "lo": {
                "host": "localhost",
                "path": str(root),
                "binary": str(workmux_exe_path),
                "isolation": "none",
            }
        },
    }
    config_dir = mux_server.home_path / ".config" / "workmux"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.yaml").write_text(yaml.dump(config))
    node = Node(root)
    yield node
    node.tmux("kill-server")
    shutil.rmtree(root, ignore_errors=True)


def git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=cwd, check=True, capture_output=True, text=True
    ).stdout.strip()


def local_records(env: TmuxEnvironment) -> List[dict]:
    directory = env.home_path / ".local/state/workmux/remote/lo/workspaces"
    if not directory.is_dir():
        return []
    return [json.loads(p.read_text()) for p in sorted(directory.glob("*.json"))]


def setup_project(repo: Path) -> str:
    """Commit a project config locally (never pushed anywhere) and leave an
    ignored file for `files.copy`; returns the commit."""
    project = {
        "nerdfont": False,
        "panes": [
            {"command": "echo AGENT-READY"},
            {"command": "echo SECOND-PANE", "split": "horizontal"},
        ],
        "files": {"copy": [".env.local"]},
        "post_create": ["echo hooked > hook-ran.txt"],
    }
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    with open(repo / ".gitignore", "a") as gitignore:
        gitignore.write(".env.local\n")
    git(repo, "add", ".workmux.yaml", ".gitignore")
    git(repo, "commit", "-m", "configure remote panes")
    (repo / ".env.local").write_text("SECRET=fake\n")
    return git(repo, "rev-parse", "HEAD")


def wm(
    env: TmuxEnvironment,
    exe: Path,
    repo: Path,
    command: str,
    extra_env: Optional[Dict[str, str]] = None,
    **kwargs,
):
    """Run workmux from the runner pane; commands that focus a remote window
    would otherwise type the next command into the remote shell."""
    assert env.runner_pane_id is not None
    env.tmux(["select-window", "-t", env.runner_pane_id])
    kwargs.setdefault("timeout", 60)
    pre_run_env = {**LOOPBACK, **(extra_env or {})}
    return run_workmux_command(
        env, exe, repo, command, pre_run_env=pre_run_env, **kwargs
    )


def pane_commands(env: TmuxEnvironment, window: str) -> List[str]:
    result = env.tmux(["list-panes", "-t", window, "-F", "#{pane_start_command}"])
    return result.stdout.splitlines()


def test_remote_lifecycle(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    head = setup_project(repo)
    window = get_window_name("feat")

    result = wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    assert "Successfully created remote workspace" in result.stdout, result.stderr
    assert "Pushing" in result.stdout

    # The node has the worktree at the unpushed commit, with files and hooks.
    [workspace] = node.workspaces()
    worktree = Path(workspace["worktree_path"])
    assert worktree.is_relative_to(node.root)
    assert git(worktree, "rev-parse", "HEAD") == head
    assert git(worktree, "branch", "--show-current") == "feat"
    assert (worktree / ".env.local").read_text() == "SECRET=fake\n"
    assert (worktree / "hook-ran.txt").read_text().strip() == "hooked"
    assert not get_worktree_path(repo, "feat").exists()

    # One session per pane, relayed into a local window with the same layout.
    [record] = local_records(mux_server)
    assert record["state"] == "ready"
    ex_id = record["execution"]["ex_id"]
    assert node.sessions() == [f"{ex_id}-0", f"{ex_id}-1"]
    assert_window_exists(mux_server, window)
    commands = pane_commands(mux_server, window)
    assert len(commands) == 2
    assert all("remote attach" in command for command in commands)
    wait_for_pane_output(mux_server, window, "AGENT-READY", timeout=20)

    # Keystrokes reach the remote shell.
    mux_server.tmux(["send-keys", "-t", window, "echo typed-$((40+2))", "Enter"])
    wait_for_pane_output(mux_server, window, "typed-42", timeout=10)

    listing = wm(mux_server, workmux_exe_path, repo, "list")
    assert "remote:lo" in listing.stdout
    rows = json.loads(wm(mux_server, workmux_exe_path, repo, "list --json").stdout)
    [remote_row] = [row for row in rows if row.get("remote") == "lo"]
    assert remote_row["handle"] == "feat"
    assert remote_row["is_open"] is True

    # Closing the window only detaches.
    wm(mux_server, workmux_exe_path, repo, "close feat")
    assert_window_not_exists(mux_server, window)
    assert node.sessions() == [f"{ex_id}-0", f"{ex_id}-1"]

    # Open rejoins the running execution instead of starting another.
    opened = wm(mux_server, workmux_exe_path, repo, "open feat")
    assert "Attached to the running agent" in opened.stdout, opened.stderr
    assert node.sessions() == [f"{ex_id}-0", f"{ex_id}-1"]
    assert_window_exists(mux_server, window)
    wait_for_pane_output(mux_server, window, "typed-42", timeout=20)
    second = wm(mux_server, workmux_exe_path, repo, "open feat")
    assert "Switched to remote workspace" in second.stdout
    assert len(pane_commands(mux_server, window)) == 2

    # Commands that cannot act on remote workspaces yet say so.
    for command in ["merge feat", "path feat", "send feat hi"]:
        refused = wm(mux_server, workmux_exe_path, repo, command, expect_fail=True)
        assert "is a remote workspace on 'lo'" in refused.stderr, command

    # The hook output is unsaved work: without a terminal, remove refuses.
    refused = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "remove feat",
        expect_fail=True,
        stdin_input="",
    )
    assert "--force" in refused.stderr
    assert worktree.is_dir()
    assert len(local_records(mux_server)) == 1

    removed = wm(mux_server, workmux_exe_path, repo, "remove --force feat")
    assert "Removed remote workspace 'feat'" in removed.stdout, removed.stderr
    assert not worktree.exists()
    assert node.workspaces() == []
    assert node.sessions() == []
    node_repo = Path(workspace["repo_path"])
    assert git(node_repo, "branch", "--list", "feat") == ""
    assert git(node_repo, "for-each-ref", "refs/workmux") == ""
    assert local_records(mux_server) == []
    assert_window_not_exists(mux_server, window)


def test_attach_relay_reconnects_and_closes_with_the_session(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    setup_project(repo)
    window = get_window_name("relay")
    wm(mux_server, workmux_exe_path, repo, "add relay --remote lo")
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    wait_for_pane_output(mux_server, window, "AGENT-READY", timeout=20)

    def client_pids() -> List[str]:
        result = node.tmux("list-clients", "-t", f"={ex_id}-0", "-F", "#{client_pid}")
        return result.stdout.split()

    # A lost connection shows a status line and comes back by itself. The
    # reconnect replaces clients that lost connections leave on the node.
    [pid] = client_pids()
    socket = node.root / "run" / "tmux.sock"
    leftover_command = f"env -u TMUX tmux -S {socket} attach -t '={ex_id}-0'"
    mux_server.tmux(["new-window", "-d", "-n", "leftover", leftover_command])
    assert poll_until(lambda: len(client_pids()) == 2, timeout=10)
    os.kill(int(pid), 9)
    wait_for_pane_output(mux_server, window, "retry in", timeout=10)
    assert poll_until(lambda: len(client_pids()) == 1, timeout=15)

    def leftover_open() -> bool:
        result = mux_server.tmux(["list-windows", "-F", "#{window_name}"])
        return "leftover" in result.stdout.split()

    assert poll_until(lambda: not leftover_open(), timeout=5)
    mux_server.tmux(["send-keys", "-t", window, "echo back-$((1+1))", "Enter"])
    wait_for_pane_output(mux_server, window, "back-2", timeout=10)

    # A detach waits for the user; keys other than r and q are dropped.
    node.tmux("detach-client", "-s", f"={ex_id}-0")
    wait_for_pane_output(mux_server, window, "detached", timeout=10)
    mux_server.tmux(["send-keys", "-t", window, "-l", "echo dropped"])
    mux_server.tmux(["send-keys", "-t", window, "r"])
    assert poll_until(lambda: len(client_pids()) == 1, timeout=15)
    mux_server.tmux(["send-keys", "-t", window, "echo rejoined", "Enter"])
    wait_for_pane_output(mux_server, window, "rejoined", timeout=10)
    content = mux_server.capture_pane(window) or ""
    assert "echo droppedecho" not in content

    # When the remote session ends, its local pane closes.
    def local_panes() -> int:
        result = mux_server.tmux(["list-panes", "-t", window], check=False)
        return len(result.stdout.splitlines()) if result.returncode == 0 else 0

    assert local_panes() == 2
    node.tmux("kill-session", "-t", f"={ex_id}-0")
    assert poll_until(lambda: local_panes() == 1, timeout=10)


def test_add_resumes_interrupted_provisioning(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    """A failed `add --remote` is resumed by running it again: the same
    workspace continues where it stopped with the config as it is now, and
    finished hooks do not rerun."""
    repo = mux_repo_path
    allow = node.root / "allow-second-hook"
    project = {
        "nerdfont": False,
        "panes": [{"command": "echo AGENT-READY"}],
        "post_create": ["echo ran >> hooks.log", f"test -e {allow}"],
    }
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "configure hooks")

    failed = wm(
        mux_server, workmux_exe_path, repo, "add feat --remote lo", expect_fail=True
    )
    assert "test -e" in failed.stderr, failed.stderr
    assert "again to resume it" in failed.stderr
    [record] = local_records(mux_server)
    assert record["state"] == "provisioning"
    assert_window_not_exists(mux_server, get_window_name("feat"))

    # The user fixes the hook and adds a pane before trying again.
    project["post_create"] = ["echo ran >> hooks.log", "true"]
    project["panes"].append({"command": "echo SECOND-PANE", "split": "horizontal"})
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    resumed = wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    assert "Resuming remote workspace 'feat'" in resumed.stdout, resumed.stderr
    [workspace] = node.workspaces()
    assert workspace["ws_id"] == record["ws_id"]
    worktree = Path(workspace["worktree_path"])
    assert (worktree / "hooks.log").read_text() == "ran\n"
    [ready] = local_records(mux_server)
    assert ready["state"] == "ready"
    ex_id = ready["execution"]["ex_id"]
    assert node.sessions() == [f"{ex_id}-0", f"{ex_id}-1"]
    assert len(pane_commands(mux_server, get_window_name("feat"))) == 2
    wait_for_pane_output(mux_server, get_window_name("feat"), "AGENT-READY", timeout=20)


def test_names_resolve_to_one_side(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    """A branch belongs to one workspace, and a local handle outranks a
    remote branch, so a name never reaches the wrong side."""
    repo = mux_repo_path
    setup_project(repo)
    wm(mux_server, workmux_exe_path, repo, "add x --name r1 --remote lo")

    for command in ["add x", "add x --name r2 --remote lo"]:
        refused = wm(mux_server, workmux_exe_path, repo, command, expect_fail=True)
        assert "Branch 'x' belongs to remote workspace 'r1' on 'lo'" in refused.stderr

    # A local worktree named `x` on another branch is reached by `x`.
    wm(mux_server, workmux_exe_path, repo, "add y --name x")
    local = get_worktree_path(repo, "x")
    path = wm(mux_server, workmux_exe_path, repo, "path x")
    assert Path(path.stdout.strip()).resolve() == local.resolve()
    wm(mux_server, workmux_exe_path, repo, "remove --force x")
    assert not local.exists()
    assert [ws["handle"] for ws in node.workspaces()] == ["r1"]
    assert [record["handle"] for record in local_records(mux_server)] == ["r1"]


def test_laptop_only_ignores_apply_on_the_node(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    """A copied file the laptop ignores through rules the node lacks stays
    ignored there, so an untouched workspace is clean and removes without
    --force."""
    repo = mux_repo_path
    project = {
        "nerdfont": False,
        "panes": [{"command": "echo AGENT-READY"}],
        "files": {"copy": [".env.local"]},
    }
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "copy env")
    exclude = repo / ".git" / "info" / "exclude"
    exclude.parent.mkdir(parents=True, exist_ok=True)
    with open(exclude, "a") as rules:
        rules.write(".env.local\n")
    (repo / ".env.local").write_text("SECRET=fake\n")

    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    [workspace] = node.workspaces()
    worktree = Path(workspace["worktree_path"])
    assert (worktree / ".env.local").read_text() == "SECRET=fake\n"
    assert git(worktree, "status", "--porcelain") == ""

    removed = wm(mux_server, workmux_exe_path, repo, "remove feat", stdin_input="")
    assert "Removed remote workspace 'feat'" in removed.stdout, removed.stderr
    assert node.workspaces() == []


def test_host_only_workspaces_never_need_docker(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    """`isolation: none` starts and removes without docker, even where a
    docker CLI exists but cannot reach its daemon."""
    stub_dir = tmp_path / "bin"
    stub_dir.mkdir()
    stub = stub_dir / "docker"
    stub.write_text(
        "#!/bin/sh\necho 'Cannot connect to the Docker daemon' >&2\nexit 1\n"
    )
    stub.chmod(0o755)
    broken_docker = {"PATH": f"{stub_dir}:{os.environ['PATH']}"}
    repo = mux_repo_path
    setup_project(repo)

    added = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "add feat --remote lo",
        extra_env=broken_docker,
    )
    assert "Successfully created remote workspace" in added.stdout, added.stderr
    removed = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "remove --force feat",
        extra_env=broken_docker,
    )
    assert "Removed remote workspace 'feat'" in removed.stdout, removed.stderr
    assert node.workspaces() == []
