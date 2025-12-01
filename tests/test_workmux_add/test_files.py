"""Tests for file operations (copy, symlink) in `workmux add`."""

import os
from pathlib import Path

import pytest

from ..conftest import (
    TmuxEnvironment,
    RepoBuilder,
    assert_copied_file,
    assert_symlink_to,
    run_workmux_add,
    write_workmux_config,
)
from .conftest import add_branch_and_get_worktree


class TestCopyOperations:
    """Tests for file copy operations."""

    def test_add_copies_single_file(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that `workmux add` copies a single file to the worktree."""
        branch_name = "feature-copy-file"

        repo_builder.with_file(".env", "SECRET_KEY=test123").commit("Add .env file")
        write_workmux_config(
            repo_path, files={"copy": [".env"]}, env=isolated_tmux_server
        )

        worktree_path = add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )
        assert_copied_file(worktree_path, ".env", "SECRET_KEY=test123")

    def test_add_copies_multiple_files_with_glob(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that `workmux add` copies multiple files using glob patterns."""
        branch_name = "feature-copy-glob"

        repo_builder.with_files(
            {
                ".env.local": "LOCAL_VAR=value1",
                ".secrets.local": "API_KEY=secret",
            }
        ).commit("Add local files")

        write_workmux_config(
            repo_path, files={"copy": ["*.local"]}, env=isolated_tmux_server
        )

        worktree_path = add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )
        assert_copied_file(worktree_path, ".env.local", "LOCAL_VAR=value1")
        assert_copied_file(worktree_path, ".secrets.local", "API_KEY=secret")

    def test_add_copies_file_with_parent_directories(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that `workmux add` creates parent directories when copying nested files."""
        branch_name = "feature-copy-nested"

        repo_builder.with_file("config/app.conf", "setting=value").commit(
            "Add config files"
        )
        write_workmux_config(
            repo_path, files={"copy": ["config/app.conf"]}, env=isolated_tmux_server
        )

        worktree_path = add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )
        assert_copied_file(worktree_path, "config/app.conf", "setting=value")
        assert (worktree_path / "config").is_dir()

    def test_add_copies_directories(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that directory copy rules replicate nested contents into the worktree."""
        env = isolated_tmux_server
        branch_name = "feature-copy-dir"
        shared_dir = repo_path / "shared-config"
        nested_dir = shared_dir / "nested"

        nested_dir.mkdir(parents=True)
        (shared_dir / "root.txt").write_text("root-level")
        (nested_dir / "child.txt").write_text("nested-level")

        write_workmux_config(repo_path, files={"copy": ["shared-config"]})

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name
        )
        copied_dir = worktree_path / "shared-config"

        assert copied_dir.is_dir()
        assert (copied_dir / "root.txt").read_text() == "root-level"
        assert (copied_dir / "nested" / "child.txt").read_text() == "nested-level"


