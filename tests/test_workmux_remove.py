import json
import os
import shlex
import shutil
import uuid
from pathlib import Path

import pytest

from .conftest import (
    DEFAULT_WINDOW_PREFIX,
    MuxEnvironment,
    TmuxEnvironment,
    create_commit,
    create_dirty_file,
    get_scripts_dir,
    get_window_name,
    get_worktree_path,
    poll_until,
    run_workmux_add,
    run_workmux_command,
    run_workmux_remove,
    write_workmux_config,
)
from .support.worktrees import (
    add_worktrees,
    assert_branches_deleted,
    assert_branches_present,
    assert_windows_closed,
    assert_worktrees_exist,
    assert_worktrees_removed,
)


@pytest.mark.parametrize("force,collision", [(True, False), (False, True)])
def test_remove_detached_worktree(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    force: bool,
    collision: bool,
):
    env = mux_server
    handle = "detached-tree"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, handle)
    tree = get_worktree_path(mux_repo_path, handle)
    env.run_command(["git", "checkout", "--detach"], cwd=tree)
    env.run_command(
        ["git", "config", f"workmux.worktree.{handle}.attachment", "headless"],
        cwd=mux_repo_path,
    )
    if collision:
        # An unmerged branch with the display label must neither prompt nor be deleted.
        commit = env.run_command(
            ["git", "commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "unmerged"],
            cwd=mux_repo_path,
        ).stdout.strip()
        env.run_command(["git", "branch", "(detached)", commit], cwd=mux_repo_path)

    flags = "--force " if force else ""
    result = run_workmux_command(
        env, workmux_exe_path, mux_repo_path, f"remove {flags}{handle}"
    )
    assert "(detached)" not in result.stdout
    pending = Path(env.env["XDG_STATE_HOME"]) / "workmux" / "pending-cleanup"
    assert poll_until(
        lambda: not tree.exists()
        and not list(tree.parent.glob(f".workmux_trash_{handle}_*"))
        and not list(pending.glob("*.json")),
        timeout=10,
    )
    assert (
        str(tree)
        not in env.run_command(
            ["git", "worktree", "list", "--porcelain"], cwd=mux_repo_path
        ).stdout
    )
    assert (
        env.run_command(
            ["git", "config", "--get-regexp", f"^workmux\\.worktree\\.{handle}\\."],
            cwd=mux_repo_path,
            check=False,
        ).returncode
        != 0
    )
    assert_branches_present(env, mux_repo_path, [handle])
    if collision:
        assert_branches_present(env, mux_repo_path, ["(detached)"])


def test_remove_branch_named_detached(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    handle = "named-detached"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, handle)
    tree = get_worktree_path(mux_repo_path, handle)
    env.run_command(["git", "branch", "-m", "(detached)"], cwd=tree)
    run_workmux_remove(env, workmux_exe_path, mux_repo_path, handle, force=True)
    assert not tree.exists()
    assert_branches_deleted(env, mux_repo_path, ["(detached)"])


@pytest.mark.tmux_only
def test_remove_detached_from_own_window(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    handle = "deferred-detached"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, handle)
    tree = get_worktree_path(mux_repo_path, handle)
    env.run_command(["git", "checkout", "--detach"], cwd=tree)
    env.run_command(["git", "branch", "(detached)"], cwd=mux_repo_path)
    output = env.tmp_path / "detached-removal-output"
    env.send_keys(
        get_window_name(handle),
        f"cd {shlex.quote(str(tree))} && "
        f"{shlex.quote(str(workmux_exe_path))} remove --force {handle} "
        f">{shlex.quote(str(output))} 2>&1",
    )
    assert poll_until(
        lambda: output.exists() and "Scheduled removal" in output.read_text()
    )
    assert "(detached)" not in output.read_text()
    pending = Path(env.env["XDG_STATE_HOME"]) / "workmux" / "pending-cleanup"
    assert poll_until(
        lambda: not tree.exists()
        and not list(tree.parent.glob(f".workmux_trash_{handle}_*"))
        and not list(pending.glob("*.json")),
        timeout=10,
    )
    assert_branches_present(env, mux_repo_path, [handle, "(detached)"])
    assert get_window_name(handle) not in env.list_windows()


def test_remove_clean_branch_succeeds_without_prompt(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove` on a branch with no unmerged commits succeeds without a prompt."""
    env = mux_server
    branch_name = "clean-branch"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path)

    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    assert worktree_path.is_dir()
    env.run_command(
        [
            "git",
            "config",
            "--local",
            "--remove-section",
            f"workmux.worktree.{branch_name}",
        ],
        cwd=mux_repo_path,
    )

    # This should succeed without any user input because the branch has no new commits
    run_workmux_remove(env, workmux_exe_path, mux_repo_path, branch_name, force=False)

    assert not worktree_path.exists()
    assert window_name not in env.list_windows()
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name not in branch_list_result.stdout


@pytest.mark.tmux_only
def test_remove_adopts_window_recreated_by_tmux_restore(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "restored-remove"
    window_name = get_window_name(branch_name)

    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    env.kill_window(window_name)
    restored_id = env.tmux(
        [
            "new-window",
            "-d",
            "-n",
            window_name,
            "-c",
            str(worktree_path),
            "-P",
            "-F",
            "#{window_id}",
        ]
    ).stdout.strip()

    run_workmux_remove(env, workmux_exe_path, mux_repo_path, branch_name, force=True)

    window_ids = env.tmux(
        ["list-windows", "-a", "-F", "#{window_id}"], check=False
    ).stdout.splitlines()
    assert restored_id not in window_ids
    assert not worktree_path.exists()


@pytest.mark.tmux_only
def test_remove_window_in_parent_session(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature/parent-session-remove"
    session_name = "prs-remove"
    window_name = get_window_name(branch_name)

    write_workmux_config(mux_repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"add {branch_name} --parent-session {session_name} --background",
    )
    worktree_path = get_worktree_path(mux_repo_path, branch_name)

    run_workmux_remove(env, workmux_exe_path, mux_repo_path, branch_name, force=True)

    result = env.tmux(
        ["list-windows", "-t", f"{session_name}:", "-F", "#{window_name}"],
        check=False,
    )
    windows = [w for w in result.stdout.strip().split("\n") if w]
    assert result.returncode != 0 or window_name not in windows
    assert not worktree_path.exists(), "Worktree should be removed"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name not in branch_list_result.stdout


def test_remove_unmerged_branch_with_confirmation(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove` on an unmerged branch succeeds after user confirmation."""
    env = mux_server
    branch_name = "unmerged-branch"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    # Create a new commit to make the branch "unmerged"
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    create_commit(env, worktree_path, "feat: new feature")

    # Run remove, piping 'y' to the confirmation prompt
    run_workmux_remove(
        env, workmux_exe_path, mux_repo_path, branch_name, force=False, user_input="y"
    )

    assert not worktree_path.exists(), "Worktree should be removed after confirmation"
    assert window_name not in env.list_windows()
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name not in branch_list_result.stdout


def test_remove_unmerged_branch_without_confirmation_when_configured(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "configured-unmerged-removal"
    write_workmux_config(mux_repo_path, confirm_unmerged_removal=False)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    create_commit(env, worktree_path, "feat: reviewed change")

    run_workmux_remove(env, workmux_exe_path, mux_repo_path, branch_name)

    assert not worktree_path.exists(), "Unmerged worktree should be removed"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name not in branch_list_result.stdout


def test_remove_unmerged_branch_aborted(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove` on an unmerged branch is aborted if not confirmed."""
    env = mux_server
    branch_name = "unmerged-aborted"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    create_commit(env, worktree_path, "feat: another feature")

    # Run remove, piping 'n' to abort
    run_workmux_remove(
        env, workmux_exe_path, mux_repo_path, branch_name, force=False, user_input="n"
    )

    assert worktree_path.exists(), "Worktree should NOT be removed after aborting"
    assert window_name in env.list_windows()
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name in branch_list_result.stdout


def test_remove_fails_on_uncommitted_changes(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove` fails if the worktree has uncommitted changes."""
    env = mux_server
    branch_name = "dirty-worktree"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    create_dirty_file(worktree_path)

    # This should fail because of uncommitted changes
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        force=False,
        expect_fail=True,
    )

    assert worktree_path.exists(), "Worktree should not be removed when command fails"


def test_configured_unmerged_removal_still_rejects_uncommitted_changes(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "configured-dirty-removal"
    write_workmux_config(mux_repo_path, confirm_unmerged_removal=False)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    create_commit(env, worktree_path, "feat: reviewed change")
    create_dirty_file(worktree_path)

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        expect_fail=True,
    )

    assert worktree_path.exists(), "Dirty worktree should not be removed"


def test_remove_with_force_on_unmerged_branch(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove -f` removes an unmerged branch without a prompt."""
    env = mux_server
    branch_name = "force-remove-unmerged"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    create_commit(env, worktree_path, "feat: something unmerged")

    # Force remove should succeed without interaction
    run_workmux_remove(env, workmux_exe_path, mux_repo_path, branch_name, force=True)

    assert not worktree_path.exists(), "Worktree should be removed"


def test_remove_with_force_on_uncommitted_changes(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove -f` removes a worktree with uncommitted changes."""
    env = mux_server
    branch_name = "force-remove-dirty"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    create_dirty_file(worktree_path)

    # Force remove should succeed despite uncommitted changes
    run_workmux_remove(env, workmux_exe_path, mux_repo_path, branch_name, force=True)

    assert not worktree_path.exists(), "Worktree should be removed"


def test_remove_existing_worktree_with_missing_git_admin_dir_requires_keep_branch(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "missing-admin-dir"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    git_file = worktree_path / ".git"
    admin_dir = Path(git_file.read_text().strip().removeprefix("gitdir: "))
    if not admin_dir.is_absolute():
        admin_dir = worktree_path / admin_dir
    env.run_command(["rm", "-rf", str(admin_dir)])

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        expect_fail=True,
    )
    assert worktree_path.exists(), "Broken worktree should remain without --keep-branch"

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        keep_branch=True,
    )

    assert not worktree_path.exists(), "Broken worktree should be removed"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name in branch_list_result.stdout, "Branch should be kept"


def test_remove_missing_admin_dir_does_not_guess_branch_from_handle(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature/TICKET-123-fix-bug"
    handle = "ticket-123-fix-bug"
    write_workmux_config(mux_repo_path, worktree_naming="basename")
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = mux_repo_path.parent / f"{mux_repo_path.name}__worktrees" / handle
    git_file = worktree_path / ".git"
    admin_dir = Path(git_file.read_text().strip().removeprefix("gitdir: "))
    if not admin_dir.is_absolute():
        admin_dir = worktree_path / admin_dir
    env.run_command(["rm", "-rf", str(admin_dir)])
    env.run_command(["git", "branch", handle], cwd=mux_repo_path)

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        handle,
        expect_fail=True,
    )

    assert worktree_path.exists(), "Broken worktree should remain without --keep-branch"
    handle_result = env.run_command(
        ["git", "branch", "--list", handle], cwd=mux_repo_path
    )
    assert handle in handle_result.stdout, "Handle-named branch should not be deleted"
    real_branch_result = env.run_command(
        ["git", "branch", "--list", branch_name], cwd=mux_repo_path
    )
    assert branch_name in real_branch_result.stdout, "Real branch should not be deleted"


def test_remove_missing_admin_dir_rejects_plain_directory(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    handle = "plain-directory"
    write_workmux_config(mux_repo_path)
    worktree_parent = mux_repo_path.parent / f"{mux_repo_path.name}__worktrees"
    plain_dir = worktree_parent / handle
    plain_dir.mkdir(parents=True)
    (plain_dir / "file.txt").write_text("not a worktree")

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        handle,
        expect_fail=True,
    )

    assert plain_dir.exists(), "Plain directory should not be removed"


@pytest.mark.tmux_only
def test_remove_external_command_ignores_active_client_window(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "external-active-target"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    env.select_window(window_name)

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        force=True,
    )
    assert not worktree_path.exists(), "Worktree should be removed"
    assert window_name not in env.list_windows(), "Window should be closed"


def test_remove_from_within_worktree_window_without_branch_arg(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove` without branch arg works from within worktree window."""
    env = mux_server
    branch_name = "remove-from-within"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    create_commit(env, worktree_path, "feat: work to remove")

    # Run remove from within the worktree window without specifying branch name
    # Should auto-detect the current branch and remove it after confirmation
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name=None,  # Don't specify branch - should auto-detect
        force=False,
        user_input="y",
        from_window=window_name,
    )

    assert not worktree_path.exists(), "Worktree should be removed"
    assert window_name not in env.list_windows(), "Window should be closed"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name not in branch_list_result.stdout, "Branch should be removed"


def test_remove_force_from_within_worktree_window_without_branch_arg(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove -f` without branch arg works from within worktree window."""
    env = mux_server
    branch_name = "force-remove-from-within"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    create_commit(env, worktree_path, "feat: unmerged work")

    # Run remove -f from within the worktree window without specifying branch name
    # Should auto-detect the current branch and remove it without confirmation
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name=None,  # Don't specify branch - should auto-detect
        force=True,
        from_window=window_name,
    )

    assert not worktree_path.exists(), "Worktree should be removed"
    assert window_name not in env.list_windows(), "Window should be closed"
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name not in branch_list_result.stdout, "Branch should be removed"


def test_remove_with_keep_branch_flag(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove --keep-branch` removes worktree and window but keeps the branch."""
    env = mux_server
    branch_name = "keep-branch-test"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    create_commit(env, worktree_path, "feat: work to keep")

    # Run remove with --keep-branch flag
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        keep_branch=True,
        from_window=window_name,
    )

    # Verify worktree is removed
    assert not worktree_path.exists(), "Worktree should be removed"

    # Verify multiplexer window is removed
    assert window_name not in env.list_windows(), "Window should be closed"

    # Verify branch still exists
    branch_list_result = env.run_command(["git", "branch", "--list", branch_name])
    assert branch_name in branch_list_result.stdout, "Branch should still exist"


def test_remove_checks_against_stored_base_branch(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies that remove checks for unmerged changes against the stored base branch, not main."""
    env = mux_server
    # Use unique branch names to avoid collisions in parallel test execution
    unique_id = uuid.uuid4().hex[:8]
    parent_branch = f"stored-base-parent-{unique_id}"
    child_branch = f"stored-base-child-{unique_id}"
    write_workmux_config(mux_repo_path)

    # Create parent branch from main
    run_workmux_add(env, workmux_exe_path, mux_repo_path, parent_branch)
    parent_worktree = get_worktree_path(mux_repo_path, parent_branch)
    create_commit(env, parent_worktree, "feat: parent work")

    # Create child branch from parent using --base
    run_workmux_add(
        env,
        workmux_exe_path,
        mux_repo_path,
        child_branch,
        base=parent_branch,
        background=True,
    )

    child_worktree = get_worktree_path(mux_repo_path, child_branch)
    create_commit(env, child_worktree, "feat: child work")

    # Verify the base branch was stored in git config
    config_result = env.run_command(
        ["git", "config", "--local", f"branch.{child_branch}.workmux-base"],
        cwd=mux_repo_path,
    )
    assert config_result.returncode == 0, "Base branch should be stored in git config"
    assert parent_branch in config_result.stdout, (
        f"Stored base should be '{parent_branch}', got: {config_result.stdout}"
    )

    # Try to remove child branch - should prompt because it has commits not merged into parent
    # (even though parent itself might not be merged into main)
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        child_branch,
        force=False,
        user_input="n",  # Abort to verify the prompt appears
    )

    # Verify worktree still exists (removal was aborted)
    assert child_worktree.exists(), "Worktree should still exist after aborting"

    # Now confirm the removal
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        child_branch,
        force=False,
        user_input="y",  # Confirm removal
    )

    # Verify child branch was removed
    assert not child_worktree.exists(), "Child worktree should be removed"
    branch_list_result = env.run_command(["git", "branch", "--list", child_branch])
    assert child_branch not in branch_list_result.stdout, (
        "Child branch should be deleted"
    )

    # Parent should still exist
    assert parent_worktree.exists(), "Parent worktree should still exist"


def test_remove_closes_window_with_basename_naming_config(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """
    Verifies that `workmux rm` correctly closes the multiplexer window when the worktree
    was created with a naming config that differs from the raw branch name.

    This is a lifecycle test that catches bugs where `add` and `rm` derive the
    window name inconsistently. See: the bug where rm used raw branch_name instead
    of the handle derived from the worktree directory basename.
    """
    env = mux_server

    # Branch name with a prefix that will be stripped by basename strategy
    branch_name = "feature/TICKET-123-fix-bug"
    # With basename, only "TICKET-123-fix-bug" is used, then slugified
    expected_handle = "ticket-123-fix-bug"
    expected_window = f"{DEFAULT_WINDOW_PREFIX}{expected_handle}"

    # Configure basename naming strategy
    write_workmux_config(mux_repo_path, worktree_naming="basename")

    # Create the worktree
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    # Verify worktree exists with the derived handle (not the full branch name)
    worktree_parent = mux_repo_path.parent / f"{mux_repo_path.name}__worktrees"
    worktree_path = worktree_parent / expected_handle
    assert worktree_path.is_dir(), (
        f"Worktree should exist at {worktree_path}, "
        f"found: {list(worktree_parent.iterdir()) if worktree_parent.exists() else 'parent not found'}"
    )

    # Verify window exists with the derived name
    assert expected_window in env.list_windows(), (
        f"Window {expected_window!r} should exist. Found: {env.list_windows()}"
    )

    # Remove the worktree using the handle (worktree directory name)
    run_workmux_remove(
        env, workmux_exe_path, mux_repo_path, expected_handle, force=True
    )

    # Verify worktree is gone
    assert not worktree_path.exists(), "Worktree should be removed"

    # Verify window is closed - this is the key assertion that catches the bug
    assert expected_window not in env.list_windows(), (
        f"Window {expected_window!r} should be closed after rm. "
        f"Still found: {env.list_windows()}"
    )


def test_remove_gone_flag(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    mux_remote_repo_path: Path,
):
    """Verifies `workmux remove --gone` removes worktrees whose upstream was deleted."""
    env = mux_server
    write_workmux_config(mux_repo_path)

    # Add remote to the repo
    env.run_command(
        ["git", "remote", "add", "origin", str(mux_remote_repo_path)], cwd=mux_repo_path
    )

    # 1. Setup a branch with upstream that will be deleted (simulating merged PR)
    gone_branch = "gone-branch"
    window_gone = get_window_name(gone_branch)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, gone_branch)
    gone_worktree = get_worktree_path(mux_repo_path, gone_branch)
    create_commit(env, gone_worktree, "feat: gone work")

    # Push to remote and set upstream
    env.run_command(["git", "push", "-u", "origin", gone_branch], cwd=gone_worktree)

    # Delete the remote branch (simulating what happens after PR merge on GitHub)
    env.run_command(["git", "branch", "-D", gone_branch], cwd=mux_remote_repo_path)

    # 2. Setup a branch with upstream that still exists
    active_branch = "active-branch"
    window_active = get_window_name(active_branch)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, active_branch)
    active_worktree = get_worktree_path(mux_repo_path, active_branch)
    create_commit(env, active_worktree, "feat: active work")

    # Push to remote and set upstream (but don't delete it)
    env.run_command(["git", "push", "-u", "origin", active_branch], cwd=active_worktree)

    # 3. Setup a branch without upstream (local only)
    local_branch = "local-branch"
    window_local = get_window_name(local_branch)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, local_branch)
    local_worktree = get_worktree_path(mux_repo_path, local_branch)

    # Verify all worktrees exist before removal
    assert gone_worktree.exists(), "Gone worktree should exist"
    assert active_worktree.exists(), "Active worktree should exist"
    assert local_worktree.exists(), "Local worktree should exist"

    # 4. Run remove --gone with 'y' confirmation
    # (--gone runs git fetch --prune internally)
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name=None,
        gone=True,
        user_input="y",
    )

    # 5. Verify only the "gone" worktree was removed
    assert not gone_worktree.exists(), "Gone branch worktree should be removed"
    assert active_worktree.exists(), "Active branch worktree should remain"
    assert local_worktree.exists(), "Local branch worktree should remain"

    # Verify windows
    windows = env.list_windows()
    assert window_gone not in windows, "Gone window should be closed"
    assert window_active in windows, "Active window should remain"
    assert window_local in windows, "Local window should remain"

    # Verify branches
    gone_result = env.run_command(
        ["git", "branch", "--list", gone_branch], cwd=mux_repo_path
    )
    assert gone_branch not in gone_result.stdout, "Gone branch should be deleted"

    active_result = env.run_command(
        ["git", "branch", "--list", active_branch], cwd=mux_repo_path
    )
    assert active_branch in active_result.stdout, "Active branch should remain"

    local_result = env.run_command(
        ["git", "branch", "--list", local_branch], cwd=mux_repo_path
    )
    assert local_branch in local_result.stdout, "Local branch should remain"


def test_remove_all_flag(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove --all` removes all worktrees except the main branch."""
    env = mux_server
    write_workmux_config(mux_repo_path)

    worktrees = add_worktrees(
        env,
        workmux_exe_path,
        mux_repo_path,
        ["feature-one", "feature-two", "feature-three"],
    )
    assert_worktrees_exist(worktrees)

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        all=True,
        user_input="y",
    )

    assert_worktrees_removed(worktrees)
    assert_windows_closed(env, worktrees)
    assert_branches_deleted(env, mux_repo_path, [wt.branch for wt in worktrees])


def test_remove_all_with_force(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove --all -f` removes all worktrees without confirmation."""
    env = mux_server
    write_workmux_config(mux_repo_path)

    branch1 = "force-all-one"
    branch2 = "force-all-two"

    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch1)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch2)

    worktree1 = get_worktree_path(mux_repo_path, branch1)
    worktree2 = get_worktree_path(mux_repo_path, branch2)

    # Create uncommitted changes in one worktree
    create_dirty_file(worktree1)

    # Create unmerged commits in another worktree
    create_commit(env, worktree2, "feat: unmerged work")

    # Run remove --all --force (should succeed without prompts)
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        all=True,
        force=True,
    )

    # Verify all worktrees were removed
    assert not worktree1.exists(), "Worktree with uncommitted changes should be removed"
    assert not worktree2.exists(), "Worktree with unmerged commits should be removed"


def test_remove_all_skips_unmerged_without_force(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove --all` skips worktrees with unmerged commits unless --force is used."""
    env = mux_server
    write_workmux_config(mux_repo_path)

    # Create a clean branch (no unmerged commits)
    clean_branch = "all-clean-branch"
    run_workmux_add(env, workmux_exe_path, mux_repo_path, clean_branch)
    clean_worktree = get_worktree_path(mux_repo_path, clean_branch)

    # Create a branch with unmerged commits
    unmerged_branch = "all-unmerged-branch"
    run_workmux_add(env, workmux_exe_path, mux_repo_path, unmerged_branch)
    unmerged_worktree = get_worktree_path(mux_repo_path, unmerged_branch)
    create_commit(env, unmerged_worktree, "feat: unmerged work")

    # Run remove --all with confirmation
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        all=True,
        user_input="y",
    )

    # Clean branch should be removed
    assert not clean_worktree.exists(), "Clean worktree should be removed"

    # Unmerged branch should be skipped (still exists)
    assert unmerged_worktree.exists(), "Unmerged worktree should be skipped"

    # Verify the unmerged branch still exists
    result = env.run_command(
        ["git", "branch", "--list", unmerged_branch], cwd=mux_repo_path
    )
    assert unmerged_branch in result.stdout, "Unmerged branch should still exist"


def test_remove_all_includes_unmerged_when_confirmation_disabled(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    write_workmux_config(mux_repo_path, confirm_unmerged_removal=False)

    unmerged_branch = "all-configured-unmerged"
    dirty_branch = "all-configured-dirty"
    run_workmux_add(env, workmux_exe_path, mux_repo_path, unmerged_branch)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, dirty_branch)

    unmerged_worktree = get_worktree_path(mux_repo_path, unmerged_branch)
    dirty_worktree = get_worktree_path(mux_repo_path, dirty_branch)
    create_commit(env, unmerged_worktree, "feat: reviewed change")
    create_dirty_file(dirty_worktree)

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        all=True,
        user_input="y",
    )

    assert not unmerged_worktree.exists(), "Unmerged worktree should be removed"
    assert dirty_worktree.exists(), "Dirty worktree should be skipped"


def test_remove_all_with_keep_branch(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove --all --keep-branch` removes worktrees but keeps branches."""
    env = mux_server
    write_workmux_config(mux_repo_path)

    branch1 = "keep-all-one"
    branch2 = "keep-all-two"

    worktrees = add_worktrees(env, workmux_exe_path, mux_repo_path, [branch1, branch2])

    create_commit(env, worktrees[0].path, "feat: work one")
    create_commit(env, worktrees[1].path, "feat: work two")

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        all=True,
        keep_branch=True,
        user_input="y",
    )

    assert_worktrees_removed(worktrees)
    assert_branches_present(env, mux_repo_path, [wt.branch for wt in worktrees])


def test_remove_multiple_branches(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove branch1 branch2` removes multiple worktrees at once."""
    env = mux_server
    write_workmux_config(mux_repo_path)

    branch1 = "multi-rm-one"
    branch2 = "multi-rm-two"
    branch3 = "multi-rm-three"

    worktrees = add_worktrees(
        env,
        workmux_exe_path,
        mux_repo_path,
        [branch1, branch2, branch3],
    )
    removed, kept = worktrees[:2], worktrees[2]
    assert_worktrees_exist(worktrees)

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name=f"{branch1} {branch2}",
        force=True,
    )

    assert_worktrees_removed(removed)
    assert kept.path.exists(), "Worktree 3 should still exist"

    windows = env.list_windows()
    assert removed[0].window not in windows, "Window 1 should be closed"
    assert removed[1].window not in windows, "Window 2 should be closed"
    assert kept.window in windows, "Window 3 should still exist"

    assert_branches_deleted(env, mux_repo_path, [wt.branch for wt in removed])
    result = env.run_command(["git", "branch", "--list", branch3], cwd=mux_repo_path)
    assert branch3 in result.stdout, f"Branch {branch3} should still exist"


def test_remove_multiple_with_unmerged_prompts_once(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove branch1 branch2` prompts once for all unmerged branches."""
    env = mux_server
    write_workmux_config(mux_repo_path)

    branch1 = "multi-unmerged-one"
    branch2 = "multi-unmerged-two"

    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch1)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch2)

    worktree1 = get_worktree_path(mux_repo_path, branch1)
    worktree2 = get_worktree_path(mux_repo_path, branch2)

    # Add unmerged commits to both worktrees
    create_commit(env, worktree1, "feat: unmerged work one")
    create_commit(env, worktree2, "feat: unmerged work two")

    # Remove both - should prompt once and remove both after confirmation
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name=f"{branch1} {branch2}",
        user_input="y",
    )

    # Verify both worktrees were removed
    assert not worktree1.exists(), "Worktree 1 should be removed"
    assert not worktree2.exists(), "Worktree 2 should be removed"


def test_remove_multiple_aborted_keeps_all(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies aborting multi-branch removal keeps all worktrees."""
    env = mux_server
    write_workmux_config(mux_repo_path)

    branch1 = "multi-abort-one"
    branch2 = "multi-abort-two"

    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch1)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch2)

    worktree1 = get_worktree_path(mux_repo_path, branch1)
    worktree2 = get_worktree_path(mux_repo_path, branch2)

    # Add unmerged commits
    create_commit(env, worktree1, "feat: unmerged work one")
    create_commit(env, worktree2, "feat: unmerged work two")

    # Abort the removal
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name=f"{branch1} {branch2}",
        user_input="n",
    )

    # Verify both worktrees still exist
    assert worktree1.exists(), "Worktree 1 should still exist"
    assert worktree2.exists(), "Worktree 2 should still exist"


