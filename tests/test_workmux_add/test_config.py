"""Tests for config file precedence and global/project config merging."""

from pathlib import Path
import shlex

import yaml

from ..conftest import (
    FakeAgentInstaller,
    MuxEnvironment,
    RepoBuilder,
    assert_copied_file,
    assert_symlink_to,
    assert_window_exists,
    create_commit,
    file_for_commit,
    get_window_name,
    get_worktree_path,
    run_workmux_command,
    slugify,
    wait_for_file,
    wait_for_pane_output,
    write_global_workmux_config,
    write_workmux_config,
)
from .conftest import add_branch_and_get_worktree


class TestConfigPrecedence:
    """Tests for project config overriding global config."""

    def test_project_config_overrides_global_config(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Project-level settings should override conflicting global settings."""
        env = mux_server
        branch_name = "feature-project-overrides"
        global_prefix = "global-"
        project_prefix = "project-"

        write_global_workmux_config(env, window_prefix=global_prefix)
        write_workmux_config(mux_repo_path, window_prefix=project_prefix)

        add_branch_and_get_worktree(env, workmux_exe_path, mux_repo_path, branch_name)

        project_window = f"{project_prefix}{branch_name}"
        assert_window_exists(env, project_window)

        existing_windows = env.list_windows()
        assert f"{global_prefix}{branch_name}" not in existing_windows

    def test_global_config_used_when_project_config_absent(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Global config should be respected even if the repository lacks .workmux.yaml."""
        env = mux_server
        branch_name = "feature-global-only"
        hook_file = "global_only_hook.txt"

        write_global_workmux_config(env, post_create=[f"touch {hook_file}"])

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, branch_name
        )
        assert (worktree_path / hook_file).exists()