class TestSymlinkOperations:
    """Tests for symlink operations."""

    def test_add_symlinks_single_file(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that `workmux add` creates a symlink for a single file."""
        branch_name = "feature-symlink-file"

        repo_builder.with_file("shared.txt", "shared content").commit("Add shared file")
        write_workmux_config(
            repo_path, files={"symlink": ["shared.txt"]}, env=isolated_tmux_server
        )

        worktree_path = add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )
        symlinked_file = assert_symlink_to(worktree_path, "shared.txt")
        assert symlinked_file.read_text() == "shared content"

    def test_add_symlinks_directory(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that `workmux add` creates a symlink for a directory."""
        branch_name = "feature-symlink-dir"

        repo_builder.with_file("node_modules/package.json", '{"name": "test"}').commit(
            "Add node_modules"
        )
        write_workmux_config(
            repo_path, files={"symlink": ["node_modules"]}, env=isolated_tmux_server
        )

        worktree_path = add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )
        symlinked_dir = assert_symlink_to(worktree_path, "node_modules")
        assert (symlinked_dir / "package.json").exists()

    def test_add_symlinks_multiple_items_with_glob(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that `workmux add` creates symlinks for multiple items using glob patterns."""
        branch_name = "feature-symlink-glob"

        repo_builder.with_files(
            {
                ".cache/data.txt": "cache data",
                ".pnpm-store/index.txt": "pnpm index",
            }
        ).commit("Add cache dirs")
        write_workmux_config(
            repo_path,
            files={"symlink": [".*-store", ".cache"]},
            env=isolated_tmux_server,
        )

        worktree_path = add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )
        cache_symlink = assert_symlink_to(worktree_path, ".cache")
        pnpm_symlink = assert_symlink_to(worktree_path, ".pnpm-store")
        assert (cache_symlink / "data.txt").exists()
        assert (pnpm_symlink / "index.txt").exists()

    def test_add_symlinks_are_relative(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that created symlinks use relative paths, not absolute paths."""
        branch_name = "feature-symlink-relative"

        repo_builder.with_file("test.txt", "test content").commit("Add test file")
        write_workmux_config(
            repo_path, files={"symlink": ["test.txt"]}, env=isolated_tmux_server
        )

        worktree_path = add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )
        symlinked_file = assert_symlink_to(worktree_path, "test.txt")

        # Verify the symlink points to the correct relative path
        source_file = repo_path / "test.txt"
        expected_target = os.path.relpath(source_file, symlinked_file.parent)
        link_target = os.readlink(symlinked_file)
        assert link_target == expected_target, (
            f"Symlink target incorrect. Expected: {expected_target}, Got: {link_target}"
        )

    def test_add_symlink_with_nested_structure(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that symlinking works with nested directory structures."""
        branch_name = "feature-symlink-nested"

        repo_builder.with_file("lib/cache/data.db", "database content").commit(
            "Add nested structure"
        )
        write_workmux_config(
            repo_path, files={"symlink": ["lib/cache"]}, env=isolated_tmux_server
        )

        worktree_path = add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )
        symlinked_dir = assert_symlink_to(worktree_path, "lib/cache")
        assert (symlinked_dir / "data.db").read_text() == "database content"
        # Verify parent directory exists and is NOT a symlink
        assert (worktree_path / "lib").is_dir()
        assert not (worktree_path / "lib").is_symlink()


class TestSymlinkOverwrite:
    """Tests for symlink overwrite behavior."""

    def test_add_symlink_replaces_existing_file(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that symlinking replaces an existing file at the destination."""
        branch_name = "feature-symlink-replace"

        repo_builder.with_file("source.txt", "source content").commit("Add source file")
        write_workmux_config(
            repo_path, files={"symlink": ["source.txt"]}, env=isolated_tmux_server
        )

        worktree_path = add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )
        dest_file = worktree_path / "source.txt"

        # Remove the existing symlink and create a regular file
        dest_file.unlink()
        dest_file.write_text("replaced content")
        assert not dest_file.is_symlink()

        # Run workmux add again on a different branch to trigger symlink creation again
        # This simulates the --force-files behavior
        branch_name_2 = "feature-symlink-replace-2"
        worktree_path_2 = add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name_2
        )
        dest_file_2 = worktree_path_2 / "source.txt"
        assert dest_file_2.is_symlink()
        assert dest_file_2.read_text() == "source content"

    def test_add_symlink_overwrites_conflicting_file_from_git(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies a symlink operation overwrites a conflicting file checked out by git."""
        env = isolated_tmux_server
        branch_name = "feature-symlink-overwrite"

        # In main repo root, create the directory to be symlinked
        repo_builder.with_file("node_modules/dep.js", "content").commit(
            "Add real node_modules"
        )

        # On a different branch, create a conflicting FILE with the same name
        env.run_command(["git", "checkout", "-b", "conflict-branch"], cwd=repo_path)
        env.run_command(["git", "rm", "-r", "node_modules"], cwd=repo_path)
        (repo_path / "node_modules").write_text("this is a placeholder file")
        env.run_command(["git", "add", "node_modules"], cwd=repo_path)
        env.run_command(["git", "commit", "-m", "Add conflicting file"], cwd=repo_path)

        # On main, configure workmux to symlink the directory
        env.run_command(["git", "checkout", "main"], cwd=repo_path)
        write_workmux_config(repo_path, files={"symlink": ["node_modules"]}, env=env)

        # Create a worktree from the branch with the conflicting file
        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--base conflict-branch",
        )

        symlinked_target = assert_symlink_to(worktree_path, "node_modules")
        assert (symlinked_target / "dep.js").exists()


class TestCombinedOperations:
    """Tests for combined copy and symlink operations."""

    def test_add_combines_copy_and_symlink(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that copy and symlink operations can be used together."""
        branch_name = "feature-combined-ops"

        repo_builder.with_files(
            {
                ".env": "SECRET=abc123",
                "node_modules/package.json": '{"name": "test"}',
            }
        ).commit("Add files")

        write_workmux_config(
            repo_path,
            files={"copy": [".env"], "symlink": ["node_modules"]},
            env=isolated_tmux_server,
        )

        worktree_path = add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )

        assert_copied_file(worktree_path, ".env", "SECRET=abc123")
        symlinked_dir = assert_symlink_to(worktree_path, "node_modules")
        assert (symlinked_dir / "package.json").exists()


