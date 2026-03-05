"""Tests for `workmux add --session` command - session mode worktree creation."""

from ..conftest import (
    assert_session_exists,
    assert_session_not_exists,
    assert_window_not_exists,
    get_session_name,
    get_window_name,
    run_workmux_command,
    write_workmux_config,
)
from .conftest import add_branch_and_get_worktree


class TestSessionCreation:
    """Tests for basic session creation functionality.

    Note: All session tests use --background because the test environment
    runs commands via tmux send-keys without an attached client, so
    switch-client would fail.
    """

    def test_add_session_creates_tmux_session(
        self, mux_server, workmux_exe_path, repo_path
    ):
        """Verifies that `workmux add --session` creates a tmux session."""
        env = mux_server
        branch_name = "feature-session"
        session_name = get_session_name(branch_name)

        write_workmux_config(repo_path)

        add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--session --background",
        )

        assert_session_exists(env, session_name)

    def test_add_session_creates_worktree(
        self, mux_server, workmux_exe_path, repo_path
    ):
        """Verifies that `workmux add --session` creates a git worktree."""
        env = mux_server
        branch_name = "feature-session-worktree"

        write_workmux_config(repo_path)

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--session --background",
        )

        # Verify worktree in git's state
        worktree_list_result = env.run_command(["git", "worktree", "list"])
        assert branch_name in worktree_list_result.stdout

        # Verify worktree directory exists
        assert worktree_path.is_dir()

    def test_add_session_does_not_create_window(
        self, mux_server, workmux_exe_path, repo_path
    ):
        """Verifies that `workmux add --session` does NOT create a tmux window."""
        env = mux_server
        branch_name = "feature-session-no-window"
        window_name = get_window_name(branch_name)

        write_workmux_config(repo_path)

        add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--session --background",
        )

        # The session should exist, but no window with that name in the original session
        assert_window_not_exists(env, window_name)

    def test_add_session_naming_follows_prefix(
        self, mux_server, workmux_exe_path, repo_path
    ):
        """Verifies session naming follows the window_prefix convention."""
        env = mux_server
        branch_name = "feature-prefix-test"
        custom_prefix = "proj-"

        write_workmux_config(repo_path, window_prefix=custom_prefix)

        add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--session --background",
        )

        # Session should use custom prefix
        expected_session = f"{custom_prefix}feature-prefix-test"
        assert_session_exists(env, expected_session)


class TestSessionBackground:
    """Tests for --session with --background flag."""

    def test_add_session_background_creates_detached_session(
        self, mux_server, workmux_exe_path, repo_path
    ):
        """Verifies `workmux add --session --background` creates a detached session.

        Note: We can't easily verify "no switch happened" in the test environment
        because there's no attached client. Instead, we verify that:
        1. The session is created
        2. The worktree is created
        3. The session has the expected structure (panes in the right directory)
        """
        env = mux_server
        branch_name = "feature-session-bg"
        session_name = get_session_name(branch_name)

        write_workmux_config(repo_path)

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--session --background",
        )

        # Verify the new session exists
        assert_session_exists(env, session_name)

        # Verify the worktree was created
        assert worktree_path.is_dir()

        # Verify the session's pane is in the worktree directory
        pane_path_result = env.tmux(
            ["display-message", "-t", f"={session_name}:", "-p", "#{pane_current_path}"]
        )
        assert str(worktree_path) in pane_path_result.stdout


class TestSessionRemove:
    """Tests for removing session-mode worktrees."""

    def test_remove_cleans_up_session(self, mux_server, workmux_exe_path, repo_path):
        """Verifies `workmux remove` cleans up session-mode worktrees."""
        env = mux_server
        branch_name = "feature-session-remove"
        session_name = get_session_name(branch_name)

        write_workmux_config(repo_path)

        # Create session-mode worktree
        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--session --background",
        )

        # Verify session exists
        assert_session_exists(env, session_name)
        assert worktree_path.is_dir()

        # Remove the worktree
        run_workmux_command(
            env, workmux_exe_path, repo_path, f"remove -f {branch_name}"
        )

        # Verify session is gone
        assert_session_not_exists(env, session_name)

        # Verify worktree directory is gone
        assert not worktree_path.exists()


