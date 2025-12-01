"""Tests for --name flag, worktree_naming, and worktree_prefix config."""

from pathlib import Path

from ..conftest import (
    TmuxEnvironment,
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
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies --name overrides the default handle for worktree directory and tmux window,
        while preserving the original git branch name."""
        env = isolated_tmux_server
        branch_name = "feature/my-new-feature"
        custom_name = "my-feature"

        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add {branch_name} --name {custom_name}",
        )

        # Worktree should use the custom name (slugified)
        expected_handle = slugify(custom_name)
        worktree_path = (
            repo_path.parent / f"{repo_path.name}__worktrees" / expected_handle
        )
        assert worktree_path.is_dir(), f"Expected worktree at {worktree_path}"

        # Worktree should NOT exist at the default (branch-derived) path
        default_handle = slugify(branch_name)
        default_path = (
            repo_path.parent / f"{repo_path.name}__worktrees" / default_handle
        )
        assert not default_path.exists()

        # Tmux window should use the custom name
        expected_window = f"wm-{expected_handle}"
        assert_window_exists(env, expected_window)

        # Git branch should use the original name, not the handle
        result = env.run_command(
            ["git", "-C", str(worktree_path), "rev-parse", "--abbrev-ref", "HEAD"]
        )
        assert result.stdout.strip() == branch_name

    def test_add_with_name_fails_with_multi_worktree_flags(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies --name cannot be combined with multi-worktree generation flags."""
        env = isolated_tmux_server

        # Test with --count > 1
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            "add my-feature --name custom -n 2",
            expect_fail=True,
        )
        assert "--name cannot be used with multi-worktree generation" in result.stderr

        # Test with --foreach
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            "add my-feature --name custom --foreach 'platform:ios,android'",
            expect_fail=True,
        )
        assert "--name cannot be used with multi-worktree generation" in result.stderr

        # Test with multiple --agent flags
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            "add my-feature --name custom -a claude -a gemini",
            expect_fail=True,
        )
        assert "--name cannot be used with multi-worktree generation" in result.stderr

    def test_add_with_name_works_with_rescue(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies --name works with --with-changes (rescue) flow."""
        env = isolated_tmux_server
        branch_name = "rescue-feature"
        custom_name = "rescued"

        # Create uncommitted changes in the main repo
        test_file = repo_path / "uncommitted.txt"
        test_file.write_text("uncommitted content")

        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name} --name {custom_name} -u",
        )

        # Verify worktree uses custom name
        expected_handle = slugify(custom_name)
        worktree_path = (
            repo_path.parent / f"{repo_path.name}__worktrees" / expected_handle
        )
        assert worktree_path.is_dir()

        # Verify the changes were moved
        assert (worktree_path / "uncommitted.txt").exists()

        # Verify original worktree is clean
        assert not (repo_path / "uncommitted.txt").exists()


class TestWorktreeNaming:
    """Tests for worktree_naming config option."""

    def test_add_respects_basename_naming_strategy(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that worktree_naming: basename uses only the last part of the branch."""
        env = isolated_tmux_server
        branch_name = "feature/user-auth"
        expected_handle = "user-auth"

        write_workmux_config(repo_path, worktree_naming="basename")

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

        # Verify worktree directory uses basename
        worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
        assert (worktrees_dir / expected_handle).is_dir()

        # Verify tmux window uses basename
        assert_window_exists(env, f"wm-{expected_handle}")


class TestWorktreePrefix:
    """Tests for worktree_prefix config option."""

    def test_add_respects_worktree_prefix(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that worktree_prefix is prepended to the handle."""
        env = isolated_tmux_server
        branch_name = "api-fix"
        prefix = "backend-"
        expected_handle = f"{prefix}{branch_name}"

        write_workmux_config(repo_path, worktree_prefix=prefix)

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

        worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
        assert (worktrees_dir / expected_handle).is_dir()
        assert_window_exists(env, f"wm-{expected_handle}")


class TestCombinedNamingOptions:
    """Tests for combined naming options."""

    def test_add_combines_basename_and_prefix(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that basename strategy and prefix work together."""
        env = isolated_tmux_server
        branch_name = "team/frontend/login"
        expected_handle = "web-login"

        write_workmux_config(
            repo_path,
            worktree_naming="basename",
            worktree_prefix="web-",
        )

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

        worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
        assert (worktrees_dir / expected_handle).is_dir()
        assert_window_exists(env, f"wm-{expected_handle}")

    def test_explicit_name_overrides_naming_config(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that --name overrides all config (naming strategy and prefix)."""
        env = isolated_tmux_server
        branch_name = "feature/complex-stuff"
        explicit_name = "simple-name"

        # Configure options that should be ignored when --name is used
        write_workmux_config(
            repo_path,
            worktree_naming="basename",
            worktree_prefix="ignored-",
        )

        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add {branch_name} --name {explicit_name}",
        )

        worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"

        # Should be exactly what was passed in --name, ignoring prefix
        assert (worktrees_dir / explicit_name).is_dir()
        assert_window_exists(env, f"wm-{explicit_name}")

        # Verify the config was ignored
        assert not (worktrees_dir / "complex-stuff").exists()
        assert not (worktrees_dir / "ignored-simple-name").exists()


class TestHandleEnvVar:
    """Tests for WORKMUX_HANDLE environment variable in hooks."""

    def test_post_create_hook_receives_workmux_handle_env_var(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """
        Verifies that post_create hooks receive the WORKMUX_HANDLE environment variable
        with the derived handle (not the raw branch name).
        """
        env = isolated_tmux_server

        # Branch with prefix that will be stripped by basename
        branch_name = "feature/my-feature"
        expected_handle = "my-feature"  # basename of branch, slugified

        # Output file where the hook will write the env var
        handle_output_file = repo_path / "handle_from_hook.txt"

        # Configure basename naming and a hook that writes WORKMUX_HANDLE to a file
        write_workmux_config(
            repo_path,
            worktree_naming="basename",
            post_create=[f"echo $WORKMUX_HANDLE > {handle_output_file}"],
        )

        # Create the worktree
        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

        # Verify the hook was run and received the correct handle
        assert handle_output_file.exists(), "Hook should have created the output file"
        actual_handle = handle_output_file.read_text().strip()
        assert actual_handle == expected_handle, (
            f"WORKMUX_HANDLE should be '{expected_handle}' (derived handle), "
            f"not '{actual_handle}' (which might be the raw branch name)"
        )
