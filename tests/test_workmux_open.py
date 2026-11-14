import subprocess
from pathlib import Path
from typing import cast

import pytest
import yaml

from .conftest import (
    DEFAULT_WINDOW_PREFIX,
    MuxEnvironment,
    TmuxEnvironment,
    assert_session_exists,
    assert_session_not_exists,
    assert_window_not_exists,
    get_session_name,
    get_window_name,
    get_worktree_path,
    poll_until,
    run_workmux_add,
    run_workmux_command,
    run_workmux_open,
    run_workmux_remove,
    slugify,
    write_workmux_config,
)


def _kill_window(env: MuxEnvironment, branch_name: str) -> None:
    """Helper to close the window for a branch if it exists."""
    window_name = get_window_name(branch_name)
    if window_name in env.list_windows():
        env.kill_window(window_name)


def _get_all_windows(env: MuxEnvironment) -> list[str]:
    """Helper to get all window names."""
    return env.list_windows()


def test_open_recreates_tmux_window_for_existing_worktree(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open` recreates a window for an existing worktree."""
    env = mux_server
    branch_name = "feature-open-success"
    window_name = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Close the original window to simulate a detached worktree
    env.kill_window(window_name)

    run_workmux_open(env, workmux_exe_path, repo_path, branch_name)

    list_windows = _get_all_windows(env)
    assert window_name in list_windows


@pytest.mark.tmux_only
def test_open_adopts_window_recreated_by_tmux_restore(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    branch_name = "restored-open"
    window_name = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
    worktree_path = get_worktree_path(repo_path, branch_name)
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

    result = run_workmux_open(env, workmux_exe_path, repo_path, branch_name)

    assert "Switched to existing" in result.stdout
    windows = env.tmux(
        ["list-windows", "-a", "-F", "#{window_id}\t#{window_name}"]
    ).stdout.splitlines()
    matching_ids = [
        line.split("\t", 1)[0] for line in windows if line.endswith(f"\t{window_name}")
    ]
    assert matching_ids == [restored_id]


# WezTerm: get_current_window() returns None because WezTerm doesn't expose
# session-level window focus via CLI - GUI focus is controlled by window manager.
@pytest.mark.tmux_only
def test_open_switches_to_existing_window_by_default(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open` switches to existing window instead of erroring."""
    env = mux_server
    branch_name = "feature-switch-test"
    target_window = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Open again - should switch, not error
    result = run_workmux_open(env, workmux_exe_path, repo_path, branch_name)

    assert "Switched to existing" in result.stdout

    # Should still only have one window for this worktree
    list_windows = _get_all_windows(env)
    matching = [
        w for w in list_windows if w.startswith(f"{DEFAULT_WINDOW_PREFIX}{branch_name}")
    ]
    assert len(matching) == 1

    # Verify focus actually switched to the target window
    active_window = env.get_current_window()
    assert active_window == target_window, (
        f"Expected active window to be '{target_window}', got '{active_window}'"
    )


@pytest.mark.tmux_only
@pytest.mark.parametrize("flag", ["-b", "--background"])
@pytest.mark.parametrize("state", ["existing", "closed", "new", "session"])
def test_open_background_preserves_focus(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    flag: str,
    state: str,
):
    env = mux_server
    write_workmux_config(repo_path)
    for name in ("background-one", "background-two"):
        run_workmux_command(
            env, workmux_exe_path, repo_path, f"add {name} --background"
        )
        if state in ("closed", "session"):
            env.kill_window(get_window_name(name))

    session = env.tmux(["display-message", "-p", "#{session_id}"]).stdout.strip()
    active_window = ["display-message", "-p", "-t", session + ":", "#{window_id}"]
    original = env.tmux(active_window).stdout.strip()
    extra = {"new": "--new", "session": "--session"}.get(state, "")
    for _ in range(2 if state == "session" else 1):
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"open background-one background-two {flag} {extra}",
        )
        assert env.tmux(active_window).stdout.strip() == original
        assert "Switched" not in result.stdout

    for name in ("background-one", "background-two"):
        if state == "session":
            assert_session_exists(env, get_session_name(name))
        else:
            assert get_window_name(name) in env.list_windows()
            if state == "new":
                assert get_window_name(name) + "-2" in env.list_windows()


def test_open_with_new_flag_creates_duplicate_window(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --new` creates a duplicate window with suffix."""
    env = mux_server
    branch_name = "feature-duplicate"
    base_window = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Open with --new flag
    result = run_workmux_open(
        env, workmux_exe_path, repo_path, branch_name, new_window=True
    )

    assert "Opened" in result.stdout

    # Should now have two windows: base and -2
    list_windows = _get_all_windows(env)
    assert base_window in list_windows
    assert f"{base_window}-2" in list_windows


def test_open_new_without_name_uses_current_worktree(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --new` without name uses current worktree from cwd."""
    env = mux_server
    branch_name = "feature-open-current"
    base_window = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Get the worktree path to run the command from
    worktree_path = get_worktree_path(repo_path, branch_name)

    # Open with --new flag but no name, from inside the worktree directory
    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        None,
        new_window=True,
        working_dir=worktree_path,
    )

    assert "Opened" in result.stdout

    # Should now have two windows: base and -2
    list_windows = _get_all_windows(env)
    assert base_window in list_windows
    assert f"{base_window}-2" in list_windows


def test_open_with_new_flag_creates_incrementing_suffixes(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies multiple `workmux open --new` creates incrementing suffixes (-2, -3, -4)."""
    env = mux_server
    branch_name = "feature-multi-dup"
    base_window = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Open three more duplicates
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)

    list_windows = _get_all_windows(env)
    assert base_window in list_windows
    assert f"{base_window}-2" in list_windows
    assert f"{base_window}-3" in list_windows
    assert f"{base_window}-4" in list_windows


# WezTerm: CLI doesn't support inserting tabs at specific positions like tmux's
# -a flag. New tabs always append at the end.
@pytest.mark.tmux_only
def test_open_new_inserts_right_of_current_window(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --new` inserts duplicate right of the current window."""
    env = mux_server

    write_workmux_config(repo_path)

    # Create three worktrees in order: feature-a, my-feature, feature-b
    run_workmux_add(env, workmux_exe_path, repo_path, "feature-a")
    run_workmux_add(env, workmux_exe_path, repo_path, "my-feature")
    run_workmux_add(env, workmux_exe_path, repo_path, "feature-b")

    # Run the duplicate open from feature-a, not from the base window.
    feature_a = get_window_name("feature-a")
    env.select_window(feature_a)
    run_workmux_open(env, workmux_exe_path, repo_path, "my-feature", new_window=True)

    # Get window list (tmux list-windows outputs in index order)
    windows = _get_all_windows(env)
    base = get_window_name("my-feature")
    dup = f"{base}-2"

    # Duplicate should be immediately after the current window where the command ran.
    assert dup in windows, f"Expected {dup} in {windows}"
    assert windows.index(dup) == windows.index(feature_a) + 1, (
        f"Duplicate {dup} should be immediately after {feature_a}. Windows: {windows}"
    )
    assert windows.index(dup) < windows.index(base), (
        f"Duplicate {dup} should appear before {base}. Windows: {windows}"
    )


def test_open_new_flag_when_no_window_exists_uses_base_name(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --new` uses base name when no window exists."""
    env = mux_server
    branch_name = "feature-new-no-existing"
    window_name = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Kill the original window
    env.kill_window(window_name)

    # Open with --new flag - should use base name since none exists
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)

    list_windows = _get_all_windows(env)
    assert window_name in list_windows
    # Should NOT have -2 suffix since there was no existing window
    assert f"{window_name}-2" not in list_windows


def test_open_new_flag_with_gap_appends_after_highest(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --new` appends after highest suffix, not filling gaps."""
    env = mux_server
    branch_name = "feature-gap-test"
    base_window = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Create -2 and -3 windows
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)

    # Verify we have base, -2, and -3
    list_windows = _get_all_windows(env)
    assert base_window in list_windows
    assert f"{base_window}-2" in list_windows
    assert f"{base_window}-3" in list_windows

    # Kill -2 to create a gap
    env.kill_window(f"{base_window}-2")

    # Verify gap exists
    list_windows = _get_all_windows(env)
    assert f"{base_window}-2" not in list_windows
    assert f"{base_window}-3" in list_windows

    # Open with --new again - should create -4, not fill the -2 gap
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)

    list_windows = _get_all_windows(env)
    assert f"{base_window}-4" in list_windows, (
        "Should append after highest suffix (-3), creating -4"
    )
    # Gap should still exist (we don't fill gaps)
    assert f"{base_window}-2" not in list_windows, "Gap at -2 should not be filled"


def test_open_fails_when_worktree_missing(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open` fails if the worktree does not exist."""
    env = mux_server
    worktree_name = "missing-worktree"

    write_workmux_config(repo_path)

    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        worktree_name,
        expect_fail=True,
    )

    assert "not found" in result.stderr


@pytest.mark.tmux_only
def test_open_mode_window_overrides_stored_session_mode(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --mode window` converts a session-mode worktree to window mode."""
    env = cast(TmuxEnvironment, mux_server)
    branch_name = "feature-session-to-window"
    session_name = get_session_name(branch_name)
    window_name = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add {branch_name} --session --background",
    )

    run_workmux_command(env, workmux_exe_path, repo_path, f"close {branch_name}")
    assert_session_not_exists(env, session_name)

    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        mode="window",
    )

    assert window_name in _get_all_windows(env)
    assert_session_not_exists(env, session_name)


