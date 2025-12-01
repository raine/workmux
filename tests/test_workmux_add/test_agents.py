"""Tests for agent configuration, prompts, and multi-agent scenarios."""

import shlex
from pathlib import Path


from ..conftest import (
    TmuxEnvironment,
    FakeAgentInstaller,
    assert_prompt_file_contents,
    assert_window_exists,
    configure_default_shell,
    get_window_name,
    get_worktree_path,
    poll_until,
    run_workmux_command,
    wait_for_file,
    write_workmux_config,
)
from .conftest import add_branch_and_get_worktree


class TestInlinePrompts:
    """Tests for inline prompt injection into agents."""

    def test_add_inline_prompt_injects_into_claude(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Inline prompts should be written to PROMPT.md and passed to claude via command substitution."""
        env = isolated_tmux_server
        branch_name = "feature-inline-prompt"
        prompt_text = "Implement inline prompt"
        output_filename = "claude_prompt.txt"
        window_name = get_window_name(branch_name)

        fake_claude_path = fake_agent_installer.install(
            "claude",
            f"""#!/bin/sh
# Debug: log all arguments
echo "ARGS: $@" > debug_args.txt
echo "ARG1: $1" >> debug_args.txt
echo "ARG2: $2" >> debug_args.txt

set -e
# The implementation calls: claude -- "$(cat PROMPT.md)"
# So we expect -- as $1 and the prompt content as the second argument
printf '%s' "$2" > "{output_filename}"
""",
        )

        # Use absolute path to ensure we use the fake claude
        write_workmux_config(repo_path, panes=[{"command": str(fake_claude_path)}])

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)}",
        )

        # Prompt file is now written to the test's temp directory
        assert_prompt_file_contents(env, branch_name, prompt_text)

        agent_output = worktree_path / output_filename
        debug_output = worktree_path / "debug_args.txt"

        wait_for_file(
            env,
            agent_output,
            timeout=2.0,
            window_name=window_name,
            worktree_path=worktree_path,
            debug_log_path=debug_output,
        )

        assert agent_output.read_text() == prompt_text


class TestPromptFile:
    """Tests for file-based prompt injection."""

    def test_add_prompt_file_injects_into_gemini(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Prompt file flag should populate PROMPT.md and pass it to gemini via command substitution."""
        env = isolated_tmux_server
        branch_name = "feature-file-prompt"
        window_name = get_window_name(branch_name)
        prompt_source = repo_path / "prompt_source.txt"
        prompt_source.write_text("File-based instructions")
        output_filename = "gemini_prompt.txt"

        fake_gemini_path = fake_agent_installer.install(
            "gemini",
            f"""#!/bin/sh
set -e
# The implementation calls: gemini -i "$(cat PROMPT.md)"
# So we expect -i flag first, then the prompt content as the second argument
if [ "$1" != "-i" ]; then
    echo "Expected -i flag first" >&2
    exit 1
fi
printf '%s' "$2" > "{output_filename}"
""",
        )

        # Use absolute path to ensure we use the fake gemini
        write_workmux_config(
            repo_path, agent="gemini", panes=[{"command": str(fake_gemini_path)}]
        )

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args=f"--prompt-file {shlex.quote(str(prompt_source))}",
        )

        # Prompt file is now written to the test's temp directory
        assert_prompt_file_contents(env, branch_name, prompt_source.read_text())

        agent_output = worktree_path / output_filename

        wait_for_file(
            env,
            agent_output,
            timeout=2.0,
            window_name=window_name,
            worktree_path=worktree_path,
        )
        assert agent_output.read_text() == prompt_source.read_text()


