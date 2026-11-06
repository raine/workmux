"""Tests for agent configuration, prompts, and multi-agent scenarios."""

import shlex
from pathlib import Path


from ..conftest import (
    MuxEnvironment,
    FakeAgentInstaller,
    ShellCommands,
    assert_prompt_file_contents,
    assert_window_exists,
    get_window_name,
    get_worktree_path,
    poll_until,
    run_workmux_command,
    wait_for_file,
    write_global_workmux_config,
    write_workmux_config,
)
from .conftest import add_branch_and_get_worktree


class TestInlinePrompts:
    """Tests for inline prompt injection into agents."""

    def test_add_inline_prompt_injects_into_claude(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
        shell_cmd: ShellCommands,
    ):
        """Inline prompts should be written to PROMPT.md and passed to claude via command substitution."""
        env = mux_server
        branch_name = "feature-inline-prompt"
        prompt_text = "Implement inline prompt"
        output_filename = "claude_prompt.txt"
        window_name = get_window_name(branch_name)

        # Configure the shell
        env.configure_default_shell(shell_cmd.path)

        fake_agent_installer.install(
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

        # Use agent name - shell will find it via PATH from RC file
        write_workmux_config(
            mux_repo_path, agent="claude", panes=[{"command": "<agent>"}]
        )

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)}",
        )

        # Prompt file is now written to <worktree>/.workmux/
        assert_prompt_file_contents(env, branch_name, prompt_text, worktree_path)

        agent_output = worktree_path / output_filename
        debug_output = worktree_path / "debug_args.txt"

        wait_for_file(
            env,
            agent_output,
            timeout=5.0,  # Increased for slower shells
            window_name=window_name,
            worktree_path=worktree_path,
            debug_log_path=debug_output,
        )

        assert agent_output.read_text() == prompt_text


class TestPromptFile:
    """Tests for file-based prompt injection."""

    def test_add_prompt_file_injects_into_gemini(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
        shell_cmd: ShellCommands,
    ):
        """Prompt file flag should populate PROMPT.md and pass it to gemini via command substitution."""
        env = mux_server
        branch_name = "feature-file-prompt"
        window_name = get_window_name(branch_name)
        prompt_source = mux_repo_path / "prompt_source.txt"
        prompt_source.write_text("File-based instructions")
        output_filename = "gemini_prompt.txt"

        # Configure the shell
        env.configure_default_shell(shell_cmd.path)

        fake_agent_installer.install(
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

        # Use agent name - shell will find it via PATH from RC file
        write_workmux_config(
            mux_repo_path, agent="gemini", panes=[{"command": "<agent>"}]
        )

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt-file {shlex.quote(str(prompt_source))}",
        )

        # Prompt file is now written to <worktree>/.workmux/
        assert_prompt_file_contents(
            env, branch_name, prompt_source.read_text(), worktree_path
        )

        agent_output = worktree_path / output_filename

        wait_for_file(
            env,
            agent_output,
            timeout=5.0,  # Increased for slower shells
            window_name=window_name,
            worktree_path=worktree_path,
        )
        assert agent_output.read_text() == prompt_source.read_text()