@pytest.mark.tmux_only
def test_open_mode_session_conflicts_with_new(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --new --mode session` is rejected."""
    env = mux_server
    branch_name = "feature-open-new-session-mode"

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        new_window=True,
        mode="session",
        expect_fail=True,
    )

    assert "--new is not supported in session mode" in result.stderr


def test_open_with_run_hooks_reexecutes_post_create_commands(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --run-hooks` re-runs post_create hooks."""
    env = mux_server
    branch_name = "feature-open-hooks"
    hook_file = "open_hook.txt"

    write_workmux_config(repo_path, post_create=[f"touch {hook_file}"])
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    hook_path = worktree_path / hook_file
    hook_path.unlink()

    _kill_window(env, branch_name)

    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        run_hooks=True,
    )

    assert hook_path.exists()


def test_open_with_force_files_reapplies_file_operations(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --force-files` reapplies copy operations."""
    env = mux_server
    branch_name = "feature-open-files"
    shared_file = repo_path / "shared.env"
    shared_file.write_text("KEY=value")

    write_workmux_config(repo_path, files={"copy": ["shared.env"]})
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktree_path = get_worktree_path(repo_path, branch_name)
    worktree_file = worktree_path / "shared.env"
    worktree_file.unlink()

    _kill_window(env, branch_name)

    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        force_files=True,
    )

    assert worktree_file.exists()
    assert worktree_file.read_text() == "KEY=value"


