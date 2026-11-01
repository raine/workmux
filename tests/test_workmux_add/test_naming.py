"""Tests for --name flag, worktree_naming, and worktree_prefix config."""

from pathlib import Path

from ..conftest import (
    DEFAULT_WINDOW_PREFIX,
    MuxEnvironment,
    TmuxEnvironment,
    assert_session_exists,
    assert_session_not_exists,
    assert_window_exists,
    run_workmux_add,
    run_workmux_command,
    slugify,
    write_workmux_config,
)


class TestNameFlag:
    """Tests for the --name flag."""

    def test_add_with_name_uses_custom_handle(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies --name overrides the default handle for worktree directory and tmux window,
        while preserving the original git branch name."""
        env = mux_server
        branch_name = "feature/my-new-feature"
        custom_name = "my-feature"

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {branch_name} --name {custom_name}",
        )

        # Worktree should use the custom name (slugified)
        expected_handle = slugify(custom_name)
        worktree_path = (
            mux_repo_path.parent / f"{mux_repo_path.name}__worktrees" / expected_handle
        )
        assert worktree_path.is_dir(), f"Expected worktree at {worktree_path}"

        # Worktree should NOT exist at the default (branch-derived) path
        default_handle = slugify(branch_name)
        default_path = (
            mux_repo_path.parent / f"{mux_repo_path.name}__worktrees" / default_handle
        )
        assert not default_path.exists()

        # Tmux window should use the custom name
        expected_window = f"{DEFAULT_WINDOW_PREFIX}{expected_handle}"
        assert_window_exists(env, expected_window)

        # Git branch should use the original name, not the handle
        result = env.run_command(
            ["git", "-C", str(worktree_path), "rev-parse", "--abbrev-ref", "HEAD"]
        )
        assert result.stdout.strip() == branch_name

    def test_add_with_name_fails_with_multi_worktree_flags(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies --name cannot be combined with multi-worktree generation flags."""
        env = mux_server

        # Test with --count > 1
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-feature --name custom -n 2",
            expect_fail=True,
        )
        assert "--name cannot be used with multi-worktree generation" in result.stderr

        # Test with --foreach
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-feature --name custom --foreach 'platform:ios,android'",
            expect_fail=True,
        )
        assert "--name cannot be used with multi-worktree generation" in result.stderr

        # Test with multiple --agent flags
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-feature --name custom -a claude -a gemini",
            expect_fail=True,
        )
        assert "--name cannot be used with multi-worktree generation" in result.stderr

    def test_add_with_name_works_with_rescue(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies --name works with --with-changes (rescue) flow."""
        env = mux_server
        branch_name = "rescue-feature"
        custom_name = "rescued"

        # Create uncommitted changes in the main repo
        test_file = mux_repo_path / "uncommitted.txt"
        test_file.write_text("uncommitted content")

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name} --name {custom_name} -u",
        )

        # Verify worktree uses custom name
        expected_handle = slugify(custom_name)
        worktree_path = (
            mux_repo_path.parent / f"{mux_repo_path.name}__worktrees" / expected_handle
        )
        assert worktree_path.is_dir()

        # Verify the changes were moved
        assert (worktree_path / "uncommitted.txt").exists()

        # Verify original worktree is clean
        assert not (mux_repo_path / "uncommitted.txt").exists()


class TestTargetNameOptions:
    """Tests for tmux target naming flags."""

    def test_add_target_name_names_window_only(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        branch_name = "feature/custom-window-target"
        custom_name = "review window"
        expected_handle = slugify(branch_name)
        expected_window = f"{DEFAULT_WINDOW_PREFIX}{slugify(custom_name)}"

        write_workmux_config(mux_repo_path)
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {branch_name} --target-name '{custom_name}'",
        )

        worktree_path = (
            mux_repo_path.parent / f"{mux_repo_path.name}__worktrees" / expected_handle
        )
        assert worktree_path.is_dir()
        assert_window_exists(env, expected_window)

    def test_add_parent_session_preserves_exact_existing_session(
        self,
        mux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        branch_name = "feature/custom-parent-session"
        handle = slugify(branch_name)
        session_name = "WalkingMate"
        target_name = "Review Window"
        window_name = f"{DEFAULT_WINDOW_PREFIX}{slugify(target_name)}"

        write_workmux_config(mux_repo_path)
        env.new_session(session_name)
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {branch_name} --parent-session {session_name} --target-name '{target_name}' --background",
        )

        assert_session_exists(env, session_name)
        assert_session_not_exists(env, session_name.lower())
        assert window_name in env.list_windows_in_session(session_name)

        metadata = env.run_command(
            [
                "git",
                "config",
                "--local",
                "--get",
                f"workmux.worktree.{handle}.window-session",
            ],
            cwd=mux_repo_path,
        )
        assert metadata.stdout.strip() == session_name

    def test_add_target_name_collision_fails_before_git_state(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        branch_name = "feature/custom-target-b"

        write_workmux_config(mux_repo_path)
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add feature/custom-target-a --target-name shared-review --background",
        )
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {branch_name} --target-name shared-review --background",
            expect_fail=True,
        )

        worktrees_dir = mux_repo_path.parent / f"{mux_repo_path.name}__worktrees"
        assert not (worktrees_dir / slugify(branch_name)).exists()
        assert "already exists" in result.stderr
        assert f"{DEFAULT_WINDOW_PREFIX}shared-review" in result.stderr

    def test_add_parent_session_parent_session_can_be_reused(
        self,
        mux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        session_name = "ReviewParents"

        write_workmux_config(mux_repo_path)
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add feature/parent-session-a --parent-session {session_name} --background",
        )
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add feature/parent-session-b --parent-session {session_name} --background",
        )

        assert_session_exists(env, session_name)
        assert_session_not_exists(env, session_name.lower())
        windows = env.list_windows_in_session(session_name)
        assert f"{DEFAULT_WINDOW_PREFIX}feature-parent-session-a" in windows
        assert f"{DEFAULT_WINDOW_PREFIX}feature-parent-session-b" in windows

    def test_custom_parent_session_lifecycle_uses_persisted_target(
        self,
        mux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        branch_name = "feature/parent-session-list"
        session_name = "prs-list"
        window_name = f"{DEFAULT_WINDOW_PREFIX}{slugify(branch_name)}"

        write_workmux_config(mux_repo_path)
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {branch_name} --parent-session {session_name} --background",
        )

        assert window_name in env.list_windows_in_session(session_name)

        list_result = run_workmux_command(env, workmux_exe_path, mux_repo_path, "list")
        row = next(
            line for line in list_result.stdout.splitlines() if branch_name in line
        )
        assert "✓" in row
        assert "closed" not in row

    def test_custom_name_lifecycle_uses_persisted_target(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        branch_name = "feature/custom-lifecycle"
        custom_window = f"{DEFAULT_WINDOW_PREFIX}review-lifecycle"

        write_workmux_config(mux_repo_path)
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {branch_name} --target-name review-lifecycle --background",
        )
        assert_window_exists(env, custom_window)

        list_result = run_workmux_command(env, workmux_exe_path, mux_repo_path, "list")
        assert branch_name in list_result.stdout
        assert "closed" not in list_result.stdout

        run_workmux_command(
            env, workmux_exe_path, mux_repo_path, f"close {branch_name}"
        )
        assert custom_window not in env.list_windows()


class TestWorktreeNaming:
    """Tests for worktree_naming config option."""

    def test_add_respects_basename_naming_strategy(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that worktree_naming: basename uses only the last part of the branch."""
        env = mux_server
        branch_name = "feature/user-auth"
        expected_handle = "user-auth"

        write_workmux_config(mux_repo_path, worktree_naming="basename")

        run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

        # Verify worktree directory uses basename
        worktrees_dir = mux_repo_path.parent / f"{mux_repo_path.name}__worktrees"
        assert (worktrees_dir / expected_handle).is_dir()

        # Verify tmux window uses basename
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}{expected_handle}")