class TestAgentConfig:
    """Tests for agent configuration from config file."""

    def test_add_uses_agent_from_config(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
        shell_cmd: ShellCommands,
    ):
        """The <agent> placeholder should use the agent configured in .workmux.yaml when --agent is not passed."""
        env = mux_server
        branch_name = "feature-config-agent"
        window_name = get_window_name(branch_name)
        prompt_text = "Using configured agent"
        output_filename = "agent_output.txt"

        # Configure the shell
        env.configure_default_shell(shell_cmd.path)

        # Install fake gemini agent
        fake_agent_installer.install(
            "gemini",
            f"""#!/bin/sh
set -e
# Gemini gets a -i flag, then the prompt as $2
printf '%s' "$2" > "{output_filename}"
""",
        )

        # Configure .workmux.yaml to use the agent name (found via PATH)
        write_workmux_config(
            mux_repo_path, agent="gemini", panes=[{"command": "<agent>"}]
        )

        # Run 'add' WITHOUT --agent flag, should use gemini from config
        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)}",
        )

        agent_output = worktree_path / output_filename

        wait_for_file(
            env,
            agent_output,
            timeout=5.0,  # Increased for slower shells
            window_name=window_name,
            worktree_path=worktree_path,
        )
        assert agent_output.read_text() == prompt_text

    def test_add_uses_omp_agent_from_config(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
        shell_cmd: ShellCommands,
    ):
        """OMP receives the prompt as a positional argument."""
        env = mux_server
        branch_name = "feature-config-omp-agent"
        window_name = get_window_name(branch_name)
        prompt_text = "Using configured OMP agent"

        env.configure_default_shell(shell_cmd.path)

        fake_agent_installer.install(
            "omp",
            """#!/bin/sh
set -e
if [ "$1" = "--" ] || [ "$1" = "-i" ]; then
    echo "unexpected prompt flag: $1" > omp_error.txt
    exit 1
fi
printf '%s' "$1" > omp_prompt.txt
""",
        )

        write_workmux_config(mux_repo_path, agent="omp", panes=[{"command": "<agent>"}])

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)}",
        )

        agent_output = worktree_path / "omp_prompt.txt"
        wait_for_file(
            env,
            agent_output,
            timeout=5.0,
            window_name=window_name,
            worktree_path=worktree_path,
        )
        assert agent_output.read_text() == prompt_text

    def test_add_passes_prompt_to_named_copilot_agent_with_interactive_flag(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Copilot receives configured flags followed by -i and the prompt."""
        env = mux_server
        branch_name = "feature-config-copilot-agent"
        window_name = get_window_name(branch_name)
        prompt_text = "Using configured Copilot agent"
        output_filename = "copilot_prompt.txt"

        fake_agent_installer.install(
            "copilot",
            f"""#!/bin/sh
set -e
if [ "$1" != "--yolo" ] || [ "$2" != "-i" ] || [ "$#" -ne 3 ]; then
    printf '%s\n' "$@" > copilot_args.txt
    exit 1
fi
printf '%s' "$3" > "{output_filename}"
""",
        )

        write_global_workmux_config(
            env,
            agent="copilot-yolo",
            agents={
                "copilot-yolo": {
                    "type": "copilot",
                    "command": "copilot",
                    "args": ["--yolo"],
                }
            },
        )
        write_workmux_config(mux_repo_path, panes=[{"command": "<agent>"}])

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)}",
        )

        agent_output = worktree_path / output_filename
        wait_for_file(
            env,
            agent_output,
            timeout=5.0,
            window_name=window_name,
            worktree_path=worktree_path,
        )
        assert agent_output.read_text() == prompt_text

    def test_add_with_agent_flag_overrides_default(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """The --agent flag should override the default agent and inject prompts correctly."""
        env = mux_server
        branch_name = "feature-agent-override"
        window_name = get_window_name(branch_name)
        prompt_text = "This is for the override agent"

        # Use absolute paths for output files to avoid cwd/shell-init races
        agent_output = env.tmp_path / "agent_output.txt"
        default_agent_output = env.tmp_path / "default_agent.txt"

        # Create two fake agents: a default one and the one we'll specify via the flag.
        # Default agent (claude)
        fake_agent_installer.install(
            "claude",
            f"#!/bin/sh\necho 'default agent ran' > {default_agent_output}",
        )

        # Override agent (gemini)
        fake_gemini_path = fake_agent_installer.install(
            "gemini",
            f"""#!/bin/sh
# Gemini gets a -i flag, then the prompt as $2
printf '%s' "$2" > "{agent_output}"
""",
        )

        # Configure workmux to use <agent> placeholder. The default should be 'claude'.
        write_workmux_config(mux_repo_path, panes=[{"command": "<agent>"}])

        # Run 'add' with the --agent flag to override the default, using absolute path
        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--agent {shlex.quote(str(fake_gemini_path))} --prompt {shlex.quote(prompt_text)}",
        )

        wait_for_file(
            env,
            agent_output,
            timeout=10.0,
            window_name=window_name,
            worktree_path=worktree_path,
        )
        assert not default_agent_output.exists(), "Default agent should not have run"
        assert agent_output.read_text() == prompt_text

    def test_add_prompt_injects_into_typed_agent_wrapper_pane(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Named wrapper agents with type: claude should allow prompt injection into Claude panes."""
        env = mux_server
        branch_name = "feature-typed-wrapper-pane"
        window_name = get_window_name(branch_name)
        prompt_text = "Use the typed wrapper pane"
        output_filename = "typed_wrapper_prompt.txt"

        fake_agent_installer.install(
            "claudeg",
            f"""#!/bin/sh
set -e
found_separator=0
for arg in "$@"; do
    if [ "$found_separator" = "1" ]; then
        printf '%s' "$arg" > "{output_filename}"
        exit 0
    fi
    if [ "$arg" = "--" ]; then
        found_separator=1
    fi
done
exit 1
""",
        )

        write_global_workmux_config(
            env,
            agent="claude-epic",
            agents={
                "claude-epic": {
                    "command": "claudeg --dangerously-skip-permissions",
                    "type": "claude",
                }
            },
        )
        write_workmux_config(
            mux_repo_path,
            panes=[
                {"command": "pnpm install", "focus": True},
                {
                    "command": "claudeg --dangerously-skip-permissions",
                    "split": "horizontal",
                },
            ],
        )

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)}",
        )

        agent_output = worktree_path / output_filename
        wait_for_file(
            env,
            agent_output,
            timeout=5.0,
            window_name=window_name,
            worktree_path=worktree_path,
        )
        assert agent_output.read_text() == prompt_text