def test_remove_multiple_with_uncommitted_fails(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies removing multiple branches fails if any has uncommitted changes."""
    env = mux_server
    write_workmux_config(mux_repo_path)

    branch1 = "multi-dirty-one"
    branch2 = "multi-dirty-two"

    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch1)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch2)

    worktree1 = get_worktree_path(mux_repo_path, branch1)
    worktree2 = get_worktree_path(mux_repo_path, branch2)

    # Add uncommitted changes to one worktree
    create_dirty_file(worktree1)

    # Should fail because of uncommitted changes
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name=f"{branch1} {branch2}",
        expect_fail=True,
    )

    # Verify both worktrees still exist (none removed due to atomic behavior)
    assert worktree1.exists(), "Worktree 1 should still exist"
    assert worktree2.exists(), "Worktree 2 should still exist"


def test_remove_multiple_with_keep_branch(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux remove --keep-branch branch1 branch2` removes worktrees but keeps branches."""
    env = mux_server
    write_workmux_config(mux_repo_path)

    branch1 = "multi-keep-one"
    branch2 = "multi-keep-two"

    worktrees = add_worktrees(env, workmux_exe_path, mux_repo_path, [branch1, branch2])

    create_commit(env, worktrees[0].path, "feat: work one")
    create_commit(env, worktrees[1].path, "feat: work two")

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name=f"{branch1} {branch2}",
        keep_branch=True,
    )

    assert_worktrees_removed(worktrees)
    assert_branches_present(env, mux_repo_path, [wt.branch for wt in worktrees])