# =============================================================================
# Close command tests with duplicate windows
# =============================================================================


def test_close_in_duplicate_window_closes_correct_window(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux close` in a duplicate window closes only that window."""
    env = mux_server
    branch_name = "feature-close-dup"
    base_window = get_window_name(branch_name)
    dup_window = f"{base_window}-2"

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Create a duplicate window
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)

    # Verify both windows exist
    list_windows = _get_all_windows(env)
    assert base_window in list_windows
    assert dup_window in list_windows

    # Send close command directly to the duplicate window's pane using send-keys
    # This properly sets TMUX_PANE environment variable unlike run-shell
    worktree_path = get_worktree_path(repo_path, branch_name)
    env.send_keys(
        dup_window,
        f"cd {worktree_path} && {workmux_exe_path} close",
        enter=True,
    )

    # Wait for the duplicate window to disappear
    def window_gone():
        return dup_window not in _get_all_windows(env)

    assert poll_until(window_gone, timeout=5.0), "Duplicate window should be closed"

    # Verify original window still exists
    list_windows = _get_all_windows(env)
    assert base_window in list_windows, "Original window should still exist"


def test_remove_closes_all_duplicate_windows(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux remove` closes all duplicate windows for a worktree."""
    env = mux_server
    branch_name = "feature-remove-dups"
    base_window = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Create multiple duplicate windows
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)

    # Verify all windows exist
    list_windows = _get_all_windows(env)
    assert base_window in list_windows
    assert f"{base_window}-2" in list_windows
    assert f"{base_window}-3" in list_windows

    # Remove the worktree
    run_workmux_remove(env, workmux_exe_path, repo_path, branch_name, force=True)

    # Verify all windows are closed
    list_windows = _get_all_windows(env)
    matching = [w for w in list_windows if w.startswith(base_window)]
    assert len(matching) == 0, f"All windows should be closed, but found: {matching}"


@pytest.mark.tmux_only
def test_remove_closes_renamed_duplicates_without_killing_reused_name(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    branch_name = "feature-remove-renamed-dups"
    base_window = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, new_window=True)

    owned = []
    for line in env.tmux(
        [
            "list-windows",
            "-a",
            "-F",
            "#{window_id}\t#{window_name}\t#{@workmux_token}",
        ]
    ).stdout.splitlines():
        window_id, window_name, token = line.split("\t")
        if token:
            owned.append((window_id, window_name))

    assert len(owned) == 3
    for index, (window_id, _) in enumerate(owned):
        env.tmux(["rename-window", "-t", window_id, f"renamed-owned-{index}"])

    replacement_id = env.tmux(
        [
            "new-window",
            "-d",
            "-n",
            base_window,
            "-P",
            "-F",
            "#{window_id}",
        ]
    ).stdout.strip()

    run_workmux_remove(env, workmux_exe_path, repo_path, branch_name, force=True)

    windows = env.tmux(
        ["list-windows", "-a", "-F", "#{window_id}\t#{window_name}"]
    ).stdout.splitlines()
    owned_ids = {window_id for window_id, _ in owned}
    assert all(line.split("\t", 1)[0] not in owned_ids for line in windows)
    assert f"{replacement_id}\t{base_window}" in windows


# =============================================================================


def test_open_with_inline_prompt(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open -p` passes prompt to the worktree."""
    env = mux_server
    branch_name = "feature-open-prompt"
    prompt_text = "Fix the login bug"

    # Configure with an agent placeholder that will receive the prompt
    write_workmux_config(repo_path, panes=[{"command": "<agent>"}], agent="claude")
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Kill the window
    _kill_window(env, branch_name)

    # Open with a prompt
    result = run_workmux_open(
        env, workmux_exe_path, repo_path, branch_name, prompt=prompt_text
    )

    assert result.exit_code == 0

    # Check that a prompt file was created in the temp directory
    prompt_files = list(env.tmp_path.glob("workmux-prompt-*.md"))
    assert len(prompt_files) >= 1, "Prompt file should have been created"

    # Verify at least one prompt file contains our text
    found_prompt = False
    for pf in prompt_files:
        if prompt_text in pf.read_text():
            found_prompt = True
            break
    assert found_prompt, f"Prompt text not found in any prompt file: {prompt_files}"


def test_open_with_special_characters_in_prompt(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies prompt handling with special characters (quotes, $VAR, backticks)."""
    env = mux_server
    branch_name = "feature-special-prompt"
    # Prompt with quotes, dollar signs, and backticks
    # Note: newlines can't be tested via -p due to tmux send-keys limitations
    prompt_text = "Refactor: 'Module' needs $FIX. Verify `code` behavior."

    write_workmux_config(repo_path, panes=[{"command": "<agent>"}], agent="claude")
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    _kill_window(env, branch_name)

    result = run_workmux_open(
        env, workmux_exe_path, repo_path, branch_name, prompt=prompt_text
    )

    assert result.exit_code == 0

    # Verify the exact prompt text was preserved in the file
    prompt_files = list(env.tmp_path.glob("workmux-prompt-*.md"))
    assert len(prompt_files) >= 1, "Prompt file should have been created"

    found_exact = False
    for pf in prompt_files:
        if prompt_text in pf.read_text():
            found_exact = True
            break
    assert found_exact, (
        "Exact prompt text with special characters not found in any prompt file"
    )


def test_open_from_inside_worktree_switches_to_other(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open` from inside one worktree can switch to another."""
    env = mux_server
    branch_a = "feature-source"
    branch_b = "feature-target"
    window_a = get_window_name(branch_a)
    window_b = get_window_name(branch_b)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_a)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_b)

    # Kill window B to simulate a detached worktree
    env.kill_window(window_b)

    # Run open from inside window A to open window B
    worktree_a_path = get_worktree_path(repo_path, branch_a)
    env.send_keys(
        window_a,
        f"cd {worktree_a_path} && {workmux_exe_path} open {branch_b}",
        enter=True,
    )

    # Wait for window B to appear
    def window_b_exists():
        return window_b in _get_all_windows(env)

    assert poll_until(window_b_exists, timeout=5.0), "Window B should be opened"

    # Both windows should exist
    list_windows = _get_all_windows(env)
    assert window_a in list_windows, "Window A should still exist"
    assert window_b in list_windows, "Window B should be opened"