class TestWorktreePrefix:
    """Tests for worktree_prefix config option."""

    def test_add_respects_worktree_prefix(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that worktree_prefix is prepended to the handle."""
        env = mux_server
        branch_name = "api-fix"
        prefix = "backend-"
        expected_handle = f"{prefix}{branch_name}"

        write_workmux_config(mux_repo_path, worktree_prefix=prefix)

        run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

        worktrees_dir = mux_repo_path.parent / f"{mux_repo_path.name}__worktrees"
        assert (worktrees_dir / expected_handle).is_dir()
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}{expected_handle}")


class TestCombinedNamingOptions:
    """Tests for combined naming options."""

    def test_add_combines_basename_and_prefix(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that basename strategy and prefix work together."""
        env = mux_server
        branch_name = "team/frontend/login"
        expected_handle = "web-login"

        write_workmux_config(
            mux_repo_path,
            worktree_naming="basename",
            worktree_prefix="web-",
        )

        run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

        worktrees_dir = mux_repo_path.parent / f"{mux_repo_path.name}__worktrees"
        assert (worktrees_dir / expected_handle).is_dir()
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}{expected_handle}")

    def test_explicit_name_overrides_naming_config(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that --name overrides all config (naming strategy and prefix)."""
        env = mux_server
        branch_name = "feature/complex-stuff"
        explicit_name = "simple-name"

        # Configure options that should be ignored when --name is used
        write_workmux_config(
            mux_repo_path,
            worktree_naming="basename",
            worktree_prefix="ignored-",
        )

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {branch_name} --name {explicit_name}",
        )

        worktrees_dir = mux_repo_path.parent / f"{mux_repo_path.name}__worktrees"

        # Should be exactly what was passed in --name, ignoring prefix
        assert (worktrees_dir / explicit_name).is_dir()
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}{explicit_name}")

        # Verify the config was ignored
        assert not (worktrees_dir / "complex-stuff").exists()
        assert not (worktrees_dir / "ignored-simple-name").exists()


class TestHandleEnvVar:
    """Tests for WORKMUX_HANDLE environment variable in hooks."""

    def test_post_create_hook_receives_workmux_handle_env_var(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """
        Verifies that post_create hooks receive the WORKMUX_HANDLE environment variable
        with the derived handle (not the raw branch name).
        """
        env = mux_server

        # Branch with prefix that will be stripped by basename
        branch_name = "feature/my-feature"
        expected_handle = "my-feature"  # basename of branch, slugified

        # Output file where the hook will write the env var
        handle_output_file = mux_repo_path / "handle_from_hook.txt"

        # Configure basename naming and a hook that writes WORKMUX_HANDLE to a file
        write_workmux_config(
            mux_repo_path,
            worktree_naming="basename",
            post_create=[f"echo $WORKMUX_HANDLE > {handle_output_file}"],
        )

        # Create the worktree
        run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

        # Verify the hook was run and received the correct handle
        assert handle_output_file.exists(), "Hook should have created the output file"
        actual_handle = handle_output_file.read_text().strip()
        assert actual_handle == expected_handle, (
            f"WORKMUX_HANDLE should be '{expected_handle}' (derived handle), "
            f"not '{actual_handle}' (which might be the raw branch name)"
        )
