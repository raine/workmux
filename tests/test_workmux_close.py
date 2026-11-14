from pathlib import Path

import pytest

from .conftest import (
    DEFAULT_WINDOW_PREFIX,
    MuxEnvironment,
    TmuxEnvironment,
    WorkmuxCommandResult,
    get_scripts_dir,
    get_window_name,
    get_worktree_path,
    poll_until,
    poll_until_file_has_content,
    run_workmux_add,
    run_workmux_command,
    write_workmux_config,
)


def run_workmux_close(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    name: str | None = None,
    expect_fail: bool = False,
) -> WorkmuxCommandResult:
    """
    Helper to run `workmux close` command.

    Uses run_shell_background to avoid hanging when close kills the current window.
    """
    scripts_dir = get_scripts_dir(env)
    stdout_file = scripts_dir / "workmux_close_stdout.txt"
    stderr_file = scripts_dir / "workmux_close_stderr.txt"
    exit_code_file = scripts_dir / "workmux_close_exit_code.txt"

    for f in [stdout_file, stderr_file, exit_code_file]:
        if f.exists():
            f.unlink()

    name_arg = name if name else ""
    close_script = (
        f"cd {repo_path} && "
        f"{workmux_exe_path} close {name_arg} "
        f"> {stdout_file} 2> {stderr_file}; "
        f"echo $? > {exit_code_file}"
    )

    env.run_shell_background(close_script)

    assert poll_until_file_has_content(exit_code_file, timeout=5.0), (
        "workmux close did not complete in time"
    )

    exit_code = int(exit_code_file.read_text().strip())
    stderr = stderr_file.read_text() if stderr_file.exists() else ""
    stdout = stdout_file.read_text() if stdout_file.exists() else ""

    result = WorkmuxCommandResult(
        exit_code=exit_code,
        stdout=stdout,
        stderr=stderr,
    )

    if expect_fail:
        if exit_code == 0:
            raise AssertionError(
                f"workmux close was expected to fail but succeeded.\nStderr:\n{stderr}"
            )
    else:
        if exit_code != 0:
            raise AssertionError(
                f"workmux close failed with exit code {exit_code}\nStderr:\n{stderr}"
            )

    return result


