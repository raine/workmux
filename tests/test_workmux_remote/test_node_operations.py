"""Lifecycle requests typed in the node's own shell, executed by the laptop."""

from pathlib import Path

import pytest

from ..conftest import TmuxEnvironment, get_window_name, poll_until
from .conftest import Node, git, local_records, wm
from .test_homecoming import commit_on_node, configure, node_worktree
from .test_siblings import node_pane

pytestmark = pytest.mark.tmux_only


def type_command(node: Node, ex_id: str, command: str) -> None:
    node.tmux("send-keys", "-t", f"{ex_id}-0:", command, "Enter")


def request(node: Node, ex_id: str, command: str) -> None:
    type_command(node, ex_id, f"{node.root}/run/bin/workmux {command}")


def wait_message(node: Node, ex_id: str, text: str, timeout: int = 30) -> None:
    assert poll_until(lambda: text in node_pane(node, ex_id), timeout=timeout), (
        node_pane(node, ex_id)
    )


def wait_removed(env: TmuxEnvironment, node: Node) -> None:
    assert poll_until(
        lambda: not local_records(env)
        and not node.workspaces()
        and get_window_name("feat") not in env.list_windows(),
        timeout=40,
    )


@pytest.mark.parametrize("operation", ["rm", "merge", "merge --rebase"])
def test_node_cleanup(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    operation: str,
):
    configure(mux_repo_path)
    wm(mux_server, workmux_exe_path, mux_repo_path, "add feat --remote lo")
    [record] = local_records(mux_server)
    if operation.startswith("merge"):
        commit_on_node(node_worktree(node, "feat"), "work.txt", "merged\n")
    request(node, record["execution"]["ex_id"], operation)
    wait_removed(mux_server, node)
    if operation.startswith("merge"):
        assert git(mux_repo_path, "show", "main:work.txt") == "merged"


def test_unmerged_refuses_then_force(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    configure(mux_repo_path)
    wm(mux_server, workmux_exe_path, mux_repo_path, "add feat --remote lo")
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    commit_on_node(node_worktree(node, "feat"), "work.txt", "unmerged\n")
    request(node, ex_id, "rm")
    wait_message(node, ex_id, "not merged into")
    assert local_records(mux_server)
    request(node, ex_id, "rm --force")
    wait_removed(mux_server, node)


@pytest.mark.parametrize(
    ("command", "message"),
    [
        ("rm other", "only request an operation on its own handle"),
        ("merge other", "only request an operation on its own handle"),
        ("rm feat feat", "only request an operation on its own handle"),
        ("rm --gone", "does not support"),
        ("rm --all", "does not support"),
        ("rm --keep-branch", "does not support"),
        ("close", "the window is on the laptop"),
        ("merge --squash", "needs interaction"),
        ("merge --into main~1", "existing laptop branch"),
    ],
)
def test_refusals(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    command: str,
    message: str,
):
    configure(mux_repo_path)
    wm(mux_server, workmux_exe_path, mux_repo_path, "add feat --remote lo")
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    request(node, ex_id, command)
    wait_message(node, ex_id, message)
    assert len(local_records(mux_server)) == len(node.workspaces()) == 1
    assert get_window_name("feat") in mux_server.list_windows()


def test_dirty_refuses(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    configure(mux_repo_path)
    wm(mux_server, workmux_exe_path, mux_repo_path, "add feat --remote lo")
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    (node_worktree(node, "feat") / "dirty").write_text("uncommitted")
    request(node, ex_id, "rm")
    wait_message(node, ex_id, "has uncommitted changes")
    assert local_records(mux_server) and node.workspaces()


def test_timeout_withdraws(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    configure(mux_repo_path)
    wm(mux_server, workmux_exe_path, mux_repo_path, "add feat --remote lo")
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    wm(mux_server, workmux_exe_path, mux_repo_path, "close feat")
    request(node, ex_id, "rm --force")
    wait_message(node, ex_id, "request withdrawn")
    assert not node.workspaces()[0].get("operation")
    wm(mux_server, workmux_exe_path, mux_repo_path, "open feat")
    # A fresh observer sees no request and leaves the workspace alone.
    wm(mux_server, workmux_exe_path, mux_repo_path, "list")
    assert local_records(mux_server) and node.workspaces()


def test_merge_conflict(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    configure(mux_repo_path)
    (mux_repo_path / "conflict").write_text("base\n")
    git(mux_repo_path, "add", "conflict")
    git(mux_repo_path, "commit", "-m", "base")
    wm(mux_server, workmux_exe_path, mux_repo_path, "add feat --remote lo")
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    commit_on_node(node_worktree(node, "feat"), "conflict", "node\n")
    (mux_repo_path / "conflict").write_text("laptop\n")
    git(mux_repo_path, "commit", "-am", "laptop")
    request(node, ex_id, "merge")
    wait_message(node, ex_id, "merge conflict")
    assert local_records(mux_server) and node.workspaces()
    assert git(mux_repo_path, "show", "main:conflict") == "laptop"


def test_staged_changes_and_keep(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    configure(mux_repo_path)
    wm(mux_server, workmux_exe_path, mux_repo_path, "add feat --remote lo")
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    worktree = node_worktree(node, "feat")
    commit_on_node(worktree, "work.txt", "committed\n")
    (worktree / "staged").write_text("staged\n")
    git(worktree, "add", "staged")
    request(node, ex_id, "merge --keep")
    wait_message(node, ex_id, "needs interaction")
    assert git(mux_repo_path, "branch", "--show-current") == "main"
    request(node, ex_id, "merge --keep --ignore-uncommitted")
    wait_message(node, ex_id, "Laptop operation completed")
    assert git(mux_repo_path, "show", "main:work.txt") == "committed"
    assert (worktree / "staged").read_text() == "staged\n"
    assert local_records(mux_server) and node.workspaces()
