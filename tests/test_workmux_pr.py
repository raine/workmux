"""
Tests for PR checkout functionality (workmux add --pr <number>)
"""

from pathlib import Path

from .conftest import (
    MuxEnvironment,
    get_window_name,
    get_worktree_path,
    install_fake_gh_cli,
    run_workmux_command,
    setup_git_repo,
)


GITHUB_URL = "https://github.com/testowner/testrepo.git"
PR_URL_BASE = "https://github.com/testowner/testrepo/pull"


def same_repo_pr_data(
    pr_number: int,
    head_ref: str,
    *,
    title: str = "Add new feature",
    state: str = "OPEN",
    is_draft: bool = False,
) -> dict:
    """PR JSON for a same-repo (non-fork) PR against testowner/testrepo."""
    return {
        "headRefName": head_ref,
        "headRepositoryOwner": {"login": "testowner"},
        "headRepository": {"name": "testrepo"},
        "isCrossRepository": False,
        "url": f"{PR_URL_BASE}/{pr_number}",
        "state": state,
        "isDraft": is_draft,
        "title": title,
        "author": {"login": "contributor"},
    }


def setup_pr_remote_and_branch(
    env: MuxEnvironment,
    repo_path: Path,
    remote_repo_path: Path,
    branch_name: str,
    pr_number: int,
):
    """Set up a fetchable origin and publish a PR head as refs/pull/<n>/head.

    workmux fetches PRs via the GitHub-style pull ref rather than the head
    branch, so the bare remote must expose that ref.
    """
    env.run_command(
        ["git", "remote", "add", "origin", GITHUB_URL],
        cwd=repo_path,
    )
    # Set pushurl to the local path so git operations actually work
    env.run_command(
        ["git", "remote", "set-url", "--push", "origin", str(remote_repo_path)],
        cwd=repo_path,
    )
    # Also need to configure insteadOf for fetch operations
    env.run_command(
        ["git", "config", f"url.{remote_repo_path}.insteadOf", GITHUB_URL],
        cwd=repo_path,
    )
    env.run_command(["git", "push", "-u", "origin", "main"], cwd=repo_path)

    # Create the PR head commit and publish it as refs/pull/<n>/head on the
    # remote. We do not push the branch itself, mirroring the case where the
    # head branch was deleted after merge.
    env.run_command(["git", "checkout", "-b", branch_name], cwd=repo_path)
    env.run_command(
        ["git", "commit", "--allow-empty", "-m", "PR changes"],
        cwd=repo_path,
    )
    env.run_command(
        ["git", "push", "origin", f"HEAD:refs/pull/{pr_number}/head"],
        cwd=repo_path,
    )
    env.run_command(["git", "checkout", "main"], cwd=repo_path)
    # Delete the local branch so workmux can create it fresh (matching gh pr checkout behavior)
    env.run_command(["git", "branch", "-D", branch_name], cwd=repo_path)


def test_add_pr_from_same_repo(mux_server, workmux_exe_path, remote_repo_path):
    """Test basic PR checkout from same repository"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "feature-branch", 123)

    pr_data = same_repo_pr_data(123, "feature-branch")
    install_fake_gh_cli(env, pr_number=123, json_response=pr_data)

    result = run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 123")

    assert "PR #123" in result.stdout
    assert "Add new feature" in result.stdout
    assert "contributor" in result.stdout

    worktree_path = get_worktree_path(repo_path, "feature-branch")
    assert worktree_path.exists()

    window_name = get_window_name("feature-branch")
    windows = env.list_windows()
    assert window_name in windows


def test_add_pr_with_custom_branch_name(mux_server, workmux_exe_path, remote_repo_path):
    """Test PR checkout with custom branch name"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "feature-branch", 123)

    pr_data = same_repo_pr_data(123, "feature-branch")
    install_fake_gh_cli(env, pr_number=123, json_response=pr_data)

    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add my-review --pr 123"
    )

    assert "PR #123" in result.stdout

    worktree_path = get_worktree_path(repo_path, "my-review")
    assert worktree_path.exists()

    window_name = get_window_name("my-review")
    windows = env.list_windows()
    assert window_name in windows


