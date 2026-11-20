"""Tests for `workmux add --with-changes` (rescue) functionality."""

import os
import pty
import select
import shlex
import stat
import subprocess
import time
from pathlib import Path

import pytest

from ..conftest import (
    FakeAgentInstaller,
    MuxEnvironment,
    RepoBuilder,
    ShellCommands,
    TmuxEnvironment,
    assert_prompt_file_contents,
    assert_window_exists,
    create_commit,
    file_for_commit,
    get_window_name,
    get_worktree_path,
    run_workmux_command,
    wait_for_file,
    write_global_workmux_config,
    write_workmux_config,
)


class TestRescueBasic:
    """Basic tests for --with-changes functionality."""

    def test_rescue_moves_uncommitted_changes_to_new_worktree(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes` moves uncommitted changes to a new worktree."""
        env = mux_server
        branch_name = "feature-rescue-basic"
        test_file = mux_repo_path / "uncommitted.txt"

        write_workmux_config(mux_repo_path)

        # Create uncommitted changes in the main worktree
        test_file.write_text("uncommitted content")
        env.run_command(["git", "add", "uncommitted.txt"], cwd=mux_repo_path)

        # Run add --with-changes command
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name}",
        )

        # Verify new worktree was created
        worktree_path = get_worktree_path(mux_repo_path, branch_name)
        assert worktree_path.is_dir()

        # Verify changes are in the new worktree
        rescued_file = worktree_path / "uncommitted.txt"
        assert rescued_file.exists()
        assert rescued_file.read_text() == "uncommitted content"

        # Verify original worktree is clean (ignoring .workmux.yaml which is untracked)
        status_result = env.run_command(
            ["git", "status", "--porcelain"], cwd=mux_repo_path
        )
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
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes` creates a tmux window."""
        env = mux_server
        branch_name = "feature-rescue-tmux"
        window_name = get_window_name(branch_name)

        write_workmux_config(mux_repo_path)

        # Create uncommitted changes
        test_file = mux_repo_path / "test.txt"
        test_file.write_text("test")
        env.run_command(["git", "add", "test.txt"], cwd=mux_repo_path)

        # Run rescue
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name}",
        )

        # Verify multiplexer window exists
        existing_windows = env.list_windows()
        assert window_name in existing_windows


class TestRescueUntrackedFiles:
    """Tests for --with-changes with untracked files."""

    def test_rescue_with_untracked_files(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes -u` includes untracked files."""
        env = mux_server
        branch_name = "feature-rescue-untracked"
        untracked_file = mux_repo_path / "new_file.txt"

        write_workmux_config(mux_repo_path)

        # Create an untracked file
        untracked_file.write_text("new content")

        # Run add --with-changes command with --include-untracked
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name} -u",
        )

        # Verify new worktree was created
        worktree_path = get_worktree_path(mux_repo_path, branch_name)
        assert worktree_path.is_dir()

        # Verify untracked file is in the new worktree
        rescued_file = worktree_path / "new_file.txt"
        assert rescued_file.exists()
        assert rescued_file.read_text() == "new content"

        # Verify original worktree doesn't have the file
        assert not (mux_repo_path / "new_file.txt").exists()

    def test_rescue_without_untracked_flag_leaves_untracked_files(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that add --with-changes without -u flag doesn't move untracked files."""
        env = mux_server
        branch_name = "feature-rescue-no-untracked"
        tracked_file = mux_repo_path / "tracked.txt"
        untracked_file = mux_repo_path / "untracked.txt"

        write_workmux_config(mux_repo_path)

        # Create both tracked and untracked changes
        tracked_file.write_text("tracked content")
        env.run_command(["git", "add", "tracked.txt"], cwd=mux_repo_path)
        untracked_file.write_text("untracked content")

        # Run add --with-changes WITHOUT --include-untracked
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name}",
        )

        worktree_path = get_worktree_path(mux_repo_path, branch_name)

        # Verify tracked file was moved
        assert (worktree_path / "tracked.txt").exists()

        # Verify untracked file was NOT moved
        assert not (worktree_path / "untracked.txt").exists()

        # Verify original still has untracked file
        assert (mux_repo_path / "untracked.txt").exists()

    def test_rescue_with_only_untracked_files(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that add --with-changes -u works when only untracked files exist."""
        env = mux_server
        branch_name = "feature-rescue-only-untracked"

        write_workmux_config(mux_repo_path)

        # Commit the config to make sure there are no tracked changes
        env.run_command(["git", "add", ".workmux.yaml"], cwd=mux_repo_path)
        env.run_command(["git", "commit", "-m", "Add config"], cwd=mux_repo_path)

        # Create ONLY untracked files (no staged or modified files)
        untracked1 = mux_repo_path / "new1.txt"
        untracked2 = mux_repo_path / "new2.txt"
        untracked1.write_text("untracked content 1")
        untracked2.write_text("untracked content 2")

        # Verify we have only untracked files
        status_result = env.run_command(
            ["git", "status", "--porcelain"], cwd=mux_repo_path
        )
        status_lines = status_result.stdout.strip().split("\n")
        for line in status_lines:
            assert line.startswith("??"), f"Expected only untracked files, got: {line}"

        # Run add --with-changes with -u flag (should succeed)
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name} -u",
        )

        worktree_path = get_worktree_path(mux_repo_path, branch_name)

        # Verify worktree was created
        assert worktree_path.is_dir()

        # Verify untracked files are in the new worktree
        assert (worktree_path / "new1.txt").exists()
        assert (worktree_path / "new2.txt").exists()
        assert (worktree_path / "new1.txt").read_text() == "untracked content 1"
        assert (worktree_path / "new2.txt").read_text() == "untracked content 2"

        # Verify original worktree doesn't have the files
        assert not (mux_repo_path / "new1.txt").exists()
        assert not (mux_repo_path / "new2.txt").exists()


class TestRescueMixedChanges:
    """Tests for --with-changes with mixed staged/unstaged changes."""

    def test_rescue_with_both_staged_and_unstaged_changes(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that add --with-changes handles both staged and unstaged changes."""
        env = mux_server
        branch_name = "feature-rescue-mixed"

        write_workmux_config(mux_repo_path)

        # Create staged changes
        staged_file = mux_repo_path / "staged.txt"
        staged_file.write_text("staged content")
        env.run_command(["git", "add", "staged.txt"], cwd=mux_repo_path)

        # Create unstaged changes
        unstaged_file = mux_repo_path / "unstaged.txt"
        unstaged_file.write_text("unstaged content")

        # Run add --with-changes with -u to include untracked files
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name} -u",
        )

        worktree_path = get_worktree_path(mux_repo_path, branch_name)

        # Verify both files are in the new worktree
        assert (worktree_path / "staged.txt").exists()
        assert (worktree_path / "unstaged.txt").exists()

        # Verify original worktree is clean (ignoring .workmux.yaml)
        status_result = env.run_command(
            ["git", "status", "--porcelain"], cwd=mux_repo_path
        )
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
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that add --with-changes handles modifications to tracked files."""
        env = mux_server
        branch_name = "feature-rescue-modified"

        write_workmux_config(mux_repo_path)

        # Create and commit a file
        repo_builder.with_file("tracked.txt", "original content").commit(
            "Add tracked file"
        )

        # Modify the file
        (mux_repo_path / "tracked.txt").write_text("modified content")

        # Run rescue
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name}",
        )

        worktree_path = get_worktree_path(mux_repo_path, branch_name)
        rescued_file = worktree_path / "tracked.txt"

        # Verify modification is in the new worktree
        assert rescued_file.read_text() == "modified content"

        # Verify original file is reset to committed version
        assert (mux_repo_path / "tracked.txt").read_text() == "original content"


class TestRescueFlags:
    """Tests for --with-changes with additional flags."""

    def test_rescue_respects_no_hooks_flag(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes --no-hooks` skips post-create hooks."""
        env = mux_server
        branch_name = "feature-rescue-no-hooks"
        hook_file = "hook_executed.txt"

        write_workmux_config(mux_repo_path, post_create=[f"touch {hook_file}"])

        # Create uncommitted changes
        test_file = mux_repo_path / "test.txt"
        test_file.write_text("test")
        env.run_command(["git", "add", "test.txt"], cwd=mux_repo_path)

        # Run add --with-changes with --no-hooks
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name} --no-hooks",
        )

        worktree_path = get_worktree_path(mux_repo_path, branch_name)

        # Verify hook was NOT executed
        assert not (worktree_path / hook_file).exists()

    def test_rescue_respects_no_file_ops_flag(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes --no-file-ops` skips file operations."""
        env = mux_server
        branch_name = "feature-rescue-no-file-ops"

        # Create a directory to copy (not in git)
        shared_dir = mux_repo_path / "shared-data"
        shared_dir.mkdir()
        (shared_dir / "file.txt").write_text("shared content")

        write_workmux_config(mux_repo_path, files={"copy": ["shared-data"]})

        # Create uncommitted changes
        test_file = mux_repo_path / "test.txt"
        test_file.write_text("test")
        env.run_command(["git", "add", "test.txt"], cwd=mux_repo_path)

        # Run add --with-changes with --no-file-ops
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name} --no-file-ops",
        )

        worktree_path = get_worktree_path(mux_repo_path, branch_name)

        # Verify worktree was created successfully
        assert worktree_path.is_dir()

        # Verify file operations were skipped - the directory should NOT have been copied
        assert not (worktree_path / "shared-data").exists()

    # WezTerm: get_current_window() returns None because WezTerm doesn't expose
    # session-level window focus via CLI - GUI focus is controlled by window manager.
    @pytest.mark.tmux_only
    def test_rescue_with_background_flag(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes --background` creates window without switching."""
        env = mux_server
        branch_name = "feature-rescue-background"
        initial_window = "initial"

        write_workmux_config(repo_path)

        # Create an initial window and remember it
        env.new_window(initial_window)
        env.select_window(initial_window)

        # Get current window before running rescue
        current_before = env.get_current_window()
        assert current_before == initial_window

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
        existing_windows = env.list_windows()
        assert window_name in existing_windows

        # Verify we're still on the initial window (didn't switch)
        current_after = env.get_current_window()
        assert current_after == initial_window


class TestRescueHooks:
    """Tests for hook execution with --with-changes."""

    def test_rescue_executes_post_create_hooks(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that add --with-changes executes post_create hooks by default."""
        env = mux_server
        branch_name = "feature-rescue-hooks"
        hook_file = "hook_executed.txt"

        write_workmux_config(mux_repo_path, post_create=[f"touch {hook_file}"])

        # Create uncommitted changes
        test_file = mux_repo_path / "test.txt"
        test_file.write_text("test")
        env.run_command(["git", "add", "test.txt"], cwd=mux_repo_path)

        # Run rescue
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name}",
        )

        worktree_path = get_worktree_path(mux_repo_path, branch_name)

        # Verify hook was executed
        assert (worktree_path / hook_file).exists()