class TestAgentConfig:
    """Tests for agent configuration from config file."""

    def test_add_uses_agent_from_config(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """The <agent> placeholder should use the agent configured in .workmux.yaml when --agent is not passed."""
        env = isolated_tmux_server
        branch_name = "feature-config-agent"
        window_name = get_window_name(branch_name)
        prompt_text = "Using configured agent"
        output_filename = "agent_output.txt"

        # Install fake gemini agent
        fake_gemini_path = fake_agent_installer.install(
            "gemini",
            f"""#!/bin/sh
set -e
# Gemini gets a -i flag, then the prompt as $2
printf '%s' "$2" > "{output_filename}"
""",
        )

        # Configure .workmux.yaml to use the absolute path to the fake agent
        write_workmux_config(
            repo_path, agent=str(fake_gemini_path), panes=[{"command": "<agent>"}]
        )

        # Run 'add' WITHOUT --agent flag, should use gemini from config
        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)}",
        )

        agent_output = worktree_path / output_filename

        wait_for_file(
            env,
            agent_output,
            timeout=2.0,
            window_name=window_name,
            worktree_path=worktree_path,
        )
        assert agent_output.read_text() == prompt_text

    def test_add_with_agent_flag_overrides_default(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """The --agent flag should override the default agent and inject prompts correctly."""
        env = isolated_tmux_server
        branch_name = "feature-agent-override"
        window_name = get_window_name(branch_name)
        prompt_text = "This is for the override agent"
        output_filename = "agent_output.txt"

        # Create two fake agents: a default one and the one we'll specify via the flag.
        # Default agent (claude)
        fake_agent_installer.install(
            "claude",
            "#!/bin/sh\necho 'default agent ran' > default_agent.txt",
        )

        # Override agent (gemini)
        fake_gemini_path = fake_agent_installer.install(
            "gemini",
            f"""#!/bin/sh
# Gemini gets a -i flag, then the prompt as $2
printf '%s' "$2" > "{output_filename}"
""",
        )

        # Configure workmux to use <agent> placeholder. The default should be 'claude'.
        write_workmux_config(repo_path, panes=[{"command": "<agent>"}])

        # Run 'add' with the --agent flag to override the default, using absolute path
        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args=f"--agent {shlex.quote(str(fake_gemini_path))} --prompt {shlex.quote(prompt_text)}",
        )

        agent_output = worktree_path / output_filename
        default_agent_output = worktree_path / "default_agent.txt"

        wait_for_file(
            env,
            agent_output,
            timeout=2.0,
            window_name=window_name,
            worktree_path=worktree_path,
        )
        assert not default_agent_output.exists(), "Default agent should not have run"
        assert agent_output.read_text() == prompt_text


class TestMultiAgent:
    """Tests for multi-agent scenarios."""

    def test_add_multi_agent_creates_separate_worktrees_and_runs_correct_agents(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Verifies `-a` with multiple agents creates distinct worktrees for each agent."""
        env = isolated_tmux_server
        base_name = "feature-multi-agent"
        prompt_text = "Implement for {{ agent }}"

        claude_path = fake_agent_installer.install(
            "claude",
            "#!/bin/sh\nprintf '%s' \"$2\" > claude_out.txt",
        )
        gemini_path = fake_agent_installer.install(
            "gemini",
            "#!/bin/sh\nprintf '%s' \"$2\" > gemini_out.txt",
        )

        write_workmux_config(repo_path, panes=[{"command": "<agent>"}])

        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add {base_name} -a {shlex.quote(str(claude_path))} -a {shlex.quote(str(gemini_path))} --prompt '{prompt_text}'",
        )

        claude_branch = f"{base_name}-claude"
        claude_worktree = get_worktree_path(repo_path, claude_branch)
        assert claude_worktree.is_dir()
        claude_window = get_window_name(claude_branch)
        assert_window_exists(env, claude_window)
        wait_for_file(
            env,
            claude_worktree / "claude_out.txt",
            window_name=claude_window,
            worktree_path=claude_worktree,
        )
        assert (
            claude_worktree / "claude_out.txt"
        ).read_text() == "Implement for claude"

        gemini_branch = f"{base_name}-gemini"
        gemini_worktree = get_worktree_path(repo_path, gemini_branch)
        assert gemini_worktree.is_dir()
        gemini_window = get_window_name(gemini_branch)
        assert_window_exists(env, gemini_window)
        wait_for_file(
            env,
            gemini_worktree / "gemini_out.txt",
            window_name=gemini_window,
            worktree_path=gemini_worktree,
        )
        assert (
            gemini_worktree / "gemini_out.txt"
        ).read_text() == "Implement for gemini"

    def test_add_with_count_and_agent_uses_agent_in_all_instances(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Verifies count with a single agent uses that agent in all generated worktrees."""
        env = isolated_tmux_server
        base_name = "feature-counted-agent"
        prompt_text = "Task {{ num }}"

        fake_gemini_path = fake_agent_installer.install(
            "gemini",
            '#!/bin/sh\nprintf \'%s\' "$2" > "gemini_task_${HOSTNAME}.txt"',
        )
        write_workmux_config(repo_path, panes=[{"command": "<agent>"}])

        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add {base_name} -a {shlex.quote(str(fake_gemini_path))} -n 2 --prompt '{prompt_text}'",
        )

        for idx in (1, 2):
            branch = f"{base_name}-gemini-{idx}"
            worktree = get_worktree_path(repo_path, branch)
            assert worktree.is_dir()
            files: list[Path] = []

            def _has_output() -> bool:
                files.clear()
                files.extend(worktree.glob("gemini_task_*.txt"))
                return len(files) == 1

            assert poll_until(_has_output, timeout=2.0), (
                f"gemini output file not found in worktree {worktree}"
            )
            assert files[0].read_text() == f"Task {idx}"


