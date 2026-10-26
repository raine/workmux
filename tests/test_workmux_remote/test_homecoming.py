"""Bringing remote work home over the loopback transport: `merge` of a
remote workspace, its conflict and pending-cleanup paths, `remote pull`, and
`remove` checks that match a local worktree's."""

import shutil
from pathlib import Path
from typing import Optional

import pytest
import yaml

from ..conftest import (
    TmuxEnvironment,
    assert_window_exists,
    get_window_name,
    get_worktree_path,
)
from .conftest import Node, git, local_records, wm, write_script

pytestmark = pytest.mark.tmux_only


def configure(repo: Path, **extra) -> None:
    project = {"nerdfont": False, "panes": [{"command": "echo AGENT-READY"}], **extra}
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "configure remote panes")


def node_worktree(node: Node, handle: str) -> Path:
    [workspace] = [w for w in node.workspaces() if w["handle"] == handle]
    return Path(workspace["worktree_path"])


def node_git(worktree: Path, *args: str) -> str:
    return git(worktree, "-c", "user.name=Agent", "-c", "user.email=a@x", *args)


def commit_on_node(worktree: Path, name: str, content: str) -> None:
    (worktree / name).write_text(content)
    node_git(worktree, "add", name)
    node_git(worktree, "commit", "-m", f"node: {name}")


def editor(tmp: Path) -> dict:
    script = write_script(tmp / "editor.sh", 'echo "commit staged work" > "$1"\n')
    return {"GIT_EDITOR": str(script)}


def stage_of(env: TmuxEnvironment, handle: str) -> Optional[str]:
    [record] = [r for r in local_records(env) if r["handle"] == handle]
    return (record.get("homecoming") or {}).get("stage")


def test_merge_brings_remote_work_home(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    marker = node.root / "pre-merge-ran"
    configure(repo, pre_merge=[f"test -e staged.txt && touch {marker}"])
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    worktree = node_worktree(node, "feat")
    commit_on_node(worktree, "work.txt", "work\n")
    (worktree / "staged.txt").write_text("staged\n")
    node_git(worktree, "add", "staged.txt")
    (worktree / "scratch.txt").write_text("scratch\n")
    copy = get_worktree_path(repo, "feat")

    # Untracked work on the node refuses the merge like a local worktree's,
    # and leaves nothing behind.
    refused = wm(mux_server, workmux_exe_path, repo, "merge feat", expect_fail=True)
    assert "untracked files" in refused.stderr
    assert "workmux open feat" in refused.stderr
    assert not copy.exists()
    assert git(repo, "branch", "--list", "feat") == ""
    assert stage_of(mux_server, "feat") is None
    assert (worktree / "scratch.txt").exists()

    # A repository inside the worktree cannot travel, so nothing proceeds,
    # even with --ignore-uncommitted.
    (worktree / "scratch.txt").unlink()
    git(worktree, "init", "-q", "vendor/lib")
    nested = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "merge --ignore-uncommitted feat",
        expect_fail=True,
    )
    assert "'vendor/lib/'" in nested.stderr, nested.stderr
    assert "separate git repository" in nested.stderr
    assert not copy.exists()
    assert (worktree / "vendor/lib/.git").exists()

    # The staged change is committed with the laptop's editor, pre_merge
    # runs on the copy, and the remote workspace goes.
    shutil.rmtree(worktree / "vendor")
    merged = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "merge feat",
        extra_env=editor(mux_server.tmp_path),
    )
    assert "Committed staged changes" in merged.stdout, merged.stderr
    assert "Successfully merged 'feat'" in merged.stdout
    assert marker.exists()
    assert git(repo, "show", "main:work.txt") == "work"
    assert git(repo, "show", "main:staged.txt") == "staged"
    assert "commit staged work" in git(repo, "log", "--format=%s", "main")
    assert not copy.exists()
    assert git(repo, "branch", "--list", "feat") == ""
    assert git(repo, "for-each-ref", "refs/workmux") == ""
    assert node.workspaces() == []
    assert local_records(mux_server) == []


