"""Tests for config file precedence and global/project config merging."""

from pathlib import Path

from ..conftest import (
    TmuxEnvironment,
    RepoBuilder,
    assert_copied_file,
    assert_symlink_to,
    assert_window_exists,
    get_window_name,
    wait_for_pane_output,
    write_global_workmux_config,
    write_workmux_config,
)
from .conftest import add_branch_and_get_worktree


class TestConfigPrecedence:
    """Tests for project config overriding global config."""

    def test_project_config_overrides_global_config(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Project-level settings should override conflicting global settings."""
        env = isolated_tmux_server
        branch_name = "feature-project-overrides"
        global_prefix = "global-"
        project_prefix = "project-"

        write_global_workmux_config(env, window_prefix=global_prefix)
        write_workmux_config(repo_path, window_prefix=project_prefix)

        add_branch_and_get_worktree(env, workmux_exe_path, repo_path, branch_name)

        project_window = f"{project_prefix}{branch_name}"
        assert_window_exists(env, project_window)

        list_windows_result = env.tmux(["list-windows", "-F", "#{window_name}"])
        existing_windows = list_windows_result.stdout.strip().split("\n")
        assert f"{global_prefix}{branch_name}" not in existing_windows

    def test_global_config_used_when_project_config_absent(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Global config should be respected even if the repository lacks .workmux.yaml."""
        env = isolated_tmux_server
        branch_name = "feature-global-only"
        hook_file = "global_only_hook.txt"

        write_global_workmux_config(env, post_create=[f"touch {hook_file}"])

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name
        )
        assert (worktree_path / hook_file).exists()


class TestGlobalPlaceholderPostCreate:
    """Tests for <global> placeholder in post_create hooks."""

    def test_global_placeholder_merges_post_create_commands(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """The '<global>' placeholder should expand to global post_create commands."""
        env = isolated_tmux_server
        branch_name = "feature-global-hooks"
        global_hook = "created_from_global.txt"
        before_hook = "project_before.txt"
        after_hook = "project_after.txt"

        write_global_workmux_config(env, post_create=[f"touch {global_hook}"])
        write_workmux_config(
            repo_path,
            post_create=[f"touch {before_hook}", "<global>", f"touch {after_hook}"],
        )

        worktree_dir = add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name
        )
        assert (worktree_dir / before_hook).exists()
        assert (worktree_dir / global_hook).exists()
        assert (worktree_dir / after_hook).exists()


class TestGlobalPlaceholderFiles:
    """Tests for <global> placeholder in file operations."""

    def test_global_placeholder_merges_file_operations(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """The '<global>' placeholder should merge copy and symlink file operations."""
        env = isolated_tmux_server
        branch_name = "feature-global-files"

        # Create files/directories that will be copied or symlinked.
        repo_builder.with_files(
            {
                "global.env": "GLOBAL",
                "project.env": "PROJECT",
                "global_cache/shared.txt": "global data",
                "project_cache/local.txt": "project data",
            }
        ).commit("Add files for global placeholder tests")

        write_global_workmux_config(
            env,
            files={"copy": ["global.env"], "symlink": ["global_cache"]},
        )
        write_workmux_config(
            repo_path,
            files={
                "copy": ["<global>", "project.env"],
                "symlink": ["<global>", "project_cache"],
            },
        )

        worktree_dir = add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name
        )
        symlinked_global = assert_symlink_to(worktree_dir, "global_cache")
        symlinked_project = assert_symlink_to(worktree_dir, "project_cache")
        assert (symlinked_global / "shared.txt").read_text() == "global data"
        assert (symlinked_project / "local.txt").read_text() == "project data"

        assert_copied_file(worktree_dir, "global.env", "GLOBAL")
        assert_copied_file(worktree_dir, "project.env", "PROJECT")

    def test_global_placeholder_only_merges_specific_file_lists(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """`<global>` can merge copy patterns while symlink patterns fully override."""
        env = isolated_tmux_server
        branch_name = "feature-partial-file-merge"

        repo_builder.add_to_gitignore(
            [
                "global_copy.txt",
                "project_copy.txt",
                "global_symlink_dir/",
                "project_symlink_dir/",
            ]
        )

        (repo_path / "global_copy.txt").write_text("global copy")
        (repo_path / "project_copy.txt").write_text("project copy")
        global_symlink_dir = repo_path / "global_symlink_dir"
        global_symlink_dir.mkdir()
        (global_symlink_dir / "global.txt").write_text("global data")
        project_symlink_dir = repo_path / "project_symlink_dir"
        project_symlink_dir.mkdir()
        (project_symlink_dir / "project.txt").write_text("project data")

        write_global_workmux_config(
            env,
            files={"copy": ["global_copy.txt"], "symlink": ["global_symlink_dir"]},
        )
        write_workmux_config(
            repo_path,
            files={
                "copy": ["<global>", "project_copy.txt"],
                "symlink": ["project_symlink_dir"],
            },
        )

        worktree_dir = add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name
        )
        assert_copied_file(worktree_dir, "global_copy.txt")
        assert_copied_file(worktree_dir, "project_copy.txt")

        assert_symlink_to(worktree_dir, "project_symlink_dir")
        assert not (worktree_dir / "global_symlink_dir").exists()


class TestEmptyOverrides:
    """Tests for empty lists overriding global config."""

    def test_project_empty_file_lists_override_global_lists(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Explicit empty lists suppress the corresponding global file operations."""
        env = isolated_tmux_server
        branch_name = "feature-empty-file-override"

        repo_builder.add_to_gitignore(
            [
                "global_only.env",
                "global_shared_dir/",
            ]
        )

        (repo_path / "global_only.env").write_text("SECRET=1")
        global_shared_dir = repo_path / "global_shared_dir"
        global_shared_dir.mkdir()
        (global_shared_dir / "package.json").write_text('{"name":"demo"}')

        write_global_workmux_config(
            env,
            files={"copy": ["global_only.env"], "symlink": ["global_shared_dir"]},
        )
        write_workmux_config(
            repo_path,
            files={"copy": [], "symlink": []},
        )

        worktree_dir = add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name
        )
        assert not (worktree_dir / "global_only.env").exists()
        assert not (worktree_dir / "global_shared_dir").exists()


class TestPaneOverrides:
    """Tests for pane config overrides."""

    def test_project_panes_replace_global_panes(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Project panes should completely replace global panes (no merging)."""
        env = isolated_tmux_server
        branch_name = "feature-pane-override"
        window_name = get_window_name(branch_name)
        global_output = "GLOBAL_PANE_OUTPUT"
        project_output = "PROJECT_PANE_OUTPUT"

        write_global_workmux_config(
            env, panes=[{"command": f"echo '{global_output}'; sleep 0.5"}]
        )
        write_workmux_config(
            repo_path, panes=[{"command": f"echo '{project_output}'; sleep 0.5"}]
        )

        add_branch_and_get_worktree(env, workmux_exe_path, repo_path, branch_name)

        wait_for_pane_output(env, window_name, project_output)

        capture_result = env.tmux(["capture-pane", "-p", "-t", window_name])
        assert global_output not in capture_result.stdout
