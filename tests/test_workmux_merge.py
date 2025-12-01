from pathlib import Path

from .conftest import (
    TmuxEnvironment,
    create_commit,
    create_dirty_file,
    get_window_name,
    get_worktree_path,
    run_workmux_add,
    run_workmux_merge,
    write_workmux_config,
)


def test_merge_default_strategy_succeeds_and_cleans_up(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies a standard merge succeeds and cleans up all resources."""
    env = isolated_tmux_server
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
    list_windows_result = env.tmux(["list-windows", "-F", "#{window_name}"])
    assert window_name not in list_windows_result.stdout, "Tmux window should be closed"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name not in branch_list_result.stdout, (
        "Local branch should be deleted"
    )

    log_result = env.run_command(["git", "log", "--oneline", "main"])
    assert commit_hash in log_result.stdout, "Feature commit should be on main branch"
    assert "Merge branch" in log_result.stdout, "A merge commit should exist on main"


def test_merge_from_within_worktree_succeeds(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux merge` with no branch arg works from inside the worktree window."""
    env = isolated_tmux_server
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

    assert not worktree_path.exists()
    list_windows_result = env.tmux(["list-windows", "-F", "#{window_name}"])
    assert window_name not in list_windows_result.stdout
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name not in branch_list_result.stdout


def test_merge_rebase_strategy_succeeds(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --rebase merge results in a linear history."""
    env = isolated_tmux_server
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge_strategy config option applies rebase without CLI flag."""
    env = isolated_tmux_server
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --squash merge combines multiple commits into one."""
    env = isolated_tmux_server
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge fails if worktree has unstaged changes."""
    env = isolated_tmux_server
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --ignore-uncommitted allows merge despite unstaged changes."""
    env = isolated_tmux_server
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge automatically commits staged changes."""
    env = isolated_tmux_server
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


def test_merge_fails_if_main_worktree_has_uncommitted_changes(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies merge fails if main worktree has uncommitted changes."""
    env = isolated_tmux_server
    branch_name = "feature-clean"
    write_workmux_config(repo_path, env=env)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    create_commit(env, worktree_path, "feat: work done")

    create_dirty_file(repo_path, "dirty_in_main.txt")

    run_workmux_merge(env, workmux_exe_path, repo_path, branch_name, expect_fail=True)

    assert worktree_path.exists(), "Worktree should remain when merge fails"


def test_merge_with_keep_flag_skips_cleanup(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --keep flag merges without cleaning up worktree, window, or branch."""
    env = isolated_tmux_server
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
    list_windows_result = env.tmux(["list-windows", "-F", "#{window_name}"])
    assert window_name in list_windows_result.stdout, "Tmux window should still exist"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name in branch_list_result.stdout, "Local branch should still exist"


def test_merge_into_different_branch(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --into flag merges into a different branch instead of main."""
    env = isolated_tmux_server
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
    list_windows_result = env.tmux(["list-windows", "-F", "#{window_name}"])
    assert child_window_name not in list_windows_result.stdout, (
        "Child tmux window should be closed"
    )
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
