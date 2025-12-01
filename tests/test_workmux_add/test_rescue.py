"""Tests for `workmux add --with-changes` (rescue) functionality."""

import stat
from pathlib import Path

from ..conftest import (
    TmuxEnvironment,
    RepoBuilder,
    get_window_name,
    get_worktree_path,
    run_workmux_command,
    write_workmux_config,
)


class TestRescueBasic:
    """Basic tests for --with-changes functionality."""

    def test_rescue_moves_uncommitted_changes_to_new_worktree(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes` moves uncommitted changes to a new worktree."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-basic"
        test_file = repo_path / "uncommitted.txt"

        write_workmux_config(repo_path)

        # Create uncommitted changes in the main worktree
        test_file.write_text("uncommitted content")
        env.run_command(["git", "add", "uncommitted.txt"], cwd=repo_path)

        # Run add --with-changes command
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name}",
        )

        # Verify new worktree was created
        worktree_path = get_worktree_path(repo_path, branch_name)
        assert worktree_path.is_dir()

        # Verify changes are in the new worktree
        rescued_file = worktree_path / "uncommitted.txt"
        assert rescued_file.exists()
        assert rescued_file.read_text() == "uncommitted content"

        # Verify original worktree is clean (ignoring .workmux.yaml which is untracked)
        status_result = env.run_command(["git", "status", "--porcelain"], cwd=repo_path)
        status_lines = [
            line
            for line in status_result.stdout.strip().split("\n")
            if line and not line.endswith(".workmux.yaml")
        ]
        assert len(status_lines) == 0, (
            f"Original worktree should be clean, but has: {status_lines}"
        )

    def test_rescue_creates_tmux_window(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes` creates a tmux window."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-tmux"
        window_name = get_window_name(branch_name)

        write_workmux_config(repo_path)

        # Create uncommitted changes
        test_file = repo_path / "test.txt"
        test_file.write_text("test")
        env.run_command(["git", "add", "test.txt"], cwd=repo_path)

        # Run rescue
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name}",
        )

        # Verify tmux window exists
        result = env.tmux(["list-windows", "-F", "#{window_name}"])
        existing_windows = [w for w in result.stdout.strip().split("\n") if w]
        assert window_name in existing_windows


