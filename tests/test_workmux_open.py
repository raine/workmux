from pathlib import Path

import pytest

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
    run_workmux_open,
    run_workmux_remove,
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
def test_open_new_inserts_after_base_group(
    mux_server: MuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `workmux open --new` inserts duplicate after base handle group, not at end."""
    env = mux_server

    write_workmux_config(repo_path)

    # Create three worktrees in order: feature-a, my-feature, feature-b
    run_workmux_add(env, workmux_exe_path, repo_path, "feature-a")
    run_workmux_add(env, workmux_exe_path, repo_path, "my-feature")
    run_workmux_add(env, workmux_exe_path, repo_path, "feature-b")

    # Create a duplicate of my-feature
    run_workmux_open(env, workmux_exe_path, repo_path, "my-feature", new_window=True)

    # Get window list (tmux list-windows outputs in index order)
    windows = _get_all_windows(env)
    base = get_window_name("my-feature")
    dup = f"{base}-2"
    feature_b = get_window_name("feature-b")

    # Duplicate should be immediately after base, and before feature-b
    assert dup in windows, f"Expected {dup} in {windows}"
    assert windows.index(dup) == windows.index(base) + 1, (
        f"Duplicate {dup} should be immediately after {base}. Windows: {windows}"
    )
    assert windows.index(feature_b) > windows.index(dup), (
        f"{feature_b} should be after duplicate {dup}. Windows: {windows}"
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

    assert "No worktree found with name" in result.stderr


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


# =============================================================================
# Prompt support tests
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
