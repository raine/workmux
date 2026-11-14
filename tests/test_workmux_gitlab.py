"""Integration tests for GitLab merge request checkout with ``workmux add``."""

import json
import shutil
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import Any

import pytest

from .conftest import (
    get_window_name,
    get_worktree_path,
    run_workmux_command,
    setup_git_repo,
)


def setup_gitlab_origin(env, repo_path: Path, remote_path: Path, url: str) -> None:
    """Configure a public GitLab identity backed by a local bare repository."""
    env.run_command(["git", "remote", "add", "origin", url], cwd=repo_path)
    env.run_command(
        ["git", "remote", "set-url", "--push", "origin", str(remote_path)],
        cwd=repo_path,
    )
    env.run_command(
        ["git", "config", f"url.{remote_path}.insteadOf", url], cwd=repo_path
    )
    env.run_command(["git", "push", "-u", "origin", "main"], cwd=repo_path)


def create_merge_request_ref(
    env,
    repo_path: Path,
    number: int,
    source_branch: str,
    marker: str,
) -> str:
    """Publish a commit only through GitLab's merge request head ref."""
    env.run_command(["git", "checkout", "-b", source_branch], cwd=repo_path)
    marker_path = repo_path / "merge-request-content.txt"
    marker_path.write_text(marker)
    env.run_command(["git", "add", marker_path.name], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Merge request changes"], cwd=repo_path)
    commit = env.run_command(["git", "rev-parse", "HEAD"], cwd=repo_path).stdout.strip()
    env.run_command(
        [
            "git",
            "push",
            "origin",
            f"{commit}:refs/merge-requests/{number}/head",
        ],
        cwd=repo_path,
    )
    env.run_command(["git", "checkout", "main"], cwd=repo_path)
    env.run_command(["git", "branch", "-D", source_branch], cwd=repo_path)
    return commit


def merge_request_json(
    *,
    number: int,
    source_branch: str,
    target_branch: str = "main",
    source_project_id: int = 100,
    target_project_id: int = 100,
    title: str = "Review GitLab changes",
    repository_url: str = "https://gitlab.com/acme/widgets",
) -> dict[str, Any]:
    return {
        "iid": number,
        "web_url": f"{repository_url}/-/merge_requests/{number}",
        "title": title,
        "source_branch": source_branch,
        "target_branch": target_branch,
        "source_project_id": source_project_id,
        "target_project_id": target_project_id,
        "state": "opened",
        "draft": False,
        "author": {"username": "gitlab-contributor"},
    }


def install_fake_glab(
    env, calls: Sequence[tuple[list[str], dict[str, Any] | str, int]]
):
    """Install a strict Python glab fake and return its invocation log."""
    script_path = env.fake_bin_dir / "glab"
    log_path = env.tmp_path / "glab-calls.jsonl"
    expected_origin = env.run_command(
        ["git", "config", "--get", "remote.origin.url"]
    ).stdout.strip()
    specifications = [
        {
            "args": args,
            "stdout": json.dumps(response) if isinstance(response, dict) else "",
            "stderr": response if isinstance(response, str) else "",
            "exit_code": exit_code,
        }
        for args, response, exit_code in calls
    ]
    env.install_script(
        script_path,
        f"""#!{sys.executable}
import json
import pathlib
import sys
import os
import subprocess

assert os.environ.get("GLAB_NO_PROMPT") == "1"
assert os.environ.get("NO_PROMPT") == "1"
assert os.environ.get("GLAB_ENABLE_CI_AUTOLOGIN") == "false"
assert not any(key in os.environ for key in ("GITLAB_REPO", "GITLAB_HOST", "GITLAB_URI", "GL_HOST"))
assert subprocess.check_output(["git", "remote"], text=True).strip() == "origin"
assert subprocess.check_output(["git", "config", "--get", "remote.origin.url"], text=True).strip() == {expected_origin!r}
assert pathlib.Path.cwd() != pathlib.Path({str(env.tmp_path)!r})
pathlib.Path({str(log_path) + ".context"!r}).write_text(str(pathlib.Path.cwd()))

log_path = pathlib.Path({str(log_path)!r})
specifications = json.loads({json.dumps(specifications)!r})
previous = log_path.read_text().splitlines() if log_path.exists() else []
args = sys.argv[1:]
with log_path.open("a") as log:
    log.write(json.dumps(args) + "\\n")
if len(previous) >= len(specifications):
    print(f"unexpected glab call: {{args!r}}", file=sys.stderr)
    raise SystemExit(97)
specification = specifications[len(previous)]
if args != specification["args"]:
    print(
        f"unexpected glab arguments: {{args!r}}; expected {{specification['args']!r}}",
        file=sys.stderr,
    )
    raise SystemExit(98)
if specification["stdout"]:
    print(specification["stdout"])
if specification["stderr"]:
    print(specification["stderr"], file=sys.stderr)
raise SystemExit(specification["exit_code"])
""",
    )
    return log_path


def assert_glab_calls(log_path: Path, expected: list[list[str]]) -> None:
    calls = [json.loads(line) for line in log_path.read_text().splitlines()]
    assert calls == expected
    assert not Path(Path(str(log_path) + ".context").read_text()).exists()


@pytest.mark.parametrize(
    "origin_url",
    [
        "https://gitlab.com/acme/widgets",
        "https://oauth2:example-token@gitlab.com/acme/widgets.git",
        "gitlab.com:acme/widgets.git",
    ],
)
def test_add_gitlab_numeric_uses_origin_and_merge_request_ref(
    mux_server, workmux_exe_path, remote_repo_path, origin_url
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)
    setup_gitlab_origin(env, repo_path, remote_repo_path, origin_url)
    env.run_command(
        [
            "git",
            "remote",
            "add",
            "upstream",
            "https://github.com/unrelated/project.git",
        ],
        cwd=repo_path,
    )
    expected_commit = create_merge_request_ref(
        env, repo_path, 123, "feature/gitlab-review", "content from MR ref\n"
    )
    view_args = [
        "mr",
        "view",
        "123",
        "--output",
        "json",
    ]
    log_path = install_fake_glab(
        env,
        [
            (
                view_args,
                merge_request_json(number=123, source_branch="feature/gitlab-review"),
                0,
            )
        ],
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add --pr 123",
        pre_run_env={
            "GITLAB_REPO": "other/project",
            "GITLAB_HOST": "other.example.com",
            "GITLAB_URI": "other.example.com",
            "GL_HOST": "other.example.com",
            "GLAB_ENABLE_CI_AUTOLOGIN": "true",
        },
    )

    worktree_path = get_worktree_path(repo_path, "feature/gitlab-review")
    assert worktree_path.exists(), result.stderr
    assert (worktree_path / "merge-request-content.txt").read_text() == (
        "content from MR ref\n"
    )
    assert (
        env.run_command(["git", "rev-parse", "HEAD"], cwd=worktree_path).stdout.strip()
        == expected_commit
    )
    base = env.run_command(
        ["git", "config", "--get", "branch.feature/gitlab-review.workmux-base"],
        cwd=repo_path,
    ).stdout.strip()
    assert base == "origin/main"
    assert get_window_name("feature/gitlab-review") in env.list_windows()
    assert_glab_calls(log_path, [view_args])


def test_add_gitlab_self_hosted_numeric_with_forge_and_custom_name(
    mux_server, workmux_exe_path, remote_repo_path
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)
    origin_url = "https://git.corp.example/platform/services/widgets"
    setup_gitlab_origin(env, repo_path, remote_repo_path, origin_url)
    create_merge_request_ref(env, repo_path, 47, "topic/server-fix", "self hosted\n")
    view_args = [
        "mr",
        "view",
        "47",
        "--output",
        "json",
    ]
    log_path = install_fake_glab(
        env,
        [
            (
                view_args,
                merge_request_json(
                    number=47,
                    source_branch="topic/server-fix",
                    repository_url=origin_url,
                ),
                0,
            )
        ],
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add corp-review --pr 47 --forge gitlab",
    )

    worktree_path = get_worktree_path(repo_path, "corp-review")
    assert worktree_path.exists(), result.stderr
    assert (worktree_path / "merge-request-content.txt").read_text() == "self hosted\n"
    assert get_window_name("corp-review") in env.list_windows()
    assert_glab_calls(log_path, [view_args])


@pytest.mark.parametrize(
    "git_url, origin_url",
    [
        (
            "https://gitlab.example.test/group/subgroup/deep/project",
            "https://gitlab.example.test/group/subgroup/deep/project",
        ),
        (
            "git@gitlab.example.test:group/subgroup/deep/project.git",
            "https://gitlab.example.test/group/subgroup/deep/project",
        ),
        (
            "git@ssh.gitlab.example.test:group/subgroup/deep/project.git",
            "https://gitlab.example.test/group/subgroup/deep/project",
        ),
        (
            "git@corp-alias:group/subgroup/deep/project.git",
            "http://gitlab.example.test:8080/group/subgroup/deep/project",
        ),
    ],
)
def test_add_gitlab_nested_group_merge_request_url_matches_origin(
    mux_server, workmux_exe_path, remote_repo_path, git_url, origin_url
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)
    setup_gitlab_origin(env, repo_path, remote_repo_path, git_url)
    create_merge_request_ref(env, repo_path, 902, "fix/nested-group", "nested group\n")
    view_args = [
        "mr",
        "view",
        "902",
        "--output",
        "json",
    ]
    log_path = install_fake_glab(
        env,
        [
            (
                view_args,
                merge_request_json(
                    number=902,
                    source_branch="fix/nested-group",
                    repository_url=origin_url,
                ),
                0,
            )
        ],
    )
    mr_url = f"{origin_url}/-/merge_requests/902"

    result = run_workmux_command(env, workmux_exe_path, repo_path, f"add --pr {mr_url}")

    worktree_path = get_worktree_path(repo_path, "fix/nested-group")
    assert worktree_path.exists(), result.stderr
    assert (worktree_path / "merge-request-content.txt").read_text() == "nested group\n"
    assert_glab_calls(log_path, [view_args])


def test_add_gitlab_url_rejects_repository_mismatch(
    mux_server, workmux_exe_path, remote_repo_path
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)
    setup_gitlab_origin(
        env,
        repo_path,
        remote_repo_path,
        "https://gitlab.com/acme/expected-project",
    )

    install_fake_glab(
        env,
        [
            (
                ["mr", "view", "12", "--output", "json"],
                merge_request_json(
                    number=12,
                    source_branch="feature",
                    repository_url="https://gitlab.com/acme/expected-project",
                ),
                0,
            )
        ],
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add --pr https://gitlab.com/acme/other-project/-/merge_requests/12",
        expect_fail=True,
    )

    assert result.exit_code != 0
    assert "origin" in result.stderr.lower()
    assert "match" in result.stderr.lower()


@pytest.mark.parametrize("forge", ["github", "gitlab"])
def test_add_forge_requires_pr(mux_server, workmux_exe_path, forge):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add ordinary-branch --forge {forge}",
        expect_fail=True,
    )

    assert result.exit_code != 0
    assert "--pr" in result.stderr