class TestAgentWithArguments:
    """Tests for agent commands that include arguments."""

    def test_agent_with_dangerously_skip_permissions_flag(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Agent config with --dangerously-skip-permissions should pass flag and prompt correctly."""
        env = mux_server
        branch_name = "feature-yolo-mode"
        window_name = get_window_name(branch_name)
        prompt_text = "Implement yolo feature"
        output_filename = "agent_output.txt"
        flag_marker = "flag_found.txt"

        # Fake claude that verifies both the flag and prompt are received
        fake_claude_path = fake_agent_installer.install(
            "claude",
            f"""#!/bin/sh
set -e
# Log all args for debugging
echo "ARGS: $@" > debug_args.txt

# Check for --dangerously-skip-permissions flag
flag_found=0
for arg in "$@"; do
    if [ "$arg" = "--dangerously-skip-permissions" ]; then
        flag_found=1
        echo "yes" > "{flag_marker}"
        break
    fi
done

if [ "$flag_found" = "0" ]; then
    echo "no" > "{flag_marker}"
fi

# The command is: claude --dangerously-skip-permissions -- "prompt"
# So after the flag, we expect -- and then the prompt
# Find the prompt (argument after --)
prompt=""
found_separator=0
for arg in "$@"; do
    if [ "$found_separator" = "1" ]; then
        prompt="$arg"
        break
    fi
    if [ "$arg" = "--" ]; then
        found_separator=1
    fi
done

printf '%s' "$prompt" > "{output_filename}"
""",
        )

        # Configure agent with the flag included
        write_workmux_config(
            mux_repo_path,
            agent=f"{fake_claude_path} --dangerously-skip-permissions",
            panes=[{"command": "<agent>"}],
        )

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)}",
        )

        agent_output = worktree_path / output_filename
        flag_file = worktree_path / flag_marker
        debug_file = worktree_path / "debug_args.txt"

        wait_for_file(
            env,
            agent_output,
            window_name=window_name,
            worktree_path=worktree_path,
            debug_log_path=debug_file,
        )

        # Verify the flag was received
        assert flag_file.exists(), "Flag marker file not created"
        assert flag_file.read_text().strip() == "yes", (
            f"--dangerously-skip-permissions flag not found. Debug: {debug_file.read_text() if debug_file.exists() else 'no debug'}"
        )

        # Verify the prompt was received
        assert agent_output.read_text() == prompt_text

    def test_agent_with_multiple_arguments(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Agent config with multiple arguments should pass all args and prompt correctly."""
        env = mux_server
        branch_name = "feature-multi-args"
        window_name = get_window_name(branch_name)
        prompt_text = "Multi-arg task"
        output_filename = "agent_output.txt"

        # Fake claude that captures all args before --
        fake_claude_path = fake_agent_installer.install(
            "claude",
            f"""#!/bin/sh
set -e
echo "ARGS: $@" > debug_args.txt

# Collect args before -- separator
args_before=""
prompt=""
found_separator=0
for arg in "$@"; do
    if [ "$found_separator" = "1" ]; then
        prompt="$arg"
        break
    fi
    if [ "$arg" = "--" ]; then
        found_separator=1
    else
        if [ -n "$args_before" ]; then
            args_before="$args_before $arg"
        else
            args_before="$arg"
        fi
    fi
done

echo "$args_before" > args_received.txt
printf '%s' "$prompt" > "{output_filename}"
""",
        )

        # Configure agent with multiple flags
        write_workmux_config(
            mux_repo_path,
            agent=f"{fake_claude_path} --verbose --model opus",
            panes=[{"command": "<agent>"}],
        )

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)}",
        )

        agent_output = worktree_path / output_filename
        args_file = worktree_path / "args_received.txt"

        wait_for_file(
            env,
            agent_output,
            window_name=window_name,
            worktree_path=worktree_path,
        )

        # Verify all args were received
        args_received = args_file.read_text().strip()
        assert "--verbose" in args_received, f"--verbose not found in: {args_received}"
        assert "--model" in args_received, f"--model not found in: {args_received}"
        assert "opus" in args_received, f"opus not found in: {args_received}"

        # Verify the prompt was received
        assert agent_output.read_text() == prompt_text