def test_open_with_prompt_file(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open -P` reads prompt from file."""
    env = mux_server
    branch_name = "feature-open-prompt-file"
    prompt_text = "Implement the new feature\n\nDetails here."

    # Create a prompt file
    prompt_file = repo_path / "my-prompt.md"
    prompt_file.write_text(prompt_text)

    # Configure with an agent placeholder that will receive the prompt
    write_workmux_config(repo_path, panes=[{"command": "<agent>"}], agent="claude")
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Kill the window
    _kill_window(env, branch_name)

    # Open with prompt file
    result = run_workmux_open(
        env, workmux_exe_path, repo_path, branch_name, prompt_file=prompt_file
    )

    assert result.exit_code == 0

    # Verify the prompt content was processed into a temp prompt file
    temp_prompt_files = list(env.tmp_path.glob("workmux-prompt-*.md"))
    assert len(temp_prompt_files) >= 1, "Prompt file should have been created"

    # Verify at least one temp file contains our prompt content
    found_content = False
    for pf in temp_prompt_files:
        if "Implement the new feature" in pf.read_text():
            found_content = True
            break
    assert found_content, (
        "Prompt file content was not processed into a temp prompt file"
    )


# =============================================================================
# Session flag tests (open --session)
# =============================================================================


@pytest.mark.tmux_only
def test_open_session_flag_creates_session_for_window_mode_worktree(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --session` creates a tmux session for a window-mode worktree."""
    env = mux_server
    branch_name = "feature-open-session"
    window_name = get_window_name(branch_name)
    session_name = get_session_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Kill the window created by add (window mode default)
    env.kill_window(window_name)

    # Open with --session flag (switch-client may fail in test env)
    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        session=True,
        expect_fail=True,
    )

    # Session should exist
    assert_session_exists(env, session_name)
    # No new window should have been created in the test session
    assert_window_not_exists(env, window_name)


@pytest.mark.tmux_only
def test_open_session_flag_switches_to_existing_session(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies second `workmux open --session` switches to existing session, no duplicate."""
    env = mux_server
    branch_name = "feature-open-session-switch"
    window_name = get_window_name(branch_name)
    session_name = get_session_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Kill the window created by add
    env.kill_window(window_name)

    # First open --session: creates the session
    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        session=True,
        expect_fail=True,
    )
    assert_session_exists(env, session_name)

    # Second open --session: should switch to existing session (may fail due to no client)
    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        session=True,
        expect_fail=True,
    )

    # Verify only ONE session with that name exists (no -2 suffix created)
    sessions_result = env.tmux(["list-sessions", "-F", "#{session_name}"], check=False)
    matching = [
        s for s in sessions_result.stdout.strip().split("\n") if s == session_name
    ]
    assert len(matching) == 1, (
        f"Expected exactly 1 session named {session_name!r}, "
        f"found {len(matching)}. All sessions: {sessions_result.stdout.strip()}"
    )


