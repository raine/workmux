"""`workmux remote gc` over the loopback transport: orphans planted on the
node and on the laptop are listed, kept on a dry run, and removed with
`--force`, while a healthy workspace stays."""

import json
import os
import shutil
import time
from pathlib import Path

import pytest

from ..conftest import TmuxEnvironment, get_window_name, wait_for_pane_output
from .conftest import Node, commit_project, git, local_records, wm

pytestmark = pytest.mark.tmux_only

ORPHAN_WS = "wdeadbeef0000"
OLD = time.time() - 3600


def age(path: Path) -> None:
    os.utime(path, (OLD, OLD))


def test_gc_lists_and_removes_orphans(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    commit_project(
        repo, {"nerdfont": False, "panes": [{"command": "echo AGENT-READY"}]}
    )
    for branch in ["healthy", "forgotten", "dirty"]:
        added = wm(mux_server, workmux_exe_path, repo, f"add {branch} --remote lo")
        assert "Successfully created remote workspace" in added.stdout, added.stderr
    wait_for_pane_output(
        mux_server, get_window_name("healthy"), "AGENT-READY", timeout=20
    )
    records = {r["branch"]: r for r in local_records(mux_server)}
    by_branch = {ws["branch"]: ws for ws in node.workspaces()}
    healthy = by_branch["healthy"]
    records_dir = mux_server.home_path / ".local/state/workmux/remote/lo/workspaces"

    # Workspaces the laptop forgot, one of them with work only on the node.
    for branch in ["forgotten", "dirty"]:
        wm(mux_server, workmux_exe_path, repo, f"close {branch}")
        (records_dir / f"{records[branch]['ws_id']}.json").unlink()
    (Path(by_branch["dirty"]["worktree_path"]) / "notes.txt").write_text("unsaved\n")

    # Leftovers of interrupted calls.
    node_repo = Path(healthy["repo_path"])
    git(node_repo, "update-ref", f"refs/workmux/base/{ORPHAN_WS}", healthy["base_sha"])
    node.tmux("new-session", "-d", "-s", "x0123456789ab-0")
    side = node.root / "state" / "workspaces" / f"{ORPHAN_WS}.config.json"
    side.write_text("{}")
    age(side)
    fresh = node.root / "state" / "workspaces" / "wfeedfeed0000.config.json"
    fresh.write_text("{}")
    secret = node.root / "secrets" / "x0123456789ab-0.tmux"
    secret.write_text("set-environment X y\n")
    age(secret)
    ghost = node_repo.parent / "worktrees" / "ghost"
    ghost.mkdir()
    (ghost / "file").write_text("x")
    # A clean checkout whose only commit is on its detached HEAD.
    detached = node_repo.parent / "worktrees" / "detached"
    git(node_repo, "worktree", "add", "--detach", str(detached), healthy["base_sha"])
    git(
        detached,
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "--allow-empty",
        "-m",
        "only here",
    )
    old_pin = node.root / "bin" / "workmux-0.0.1"
    shutil.copy(
        node.root / "bin" / records["healthy"]["execution"]["node_bin"], old_pin
    )
    stale = {
        **records["healthy"],
        "ws_id": "w111111111111",
        "handle": "stale",
        "branch": "stale",
        "created_at": int(OLD),
        "execution": None,
    }
    (records_dir / "w111111111111.json").write_text(json.dumps(stale))

    listing = wm(mux_server, workmux_exe_path, repo, "remote gc lo --dry-run")
    out = listing.stdout
    for expected in [
        "workspace 'forgotten'",
        "workspace 'dirty'",
        "(kept without --force)",
        "tmux session x0123456789ab-0",
        f"refs/workmux/base/{ORPHAN_WS}",
        f"state/workspaces/{ORPHAN_WS}.config.json",
        "secrets/x0123456789ab-0.tmux",
        "worktrees/ghost",
        "bin/workmux-0.0.1",
        "local record of 'stale'",
    ]:
        assert expected in out, (expected, out, listing.stderr)
    [detached_line] = [
        line for line in out.splitlines() if "worktrees/detached" in line
    ]
    assert "(kept without --force)" in detached_line, out
    assert "workspace 'healthy'" not in out
    assert "wfeedfeed0000" not in out
    assert len(node.workspaces()) == 3 and old_pin.exists()

    refused = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "remote gc lo",
        expect_fail=True,
        stdin_input="",
    )
    assert "Nothing removed" in refused.stderr
    assert len(node.workspaces()) == 3

    cleaned = wm(
        mux_server, workmux_exe_path, repo, "remote gc lo --force", timeout=120
    )
    assert "Removed" in cleaned.stdout, cleaned.stderr
    assert [ws["branch"] for ws in node.workspaces()] == ["healthy"]
    assert [r["branch"] for r in local_records(mux_server)] == ["healthy"]
    assert node.sessions() == [f"{records['healthy']['execution']['ex_id']}-0"]
    refs = git(node_repo, "for-each-ref", "--format=%(refname)", "refs/workmux/")
    assert refs.split() == [f"refs/workmux/base/{healthy['ws_id']}"]
    assert not side.exists() and not secret.exists() and not ghost.exists()
    assert not detached.exists()
    assert fresh.exists()
    assert not old_pin.exists()
    assert (node.root / "bin" / records["healthy"]["execution"]["node_bin"]).exists()

    again = wm(mux_server, workmux_exe_path, repo, "remote gc lo --dry-run")
    assert "Nothing to clean up on 'lo'" in again.stdout, again.stdout
