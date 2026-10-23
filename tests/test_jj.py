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


def test_add_with_undescribed_changes_in_main(mux_server, jj_repo, wm):
    (jj_repo / "wip").write_text("wip\n")
    jj(mux_server, jj_repo, "status")
    wm("add feature --background")
    workspace = get_worktree_path(jj_repo, "feature")
    assert revision(mux_server, workspace, "@-") == revision(
        mux_server, jj_repo, "main"
    )
    assert (jj_repo / "wip").read_text() == "wip\n"


def test_merge_includes_commits_above_edited_commit(mux_server, jj_repo, wm):
    wm("add feature --background")
    workspace = get_worktree_path(jj_repo, "feature")
    commit(mux_server, workspace, "first")
    commit(mux_server, workspace, "second")
    jj(mux_server, workspace, "edit", "@--")
    (workspace / "first").write_text("edited\n")
    wm("merge feature")
    assert jj(mux_server, jj_repo, "file", "show", "-r", "main", "first") == "edited"
    assert jj(mux_server, jj_repo, "file", "show", "-r", "main", "second") == "second"
    assert_cleaned(mux_server, jj_repo, "feature")


def test_merge_rejects_several_stacks(mux_server, jj_repo, wm):
    wm("add feature --background")
    workspace = get_worktree_path(jj_repo, "feature")
    commit(mux_server, workspace, "shared")
    shared = revision(mux_server, workspace, "@-")
    for name in ("one", "two"):
        jj(mux_server, workspace, "new", shared, "-m", name)
        (workspace / name).write_text(name + "\n")
    jj(mux_server, workspace, "edit", shared)
    target = revision(mux_server, jj_repo, "main")
    result = wm("merge feature", expect_fail=True)
    assert "several stacks" in result.stderr
    assert revision(mux_server, jj_repo, "main") == target


def test_merge_rejects_conflicted_commits(mux_server, jj_repo, wm):
    wm("add feature --background")
    workspace = get_worktree_path(jj_repo, "feature")
    (workspace / "conflict").write_text("feature\n")
    jj(mux_server, workspace, "commit", "-m", "feature change")
    (jj_repo / "conflict").write_text("target\n")
    jj(mux_server, jj_repo, "commit", "-m", "target change")
    jj(mux_server, jj_repo, "bookmark", "set", "main", "-r", "@-")
    jj(mux_server, workspace, "rebase", "-b", "@", "-d", "main")
    target = revision(mux_server, jj_repo, "main")
    result = wm("merge feature", expect_fail=True)
    assert "unresolved conflicts" in result.stderr
    assert revision(mux_server, jj_repo, "main") == target


def test_merge_rejects_conflicted_target_bookmark(mux_server, jj_repo, wm):
    wm("add feature --background")
    commit(mux_server, get_worktree_path(jj_repo, "feature"), "feature-change")
    commit(mux_server, jj_repo, "one")
    one = revision(mux_server, jj_repo, "@-")
    jj(mux_server, jj_repo, "new", "main")
    commit(mux_server, jj_repo, "two")
    two = revision(mux_server, jj_repo, "@-")
    operation = jj(mux_server, jj_repo, "op", "log", "-n1", "--no-graph", "-T", "id")
    for rev in (one, two):
        jj(
            mux_server,
            jj_repo,
            "--at-op",
            operation,
            "bookmark",
            "set",
            "main",
            "-r",
            rev,
        )
    result = wm("merge feature", expect_fail=True)
    assert "Bookmark 'main' is conflicted" in result.stderr
    assert get_worktree_path(jj_repo, "feature").exists()


