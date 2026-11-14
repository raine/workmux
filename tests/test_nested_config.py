"""Integration tests for nested .workmux.yaml config support."""

import subprocess
from pathlib import Path

import pytest

from .conftest import (
    MuxEnvironment,
    TmuxEnvironment,
    run_workmux_command,
    run_workmux_open,
)

# These tests use tmux-specific features (get pane cwd, kill-window)
pytestmark = pytest.mark.tmux_only


def run_cmd(
    cmd: list[str], cwd: Path, env: MuxEnvironment
) -> subprocess.CompletedProcess:
    """Run a command in the test environment."""
    return env.run_command(cmd, cwd=cwd, check=True)


def get_pane_cwd(env: MuxEnvironment, window_name: str) -> Path:
    """Get the current working directory of a tmux pane by window name."""
    assert isinstance(env, TmuxEnvironment), "get_pane_cwd requires tmux backend"
    result = env.tmux(
        ["display-message", "-p", "-t", window_name, "#{pane_current_path}"]
    )
    return Path(result.stdout.strip())


def wait_for_file_with_content(
    file_path: Path, timeout: float = 5.0, interval: float = 0.1
) -> None:
    """Wait for a file to exist and have content."""
    import time

    start = time.time()
    while time.time() - start < timeout:
        if file_path.exists() and file_path.stat().st_size > 0:
            return
        time.sleep(interval)
    raise TimeoutError(f"File {file_path} not created within {timeout}s")