class TestRescueFileModes:
    """Tests for file mode preservation with --with-changes."""

    def test_rescue_preserves_file_modes(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that add --with-changes preserves file permissions."""
        env = mux_server
        branch_name = "feature-rescue-modes"

        write_workmux_config(mux_repo_path)

        # Create an executable file
        script_file = mux_repo_path / "script.sh"
        script_file.write_text("#!/bin/bash\necho 'hello'")
        script_file.chmod(0o755)
        env.run_command(["git", "add", "script.sh"], cwd=mux_repo_path)

        # Run rescue
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name} -u",
        )

        worktree_path = get_worktree_path(mux_repo_path, branch_name)
        rescued_script = worktree_path / "script.sh"

        # Verify file exists and is executable
        assert rescued_script.exists()
        assert rescued_script.stat().st_mode & stat.S_IXUSR


class TestRescueGitignore:
    """Tests for gitignore handling with --with-changes."""

    def test_rescue_with_gitignored_files(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that add --with-changes -u does NOT include gitignored files."""
        env = mux_server
        branch_name = "feature-rescue-gitignored"

        write_workmux_config(mux_repo_path)

        # Create .gitignore
        repo_builder.with_file(".gitignore", "*.log\nignored_dir/\n").commit(
            "Add gitignore"
        )

        # Create some tracked changes
        tracked_file = mux_repo_path / "tracked.txt"
        tracked_file.write_text("tracked content")
        env.run_command(["git", "add", "tracked.txt"], cwd=mux_repo_path)

        # Create gitignored files
        log_file = mux_repo_path / "test.log"
        log_file.write_text("log content")

        ignored_dir = mux_repo_path / "ignored_dir"
        ignored_dir.mkdir()
        (ignored_dir / "file.txt").write_text("ignored content")

        # Create a regular untracked file (not ignored)
        untracked_file = mux_repo_path / "untracked.txt"
        untracked_file.write_text("untracked content")

        # Run add --with-changes with -u
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name} -u",
        )

        worktree_path = get_worktree_path(mux_repo_path, branch_name)

        # Verify tracked file was moved
        assert (worktree_path / "tracked.txt").exists()

        # Verify untracked file was moved
        assert (worktree_path / "untracked.txt").exists()

        # Verify gitignored files were NOT moved (git stash -u doesn't include ignored files)
        assert not (worktree_path / "test.log").exists()
        assert not (worktree_path / "ignored_dir").exists()

        # Verify gitignored files still exist in original worktree
        assert (mux_repo_path / "test.log").exists()
        assert (mux_repo_path / "ignored_dir" / "file.txt").exists()