class TestRescueUntrackedFiles:
    """Tests for --with-changes with untracked files."""

    def test_rescue_with_untracked_files(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes -u` includes untracked files."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-untracked"
        untracked_file = repo_path / "new_file.txt"

        write_workmux_config(repo_path)

        # Create an untracked file
        untracked_file.write_text("new content")

        # Run add --with-changes command with --include-untracked
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name} -u",
        )

        # Verify new worktree was created
        worktree_path = get_worktree_path(repo_path, branch_name)
        assert worktree_path.is_dir()

        # Verify untracked file is in the new worktree
        rescued_file = worktree_path / "new_file.txt"
        assert rescued_file.exists()
        assert rescued_file.read_text() == "new content"

        # Verify original worktree doesn't have the file
        assert not (repo_path / "new_file.txt").exists()

    def test_rescue_without_untracked_flag_leaves_untracked_files(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that add --with-changes without -u flag doesn't move untracked files."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-no-untracked"
        tracked_file = repo_path / "tracked.txt"
        untracked_file = repo_path / "untracked.txt"

        write_workmux_config(repo_path)

        # Create both tracked and untracked changes
        tracked_file.write_text("tracked content")
        env.run_command(["git", "add", "tracked.txt"], cwd=repo_path)
        untracked_file.write_text("untracked content")

        # Run add --with-changes WITHOUT --include-untracked
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name}",
        )

        worktree_path = get_worktree_path(repo_path, branch_name)

        # Verify tracked file was moved
        assert (worktree_path / "tracked.txt").exists()

        # Verify untracked file was NOT moved
        assert not (worktree_path / "untracked.txt").exists()

        # Verify original still has untracked file
        assert (repo_path / "untracked.txt").exists()

    def test_rescue_with_only_untracked_files(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that add --with-changes -u works when only untracked files exist."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-only-untracked"

        write_workmux_config(repo_path)

        # Commit the config to make sure there are no tracked changes
        env.run_command(["git", "add", ".workmux.yaml"], cwd=repo_path)
        env.run_command(["git", "commit", "-m", "Add config"], cwd=repo_path)

        # Create ONLY untracked files (no staged or modified files)
        untracked1 = repo_path / "new1.txt"
        untracked2 = repo_path / "new2.txt"
        untracked1.write_text("untracked content 1")
        untracked2.write_text("untracked content 2")

        # Verify we have only untracked files
        status_result = env.run_command(["git", "status", "--porcelain"], cwd=repo_path)
        status_lines = status_result.stdout.strip().split("\n")
        for line in status_lines:
            assert line.startswith("??"), f"Expected only untracked files, got: {line}"

        # Run add --with-changes with -u flag (should succeed)
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name} -u",
        )

        worktree_path = get_worktree_path(repo_path, branch_name)

        # Verify worktree was created
        assert worktree_path.is_dir()

        # Verify untracked files are in the new worktree
        assert (worktree_path / "new1.txt").exists()
        assert (worktree_path / "new2.txt").exists()
        assert (worktree_path / "new1.txt").read_text() == "untracked content 1"
        assert (worktree_path / "new2.txt").read_text() == "untracked content 2"

        # Verify original worktree doesn't have the files
        assert not (repo_path / "new1.txt").exists()
        assert not (repo_path / "new2.txt").exists()


class TestRescueMixedChanges:
    """Tests for --with-changes with mixed staged/unstaged changes."""

    def test_rescue_with_both_staged_and_unstaged_changes(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that add --with-changes handles both staged and unstaged changes."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-mixed"

        write_workmux_config(repo_path)

        # Create staged changes
        staged_file = repo_path / "staged.txt"
        staged_file.write_text("staged content")
        env.run_command(["git", "add", "staged.txt"], cwd=repo_path)

        # Create unstaged changes
        unstaged_file = repo_path / "unstaged.txt"
        unstaged_file.write_text("unstaged content")

        # Run add --with-changes with -u to include untracked files
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name} -u",
        )

        worktree_path = get_worktree_path(repo_path, branch_name)

        # Verify both files are in the new worktree
        assert (worktree_path / "staged.txt").exists()
        assert (worktree_path / "unstaged.txt").exists()

        # Verify original worktree is clean (ignoring .workmux.yaml)
        status_result = env.run_command(["git", "status", "--porcelain"], cwd=repo_path)
        status_lines = [
            line
            for line in status_result.stdout.strip().split("\n")
            if line and not line.endswith(".workmux.yaml")
        ]
        assert len(status_lines) == 0, (
            f"Original worktree should be clean, but has: {status_lines}"
        )

    def test_rescue_handles_modified_tracked_files(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that add --with-changes handles modifications to tracked files."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-modified"

        write_workmux_config(repo_path)

        # Create and commit a file
        repo_builder.with_file("tracked.txt", "original content").commit(
            "Add tracked file"
        )

        # Modify the file
        (repo_path / "tracked.txt").write_text("modified content")

        # Run rescue
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name}",
        )

        worktree_path = get_worktree_path(repo_path, branch_name)
        rescued_file = worktree_path / "tracked.txt"

        # Verify modification is in the new worktree
        assert rescued_file.read_text() == "modified content"

        # Verify original file is reset to committed version
        assert (repo_path / "tracked.txt").read_text() == "original content"


class TestRescueFlags:
    """Tests for --with-changes with additional flags."""

    def test_rescue_respects_no_hooks_flag(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes --no-hooks` skips post-create hooks."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-no-hooks"
        hook_file = "hook_executed.txt"

        write_workmux_config(repo_path, post_create=[f"touch {hook_file}"])

        # Create uncommitted changes
        test_file = repo_path / "test.txt"
        test_file.write_text("test")
        env.run_command(["git", "add", "test.txt"], cwd=repo_path)

        # Run add --with-changes with --no-hooks
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name} --no-hooks",
        )

        worktree_path = get_worktree_path(repo_path, branch_name)

        # Verify hook was NOT executed
        assert not (worktree_path / hook_file).exists()

    def test_rescue_respects_no_file_ops_flag(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes --no-file-ops` skips file operations."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-no-file-ops"

        # Create a directory to copy (not in git)
        shared_dir = repo_path / "shared-data"
        shared_dir.mkdir()
        (shared_dir / "file.txt").write_text("shared content")

        write_workmux_config(repo_path, files={"copy": ["shared-data"]})

        # Create uncommitted changes
        test_file = repo_path / "test.txt"
        test_file.write_text("test")
        env.run_command(["git", "add", "test.txt"], cwd=repo_path)

        # Run add --with-changes with --no-file-ops
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name} --no-file-ops",
        )

        worktree_path = get_worktree_path(repo_path, branch_name)

        # Verify worktree was created successfully
        assert worktree_path.is_dir()

        # Verify file operations were skipped - the directory should NOT have been copied
        assert not (worktree_path / "shared-data").exists()

    def test_rescue_with_background_flag(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes --background` creates window without switching."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-background"
        initial_window = "initial"

        write_workmux_config(repo_path)

        # Create an initial window and remember it
        env.tmux(["new-window", "-n", initial_window])
        env.tmux(["select-window", "-t", initial_window])

        # Get current window before running rescue
        current_before = env.tmux(["display-message", "-p", "#{window_name}"])
        assert initial_window in current_before.stdout

        # Create uncommitted changes
        test_file = repo_path / "test.txt"
        test_file.write_text("test")
        env.run_command(["git", "add", "test.txt"], cwd=repo_path)

        # Run add --with-changes with --background
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name} --background",
        )

        # Verify worktree was created
        worktree_path = get_worktree_path(repo_path, branch_name)
        assert worktree_path.is_dir()

        # Verify the new window exists
        window_name = get_window_name(branch_name)
        result = env.tmux(["list-windows", "-F", "#{window_name}"])
        existing_windows = [w for w in result.stdout.strip().split("\n") if w]
        assert window_name in existing_windows

        # Verify we're still on the initial window (didn't switch)
        current_after = env.tmux(["display-message", "-p", "#{window_name}"])
        assert initial_window in current_after.stdout