class TestMultiAgent:
    """Tests for multi-agent scenarios."""

    def test_add_multi_agent_creates_separate_worktrees_and_runs_correct_agents(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Verifies `-a` with multiple agents creates distinct worktrees for each agent."""
        env = mux_server
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

        write_workmux_config(mux_repo_path, panes=[{"command": "<agent>"}])

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {base_name} -a {shlex.quote(str(claude_path))} -a {shlex.quote(str(gemini_path))} --prompt '{prompt_text}'",
        )

        claude_branch = f"{base_name}-claude"
        claude_worktree = get_worktree_path(mux_repo_path, claude_branch)
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
        gemini_worktree = get_worktree_path(mux_repo_path, gemini_branch)
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
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Verifies count with a single agent uses that agent in all generated worktrees."""
        env = mux_server
        base_name = "feature-counted-agent"
        prompt_text = "Task {{ num }}"

        fake_gemini_path = fake_agent_installer.install(
            "gemini",
            '#!/bin/sh\nprintf \'%s\' "$2" > "gemini_task_${HOSTNAME}.txt"',
        )
        write_workmux_config(mux_repo_path, panes=[{"command": "<agent>"}])

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {base_name} -a {shlex.quote(str(fake_gemini_path))} -n 2 --prompt '{prompt_text}'",
        )

        for idx in (1, 2):
            branch = f"{base_name}-gemini-{idx}"
            worktree = get_worktree_path(mux_repo_path, branch)
            assert worktree.is_dir()
            files: list[Path] = []

            def _has_output() -> bool:
                files.clear()
                files.extend(worktree.glob("gemini_task_*.txt"))
                return len(files) == 1

            assert poll_until(_has_output, timeout=5.0), (
                f"gemini output file not found in worktree {worktree}"
            )
            assert files[0].read_text() == f"Task {idx}"


class TestForeach:
    """Tests for --foreach matrix expansion."""

    def test_add_foreach_creates_worktrees_from_matrix(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Verifies foreach matrix expands into multiple worktrees with templated prompts."""
        env = mux_server
        base_name = "feature-matrix"
        prompt_text = "Build for {{ platform }} using {{ lang }}"

        claude_path = fake_agent_installer.install(
            "claude",
            "#!/bin/sh\nprintf '%s' \"$2\" > out.txt",
        )
        write_workmux_config(
            mux_repo_path, agent=str(claude_path), panes=[{"command": "<agent>"}]
        )

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
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
            worktree = get_worktree_path(mux_repo_path, branch)
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
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Verifies `--branch-template` controls the branch naming scheme."""
        env = mux_server
        base_name = "TICKET-123"
        template = r"{{ agent }}/{{ base_name | lower }}-{{ num }}"

        fake_agent_installer.install("Gemini", "#!/bin/sh\nexit 0\n")
        write_workmux_config(mux_repo_path)
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add {base_name} -a Gemini -n 2 --branch-template '{template}'",
        )

        for idx in (1, 2):
            branch = f"gemini/ticket-123-{idx}"
            worktree = get_worktree_path(mux_repo_path, branch)
            assert worktree.is_dir(), f"Worktree {branch} not found"


