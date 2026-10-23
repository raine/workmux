"""Colocated Jujutsu workflows, using real jj and isolated multiplexers."""

import re
import shutil
import subprocess
from pathlib import Path

import pytest

from .conftest import (
    MuxEnvironment,
    get_worktree_path,
    run_workmux_command,
    setup_git_repo,
    write_workmux_config,
)


def require_jj():
    executable = shutil.which("jj")
    if executable is None:
        pytest.skip("jj >= 0.46 required", allow_module_level=True)
    result = subprocess.run(
        [executable, "--version"], capture_output=True, text=True, check=True
    )
    version = re.search(r"jj (\d+)\.(\d+)\.(\d+)", result.stdout)
    if version is None or tuple(map(int, version.groups())) < (0, 46, 0):
        pytest.skip("jj >= 0.46 required", allow_module_level=True)


require_jj()


@pytest.fixture
def jj_repo(mux_server: MuxEnvironment):
    # Pass these to commands in the runner pane too: changing the server's
    # environment does not change an already-running shell's environment.
    mux_server.env.update(
        JJ_CONFIG="/dev/null",
        JJ_USER="Test User",
        JJ_EMAIL="test@example.com",
        JJ_EDITOR="true",
    )
    path = mux_server.tmp_path / "repo"
    path.mkdir()
    setup_git_repo(path, mux_server.env)
    write_workmux_config(path, panes=[{"command": "sleep 600"}], env=mux_server)
    mux_server.run_command(["jj", "git", "init", "--colocate"], cwd=path)
    return path


@pytest.fixture
def wm(mux_server: MuxEnvironment, workmux_exe_path: Path, jj_repo: Path):
    def run(command: str, *, expect_fail: bool = False, cwd: Path | None = None):
        return run_workmux_command(
            mux_server,
            workmux_exe_path,
            jj_repo,
            command,
            expect_fail=expect_fail,
            working_dir=cwd,
            pre_run_env={
                key: value
                for key, value in mux_server.env.items()
                if key.startswith("JJ_") or key.startswith("XDG_")
            },
        )

    return run


def jj(env: MuxEnvironment, path: Path, *args: str) -> str:
    return env.run_command(["jj", *args], cwd=path).stdout.strip()


def revision(env: MuxEnvironment, path: Path, rev: str) -> str:
    return jj(env, path, "log", "--no-graph", "-r", rev, "-T", "commit_id")


def commit(env: MuxEnvironment, path: Path, name: str, method: str = "jj"):
    (path / name).write_text(name + "\n")
    if method == "jj":
        jj(env, path, "commit", "-m", name)
    else:
        env.run_command(["git", "add", name], cwd=path)
        env.run_command(["git", "commit", "-m", name], cwd=path)


def assert_cleaned(env: MuxEnvironment, repo: Path, name: str):
    path = get_worktree_path(repo, name)
    assert not path.exists()
    assert (
        str(path)
        not in env.run_command(
            ["git", "worktree", "list", "--porcelain"], cwd=repo
        ).stdout
    )
    assert not jj(env, repo, "bookmark", "list", name)
    assert (
        name
        not in jj(env, repo, "workspace", "list", "-T", 'name ++ "\\n"').splitlines()
    )
    metadata = env.run_command(
        ["git", "config", "--get-regexp", rf"^workmux\.worktree\.{name}\."],
        cwd=repo,
        check=False,
    )
    assert metadata.returncode == 1
    assert not metadata.stdout


@pytest.mark.parametrize("explicit_base", [False, True])
def test_add_and_list(mux_server, jj_repo, wm, explicit_base):
    if explicit_base:
        commit(mux_server, jj_repo, "alternate-base")
        jj(mux_server, jj_repo, "bookmark", "create", "alternate", "-r", "@-")
        jj(mux_server, jj_repo, "new", "main")
    base = revision(mux_server, jj_repo, "alternate" if explicit_base else "main")
    wm("add feature --background" + (" --base alternate" if explicit_base else ""))
    workspace = get_worktree_path(jj_repo, "feature")
    assert (workspace / ".jj").is_dir()
    assert (workspace / ".git").is_file()
    assert revision(mux_server, workspace, "@-") == base
    assert revision(mux_server, jj_repo, "feature") == base
    listing = wm("list").stdout
    feature_rows = [
        line for line in listing.splitlines() if "repo__worktrees/feature" in line
    ]
    assert len(feature_rows) == 1
    assert feature_rows[0].split()[0] == "feature"
    assert "(detached)" not in feature_rows[0]


def test_add_requires_unambiguous_default_base(mux_server, jj_repo, wm):
    jj(mux_server, jj_repo, "bookmark", "create", "alternative", "-r", "main")
    result = wm("add feature --background", expect_fail=True)
    assert "--base" in result.stderr
    assert "main" in result.stderr
    assert "alternative" in result.stderr
    assert not get_worktree_path(jj_repo, "feature").exists()
    wm("add feature --background --base alternative")


