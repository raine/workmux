"""Cleanup from a local shell pane in the remote workspace's own window."""

import shlex
import time
from pathlib import Path

import pytest

from ..conftest import TmuxEnvironment, get_window_name, poll_until
from .conftest import LOOPBACK, Node, agent_dirs_env, git, local_records, wm
from .test_homecoming import commit_on_node, configure, node_worktree

pytestmark = pytest.mark.tmux_only


def run_inside(env: TmuxEnvironment, exe: Path, repo: Path, command: str):
    """Use a local split, not the relay's remote shell; it dies with the window."""
    stdout = env.tmp_path / "inside-stdout"
    stderr = env.tmp_path / "inside-stderr"
    exited = env.tmp_path / "inside-exit"
    for path in (stdout, stderr, exited):
        path.unlink(missing_ok=True)
    overrides = {
        **LOOPBACK,
        **agent_dirs_env(env.home_path),
        "HOME": str(env.home_path),
        "PATH": env.env["PATH"],
        "WORKMUX_TEST": "1",
    }
    invocation = shlex.join(
        ["env", *(f"{key}={value}" for key, value in overrides.items()), str(exe)]
        + shlex.split(command)
    )
    shell = (
        f"cd {shlex.quote(str(repo))} && {invocation} "
        f">{shlex.quote(str(stdout))} 2>{shlex.quote(str(stderr))}; "
        f"echo $? >{shlex.quote(str(exited))}; exec /bin/sh"
    )
    env.tmux(["split-window", "-d", "-t", get_window_name("feat"), shell])
    return stdout, stderr, exited


@pytest.mark.parametrize("operation", ["remove", "merge", "multi", "offline"])
def test_cleanup_inside_own_window(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    operation: str,
    record_property,
):
    repo = mux_repo_path
    configure(repo)
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    command = "remove feat"
    if operation == "merge":
        commit_on_node(node_worktree(node, "feat"), "work.txt", "merged\n")
        command = "merge feat"
    elif operation == "multi":
        wm(mux_server, workmux_exe_path, repo, "add other --remote lo")
        wm(mux_server, workmux_exe_path, repo, "add local --background")
        command = "remove feat other local"
    elif operation == "offline":
        config = mux_server.home_path / ".config/workmux/config.yaml"
        config.write_text("nerdfont: false\n")
        command = "remove --force feat"

    started = time.monotonic()
    stdout, stderr, _ = run_inside(mux_server, workmux_exe_path, repo, command)
    assert poll_until(
        lambda: get_window_name("feat") not in mux_server.list_windows(), timeout=30
    ), stderr.read_text()
    record_property("cleanup_seconds", time.monotonic() - started)
    assert local_records(mux_server) == []
    if operation == "offline":
        assert "forgot remote workspace 'feat'" in stderr.read_text()
        assert len(node.workspaces()) == 1
    else:
        assert node.workspaces() == []
        if operation == "merge":
            assert "Successfully merged 'feat'" in stdout.read_text()
            assert git(repo, "show", "main:work.txt") == "merged"
        else:
            assert "Removed remote workspace 'feat'" in stdout.read_text()
        if operation == "multi":
            assert "Removed remote workspace 'other'" in stdout.read_text()
            assert "Removed worktree 'local'" in stdout.read_text()
            assert get_window_name("other") not in mux_server.list_windows()


@pytest.mark.parametrize("operation", ["remove", "merge"])
def test_failed_cleanup_inside_window_is_retryable(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    operation: str,
):
    repo = mux_repo_path
    block = node.root / "block-remove"
    configure(
        repo,
        pre_remove=[f'case "$(pwd -P)" in {node.root}/*) test ! -e {block} ;; esac'],
    )
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    if operation == "merge":
        commit_on_node(node_worktree(node, "feat"), "work.txt", "merged\n")
    block.touch()
    _, stderr, exited = run_inside(
        mux_server, workmux_exe_path, repo, f"{operation} feat"
    )
    assert poll_until(lambda: exited.exists() and bool(exited.read_text()), timeout=30)
    assert exited.read_text().strip() != "0", stderr.read_text()
    assert get_window_name("feat") in mux_server.list_windows()
    assert len(local_records(mux_server)) == 1
    assert len(node.workspaces()) == 1
    before = git(repo, "rev-parse", "main")
    block.unlink()
    stdout, stderr, _ = run_inside(
        mux_server, workmux_exe_path, repo, f"{operation} feat"
    )
    assert poll_until(
        lambda: get_window_name("feat") not in mux_server.list_windows(), timeout=30
    ), stderr.read_text()
    assert local_records(mux_server) == []
    assert node.workspaces() == []
    assert git(repo, "rev-parse", "main") == before
    assert "remote workspace" in stdout.read_text()