def test_remove_branch_merged_into_local_main_not_remote(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    mux_remote_repo_path: Path,
):
    """
    Verifies that removing a branch merged into local main (but not pushed to remote)
    succeeds without prompting for confirmation.

    This is a regression test for issue #30:
    https://github.com/raine/workmux/issues/30

    The bug was that get_merge_base() prioritized origin/main over local main,
    causing false "unmerged commits" warnings when the local main was ahead of remote.
    """
    env = mux_server
    write_workmux_config(mux_repo_path)

    # Setup remote and push main to it
    env.run_command(
        ["git", "remote", "add", "origin", str(mux_remote_repo_path)], cwd=mux_repo_path
    )
    env.run_command(["git", "push", "-u", "origin", "main"], cwd=mux_repo_path)

    # Create feature branch and commit
    branch_name = "feature-local-merge"
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    create_commit(env, worktree_path, "feat: my feature")

    # Merge feature into local main (but don't push to remote)
    env.run_command(["git", "merge", branch_name], cwd=mux_repo_path)

    # Verify local main is ahead of origin/main
    status_result = env.run_command(["git", "status"], cwd=mux_repo_path)
    assert "ahead" in status_result.stdout, "Local main should be ahead of origin/main"

    # Remove the feature worktree - should succeed WITHOUT prompting
    # because the branch IS merged into local main.
    # If the bug exists, this would require user_input="y" to confirm.
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        force=False,
        # No user_input - if the fix works, no prompt should appear
    )

    # Verify the worktree was removed
    assert not worktree_path.exists(), (
        "Worktree should be removed without prompting because "
        "the branch is merged into local main"
    )

    # Verify the branch was deleted
    branch_list_result = env.run_command(
        ["git", "branch", "--list", branch_name], cwd=mux_repo_path
    )
    assert branch_name not in branch_list_result.stdout, "Branch should be deleted"