def test_rebase_conflict_is_resolved_on_the_laptop(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    configure(repo)
    (repo / "shared.txt").write_text("base\n")
    git(repo, "add", "shared.txt")
    git(repo, "commit", "-m", "shared base")
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    worktree = node_worktree(node, "feat")
    commit_on_node(worktree, "shared.txt", "remote\n")
    (repo / "shared.txt").write_text("local\n")
    git(repo, "commit", "-am", "local change")
    copy = get_worktree_path(repo, "feat")

    failed = wm(
        mux_server, workmux_exe_path, repo, "merge --rebase feat", expect_fail=True
    )
    assert "Rebase failed" in failed.stderr, failed.stderr
    assert "run `workmux merge feat` again" in failed.stderr
    assert stage_of(mux_server, "feat") == "conflicted"
    assert (copy / "shared.txt").exists()
    assert len(node.workspaces()) == 1

    # A retry before the rebase is finished says what is in the way.
    early = wm(
        mux_server, workmux_exe_path, repo, "merge --rebase feat", expect_fail=True
    )
    assert "a rebase is in progress" in early.stderr

    (copy / "shared.txt").write_text("resolved\n")
    git(copy, "add", "shared.txt")
    git(copy, "-c", "core.editor=true", "rebase", "--continue")

    merged = wm(mux_server, workmux_exe_path, repo, "merge --rebase feat")
    assert "Resuming the merge" in merged.stdout, merged.stderr
    assert "Successfully merged 'feat'" in merged.stdout
    assert git(repo, "show", "main:shared.txt") == "resolved"
    assert node.workspaces() == []
    assert local_records(mux_server) == []
    assert not copy.exists()


def test_keep_then_cleanup_pending_never_merges_twice(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    block = node.root / "block-remove"
    # Fails only on the node, and only while `block` exists.
    configure(
        repo,
        pre_remove=[f'case "$(pwd -P)" in {node.root}/*) test ! -e {block} ;; esac'],
    )
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    worktree = node_worktree(node, "feat")
    commit_on_node(worktree, "one.txt", "one\n")
    (worktree / "staged.txt").write_text("staged\n")
    node_git(worktree, "add", "staged.txt")

    # --keep merges and moves the remote branch to the commit the laptop
    # made, so the workspace continues from the merged state.
    kept = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "merge --keep feat",
        extra_env=editor(mux_server.tmp_path),
    )
    assert "Remote workspace, agent and branch kept" in kept.stdout, kept.stderr
    assert git(repo, "show", "main:staged.txt") == "staged"
    assert git(worktree, "log", "-1", "--format=%s") == "commit staged work"
    assert git(worktree, "status", "--porcelain") == ""
    assert stage_of(mux_server, "feat") is None
    assert_window_exists(mux_server, get_window_name("feat"))

    # The node cannot remove the workspace: the merge stands and the cleanup
    # waits.
    commit_on_node(worktree, "two.txt", "two\n")
    block.touch()
    pending = wm(mux_server, workmux_exe_path, repo, "merge feat", expect_fail=True)
    [message] = [line for line in pending.stderr.splitlines() if "pending" in line]
    assert "merged; remote cleanup pending on 'lo' (" in message, pending.stderr
    assert message.endswith(
        "`workmux merge feat`, or the next merge, remove, open, add, rebase or "
        "`remote pull` that reaches 'lo', finishes it"
    ), message
    assert "Caused by" not in pending.stderr
    assert stage_of(mux_server, "feat") == "merged"
    merges = git(repo, "rev-list", "--count", "main")
    assert git(repo, "show", "main:two.txt") == "two"
    listing = wm(mux_server, workmux_exe_path, repo, "list")
    assert "cleanup pending" in listing.stdout
    reopened = wm(mux_server, workmux_exe_path, repo, "open feat", expect_fail=True)
    assert "remote cleanup pending" in reopened.stderr, reopened.stderr
    rebased = wm(mux_server, workmux_exe_path, repo, "rebase feat", expect_fail=True)
    assert "`workmux merge feat` finishes removing" in rebased.stderr, rebased.stderr
    assert git(repo, "rev-list", "--count", "main") == merges

    # A retry only cleans up.
    again = wm(mux_server, workmux_exe_path, repo, "merge feat", expect_fail=True)
    assert "already merged" in again.stdout
    assert "remote cleanup pending" in again.stderr
    assert git(repo, "rev-list", "--count", "main") == merges

    # Any command that reaches the node finishes it.
    block.unlink()
    added = wm(mux_server, workmux_exe_path, repo, "add other --remote lo")
    assert "Finished removing remote workspace 'feat'" in added.stdout, added.stderr
    assert [w["handle"] for w in node.workspaces()] == ["other"]
    assert [r["handle"] for r in local_records(mux_server)] == ["other"]
    assert git(repo, "rev-list", "--count", "main") == merges


def test_existing_branch_merges_into_its_stored_base(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    configure(repo)
    git(repo, "branch", "develop")
    git(repo, "branch", "feat", "develop")
    git(repo, "config", "branch.feat.workmux-base", "develop")
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    commit_on_node(node_worktree(node, "feat"), "work.txt", "work\n")

    merged = wm(mux_server, workmux_exe_path, repo, "merge feat")
    assert "Merging 'feat' into 'develop'" in merged.stdout, merged.stderr
    assert git(repo, "show", "develop:work.txt") == "work"
    assert git(repo, "ls-tree", "--name-only", "main", "work.txt") == ""


def test_remote_pull_brings_the_workspace_home(
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
    (repo / "tracked.txt").write_text("base\n")
    git(repo, "add", "tracked.txt")
    git(repo, "commit", "-m", "tracked")
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    worktree = node_worktree(node, "feat")
    commit_on_node(worktree, "work.txt", "work\n")
    head = git(worktree, "rev-parse", "HEAD")
    (worktree / "staged.txt").write_text("staged\n")
    node_git(worktree, "add", "staged.txt")
    (worktree / "tracked.txt").write_text("edited\n")
    (worktree / "new.txt").write_text("new\n")

    # The node cannot remove the workspace yet: the work is home and the
    # cleanup waits for the next pull.
    block.touch()
    pulled = wm(
        mux_server, workmux_exe_path, repo, "remote pull feat", expect_fail=True
    )
    assert "Brought 'feat' home" in pulled.stdout, pulled.stderr
    assert "pulled; remote cleanup pending on 'lo' (" in pulled.stderr, pulled.stderr
    assert "`workmux remote pull feat`, or the next merge" in pulled.stderr
    assert stage_of(mux_server, "feat") == "pulled"
    block.unlink()
    again = wm(mux_server, workmux_exe_path, repo, "remote pull feat")
    assert "Removed the remote workspace" in again.stdout, again.stderr

    copy = get_worktree_path(repo, "feat")
    assert git(copy, "rev-parse", "HEAD") == head
    assert git(copy, "branch", "--show-current") == "feat"
    assert git(copy, "diff", "--cached", "--name-only") == "staged.txt"
    assert git(copy, "diff", "--name-only") == "tracked.txt"
    assert (copy / "new.txt").read_text() == "new\n"
    assert git(copy, "ls-files", "--others", "--exclude-standard") == "new.txt"

    window = get_window_name("feat")
    assert_window_exists(mux_server, window)
    commands = mux_server.tmux(
        ["list-panes", "-t", window, "-F", "#{pane_start_command}"]
    ).stdout
    assert "remote attach" not in commands
    assert node.workspaces() == []
    assert local_records(mux_server) == []
    assert git(repo, "for-each-ref", "refs/workmux") == ""


def test_remove_checks_like_a_local_worktree(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    configure(repo)
    with open(repo / ".git" / "info" / "exclude", "a") as exclude:
        exclude.write("laptop-ignored.txt\nlaptop-ignored/\n")
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    worktree = node_worktree(node, "feat")

    # Files the laptop ignores do not count as work, as locally.
    (worktree / "laptop-ignored.txt").write_text("x\n")
    commit_on_node(worktree, "work.txt", "work\n")
    (worktree / "edit.txt").write_text("edit\n")
    refused = wm(mux_server, workmux_exe_path, repo, "remove feat", expect_fail=True)
    assert "uncommitted changes" in refused.stderr
    assert "edit.txt" in refused.stderr
    assert "laptop-ignored.txt" not in refused.stderr
    assert len(node.workspaces()) == 1

    # A listing the node cut short may hide work the laptop does not ignore.
    (worktree / "edit.txt").unlink()
    ignored = worktree / "laptop-ignored"
    ignored.mkdir()
    for i in range(1001):
        (ignored / f"{i:04}").write_text("x\n")
    (worktree / "zz-work.txt").write_text("work\n")
    cut = wm(mux_server, workmux_exe_path, repo, "remove feat", expect_fail=True)
    assert "uncommitted changes" in cut.stderr, cut.stderr
    assert len(node.workspaces()) == 1
    shutil.rmtree(ignored)
    (worktree / "zz-work.txt").unlink()

    # Unmerged commits ask; declining keeps everything.
    declined = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "remove feat",
        stdin_input="n\n",
    )
    assert "commits not merged into their base" in declined.stdout
    assert "Aborted." in declined.stdout
    assert len(node.workspaces()) == 1

    # --keep-branch brings the branch home instead of asking.
    head = git(worktree, "rev-parse", "HEAD")
    kept = wm(mux_server, workmux_exe_path, repo, "remove --keep-branch feat")
    assert "Kept branch 'feat' locally" in kept.stdout, kept.stderr
    assert git(repo, "rev-parse", "feat") == head
    assert node.workspaces() == []
    # The branch is home, so the node does not keep a copy that would block
    # a later workspace on it.
    node_repo = next((node.root / "projects").glob("*/repo"))
    assert git(node_repo, "branch", "--list", "feat") == ""
    assert local_records(mux_server) == []


def test_rebase_runs_on_the_node(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    configure(repo)
    (repo / "shared.txt").write_text("base\n")
    git(repo, "add", "shared.txt")
    git(repo, "commit", "-m", "shared base")
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    worktree = node_worktree(node, "feat")
    commit_on_node(worktree, "work.txt", "work\n")
    (repo / "main.txt").write_text("main\n")
    git(repo, "add", "main.txt")
    git(repo, "commit", "-m", "main moves")
    main = git(repo, "rev-parse", "main")

    rebased = wm(mux_server, workmux_exe_path, repo, "rebase feat")
    assert "Rebased 'feat' onto 'main'" in rebased.stdout, rebased.stderr
    assert (worktree / "main.txt").exists()
    assert git(worktree, "merge-base", "--is-ancestor", main, "HEAD") == ""
    # Rebased as the laptop user, like a local rebase.
    assert git(worktree, "log", "-1", "--format=%ce") == "test@example.com"
    [record] = local_records(mux_server)
    assert record["base_sha"] == main

    # A conflict stops the rebase on the node, where it is resolved.
    commit_on_node(worktree, "shared.txt", "remote\n")
    (repo / "shared.txt").write_text("local\n")
    git(repo, "commit", "-am", "local change")
    failed = wm(mux_server, workmux_exe_path, repo, "rebase feat", expect_fail=True)
    assert "stopped on conflicts on the node" in failed.stderr, failed.stderr
    assert "git rebase --continue" in failed.stderr
    blocked = wm(mux_server, workmux_exe_path, repo, "merge feat", expect_fail=True)
    assert "rebase is in progress" in blocked.stderr, blocked.stderr
    assert not get_worktree_path(repo, "feat").exists()

    (worktree / "shared.txt").write_text("resolved\n")
    node_git(worktree, "add", "shared.txt")
    node_git(worktree, "-c", "core.editor=true", "rebase", "--continue")
    merged = wm(mux_server, workmux_exe_path, repo, "merge feat")
    assert "Successfully merged 'feat'" in merged.stdout, merged.stderr
    assert git(repo, "show", "main:shared.txt") == "resolved"


def test_squash_and_configured_rebase_strategies(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    configure(repo, merge_strategy="rebase")
    wm(mux_server, workmux_exe_path, repo, "add squashed --remote lo")
    wm(mux_server, workmux_exe_path, repo, "add rebased --remote lo")
    squashed = node_worktree(node, "squashed")
    commit_on_node(squashed, "a.txt", "a\n")
    commit_on_node(squashed, "b.txt", "b\n")
    rebased = node_worktree(node, "rebased")
    commit_on_node(rebased, "c.txt", "c\n")
    commit_on_node(rebased, "d.txt", "d\n")
    (repo / "main.txt").write_text("main\n")
    git(repo, "add", "main.txt")
    git(repo, "commit", "-m", "main moves")

    # --squash overrides the configured strategy: one commit, laptop editor.
    before = int(git(repo, "rev-list", "--count", "main"))
    result = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "merge --squash squashed",
        extra_env=editor(mux_server.tmp_path),
    )
    assert "Successfully merged 'squashed'" in result.stdout, result.stderr
    assert int(git(repo, "rev-list", "--count", "main")) == before + 1
    assert git(repo, "log", "-1", "--format=%s", "main") == "commit staged work"
    assert git(repo, "show", "main:b.txt") == "b"

    # merge_strategy: rebase from config keeps the commits, linear.
    before = int(git(repo, "rev-list", "--count", "main"))
    result = wm(mux_server, workmux_exe_path, repo, "merge rebased")
    assert "Successfully merged 'rebased'" in result.stdout, result.stderr
    assert int(git(repo, "rev-list", "--count", "main")) == before + 2
    assert git(repo, "rev-list", "--merges", "--count", "main") == "0"
    assert git(repo, "log", "-1", "--format=%s", "main") == "node: d.txt"
    assert node.workspaces() == []


@pytest.mark.parametrize("flags", ["", "--no-hooks", "--no-verify", "--keep"])
def test_merge_provisions_headless_dependencies(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    flags: str,
):
    repo = mux_repo_path
    (repo / ".gitignore").write_text("dependency\nprepared\n")
    git(repo, "add", ".gitignore")
    (repo / "dependency").write_text("laptop dependency\n")
    marker = node.root / "laptop-setup"
    configure(
        repo,
        files={"symlink": ["dependency"]},
        remote_files={"symlink": ["dependency"]},
        post_create=["test -L dependency && touch prepared"],
        pre_merge=["test -L dependency && test -f prepared && echo HOOK-OUTPUT"],
    )
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    commit_on_node(node_worktree(node, "feat"), "work.txt", "work\n")
    # Only the laptop's current config controls homecoming setup.
    configure(
        repo,
        files={"symlink": ["dependency"]},
        remote_files={"symlink": ["dependency"]},
        post_create=[f"test -L dependency && touch prepared {marker}"],
        pre_merge=["test -L dependency && test -f prepared && echo HOOK-OUTPUT"],
        panes=[{"command": "exit 99"}],
    )
    result = wm(mux_server, workmux_exe_path, repo, f"merge feat {flags}")
    assert marker.exists() == (flags != "--no-hooks")
    assert ("HOOK-OUTPUT" in result.stdout) == (
        flags not in ("--no-hooks", "--no-verify")
    )
    assert git(repo, "show", "main:work.txt") == "work"