def test_add_without_bookmark_at_current_revision(mux_server, jj_repo, wm):
    commit(mux_server, jj_repo, "unbookmarked")
    result = wm("add feature --background", expect_fail=True)
    assert "--base" in result.stderr
    assert "main" in result.stderr
    wm("add feature --background --base main")


@pytest.mark.parametrize("method", ["jj", "git"])
@pytest.mark.parametrize(
    "strategy",
    [
        "",
        "--rebase",
        "--squash",
        "--keep",
    ],
)
def test_merge(mux_server, jj_repo, wm, method, strategy):
    wm("add feature --background")
    workspace = get_worktree_path(jj_repo, "feature")
    commit(mux_server, workspace, "feature-one", method)
    commit(mux_server, workspace, "feature-two", method)
    # Diverge the target so default merge and rebase exercise real integration,
    # not just the fast-forward shortcut.
    commit(mux_server, jj_repo, "target-only")
    jj(mux_server, jj_repo, "bookmark", "set", "main", "-r", "@-")
    old_target = revision(mux_server, jj_repo, "main")
    wm(f"merge feature {strategy}")
    for name in ("feature-one", "feature-two", "target-only"):
        assert jj(mux_server, jj_repo, "file", "show", "-r", "main", name) == name
    assert revision(mux_server, jj_repo, "main") != old_target
    parents = jj(
        mux_server, jj_repo, "log", "--no-graph", "-r", "main", "-T", "parents.len()"
    )
    assert parents == ("2" if strategy in ("", "--keep") else "1")
    count = jj(
        mux_server,
        jj_repo,
        "log",
        "--no-graph",
        "-r",
        f"{old_target}..main",
        "-T",
        'commit_id ++ "\\n"',
    )
    assert len(count.splitlines()) == (
        1 if strategy == "--squash" else 2 if strategy == "--rebase" else 3
    )
    if strategy == "--keep":
        assert workspace.exists()
        assert jj(mux_server, jj_repo, "bookmark", "list", "feature")
        assert "feature" in jj(mux_server, jj_repo, "workspace", "list")
    else:
        assert_cleaned(mux_server, jj_repo, "feature")


@pytest.mark.parametrize("method", ["jj", "git"])
def test_merge_fast_forwards(mux_server, jj_repo, wm, method):
    wm("add feature --background")
    workspace = get_worktree_path(jj_repo, "feature")
    commit(mux_server, workspace, "feature-change", method)
    tip = revision(mux_server, workspace, "@-")
    wm("merge feature")
    assert revision(mux_server, jj_repo, "main") == tip
    assert (jj_repo / "feature-change").read_text() == "feature-change\n"
    assert_cleaned(mux_server, jj_repo, "feature")


def test_merge_rejects_uncommitted_changes(mux_server, jj_repo, wm):
    wm("add feature --background")
    workspace = get_worktree_path(jj_repo, "feature")
    (workspace / "dirty").write_text("uncommitted\n")
    target = revision(mux_server, jj_repo, "main")
    result = wm("merge feature", expect_fail=True)
    assert "uncommitted changes" in result.stderr
    assert revision(mux_server, jj_repo, "main") == target
    assert (workspace / "dirty").read_text() == "uncommitted\n"


@pytest.mark.parametrize("strategy", ["", "--rebase", "--squash"])
def test_conflicting_merge_preserves_target(mux_server, jj_repo, wm, strategy):
    wm("add feature --background")
    workspace = get_worktree_path(jj_repo, "feature")
    (workspace / "conflict").write_text("feature\n")
    jj(mux_server, workspace, "commit", "-m", "feature change")
    (jj_repo / "conflict").write_text("target\n")
    jj(mux_server, jj_repo, "commit", "-m", "target change")
    jj(mux_server, jj_repo, "bookmark", "set", "main", "-r", "@-")
    target = revision(mux_server, jj_repo, "main")
    result = wm(f"merge feature {strategy}", expect_fail=True)
    assert "conflict" in result.stderr.lower()
    assert revision(mux_server, jj_repo, "main") == target
    assert jj(mux_server, jj_repo, "file", "show", "-r", "main", "conflict") == "target"
    assert workspace.exists()


def test_remove_cleans_all_state(mux_server, jj_repo, wm):
    wm("add feature --background")
    commit(mux_server, get_worktree_path(jj_repo, "feature"), "unmerged")
    wm("remove feature --force")
    assert_cleaned(mux_server, jj_repo, "feature")


def test_rename_is_rejected(jj_repo, wm):
    wm("add feature --background")
    result = wm("rename feature renamed", expect_fail=True)
    assert "jj" in result.stderr
    assert get_worktree_path(jj_repo, "feature").exists()
    assert not get_worktree_path(jj_repo, "renamed").exists()


def test_non_colocated_repo_is_rejected(mux_server, jj_repo, wm):
    path = jj_repo.parent / "non-colocated"
    jj(mux_server, jj_repo, "git", "init", "--no-colocate", str(path))
    result = wm("add feature --background", cwd=path, expect_fail=True)
    assert "colocat" in result.stderr.lower()
    assert not get_worktree_path(path, "feature").exists()