@pytest.mark.tmux_only
def test_deferred_remove_suppresses_reference_transaction_hook(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "deferred-hook"
    handle = branch_name
    window_name = get_window_name(handle)
    worktree_path = get_worktree_path(mux_repo_path, handle)
    marker = env.tmp_path / "reference-transaction-ran"
    hook = mux_repo_path / ".git" / "hooks" / "reference-transaction"

    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    env.run_command(["git", "branch", "plain-git-control"], cwd=mux_repo_path)
    hook.write_text(f"#!/bin/sh\ntouch {shlex.quote(str(marker))}\n")
    hook.chmod(0o755)

    env.run_command(["git", "branch", "-D", "plain-git-control"], cwd=mux_repo_path)
    assert marker.exists(), "plain Git must execute the positive-control hook"
    marker.unlink()

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        force=True,
        from_window=window_name,
    )

    assert not marker.exists()
    assert not worktree_path.exists()
    assert window_name not in env.list_windows()
    assert (
        branch_name
        not in env.run_command(
            ["git", "branch", "--list", branch_name], cwd=mux_repo_path
        ).stdout
    )

    def metadata_removed() -> bool:
        return (
            env.run_command(
                [
                    "git",
                    "config",
                    "--local",
                    "--get-regexp",
                    f"^workmux\\.worktree\\.{handle}\\.",
                ],
                cwd=mux_repo_path,
                check=False,
            ).returncode
            != 0
        )

    assert poll_until(metadata_removed, timeout=10.0)


