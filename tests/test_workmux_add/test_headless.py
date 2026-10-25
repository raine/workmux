"""Tests for mux-free worktree provisioning."""

import json
import os
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

import pytest
import yaml

from ..conftest import MuxEnvironment, write_workmux_config


def run_headless(
    env: MuxEnvironment,
    executable: Path,
    repo: Path,
    *args: str,
) -> subprocess.CompletedProcess[str]:
    process_env = env.env.copy()
    process_env.pop("TMUX", None)
    process_env.pop("TMUX_PANE", None)
    return subprocess.run(
        [str(executable), "add", *args, "--headless", "--json"],
        cwd=repo,
        env=process_env,
        capture_output=True,
        text=True,
        check=False,
    )


def test_headless_add_emits_json_and_creates_no_mux_target(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
):
    branch = "suba-headless-a1b2c3d4"
    source = mux_repo_path / ".env.local"
    source.write_text("TOKEN=test\n")
    write_workmux_config(
        mux_repo_path,
        files={"copy": [source.name]},
        post_create=["echo hook-output; touch hook-ran"],
    )
    windows_before = mux_server.list_window_ids()

    result = run_headless(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        branch,
        "--name",
        branch,
    )

    assert result.returncode == 0, result.stderr
    receipt = json.loads(result.stdout)
    assert receipt["schema_version"] == 1
    assert receipt["handle"] == branch
    assert receipt["branch"] == branch
    worktree = Path(receipt["worktree_path"])
    assert worktree.is_absolute()
    assert Path(receipt["working_directory"]).is_absolute()
    assert (worktree / source.name).read_text() == "TOKEN=test\n"
    assert (worktree / "hook-ran").exists()
    assert "hook-output" in result.stderr
    assert mux_server.list_window_ids() == windows_before
    attachment = mux_server.run_command(
        [
            "git",
            "config",
            "--local",
            "--get",
            f"workmux.worktree.{branch}.attachment",
        ],
        cwd=mux_repo_path,
    )
    assert attachment.stdout.strip() == "headless"


def test_headless_config_outside_repo_uses_git_project_root(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    tmp_path: Path,
):
    branch = "external-config-root-a1b2c3d4"
    external_dir = tmp_path.parent / f"{tmp_path.name}-config"
    external_dir.mkdir()
    config_path = external_dir / "alternate.yaml"
    hook_env_path = external_dir / "hook-env.txt"
    git_root_path = external_dir / "git-root.txt"
    source_name = "config-source.txt"
    (external_dir / source_name).write_text("external config source\n")
    (mux_repo_path / source_name).write_text("project source\n")
    hook = (
        "printf '%s\\n%s\\n%s\\n%s\\n' "
        '"$WM_PROJECT_ROOT" "$WM_CONFIG_DIR" "$WM_WORKTREE_PATH" "$PWD" '
        f"> {shlex.quote(str(hook_env_path))}; "
        'git -C "$WM_PROJECT_ROOT" rev-parse --show-toplevel '
        f"> {shlex.quote(str(git_root_path))}"
    )
    config_path.write_text(
        yaml.safe_dump(
            {
                "files": {"copy": [source_name]},
                "post_create": [hook],
            }
        )
    )

    result = run_headless(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        branch,
        "--name",
        branch,
        "--config",
        str(config_path),
    )

    assert result.returncode == 0, result.stderr
    receipt = json.loads(result.stdout)
    worktree = Path(receipt["worktree_path"])
    project_root, config_dir, worktree_path, hook_cwd = map(
        Path, hook_env_path.read_text().splitlines()
    )
    assert project_root.resolve() == mux_repo_path.resolve()
    assert Path(git_root_path.read_text().strip()).resolve() == mux_repo_path.resolve()
    assert config_dir.resolve() == worktree.resolve()
    assert worktree_path.resolve() == worktree.resolve()
    assert hook_cwd.resolve() == worktree.resolve()
    assert (worktree / source_name).read_text() == "external config source\n"


def test_headless_add_rolls_back_failed_provisioning(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
):
    branch = "suba-failed-a1b2c3d4"
    write_workmux_config(mux_repo_path, post_create=["echo failed-hook; exit 7"])

    result = run_headless(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        branch,
        "--name",
        branch,
    )

    assert result.returncode != 0
    assert result.stdout == ""
    assert "failed-hook" in result.stderr
    worktrees = mux_server.run_command(
        ["git", "worktree", "list", "--porcelain"], cwd=mux_repo_path
    )
    branches = mux_server.run_command(
        ["git", "branch", "--list", branch], cwd=mux_repo_path
    )
    assert branch not in worktrees.stdout
    assert branch not in branches.stdout
    metadata = mux_server.run_command(
        [
            "git",
            "config",
            "--local",
            "--get-regexp",
            f"^workmux\\.worktree\\.{branch}\\.",
        ],
        check=False,
        cwd=mux_repo_path,
    )
    assert metadata.returncode != 0


def test_headless_rejects_mux_flags_before_creating_worktree(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
):
    branch = "suba-invalid-a1b2c3d4"
    result = run_headless(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        branch,
        "--name",
        branch,
        "--background",
    )

    assert result.returncode != 0
    assert "cannot be used with --headless" in result.stderr
    branches = mux_server.run_command(
        ["git", "branch", "--list", branch], cwd=mux_repo_path
    )
    assert branches.stdout.strip() == ""


