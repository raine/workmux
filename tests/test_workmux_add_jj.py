"""Tests for `workmux add`/`workmux remove` against jj-backed repositories.

Covers the plan's "bookmark-per-workspace by default" ruling for `add`, and
that `remove` cleans up both the jj workspace registration/bookmark and
workmux's own metadata file - for both a colocated jj+git repo and a
jj-only repo (no `.git` at the workspace root).

`--session` mode is used rather than the default window mode: creating a
window-mode worktree under tmux requires window-ownership token tracking,
which `workflow::create` explicitly does not support yet for jj repos (see
`src/workflow/create.rs`'s window-token guard) - session mode has no such
restriction. Marked `tmux_only` because session mode itself is tmux-only
(WezTerm has no session concept), independent of the jj-specific
window-token restriction.
"""

from pathlib import Path

import pytest

from .conftest import (
    MuxEnvironment,
    assert_session_exists,
    assert_session_not_exists,
    get_session_name,
    run_workmux_command,
    write_workmux_config,
)
from .support.jj_repo import (
    assert_jj_bookmark_exists,
    assert_jj_bookmark_removed,
    assert_jj_workspace_exists,
    assert_jj_workspace_removed,
    setup_jj_repo,
    skip_if_jj_unavailable,
)


@pytest.fixture
def jj_colocated_repo_path(mux_server: MuxEnvironment) -> Path:
    """A colocated jj+git repo (`jj git init --colocate`) inside the
    isolated multiplexer environment's tmp_path, with an initial commit and
    a `main` bookmark.
    """
    skip_if_jj_unavailable()
    path = mux_server.tmp_path / "jj_colocated_repo"
    path.mkdir()
    setup_jj_repo(path, colocate=True, env_vars=mux_server.env)
    write_workmux_config(path, base_branch="main")
    return path


@pytest.fixture
def jj_only_repo_path(mux_server: MuxEnvironment) -> Path:
    """A jj-only repo (`jj git init --no-colocate`, no `.git` at the
    workspace root) inside the isolated multiplexer environment's
    tmp_path, with an initial commit and a `main` bookmark.
    """
    skip_if_jj_unavailable()
    path = mux_server.tmp_path / "jj_only_repo"
    path.mkdir()
    setup_jj_repo(path, colocate=False, env_vars=mux_server.env)
    write_workmux_config(path, base_branch="main")
    return path


@pytest.mark.tmux_only
class TestAddCreatesWorkspaceAndBookmark:
    """`workmux add` creates a jj workspace with a bookmark checked out."""

    def test_colocated(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path,
        jj_colocated_repo_path,
    ):
        env = mux_server
        branch_name = "jj-feature-colocated"
        session_name = get_session_name(branch_name)

        run_workmux_command(
            env,
            workmux_exe_path,
            jj_colocated_repo_path,
            f"add {branch_name} --session --background",
        )

        assert_session_exists(env, session_name)
        assert_jj_workspace_exists(env, jj_colocated_repo_path, branch_name)
        assert_jj_bookmark_exists(env, jj_colocated_repo_path, branch_name)

        worktree_path = (
            jj_colocated_repo_path.parent
            / f"{jj_colocated_repo_path.name}__worktrees"
            / branch_name
        )
        assert worktree_path.is_dir(), f"Worktree {worktree_path} should exist"

    def test_jj_only(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path,
        jj_only_repo_path,
    ):
        env = mux_server
        branch_name = "jj-feature-only"
        session_name = get_session_name(branch_name)

        run_workmux_command(
            env,
            workmux_exe_path,
            jj_only_repo_path,
            f"add {branch_name} --session --background",
        )

        assert_session_exists(env, session_name)
        assert_jj_workspace_exists(env, jj_only_repo_path, branch_name)
        assert_jj_bookmark_exists(env, jj_only_repo_path, branch_name)

        worktree_path = (
            jj_only_repo_path.parent
            / f"{jj_only_repo_path.name}__worktrees"
            / branch_name
        )
        assert worktree_path.is_dir(), f"Worktree {worktree_path} should exist"


@pytest.mark.tmux_only
class TestRemoveCleansUpWorkspaceAndMetadata:
    """`workmux remove` forgets the jj workspace, deletes the bookmark, and
    removes workmux's own per-handle metadata (from `workmux/metadata.toml`
    at the repo root, jj's `WorkmuxMetaStore` equivalent of git's
    `workmux.worktree.<handle>.*` config keys).
    """

    def test_colocated(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path,
        jj_colocated_repo_path,
    ):
        env = mux_server
        branch_name = "jj-remove-colocated"
        session_name = get_session_name(branch_name)

        run_workmux_command(
            env,
            workmux_exe_path,
            jj_colocated_repo_path,
            f"add {branch_name} --session --background",
        )
        assert_session_exists(env, session_name)
        assert_jj_workspace_exists(env, jj_colocated_repo_path, branch_name)

        worktree_path = (
            jj_colocated_repo_path.parent
            / f"{jj_colocated_repo_path.name}__worktrees"
            / branch_name
        )
        assert worktree_path.is_dir()

        run_workmux_command(
            env,
            workmux_exe_path,
            jj_colocated_repo_path,
            f"remove -f {branch_name}",
        )

        assert not worktree_path.exists(), (
            f"Worktree {worktree_path} should be removed"
        )
        assert_session_not_exists(env, session_name)
        assert_jj_workspace_removed(env, jj_colocated_repo_path, branch_name)
        assert_jj_bookmark_removed(env, jj_colocated_repo_path, branch_name)

        metadata_path = jj_colocated_repo_path / "workmux" / "metadata.toml"
        assert metadata_path.exists(), "workmux metadata file should still exist"
        metadata = metadata_path.read_text()
        assert f"[worktree.{branch_name}]" not in metadata, (
            f"Metadata for handle {branch_name!r} should have been removed:\n"
            f"{metadata}"
        )

    def test_jj_only(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path,
        jj_only_repo_path,
    ):
        env = mux_server
        branch_name = "jj-remove-only"
        session_name = get_session_name(branch_name)

        run_workmux_command(
            env,
            workmux_exe_path,
            jj_only_repo_path,
            f"add {branch_name} --session --background",
        )
        assert_session_exists(env, session_name)
        assert_jj_workspace_exists(env, jj_only_repo_path, branch_name)

        worktree_path = (
            jj_only_repo_path.parent
            / f"{jj_only_repo_path.name}__worktrees"
            / branch_name
        )
        assert worktree_path.is_dir()

        run_workmux_command(
            env,
            workmux_exe_path,
            jj_only_repo_path,
            f"remove -f {branch_name}",
        )

        assert not worktree_path.exists(), (
            f"Worktree {worktree_path} should be removed"
        )
        assert_session_not_exists(env, session_name)
        assert_jj_workspace_removed(env, jj_only_repo_path, branch_name)
        assert_jj_bookmark_removed(env, jj_only_repo_path, branch_name)

        metadata_path = jj_only_repo_path / "workmux" / "metadata.toml"
        assert metadata_path.exists(), "workmux metadata file should still exist"
        metadata = metadata_path.read_text()
        assert f"[worktree.{branch_name}]" not in metadata, (
            f"Metadata for handle {branch_name!r} should have been removed:\n"
            f"{metadata}"
        )