@pytest.mark.tmux_only
def test_open_without_session_flag_uses_stored_window_mode(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open` without --session uses stored window mode (regression)."""
    env = mux_server
    branch_name = "feature-open-window-mode"
    window_name = get_window_name(branch_name)
    session_name = get_session_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Kill the window created by add
    env.kill_window(window_name)

    # Open without --session flag: should recreate as window
    run_workmux_open(env, workmux_exe_path, repo_path, branch_name)

    # Window should exist
    assert window_name in _get_all_windows(env)
    # No session should have been created
    assert_session_not_exists(env, session_name)


@pytest.mark.tmux_only
def test_open_session_flag_with_new_flag_fails(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --session --new` fails (session mode rejects --new)."""
    env = mux_server
    branch_name = "feature-open-session-new"
    window_name = get_window_name(branch_name)
    session_name = get_session_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Kill the window created by add
    env.kill_window(window_name)

    # First, create the session so we can test --new rejection
    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        session=True,
        expect_fail=True,
    )
    assert_session_exists(env, session_name)

    # Now try --session --new, which should fail
    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        session=True,
        new_window=True,
        expect_fail=True,
    )

    # Command should have exited non-zero (expect_fail=True already asserts this)
    assert result.exit_code != 0


@pytest.mark.tmux_only
def test_open_legacy_worktree_falls_back_to_config_mode(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open` falls back to config mode for legacy worktrees without metadata.

    Simulates the scenario where a worktree was created before mode metadata
    persistence existed. When the config has `mode: session`, open should use
    session mode and backfill the metadata.
    """
    env = mux_server
    branch_name = "feature-legacy-session"
    handle = slugify(branch_name)
    session_name = get_session_name(branch_name)
    window_name = get_window_name(branch_name)

    # Create worktree in default window mode
    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Kill the window created by add
    env.kill_window(window_name)

    # Remove mode metadata to simulate a legacy worktree (created before metadata persistence)
    subprocess.run(
        [
            "git",
            "config",
            "--local",
            "--unset",
            f"workmux.worktree.{handle}.mode",
        ],
        cwd=repo_path,
        check=True,
        env=env.env,
    )

    # Rewrite config with mode: session (simulating user adding session mode to config)
    config: dict = {
        "nerdfont": False,
        "mode": "session",
    }
    (repo_path / ".workmux.yaml").write_text(yaml.dump(config))

    # Open without --session flag: should fall back to config mode (session)
    # switch-client may fail in test env, so expect_fail=True
    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        expect_fail=True,
    )

    # Session should have been created (config fallback to session mode)
    assert_session_exists(env, session_name)

    # Verify metadata was backfilled
    result = subprocess.run(
        [
            "git",
            "config",
            "--local",
            "--get",
            f"workmux.worktree.{handle}.mode",
        ],
        cwd=repo_path,
        capture_output=True,
        text=True,
        env=env.env,
    )
    assert result.stdout.strip() == "session", (
        f"Expected backfilled mode to be 'session', got: {result.stdout.strip()!r}"
    )