class TestRescueHooks:
    """Tests for hook execution with --with-changes."""

    def test_rescue_executes_post_create_hooks(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that add --with-changes executes post_create hooks by default."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-hooks"
        hook_file = "hook_executed.txt"

        write_workmux_config(repo_path, post_create=[f"touch {hook_file}"])

        # Create uncommitted changes
        test_file = repo_path / "test.txt"
        test_file.write_text("test")
        env.run_command(["git", "add", "test.txt"], cwd=repo_path)

        # Run rescue
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name}",
        )

        worktree_path = get_worktree_path(repo_path, branch_name)

        # Verify hook was executed
        assert (worktree_path / hook_file).exists()


class TestRescueFileModes:
    """Tests for file mode preservation with --with-changes."""

    def test_rescue_preserves_file_modes(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that add --with-changes preserves file permissions."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-modes"

        write_workmux_config(repo_path)

        # Create an executable file
        script_file = repo_path / "script.sh"
        script_file.write_text("#!/bin/bash\necho 'hello'")
        script_file.chmod(0o755)
        env.run_command(["git", "add", "script.sh"], cwd=repo_path)

        # Run rescue
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name} -u",
        )

        worktree_path = get_worktree_path(repo_path, branch_name)
        rescued_script = worktree_path / "script.sh"

        # Verify file exists and is executable
        assert rescued_script.exists()
        assert rescued_script.stat().st_mode & stat.S_IXUSR