def test_add_pr_merged_state_warning(mux_server, workmux_exe_path, remote_repo_path):
    """Test warning is displayed for merged PRs"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "merged-branch", 456)

    pr_data = same_repo_pr_data(
        456, "merged-branch", title="Already merged PR", state="MERGED"
    )
    install_fake_gh_cli(env, pr_number=456, json_response=pr_data)

    result = run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 456")

    assert "Warning" in result.stderr or "MERGED" in result.stderr
    assert "456" in result.stdout

    worktree_path = get_worktree_path(repo_path, "merged-branch")
    assert worktree_path.exists()


def test_add_pr_draft_warning(mux_server, workmux_exe_path, remote_repo_path):
    """Test warning is displayed for draft PRs"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "draft-branch", 789)

    pr_data = same_repo_pr_data(
        789, "draft-branch", title="WIP: Work in progress", is_draft=True
    )
    install_fake_gh_cli(env, pr_number=789, json_response=pr_data)

    result = run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 789")

    assert "DRAFT" in result.stderr or "draft" in result.stderr.lower()

    worktree_path = get_worktree_path(repo_path, "draft-branch")
    assert worktree_path.exists()


def test_add_pr_fails_on_invalid_pr_number(
    mux_server, workmux_exe_path, remote_repo_path
):
    """Test error handling for invalid PR number"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    env.run_command(
        ["git", "remote", "add", "origin", str(remote_repo_path)],
        cwd=repo_path,
    )

    install_fake_gh_cli(
        env,
        pr_number=999,
        json_response=None,
        stderr="pull request not found",
        exit_code=1,
    )

    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add --pr 999", expect_fail=True
    )

    assert result.exit_code != 0
    assert (
        "Failed to fetch" in result.stderr or "pull request not found" in result.stderr
    )


def test_add_pr_fails_when_gh_not_installed(
    mux_server, workmux_exe_path, remote_repo_path
):
    """Test error when gh CLI is not available"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    env.run_command(
        ["git", "remote", "add", "origin", str(remote_repo_path)],
        cwd=repo_path,
    )

    # Don't install fake gh CLI - it won't be found in PATH

    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add --pr 123", expect_fail=True
    )

    assert result.exit_code != 0
    assert "gh" in result.stderr.lower() or "GitHub CLI" in result.stderr


def test_add_pr_conflicts_with_base_flag(
    mux_server, workmux_exe_path, remote_repo_path
):
    """Test that --pr conflicts with --base flag"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add --pr 123 --base main",
        expect_fail=True,
    )

    assert result.exit_code != 0
    assert (
        "conflict" in result.stderr.lower() or "cannot be used" in result.stderr.lower()
    )


def test_add_pr_fork_with_main_branch(mux_server, workmux_exe_path, remote_repo_path):
    """Cross-repo PR with head branch 'main' is fetched via pull ref and
    checked out under an owner-prefixed name to avoid clobbering local main."""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    # Origin = base repo. Publish the fork's commit as refs/pull/16/head there;
    # workmux never needs to talk to the fork directly.
    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "fork-work", 16)

    pr_data = {
        "headRefName": "main",
        "headRepositoryOwner": {"login": "forkowner"},
        "headRepository": {"name": "testrepo"},
        "isCrossRepository": True,
        "url": f"{PR_URL_BASE}/16",
        "state": "OPEN",
        "isDraft": False,
        "title": "Use ANSI palette colors",
        "author": {"login": "forkowner"},
    }
    install_fake_gh_cli(env, pr_number=16, json_response=pr_data)

    result = run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 16")

    assert "PR #16" in result.stdout
    assert "Use ANSI palette colors" in result.stdout

    # The worktree should be created with the prefixed branch name
    worktree_path = get_worktree_path(repo_path, "forkowner-main")
    assert worktree_path.exists(), (
        f"Expected worktree at {worktree_path} (forkowner-main), "
        f"but it does not exist. stderr: {result.stderr}"
    )

    window_name = get_window_name("forkowner-main")
    windows = env.list_windows()
    assert window_name in windows

    # Upstream tracking should point at the fork's URL so push/pull target it.
    remote = env.run_command(
        ["git", "config", "branch.forkowner-main.remote"], cwd=repo_path
    ).stdout.strip()
    assert remote == "git@github.com:forkowner/testrepo.git"


def test_add_pr_fails_when_worktree_exists(
    mux_server, workmux_exe_path, remote_repo_path
):
    """Test error when trying to checkout same PR twice"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "feature-branch", 123)

    pr_data = same_repo_pr_data(123, "feature-branch")
    install_fake_gh_cli(env, pr_number=123, json_response=pr_data)

    # First checkout should succeed
    run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 123")

    # Second checkout should fail
    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add --pr 123", expect_fail=True
    )

    assert result.exit_code != 0
    assert (
        "already exists" in result.stderr.lower() or "worktree" in result.stderr.lower()
    )