def test_open_multiple_worktrees_at_once(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open` can open multiple worktrees in a single command."""
    env = mux_server
    branches = ["feature-multi-a", "feature-multi-b", "feature-multi-c"]

    write_workmux_config(repo_path)
    for branch in branches:
        run_workmux_add(env, workmux_exe_path, repo_path, branch)
        env.kill_window(get_window_name(branch))

    # Open all three at once
    result = run_workmux_open(env, workmux_exe_path, repo_path, branches)

    windows = _get_all_windows(env)
    for branch in branches:
        window_name = get_window_name(branch)
        assert window_name in windows, (
            f"Expected window '{window_name}' to exist after multi-open"
        )

    # Verify output mentions all worktrees
    for branch in branches:
        assert branch in result.stdout


def test_open_multiple_with_partial_failure(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open` with multiple names continues on failure."""
    env = mux_server
    valid_branch = "feature-multi-valid"
    invalid_branch = "feature-multi-nonexistent"

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, valid_branch)
    env.kill_window(get_window_name(valid_branch))

    # One valid, one invalid: should open the valid one and report error for invalid
    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        [valid_branch, invalid_branch],
        expect_fail=True,
    )

    # Valid worktree should still be opened
    windows = _get_all_windows(env)
    assert get_window_name(valid_branch) in windows

    # Output should report the failure
    assert invalid_branch in result.stderr


def test_open_multiple_with_prompt_fails(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open` rejects prompt args when opening multiple worktrees."""
    env = mux_server
    branches = ["feature-prompt-a", "feature-prompt-b"]

    write_workmux_config(repo_path)
    for branch in branches:
        run_workmux_add(env, workmux_exe_path, repo_path, branch)

    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branches,
        prompt="some prompt",
        expect_fail=True,
    )

    assert "cannot be used when opening multiple worktrees" in result.stderr


