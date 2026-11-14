"""
Tests for PR checkout functionality (`workmux add --pr <number-or-url>`).
"""

from .conftest import (
    get_window_name,
    get_worktree_path,
    install_fake_gh_cli,
    run_workmux_command,
    setup_git_repo,
)
from .support.pr import (
    commit_file,
    install_fake_pr_view,
    setup_pr_remote,
    setup_pr_remote_and_branch,
)


def test_add_pr_from_same_repo(mux_server, workmux_exe_path, remote_repo_path):
    """Test basic PR checkout from same repository"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "feature-branch")
    install_fake_pr_view(env)

    result = run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 123")

    assert "PR #123" in result.stdout
    assert "Add new feature" in result.stdout
    assert "contributor" in result.stdout

    worktree_path = get_worktree_path(repo_path, "feature-branch")
    assert worktree_path.exists()

    window_name = get_window_name("feature-branch")
    windows = env.list_windows()
    assert window_name in windows


def test_add_pr_from_github_url(mux_server, workmux_exe_path, remote_repo_path):
    """Test PR checkout from a full GitHub pull request URL."""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "feature-branch")
    install_fake_pr_view(env, number=190)

    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add --pr https://github.com/raine/workmux/pull/190 url-review",
    )

    assert "PR #190" in result.stdout
    assert get_worktree_path(repo_path, "url-review").exists()
    assert get_window_name("url-review") in env.list_windows()


def test_add_pr_rejects_invalid_github_url(mux_server, workmux_exe_path):
    """Test that --pr rejects GitHub URLs outside the pull request path."""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add --pr https://github.com/raine/workmux/issues/190",
        expect_fail=True,
    )

    assert result.exit_code != 0
    assert "full GitHub pull request URL" in result.stderr


def test_add_pr_with_custom_branch_name(mux_server, workmux_exe_path, remote_repo_path):
    """Test PR checkout with custom branch name"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "feature-branch")
    install_fake_pr_view(env)

    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add my-review --pr 123"
    )

    assert "PR #123" in result.stdout

    worktree_path = get_worktree_path(repo_path, "my-review")
    assert worktree_path.exists()

    window_name = get_window_name("my-review")
    windows = env.list_windows()
    assert window_name in windows