def test_merge_into_workspace_bookmark(mux_server, jj_repo, wm):
    wm("add parent --background")
    parent = get_worktree_path(jj_repo, "parent")
    commit(mux_server, parent, "parent-one")
    wm("add child --background --base parent")
    child = get_worktree_path(jj_repo, "child")
    commit(mux_server, child, "child-change")
    # The parent bookmark lags behind work committed after the child started.
    commit(mux_server, parent, "parent-two")
    wm("merge child --into parent")
    for name in ("parent-one", "parent-two", "child-change"):
        assert jj(mux_server, jj_repo, "file", "show", "-r", "parent", name) == name
    assert revision(mux_server, parent, "@-") == revision(mux_server, jj_repo, "parent")
    assert_cleaned(mux_server, jj_repo, "child")
    wm("merge parent")
    for name in ("parent-one", "parent-two", "child-change"):
        assert jj(mux_server, jj_repo, "file", "show", "-r", "main", name) == name


def make_stale(env: MuxEnvironment, repo: Path, workspace: Path):
    commit(env, workspace, "feature-change")
    commit(env, repo, "target-change")
    jj(env, repo, "bookmark", "set", "main", "-r", "@-")
    jj(env, repo, "rebase", "-b", "feature@", "-d", "main")


def test_merge_updates_stale_workspace(mux_server, jj_repo, wm):
    wm("add feature --background")
    workspace = get_worktree_path(jj_repo, "feature")
    make_stale(mux_server, jj_repo, workspace)
    wm("merge feature")
    for name in ("feature-change", "target-change"):
        assert jj(mux_server, jj_repo, "file", "show", "-r", "main", name) == name
    assert_cleaned(mux_server, jj_repo, "feature")


def test_merge_rejects_divergent_workspace(mux_server, jj_repo, wm):
    wm("add feature --background")
    workspace = get_worktree_path(jj_repo, "feature")
    (workspace / "snapshotted").write_text("snapshotted\n")
    make_stale(mux_server, jj_repo, workspace)
    # Edits made while stale make updating the working copy divergent.
    (workspace / "late").write_text("late\n")
    target = revision(mux_server, jj_repo, "main")
    result = wm("merge feature", expect_fail=True)
    assert "divergent" in result.stderr
    assert revision(mux_server, jj_repo, "main") == target
    # Updating the stale working copy keeps those edits in a divergent commit.
    assert jj(
        mux_server, workspace, "log", "--no-graph", "-r", "divergent() & files(late)"
    )


def test_symlinked_ignored_directory_is_not_snapshotted(mux_server, jj_repo, wm):
    write_workmux_config(
        jj_repo, panes=[{"command": "sleep 600"}], files={"symlink": ["node_modules"]}
    )
    (jj_repo / ".gitignore").write_text("node_modules/\n")
    jj(mux_server, jj_repo, "commit", "-m", "ignore")
    jj(mux_server, jj_repo, "bookmark", "set", "main", "-r", "@-")
    (jj_repo / "node_modules").mkdir()
    (jj_repo / "node_modules" / "module.js").write_text("module\n")
    wm("add feature --background --base main")
    workspace = get_worktree_path(jj_repo, "feature")
    assert (workspace / "node_modules").is_symlink()
    assert "node_modules" not in jj(mux_server, workspace, "file", "list", "-r", "@")


def test_prompt_file_is_not_snapshotted(mux_server, jj_repo, wm):
    write_workmux_config(jj_repo, panes=[{"command": "<agent>"}], agent="cat")
    wm("add feature --background --base main -p 'inline prompt'")
    workspace = get_worktree_path(jj_repo, "feature")
    assert list((workspace / ".workmux").iterdir())
    assert ".workmux/" not in jj(mux_server, workspace, "file", "list", "-r", "@")


def test_remove_after_workspace_directory_deleted(mux_server, jj_repo, wm):
    wm("add feature --background")
    shutil.rmtree(get_worktree_path(jj_repo, "feature"))
    wm("remove feature --force")
    assert_cleaned(mux_server, jj_repo, "feature")


def test_remote_branch_is_rejected_without_fetching(mux_server, jj_repo, wm):
    jj(mux_server, jj_repo, "git", "remote", "add", "origin", "/nonexistent")
    result = wm("add origin/feature --background", expect_fail=True)
    assert "not supported in jj" in result.stderr
    assert "Failed to fetch" not in result.stderr
