import shlex
import shutil
from pathlib import Path

import pytest

from .conftest import (
    MuxEnvironment,
    create_commit,
    create_dirty_file,
    get_scripts_dir,
    get_window_name,
    get_worktree_path,
    poll_until,
    run_workmux_add,
    run_workmux_merge,
    write_global_workmux_config,
    write_workmux_config,
)
from .support.remote_base import setup_worktree_with_remote_only_base


def test_merge_default_strategy_succeeds_and_cleans_up(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies a standard merge succeeds and cleans up all resources."""
    env = mux_server
    branch_name = "feature-to-merge"
    window_name = get_window_name(branch_name)
    write_workmux_config(repo_path, env=env)

    # Branch off first, then create commits on both branches to force a merge commit
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Create a commit on main after branching to create divergent history
    main_file = repo_path / "main_file.txt"
    main_file.write_text("content on main")
    env.run_command(["git", "add", "main_file.txt"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "commit on main"], cwd=repo_path)

    # Create a commit on feature branch
    worktree_path = get_worktree_path(repo_path, branch_name)
    commit_msg = "feat: add new file"
    create_commit(env, worktree_path, commit_msg)

    commit_hash = env.run_command(
        ["git", "rev-parse", "--short", "HEAD"], cwd=worktree_path
    ).stdout.strip()

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name)

    assert not worktree_path.exists(), "Worktree directory should be removed"
    windows = env.list_windows()
    assert window_name not in windows, "Window should be closed"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name not in branch_list_result.stdout, (
        "Local branch should be deleted"
    )

    log_result = env.run_command(["git", "log", "--oneline", "main"])
    assert commit_hash in log_result.stdout, "Feature commit should be on main branch"
    assert "Merge branch" in log_result.stdout, "A merge commit should exist on main"


def test_merge_from_within_worktree_succeeds(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux merge` with no branch arg works from inside the worktree window."""
    env = mux_server
    branch_name = "feature-in-window"
    window_name = get_window_name(branch_name)
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: a simple change")

    run_workmux_merge(
        env,
        workmux_exe_path,
        repo_path,
        branch_name=None,
        from_window=window_name,
    )

    stdout = (get_scripts_dir(env) / "workmux_merge_stdout.txt").read_text()
    assert f"Successfully merged '{branch_name}'; cleanup scheduled" in stdout
    assert f"Successfully merged and cleaned up '{branch_name}'" not in stdout
    assert poll_until(lambda: not worktree_path.exists(), timeout=15.0)
    windows = env.list_windows()
    assert window_name not in windows
    assert poll_until(
        lambda: branch_name
        not in env.run_command(["git", "branch", "--list", branch_name]).stdout,
        timeout=10.0,
    )


def test_merge_reports_synchronous_metadata_cleanup_failure_separately(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    branch_name = "merge-metadata-lock"
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: metadata lock")
    commit_hash = env.run_command(
        ["git", "rev-parse", "HEAD"], cwd=worktree_path
    ).stdout.strip()
    lock_path = repo_path / ".git" / "config.lock"
    lock_path.write_text("locked")

    run_workmux_merge(
        env,
        workmux_exe_path,
        repo_path,
        branch_name=branch_name,
        expect_fail=True,
    )

    stdout = (get_scripts_dir(env) / "workmux_merge_stdout.txt").read_text()
    stderr = (get_scripts_dir(env) / "workmux_merge_stderr.txt").read_text()
    assert f"✓ Merged '{branch_name}'" in stdout
    assert "Merge completed, but cleanup failed:" in stderr
    assert "Failed to remove Workmux metadata after Git cleanup" in stderr
    assert "Failed to merge worktree" not in stderr
    env.run_command(
        ["git", "merge-base", "--is-ancestor", commit_hash, "main"], cwd=repo_path
    )

    lock_path.unlink()
    env.run_command(
        [
            "git",
            "config",
            "--local",
            "--remove-section",
            f"workmux.worktree.{branch_name}",
        ],
        cwd=repo_path,
    )
    for trash_path in worktree_path.parent.glob(
        f".workmux_trash_{worktree_path.name}_*"
    ):
        shutil.rmtree(trash_path)


@pytest.mark.tmux_only
def test_merge_reports_close_scheduling_failure_after_success(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    branch_name = "merge-close-failure"
    window_name = get_window_name(branch_name)
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: close failure")
    commit_hash = env.run_command(
        ["git", "rev-parse", "HEAD"], cwd=worktree_path
    ).stdout.strip()

    real_tmux = shutil.which("tmux")
    assert real_tmux is not None
    fake_tmux = env.fake_bin_dir / "tmux"
    env.install_script(
        fake_tmux,
        "#!/bin/sh\n"
        'for arg in "$@"; do\n'
        '  if [ "$arg" = run-shell ]; then exit 42; fi\n'
        "done\n"
        f'exec {shlex.quote(real_tmux)} "$@"\n',
    )

    run_workmux_merge(
        env,
        workmux_exe_path,
        repo_path,
        branch_name=None,
        from_window=window_name,
        expect_fail=True,
    )

    stdout = (get_scripts_dir(env) / "workmux_merge_stdout.txt").read_text()
    stderr = (get_scripts_dir(env) / "workmux_merge_stderr.txt").read_text()
    assert f"✓ Merged '{branch_name}'" in stdout
    assert "Merge completed, but cleanup failed:" in stderr
    assert "Failed to schedule source target close" in stderr
    assert "Failed to merge worktree" not in stderr
    env.run_command(
        ["git", "merge-base", "--is-ancestor", commit_hash, "main"], cwd=repo_path
    )
    assert worktree_path.is_dir()
    assert window_name in env.list_windows()

    env.run_command(
        [str(workmux_exe_path), "remove", "--force", branch_name], cwd=repo_path
    )
    assert poll_until(lambda: not worktree_path.exists(), timeout=10.0)
    fake_tmux.unlink()


def test_merge_refuses_main_worktree_as_source(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge never treats the main worktree as a disposable source."""
    env = mux_server
    branch_name = "feature-in-main-worktree"
    write_workmux_config(repo_path, env=env)
    env.run_command(["git", "switch", "-c", branch_name], cwd=repo_path)
    create_commit(env, repo_path, "feat: work in main worktree")

    main_before = env.run_command(
        ["git", "rev-parse", "main"], cwd=repo_path
    ).stdout.strip()
    feature_before = env.run_command(
        ["git", "rev-parse", branch_name], cwd=repo_path
    ).stdout.strip()

    def assert_main_worktree_unchanged() -> None:
        assert repo_path.is_dir()
        assert (repo_path / ".git").is_dir()
        assert (
            env.run_command(
                ["git", "branch", "--show-current"], cwd=repo_path
            ).stdout.strip()
            == branch_name
        )
        assert (
            env.run_command(["git", "rev-parse", "main"], cwd=repo_path).stdout.strip()
            == main_before
        )
        assert (
            env.run_command(
                ["git", "rev-parse", branch_name], cwd=repo_path
            ).stdout.strip()
            == feature_before
        )
        assert not list(repo_path.parent.glob(f".workmux_trash_{repo_path.name}_*"))

    invocations = [
        {"branch_name": None},
        {"branch_name": None, "keep": True},
        {"branch_name": repo_path.name},
        {"branch_name": branch_name},
    ]
    for invocation in invocations:
        run_workmux_merge(
            env,
            workmux_exe_path,
            repo_path,
            expect_fail=True,
            **invocation,
        )
        assert_main_worktree_unchanged()

    linked_worktree = repo_path.parent / "observer-worktree"
    env.run_command(
        ["git", "worktree", "add", "-b", "observer", str(linked_worktree), "main"],
        cwd=repo_path,
    )
    result = env.run_command(
        [str(workmux_exe_path), "merge", repo_path.name],
        check=False,
        cwd=linked_worktree,
    )
    assert result.returncode != 0
    assert "checked out in the main worktree" in result.stderr
    assert_main_worktree_unchanged()


def test_merge_rebase_strategy_succeeds(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --rebase merge results in a linear history."""
    env = mux_server
    branch_name = "feature-to-rebase"
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Create a commit on main after branching to create divergent history
    main_file = repo_path / "main_update.txt"
    main_file.write_text("update on main")
    env.run_command(["git", "add", "main_update.txt"], cwd=repo_path)
    main_commit_msg = "docs: update readme on main"
    env.run_command(["git", "commit", "-m", main_commit_msg], cwd=repo_path)

    # Create a commit on the feature branch
    worktree_path = get_worktree_path(repo_path, branch_name)
    feature_commit_msg = "feat: rebased feature"
    create_commit(env, worktree_path, feature_commit_msg)

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name, rebase=True)

    assert not worktree_path.exists()

    log_result = env.run_command(["git", "log", "--oneline", "main"])
    # Note: After rebase, the commit hash changes, so we check for the message
    assert feature_commit_msg in log_result.stdout, (
        "Feature commit should be in main history"
    )
    assert "Merge branch" not in log_result.stdout, (
        "No merge commit should exist for rebase"
    )

    # Verify linear history: the feature commit should come after the main commit
    log_lines = log_result.stdout.strip().split("\n")
    feature_commit_index = next(
        i for i, line in enumerate(log_lines) if feature_commit_msg in line
    )
    main_commit_index = next(
        i for i, line in enumerate(log_lines) if main_commit_msg in line
    )
    assert feature_commit_index < main_commit_index, (
        "Feature commit should be rebased on top of main's new commit"
    )


def test_merge_strategy_config_rebase(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge_strategy config option applies rebase without CLI flag."""
    env = mux_server
    branch_name = "feature-config-rebase"
    write_workmux_config(repo_path, env=env, merge_strategy="rebase")
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Create a commit on main after branching to create divergent history
    main_file = repo_path / "main_config_update.txt"
    main_file.write_text("update on main")
    env.run_command(["git", "add", "main_config_update.txt"], cwd=repo_path)
    main_commit_msg = "docs: update on main for config test"
    env.run_command(["git", "commit", "-m", main_commit_msg], cwd=repo_path)

    # Create a commit on the feature branch
    worktree_path = get_worktree_path(repo_path, branch_name)
    feature_commit_msg = "feat: feature via config rebase"
    create_commit(env, worktree_path, feature_commit_msg)

    # Run merge WITHOUT --rebase flag - should use config
    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name)

    assert not worktree_path.exists()

    log_result = env.run_command(["git", "log", "--oneline", "main"])
    assert feature_commit_msg in log_result.stdout, (
        "Feature commit should be in main history"
    )
    assert "Merge branch" not in log_result.stdout, (
        "No merge commit should exist when merge_strategy: rebase is configured"
    )


def test_merge_squash_strategy_succeeds(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --squash merge combines multiple commits into one."""
    env = mux_server
    branch_name = "feature-to-squash"
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: first commit")
    first_commit_hash = env.run_command(
        ["git", "rev-parse", "--short", "HEAD"], cwd=worktree_path
    ).stdout.strip()
    create_commit(env, worktree_path, "feat: second commit")
    second_commit_hash = env.run_command(
        ["git", "rev-parse", "--short", "HEAD"], cwd=worktree_path
    ).stdout.strip()

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name, squash=True)

    assert not worktree_path.exists()

    log_result = env.run_command(["git", "log", "--oneline", "main"])
    assert first_commit_hash not in log_result.stdout, (
        "Original commits should not be in main history"
    )
    assert second_commit_hash not in log_result.stdout, (
        "Original commits should not be in main history"
    )
    assert "Merge branch" not in log_result.stdout, "No merge commit for squash"


def test_merge_fails_on_unstaged_changes(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge fails if worktree has unstaged changes."""
    env = mux_server
    branch_name = "feature-with-unstaged"
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    # Create a commit first, then modify the file to create unstaged changes
    create_commit(env, worktree_path, "feat: initial work")
    # Modify an existing tracked file to create unstaged changes
    (worktree_path / "file_for_feat_initial_work.txt").write_text("modified content")

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name, expect_fail=True)

    assert worktree_path.exists(), "Worktree should not be removed when command fails"


def test_merge_succeeds_with_ignore_uncommitted_flag(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --ignore-uncommitted allows merge despite unstaged changes."""
    env = mux_server
    branch_name = "feature-ignore-uncommitted"
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: committed work")
    create_dirty_file(worktree_path)

    run_workmux_merge(
        env, workmux_exe_path, repo_path, branch_name, ignore_uncommitted=True
    )

    assert not worktree_path.exists(), "Worktree should be removed despite dirty files"


def test_merge_commits_staged_changes_before_merge(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge automatically commits staged changes."""
    env = mux_server
    branch_name = "feature-with-staged"
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    staged_file = worktree_path / "staged_file.txt"
    staged_file.write_text("staged content")
    env.run_command(["git", "add", "staged_file.txt"], cwd=worktree_path)

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name)

    assert not worktree_path.exists()
    show_result = env.run_command(["git", "show", "main:staged_file.txt"])
    assert "staged content" in show_result.stdout, "Staged file should be in main"


def test_merge_fails_if_main_worktree_has_uncommitted_tracked_changes(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge fails if main worktree has uncommitted tracked changes."""
    env = mux_server
    branch_name = "feature-clean"
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: work done")

    # Modify a tracked file to create uncommitted tracked changes
    # The .workmux.yaml is committed by write_workmux_config
    workmux_config = repo_path / ".workmux.yaml"
    workmux_config.write_text(workmux_config.read_text() + "\n# dirty")

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name, expect_fail=True)

    assert worktree_path.exists(), "Worktree should remain when merge fails"


def test_merge_succeeds_with_untracked_files_in_main_worktree(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge succeeds when main worktree has untracked files."""
    env = mux_server
    branch_name = "feature-with-untracked-main"
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: work done")

    # Create an untracked file in the main worktree - this should NOT block merge
    untracked_file = repo_path / "untracked_in_main.txt"
    create_dirty_file(repo_path, "untracked_in_main.txt")

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name)

    assert not worktree_path.exists(), (
        "Worktree should be removed after successful merge"
    )
    assert untracked_file.exists(), "Untracked file in main should be preserved"


def test_merge_with_keep_flag_skips_cleanup(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --keep flag merges without cleaning up worktree, window, or branch."""
    env = mux_server
    branch_name = "feature-to-keep"
    window_name = get_window_name(branch_name)
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    commit_msg = "feat: add feature"
    create_commit(env, worktree_path, commit_msg)

    commit_hash = env.run_command(
        ["git", "rev-parse", "--short", "HEAD"], cwd=worktree_path
    ).stdout.strip()

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name, keep=True)

    # Verify the merge happened
    log_result = env.run_command(["git", "log", "--oneline", "main"])
    assert commit_hash in log_result.stdout, "Feature commit should be on main branch"

    # Verify cleanup did NOT happen
    assert worktree_path.exists(), "Worktree should still exist with --keep"
    windows = env.list_windows()
    assert window_name in windows, "Window should still exist"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name in branch_list_result.stdout, "Local branch should still exist"


def test_merge_keep_config_skips_cleanup(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    branch_name = "feature-config-keep"
    window_name = get_window_name(branch_name)
    write_workmux_config(repo_path, env=env, merge_keep=True)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: keep via config")
    commit_hash = env.run_command(
        ["git", "rev-parse", "--short", "HEAD"], cwd=worktree_path
    ).stdout.strip()

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name)

    log_result = env.run_command(["git", "log", "--oneline", "main"])
    assert commit_hash in log_result.stdout, "Feature commit should be on main branch"
    assert worktree_path.exists(), "Worktree should still exist with merge_keep"
    windows = env.list_windows()
    assert window_name in windows, "Window should still exist with merge_keep"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name in branch_list_result.stdout, (
        "Local branch should still exist with merge_keep"
    )


def test_merge_cleanup_flag_overrides_merge_keep_config(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    branch_name = "feature-config-cleanup"
    window_name = get_window_name(branch_name)
    write_workmux_config(repo_path, env=env, merge_keep=True)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: cleanup override")

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name, cleanup=True)

    assert not worktree_path.exists(), "Worktree should be removed with --cleanup"
    windows = env.list_windows()
    assert window_name not in windows, "Window should be closed with --cleanup"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name not in branch_list_result.stdout, (
        "Local branch should be deleted with --cleanup"
    )


def test_merge_keep_global_config_skips_cleanup(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    branch_name = "feature-global-keep"
    window_name = get_window_name(branch_name)
    write_global_workmux_config(env, merge_keep=True)
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: global keep")

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name)

    assert worktree_path.exists(), "Worktree should still exist with global merge_keep"
    windows = env.list_windows()
    assert window_name in windows, "Window should still exist with global merge_keep"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name in branch_list_result.stdout, (
        "Local branch should still exist with global merge_keep"
    )


def test_merge_keep_project_config_false_overrides_global_true(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    branch_name = "feature-project-cleanup"
    window_name = get_window_name(branch_name)
    write_global_workmux_config(env, merge_keep=True)
    write_workmux_config(repo_path, env=env, merge_keep=False)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: project cleanup")

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name)

    assert not worktree_path.exists(), "Project merge_keep false should clean up"
    windows = env.list_windows()
    assert window_name not in windows, "Project merge_keep false should close window"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name not in branch_list_result.stdout, (
        "Project merge_keep false should delete local branch"
    )


def test_merge_into_different_branch(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --into flag merges into a different branch instead of main."""
    env = mux_server
    parent_branch = "feature/parent"
    child_branch = "feature/child"
    child_window_name = get_window_name(child_branch)
    write_workmux_config(repo_path, env=env)

    # Create parent feature branch with a worktree
    run_workmux_add(env, workmux_exe_path, repo_path, parent_branch)
    parent_worktree_path = get_worktree_path(repo_path, parent_branch)
    create_commit(env, parent_worktree_path, "feat: parent feature base")

    # Create child branch based on parent
    run_workmux_add(env, workmux_exe_path, repo_path, child_branch, base=parent_branch)
    child_worktree_path = get_worktree_path(repo_path, child_branch)

    # Create a commit on the child branch
    child_commit_msg = "feat: child subtask work"
    create_commit(env, child_worktree_path, child_commit_msg)
    child_commit_hash = env.run_command(
        ["git", "rev-parse", "--short", "HEAD"], cwd=child_worktree_path
    ).stdout.strip()

    # Merge child into parent (not main)
    run_workmux_merge(
        env, workmux_exe_path, repo_path, child_branch, into=parent_branch
    )

    # Verify child worktree was cleaned up
    assert not child_worktree_path.exists(), "Child worktree should be removed"
    windows = env.list_windows()
    assert child_window_name not in windows, "Child window should be closed"
    branch_list_result = env.run_command(["git", "branch", "--list", child_branch])
    assert child_branch not in branch_list_result.stdout, (
        "Child branch should be deleted"
    )

    # Verify the commit is on parent branch, NOT on main
    parent_log_result = env.run_command(
        ["git", "log", "--oneline", parent_branch], cwd=repo_path
    )
    assert child_commit_hash in parent_log_result.stdout, (
        "Child commit should be on parent branch"
    )

    main_log_result = env.run_command(
        ["git", "log", "--oneline", "main"], cwd=repo_path
    )
    assert child_commit_hash not in main_log_result.stdout, (
        "Child commit should NOT be on main branch"
    )

    # Verify parent worktree still exists
    assert parent_worktree_path.exists(), "Parent worktree should still exist"


def test_merge_auto_detects_base_branch(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge auto-detects base branch when --into is not specified.

    When a branch is created from a non-main branch using `workmux add --base`,
    the merge command should automatically merge back into that base branch
    without requiring --into to be specified.
    """
    env = mux_server
    parent_branch = "feature/parent-auto"
    child_branch = "feature/child-auto"
    child_window_name = get_window_name(child_branch)
    write_workmux_config(repo_path, env=env)

    # Create parent feature branch with a worktree
    run_workmux_add(env, workmux_exe_path, repo_path, parent_branch)
    parent_worktree_path = get_worktree_path(repo_path, parent_branch)
    create_commit(env, parent_worktree_path, "feat: parent feature base")

    # Create child branch based on parent (this stores the base branch in git config)
    run_workmux_add(env, workmux_exe_path, repo_path, child_branch, base=parent_branch)
    child_worktree_path = get_worktree_path(repo_path, child_branch)

    # Create a commit on the child branch
    child_commit_msg = "feat: child auto-merge work"
    create_commit(env, child_worktree_path, child_commit_msg)
    child_commit_hash = env.run_command(
        ["git", "rev-parse", "--short", "HEAD"], cwd=child_worktree_path
    ).stdout.strip()

    # Merge child WITHOUT specifying --into - should auto-detect parent as target
    run_workmux_merge(env, workmux_exe_path, repo_path, child_branch)

    # Verify child worktree was cleaned up
    assert not child_worktree_path.exists(), "Child worktree should be removed"
    windows = env.list_windows()
    assert child_window_name not in windows, "Child window should be closed"
    branch_list_result = env.run_command(["git", "branch", "--list", child_branch])
    assert child_branch not in branch_list_result.stdout, (
        "Child branch should be deleted"
    )

    # Verify the commit is on parent branch (auto-detected), NOT on main
    parent_log_result = env.run_command(
        ["git", "log", "--oneline", parent_branch], cwd=repo_path
    )
    assert child_commit_hash in parent_log_result.stdout, (
        "Child commit should be on parent branch (auto-detected)"
    )

    main_log_result = env.run_command(
        ["git", "log", "--oneline", "main"], cwd=repo_path
    )
    assert child_commit_hash not in main_log_result.stdout, (
        "Child commit should NOT be on main branch"
    )

    # Verify parent worktree still exists
    assert parent_worktree_path.exists(), "Parent worktree should still exist"


def test_merge_falls_back_to_main_when_base_branch_deleted(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge falls back to main when the stored base branch no longer exists."""
    env = mux_server
    parent_branch = "feature/temp-parent"
    child_branch = "feature/orphan-child"
    write_workmux_config(repo_path, env=env)

    # Create parent feature branch with a worktree
    run_workmux_add(env, workmux_exe_path, repo_path, parent_branch)
    parent_worktree_path = get_worktree_path(repo_path, parent_branch)
    create_commit(env, parent_worktree_path, "feat: temp parent base")

    # Create child branch based on parent
    run_workmux_add(env, workmux_exe_path, repo_path, child_branch, base=parent_branch)
    child_worktree_path = get_worktree_path(repo_path, child_branch)

    # Create a commit on the child branch
    child_commit_msg = "feat: orphan child work"
    create_commit(env, child_worktree_path, child_commit_msg)
    child_commit_hash = env.run_command(
        ["git", "rev-parse", "--short", "HEAD"], cwd=child_worktree_path
    ).stdout.strip()

    # Delete the parent branch (simulating it was merged and deleted elsewhere)
    # First remove the worktree, then delete the branch
    env.run_command(
        [str(workmux_exe_path), "remove", "--force", parent_branch], cwd=repo_path
    )

    # Merge child WITHOUT specifying --into - should fall back to main since parent is gone
    run_workmux_merge(env, workmux_exe_path, repo_path, child_branch)

    # Verify child worktree was cleaned up
    assert not child_worktree_path.exists(), "Child worktree should be removed"

    # Verify the commit ended up on main (fallback behavior)
    main_log_result = env.run_command(
        ["git", "log", "--oneline", "main"], cwd=repo_path
    )
    assert child_commit_hash in main_log_result.stdout, (
        "Child commit should be on main branch (fallback when base deleted)"
    )


def test_merge_falls_back_to_main_when_base_is_remote_only(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge ignores remote-only stored base refs."""
    env = mux_server
    branch_name = "feature/remote-base-child"
    worktree = setup_worktree_with_remote_only_base(
        env, workmux_exe_path, repo_path, branch_name
    )
    worktree_path = worktree.path

    commit_msg = "feat: remote base child work"
    create_commit(env, worktree_path, commit_msg)
    commit_hash = env.run_command(
        ["git", "rev-parse", "--short", "HEAD"], cwd=worktree_path
    ).stdout.strip()

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name)

    assert not worktree_path.exists(), "Worktree should be removed"
    main_log_result = env.run_command(
        ["git", "log", "--oneline", "main"], cwd=repo_path
    )
    assert commit_hash in main_log_result.stdout


def test_merge_succeeds_when_target_branch_checked_out_in_another_worktree(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge works when target branch is in a linked worktree.

    When the target branch (e.g., main) is already checked out in a separate worktree,
    workmux should perform the merge in that worktree instead of trying to switch
    branches in the main worktree root.
    """
    env = mux_server
    feature_branch = "feature-issue-29"
    write_workmux_config(repo_path, env=env)

    # Create and switch to develop branch in main worktree
    env.run_command(["git", "checkout", "-b", "develop"], cwd=repo_path)

    # Create a separate worktree for main branch
    main_worktree_path = repo_path.parent / "main-worktree"
    env.run_command(
        ["git", "worktree", "add", str(main_worktree_path), "main"], cwd=repo_path
    )

    # Create feature worktree using workmux, based on main
    run_workmux_add(env, workmux_exe_path, repo_path, feature_branch, base="main")
    feature_worktree_path = get_worktree_path(repo_path, feature_branch)

    # Create a commit on the feature branch
    feature_commit_msg = "feat: issue 29 test commit"
    create_commit(env, feature_worktree_path, feature_commit_msg)
    commit_hash = env.run_command(
        ["git", "rev-parse", "--short", "HEAD"], cwd=feature_worktree_path
    ).stdout.strip()

    # Merge into main - should succeed by using the main-worktree
    run_workmux_merge(env, workmux_exe_path, repo_path, feature_branch, into="main")

    # Verify the feature worktree was cleaned up
    assert not feature_worktree_path.exists(), "Feature worktree should be removed"

    # Verify the commit is on main branch
    main_log_result = env.run_command(
        ["git", "log", "--oneline", "main"], cwd=repo_path
    )
    assert commit_hash in main_log_result.stdout, (
        "Feature commit should be on main branch"
    )


def test_merge_succeeds_with_bare_repo_and_linked_worktrees(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies merge works correctly in a bare repo with linked worktrees setup.

    This tests the scenario where:
    1. The repository is a bare repo (e.g., .bare directory)
    2. All branches including main are in linked worktrees
    3. The worktree being merged has a name that sorts alphabetically before main

    This is a regression test for a bug where `git worktree prune` would fail
    with "No such file or directory" because get_main_worktree_root() returned
    a non-existent worktree path.

    The bug occurs because:
    1. git worktree list returns worktrees sorted by path
    2. When all worktrees are siblings (same directory level), they sort alphabetically
    3. "add-feature" < "main", so the feature worktree is listed first
    4. get_main_worktree_root() returns the first non-bare worktree
    5. After the feature worktree is renamed to trash, that path no longer exists
    6. prune_worktrees() tries to run git from that non-existent path and fails
    """
    env = mux_server
    base_dir = env.tmp_path / "bare-repo-test"
    base_dir.mkdir()

    # Create a bare repository
    bare_repo = base_dir / ".bare"
    env.run_command(["git", "init", "--bare", str(bare_repo)])

    # Configure git user in the bare repo
    env.run_command(["git", "config", "user.name", "Test User"], cwd=bare_repo)
    env.run_command(["git", "config", "user.email", "test@example.com"], cwd=bare_repo)

    # Create the main worktree from bare repo
    main_worktree = base_dir / "main"
    env.run_command(
        ["git", "worktree", "add", str(main_worktree), "-b", "main"],
        cwd=bare_repo,
    )

    # Create an initial commit in main
    initial_file = main_worktree / "README.md"
    initial_file.write_text("# Test Repo")
    env.run_command(["git", "add", "README.md"], cwd=main_worktree)
    env.run_command(["git", "commit", "-m", "Initial commit"], cwd=main_worktree)

    # Create .workmux.yaml and commit it
    write_workmux_config(main_worktree, env=env)

    # Create a feature branch worktree MANUALLY as a sibling of main.
    # This is critical: the worktree must be at the same directory level as main
    # so that it sorts alphabetically BEFORE main in git worktree list.
    # Using workmux add would create it in a __worktrees subdirectory, which
    # would sort AFTER main and not trigger the bug.
    branch_name = "add-feature"  # 'add' < 'main' alphabetically
    feature_worktree = base_dir / branch_name  # Sibling of main, not in __worktrees
    env.run_command(
        ["git", "worktree", "add", str(feature_worktree), "-b", branch_name],
        cwd=main_worktree,
    )

    # Verify the worktree order - add-feature should come before main
    worktree_list = env.run_command(
        ["git", "worktree", "list", "--porcelain"], cwd=main_worktree
    )
    # The order should be: .bare, add-feature, main
    assert worktree_list.stdout.index("add-feature") < worktree_list.stdout.index(
        "/main\n"
    ), "add-feature worktree must be listed before main worktree to trigger the bug"

    # Create a commit on the feature branch
    create_commit(env, feature_worktree, "feat: new feature")

    # Merge the feature branch - this should succeed without errors
    # Before the fix, this would fail with:
    # "Failed to prune worktrees" / "No such file or directory"
    run_workmux_merge(env, workmux_exe_path, main_worktree, branch_name)

    # Verify cleanup succeeded
    assert not feature_worktree.exists(), "Feature worktree should be removed"

    branch_list_result = env.run_command(
        ["git", "branch", "--list", branch_name], cwd=main_worktree
    )
    assert branch_name not in branch_list_result.stdout, (
        "Local branch should be deleted"
    )