def test_open_target_name_names_window_only(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies open can recreate an existing worktree with a custom window name."""
    env = mux_server
    branch_name = "feature-open-custom-window"
    custom_window = f"{DEFAULT_WINDOW_PREFIX}review-open"

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
    env.kill_window(get_window_name(branch_name))

    run_workmux_open(
        env, workmux_exe_path, repo_path, branch_name, target_name="review-open"
    )

    assert custom_window in _get_all_windows(env)
    assert get_window_name(branch_name) not in _get_all_windows(env)


def test_open_target_name_collision_fails(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server

    write_workmux_config(repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add feature/open-target-a --target-name shared-open --background",
    )
    run_workmux_add(env, workmux_exe_path, repo_path, "feature/open-target-b")
    env.kill_window(get_window_name("feature/open-target-b"))

    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        "feature/open-target-b",
        target_name="shared-open",
        expect_fail=True,
    )

    assert "already exists" in result.stderr
    assert f"{DEFAULT_WINDOW_PREFIX}shared-open" in result.stderr


def test_open_target_name_owner_can_switch_existing_target(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server

    write_workmux_config(repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add feature/open-target-owner --target-name owned-open --background",
    )

    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        "feature/open-target-owner",
        target_name="owned-open",
    )

    assert "Switched to existing tmux window" in result.stdout


@pytest.mark.tmux_only
def test_open_parent_session_preserves_exact_existing_session(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    branch_name = "feature-open-parent-session"
    handle = slugify(branch_name)
    session_name = "WalkingMateOpen"
    target_name = "Review Open Session"
    custom_window = f"{DEFAULT_WINDOW_PREFIX}{slugify(target_name)}"

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
    env.kill_window(get_window_name(branch_name))
    env.tmux(["new-session", "-d", "-s", session_name])

    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        parent_session=session_name,
        target_name=target_name,
    )

    assert_session_exists(env, session_name)
    assert_session_not_exists(env, session_name.lower())
    result = env.tmux(
        ["list-windows", "-t", f"{session_name}:", "-F", "#{window_name}"]
    )
    assert custom_window in [w for w in result.stdout.strip().split("\n") if w]

    metadata = env.run_command(
        [
            "git",
            "config",
            "--local",
            "--get",
            f"workmux.worktree.{handle}.window-session",
        ],
        cwd=repo_path,
    )
    assert metadata.stdout.strip() == session_name


@pytest.mark.tmux_only
def test_open_reuses_window_in_parent_session(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    branch_name = "feature/parent-session-open"
    session_name = "prs-open"
    window_name = get_window_name(branch_name)

    write_workmux_config(repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add {branch_name} --parent-session {session_name} --background",
    )

    run_workmux_open(env, workmux_exe_path, repo_path, branch_name)

    result = env.tmux(
        ["list-windows", "-t", f"{session_name}:", "-F", "#{window_name}"]
    )
    matching = [w for w in result.stdout.strip().split("\n") if w == window_name]
    assert matching == [window_name]


@pytest.mark.tmux_only
def test_open_window_parent_session_does_not_pollute_session_target(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    branch_name = "feature/parent-session-no-pollute"
    parent_session = "prs-no-pollute"
    expected_session = get_session_name(branch_name)
    polluted_session = get_session_name(parent_session)

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
    env.kill_window(get_window_name(branch_name))

    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        parent_session=parent_session,
    )
    run_workmux_command(env, workmux_exe_path, repo_path, f"close {branch_name}")

    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        mode="session",
        expect_fail=True,
    )

    assert_session_exists(env, expected_session)
    assert_session_not_exists(env, polluted_session)


@pytest.mark.tmux_only
def test_open_mode_session_name_overrides_session_target(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies open session mode can use a custom session target name."""
    env = mux_server
    branch_name = "feature-open-custom-session"
    custom_session = get_session_name("review-open-session")

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
    env.kill_window(get_window_name(branch_name))

    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        mode="session",
        target_name="review-open-session",
        expect_fail=True,
    )

    assert_session_exists(env, custom_session)