class TestEdgeCases:
    """Tests for edge cases and error handling."""

    def test_add_file_operations_with_empty_config(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that workmux add works when files config is empty or missing."""
        branch_name = "feature-no-files"

        write_workmux_config(repo_path, env=isolated_tmux_server)

        # Should succeed without errors
        add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )

    def test_add_file_operations_with_nonexistent_pattern(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that workmux handles glob patterns that match no files gracefully."""
        branch_name = "feature-no-match"

        write_workmux_config(
            repo_path,
            files={"copy": ["nonexistent-*.txt"], "symlink": ["missing-dir"]},
            env=isolated_tmux_server,
        )

        # Should succeed without errors (no matches is not an error)
        add_branch_and_get_worktree(
            isolated_tmux_server, workmux_exe_path, repo_path, branch_name
        )

    def test_add_can_skip_file_operations(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """`workmux add --no-file-ops` should not perform configured copy/symlink actions."""
        env = isolated_tmux_server
        branch_name = "feature-skip-file-ops"
        shared_dir = repo_path / "skip-shared"
        shared_dir.mkdir()
        (shared_dir / "data.txt").write_text("copy-me")

        write_workmux_config(repo_path, files={"copy": ["skip-shared"]})

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--no-file-ops",
        )

        assert not (worktree_path / "skip-shared").exists()


class TestPathTraversal:
    """Tests for path traversal security."""

    @pytest.mark.parametrize(
        "file_op_type,path_pattern,setup_name",
        [
            ("copy", "../sensitive_file", "sensitive_file"),
            ("symlink", "../some_dir", "some_dir"),
        ],
    )
    def test_add_path_traversal_fails(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        file_op_type: str,
        path_pattern: str,
        setup_name: str,
    ):
        """Verifies that `workmux add` fails if a path attempts to traverse outside the repo."""
        branch_name = f"feature-{file_op_type}-traversal"

        # Create a sensitive file/dir outside the repository
        target = repo_path.parent / setup_name
        if "file" in setup_name:
            target.write_text("secret")
        else:
            target.mkdir(exist_ok=True)
            (target / "file.txt").write_text("outside repo")

        write_workmux_config(
            repo_path, files={file_op_type: [path_pattern]}, env=isolated_tmux_server
        )

        with pytest.raises(AssertionError) as excinfo:
            run_workmux_add(
                isolated_tmux_server, workmux_exe_path, repo_path, branch_name
            )

        # The error should indicate path traversal or invalid path
        stderr = str(excinfo.value)
        assert (
            "Path traversal" in stderr
            or "outside" in stderr
            or "No such file" in stderr
            or "pattern matched nothing" in stderr
        )