class TestNoPrompt:
    """Tests for behavior without prompts."""

    def test_add_without_prompt_skips_prompt_file(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Worktrees created without prompt flags should not create PROMPT.md."""
        env = mux_server
        branch_name = "feature-no-prompt"

        from ..conftest import prompt_file_for_branch

        write_workmux_config(mux_repo_path, panes=[])

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, branch_name
        )
        # Verify no PROMPT.md in worktree
        assert not (worktree_path / "PROMPT.md").exists()
        # Verify no prompt file in temp dir either
        assert not prompt_file_for_branch(env.tmp_path, branch_name).exists()


class TestShellAliases:
    """Tests for shell alias support with agents."""

    def test_agent_placeholder_respects_shell_aliases(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
        shell_cmd: ShellCommands,
    ):
        """Verifies that the <agent> placeholder triggers aliases defined in shell rc files."""
        env = mux_server
        branch_name = "feature-agent-alias"
        window_name = get_window_name(branch_name)
        marker_content = "alias_was_expanded"

        # Configure the default shell
        env.configure_default_shell(shell_cmd.path)

        # Append alias definition to RC file (PATH is already set by MuxEnvironment)
        rc_path = env.home_path / shell_cmd.rc_filename
        with rc_path.open("a") as f:
            f.write(shell_cmd.alias("claude", "claude --aliased") + "\n")

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

        write_workmux_config(
            mux_repo_path, agent="claude", panes=[{"command": "<agent>"}]
        )

        worktree_path = add_branch_and_get_worktree(
            env, workmux_exe_path, mux_repo_path, branch_name
        )
        marker_file = worktree_path / "alias_marker.txt"

        wait_for_file(
            env,
            marker_file,
            timeout=5.0,  # Increased for slower shells like nushell
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
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies --count cannot be combined with multiple --agent flags."""
        env = mux_server
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-feature -n 2 -a claude -a gemini",
            expect_fail=True,
        )
        assert "--count can only be used with zero or one --agent" in result.stderr

    def test_add_fails_with_foreach_and_agent(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies clap rejects --foreach in combination with --agent."""
        env = mux_server
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-feature --foreach 'p:a' -a claude",
            expect_fail=True,
        )
        assert (
            "'--foreach <FOREACH>' cannot be used with '--agent <AGENT>'"
            in result.stderr
        )

    def test_add_fails_with_foreach_mismatched_lengths(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies foreach parser enforces equal list lengths."""
        env = mux_server
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-feature --foreach 'platform:ios,android;lang:swift'",
            expect_fail=True,
        )
        assert (
            "All --foreach variables must have the same number of values"
            in result.stderr
        )

    def test_add_fails_with_prompt_but_no_pane_has_agent_placeholder(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies -p fails when panes don't include <agent> placeholder and don't run the default agent."""
        env = mux_server
        # Config with no <agent> placeholder - agent defaults to "claude" but no pane runs it
        write_workmux_config(mux_repo_path, panes=[{"command": "clear"}])
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-feature --prompt 'do something'",
            expect_fail=True,
        )
        # Agent defaults to "claude", so error says no pane runs claude
        assert "no pane is configured to run the agent" in result.stderr
        assert "claude" in result.stderr

    def test_add_fails_with_prompt_but_no_pane_runs_agent(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies -p fails when panes don't run the configured agent."""
        env = mux_server
        # Config with agent but panes don't use it
        write_workmux_config(
            mux_repo_path,
            agent="claude",
            panes=[{"command": "vim"}, {"command": "clear", "split": "horizontal"}],
        )
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-feature --prompt 'do something'",
            expect_fail=True,
        )
        assert "no pane is configured to run the agent" in result.stderr
        assert "claude" in result.stderr

    def test_add_fails_with_prompt_and_no_pane_cmds(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies -p fails when combined with --no-pane-cmds."""
        env = mux_server
        write_workmux_config(mux_repo_path, panes=[{"command": "<agent>"}])
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-feature --prompt 'do something' --no-pane-cmds",
            expect_fail=True,
        )
        assert "pane commands are disabled" in result.stderr


class TestTemplateVariableValidation:
    """Tests for template variable validation."""

    def test_add_single_worktree_leaves_prompt_template_syntax_literal(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Single-worktree prompts should not treat common template syntax as MiniJinja."""
        env = mux_server
        branch_name = "my-feature"
        prompt_text = "Use GitHub Actions $" + "{{ secrets.REGISTRY_TOKEN }} here"

        claude_path = fake_agent_installer.install(
            "claude",
            "#!/bin/sh\nprintf '%s' \"$2\" > out.txt",
        )
        write_workmux_config(
            mux_repo_path, agent=str(claude_path), panes=[{"command": "<agent>"}]
        )

        worktree = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)}",
        )
        window = get_window_name(branch_name)
        wait_for_file(
            env,
            worktree / "out.txt",
            window_name=window,
            worktree_path=worktree,
        )
        assert (worktree / "out.txt").read_text() == prompt_text

    def test_add_fails_with_undefined_branch_template_variable(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies branch template with undefined variable fails with helpful error."""
        env = mux_server
        write_workmux_config(mux_repo_path, panes=[])
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-feature -n 2 --branch-template '{{ base_name }}-{{ typo }}'",
            expect_fail=True,
        )
        assert "Invalid branch name template" in result.stderr
        assert "typo" in result.stderr

    def test_add_succeeds_with_valid_foreach_variables(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        """Verifies foreach variables are available in prompts."""
        env = mux_server
        base_name = "feature-valid-vars"

        claude_path = fake_agent_installer.install(
            "claude",
            "#!/bin/sh\nprintf '%s' \"$2\" > out.txt",
        )
        write_workmux_config(
            mux_repo_path, agent=str(claude_path), panes=[{"command": "<agent>"}]
        )

        # This should succeed because platform and lang are defined by foreach
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            (
                f"add {base_name} --foreach "
                "'platform:ios;lang:swift' "
                "--prompt 'Build {{ platform }} with {{ lang }}'"
            ),
        )

        # Verify worktree was created
        worktree = get_worktree_path(mux_repo_path, f"{base_name}-swift-ios")
        assert worktree.is_dir()

    def test_add_fails_with_typo_in_foreach_variable_name(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies typo in foreach variable name fails with helpful error."""
        env = mux_server
        write_workmux_config(mux_repo_path, panes=[{"command": "<agent>"}])
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            (
                "add my-feature --foreach "
                "'platform:ios,android' "
                "--prompt 'Build {{ plattform }}'"  # typo: plattform instead of platform
            ),
            expect_fail=True,
        )
        assert "undefined variables" in result.stderr
        assert "plattform" in result.stderr
        # Should suggest the correct variable
        assert "platform" in result.stderr


