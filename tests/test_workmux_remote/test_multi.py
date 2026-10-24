"""Several remote agents on one node at once: two of one repository and one
of another, each with its own worktree, sessions and window."""

import json
import tempfile
from pathlib import Path

import pytest

from ..conftest import (
    TmuxEnvironment,
    get_window_name,
    setup_git_repo,
    wait_for_pane_output,
)
from .conftest import Node, commit_project, local_records, wm

pytestmark = pytest.mark.tmux_only


def configure(repo: Path, marker: str) -> None:
    commit_project(repo, {"nerdfont": False, "panes": [{"command": f"echo {marker}"}]})


def test_agents_of_two_repos_share_a_node(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    first = mux_repo_path
    second = Path(tempfile.mkdtemp(prefix="second-repo-", dir=first.parent)) / "other"
    second.mkdir()
    setup_git_repo(second)
    configure(first, "FIRST-REPO")
    configure(second, "SECOND-REPO")

    for repo, branch in [(first, "alpha"), (first, "beta"), (second, "gamma")]:
        added = wm(mux_server, workmux_exe_path, repo, f"add {branch} --remote lo")
        assert "Successfully created remote workspace" in added.stdout, added.stderr
    for branch, marker in [
        ("alpha", "FIRST-REPO"),
        ("beta", "FIRST-REPO"),
        ("gamma", "SECOND-REPO"),
    ]:
        wait_for_pane_output(mux_server, get_window_name(branch), marker, timeout=20)

    workspaces = {ws["branch"]: ws for ws in node.workspaces()}
    assert sorted(workspaces) == ["alpha", "beta", "gamma"]
    assert workspaces["alpha"]["project_id"] == workspaces["beta"]["project_id"]
    assert workspaces["alpha"]["project_id"] != workspaces["gamma"]["project_id"]
    assert workspaces["alpha"]["repo_path"] == workspaces["beta"]["repo_path"]
    worktrees = {ws["worktree_path"] for ws in workspaces.values()}
    assert len(worktrees) == 3
    executions = [r["execution"]["ex_id"] for r in local_records(mux_server)]
    assert node.sessions() == sorted(f"{ex}-0" for ex in executions)

    # Each repository lists its own; --all lists every one.
    rows = json.loads(wm(mux_server, workmux_exe_path, first, "list --json").stdout)
    assert sorted(r["branch"] for r in rows if r.get("remote")) == ["alpha", "beta"]
    rows = json.loads(wm(mux_server, workmux_exe_path, second, "list --json").stdout)
    assert [r["branch"] for r in rows if r.get("remote")] == ["gamma"]

    # Removing one leaves the others running.
    wm(mux_server, workmux_exe_path, first, "remove --force beta")
    assert sorted(ws["branch"] for ws in node.workspaces()) == ["alpha", "gamma"]
    assert len(node.sessions()) == 2
