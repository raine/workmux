"""Helpers for setting up jj-backed test repositories.

Mirrors the shape of the git helpers this module sits alongside
(`tests/support/worktrees.py`) and `conftest.py`'s `setup_git_repo`/
`skip_if_backend_unavailable`, but for jj fixtures instead of git ones.
"""

import shutil
import subprocess
from pathlib import Path
from typing import Optional

import pytest

from ..conftest import MuxEnvironment


def skip_if_jj_unavailable() -> None:
    """Skip the current test if the `jj` binary is not available.

    Mirrors `skip_if_backend_unavailable` in `conftest.py` (checked for
    tmux/wezterm): verifies the binary exists on PATH *and* that invoking it
    actually succeeds, so CI environments without jj installed skip these
    tests gracefully instead of failing with a confusing subprocess error.
    """
    if not shutil.which("jj"):
        pytest.skip("jj not installed")
    result = subprocess.run(
        ["jj", "--version"], capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        pytest.skip("jj not available")


def setup_jj_repo(
    path: Path, colocate: bool, env_vars: Optional[dict] = None
) -> None:
    """Initializes a jj repository in `path` with an initial commit and a
    `main` bookmark, mirroring `setup_git_repo`'s shape for git repos.

    Args:
        path: Directory to initialize the repo in (must already exist).
        colocate: `True` mirrors `jj git init --colocate` (both `.jj` and
            `.git` visible at the workspace root - plain `git` commands
            also work there). `False` mirrors `jj git init --no-colocate`
            (a "jj-only" repo: the git backend lives inside
            `.jj/repo/store/git`, no `.git` at the workspace root).
        env_vars: Optional environment for the `jj` subprocess calls,
            matching `setup_git_repo`'s `env_vars` parameter. When omitted,
            the calling process's environment is inherited, which is only
            appropriate outside an isolated `MuxEnvironment`, exactly like
            `setup_git_repo`.
    """
    subprocess.run(
        ["jj", "git", "init", "--colocate" if colocate else "--no-colocate"],
        cwd=path,
        check=True,
        capture_output=True,
        env=env_vars,
    )
    (path / "README.md").write_text("test\n")
    subprocess.run(
        ["jj", "describe", "-m", "Initial commit"],
        cwd=path,
        check=True,
        capture_output=True,
        env=env_vars,
    )
    subprocess.run(
        ["jj", "bookmark", "create", "main", "-r", "@"],
        cwd=path,
        check=True,
        capture_output=True,
        env=env_vars,
    )
    # Move `@` off `main`, mirroring `seed()` in `src/vcs/jj_backend.rs`'s
    # own Rust test module: leaves the bookmark pointing at a real commit
    # rather than at the empty working-copy commit `workmux add --base main`
    # will build new workspaces on top of.
    subprocess.run(
        ["jj", "new"],
        cwd=path,
        check=True,
        capture_output=True,
        env=env_vars,
    )


def jj_workspace_list(env: MuxEnvironment, repo_path: Path) -> str:
    """Return the raw `jj workspace list` output for `repo_path`."""
    result = env.run_command(["jj", "workspace", "list"], cwd=repo_path)
    return result.stdout


def jj_bookmark_list(env: MuxEnvironment, repo_path: Path) -> str:
    """Return the raw `jj bookmark list` output for `repo_path`."""
    result = env.run_command(["jj", "bookmark", "list"], cwd=repo_path)
    return result.stdout


def assert_jj_workspace_exists(
    env: MuxEnvironment, repo_path: Path, handle: str
) -> None:
    workspaces = jj_workspace_list(env, repo_path)
    assert handle in workspaces, (
        f"jj workspace {handle!r} should exist. Workspaces:\n{workspaces}"
    )


def assert_jj_workspace_removed(
    env: MuxEnvironment, repo_path: Path, handle: str
) -> None:
    workspaces = jj_workspace_list(env, repo_path)
    assert handle not in workspaces, (
        f"jj workspace {handle!r} should have been forgotten. "
        f"Workspaces:\n{workspaces}"
    )


def assert_jj_bookmark_exists(
    env: MuxEnvironment, repo_path: Path, bookmark: str
) -> None:
    bookmarks = jj_bookmark_list(env, repo_path)
    assert bookmark in bookmarks, (
        f"jj bookmark {bookmark!r} should exist. Bookmarks:\n{bookmarks}"
    )


def assert_jj_bookmark_removed(
    env: MuxEnvironment, repo_path: Path, bookmark: str
) -> None:
    bookmarks = jj_bookmark_list(env, repo_path)
    assert bookmark not in bookmarks, (
        f"jj bookmark {bookmark!r} should have been deleted. "
        f"Bookmarks:\n{bookmarks}"
    )