class TestNestedConfigDiscovery:
    """Tests for config file discovery."""

    def test_find_config_in_current_directory(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Config in current directory is found."""
        env = mux_server
        backend = repo_path / "backend"
        backend.mkdir()
        (backend / ".workmux.yaml").write_text("agent: claude\n")

        # Commit the new files
        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(["git", "commit", "-m", "add backend config"], cwd=repo_path, env=env)

        result = run_workmux_command(
            env, workmux_exe_path, repo_path, "add test-branch", working_dir=backend
        )
        assert result.exit_code == 0

    def test_find_config_in_parent_directory(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Config in parent directory is found when running from subdirectory."""
        env = mux_server
        backend = repo_path / "backend"
        src = backend / "src"
        src.mkdir(parents=True)
        (backend / ".workmux.yaml").write_text("agent: claude\n")

        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(["git", "commit", "-m", "add backend config"], cwd=repo_path, env=env)

        # Run from src/, should find backend/.workmux.yaml
        result = run_workmux_command(
            env, workmux_exe_path, repo_path, "add test-branch", working_dir=src
        )
        assert result.exit_code == 0

    def test_nested_config_takes_precedence_over_root(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        tmp_path: Path,
    ):
        """When both root and nested configs exist, nearest (nested) wins."""
        env = mux_server
        output_file = tmp_path / "which_config.txt"

        # Create root config that writes "root" to file
        (repo_path / ".workmux.yaml").write_text(
            f"agent: claude\npost_create:\n  - 'echo root > {output_file}'\n"
        )

        # Create nested config that writes "nested" to file
        backend = repo_path / "backend"
        backend.mkdir()
        (backend / ".workmux.yaml").write_text(
            f"agent: claude\npost_create:\n  - 'echo nested > {output_file}'\n"
        )

        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(["git", "commit", "-m", "add configs"], cwd=repo_path, env=env)

        # Run from backend/ - should use nested config
        run_workmux_command(
            env, workmux_exe_path, repo_path, "add test-branch", working_dir=backend
        )

        wait_for_file_with_content(output_file)
        assert output_file.read_text().strip() == "nested"


class TestWorkingDirectory:
    """Tests for working directory in new worktrees."""

    def test_add_from_nested_config_sets_working_dir(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """wm add from nested config opens tmux in nested directory."""
        env = mux_server
        backend = repo_path / "backend"
        backend.mkdir()
        (backend / ".workmux.yaml").write_text("agent: claude\n")

        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(["git", "commit", "-m", "add backend"], cwd=repo_path, env=env)

        result = run_workmux_command(
            env, workmux_exe_path, repo_path, "add feature-nested", working_dir=backend
        )
        assert result.exit_code == 0

        # Verify tmux pane is in the nested directory
        pane_cwd = get_pane_cwd(env, "wm-feature-nested")
        worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
        expected = worktrees_dir / "feature-nested" / "backend"
        assert pane_cwd.resolve() == expected.resolve()

    def test_open_from_nested_config_sets_working_dir(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """wm open from nested config opens tmux in nested directory."""
        env = mux_server
        backend = repo_path / "backend"
        backend.mkdir()
        (backend / ".workmux.yaml").write_text("agent: claude\n")

        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(["git", "commit", "-m", "add backend"], cwd=repo_path, env=env)

        # Create worktree
        run_workmux_command(
            env, workmux_exe_path, repo_path, "add test-branch", working_dir=backend
        )

        # Close the tmux window (but keep worktree on disk)
        assert isinstance(env, TmuxEnvironment)
        env.tmux(["kill-window", "-t", "wm-test-branch"])

        # Reopen from backend/
        result = run_workmux_open(
            env, workmux_exe_path, repo_path, "test-branch", working_dir=backend
        )
        assert result.exit_code == 0

        # Verify opened in nested directory
        pane_cwd = get_pane_cwd(env, "wm-test-branch")
        worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
        expected = worktrees_dir / "test-branch" / "backend"
        assert pane_cwd.resolve() == expected.resolve()


class TestFileOperations:
    """Tests for file copy/symlink operations."""

    def test_file_copy_from_nested_config_dir(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Files are copied from config directory, not repo root."""
        env = mux_server
        backend = repo_path / "backend"
        backend.mkdir()
        (backend / ".workmux.yaml").write_text(
            "agent: claude\nfiles:\n  copy:\n    - .env\n"
        )
        (backend / ".env").write_text("SOURCE=backend")

        # Also create a root .env to verify we don't copy from there
        (repo_path / ".env").write_text("SOURCE=root")

        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(["git", "commit", "-m", "add backend with env"], cwd=repo_path, env=env)

        # Create worktree from backend
        result = run_workmux_command(
            env, workmux_exe_path, repo_path, "add test-branch", working_dir=backend
        )
        assert result.exit_code == 0

        # The .env in the worktree's backend/ should come from backend/.env
        worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
        worktree = worktrees_dir / "test-branch"
        env_content = (worktree / "backend" / ".env").read_text()
        assert env_content == "SOURCE=backend"


class TestHooksEnvironment:
    """Tests for hook environment variables."""

    def test_wm_config_dir_env_var(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        tmp_path: Path,
    ):
        """WM_CONFIG_DIR points to nested directory in new worktree."""
        env = mux_server
        output_file = tmp_path / "wm_config_dir.txt"

        backend = repo_path / "backend"
        backend.mkdir()
        (backend / ".workmux.yaml").write_text(
            f"agent: claude\npost_create:\n  - 'echo $WM_CONFIG_DIR > {output_file}'\n"
        )

        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(
            ["git", "commit", "-m", "add backend with hook"], cwd=repo_path, env=env
        )

        run_workmux_command(
            env, workmux_exe_path, repo_path, "add test-branch", working_dir=backend
        )

        # Wait for hook to complete and verify
        wait_for_file_with_content(output_file)
        config_dir = Path(output_file.read_text().strip())
        worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
        expected = worktrees_dir / "test-branch" / "backend"
        assert config_dir.resolve() == expected.resolve()

    def test_wm_project_root_env_var(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        tmp_path: Path,
    ):
        """WM_PROJECT_ROOT points to the main worktree for nested configs."""
        env = mux_server
        output_file = tmp_path / "wm_project_root.txt"

        backend = repo_path / "backend"
        backend.mkdir()
        (backend / ".workmux.yaml").write_text(
            f"agent: claude\npost_create:\n  - 'echo $WM_PROJECT_ROOT > {output_file}'\n"
        )

        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(
            ["git", "commit", "-m", "add backend with hook"], cwd=repo_path, env=env
        )

        run_workmux_command(
            env, workmux_exe_path, repo_path, "add test-branch", working_dir=backend
        )

        wait_for_file_with_content(output_file)
        project_root = Path(output_file.read_text().strip())
        assert project_root.resolve() == repo_path.resolve()

    def test_hook_cwd_is_nested_directory(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        tmp_path: Path,
    ):
        """Hooks run with CWD set to nested directory."""
        env = mux_server
        output_file = tmp_path / "hook_cwd.txt"

        backend = repo_path / "backend"
        backend.mkdir()
        (backend / ".workmux.yaml").write_text(
            f"agent: claude\npost_create:\n  - 'pwd > {output_file}'\n"
        )

        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(
            ["git", "commit", "-m", "add backend with hook"], cwd=repo_path, env=env
        )

        run_workmux_command(
            env, workmux_exe_path, repo_path, "add test-branch", working_dir=backend
        )

        wait_for_file_with_content(output_file)
        hook_cwd = Path(output_file.read_text().strip())
        worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
        expected = worktrees_dir / "test-branch" / "backend"
        assert hook_cwd.resolve() == expected.resolve()


class TestEdgeCases:
    """Tests for edge cases and fallbacks."""

    def test_fallback_when_subdir_missing_in_target(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Falls back to worktree root if subdirectory doesn't exist in target branch."""
        env = mux_server

        # Create backend config on main
        backend = repo_path / "backend"
        backend.mkdir()
        (backend / ".workmux.yaml").write_text("agent: claude\n")
        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(["git", "commit", "-m", "add backend"], cwd=repo_path, env=env)

        # Create an old branch without backend/
        run_cmd(["git", "checkout", "--orphan", "old-base"], cwd=repo_path, env=env)
        run_cmd(["git", "rm", "-rf", "."], cwd=repo_path, env=env)
        (repo_path / "readme.md").write_text("old structure")
        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(["git", "commit", "-m", "old base"], cwd=repo_path, env=env)

        # Switch back to main
        run_cmd(["git", "checkout", "main"], cwd=repo_path, env=env)

        # Run add for branch based on old-base (which lacks backend/)
        # Run FROM backend/ to trigger nested config discovery
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            "add feature-old --base old-base",
            working_dir=backend,
        )
        assert result.exit_code == 0

        # Working dir should fall back to worktree root since backend/ doesn't exist
        pane_cwd = get_pane_cwd(env, "wm-feature-old")
        worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
        expected_root = worktrees_dir / "feature-old"
        assert pane_cwd.resolve() == expected_root.resolve()


class TestBackwardsCompatibility:
    """Tests ensuring existing behavior is preserved."""

    def test_root_config_still_works(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Config at repo root works as before."""
        env = mux_server
        (repo_path / ".workmux.yaml").write_text(
            "agent: claude\nfiles:\n  copy:\n    - .env\n"
        )
        (repo_path / ".env").write_text("ROOT=true")

        run_cmd(["git", "add", "."], cwd=repo_path, env=env)
        run_cmd(["git", "commit", "-m", "add root config"], cwd=repo_path, env=env)

        result = run_workmux_command(
            env, workmux_exe_path, repo_path, "add test-branch"
        )
        assert result.exit_code == 0

        worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
        worktree = worktrees_dir / "test-branch"
        assert (worktree / ".env").read_text() == "ROOT=true"

    def test_no_config_uses_defaults(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """No config file uses default behavior."""
        env = mux_server

        result = run_workmux_command(
            env, workmux_exe_path, repo_path, "add test-branch"
        )
        assert result.exit_code == 0