@pytest.mark.tmux_only
def test_deferred_remove_cleans_up_after_final_tmux_window_closes(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "final-window-cleanup"
    window_name = get_window_name(branch_name)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    windows = env.tmux(
        ["list-windows", "-F", "#{window_id} #{window_name}"]
    ).stdout.splitlines()
    for window in windows:
        window_id, name = window.split(" ", 1)
        if name != window_name:
            env.tmux(["kill-window", "-t", window_id])

    output = env.tmp_path / "final-window-remove-output"
    env.send_keys(
        window_name,
        f"cd {shlex.quote(str(worktree_path))} && "
        f"{shlex.quote(str(workmux_exe_path))} remove --force {branch_name} "
        f">{shlex.quote(str(output))} 2>&1",
    )

    assert poll_until(
        lambda: output.exists() and "Scheduled removal" in output.read_text()
    )
    assert poll_until(lambda: not worktree_path.exists(), timeout=10.0)
    assert poll_until(
        lambda: branch_name
        not in env.run_command(
            ["git", "branch", "--list", branch_name], cwd=mux_repo_path
        ).stdout,
        timeout=10.0,
    )
    registered = env.run_command(
        ["git", "worktree", "list", "--porcelain"], cwd=mux_repo_path
    ).stdout
    assert str(worktree_path) not in registered
    assert poll_until(
        lambda: env.run_command(
            [
                "git",
                "config",
                "--local",
                "--get-regexp",
                f"^workmux\\.worktree\\.{branch_name}\\.",
            ],
            cwd=mux_repo_path,
            check=False,
        ).returncode
        != 0,
        timeout=10.0,
    )
    assert env.tmux(["has-session"], check=False).returncode != 0
    pending = Path(env.env["XDG_STATE_HOME"]) / "workmux" / "pending-cleanup"
    assert poll_until(
        lambda: not list(worktree_path.parent.glob(f".workmux_trash_{branch_name}_*"))
        and not list(pending.glob("*.json")),
        timeout=10.0,
    )


@pytest.mark.tmux_only
def test_synchronous_remove_rejects_symlink_substitution(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    write_workmux_config(mux_repo_path)
    add_worktrees(env, workmux_exe_path, mux_repo_path, ["substitute-a", "victim-b"])
    path_a = get_worktree_path(mux_repo_path, "substitute-a")
    path_b = get_worktree_path(mux_repo_path, "victim-b")
    parked_a = path_a.with_name("parked-a")
    (path_a / "a-sentinel").write_text("a")
    (path_b / "b-sentinel").write_text("b")
    hook = f"mv {shlex.quote(str(path_a))} {shlex.quote(str(parked_a))}; ln -s {shlex.quote(str(path_b))} {shlex.quote(str(path_a))}"
    write_workmux_config(mux_repo_path, pre_remove=[hook])

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        "substitute-a",
        force=True,
        expect_fail=True,
    )

    assert path_a.is_symlink()
    assert (parked_a / "a-sentinel").read_text() == "a"
    assert (path_b / "b-sentinel").read_text() == "b"
    assert_branches_present(env, mux_repo_path, ["substitute-a", "victim-b"])


@pytest.mark.tmux_only
def test_deferred_remove_rejects_worktree_substitution_while_waiting(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    ready = env.tmp_path / "remove-hook-ready"
    proceed = env.tmp_path / "remove-hook-proceed"
    hook = (
        f"touch {shlex.quote(str(ready))}; "
        f"while [ ! -e {shlex.quote(str(proceed))} ]; do sleep 0.01; done"
    )
    write_workmux_config(mux_repo_path, pre_remove=[hook])
    add_worktrees(env, workmux_exe_path, mux_repo_path, ["deferred-a", "victim-b"])
    path_a = get_worktree_path(mux_repo_path, "deferred-a")
    path_b = get_worktree_path(mux_repo_path, "victim-b")
    parked_a = path_a.with_name("parked-deferred-a")
    (path_a / "a-sentinel").write_text("a")
    (path_b / "b-sentinel").write_text("b")
    window_a = get_window_name("deferred-a")
    output = env.tmp_path / "deferred-substitution-output"
    command = (
        f"cd {shlex.quote(str(path_a))} && "
        f"{shlex.quote(str(workmux_exe_path))} remove --force deferred-a "
        f">{shlex.quote(str(output))} 2>&1"
    )
    env.send_keys(window_a, command)
    assert poll_until(ready.exists)

    path_a.rename(parked_a)
    path_b.rename(path_a)
    proceed.touch()

    assert poll_until(lambda: window_a not in env.list_windows(), timeout=15.0)
    assert poll_until(
        lambda: output.exists()
        and "Scheduled removal of worktree 'deferred-a'" in output.read_text(),
        timeout=5.0,
    )
    parent_output = output.read_text()
    assert "Scheduled removal of worktree 'deferred-a'" in parent_output
    assert "Removed worktree 'deferred-a'" not in parent_output
    log_path = Path(env.env["XDG_STATE_HOME"]) / "workmux" / "workmux.log"
    assert poll_until(
        lambda: log_path.exists()
        and 'deferred cleanup worker failed handle="deferred-a"' in log_path.read_text()
        and "Worktree identity changed before quarantine" in log_path.read_text(),
        timeout=10.0,
    ), f"Expected substitution failure in {log_path}:\n" + (
        log_path.read_text() if log_path.exists() else "Log file missing"
    )
    assert (parked_a / "a-sentinel").read_text() == "a"
    assert (path_a / "b-sentinel").read_text() == "b"
    assert_branches_present(env, mux_repo_path, ["deferred-a", "victim-b"])
    assert not list(path_a.parent.glob(".workmux_trash_*"))


@pytest.mark.tmux_only
def test_deferred_remove_uses_structured_hostile_arguments(
    mux_server: TmuxEnvironment, workmux_exe_path: Path
):
    env = mux_server
    marker = Path("/tmp") / f"workmux-hostile-{uuid.uuid4().hex}"
    repo_path = env.tmp_path / "repo space ' ; $(literal)"
    repo_path.mkdir()
    env.run_command(["git", "init", "-b", "main"], cwd=repo_path)
    env.run_command(["git", "config", "user.email", "test@example.com"], cwd=repo_path)
    env.run_command(["git", "config", "user.name", "Test User"], cwd=repo_path)
    (repo_path / "README").write_text("test\n")
    env.run_command(["git", "add", "README"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "initial"], cwd=repo_path)
    write_workmux_config(repo_path)
    branch_name = f"evil;touch${{IFS}}{marker};#$(touch${{IFS}}{marker})"
    handle = "hostile-cleanup"

    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add {shlex.quote(branch_name)} --name {handle} --background",
    )
    worktree_path = get_worktree_path(repo_path, handle)
    window_name = get_window_name(handle)
    run_workmux_remove(
        env,
        workmux_exe_path,
        repo_path,
        force=True,
        from_window=window_name,
    )

    assert not marker.exists()
    assert not worktree_path.exists()
    assert window_name not in env.list_windows()
    assert poll_until(
        lambda: branch_name
        not in env.run_command(
            ["git", "branch", "--list", branch_name], cwd=repo_path
        ).stdout,
        timeout=10.0,
    )


@pytest.mark.tmux_only
def test_deferred_worker_timeout_is_scheduled_and_durably_logged(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "deferred-timeout"
    window_name = get_window_name(branch_name)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    output = env.tmp_path / "deferred-timeout-output"
    log_path = Path(env.env["XDG_STATE_HOME"]) / "workmux" / "workmux.log"
    real_tmux = shutil.which("tmux")
    assert real_tmux is not None

    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    fake_tmux = env.fake_bin_dir / "tmux"
    env.install_script(
        fake_tmux,
        "#!/bin/sh\n"
        'for arg in "$@"; do\n'
        '  case "$arg" in kill-window|kill-session) exit 0;; esac\n'
        "done\n"
        f'exec {shlex.quote(real_tmux)} "$@"\n',
    )
    command = (
        f"cd {shlex.quote(str(worktree_path))} && "
        f"{shlex.quote(str(workmux_exe_path))} remove --force {branch_name} "
        f">{shlex.quote(str(output))} 2>&1"
    )
    env.send_keys(window_name, command)

    assert poll_until(output.exists)
    assert poll_until(
        lambda: "Scheduled removal of worktree" in output.read_text(), timeout=5.0
    )
    parent_output = output.read_text()
    assert "Removed worktree" not in parent_output
    assert poll_until(
        lambda: log_path.exists()
        and f'deferred cleanup worker failed handle="{branch_name}"'
        in log_path.read_text()
        and "Timed out waiting for source target" in log_path.read_text()
        and "close it and retry removal" in log_path.read_text(),
        timeout=10.0,
    )
    assert worktree_path.is_dir()
    assert window_name in env.list_windows()
    assert_branches_present(env, mux_repo_path, [branch_name])
    fake_tmux.unlink()


def test_synchronous_metadata_lock_failure_reports_partial_cleanup(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "sync-metadata-lock"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    lock_path = mux_repo_path / ".git" / "config.lock"
    lock_path.write_text("locked")

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        force=True,
        expect_fail=True,
    )

    scripts_dir = get_scripts_dir(env)
    stdout = (scripts_dir / "workmux_remove_stdout.txt").read_text()
    stderr = (scripts_dir / "workmux_remove_stderr.txt").read_text()
    assert "Removed worktree" not in stdout
    assert "Failed to remove Workmux metadata after Git cleanup" in stderr
    pending = Path(env.env["XDG_STATE_HOME"]) / "workmux" / "pending-cleanup"
    records = list(pending.glob("*.json"))
    assert len(records) == 1
    record = json.loads(records[0].read_text())
    assert Path(record["quarantine_path"]).is_dir()
    metadata = env.run_command(
        [
            "git",
            "config",
            "--local",
            "--get-regexp",
            f"^workmux\\.worktree\\.{branch_name}\\.",
        ],
        cwd=mux_repo_path,
        check=False,
    )
    assert metadata.returncode == 0
    lock_path.unlink()


@pytest.mark.tmux_only
def test_deferred_metadata_lock_failure_is_scheduled_and_durably_logged(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "deferred-metadata-lock"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    lock_path = mux_repo_path / ".git" / "config.lock"
    lock_path.write_text("locked")
    log_path = Path(env.env["XDG_STATE_HOME"]) / "workmux" / "workmux.log"

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        force=True,
        from_window=window_name,
    )

    scripts_dir = get_scripts_dir(env)
    stdout_path = scripts_dir / "workmux_remove_stdout.txt"
    assert poll_until(
        lambda: stdout_path.exists()
        and f"Scheduled removal of worktree '{branch_name}'" in stdout_path.read_text(),
        timeout=5.0,
    )
    stdout = stdout_path.read_text()
    assert f"Scheduled removal of worktree '{branch_name}'" in stdout
    assert f"Removed worktree '{branch_name}'" not in stdout
    assert poll_until(
        lambda: log_path.exists()
        and f'deferred cleanup worker failed handle="{branch_name}"'
        in log_path.read_text()
        and "Failed to remove Workmux metadata after Git cleanup"
        in log_path.read_text(),
        timeout=10.0,
    )
    metadata = env.run_command(
        [
            "git",
            "config",
            "--local",
            "--get-regexp",
            f"^workmux\\.worktree\\.{branch_name}\\.",
        ],
        cwd=mux_repo_path,
        check=False,
    )
    assert metadata.returncode == 0
    lock_path.unlink()


@pytest.mark.tmux_only
def test_missing_worktree_path_cleans_git_state_from_matching_target(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "missing-path-cleanup"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    parked_path = worktree_path.with_name(f"{worktree_path.name}-parked")
    worktree_path.rename(parked_path)

    output = env.tmp_path / "missing-path-output"
    command = (
        f"cd {shlex.quote(str(mux_repo_path))} && "
        f"{shlex.quote(str(workmux_exe_path))} remove --force {branch_name} "
        f">{shlex.quote(str(output))} 2>&1"
    )
    env.send_keys(window_name, command)

    assert poll_until(lambda: output.exists() and bool(output.read_text().strip()))
    assert poll_until(lambda: window_name not in env.list_windows(), timeout=10.0)
    assert poll_until(
        lambda: branch_name
        not in env.run_command(
            ["git", "branch", "--list", branch_name], cwd=mux_repo_path
        ).stdout,
        timeout=10.0,
    )
    registered = env.run_command(
        ["git", "worktree", "list", "--porcelain"], cwd=mux_repo_path
    ).stdout
    assert str(worktree_path) not in registered
    metadata = env.run_command(
        [
            "git",
            "config",
            "--local",
            "--get-regexp",
            f"^workmux\\.worktree\\.{branch_name}\\.",
        ],
        cwd=mux_repo_path,
        check=False,
    )
    assert metadata.returncode != 0
    parked_path.rename(worktree_path)
    shutil.rmtree(worktree_path)


def test_absent_locked_worktree_keep_branch_removes_registration(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "absent-locked-keep-branch"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    parked_path = worktree_path.with_name(f"{worktree_path.name}-parked")
    env.run_command(["git", "worktree", "lock", str(worktree_path)], cwd=mux_repo_path)
    worktree_path.rename(parked_path)

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        force=True,
        keep_branch=True,
    )

    assert_branches_present(env, mux_repo_path, [branch_name])
    registered = env.run_command(
        ["git", "worktree", "list", "--porcelain"], cwd=mux_repo_path
    ).stdout
    assert str(worktree_path) not in registered
    metadata = env.run_command(
        [
            "git",
            "config",
            "--local",
            "--get-regexp",
            f"^workmux\\.worktree\\.{branch_name}\\.",
        ],
        cwd=mux_repo_path,
        check=False,
    )
    assert metadata.returncode != 0
    shutil.rmtree(parked_path)


def linked_admin_dir(worktree_path: Path) -> Path:
    pointer = (worktree_path / ".git").read_text().strip()
    admin_dir = Path(pointer.removeprefix("gitdir: "))
    if not admin_dir.is_absolute():
        admin_dir = worktree_path / admin_dir
    return admin_dir


def test_synchronous_worktree_lock_removal_failure_is_partial(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "sync-registration-lock"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    admin_dir = linked_admin_dir(worktree_path)
    lock_path = admin_dir / "locked"
    lock_path.write_text("locked")
    admin_dir.chmod(0o555)

    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        force=True,
        expect_fail=True,
    )

    stderr = (get_scripts_dir(env) / "workmux_remove_stderr.txt").read_text()
    assert "Failed to remove linked-worktree lock" in stderr
    assert_branches_present(env, mux_repo_path, [branch_name])
    assert lock_path.is_file()
    assert worktree_path.is_dir()
    assert not list(worktree_path.parent.glob(f".workmux_trash_{branch_name}_*"))

    admin_dir.chmod(0o755)
    run_workmux_remove(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        force=True,
    )
    assert not worktree_path.exists()
    assert_branches_deleted(env, mux_repo_path, [branch_name])


@pytest.mark.tmux_only
def test_deferred_worktree_lock_removal_failure_is_durably_logged(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "deferred-registration-lock"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    admin_dir = linked_admin_dir(worktree_path)
    lock_path = admin_dir / "locked"
    lock_path.write_text("locked")
    admin_dir.chmod(0o555)
    log_path = Path(env.env["XDG_STATE_HOME"]) / "workmux" / "workmux.log"

    output = env.tmp_path / "deferred-lock-output"
    command = (
        f"cd {shlex.quote(str(worktree_path))} && "
        f"{shlex.quote(str(workmux_exe_path))} remove --force {branch_name} "
        f">{shlex.quote(str(output))} 2>&1"
    )
    env.send_keys(window_name, command)
    assert poll_until(
        lambda: output.exists() and "Scheduled removal" in output.read_text()
    )
    assert poll_until(lambda: window_name not in env.list_windows())

    assert poll_until(
        lambda: log_path.exists()
        and f'deferred cleanup worker failed handle="{branch_name}"'
        in log_path.read_text()
        and "Failed to remove linked-worktree lock" in log_path.read_text(),
        timeout=10.0,
    )
    assert_branches_present(env, mux_repo_path, [branch_name])
    assert lock_path.is_file()
    assert worktree_path.is_dir()
    admin_dir.chmod(0o755)


@pytest.mark.tmux_only
@pytest.mark.skipif(os.geteuid() == 0, reason="root bypasses directory permissions")
def test_deferred_cleanup_retains_record_when_quarantine_deletion_fails(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
):
    """A detached worker durably records a partially deleted quarantine."""
    env = mux_server
    branch = "cleanup-orphan"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch)
    tree = get_worktree_path(mux_repo_path, branch)
    build = tree / "target" / "debug" / "build"
    sibling = build / "crate-0"
    sibling.mkdir(parents=True)
    (sibling / "artifact").touch()
    protected = build / "protected"
    protected.mkdir()
    (protected / "artifact").touch()
    protected.chmod(0o555)
    try:
        output = env.tmp_path / "cleanup-orphan-output"
        command = (
            f"cd {shlex.quote(str(tree))} && "
            f"{shlex.quote(str(workmux_exe_path))} remove --force {branch} "
            f">{shlex.quote(str(output))} 2>&1"
        )
        env.send_keys(get_window_name(branch), command)
        assert poll_until(
            lambda: output.exists() and "Scheduled removal" in output.read_text()
        ), env.capture_pane(get_window_name(branch))
        state = Path(env.env["XDG_STATE_HOME"]) / "workmux"
        pending = state / "pending-cleanup"
        log = state / "workmux.log"
        assert poll_until(
            lambda: log.exists()
            and "pending cleanup record retained at" in log.read_text(),
            timeout=15,
        )
        assert get_window_name(branch) not in env.list_windows()
        assert not tree.exists()
        records = list(pending.glob("*.json"))
        assert len(records) == 1
        record = json.loads(records[0].read_text())
        trash = Path(record["quarantine_path"])
        assert record["original_path"] == str(tree)
        assert trash.is_dir()
        assert trash.stat().st_ino == record["inode"]
        assert trash.stat().st_dev == record["device"]
        assert "kind=PermissionDenied" in log.read_text()
        assert "Permission denied" in log.read_text()
        assert "Recursive deletion encountered" in log.read_text()
        assert "post-failure snapshot" in log.read_text()
        assert not list(trash.rglob("crate-*")), (
            "An undeletable directory must not block sibling cleanup"
        )
        assert (trash / "target/debug/build/protected/artifact").is_file()
    finally:
        # The worker may have renamed the tree even if an assertion failed.
        candidates = [tree, *tree.parent.glob(f".workmux_trash_{branch}_*")]
        for candidate in candidates:
            directory = candidate / "target/debug/build/protected"
            if directory.exists():
                directory.chmod(0o755)