def test_close_kills_window_keeps_worktree(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux close` kills the multiplexer window but keeps the worktree."""
    env = mux_server
    branch_name = "feature-close-test"
    window_name = get_window_name(branch_name)

    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    # Verify window exists before close
    windows = env.list_windows()
    assert window_name in windows

    # Close the window
    run_workmux_close(env, workmux_exe_path, mux_repo_path, branch_name)

    # Verify window is gone
    assert poll_until(lambda: window_name not in env.list_windows(), timeout=5.0)

    # Verify worktree still exists
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    assert worktree_path.exists(), "Worktree should still exist after close"


@pytest.mark.tmux_only
def test_close_adopts_window_recreated_by_tmux_restore(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature-restored-close"
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
    restored_token = env.tmux(
        ["display-message", "-p", "-t", restored_id, "#{@workmux_token}"]
    ).stdout.strip()
    assert restored_token == ""

    run_workmux_close(env, workmux_exe_path, mux_repo_path, branch_name)

    window_ids = env.tmux(
        ["list-windows", "-a", "-F", "#{window_id}"], check=False
    ).stdout.splitlines()
    assert restored_id not in window_ids
    assert worktree_path.exists()


@pytest.mark.tmux_only
def test_close_does_not_adopt_same_name_window_from_another_directory(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature-restore-impostor"
    window_name = get_window_name(branch_name)

    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    env.kill_window(window_name)
    impostor_id = env.tmux(
        [
            "new-window",
            "-d",
            "-n",
            window_name,
            "-c",
            str(mux_repo_path),
            "-P",
            "-F",
            "#{window_id}",
        ]
    ).stdout.strip()

    result = run_workmux_close(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        expect_fail=True,
    )

    assert "No active window found" in result.stderr
    window_ids = env.tmux(
        ["list-windows", "-a", "-F", "#{window_id}"]
    ).stdout.splitlines()
    assert impostor_id in window_ids


def test_close_window_in_parent_session(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Close finds a window in its parent session from another session."""
    env = mux_server
    if env.backend_name not in ("tmux", "rex"):
        pytest.skip(f"{env.backend_name} has no parent sessions")
    branch_name = "feature/parent-session-close"
    session_name = "prs-close"
    window_name = get_window_name(branch_name)

    def windows_in_parent() -> list[str]:
        # The parent session may be gone once its last window closes
        try:
            return env.list_windows_in_session(session_name)
        except Exception:
            return []

    write_workmux_config(mux_repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"add {branch_name} --parent-session {session_name} --background",
    )

    assert window_name in windows_in_parent()

    # Runs from the test session, not the parent session
    run_workmux_close(env, workmux_exe_path, mux_repo_path, branch_name)

    assert poll_until(lambda: window_name not in windows_in_parent(), timeout=5.0)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    assert worktree_path.exists(), "Worktree should still exist after close"


@pytest.mark.tmux_only
def test_close_uses_stable_id_after_rename_and_name_reuse(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature-close-renamed"
    window_name = get_window_name(branch_name)

    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    original_id = env.tmux(
        ["display-message", "-p", "-t", f"={window_name}", "#{window_id}"]
    ).stdout.strip()
    env.tmux(["rename-window", "-t", original_id, "user-renamed"])
    replacement_id = env.tmux(
        [
            "new-window",
            "-d",
            "-n",
            window_name,
            "-P",
            "-F",
            "#{window_id}",
        ]
    ).stdout.strip()

    run_workmux_close(env, workmux_exe_path, mux_repo_path, branch_name)

    windows = env.tmux(
        ["list-windows", "-a", "-F", "#{window_id}\t#{window_name}"]
    ).stdout.splitlines()
    assert all(not line.startswith(f"{original_id}\t") for line in windows)
    assert f"{replacement_id}\t{window_name}" in windows


@pytest.mark.tmux_only
def test_close_from_inside_renamed_window_uses_stable_id(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature-self-close-renamed"
    window_name = get_window_name(branch_name)

    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    window_id = env.tmux(
        ["display-message", "-p", "-t", f"={window_name}", "#{window_id}"]
    ).stdout.strip()
    env.tmux(["rename-window", "-t", window_id, "user-renamed-self"])

    env.send_keys(window_id, f"{workmux_exe_path} close", enter=True)

    def window_is_gone():
        windows = env.tmux(
            ["list-windows", "-a", "-F", "#{window_id}"], check=False
        ).stdout.splitlines()
        return window_id not in windows

    assert poll_until(window_is_gone, timeout=5.0)
    assert get_worktree_path(mux_repo_path, branch_name).exists()


@pytest.mark.tmux_only
def test_close_survives_parent_session_and_window_renames(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    branch_name = "feature-parent-and-window-renamed"
    parent_session = "original-parent"
    window_name = get_window_name(branch_name)

    write_workmux_config(mux_repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"add {branch_name} --parent-session {parent_session} --background",
    )
    window_id = env.tmux(
        [
            "display-message",
            "-p",
            "-t",
            f"{parent_session}:={window_name}",
            "#{window_id}",
        ]
    ).stdout.strip()
    env.tmux(["rename-session", "-t", parent_session, "renamed-parent"])
    env.tmux(["rename-window", "-t", window_id, "renamed-child"])

    run_workmux_close(env, workmux_exe_path, mux_repo_path, branch_name)

    assert poll_until(
        lambda: window_id
        not in env.tmux(
            ["list-windows", "-a", "-F", "#{window_id}"], check=False
        ).stdout.splitlines(),
        timeout=5.0,
    )


def test_close_fails_when_no_window_exists(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux close` fails if no window exists for the worktree."""
    env = mux_server
    branch_name = "feature-no-window"
    window_name = get_window_name(branch_name)

    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    # Kill the window manually
    env.kill_window(window_name)

    # Now try to close - should fail because window doesn't exist
    result = run_workmux_close(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        expect_fail=True,
    )

    assert "No active window found" in result.stderr


def test_close_fails_when_worktree_missing(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux close` fails if the worktree does not exist."""
    env = mux_server
    worktree_name = "nonexistent-worktree"

    write_workmux_config(mux_repo_path)

    result = run_workmux_close(
        env,
        workmux_exe_path,
        mux_repo_path,
        worktree_name,
        expect_fail=True,
    )

    assert "not found" in result.stderr


def test_close_can_reopen_with_open(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies that after `workmux close`, `workmux open` can recreate the window."""
    env = mux_server
    branch_name = "feature-close-reopen"
    window_name = get_window_name(branch_name)

    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    # Close the window
    run_workmux_close(env, workmux_exe_path, mux_repo_path, branch_name)

    # Verify window is gone
    assert poll_until(lambda: window_name not in env.list_windows(), timeout=5.0)

    # Reopen with workmux open
    run_workmux_command(env, workmux_exe_path, mux_repo_path, f"open {branch_name}")

    # Verify window is back
    windows = env.list_windows()
    assert window_name in windows


def test_close_from_inside_worktree_window(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux close` works when run from inside the target window itself."""
    env = mux_server
    branch_name = "feature-self-close"
    window_name = get_window_name(branch_name)

    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    # Verify window exists
    windows = env.list_windows()
    assert window_name in windows

    # Send keystrokes directly to the worktree window to run close
    # This tests the schedule_window_close path (self-closing)
    cmd = f"{workmux_exe_path} close"
    env.send_keys(window_name, cmd, enter=True)

    # Poll until window is gone
    def window_is_gone():
        return window_name not in env.list_windows()

    assert poll_until(window_is_gone, timeout=5.0), "Window did not close itself"

    # Verify worktree still exists
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    assert worktree_path.exists(), "Worktree should still exist after self-close"


def test_close_by_branch_name_when_handle_differs(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux close <branch>` resolves correctly when handle differs from branch name."""
    env = mux_server
    branch_name = "feature/close-handle-test"
    handle = "close-handle-test"
    window_name = f"{DEFAULT_WINDOW_PREFIX}{handle}"

    write_workmux_config(mux_repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"add {branch_name} --name {handle}",
    )

    # Verify window exists (named after handle, not branch)
    windows = env.list_windows()
    assert window_name in windows

    # Close using the branch name -- this should resolve to the correct handle
    run_workmux_close(env, workmux_exe_path, mux_repo_path, branch_name)

    # Verify window is gone
    assert poll_until(lambda: window_name not in env.list_windows(), timeout=5.0)