@pytest.mark.tmux_only
def test_open_target_name_session_collision_fails(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server

    write_workmux_config(repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add feature/open-session-target-a --session --target-name shared-open-session --background",
    )
    run_workmux_add(env, workmux_exe_path, repo_path, "feature/open-session-target-b")

    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        "feature/open-session-target-b",
        mode="session",
        target_name="shared-open-session",
        expect_fail=True,
    )

    assert "already exists" in result.stderr
    assert get_session_name("shared-open-session") in result.stderr


@pytest.mark.tmux_only
def test_open_target_name_session_owner_can_switch_existing_target(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server

    write_workmux_config(repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add feature/open-session-owner --session --target-name owned-open-session --background",
    )

    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        "feature/open-session-owner",
        mode="session",
        target_name="owned-open-session",
        expect_fail=True,
    )

    assert "no current client" in result.stderr


def test_open_rejects_parent_session_in_session_mode(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --parent-session is rejected after effective session mode resolution."""
    env = mux_server
    branch_name = "feature-open-session-name-window"

    write_workmux_config(repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add {branch_name} --session --background",
    )

    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        parent_session="nope",
        expect_fail=True,
    )

    assert "--parent-session requires window mode" in result.stderr


def test_open_new_target_name_suffixes_custom_name(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --new suffixes the custom window name rather than the worktree handle."""
    env = mux_server
    branch_name = "feature-open-new-custom-window"
    custom_window = f"{DEFAULT_WINDOW_PREFIX}review-new"

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
    env.kill_window(get_window_name(branch_name))

    run_workmux_open(
        env, workmux_exe_path, repo_path, branch_name, target_name="review-new"
    )
    run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        new_window=True,
        target_name="review-new",
    )

    windows = _get_all_windows(env)
    assert custom_window in windows
    assert f"{custom_window}-2" in windows


def test_open_with_config_override(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --config` loads an alternate config file."""
    env = mux_server
    branch_name = "feature-open-config"
    override_prefix = "open-override-"

    write_workmux_config(repo_path, window_prefix="default-")
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Kill the window so we can recreate it with open
    env.kill_window(f"default-{branch_name}")

    # Write an alternate config
    alt_config = repo_path / ".workmux.open.yaml"
    alt_config.write_text(f"window_prefix: {override_prefix}\nnerdfont: false\n")

    run_workmux_open(env, workmux_exe_path, repo_path, branch_name, config=alt_config)

    assert f"{override_prefix}{branch_name}" in _get_all_windows(env)
    assert f"default-{branch_name}" not in _get_all_windows(env)


def test_open_config_override_missing_file_fails(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --config` with a missing file fails clearly."""
    env = mux_server
    branch_name = "feature-open-missing-config"
    missing_config = repo_path / "nonexistent.yaml"

    write_workmux_config(repo_path)
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
    env.kill_window(get_window_name(branch_name))

    result = run_workmux_open(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        config=missing_config,
        expect_fail=True,
    )

    assert "Config file not found" in result.stderr