def test_headless_rejects_remote_branch_syntax(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
):
    mux_server.run_command(
        ["git", "remote", "add", "upstream", str(mux_repo_path)], cwd=mux_repo_path
    )
    result = run_headless(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "upstream/feature",
        "--name",
        "suba-remote-a1b2c3d4",
    )

    assert result.returncode != 0
    assert "does not resolve remote branch" in result.stderr
    branches = mux_server.run_command(
        ["git", "branch", "--list", "upstream/feature"], cwd=mux_repo_path
    )
    assert branches.stdout.strip() == ""


def test_headless_rename_restores_attachment_when_move_fails(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    tmp_path: Path,
):
    branch = "suba-rename-failure-a1b2c3d4"
    renamed = "suba-rename-failure-new-a1b2c3d4"
    result = run_headless(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        branch,
        "--name",
        branch,
        "--no-hooks",
        "--no-file-ops",
    )
    assert result.returncode == 0, result.stderr
    receipt = json.loads(result.stdout)

    real_git = shutil.which("git")
    assert real_git is not None
    wrapper_dir = tmp_path / "bin"
    wrapper_dir.mkdir()
    wrapper = wrapper_dir / "git"
    wrapper.write_text(
        f"""#!{sys.executable}
import os
import sys

for index in range(len(sys.argv)):
    if sys.argv[index : index + 2] == ["worktree", "move"]:
        print("forced worktree move failure", file=sys.stderr)
        raise SystemExit(1)
os.execv({real_git!r}, [{real_git!r}, *sys.argv[1:]])
"""
    )
    wrapper.chmod(0o755)

    process_env = mux_server.env.copy()
    process_env["PATH"] = f"{wrapper_dir}{os.pathsep}{process_env['PATH']}"
    process_env.pop("TMUX", None)
    process_env.pop("TMUX_PANE", None)
    failed = subprocess.run(
        [str(workmux_exe_path), "rename", branch, renamed],
        cwd=mux_repo_path,
        env=process_env,
        capture_output=True,
        text=True,
        check=False,
    )

    assert failed.returncode != 0
    assert "forced worktree move failure" in failed.stderr
    assert Path(receipt["worktree_path"]).exists()
    old_attachment = mux_server.run_command(
        [
            "git",
            "config",
            "--local",
            "--get",
            f"workmux.worktree.{branch}.attachment",
        ],
        cwd=mux_repo_path,
    )
    assert old_attachment.stdout.strip() == "headless"
    new_attachment = mux_server.run_command(
        [
            "git",
            "config",
            "--local",
            "--get",
            f"workmux.worktree.{renamed}.attachment",
        ],
        check=False,
        cwd=mux_repo_path,
    )
    assert new_attachment.returncode != 0


def test_headless_remove_does_not_close_same_named_window(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
):
    branch = "suba-collision-a1b2c3d4"
    result = run_headless(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        branch,
        "--name",
        branch,
        "--no-hooks",
        "--no-file-ops",
    )
    assert result.returncode == 0, result.stderr

    same_name = f"wm-{branch}"
    mux_server.mux_command(["new-window", "-d", "-n", same_name])
    mux_server.run_command(
        [
            "git",
            "config",
            "--local",
            f"workmux.worktree.{branch}.attachment",
            "future-value",
        ],
        cwd=mux_repo_path,
    )
    process_env = mux_server.env.copy()
    process_env.pop("TMUX", None)
    process_env.pop("TMUX_PANE", None)
    removed = subprocess.run(
        [str(workmux_exe_path), "remove", "--force", branch],
        cwd=mux_repo_path,
        env=process_env,
        capture_output=True,
        text=True,
        check=False,
    )

    assert removed.returncode == 0, removed.stderr
    assert same_name in mux_server.list_windows()


@pytest.mark.parametrize("layout", ["symlink", "dangling", "hardlink", "directory"])
@pytest.mark.parametrize("linked_source", [False, True])
def test_headless_add_accepts_linked_git_hooks(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    layout: str,
    linked_source: bool,
):
    source = mux_repo_path
    if linked_source:
        source = mux_repo_path / "linked-source"
        subprocess.run(
            ["git", "worktree", "add", "-b", "linked-source", str(source)],
            cwd=mux_repo_path,
            env=mux_server.env,
            check=True,
            capture_output=True,
        )
    target = mux_repo_path / "hook-target"
    marker = mux_repo_path / "hook-ran"
    target.write_text(f"#!/bin/sh\ntouch '{marker}'\n")
    target.chmod(0o755)
    hooks = mux_repo_path / ".git" / "hooks"
    if layout == "directory":
        shutil.rmtree(hooks)
        external = mux_repo_path / "external-hooks"
        external.mkdir()
        hooks.symlink_to(external, target_is_directory=True)
    for name in ["pre-commit", "prepare-commit-msg", "post-checkout"]:
        hook = hooks / name
        if layout == "symlink":
            hook.symlink_to("../../hook-target")
        elif layout == "dangling":
            hook.symlink_to("../../missing-hook")
        elif layout == "hardlink":
            os.link(target, hook)
        else:
            shutil.copy2(target, hook)

    result = run_headless(
        mux_server, workmux_exe_path, source, "linked-hooks-probe", "--no-hooks"
    )
    assert result.returncode == 0, result.stderr
    receipt = json.loads(result.stdout)
    assert Path(receipt["worktree_path"]).is_dir()
    assert not marker.exists(), "Git hooks must not execute during worktree creation"