class TestPromptFileOnly:
    """Tests for --prompt-file-only flag."""

    def test_add_prompt_file_only_succeeds_without_agent_pane(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """--prompt-file-only should succeed even when no agent pane is configured."""
        env = mux_server
        branch_name = "feature-file-only"
        prompt_text = "Task for embedded agent"

        # Config with only vim - no agent pane at all
        write_workmux_config(mux_repo_path, panes=[{"command": "vim"}])

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)} --prompt-file-only",
        )

        # Prompt file should be written to the worktree
        assert_prompt_file_contents(env, branch_name, prompt_text, worktree_path)

    def test_add_prompt_file_only_does_not_inject_into_agent(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
        shell_cmd: ShellCommands,
    ):
        """--prompt-file-only should write the file but NOT inject the prompt into the agent."""
        env = mux_server
        branch_name = "feature-file-only-no-inject"
        prompt_text = "Should not be injected"
        output_filename = "agent_output.txt"
        window_name = get_window_name(branch_name)

        env.configure_default_shell(shell_cmd.path)

        # Install a fake claude that writes its arguments
        fake_agent_installer.install(
            "claude",
            f"""#!/bin/sh
# If prompt was injected, $1 would be "--" and $2 would be the prompt.
# In file-only mode, claude should receive no arguments.
echo "ARGS:$@" > "{output_filename}"
""",
        )

        write_workmux_config(
            mux_repo_path, agent="claude", panes=[{"command": "<agent>"}]
        )

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)} --prompt-file-only",
        )

        # Prompt file should still be written
        assert_prompt_file_contents(env, branch_name, prompt_text, worktree_path)

        # Agent should have been launched without prompt injection
        agent_output = worktree_path / output_filename
        wait_for_file(
            env,
            agent_output,
            timeout=5.0,
            window_name=window_name,
            worktree_path=worktree_path,
        )
        # In file-only mode, claude gets no arguments (no "-- <prompt>")
        assert agent_output.read_text().strip() == "ARGS:"

    def test_add_prompt_file_only_with_config_option(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """prompt_file_only config option should work the same as the CLI flag."""
        env = mux_server
        branch_name = "feature-file-only-config"
        prompt_text = "Config-driven file-only"

        # Config with prompt_file_only and no agent
        write_workmux_config(
            mux_repo_path,
            panes=[{"command": "vim"}],
            prompt_file_only=True,
        )

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            mux_repo_path,
            branch_name,
            extra_args=f"--prompt {shlex.quote(prompt_text)}",
        )

        assert_prompt_file_contents(env, branch_name, prompt_text, worktree_path)