class TestGlobalPlaceholderPostCreate:
    """Tests for <global> placeholder in post_create hooks."""

    def test_global_placeholder_merges_post_create_commands(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """The '<global>' placeholder should expand to global post_create commands."""
        env = mux_server
        branch_name = "feature-global-hooks"
        global_hook = "created_from_global.txt"
        before_hook = "project_before.txt"
        after_hook = "project_after.txt"

        write_global_workmux_config(env, post_create=[f"touch {global_hook}"])
        write_workmux_config(
            mux_repo_path,
            post_create=[f"touch {before_hook}", "<global>", f"touch {after_hook}"],
        )

        worktree_dir = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, branch_name
        )
        assert (worktree_dir / before_hook).exists()
        assert (worktree_dir / global_hook).exists()
        assert (worktree_dir / after_hook).exists()

    def test_global_placeholder_is_dropped_without_global_hook(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Project hooks should run when no global post_create hook exists."""
        env = mux_server
        branch_name = "feature-missing-global-hook"
        project_hook = "created_from_project.txt"

        write_workmux_config(
            mux_repo_path,
            post_create=["<global>", f"touch {project_hook}"],
        )

        worktree_dir = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, branch_name
        )
        assert (worktree_dir / project_hook).exists()


class TestGlobalPlaceholderFiles:
    """Tests for <global> placeholder in file operations."""

    def test_global_placeholder_merges_file_operations(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """The '<global>' placeholder should merge copy and symlink file operations."""
        env = mux_server
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
            mux_repo_path,
            files={
                "copy": ["<global>", "project.env"],
                "symlink": ["<global>", "project_cache"],
            },
        )

        worktree_dir = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, branch_name
        )
        symlinked_global = assert_symlink_to(worktree_dir, "global_cache")
        symlinked_project = assert_symlink_to(worktree_dir, "project_cache")
        assert (symlinked_global / "shared.txt").read_text() == "global data"
        assert (symlinked_project / "local.txt").read_text() == "project data"

        assert_copied_file(worktree_dir, "global.env", "GLOBAL")
        assert_copied_file(worktree_dir, "project.env", "PROJECT")

    def test_global_placeholder_only_merges_specific_file_lists(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """`<global>` can merge copy patterns while symlink patterns fully override."""
        env = mux_server
        branch_name = "feature-partial-file-merge"

        repo_builder.add_to_gitignore(
            [
                "global_copy.txt",
                "project_copy.txt",
                "global_symlink_dir/",
                "project_symlink_dir/",
            ]
        )

        (mux_repo_path / "global_copy.txt").write_text("global copy")
        (mux_repo_path / "project_copy.txt").write_text("project copy")
        global_symlink_dir = mux_repo_path / "global_symlink_dir"
        global_symlink_dir.mkdir()
        (global_symlink_dir / "global.txt").write_text("global data")
        project_symlink_dir = mux_repo_path / "project_symlink_dir"
        project_symlink_dir.mkdir()
        (project_symlink_dir / "project.txt").write_text("project data")

        write_global_workmux_config(
            env,
            files={"copy": ["global_copy.txt"], "symlink": ["global_symlink_dir"]},
        )
        write_workmux_config(
            mux_repo_path,
            files={
                "copy": ["<global>", "project_copy.txt"],
                "symlink": ["project_symlink_dir"],
            },
        )

        worktree_dir = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, branch_name
        )
        assert_copied_file(worktree_dir, "global_copy.txt")
        assert_copied_file(worktree_dir, "project_copy.txt")

        assert_symlink_to(worktree_dir, "project_symlink_dir")
        assert not (worktree_dir / "global_symlink_dir").exists()


class TestEmptyOverrides:
    """Tests for empty lists overriding global config."""

    def test_project_empty_file_lists_override_global_lists(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        repo_builder: RepoBuilder,
    ):
        """Explicit empty lists suppress the corresponding global file operations."""
        env = mux_server
        branch_name = "feature-empty-file-override"

        repo_builder.add_to_gitignore(
            [
                "global_only.env",
                "global_shared_dir/",
            ]
        )

        (mux_repo_path / "global_only.env").write_text("SECRET=1")
        global_shared_dir = mux_repo_path / "global_shared_dir"
        global_shared_dir.mkdir()
        (global_shared_dir / "package.json").write_text('{"name":"demo"}')

        write_global_workmux_config(
            env,
            files={"copy": ["global_only.env"], "symlink": ["global_shared_dir"]},
        )
        write_workmux_config(
            mux_repo_path,
            files={"copy": [], "symlink": []},
        )

        worktree_dir = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, branch_name
        )
        assert not (worktree_dir / "global_only.env").exists()
        assert not (worktree_dir / "global_shared_dir").exists()


class TestPaneOverrides:
    """Tests for pane config overrides."""

    def test_project_panes_replace_global_panes(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Project panes should completely replace global panes (no merging)."""
        env = mux_server
        branch_name = "feature-pane-override"
        window_name = get_window_name(branch_name)
        global_output = "GLOBAL_PANE_OUTPUT"
        project_output = "PROJECT_PANE_OUTPUT"

        write_global_workmux_config(
            env, panes=[{"command": f"echo '{global_output}'; sleep 0.5"}]
        )
        write_workmux_config(
            mux_repo_path, panes=[{"command": f"echo '{project_output}'; sleep 0.5"}]
        )

        add_branch_and_get_worktree(env, workmux_exe_path, mux_repo_path, branch_name)

        wait_for_pane_output(env, window_name, project_output)

        pane_content = env.capture_pane(window_name)
        assert pane_content is not None
        assert global_output not in pane_content


class TestGlobalAgentDefault:
    """Tests for global agent config triggering agent-aware default panes."""

    def test_global_agent_starts_in_default_pane(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """When agent is set in global config and no panes are defined, the agent should run."""
        env = mux_server
        branch_name = "feature-global-agent-default"
        window_name = get_window_name(branch_name)

        # Ensure CLAUDE.md does not exist so we isolate the global agent config behavior
        assert not (mux_repo_path / "CLAUDE.md").exists()

        # Use absolute path for output to avoid cwd/shell-init races
        agent_output = env.tmp_path / "global_agent_ran.txt"

        # Install fake agent; use absolute path for both agent command and output
        # to avoid PATH resolution issues when the login shell re-initializes PATH
        agent_path = fake_agent_installer.install(
            "global_agent",
            f"#!/bin/sh\necho ran > {agent_output}\n",
        )

        # Write global config with absolute agent path but NO explicit panes
        write_global_workmux_config(env, agent=str(agent_path))

        # Do NOT write project-level .workmux.yaml

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, branch_name
        )

        wait_for_file(
            env,
            agent_output,
            timeout=10.0,
            window_name=window_name,
            worktree_path=worktree_path,
        )
        assert agent_output.read_text().strip() == "ran"


class TestBaseBranchConfig:
    """Tests for base_branch config option."""

    def _add_from_dashboard(
        self,
        env: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        new_branch: str,
        expected_file: Path,
    ) -> Path:
        command = (
            f"cd {shlex.quote(str(mux_repo_path))} && "
            f"{shlex.quote(str(workmux_exe_path))} dashboard --tab worktrees"
        )
        env.send_keys("test:", command)
        wait_for_pane_output(env, "test", "Worktrees", timeout=10.0)
        env.send_keys("test:", "a", enter=False)
        wait_for_pane_output(env, "test", "Add Worktree", timeout=10.0)
        env.send_keys("test:", new_branch)
        worktree_path = get_worktree_path(mux_repo_path, new_branch)
        wait_for_file(
            env,
            expected_file,
            timeout=10.0,
            window_name="test",
            worktree_path=worktree_path,
        )
        return worktree_path

    def test_config_base_branch_used_when_base_flag_omitted(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Config base_branch should be used as default when --base is not passed."""
        env = mux_server
        base_branch = "config-base-source"
        new_branch = "feature-from-config-base"
        commit_msg = "Commit on config base"

        # Create a branch with a unique commit
        env.run_command(["git", "checkout", "-b", base_branch], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, commit_msg)

        # Go back to main so the current branch is NOT the base
        env.run_command(["git", "checkout", "main"], cwd=mux_repo_path)

        # Write config with base_branch pointing to our branch
        write_workmux_config(mux_repo_path, base_branch=base_branch)

        # Add without --base; config should kick in
        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, new_branch
        )

        # The new worktree should have the commit from the configured base branch
        expected_file = file_for_commit(worktree_path, commit_msg)
        assert expected_file.exists()

    def test_auto_base_branch_uses_configured_main_branch(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """The auto value should resolve to the effective main branch."""
        env = mux_server
        main_branch = "project-trunk"
        current_branch = "unrelated-feature"
        new_branch = "feature-from-auto-base"
        main_commit = "Commit on configured main branch"
        current_commit = "Commit on unrelated current branch"

        env.run_command(["git", "checkout", "-b", main_branch], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, main_commit)
        env.run_command(["git", "checkout", "main"], cwd=mux_repo_path)
        env.run_command(["git", "checkout", "-b", current_branch], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, current_commit)
        write_workmux_config(mux_repo_path, main_branch=main_branch, base_branch="auto")

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, new_branch
        )

        assert file_for_commit(worktree_path, main_commit).exists()
        assert not file_for_commit(worktree_path, current_commit).exists()
        result = env.run_command(
            ["git", "config", "--local", f"branch.{new_branch}.workmux-base"],
            cwd=mux_repo_path,
        )
        assert result.stdout.strip() == main_branch

    def test_global_auto_base_branch_uses_detected_main_branch(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """A global auto value should adapt to the current repository."""
        env = mux_server
        current_branch = "global-auto-current-feature"
        new_branch = "feature-from-global-auto"
        current_commit = "Commit on global auto current branch"

        env.run_command(["git", "checkout", "-b", current_branch], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, current_commit)
        write_global_workmux_config(env, base_branch="auto")

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, new_branch
        )

        assert not file_for_commit(worktree_path, current_commit).exists()
        result = env.run_command(
            ["git", "config", "--local", f"branch.{new_branch}.workmux-base"],
            cwd=mux_repo_path,
        )
        assert result.stdout.strip() == "main"

    def test_cli_base_overrides_auto_base_branch(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """An explicit --base should take precedence over configured auto."""
        env = mux_server
        cli_base = "explicit-auto-override"
        new_branch = "feature-from-explicit-auto-override"
        commit_msg = "Commit on explicit auto override"

        env.run_command(["git", "checkout", "-b", cli_base], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, commit_msg)
        env.run_command(["git", "checkout", "main"], cwd=mux_repo_path)
        write_workmux_config(mux_repo_path, base_branch="auto")

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            new_branch,
            extra_args=f"--base {cli_base}",
        )

        assert file_for_commit(worktree_path, commit_msg).exists()
        result = env.run_command(
            ["git", "config", "--local", f"branch.{new_branch}.workmux-base"],
            cwd=mux_repo_path,
        )
        assert result.stdout.strip() == cli_base

    def test_dry_run_resolves_auto_base_branch(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Dry-run output should show the resolved branch name."""
        env = mux_server
        current_branch = "dry-run-current-feature"
        env.run_command(["git", "checkout", "-b", current_branch], cwd=mux_repo_path)
        write_workmux_config(mux_repo_path, base_branch="auto")

        result = env.run_command(
            [str(workmux_exe_path), "add", "--dry-run", "dry-run-auto-base"],
            cwd=mux_repo_path,
        )

        assert "Base:     main" in result.stdout
        assert "Base:     auto" not in result.stdout

    def test_dashboard_add_uses_auto_base_branch(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Dashboard creation should resolve auto to the effective main branch."""
        env = mux_server
        current_branch = "dashboard-unrelated-feature"
        new_branch = "dashboard-auto-created"
        main_commit = "Commit on dashboard main"
        current_commit = "Commit on dashboard unrelated feature"

        create_commit(env, mux_repo_path, main_commit)
        env.run_command(["git", "checkout", "-b", current_branch], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, current_commit)
        write_workmux_config(mux_repo_path, base_branch="auto")

        expected_file = file_for_commit(
            get_worktree_path(mux_repo_path, new_branch), main_commit
        )
        worktree_path = self._add_from_dashboard(
            env, workmux_exe_path, mux_repo_path, new_branch, expected_file
        )

        assert expected_file.exists()
        assert not file_for_commit(worktree_path, current_commit).exists()
        result = env.run_command(
            ["git", "config", "--local", f"branch.{new_branch}.workmux-base"],
            cwd=mux_repo_path,
        )
        assert result.stdout.strip() == "main"

    def test_dashboard_add_uses_project_base_branch_over_global(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Dashboard add should use project base_branch before global base_branch."""
        env = mux_server
        global_base = "dashboard-global-base"
        project_base = "dashboard-project-base"
        new_branch = "dashboard-project-created"
        global_commit = "Commit on dashboard global base"
        project_commit = "Commit on dashboard project base"

        env.run_command(["git", "checkout", "-b", global_base], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, global_commit)
        env.run_command(["git", "checkout", "main"], cwd=mux_repo_path)
        env.run_command(["git", "checkout", "-b", project_base], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, project_commit)
        env.run_command(["git", "checkout", "main"], cwd=mux_repo_path)

        write_global_workmux_config(env, base_branch=global_base)
        write_workmux_config(mux_repo_path, base_branch=project_base)

        expected_file = file_for_commit(
            get_worktree_path(mux_repo_path, new_branch), project_commit
        )
        worktree_path = self._add_from_dashboard(
            env, workmux_exe_path, mux_repo_path, new_branch, expected_file
        )

        assert expected_file.exists()
        assert not file_for_commit(worktree_path, global_commit).exists()
        result = env.run_command(
            ["git", "config", "--local", f"branch.{new_branch}.workmux-base"],
            cwd=mux_repo_path,
        )
        assert result.stdout.strip() == project_base

    def test_dashboard_add_uses_global_base_branch(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Dashboard add should use global base_branch when project config is absent."""
        env = mux_server
        base_branch = "dashboard-global-only-base"
        new_branch = "dashboard-global-created"
        commit_msg = "Commit on dashboard global only base"

        env.run_command(["git", "checkout", "-b", base_branch], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, commit_msg)
        env.run_command(["git", "checkout", "main"], cwd=mux_repo_path)
        write_global_workmux_config(env, base_branch=base_branch)

        expected_file = file_for_commit(
            get_worktree_path(mux_repo_path, new_branch), commit_msg
        )
        self._add_from_dashboard(
            env, workmux_exe_path, mux_repo_path, new_branch, expected_file
        )

        assert expected_file.exists()
        result = env.run_command(
            ["git", "config", "--local", f"branch.{new_branch}.workmux-base"],
            cwd=mux_repo_path,
        )
        assert result.stdout.strip() == base_branch

    def test_dashboard_add_without_config_uses_current_branch(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Dashboard add should fall back to current branch when base_branch is unset."""
        env = mux_server
        base_branch = "dashboard-current-base"
        new_branch = "dashboard-current-created"
        commit_msg = "Commit on dashboard current base"

        env.run_command(["git", "checkout", "-b", base_branch], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, commit_msg)

        expected_file = file_for_commit(
            get_worktree_path(mux_repo_path, new_branch), commit_msg
        )
        self._add_from_dashboard(
            env, workmux_exe_path, mux_repo_path, new_branch, expected_file
        )

        assert expected_file.exists()
        result = env.run_command(
            ["git", "config", "--local", f"branch.{new_branch}.workmux-base"],
            cwd=mux_repo_path,
        )
        assert result.stdout.strip() == base_branch

    def test_empty_config_base_branch_falls_back_to_current_branch(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Empty base_branch should not block the current-branch fallback."""
        env = mux_server
        base_branch = "empty-config-current-base"
        new_branch = "empty-config-created"
        commit_msg = "Commit on empty config current base"

        env.run_command(["git", "checkout", "-b", base_branch], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, commit_msg)
        (mux_repo_path / ".workmux.yaml").write_text(
            'base_branch: ""\nnerdfont: false\n'
        )

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, new_branch
        )

        assert file_for_commit(worktree_path, commit_msg).exists()
        result = env.run_command(
            ["git", "config", "--local", f"branch.{new_branch}.workmux-base"],
            cwd=mux_repo_path,
        )
        assert result.stdout.strip() == base_branch

    def test_cli_base_overrides_config_base_branch(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """CLI --base flag should override config base_branch."""
        env = mux_server
        config_base = "config-base-override"
        cli_base = "cli-base-override"
        new_branch = "feature-cli-overrides-config"
        config_commit = "Commit on config base branch"
        cli_commit = "Commit on cli base branch"

        # Create the config base branch with a commit
        env.run_command(["git", "checkout", "-b", config_base], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, config_commit)

        # Create the CLI base branch with a different commit
        env.run_command(["git", "checkout", "main"], cwd=mux_repo_path)
        env.run_command(["git", "checkout", "-b", cli_base], cwd=mux_repo_path)
        create_commit(env, mux_repo_path, cli_commit)

        # Go back to main
        env.run_command(["git", "checkout", "main"], cwd=mux_repo_path)

        # Config points to config_base, but we pass --base cli_base
        write_workmux_config(mux_repo_path, base_branch=config_base)

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            new_branch,
            extra_args=f"--base {cli_base}",
        )

        # Should have the CLI base commit, not the config base commit
        assert file_for_commit(worktree_path, cli_commit).exists()
        assert not file_for_commit(worktree_path, config_commit).exists()


class TestConfigOverride:
    """Tests for --config CLI override."""

    def test_config_override_applies_settings(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """--config should load the specified config file instead of discovering."""
        env = mux_server
        branch_name = "feature-config-override"
        override_prefix = "override-"

        # Write the default project config with a different prefix
        write_workmux_config(mux_repo_path, window_prefix="default-")

        # Write an alternate config file
        alt_config = mux_repo_path / ".workmux.alt.yaml"
        alt_config.write_text(f"window_prefix: {override_prefix}\nnerdfont: false\n")

        add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--config {alt_config}",
        )

        override_window = f"{override_prefix}{branch_name}"
        assert_window_exists(env, override_window)

        # The default prefix should NOT have been used
        existing_windows = env.list_windows()
        assert f"default-{branch_name}" not in existing_windows

    def test_config_override_missing_file_fails(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """--config with a nonexistent file should produce a clear error."""
        env = mux_server
        branch_name = "feature-missing-config"
        missing_config = mux_repo_path / "nonexistent.yaml"

        write_workmux_config(mux_repo_path)

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {branch_name} --config {missing_config}",
            expect_fail=True,
        )

        assert "Config file not found" in result.stderr

    def test_config_override_directory_fails(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """--config with a directory path should produce a clear error."""
        env = mux_server
        branch_name = "feature-dir-config"
        dir_path = mux_repo_path / "config_dir"
        dir_path.mkdir()

        write_workmux_config(mux_repo_path)

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {branch_name} --config {dir_path}",
            expect_fail=True,
        )

        assert "must be a file, not a directory" in result.stderr

    def test_config_override_still_merges_with_global(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """--config should still merge with global config (global values apply when not overridden)."""
        env = mux_server
        branch_name = "feature-config-merge"
        global_hook = "global_from_merge.txt"
        override_prefix = "merged-"

        write_global_workmux_config(env, post_create=[f"touch {global_hook}"])

        # Alternate config only overrides window_prefix, not post_create
        alt_config = mux_repo_path / ".workmux.merge.yaml"
        alt_config.write_text(f"window_prefix: {override_prefix}\nnerdfont: false\n")

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--config {alt_config}",
        )

        # Global post_create should still have run
        assert (worktree_path / global_hook).exists()

        # Override prefix should have been used
        assert_window_exists(env, f"{override_prefix}{branch_name}")


class TestWorktreeDirPlaceholders:
    """Tests for `{project}` and tilde expansion in `worktree_dir`."""

    def test_global_worktree_dir_with_project_placeholder_and_tilde(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """A single global `worktree_dir: ~/.workmux/{project}` should namespace each
        repo under HOME without per-project configuration (the use case from #148)."""
        env = mux_server
        branch_name = "feature-placeholder"

        # Write directly to the global config: the existing helper does not
        # accept `worktree_dir`, but we want to exercise the actual user flow
        # of setting this once globally.
        global_config_dir = env.home_path / ".config" / "workmux"
        global_config_dir.mkdir(parents=True, exist_ok=True)
        (global_config_dir / "config.yaml").write_text(
            yaml.dump({"worktree_dir": "~/.workmux/{project}", "nerdfont": False})
        )

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {branch_name}",
        )

        handle = slugify(branch_name)
        expected = env.home_path / ".workmux" / mux_repo_path.name / handle
        assert expected.is_dir(), f"expected worktree at {expected}"