def test_multi_remove_reports_failure_before_closing_invoking_window(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    configure(repo)
    for handle in ("feat", "dirty", "last"):
        wm(mux_server, workmux_exe_path, repo, f"add {handle} --remote lo")
    (node_worktree(node, "dirty") / "untracked.txt").write_text("keep me\n")
    stdout, stderr, _ = run_inside(
        mux_server, workmux_exe_path, repo, "remove feat dirty last"
    )
    assert poll_until(
        lambda: get_window_name("feat") not in mux_server.list_windows(), timeout=30
    ), stderr.read_text()
    assert [record["handle"] for record in local_records(mux_server)] == ["dirty"]
    assert [workspace["handle"] for workspace in node.workspaces()] == ["dirty"]
    assert get_window_name("dirty") in mux_server.list_windows()
    assert get_window_name("last") not in mux_server.list_windows()
    assert "Removed remote workspace 'last'" in stdout.read_text()
    assert "1 remote workspace(s) could not be removed" in stderr.read_text()


def test_multi_remove_pending_merges_keeps_remote_target_identity(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    block = node.root / "block-remove"
    configure(
        repo,
        pre_remove=[f'case "$(pwd -P)" in {node.root}/*) test ! -e {block} ;; esac'],
    )
    block.touch()
    for handle in ("feat", "other"):
        wm(mux_server, workmux_exe_path, repo, f"add {handle} --remote lo")
        commit_on_node(node_worktree(node, handle), f"{handle}.txt", "merged\n")
        # A failed merge stops the agent; keep a local shell in each window.
        mux_server.tmux(
            ["split-window", "-d", "-t", get_window_name(handle), "/bin/sh"]
        )
        wm(mux_server, workmux_exe_path, repo, f"merge {handle}", expect_fail=True)
    block.unlink()
    stdout, stderr, _ = run_inside(
        mux_server, workmux_exe_path, repo, "remove feat other"
    )
    assert poll_until(
        lambda: get_window_name("feat") not in mux_server.list_windows(), timeout=30
    ), stderr.read_text()
    assert local_records(mux_server) == []
    assert node.workspaces() == []
    assert get_window_name("other") not in mux_server.list_windows()
    assert "could not be removed" not in stderr.read_text()
    assert "Successfully merged 'other'" in stdout.read_text()


@pytest.mark.parametrize("pending_count", [3, 4])
def test_batch_pending_cleanup_attempts_each_hook_once(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    pending_count: int,
):
    repo = mux_repo_path
    attempts = node.root / "attempts"
    block = node.root / "block"
    configure(
        repo,
        pre_remove=[
            f'case "$(pwd -P)" in {node.root}/*) '
            f"echo attempt >> {attempts}; sleep 0.1; test ! -e {block} ;; esac"
        ],
    )
    block.touch()
    handles = ("one", "two", "three", "unrelated")[:pending_count]
    for handle in handles:
        wm(mux_server, workmux_exe_path, repo, f"add {handle} --remote lo")
        commit_on_node(node_worktree(node, handle), f"{handle}.txt", "merged\n")
        wm(mux_server, workmux_exe_path, repo, f"merge {handle}", expect_fail=True)
    attempts.write_text("")
    started = time.monotonic()
    result = wm(
        mux_server, workmux_exe_path, repo, "remove one two three", expect_fail=True
    )
    count = len(attempts.read_text().splitlines())
    print(f"batch cleanup: {count} hooks, {time.monotonic() - started:.3f}s")
    assert "3 remote workspace(s) could not be removed" in result.stderr
    assert len(local_records(mux_server)) == pending_count
    assert count == pending_count
    block.unlink()
    wm(mux_server, workmux_exe_path, repo, "remove " + " ".join(handles))
    assert local_records(mux_server) == []
    assert node.workspaces() == []
