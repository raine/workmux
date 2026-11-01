"""Project transfer refs keep pushes incremental without retaining workspace branches."""

import os
import pty
import subprocess
import threading
import time
from pathlib import Path

import pytest

from ..conftest import TmuxEnvironment
from .conftest import LOOPBACK, Node, agent_dirs_env, commit_project, git, wm

pytestmark = pytest.mark.tmux_only
TRANSFER_REF = "refs/workmux/project/transfer"


def packs(repo: Path) -> dict[str, tuple[int, int]]:
    return {
        p.name: (p.stat().st_size, p.stat().st_mtime_ns)
        for p in (repo / ".git/objects/pack").glob("*.pack")
    }


def test_project_transfer_survives_remove_and_gc(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    commit_project(repo, {"nerdfont": False, "panes": [{"command": "echo READY"}]})
    (repo / "payload").write_bytes(os.urandom(2 * 1024 * 1024))
    git(repo, "add", "payload")
    git(repo, "commit", "-m", "large base")
    # More than receive.unpackLimit objects ensures the first push stays packed.
    for i in range(40):
        (repo / "small").write_text(f"revision {i}\n")
        git(repo, "add", "small")
        git(repo, "commit", "-m", f"revision {i}")
    first_head = git(repo, "rev-parse", "HEAD")

    def add() -> float:
        started = time.monotonic()
        result = wm(mux_server, workmux_exe_path, repo, "add transfer --remote lo")
        assert "Successfully created remote workspace" in result.stdout, result.stderr
        # The runner redirects stderr: no progress chatter in noninteractive use.
        assert "Writing objects:" not in result.stderr
        [workspace] = node.workspaces()
        assert git(Path(workspace["worktree_path"]), "rev-parse", "HEAD") == git(
            repo, "rev-parse", "HEAD"
        )
        assert (
            git(Path(workspace["worktree_path"]), "branch", "--show-current")
            == "transfer"
        )
        return time.monotonic() - started

    cold_seconds = add()
    [workspace] = node.workspaces()
    remote = Path(workspace["repo_path"])
    git(remote, "config", "receive.unpackLimit", "1")
    initial = packs(remote)
    assert initial
    initial_bytes = sum(size for size, _ in initial.values())
    assert initial_bytes > 2 * 1024 * 1024

    def remove() -> None:
        wm(mux_server, workmux_exe_path, repo, "remove --force transfer")
        assert not node.workspaces()
        assert git(remote, "for-each-ref", "--format=%(refname)") == TRANSFER_REF

    remove()
    assert git(remote, "rev-parse", TRANSFER_REF) == first_head
    wm(mux_server, workmux_exe_path, repo, "remote gc lo --force")
    assert git(remote, "rev-parse", TRANSFER_REF) == first_head
    # Ordinary git maintenance must also keep the advertised history reachable.
    git(remote, "gc", "--prune=now")
    before_warm = packs(remote)
    warm_seconds = add()
    assert packs(remote) == before_warm
    remove()

    (repo / "small").write_text("advanced base\n")
    git(repo, "add", "small")
    git(repo, "commit", "-m", "advance source")
    advanced_seconds = add()
    advanced = packs(remote)
    incremental_bytes = sum(
        size for name, (size, _) in advanced.items() if name not in before_warm
    )
    assert 0 < incremental_bytes < initial_bytes // 100
    assert git(remote, "rev-parse", TRANSFER_REF) == git(repo, "rev-parse", "HEAD")
    remove()
    assert git(remote, "for-each-ref", "refs/heads/") == ""

    # A retained newer tip must not change an explicitly selected older base.
    source_branch = git(repo, "branch", "--show-current")
    git(repo, "checkout", "-b", "older-base", first_head)
    add()
    assert git(remote, "rev-parse", TRANSFER_REF) == first_head
    remove()
    git(repo, "checkout", source_branch)

    # A fixture-only control models the absence of the project transfer ref.
    git(remote, "update-ref", "-d", TRANSFER_REF)
    git(repo, "commit", "--allow-empty", "-m", "cold control")
    control_seconds = add()
    control_bytes = sum(
        size for name, (size, _) in packs(remote).items() if name not in advanced
    )
    assert control_bytes > initial_bytes * 0.9
    remove()
    print(
        f"transfer evidence: initial={initial_bytes}B/{cold_seconds:.3f}s "
        f"warm=0B/{warm_seconds:.3f}s "
        f"advanced={incremental_bytes}B/{advanced_seconds:.3f}s "
        f"no-ref-control={control_bytes}B/{control_seconds:.3f}s"
    )


def test_terminal_progress_and_push_failure(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    commit_project(repo, {"nerdfont": False, "panes": [{"command": "echo READY"}]})
    (repo / "payload").write_bytes(os.urandom(256 * 1024))
    git(repo, "add", "payload")
    git(repo, "commit", "-m", "payload")
    server_pid = mux_server.tmux(["display-message", "-p", "#{pid}"]).stdout.strip()
    env = {
        **mux_server.env,
        **LOOPBACK,
        **agent_dirs_env(mux_server.home_path),
        "TMUX": f"{mux_server.socket_path},{server_pid},0",
        "TMUX_PANE": str(mux_server.runner_pane_id),
    }
    master, slave = pty.openpty()
    chunks: list[bytes] = []

    def drain() -> None:
        while True:
            try:
                chunk = os.read(master, 4096)
            except OSError:
                return
            if not chunk:
                return
            chunks.append(chunk)

    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    try:
        result = subprocess.run(
            [str(workmux_exe_path), "add", "progress", "--remote", "lo"],
            cwd=repo,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=slave,
            timeout=60,
        )
    finally:
        os.close(slave)
        reader.join(timeout=10)
        os.close(master)
    stderr = b"".join(chunks).decode()
    assert result.returncode == 0, stderr
    assert "Writing objects:" in stderr
    assert "refs/workmux/" not in stderr
    [workspace] = node.workspaces()
    remote = Path(workspace["repo_path"])
    retained = git(remote, "rev-parse", TRANSFER_REF)
    wm(mux_server, workmux_exe_path, repo, "remove --force progress")

    # A rejected transfer ref must surface Git's error and stop provisioning.
    git(remote, "config", "receive.hideRefs", TRANSFER_REF)
    git(repo, "commit", "--allow-empty", "-m", "rejected base")
    failed = wm(
        mux_server, workmux_exe_path, repo, "add rejected --remote lo", expect_fail=True
    )
    assert "Failed to push the base commit" in failed.stderr
    assert "deny updating a hidden ref" in failed.stderr
    assert git(remote, "rev-parse", TRANSFER_REF) == retained
    assert git(remote, "for-each-ref", "refs/heads/rejected") == ""
    [partial] = node.workspaces()
    assert not Path(partial["worktree_path"]).exists()