@pytest.mark.parametrize(
    ("forge", "review_url"),
    [
        ("github", "https://gitlab.com/acme/widgets/-/merge_requests/123"),
        ("gitlab", "https://github.com/acme/widgets/pull/123"),
    ],
)
def test_add_rejects_forge_that_mismatches_review_url(
    mux_server, workmux_exe_path, forge, review_url
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)

    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add --pr {review_url} --forge {forge}",
        expect_fail=True,
    )

    assert result.exit_code != 0
    assert "forge" in result.stderr.lower()
    assert any(word in result.stderr.lower() for word in ("conflict", "match"))


@pytest.mark.parametrize("use_mr_ref", [False, True])
@pytest.mark.parametrize("host", ["gitlab.com", "gitlab.example.com:8443"])
def test_add_gitlab_fork_uses_project_remote_and_source_branch(
    mux_server, workmux_exe_path, remote_repo_path, use_mr_ref, host
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)
    origin_url = f"https://{host}/acme/widgets"
    setup_gitlab_origin(env, repo_path, remote_repo_path, origin_url)

    fork_repo_path = repo_path.parent / f"{remote_repo_path.name}-different-project.git"
    env.run_command(
        ["git", "clone", "--bare", str(remote_repo_path), str(fork_repo_path)]
    )
    fork_work_path = repo_path.parent / f"{remote_repo_path.name}-fork-work"
    env.run_command(["git", "clone", str(fork_repo_path), str(fork_work_path)])
    env.run_command(["git", "config", "user.name", "Fork User"], cwd=fork_work_path)
    env.run_command(
        ["git", "config", "user.email", "fork@example.com"], cwd=fork_work_path
    )
    env.run_command(["git", "checkout", "-b", "feature/new-ui"], cwd=fork_work_path)
    (fork_work_path / "fork-content.txt").write_text("from different project\n")
    env.run_command(["git", "add", "fork-content.txt"], cwd=fork_work_path)
    env.run_command(["git", "commit", "-m", "Fork changes"], cwd=fork_work_path)
    env.run_command(["git", "push", "origin", "feature/new-ui"], cwd=fork_work_path)

    if use_mr_ref:
        env.run_command(
            [
                "git",
                "push",
                str(remote_repo_path),
                "feature/new-ui:refs/merge-requests/321/head",
            ],
            cwd=fork_work_path,
        )
        env.run_command(
            ["git", "push", "origin", "--delete", "feature/new-ui"], cwd=fork_work_path
        )

    fork_https_url = f"https://{host}/contributors/different-project.git"
    fork_ssh_url = "git@gitlab.com:contributors/different-project.git"
    env.run_command(
        ["git", "config", "--add", f"url.{fork_repo_path}.insteadOf", fork_https_url],
        cwd=repo_path,
    )
    env.run_command(
        ["git", "config", "--add", f"url.{fork_repo_path}.insteadOf", fork_ssh_url],
        cwd=repo_path,
    )

    if not use_mr_ref:
        env.run_command(
            ["git", "remote", "add", "gitlab-200", fork_https_url], cwd=repo_path
        )
        env.run_command(
            [
                "git",
                "config",
                "remote.gitlab-200.fetch",
                "+refs/heads/main:refs/remotes/gitlab-200/main",
            ],
            cwd=repo_path,
        )

    view_args = [
        "mr",
        "view",
        "321",
        "--output",
        "json",
    ]
    api_args = ["api", "projects/200"]
    source_project = {
        "ssh_url_to_repo": fork_ssh_url,
        "http_url_to_repo": fork_https_url,
        "path_with_namespace": "contributors/different-project",
    }
    log_path = install_fake_glab(
        env,
        [
            (
                view_args,
                merge_request_json(
                    number=321,
                    source_branch="feature/new-ui",
                    source_project_id=200,
                    target_project_id=100,
                    repository_url=origin_url,
                ),
                0,
            ),
            (api_args, source_project, 0),
            (
                view_args,
                merge_request_json(
                    number=321,
                    source_branch="feature/new-ui",
                    source_project_id=200,
                    target_project_id=100,
                    repository_url=origin_url,
                ),
                0,
            ),
            (api_args, source_project, 0),
        ],
    )

    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add --pr 321 --forge gitlab"
    )

    local_branch = "gitlab-200-feature/new-ui"
    worktree_path = get_worktree_path(repo_path, local_branch)
    assert worktree_path.exists(), result.stderr
    assert (worktree_path / "fork-content.txt").read_text() == (
        "from different project\n"
    )
    remotes = env.run_command(["git", "remote"], cwd=repo_path).stdout.splitlines()
    assert "gitlab-200" in remotes
    source_remote_url = env.run_command(
        ["git", "config", "--get", "remote.gitlab-200.url"], cwd=repo_path
    ).stdout.strip()
    assert source_remote_url in {fork_ssh_url, fork_https_url}
    assert_glab_calls(log_path, [view_args, api_args])

    env.run_command(
        ["git", "remote", "rename", "gitlab-200", "contributor"], cwd=repo_path
    )
    second_result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add reused-fork --pr 321 --forge gitlab"
    )

    assert get_worktree_path(repo_path, "reused-fork").exists(), second_result.stderr
    remotes = env.run_command(["git", "remote"], cwd=repo_path).stdout.splitlines()
    assert "contributor" in remotes
    assert "gitlab-200" not in remotes
    assert_glab_calls(log_path, [view_args, api_args, view_args, api_args])