class TestRescueGitignore:
    """Tests for gitignore handling with --with-changes."""

    def test_rescue_with_gitignored_files(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that add --with-changes -u does NOT include gitignored files."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-gitignored"

        write_workmux_config(repo_path)

        # Create .gitignore
        repo_builder.with_file(".gitignore", "*.log\nignored_dir/\n").commit(
            "Add gitignore"
        )

        # Create some tracked changes
        tracked_file = repo_path / "tracked.txt"
        tracked_file.write_text("tracked content")
        env.run_command(["git", "add", "tracked.txt"], cwd=repo_path)

        # Create gitignored files
        log_file = repo_path / "test.log"
        log_file.write_text("log content")

        ignored_dir = repo_path / "ignored_dir"
        ignored_dir.mkdir()
        (ignored_dir / "file.txt").write_text("ignored content")

        # Create a regular untracked file (not ignored)
        untracked_file = repo_path / "untracked.txt"
        untracked_file.write_text("untracked content")

        # Run add --with-changes with -u
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name} -u",
        )

        worktree_path = get_worktree_path(repo_path, branch_name)

        # Verify tracked file was moved
        assert (worktree_path / "tracked.txt").exists()

        # Verify untracked file was moved
        assert (worktree_path / "untracked.txt").exists()

        # Verify gitignored files were NOT moved (git stash -u doesn't include ignored files)
        assert not (worktree_path / "test.log").exists()
        assert not (worktree_path / "ignored_dir").exists()

        # Verify gitignored files still exist in original worktree
        assert (repo_path / "test.log").exists()
        assert (repo_path / "ignored_dir" / "file.txt").exists()


class TestRescueErrors:
    """Tests for error handling with --with-changes."""

    def test_rescue_fails_with_no_uncommitted_changes(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that `workmux add --with-changes` fails when there are no uncommitted changes."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-no-changes"

        write_workmux_config(repo_path)

        # Commit the config file so working directory is truly clean
        env.run_command(["git", "add", ".workmux.yaml"], cwd=repo_path)
        env.run_command(["git", "commit", "-m", "Add config"], cwd=repo_path)

        # Ensure working directory is clean
        status_result = env.run_command(["git", "status", "--porcelain"], cwd=repo_path)
        assert status_result.stdout.strip() == ""

        # Run add --with-changes command - should fail
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name}",
            expect_fail=True,
        )

        assert "No uncommitted changes to move" in result.stderr

    def test_rescue_fails_when_branch_exists(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes` fails if the target branch already exists."""
        env = isolated_tmux_server
        branch_name = "existing-branch"

        write_workmux_config(repo_path)

        # Create the branch
        env.run_command(["git", "checkout", "-b", branch_name], cwd=repo_path)
        env.run_command(["git", "checkout", "main"], cwd=repo_path)

        # Create uncommitted changes
        test_file = repo_path / "test.txt"
        test_file.write_text("test")
        env.run_command(["git", "add", "test.txt"], cwd=repo_path)

        # Run add --with-changes command - should fail
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name}",
            expect_fail=True,
        )

        assert f"Branch '{branch_name}' already exists" in result.stderr

    def test_rescue_fails_with_only_untracked_files_without_u_flag(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that add --with-changes fails when only untracked files exist and -u is not used."""
        env = isolated_tmux_server
        branch_name = "feature-rescue-fail-untracked"

        write_workmux_config(repo_path)

        # Commit the config to make sure there are no tracked changes
        env.run_command(["git", "add", ".workmux.yaml"], cwd=repo_path)
        env.run_command(["git", "commit", "-m", "Add config"], cwd=repo_path)

        # Create ONLY untracked files
        untracked_file = repo_path / "new.txt"
        untracked_file.write_text("untracked content")

        # Run add --with-changes WITHOUT -u flag (should fail)
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add --with-changes {branch_name}",
            expect_fail=True,
        )

        assert "No uncommitted changes to move" in result.stderr