class TestRescueErrors:
    """Tests for error handling with --with-changes."""

    def test_rescue_fails_with_no_uncommitted_changes(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that `workmux add --with-changes` fails when there are no uncommitted changes."""
        env = mux_server
        branch_name = "feature-rescue-no-changes"

        write_workmux_config(mux_repo_path)

        # Commit the config file so working directory is truly clean
        env.run_command(["git", "add", ".workmux.yaml"], cwd=mux_repo_path)
        env.run_command(["git", "commit", "-m", "Add config"], cwd=mux_repo_path)

        # Ensure working directory is clean
        status_result = env.run_command(
            ["git", "status", "--porcelain"], cwd=mux_repo_path
        )
        assert status_result.stdout.strip() == ""

        # Run add --with-changes command - should fail
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name}",
            expect_fail=True,
        )

        assert "No uncommitted changes to move" in result.stderr

    def test_rescue_fails_when_branch_exists(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that `workmux add --with-changes` fails if the target branch already exists."""
        env = mux_server
        branch_name = "existing-branch"

        write_workmux_config(mux_repo_path)

        # Create the branch
        env.run_command(["git", "checkout", "-b", branch_name], cwd=mux_repo_path)
        env.run_command(["git", "checkout", "main"], cwd=mux_repo_path)

        # Create uncommitted changes
        test_file = mux_repo_path / "test.txt"
        test_file.write_text("test")
        env.run_command(["git", "add", "test.txt"], cwd=mux_repo_path)

        # Run add --with-changes command - should fail
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name}",
            expect_fail=True,
        )

        assert f"Branch '{branch_name}' already exists" in result.stderr

    def test_rescue_fails_with_only_untracked_files_without_u_flag(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Verifies that add --with-changes fails when only untracked files exist and -u is not used."""
        env = mux_server
        branch_name = "feature-rescue-fail-untracked"

        write_workmux_config(mux_repo_path)

        # Commit the config to make sure there are no tracked changes
        env.run_command(["git", "add", ".workmux.yaml"], cwd=mux_repo_path)
        env.run_command(["git", "commit", "-m", "Add config"], cwd=mux_repo_path)

        # Create ONLY untracked files
        untracked_file = mux_repo_path / "new.txt"
        untracked_file.write_text("untracked content")

        # Run add --with-changes WITHOUT -u flag (should fail)
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add --with-changes {branch_name}",
            expect_fail=True,
        )

        assert "No uncommitted changes to move" in result.stderr


class TestRescuePatch:
    """Tests for --with-changes --patch, which needs a terminal for hunk selection."""

    @pytest.mark.tmux_only
    def test_rescue_patch_keeps_unselected_hunks_in_source(
        self,
        mux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Hunks left out of the --patch selection stay in the original worktree."""
        env = mux_server
        branch_name = "feature-rescue-patch"
        write_workmux_config(mux_repo_path)

        for name in ("a.txt", "b.txt"):
            (mux_repo_path / name).write_text(f"{name} original\n")
        env.run_command(["git", "add", "a.txt", "b.txt"], cwd=mux_repo_path)
        env.run_command(["git", "commit", "-m", "Add a and b"], cwd=mux_repo_path)
        (mux_repo_path / "a.txt").write_text("a.txt changed\n")
        (mux_repo_path / "b.txt").write_text("b.txt changed\n")

        server_pid = env.tmux(["display-message", "-p", "#{pid}"]).stdout.strip()
        proc_env = {
            **env.env,
            "TMUX": f"{env.socket_path},{server_pid},0",
            "TMUX_PANE": str(env.runner_pane_id),
        }
        master, slave = pty.openpty()
        proc = subprocess.Popen(
            [str(workmux_exe_path), "add", branch_name, "--with-changes", "--patch"],
            cwd=mux_repo_path,
            env=proc_env,
            stdin=slave,
            stdout=slave,
            stderr=slave,
        )
        os.close(slave)

        # git asks once per file, in path order: skip a.txt, take b.txt.
        answers = [b"n\n", b"y\n"]
        output = b""
        deadline = time.monotonic() + 30
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([master], [], [], 0.5)
                if ready:
                    try:
                        chunk = os.read(master, 4096)
                    except OSError:
                        break
                    if not chunk:
                        break
                    output += chunk
                    asked = output.count(b"Stash this hunk")
                    sent = 2 - len(answers)
                    if answers and asked > sent:
                        os.write(master, answers.pop(0))
                elif proc.poll() is not None:
                    break
            returncode = proc.wait(timeout=10)
        finally:
            if proc.poll() is None:
                proc.kill()
            os.close(master)

        text = output.decode(errors="replace")
        assert returncode == 0, text
        assert not answers, f"git did not ask for every hunk:\n{text}"

        worktree_path = get_worktree_path(mux_repo_path, branch_name)
        assert (worktree_path / "b.txt").read_text() == "b.txt changed\n"
        assert (worktree_path / "a.txt").read_text() == "a.txt original\n"

        assert (mux_repo_path / "a.txt").read_text() == "a.txt changed\n"
        assert (mux_repo_path / "b.txt").read_text() == "b.txt original\n"
        assert "now clean" not in text

        stash_list = env.run_command(["git", "stash", "list"], cwd=mux_repo_path)
        assert stash_list.stdout.strip() == ""


def _git(env: MuxEnvironment, repo: Path, *args: str) -> str:
    return env.run_command(["git", *args], cwd=repo).stdout


def _repo_state(env: MuxEnvironment, repo: Path) -> dict[str, str]:
    """Everything a rejected --with-changes must leave untouched."""
    return {
        "branches": _git(env, repo, "branch", "--list"),
        "worktrees": _git(env, repo, "worktree", "list", "--porcelain"),
        "status": _git(env, repo, "status", "--porcelain"),
        "stash": _git(env, repo, "stash", "list"),
        "remote_refs": _git(env, repo, "for-each-ref", "refs/remotes"),
    }


def _commit_config(env: MuxEnvironment, repo: Path, **config) -> None:
    """Write and commit the config, so -u does not move it along with the changes."""
    write_workmux_config(repo, **config)
    env.run_command(["git", "add", ".workmux.yaml"], cwd=repo)
    env.run_command(["git", "commit", "-qm", "Add config"], cwd=repo)


def _make_changes(repo: Path) -> None:
    """A tracked modification plus an untracked file."""
    (repo / "tracked.txt").write_text("modified\n")
    (repo / "moved.txt").write_text("moved content\n")


def _prepare_repo(env: MuxEnvironment, repo: Path, **config) -> None:
    # The fake agents live in the repository directory; -u must not move them away.
    with (repo / ".git" / "info" / "exclude").open("a") as exclude:
        exclude.write("fake-bin/\n")
    (repo / "tracked.txt").write_text("original\n")
    env.run_command(["git", "add", "tracked.txt"], cwd=repo)
    env.run_command(["git", "commit", "-qm", "Add tracked file"], cwd=repo)
    _commit_config(env, repo, **config)
    _make_changes(repo)


class TestRescueRejectedCombinations:
    """--with-changes combinations that create more than one worktree or ignore HEAD."""

    @pytest.mark.parametrize(
        ("args", "message"),
        [
            ("add foo --with-changes --base main", "cannot be used with --base"),
            ("add --pr 123 --with-changes", "cannot be used with --pr"),
        ],
        ids=["base", "pr"],
    )
    def test_rejected_before_anything_changes(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        args: str,
        message: str,
    ):
        env = mux_server
        _prepare_repo(env, mux_repo_path)
        before = _repo_state(env, mux_repo_path)

        result = run_workmux_command(
            env, workmux_exe_path, mux_repo_path, args, expect_fail=True
        )

        assert message in result.stderr
        assert "relative to the current HEAD" in result.stderr
        # No fetch ran for --pr either: the remote refs are unchanged.
        assert _repo_state(env, mux_repo_path) == before

    def test_patch_with_untracked_is_rejected(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """git stash --patch cannot move untracked files, so -u is refused."""
        env = mux_server
        _prepare_repo(env, mux_repo_path)
        before = _repo_state(env, mux_repo_path)

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add foo -w --patch -u",
            expect_fail=True,
        )

        assert "--patch" in result.stderr
        assert "--include-untracked" in result.stderr
        assert _repo_state(env, mux_repo_path) == before

    def test_stdin_branch_input_is_rejected(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        _prepare_repo(env, mux_repo_path)
        before = _repo_state(env, mux_repo_path)

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add foo --with-changes",
            stdin_input="one\ntwo\n",
            expect_fail=True,
        )

        assert "creates a single worktree" in result.stderr
        assert "stdin" in result.stderr
        assert _repo_state(env, mux_repo_path) == before

    def test_frontmatter_foreach_is_rejected(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        _prepare_repo(env, mux_repo_path, panes=[{"command": "<agent>"}])
        prompt_file = mux_repo_path.parent / "prompt_with_matrix.md"
        prompt_file.write_text("---\nforeach:\n  env: [dev, prod]\n---\nTask\n")
        before = _repo_state(env, mux_repo_path)

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add foo -w -P {shlex.quote(str(prompt_file))}",
            expect_fail=True,
        )

        assert "creates a single worktree" in result.stderr
        assert "foreach" in result.stderr
        assert _repo_state(env, mux_repo_path) == before


def _install_recording_agent(installer: FakeAgentInstaller, name: str) -> None:
    """An agent that records its prompt argument and what it saw of the moved file."""
    installer.install(
        name,
        """#!/bin/sh
cat moved.txt > agent_saw.txt 2>/dev/null || echo missing > agent_saw.txt
printf '%s' "$*" > agent_args.txt
""",
    )


class TestRescueWithAddOptions:
    """Options of a plain `workmux add` keep working together with --with-changes."""

    def test_inline_prompt_reaches_agent_with_moved_files(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
        shell_cmd: ShellCommands,
    ):
        env = mux_server
        env.configure_default_shell(shell_cmd.path)
        _install_recording_agent(fake_agent_installer, "claude")
        _prepare_repo(
            env, mux_repo_path, agent="claude", panes=[{"command": "<agent>"}]
        )

        run_workmux_command(
            env, workmux_exe_path, mux_repo_path, 'add foo -w -u -p "do"'
        )

        worktree = get_worktree_path(mux_repo_path, "foo")
        assert_prompt_file_contents(env, "foo", "do", worktree)
        wait_for_file(
            env,
            worktree / "agent_args.txt",
            window_name=get_window_name("foo"),
            worktree_path=worktree,
        )
        assert (worktree / "agent_args.txt").read_text().endswith("do")
        assert (worktree / "agent_saw.txt").read_text() == "moved content\n"
        assert (worktree / "tracked.txt").read_text() == "modified\n"
        assert _git(env, mux_repo_path, "status", "--porcelain") == ""
        assert _git(env, mux_repo_path, "stash", "list") == ""

    def test_configured_base_branch_is_ignored(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        env.run_command(["git", "checkout", "-qb", "feature-x"], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, "Commit on feature-x")
        _prepare_repo(env, mux_repo_path, base_branch="auto")

        run_workmux_command(env, workmux_exe_path, mux_repo_path, "add foo -w")

        worktree = get_worktree_path(mux_repo_path, "foo")
        assert file_for_commit(worktree, "Commit on feature-x").exists()
        assert (worktree / "tracked.txt").read_text() == "modified\n"
        assert (
            _git(env, mux_repo_path, "rev-parse", "foo").strip()
            == _git(env, mux_repo_path, "rev-parse", "feature-x").strip()
        )
        base = _git(env, mux_repo_path, "config", "--local", "branch.foo.workmux-base")
        assert base.strip() == "feature-x"

    def test_single_agent_runs(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
        shell_cmd: ShellCommands,
    ):
        env = mux_server
        env.configure_default_shell(shell_cmd.path)
        _install_recording_agent(fake_agent_installer, "codex")
        _prepare_repo(env, mux_repo_path, panes=[{"command": "<agent>"}])

        run_workmux_command(
            env, workmux_exe_path, mux_repo_path, 'add foo -w -u -a codex -p "do"'
        )

        worktree = get_worktree_path(mux_repo_path, "foo")
        wait_for_file(
            env,
            worktree / "agent_args.txt",
            window_name=get_window_name("foo"),
            worktree_path=worktree,
        )
        assert "do" in (worktree / "agent_args.txt").read_text()
        assert (worktree / "agent_saw.txt").read_text() == "moved content\n"

    def test_explicit_name(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        _prepare_repo(env, mux_repo_path)

        run_workmux_command(
            env, workmux_exe_path, mux_repo_path, "add foo -w --name bar"
        )

        worktree = get_worktree_path(mux_repo_path, "bar")
        assert (worktree / "tracked.txt").read_text() == "modified\n"
        assert not get_worktree_path(mux_repo_path, "foo").exists()
        assert_window_exists(env, get_window_name("bar"))
        assert _git(env, worktree, "branch", "--show-current").strip() == "foo"

    def test_prompt_file_only(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
        shell_cmd: ShellCommands,
    ):
        env = mux_server
        env.configure_default_shell(shell_cmd.path)
        _install_recording_agent(fake_agent_installer, "claude")
        _prepare_repo(
            env, mux_repo_path, agent="claude", panes=[{"command": "<agent>"}]
        )

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            'add foo -w -u -p "do" --prompt-file-only',
        )

        worktree = get_worktree_path(mux_repo_path, "foo")
        assert_prompt_file_contents(env, "foo", "do", worktree)
        wait_for_file(
            env,
            worktree / "agent_args.txt",
            window_name=get_window_name("foo"),
            worktree_path=worktree,
        )
        assert (worktree / "agent_args.txt").read_text() == ""
        assert (worktree / "agent_saw.txt").read_text() == "moved content\n"

    def test_auto_name(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
        shell_cmd: ShellCommands,
    ):
        env = mux_server
        env.configure_default_shell(shell_cmd.path)
        _install_recording_agent(fake_agent_installer, "claude")
        write_global_workmux_config(
            env, auto_name={"command": "sh -c 'printf \"gen-branch\\n\"'"}
        )
        _prepare_repo(
            env, mux_repo_path, agent="claude", panes=[{"command": "<agent>"}]
        )

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            'add -A -w -u -p "fix login redirect"',
        )

        worktree = get_worktree_path(mux_repo_path, "gen-branch")
        assert (worktree / "tracked.txt").read_text() == "modified\n"
        assert_prompt_file_contents(env, "gen-branch", "fix login redirect", worktree)
        wait_for_file(
            env,
            worktree / "agent_args.txt",
            window_name=get_window_name("gen-branch"),
            worktree_path=worktree,
        )
        assert (worktree / "agent_args.txt").read_text().endswith("fix login redirect")
        assert (worktree / "agent_saw.txt").read_text() == "moved content\n"
        assert _git(env, mux_repo_path, "status", "--porcelain") == ""


class TestRescueOutput:
    """Dry-run plan and the success message."""

    def test_dry_run_describes_transfer_and_changes_nothing(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        _prepare_repo(env, mux_repo_path)
        before = _repo_state(env, mux_repo_path)

        result = run_workmux_command(
            env, workmux_exe_path, mux_repo_path, "add foo -w -u --dry-run"
        )

        assert (
            f"Changes:  move tracked changes and untracked files from {mux_repo_path}"
            in result.stdout
        )
        assert _repo_state(env, mux_repo_path) == before
        assert not get_worktree_path(mux_repo_path, "foo").exists()

    def test_success_message_reports_untracked_left_behind(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        _prepare_repo(env, mux_repo_path)

        result = run_workmux_command(env, workmux_exe_path, mux_repo_path, "add foo -w")

        assert "Changes that were not moved remain" in result.stdout
        assert "now clean" not in result.stdout
        assert (mux_repo_path / "moved.txt").exists()

    def test_success_message_reports_clean_source(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        env = mux_server
        _prepare_repo(env, mux_repo_path)

        result = run_workmux_command(
            env, workmux_exe_path, mux_repo_path, "add foo -w -u"
        )

        assert "Original worktree is now clean" in result.stdout