def test_add_pr_reuses_existing_local_branch(
    mux_server, workmux_exe_path, remote_repo_path
):
    """Test PR checkout when the PR branch already exists locally"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "feature-branch")
    env.run_command(
        ["git", "branch", "feature-branch", "origin/feature-branch"],
        cwd=repo_path,
    )
    original_tip = env.run_command(
        ["git", "rev-parse", "feature-branch"],
        cwd=repo_path,
    ).stdout.strip()

    install_fake_pr_view(env)

    result = run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 123")

    assert "PR #123" in result.stdout

    worktree_path = get_worktree_path(repo_path, "feature-branch")
    assert worktree_path.exists()
    assert (worktree_path / ".git").exists()

    worktree_tip = env.run_command(
        ["git", "rev-parse", "HEAD"],
        cwd=worktree_path,
    ).stdout.strip()
    assert worktree_tip == original_tip

    window_name = get_window_name("feature-branch")
    windows = env.list_windows()
    assert window_name in windows


def test_add_pr_merged_state_warning(mux_server, workmux_exe_path, remote_repo_path):
    """Test warning is displayed for merged PRs"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "merged-branch")

    install_fake_pr_view(
        env,
        number=456,
        branch="merged-branch",
        state="MERGED",
        title="Already merged PR",
    )

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

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "draft-branch")

    install_fake_pr_view(
        env,
        number=789,
        branch="draft-branch",
        draft=True,
        title="WIP: Work in progress",
    )

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
    """Test that fork PRs with branch 'main' get prefixed with owner to avoid conflict"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote(env, repo_path, remote_repo_path)

    fork_repo_path = repo_path.parent / "fork_repo.git"
    env.run_command(
        ["git", "clone", "--bare", str(remote_repo_path), str(fork_repo_path)]
    )

    fork_work = repo_path.parent / "fork_work"
    env.run_command(
        ["git", "clone", str(fork_repo_path), str(fork_work)], cwd=repo_path
    )
    env.run_command(
        ["git", "switch", "--force-create", "main", "origin/main"], cwd=fork_work
    )
    env.run_command(["git", "config", "user.name", "Fork User"], cwd=fork_work)
    env.run_command(["git", "config", "user.email", "fork@example.com"], cwd=fork_work)
    fork_commit = commit_file(
        env,
        fork_work,
        "fork-content.txt",
        "Fork PR changes\n",
        "Fork PR changes",
    )
    env.run_command(["git", "push", "origin", "main"], cwd=fork_work)

    fork_github_url = "https://github.com/forkowner/testrepo.git"
    env.run_command(
        ["git", "config", f"url.{fork_repo_path}.insteadOf", fork_github_url],
        cwd=repo_path,
    )

    install_fake_pr_view(
        env,
        number=16,
        branch="main",
        owner="forkowner",
        title="Use ANSI palette colors",
        author="forkowner",
    )

    result = run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 16")

    assert "PR #16" in result.stdout
    assert "Use ANSI palette colors" in result.stdout

    worktree_path = get_worktree_path(repo_path, "forkowner-main")
    assert worktree_path.exists(), (
        f"Expected worktree at {worktree_path} (forkowner-main), "
        f"but it does not exist. stderr: {result.stderr}"
    )

    window_name = get_window_name("forkowner-main")
    windows = env.list_windows()
    assert window_name in windows

    # The fork head is checked out, but the review base is the target branch
    # in the target repository, not the fork remote.
    assert (worktree_path / "fork-content.txt").read_text() == "Fork PR changes\n"
    assert (
        env.run_command(["git", "rev-parse", "HEAD"], cwd=worktree_path).stdout.strip()
        == fork_commit
    )
    base = env.run_command(
        ["git", "config", "--get", "branch.forkowner-main.workmux-base"],
        cwd=repo_path,
    ).stdout.strip()
    assert base == "origin/main"
    diff = env.run_command(
        ["git", "diff", "--name-only", "origin/main...HEAD"], cwd=worktree_path
    ).stdout.strip()
    assert "fork-content.txt" in diff


def test_add_pr_fails_when_worktree_exists(
    mux_server, workmux_exe_path, remote_repo_path
):
    """Test error when trying to checkout same PR twice"""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    setup_pr_remote_and_branch(env, repo_path, remote_repo_path, "feature-branch")
    install_fake_pr_view(env)

    run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 123")

    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add --pr 123", expect_fail=True
    )

    assert result.exit_code != 0
    assert (
        "already exists" in result.stderr.lower() or "worktree" in result.stderr.lower()
    )


def test_add_pr_records_target_base_for_review(
    mux_server, workmux_exe_path, remote_repo_path
):
    """PR checkout must store the target branch, not the head, as review base."""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    feature_commit = setup_pr_remote_and_branch(
        env, repo_path, remote_repo_path, "feature-branch"
    )
    install_fake_pr_view(env, branch="feature-branch", base="main")

    result = run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 123")
    assert result.exit_code == 0, result.stderr

    worktree_path = get_worktree_path(repo_path, "feature-branch")
    assert (worktree_path / "pr-content.txt").read_text() == "PR change\n"
    head = env.run_command(
        ["git", "rev-parse", "HEAD"], cwd=worktree_path
    ).stdout.strip()
    assert head == feature_commit

    base = env.run_command(
        ["git", "config", "--get", "branch.feature-branch.workmux-base"],
        cwd=repo_path,
    ).stdout.strip()
    assert base == "origin/main"

    diff = env.run_command(
        ["git", "diff", "--name-only", "origin/main...HEAD"], cwd=worktree_path
    ).stdout.strip()
    assert "pr-content.txt" in diff


def test_add_pr_records_non_default_target_base(
    mux_server, workmux_exe_path, remote_repo_path
):
    """PRs targeting a non-default branch store that branch as the base."""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    feature_commit = setup_pr_remote_and_branch(
        env,
        repo_path,
        remote_repo_path,
        "topic/non-default",
        base_branch="release",
    )
    install_fake_pr_view(
        env, branch="topic/non-default", base="release", title="Target release"
    )

    result = run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 123")
    assert result.exit_code == 0, result.stderr

    worktree_path = get_worktree_path(repo_path, "topic/non-default")
    head = env.run_command(
        ["git", "rev-parse", "HEAD"], cwd=worktree_path
    ).stdout.strip()
    assert head == feature_commit

    base = env.run_command(
        ["git", "config", "--get", "branch.topic/non-default.workmux-base"],
        cwd=repo_path,
    ).stdout.strip()
    assert base == "origin/release"

    diff = env.run_command(
        ["git", "diff", "--name-only", "origin/release...HEAD"], cwd=worktree_path
    ).stdout.strip()
    # The change that landed on the release branch is part of the base, so only
    # the PR change appears in the review diff.
    assert "pr-content.txt" in diff
    assert "base-content.txt" not in diff


def test_add_pr_fetches_missing_target_ref(
    mux_server, workmux_exe_path, remote_repo_path
):
    """A target ref absent locally is fetched before it is stored as the base."""
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    feature_commit = setup_pr_remote_and_branch(
        env, repo_path, remote_repo_path, "feature-branch"
    )
    env.run_command(
        ["git", "update-ref", "-d", "refs/remotes/origin/main"], cwd=repo_path
    )
    missing = env.run_command(
        ["git", "rev-parse", "--verify", "--quiet", "origin/main"],
        cwd=repo_path,
        check=False,
    )
    assert missing.returncode != 0

    install_fake_pr_view(env, branch="feature-branch", base="main")

    result = run_workmux_command(env, workmux_exe_path, repo_path, "add --pr 123")
    assert result.exit_code == 0, result.stderr

    worktree_path = get_worktree_path(repo_path, "feature-branch")
    head = env.run_command(
        ["git", "rev-parse", "HEAD"], cwd=worktree_path
    ).stdout.strip()
    assert head == feature_commit
    fetched = env.run_command(
        ["git", "rev-parse", "origin/main"], cwd=worktree_path
    ).stdout.strip()
    remote_tip = env.run_command(
        ["git", "rev-parse", "main"], cwd=remote_repo_path
    ).stdout.strip()
    assert fetched == remote_tip
    base = env.run_command(
        ["git", "config", "--get", "branch.feature-branch.workmux-base"],
        cwd=repo_path,
    ).stdout.strip()
    assert base == "origin/main"