class TestForeach:
    """Tests for --foreach matrix expansion."""

    def test_add_foreach_creates_worktrees_from_matrix(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Verifies foreach matrix expands into multiple worktrees with templated prompts."""
        env = isolated_tmux_server
        base_name = "feature-matrix"
        prompt_text = "Build for {{ platform }} using {{ lang }}"

        claude_path = fake_agent_installer.install(
            "claude",
            "#!/bin/sh\nprintf '%s' \"$2\" > out.txt",
        )
        write_workmux_config(
            repo_path, agent=str(claude_path), panes=[{"command": "<agent>"}]
        )

        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            (
                f"add {base_name} --foreach "
                "'platform:ios,android;lang:swift,kotlin' "
                f"--prompt '{prompt_text}'"
            ),
        )

        combos = [
            ("ios", "swift"),
            ("android", "kotlin"),
        ]
        for platform, lang in combos:
            branch = f"{base_name}-{lang}-{platform}"
            worktree = get_worktree_path(repo_path, branch)
            assert worktree.is_dir()
            window = get_window_name(branch)
            assert_window_exists(env, window)
            wait_for_file(
                env,
                worktree / "out.txt",
                window_name=window,
                worktree_path=worktree,
            )
            assert (
                worktree / "out.txt"
            ).read_text() == f"Build for {platform} using {lang}"


class TestBranchTemplate:
    """Tests for --branch-template flag."""

    def test_add_with_custom_branch_template(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies `--branch-template` controls the branch naming scheme."""
        env = isolated_tmux_server
        base_name = "TICKET-123"
        template = r"{{ agent }}/{{ base_name | lower }}-{{ num }}"

        write_workmux_config(repo_path)
        run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add {base_name} -a Gemini -n 2 --branch-template '{template}'",
        )

        for idx in (1, 2):
            branch = f"gemini/ticket-123-{idx}"
            worktree = get_worktree_path(repo_path, branch)
            assert worktree.is_dir(), f"Worktree {branch} not found"


class TestNoPrompt:
    """Tests for behavior without prompts."""

    def test_add_without_prompt_skips_prompt_file(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Worktrees created without prompt flags should not create PROMPT.md."""
        env = isolated_tmux_server
        branch_name = "feature-no-prompt"

        from ..conftest import prompt_file_for_branch

        write_workmux_config(repo_path, panes=[])

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name
        )
        # Verify no PROMPT.md in worktree
        assert not (worktree_path / "PROMPT.md").exists()
        # Verify no prompt file in temp dir either
        assert not prompt_file_for_branch(env.tmp_path, branch_name).exists()


class TestShellAliases:
    """Tests for shell alias support with agents."""

    def test_agent_placeholder_respects_shell_aliases(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Verifies that the <agent> placeholder triggers aliases defined in shell rc files."""
        env = isolated_tmux_server
        branch_name = "feature-agent-alias"
        window_name = get_window_name(branch_name)
        marker_content = "alias_was_expanded"

        # Get the path where the fake agent will be installed
        fake_bin_dir = fake_agent_installer.bin_dir

        # Write a .zshrc that prepends our fake bin to the PATH and defines the alias.
        # This ensures the shell finds our fake `claude` before any system-wide one.
        (env.home_path / ".zshrc").write_text(
            f"""
export PATH="{fake_bin_dir}:$PATH"
alias claude='claude --aliased'
""".strip()
            + "\n"
        )

        fake_agent_installer.install(
            "claude",
            f"""#!/bin/sh
set -e
for arg in "$@"; do
  if [ "$arg" = "--aliased" ]; then
    echo "{marker_content}" > alias_marker.txt
    exit 0
  fi
done
echo "Alias flag not found" > alias_marker.txt
exit 1
""",
        )

        write_workmux_config(repo_path, agent="claude", panes=[{"command": "<agent>"}])

        pre_cmds = configure_default_shell()

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, repo_path, branch_name, pre_run_tmux_cmds=pre_cmds
        )
        marker_file = worktree_path / "alias_marker.txt"

        wait_for_file(
            env,
            marker_file,
            timeout=2.0,
            window_name=window_name,
            worktree_path=worktree_path,
        )
        assert marker_file.read_text().strip() == marker_content, (
            "Alias marker content incorrect; alias flag not detected."
        )


class TestAgentErrors:
    """Tests for error handling with agent flags."""

    def test_add_fails_with_count_and_multiple_agents(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies --count cannot be combined with multiple --agent flags."""
        env = isolated_tmux_server
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            "add my-feature -n 2 -a claude -a gemini",
            expect_fail=True,
        )
        assert "--count can only be used with zero or one --agent" in result.stderr

    def test_add_fails_with_foreach_and_agent(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies clap rejects --foreach in combination with --agent."""
        env = isolated_tmux_server
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            "add my-feature --foreach 'p:a' -a claude",
            expect_fail=True,
        )
        assert (
            "'--foreach <FOREACH>' cannot be used with '--agent <AGENT>'"
            in result.stderr
        )

    def test_add_fails_with_foreach_mismatched_lengths(
        self,
        isolated_tmux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Verifies foreach parser enforces equal list lengths."""
        env = isolated_tmux_server
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            "add my-feature --foreach 'platform:ios,android;lang:swift'",
            expect_fail=True,
        )
        assert (
            "All --foreach variables must have the same number of values"
            in result.stderr
        )