@pytest.mark.parametrize("is_fork", [False, True])
def test_add_gitlab_dry_run_reads_metadata_without_mutation(
    mux_server, workmux_exe_path, remote_repo_path, is_fork
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)
    origin_url = "https://gitlab.com/acme/widgets"
    setup_gitlab_origin(env, repo_path, remote_repo_path, origin_url)
    view_args = [
        "mr",
        "view",
        "88",
        "--output",
        "json",
    ]
    calls = [
        (
            view_args,
            merge_request_json(
                number=88,
                source_branch="topic/dry-run",
                source_project_id=200 if is_fork else 100,
            ),
            0,
        )
    ]
    api_args = ["api", "projects/200"]
    if is_fork:
        calls.append(
            (
                api_args,
                {
                    "ssh_url_to_repo": "git@gitlab.com:contributors/fork.git",
                    "http_url_to_repo": "https://gitlab.com/contributors/fork.git",
                },
                0,
            )
        )
    log_path = install_fake_glab(env, calls)
    remotes_before = env.run_command(["git", "remote", "-v"], cwd=repo_path).stdout
    refs_before = env.run_command(
        ["git", "for-each-ref", "--format=%(refname) %(objectname)"], cwd=repo_path
    ).stdout
    worktrees_before = env.run_command(
        ["git", "worktree", "list", "--porcelain"], cwd=repo_path
    ).stdout
    windows_before = env.list_windows()

    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add --pr 88 --dry-run"
    )

    assert "topic/dry-run" in result.stdout
    assert (
        env.run_command(["git", "remote", "-v"], cwd=repo_path).stdout == remotes_before
    )
    assert (
        env.run_command(
            ["git", "for-each-ref", "--format=%(refname) %(objectname)"],
            cwd=repo_path,
        ).stdout
        == refs_before
    )
    assert (
        env.run_command(
            ["git", "worktree", "list", "--porcelain"], cwd=repo_path
        ).stdout
        == worktrees_before
    )
    windows_after = env.list_windows()
    assert len(windows_after) == len(windows_before)
    assert get_window_name("topic/dry-run") not in windows_after
    assert not get_worktree_path(repo_path, "topic/dry-run").exists()
    assert_glab_calls(log_path, [view_args, api_args] if is_fork else [view_args])


