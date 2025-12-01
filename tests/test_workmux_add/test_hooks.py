"""Tests for post_create hooks and pane commands in `workmux add`."""

from pathlib import Path

from ..conftest import (
    TmuxEnvironment,
    configure_default_shell,
    get_window_name,
    wait_for_pane_output,
    write_workmux_config,
)
from .conftest import add_branch_and_get_worktree


class TestPostCreateHooks:
    """Tests for post_create hook execution."""

    def test_add_executes_post_create_hooks(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that `workmux add` executes post_create hooks in the worktree directory."""
        env = isolated_tmux_server
        branch_name = "feature-hooks"
        hook_file = "hook_was_executed.txt"

        write_workmux_config(repo_path, post_create=[f"touch {hook_file}"])

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name
        )

        # Verify hook file was created in the worktree directory
        assert (worktree_path / hook_file).exists()

    def test_add_can_skip_post_create_hooks(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """`workmux add --no-hooks` should not run configured post_create hooks."""
        env = isolated_tmux_server
        branch_name = "feature-skip-hooks"
        hook_file = "hook_should_not_exist.txt"

        write_workmux_config(repo_path, post_create=[f"touch {hook_file}"])

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--no-hooks",
        )

        assert not (worktree_path / hook_file).exists()


class TestPaneCommands:
    """Tests for pane command execution."""

    def test_add_executes_pane_commands(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that `workmux add` executes commands in configured panes."""
        env = isolated_tmux_server
        branch_name = "feature-panes"
        window_name = get_window_name(branch_name)
        expected_output = "test pane command output"

        write_workmux_config(
            repo_path, panes=[{"command": f"echo '{expected_output}'; sleep 0.5"}]
        )

        add_branch_and_get_worktree(env, workmux_exe_path, repo_path, branch_name)

        wait_for_pane_output(env, window_name, expected_output)

    def test_add_can_skip_pane_commands(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """`workmux add --no-pane-cmds` should create panes without running commands."""
        env = isolated_tmux_server
        branch_name = "feature-skip-pane-cmds"
        marker_file = "pane_command_output.txt"

        write_workmux_config(repo_path, panes=[{"command": f"touch {marker_file}"}])

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--no-pane-cmds",
        )

        assert not (worktree_path / marker_file).exists()


class TestShellRcFiles:
    """Tests for shell rc file sourcing."""

    def test_add_sources_shell_rc_files(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies that shell rc files (.zshrc) are sourced and aliases work in pane commands."""
        env = isolated_tmux_server
        branch_name = "feature-aliases"
        window_name = get_window_name(branch_name)
        alias_output = "custom_alias_worked_correctly"

        # The environment now provides an isolated HOME directory.
        # Write the .zshrc file there.
        zshrc_content = f"""
# Test alias
alias testcmd='echo "{alias_output}"'
"""
        (env.home_path / ".zshrc").write_text(zshrc_content)

        write_workmux_config(repo_path, panes=[{"command": "testcmd; sleep 0.5"}])

        pre_cmds = configure_default_shell()

        add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name, pre_run_tmux_cmds=pre_cmds
        )

        wait_for_pane_output(
            env,
            window_name,
            alias_output,
            timeout=2.0,
        )
