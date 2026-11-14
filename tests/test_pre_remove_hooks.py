"""Tests for pre_remove hooks in `workmux remove` and `workmux merge`."""

from pathlib import Path

from .conftest import (
    MuxEnvironment,
    get_worktree_path,
    run_workmux_add,
    run_workmux_merge,
    run_workmux_remove,
    write_workmux_config,
    create_commit,
)


class TestPreRemoveHooksRemove:
    """Tests for pre_remove hook execution during `workmux remove`."""

    def test_pre_remove_hook_runs_on_remove(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that pre_remove hooks run when removing a worktree."""
        env = mux_server
        branch_name = "feature-pre-remove"
        marker_file = env.tmp_path / "pre_remove_ran.txt"

        write_workmux_config(
            repo_path,
            pre_remove=[f"touch {marker_file}"],
        )

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
        worktree_path = get_worktree_path(repo_path, branch_name)
        assert worktree_path.exists()

        run_workmux_remove(env, workmux_exe_path, repo_path, branch_name, force=True)

        assert marker_file.exists(), "pre_remove hook should have created marker file"
        assert not worktree_path.exists(), "Worktree should be removed after hook runs"

    def test_global_placeholder_is_dropped_without_global_hook(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Project hooks should run when no global pre_remove hook exists."""
        env = mux_server
        branch_name = "feature-missing-global-pre-remove"
        marker_file = env.tmp_path / "project_pre_remove_ran.txt"

        write_workmux_config(
            repo_path,
            pre_remove=["<global>", f"touch {marker_file}"],
        )

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
        worktree_path = get_worktree_path(repo_path, branch_name)
        run_workmux_remove(env, workmux_exe_path, repo_path, branch_name, force=True)

        assert marker_file.exists(), "project pre_remove hook should have run"
        assert not worktree_path.exists(), "Worktree should be removed after hooks run"

    def test_pre_remove_hook_receives_wm_handle(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that WM_HANDLE environment variable is set correctly."""
        env = mux_server
        branch_name = "feature-handle-test"
        env_file = env.tmp_path / "hook_env.txt"

        write_workmux_config(
            repo_path,
            pre_remove=[f'echo "$WM_HANDLE" > {env_file}'],
        )

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
        run_workmux_remove(env, workmux_exe_path, repo_path, branch_name, force=True)

        assert env_file.exists(), "Hook should have written environment variable"
        content = env_file.read_text().strip()
        assert content == branch_name, (
            f"WM_HANDLE should be '{branch_name}', got '{content}'"
        )

    def test_pre_remove_hook_receives_wm_worktree_path(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that WM_WORKTREE_PATH environment variable is set correctly."""
        env = mux_server
        branch_name = "feature-path-test"
        env_file = env.tmp_path / "hook_worktree_path.txt"

        write_workmux_config(
            repo_path,
            pre_remove=[f'echo "$WM_WORKTREE_PATH" > {env_file}'],
        )

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
        expected_path = get_worktree_path(repo_path, branch_name)

        run_workmux_remove(env, workmux_exe_path, repo_path, branch_name, force=True)

        assert env_file.exists(), "Hook should have written environment variable"
        content = env_file.read_text().strip()
        assert content == str(expected_path), (
            f"WM_WORKTREE_PATH should be '{expected_path}', got '{content}'"
        )

    def test_pre_remove_hook_receives_wm_project_root(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that WM_PROJECT_ROOT environment variable is set correctly."""
        env = mux_server
        branch_name = "feature-root-test"
        env_file = env.tmp_path / "hook_project_root.txt"

        write_workmux_config(
            repo_path,
            pre_remove=[f'echo "$WM_PROJECT_ROOT" > {env_file}'],
        )

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
        run_workmux_remove(env, workmux_exe_path, repo_path, branch_name, force=True)

        assert env_file.exists(), "Hook should have written environment variable"
        content = env_file.read_text().strip()
        assert content == str(repo_path), (
            f"WM_PROJECT_ROOT should be '{repo_path}', got '{content}'"
        )

    def test_pre_remove_hook_can_copy_files_to_project_root(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that hooks can use env vars to copy files to the project root."""
        env = mux_server
        branch_name = "feature-copy-test"
        artifacts_dir = "artifacts"

        # Hook creates artifacts dir and copies a file there
        write_workmux_config(
            repo_path,
            post_create=["echo 'test content' > artifact.txt"],
            pre_remove=[
                f'mkdir -p "$WM_PROJECT_ROOT/{artifacts_dir}/$WM_HANDLE"',
                f'cp artifact.txt "$WM_PROJECT_ROOT/{artifacts_dir}/$WM_HANDLE/"',
            ],
        )

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
        run_workmux_remove(env, workmux_exe_path, repo_path, branch_name, force=True)

        # Verify the artifact was copied to the main project
        copied_file = repo_path / artifacts_dir / branch_name / "artifact.txt"
        assert copied_file.exists(), f"Artifact should be copied to {copied_file}"
        assert "test content" in copied_file.read_text()


class TestPreRemoveHooksMerge:
    """Tests for pre_remove hook execution during `workmux merge`."""

    def test_pre_remove_hook_runs_on_merge(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that pre_remove hooks run when merging a worktree."""
        env = mux_server
        branch_name = "feature-merge-hook"
        marker_file = env.tmp_path / "pre_remove_merge_ran.txt"

        write_workmux_config(
            repo_path,
            pre_remove=[f"touch {marker_file}"],
            env=env,  # Commit config to avoid uncommitted changes error
        )

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
        worktree_path = get_worktree_path(repo_path, branch_name)
        create_commit(env, worktree_path, "feat: test commit")

        run_workmux_merge(env, workmux_exe_path, repo_path, branch_name)

        assert marker_file.exists(), "pre_remove hook should have run during merge"
        assert not worktree_path.exists(), "Worktree should be removed after merge"

    def test_pre_remove_hook_not_run_on_merge_with_keep(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that pre_remove hooks do NOT run with --keep flag."""
        env = mux_server
        branch_name = "feature-merge-keep"
        marker_file = env.tmp_path / "pre_remove_keep_ran.txt"

        write_workmux_config(
            repo_path,
            pre_remove=[f"touch {marker_file}"],
            env=env,
        )

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
        worktree_path = get_worktree_path(repo_path, branch_name)
        create_commit(env, worktree_path, "feat: test commit")

        run_workmux_merge(env, workmux_exe_path, repo_path, branch_name, keep=True)

        assert not marker_file.exists(), (
            "pre_remove hook should NOT run when using --keep"
        )
        assert worktree_path.exists(), "Worktree should still exist with --keep"

    def test_pre_remove_hook_receives_all_env_vars_on_merge(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies all environment variables are set correctly during merge."""
        env = mux_server
        branch_name = "feature-merge-env"
        env_file = env.tmp_path / "merge_hook_env.txt"

        write_workmux_config(
            repo_path,
            pre_remove=[
                f'echo "HANDLE=$WM_HANDLE" >> {env_file}',
                f'echo "PATH=$WM_WORKTREE_PATH" >> {env_file}',
                f'echo "ROOT=$WM_PROJECT_ROOT" >> {env_file}',
            ],
            env=env,
        )

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
        expected_worktree = get_worktree_path(repo_path, branch_name)
        create_commit(env, expected_worktree, "feat: test commit")

        run_workmux_merge(env, workmux_exe_path, repo_path, branch_name)

        assert env_file.exists(), "Hook should have written environment variables"
        content = env_file.read_text()
        assert f"HANDLE={branch_name}" in content
        assert f"PATH={expected_worktree}" in content
        assert f"ROOT={repo_path}" in content


class TestPreRemoveHookFailure:
    """Tests for pre_remove hook failure handling."""

    def test_pre_remove_hook_failure_aborts_remove(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that a failing pre_remove hook aborts the removal."""
        env = mux_server
        branch_name = "feature-fail-hook"

        write_workmux_config(
            repo_path,
            pre_remove=["exit 1"],  # Hook that fails
        )

        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)
        worktree_path = get_worktree_path(repo_path, branch_name)

        run_workmux_remove(
            env, workmux_exe_path, repo_path, branch_name, force=True, expect_fail=True
        )

        assert worktree_path.exists(), "Worktree should NOT be removed when hook fails"