def test_add_gitlab_reports_glab_command_failure(
    mux_server, workmux_exe_path, remote_repo_path
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)
    origin_url = "https://gitlab.com/acme/widgets"
    setup_gitlab_origin(env, repo_path, remote_repo_path, origin_url)
    view_args = [
        "mr",
        "view",
        "404",
        "--output",
        "json",
    ]
    log_path = install_fake_glab(env, [(view_args, "merge request not found", 1)])

    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add --pr 404", expect_fail=True
    )

    assert result.exit_code != 0
    assert "merge request not found" in result.stderr
    assert_glab_calls(log_path, [view_args])


def test_add_gitlab_reports_missing_glab(
    mux_server, workmux_exe_path, remote_repo_path
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)
    setup_gitlab_origin(
        env, repo_path, remote_repo_path, "https://gitlab.com/acme/widgets"
    )
    isolated_bin = repo_path.parent / "git-only-bin"
    isolated_bin.mkdir()
    for executable in ("git", env.backend_name):
        executable_path = shutil.which(executable)
        assert executable_path is not None
        (isolated_bin / executable).symlink_to(executable_path)

    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add --pr 123",
        expect_fail=True,
        pre_run_env={"PATH": str(isolated_bin)},
    )

    assert result.exit_code != 0
    assert "glab" in result.stderr.lower()