class TestSessionClose:
    """Tests for closing session-mode worktrees."""

    def test_close_closes_session(self, mux_server, workmux_exe_path, repo_path):
        """Verifies `workmux close` closes the session for session-mode worktrees."""
        env = mux_server
        branch_name = "feature-session-close"
        session_name = get_session_name(branch_name)

        write_workmux_config(repo_path)

        # Create session-mode worktree in background
        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--session --background",
        )

        # Verify session exists
        assert_session_exists(env, session_name)

        # Close the worktree (from the main repo, not from inside the session)
        run_workmux_command(env, workmux_exe_path, repo_path, f"close {branch_name}")

        # Verify session is gone
        assert_session_not_exists(env, session_name)

        # Verify worktree still exists (close only kills tmux, not the worktree)
        assert worktree_path.is_dir()


class TestSessionOpen:
    """Tests for opening session-mode worktrees."""

    def test_open_respects_stored_session_mode(
        self, mux_server, workmux_exe_path, repo_path
    ):
        """Verifies `workmux open` respects stored session mode and opens as session.

        Note: workmux open doesn't have --background, but for session mode
        the session is created detached anyway, so we can verify it exists
        after the open command completes.
        """
        env = mux_server
        branch_name = "feature-session-reopen"
        session_name = get_session_name(branch_name)

        write_workmux_config(repo_path)

        # Create session-mode worktree in background
        add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--session --background",
        )

        # Close the session
        run_workmux_command(env, workmux_exe_path, repo_path, f"close {branch_name}")

        # Verify session is gone
        assert_session_not_exists(env, session_name)

        # Re-open the worktree (will try to switch but fail silently in test env)
        # The session should still be created
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"open {branch_name}",
            expect_fail=True,  # May fail due to switch-client, but session should exist
        )

        # Even if switch-client fails, the session should be recreated
        # Check if session exists OR if it was a switch-client error
        sessions_result = env.tmux(
            ["list-sessions", "-F", "#{session_name}"], check=False
        )
        existing_sessions = [s for s in sessions_result.stdout.strip().split("\n") if s]

        # The session should exist (even if switching to it failed)
        assert session_name in existing_sessions, (
            f"Session {session_name!r} should exist after open. "
            f"Existing sessions: {existing_sessions!r}. "
            f"Open command stderr: {result.stderr}"
        )


class TestOpenSessionFlag:
    """Tests for `workmux open --session` flag to override stored mode."""

    def test_open_session_flag_converts_window_to_session(
        self, mux_server, workmux_exe_path, repo_path
    ):
        """Verifies `workmux open --session` converts a window-mode worktree to session mode."""
        env = mux_server
        branch_name = "feature-window-to-session"
        session_name = get_session_name(branch_name)

        write_workmux_config(repo_path)

        # Create as window-mode worktree
        add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name, extra_args="--background"
        )

        # Close the window
        run_workmux_command(env, workmux_exe_path, repo_path, f"close {branch_name}")

        # Re-open with --session flag
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"open {branch_name} --session",
            expect_fail=True,  # May fail due to switch-client in test env
        )

        # The session should be created
        sessions_result = env.tmux(
            ["list-sessions", "-F", "#{session_name}"], check=False
        )
        existing_sessions = [s for s in sessions_result.stdout.strip().split("\n") if s]

        assert session_name in existing_sessions, (
            f"Session {session_name!r} should exist after open --session. "
            f"Existing sessions: {existing_sessions!r}. "
            f"Open command stderr: {result.stderr}"
        )

    def test_open_session_flag_persists_mode(
        self, mux_server, workmux_exe_path, repo_path
    ):
        """Verifies that --session persists the mode change for subsequent opens."""
        env = mux_server
        branch_name = "feature-persist-session"
        session_name = get_session_name(branch_name)

        write_workmux_config(repo_path)

        # Create as window-mode worktree
        add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name, extra_args="--background"
        )

        # Close the window
        run_workmux_command(env, workmux_exe_path, repo_path, f"close {branch_name}")

        # Open with --session to convert
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"open {branch_name} --session",
            expect_fail=True,
        )

        # Close again
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"close {branch_name}",
        )
        assert_session_not_exists(env, session_name)

        # Open again WITHOUT --session; should still use session mode (persisted)
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"open {branch_name}",
            expect_fail=True,
        )

        sessions_result = env.tmux(
            ["list-sessions", "-F", "#{session_name}"], check=False
        )
        existing_sessions = [s for s in sessions_result.stdout.strip().split("\n") if s]

        assert session_name in existing_sessions, (
            f"Session {session_name!r} should exist after subsequent open (mode persisted). "
            f"Existing sessions: {existing_sessions!r}"
        )

    def test_open_session_flag_closes_existing_window(
        self, mux_server, workmux_exe_path, repo_path
    ):
        """Verifies that --session closes the existing window when converting modes."""
        env = mux_server
        branch_name = "feature-close-on-convert"
        window_name = get_window_name(branch_name)
        session_name = get_session_name(branch_name)

        write_workmux_config(repo_path)

        # Create as window-mode worktree
        add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name, extra_args="--background"
        )

        # Verify window exists
        result = env.tmux(["list-windows", "-t", "test:", "-F", "#{window_name}"])
        existing_windows = [w for w in result.stdout.strip().split("\n") if w]
        assert window_name in existing_windows

        # Open with --session (should close old window and create session)
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"open {branch_name} --session",
            expect_fail=True,
        )

        # Old window should be gone
        result = env.tmux(["list-windows", "-t", "test:", "-F", "#{window_name}"])
        existing_windows = [w for w in result.stdout.strip().split("\n") if w]
        assert window_name not in existing_windows, (
            f"Window {window_name!r} should have been closed during mode conversion"
        )

        # New session should exist
        sessions_result = env.tmux(
            ["list-sessions", "-F", "#{session_name}"], check=False
        )
        existing_sessions = [s for s in sessions_result.stdout.strip().split("\n") if s]
        assert session_name in existing_sessions, (
            f"Session {session_name!r} should exist after conversion. "
            f"Existing sessions: {existing_sessions!r}"
        )


