"""Helpers for PR checkout tests."""

from pathlib import Path
from typing import Any

from ..conftest import MuxEnvironment, install_fake_gh_cli


def setup_pr_remote(
    env: MuxEnvironment,
    repo_path: Path,
    remote_repo_path: Path,
) -> None:
    github_url = "https://github.com/testowner/testrepo.git"

    env.run_command(
        ["git", "remote", "add", "origin", github_url],
        cwd=repo_path,
    )
    env.run_command(
        ["git", "remote", "set-url", "--push", "origin", str(remote_repo_path)],
        cwd=repo_path,
    )
    env.run_command(
        ["git", "config", f"url.{remote_repo_path}.insteadOf", github_url],
        cwd=repo_path,
    )
    env.run_command(["git", "push", "-u", "origin", "main"], cwd=repo_path)


def commit_file(
    env: MuxEnvironment,
    repo_path: Path,
    relative_path: str,
    content: str,
    message: str,
) -> str:
    """Commit a real file change and return the resulting commit SHA."""
    path = repo_path / relative_path
    path.write_text(content)
    env.run_command(["git", "add", relative_path], cwd=repo_path)
    env.run_command(["git", "commit", "-m", message], cwd=repo_path)
    return env.run_command(["git", "rev-parse", "HEAD"], cwd=repo_path).stdout.strip()


def setup_pr_remote_and_branch(
    env: MuxEnvironment,
    repo_path: Path,
    remote_repo_path: Path,
    branch_name: str,
    base_branch: str = "main",
) -> str:
    """Publish a feature branch with a real change and return its commit SHA.

    When ``base_branch`` is not ``main`` it is created and pushed first so the
    PR targets a non-default branch.
    """
    setup_pr_remote(env, repo_path, remote_repo_path)

    if base_branch != "main":
        env.run_command(["git", "checkout", "-b", base_branch], cwd=repo_path)
        commit_file(
            env,
            repo_path,
            "base-content.txt",
            f"{base_branch}\n",
            f"{base_branch} base commit",
        )
        env.run_command(["git", "push", "-u", "origin", base_branch], cwd=repo_path)

    env.run_command(["git", "checkout", "-b", branch_name], cwd=repo_path)
    commit = commit_file(
        env,
        repo_path,
        "pr-content.txt",
        "PR change\n",
        "PR changes",
    )
    env.run_command(["git", "push", "-u", "origin", branch_name], cwd=repo_path)
    env.run_command(["git", "checkout", base_branch], cwd=repo_path)
    env.run_command(["git", "branch", "-D", branch_name], cwd=repo_path)
    return commit


def pr_view_json(
    *,
    branch: str = "feature-branch",
    base: str = "main",
    owner: str = "testowner",
    state: str = "OPEN",
    draft: bool = False,
    title: str = "Add new feature",
    author: str = "contributor",
) -> dict[str, Any]:
    return {
        "headRefName": branch,
        "baseRefName": base,
        "headRepositoryOwner": {"login": owner},
        "state": state,
        "isDraft": draft,
        "title": title,
        "author": {"login": author},
    }


def install_fake_pr_view(
    env: MuxEnvironment,
    *,
    number: int = 123,
    branch: str = "feature-branch",
    base: str = "main",
    owner: str = "testowner",
    state: str = "OPEN",
    draft: bool = False,
    title: str = "Add new feature",
    author: str = "contributor",
) -> dict[str, Any]:
    pr_data = pr_view_json(
        branch=branch,
        base=base,
        owner=owner,
        state=state,
        draft=draft,
        title=title,
        author=author,
    )
    install_fake_gh_cli(env, pr_number=number, json_response=pr_data)
    return pr_data