@pytest.mark.parametrize(
    "invalid_field, value, message",
    [
        ("iid", 999, "instead of"),
        ("source_branch", "invalid:branch", "invalid source branch"),
        ("source_project_id", None, "source project was deleted"),
        ("source_project_id", 0, "source project was deleted"),
    ],
)
def test_add_gitlab_rejects_unusable_metadata(
    mux_server, workmux_exe_path, remote_repo_path, invalid_field, value, message
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)
    url = "https://gitlab.com/acme/widgets"
    setup_gitlab_origin(env, repo_path, remote_repo_path, url)
    view_args = ["mr", "view", "123", "--output", "json"]
    metadata = merge_request_json(number=123, source_branch="feature")
    metadata[invalid_field] = value
    install_fake_glab(env, [(view_args, metadata, 0)])
    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add --pr 123 --dry-run", expect_fail=True
    )
    assert message in result.stderr


def test_add_gitlab_preserves_conflicting_remote(
    mux_server, workmux_exe_path, remote_repo_path
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)
    url = "https://gitlab.com/acme/widgets"
    setup_gitlab_origin(env, repo_path, remote_repo_path, url)
    conflicting_url = "https://gitlab.com/unrelated/project.git"
    env.run_command(
        ["git", "remote", "add", "gitlab-200", conflicting_url], cwd=repo_path
    )
    view_args = ["mr", "view", "123", "--output", "json"]
    api_args = ["api", "projects/200"]
    install_fake_glab(
        env,
        [
            (
                view_args,
                merge_request_json(
                    number=123, source_branch="feature", source_project_id=200
                ),
                0,
            ),
            (
                api_args,
                {
                    "ssh_url_to_repo": "git@gitlab.com:author/fork.git",
                    "http_url_to_repo": "https://gitlab.com/author/fork.git",
                },
                0,
            ),
        ],
    )
    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add --pr 123", expect_fail=True
    )
    assert "already points to a different repository" in result.stderr
    assert (
        env.run_command(
            ["git", "config", "--get", "remote.gitlab-200.url"], cwd=repo_path
        ).stdout.strip()
        == conflicting_url
    )


def test_add_gitlab_rejects_stale_source_ref(
    mux_server, workmux_exe_path, remote_repo_path
):
    env = mux_server
    repo_path = env.tmp_path
    setup_git_repo(repo_path, env.env)
    url = "https://gitlab.com/acme/widgets"
    setup_gitlab_origin(env, repo_path, remote_repo_path, url)
    env.run_command(
        ["git", "update-ref", "refs/remotes/origin/deleted-feature", "HEAD"],
        cwd=repo_path,
    )
    env.run_command(["git", "config", "fetch.prune", "false"], cwd=repo_path)
    view_args = ["mr", "view", "123", "--output", "json"]
    install_fake_glab(
        env,
        [
            (
                view_args,
                merge_request_json(number=123, source_branch="deleted-feature"),
                0,
            )
        ],
    )
    result = run_workmux_command(
        env, workmux_exe_path, repo_path, "add --pr 123", expect_fail=True
    )
    assert result.exit_code != 0
    assert not get_worktree_path(repo_path, "deleted-feature").exists()