class TestMixedMode:
    """Tests for mixed-mode scenarios (some worktrees as windows, some as sessions)."""

    def test_mixed_mode_creates_correct_targets(
        self, mux_server, workmux_exe_path, repo_path
    ):
        """Verifies mixed-mode worktrees create correct tmux types."""
        env = mux_server
        window_branch = "feature-window-mode"
        session_branch = "feature-session-mode"

        write_workmux_config(repo_path)

        # Create window-mode worktree (use --background to stay in test session)
        add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, window_branch, extra_args="--background"
        )

        # Create session-mode worktree
        add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            session_branch,
            extra_args="--session --background",
        )

        # Verify window exists for window-mode (specify test session explicitly)
        result = env.tmux(["list-windows", "-t", "test:", "-F", "#{window_name}"])
        existing_windows = [w for w in result.stdout.strip().split("\n") if w]
        window_name = get_window_name(window_branch)
        assert window_name in existing_windows, (
            f"Window {window_name!r} not found in test session. Existing: {existing_windows!r}"
        )

        # Verify session exists for session-mode
        assert_session_exists(env, get_session_name(session_branch))

    def test_mixed_mode_remove_cleans_up_correctly(
        self, mux_server, workmux_exe_path, repo_path
    ):
        """Verifies remove cleans up the correct type in mixed-mode."""
        env = mux_server
        window_branch = "feature-window-cleanup"
        session_branch = "feature-session-cleanup"

        write_workmux_config(repo_path)

        # Create both types in background
        add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, window_branch, extra_args="--background"
        )
        add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            session_branch,
            extra_args="--session --background",
        )

        # Verify both exist (specify test session for window check)
        result = env.tmux(["list-windows", "-t", "test:", "-F", "#{window_name}"])
        existing_windows = [w for w in result.stdout.strip().split("\n") if w]
        window_name = get_window_name(window_branch)
        assert window_name in existing_windows, (
            f"Window {window_name!r} not found in test session. Existing: {existing_windows!r}"
        )
        assert_session_exists(env, get_session_name(session_branch))

        # Remove session-mode worktree
        run_workmux_command(
            env, workmux_exe_path, repo_path, f"remove -f {session_branch}"
        )

        # Verify session is gone but window still exists
        assert_session_not_exists(env, get_session_name(session_branch))
        result = env.tmux(["list-windows", "-t", "test:", "-F", "#{window_name}"])
        existing_windows = [w for w in result.stdout.strip().split("\n") if w]
        assert window_name in existing_windows, (
            f"Window {window_name!r} should still exist after session removal"
        )

        # Remove window-mode worktree
        run_workmux_command(
            env, workmux_exe_path, repo_path, f"remove -f {window_branch}"
        )

        # Verify window is gone
        result = env.tmux(["list-windows", "-t", "test:", "-F", "#{window_name}"])
        existing_windows = [w for w in result.stdout.strip().split("\n") if w]
        assert window_name not in existing_windows, (
            f"Window {window_name!r} should be removed"
        )
