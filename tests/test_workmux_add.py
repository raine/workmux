import os
import shlex
from pathlib import Path

import pytest

from .conftest import (
    TmuxEnvironment,
    create_commit,
    get_window_name,
    get_worktree_path,
    poll_until,
    run_workmux_add,
    run_workmux_command,
    write_workmux_config,
    write_global_workmux_config,
)


def install_fake_agent(env: TmuxEnvironment, name: str, script_body: str) -> Path:
    """Creates a fake agent command in PATH so tmux panes can invoke it."""
    bin_dir = env.tmp_path / "agents-bin"
    bin_dir.mkdir(exist_ok=True)
    script_path = bin_dir / name
    script_path.write_text(script_body)
    script_path.chmod(0o755)

    new_path = f"{bin_dir}:{env.env.get('PATH', '')}"
    env.env["PATH"] = new_path
    env.tmux(["set-environment", "-g", "PATH", new_path])
    return script_path


def add_branch_and_get_worktree(
    env: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch_name: str,
    extra_args: str = "",
    command_target: str | None = None,
    **kwargs,
) -> Path:
    """Run `workmux add` and return the new worktree path."""
    target = command_target or branch_name
    command = f"add {target}"
    if extra_args:
        command = f"{command} {extra_args}"

    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        command,
        **kwargs,
    )

    worktree_path = get_worktree_path(repo_path, branch_name)
    assert worktree_path.is_dir()
    return worktree_path


def assert_window_exists(env: TmuxEnvironment, window_name: str) -> None:
    """Ensure a tmux window with the provided name exists."""
    result = env.tmux(["list-windows", "-F", "#{window_name}"])
    existing_windows = [w for w in result.stdout.strip().split("\n") if w]
    assert window_name in existing_windows, (
        f"Window {window_name!r} not found. Existing: {existing_windows!r}"
    )


def wait_for_pane_output(
    env: TmuxEnvironment, window_name: str, text: str, timeout: float = 2.0
) -> None:
    """Poll until the specified text appears in the pane."""

    final_content = f"Pane for window '{window_name}' was not captured."

    def _has_output() -> bool:
        nonlocal final_content
        capture_result = env.tmux(
            ["capture-pane", "-p", "-t", window_name], check=False
        )
        if capture_result.returncode == 0:
            final_content = capture_result.stdout
            return text in final_content
        final_content = (
            f"Error capturing pane for window '{window_name}':\n{capture_result.stderr}"
        )
        return False

    if not poll_until(_has_output, timeout=timeout):
        assert False, (
            f"Expected output {text!r} not found in window {window_name!r} within {timeout}s.\n"
            f"--- FINAL PANE CONTENT ---\n"
            f"{final_content}\n"
            f"--------------------------"
        )


def prompt_file_for_branch(tmp_path: Path, branch_name: str) -> Path:
    """Return the path to the prompt file for the given branch."""
    return tmp_path / f"workmux-prompt-{branch_name}.md"


def assert_prompt_file_contents(
    env: TmuxEnvironment, branch_name: str, expected_text: str
) -> None:
    """Assert that a prompt file exists for the branch and matches the expected text."""
    prompt_file = prompt_file_for_branch(env.tmp_path, branch_name)
    assert prompt_file.exists(), f"Prompt file not found at {prompt_file}"
    actual_text = prompt_file.read_text()
    assert actual_text == expected_text, (
        f"Content mismatch for prompt file: {prompt_file}"
    )


def wait_for_file(
    env: TmuxEnvironment,
    file_path: Path,
    timeout: float = 2.0,
    *,
    window_name: str | None = None,
    worktree_path: Path | None = None,
    debug_log_path: Path | None = None,
) -> None:
    """
    Poll for a file to exist. On timeout, fail with diagnostics about panes, worktrees, and logs.
    """

    def _file_exists() -> bool:
        return file_path.exists()

    if poll_until(_file_exists, timeout=timeout):
        return

    diagnostics: list[str] = [f"Target file: {file_path}"]

    if worktree_path is not None:
        diagnostics.append(f"Worktree path: {worktree_path}")
        if worktree_path.exists():
            try:
                files = sorted(p.name for p in worktree_path.iterdir())
                diagnostics.append(f"Worktree files: {files}")
            except Exception as exc:  # pragma: no cover - best effort diagnostics
                diagnostics.append(f"Error listing worktree files: {exc}")
        else:
            diagnostics.append("Worktree directory not found.")

    if debug_log_path is not None:
        if debug_log_path.exists():
            diagnostics.append(
                f"Debug log '{debug_log_path.name}':\n{debug_log_path.read_text()}"
            )
        else:
            diagnostics.append(f"Debug log '{debug_log_path.name}' not found.")

    if window_name is not None:
        pane_target = f"={window_name}.0"
        pane_content = f"Could not capture pane for window '{window_name}'."
        capture_result = env.tmux(
            ["capture-pane", "-p", "-t", pane_target], check=False
        )
        if capture_result.returncode == 0:
            pane_content = capture_result.stdout
        else:
            pane_content = (
                f"{pane_content}\nError capturing pane:\n{capture_result.stderr}"
            )
        diagnostics.append(f"Tmux pane '{pane_target}' content:\n{pane_content}")

    diag_str = "\n".join(diagnostics)
    assert False, (
        f"File not found after {timeout}s: {file_path}\n\n"
        f"-- Diagnostics --\n{diag_str}\n-----------------"
    )


def file_for_commit(worktree_path: Path, commit_message: str) -> Path:
    """Return the expected file path generated by create_commit for a message."""
    sanitized = commit_message.replace(" ", "_").replace(":", "")
    return worktree_path / f"file_for_{sanitized}.txt"


def assert_copied_file(
    worktree_path: Path, relative_path: str, expected_text: str | None = None
) -> Path:
    """Assert that a copied file exists in the worktree and is not a symlink."""
    file_path = worktree_path / relative_path
    assert file_path.exists(), f"Expected copied file {relative_path} to exist"
    assert not file_path.is_symlink(), (
        f"Expected {relative_path} to be a regular file, but found a symlink"
    )
    if expected_text is not None:
        assert file_path.read_text() == expected_text
    return file_path


def assert_symlink_to(worktree_path: Path, relative_path: str) -> Path:
    """Assert that a symlink exists in the worktree and return the path."""
    symlink_path = worktree_path / relative_path
    assert symlink_path.exists(), f"Expected symlink {relative_path} to exist"
    assert symlink_path.is_symlink(), f"Expected {relative_path} to be a symlink"
    return symlink_path


def configure_default_shell(shell: str | None = None) -> list[list[str]]:
    """Return tmux commands that configure the default shell for panes."""
    shell_path = shell or os.environ.get("SHELL", "/bin/zsh")
    return [["set-option", "-g", "default-shell", shell_path]]


def test_add_creates_worktree(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` creates a git worktree."""
    env = isolated_tmux_server
    branch_name = "feature-worktree"

    write_workmux_config(repo_path)

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )

    # Verify worktree in git's state
    worktree_list_result = env.run_command(["git", "worktree", "list"])
    assert branch_name in worktree_list_result.stdout

    # Verify worktree directory exists
    assert worktree_path.is_dir()


def test_add_creates_tmux_window(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` creates a tmux window with the correct name."""
    env = isolated_tmux_server
    branch_name = "feature-window"
    window_name = get_window_name(branch_name)

    write_workmux_config(repo_path)

    add_branch_and_get_worktree(env, workmux_exe_path, repo_path, branch_name)

    assert_window_exists(env, window_name)


def test_add_from_inside_worktree_creates_sibling_worktree(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """
    Verifies that running `workmux add` from inside an existing worktree
    creates the new worktree as a sibling to the repo, not nested inside the current worktree.
    """
    env = isolated_tmux_server
    first_branch = "feature-first"
    second_branch = "feature-second"

    write_workmux_config(repo_path)

    # 1. Create the first worktree normally
    first_worktree = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, first_branch
    )

    # Create a unique commit in the first worktree to verify ancestry
    commit_msg = "Commit in first worktree"
    create_commit(env, first_worktree, commit_msg)

    # 2. Run `workmux add` for the second branch FROM INSIDE the first worktree
    add_branch_and_get_worktree(
        env,
        workmux_exe_path,
        repo_path,
        second_branch,
        working_dir=first_worktree,
    )

    # 3. Verify the second worktree exists at the correct sibling location
    expected_path = get_worktree_path(repo_path, second_branch)
    assert expected_path.exists(), f"Worktree should be created at {expected_path}"
    assert expected_path.is_dir()

    # 4. Verify it is NOT nested inside the first worktree
    nested_path = (
        first_worktree.parent / f"{first_worktree.name}__worktrees" / second_branch
    )
    assert not nested_path.exists(), f"Worktree should not be nested at {nested_path}"

    # 5. Verify ancestry: The second branch should be based on the first branch
    expected_file = file_for_commit(expected_path, commit_msg)
    assert expected_file.exists(), (
        "New branch should inherit commit from the current worktree context"
    )

    # 6. Verify git internal state is correct
    worktree_list = env.run_command(["git", "worktree", "list"], cwd=repo_path).stdout
    assert str(expected_path) in worktree_list


def test_add_with_count_creates_numbered_worktrees(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `-n` spawns multiple numbered worktrees."""
    env = isolated_tmux_server
    base_name = "feature-counted"

    write_workmux_config(repo_path)
    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add {base_name} -n 2",
    )

    for idx in (1, 2):
        branch = f"{base_name}-{idx}"
        worktree = get_worktree_path(repo_path, branch)
        assert worktree.is_dir()
        assert_window_exists(env, get_window_name(branch))


def test_add_with_count_and_agent_uses_agent_in_all_instances(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies count with a single agent uses that agent in all generated worktrees."""
    env = isolated_tmux_server
    base_name = "feature-counted-agent"
    prompt_text = "Task {{ num }}"

    fake_gemini_path = install_fake_agent(
        env,
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


def test_add_inline_prompt_injects_into_claude(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Inline prompts should be written to PROMPT.md and passed to claude via command substitution."""
    env = isolated_tmux_server
    branch_name = "feature-inline-prompt"
    prompt_text = "Implement inline prompt"
    output_filename = "claude_prompt.txt"
    window_name = get_window_name(branch_name)

    fake_claude_path = install_fake_agent(
        env,
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


def test_add_prompt_file_injects_into_gemini(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Prompt file flag should populate PROMPT.md and pass it to gemini via command substitution."""
    env = isolated_tmux_server
    branch_name = "feature-file-prompt"
    window_name = get_window_name(branch_name)
    prompt_source = repo_path / "prompt_source.txt"
    prompt_source.write_text("File-based instructions")
    output_filename = "gemini_prompt.txt"

    fake_gemini_path = install_fake_agent(
        env,
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


def test_add_uses_agent_from_config(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """The <agent> placeholder should use the agent configured in .workmux.yaml when --agent is not passed."""
    env = isolated_tmux_server
    branch_name = "feature-config-agent"
    window_name = get_window_name(branch_name)
    prompt_text = "Using configured agent"
    output_filename = "agent_output.txt"

    # Install fake gemini agent
    fake_gemini_path = install_fake_agent(
        env,
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """The --agent flag should override the default agent and inject prompts correctly."""
    env = isolated_tmux_server
    branch_name = "feature-agent-override"
    window_name = get_window_name(branch_name)
    prompt_text = "This is for the override agent"
    output_filename = "agent_output.txt"

    # Create two fake agents: a default one and the one we'll specify via the flag.
    # Default agent (claude)
    install_fake_agent(
        env,
        "claude",
        "#!/bin/sh\necho 'default agent ran' > default_agent.txt",
    )

    # Override agent (gemini)
    fake_gemini_path = install_fake_agent(
        env,
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


def test_add_multi_agent_creates_separate_worktrees_and_runs_correct_agents(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies `-a` with multiple agents creates distinct worktrees for each agent."""
    env = isolated_tmux_server
    base_name = "feature-multi-agent"
    prompt_text = "Implement for {{ agent }}"

    claude_path = install_fake_agent(
        env,
        "claude",
        "#!/bin/sh\nprintf '%s' \"$2\" > claude_out.txt",
    )
    gemini_path = install_fake_agent(
        env,
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
    assert (claude_worktree / "claude_out.txt").read_text() == "Implement for claude"

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
    assert (gemini_worktree / "gemini_out.txt").read_text() == "Implement for gemini"


def test_add_foreach_creates_worktrees_from_matrix(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies foreach matrix expands into multiple worktrees with templated prompts."""
    env = isolated_tmux_server
    base_name = "feature-matrix"
    prompt_text = "Build for {{ platform }} using {{ lang }}"

    claude_path = install_fake_agent(
        env,
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


def test_add_with_custom_branch_template(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_add_fails_with_count_and_multiple_agents(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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
        "'--foreach <FOREACH>' cannot be used with '--agent <AGENT>'" in result.stderr
    )


def test_add_fails_with_foreach_mismatched_lengths(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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
        "All --foreach variables must have the same number of values" in result.stderr
    )


def test_add_executes_post_create_hooks(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_add_without_prompt_skips_prompt_file(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Worktrees created without prompt flags should not create PROMPT.md."""
    env = isolated_tmux_server
    branch_name = "feature-no-prompt"

    write_workmux_config(repo_path, panes=[])

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )
    # Verify no PROMPT.md in worktree
    assert not (worktree_path / "PROMPT.md").exists()
    # Verify no prompt file in temp dir either
    assert not prompt_file_for_branch(env.tmp_path, branch_name).exists()


def test_add_can_skip_post_create_hooks(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_add_executes_pane_commands(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_add_copies_directories(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_add_can_skip_file_operations(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_add_sources_shell_rc_files(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_agent_placeholder_respects_shell_aliases(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that the <agent> placeholder triggers aliases defined in shell rc files."""
    env = isolated_tmux_server
    branch_name = "feature-agent-alias"
    window_name = get_window_name(branch_name)
    marker_content = "alias_was_expanded"

    # Get the path where the fake agent will be installed
    fake_bin_dir = env.tmp_path / "agents-bin"

    # Write a .zshrc that prepends our fake bin to the PATH and defines the alias.
    # This ensures the shell finds our fake `claude` before any system-wide one.
    (env.home_path / ".zshrc").write_text(
        f"""
export PATH="{fake_bin_dir}:$PATH"
alias claude='claude --aliased'
""".strip()
        + "\n"
    )

    install_fake_agent(
        env,
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


def test_project_config_overrides_global_config(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_global_placeholder_merges_post_create_commands(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_global_placeholder_merges_file_operations(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """The '<global>' placeholder should merge copy and symlink file operations."""
    env = isolated_tmux_server
    branch_name = "feature-global-files"

    # Create files/directories that will be copied or symlinked.
    global_copy = repo_path / "global.env"
    project_copy = repo_path / "project.env"
    global_copy.write_text("GLOBAL")
    project_copy.write_text("PROJECT")

    global_dir = repo_path / "global_cache"
    project_dir = repo_path / "project_cache"
    global_dir.mkdir()
    (global_dir / "shared.txt").write_text("global data")
    project_dir.mkdir()
    (project_dir / "local.txt").write_text("project data")

    env.run_command(
        ["git", "add", "global.env", "project.env", "global_cache", "project_cache"],
        cwd=repo_path,
    )
    env.run_command(
        ["git", "commit", "-m", "Add files for global placeholder tests"], cwd=repo_path
    )

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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """`<global>` can merge copy patterns while symlink patterns fully override."""
    env = isolated_tmux_server
    branch_name = "feature-partial-file-merge"

    ignored_files = [
        "global_copy.txt",
        "project_copy.txt",
        "global_symlink_dir/",
        "project_symlink_dir/",
    ]
    with (repo_path / ".gitignore").open("a") as gitignore:
        for entry in ignored_files:
            gitignore.write(f"{entry}\n")

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


def test_project_empty_file_lists_override_global_lists(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Explicit empty lists suppress the corresponding global file operations."""
    env = isolated_tmux_server
    branch_name = "feature-empty-file-override"

    ignored_files = [
        "global_only.env",
        "global_shared_dir/",
    ]
    with (repo_path / ".gitignore").open("a") as gitignore:
        for entry in ignored_files:
            gitignore.write(f"{entry}\n")

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


def test_project_panes_replace_global_panes(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_add_from_specific_branch(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add --base` creates a worktree from a specific branch."""
    env = isolated_tmux_server
    new_branch = "feature-from-base"

    write_workmux_config(repo_path)

    # Create a commit on the current branch
    create_commit(env, repo_path, "Add base file")

    # Get current branch name
    result = env.run_command(["git", "branch", "--show-current"], cwd=repo_path)
    base_branch = result.stdout.strip()

    # Run workmux add with --base flag
    worktree_path = add_branch_and_get_worktree(
        env,
        workmux_exe_path,
        repo_path,
        new_branch,
        extra_args=f"--base {base_branch}",
    )

    # Verify the new branch contains the file from base branch
    expected_file = file_for_commit(worktree_path, "Add base file")
    assert expected_file.exists()

    # Verify tmux window was created
    window_name = get_window_name(new_branch)
    assert_window_exists(env, window_name)


def test_add_defaults_to_current_branch(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """`workmux add` without --base should inherit from the current branch."""
    env = isolated_tmux_server
    base_branch = "feature-default-base"
    stacked_branch = "feature-default-child"
    commit_message = "Stack default change"

    write_workmux_config(repo_path)

    env.run_command(["git", "checkout", "-b", base_branch], cwd=repo_path)
    create_commit(env, repo_path, commit_message)

    stacked_worktree = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, stacked_branch
    )
    expected_file = file_for_commit(stacked_worktree, commit_message)
    assert expected_file.exists()

    window_name = get_window_name(stacked_branch)
    assert_window_exists(env, window_name)


def test_add_errors_when_detached_head_without_base(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Detached HEAD states should require --base."""
    env = isolated_tmux_server
    branch_name = "feature-detached-head"

    write_workmux_config(repo_path)

    head_sha = env.run_command(
        ["git", "rev-parse", "HEAD"], cwd=repo_path
    ).stdout.strip()
    env.run_command(["git", "checkout", head_sha], cwd=repo_path)

    result = run_workmux_command(
        env, workmux_exe_path, repo_path, f"add {branch_name}", expect_fail=True
    )

    assert "detached HEAD" in result.stderr


def test_add_allows_detached_head_with_explicit_base(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Detached HEAD states can still create worktrees when --base is provided."""
    env = isolated_tmux_server
    branch_name = "feature-detached-head-base"
    commit_message = "Detached baseline"

    write_workmux_config(repo_path)
    create_commit(env, repo_path, commit_message)

    head_sha = env.run_command(
        ["git", "rev-parse", "HEAD"], cwd=repo_path
    ).stdout.strip()
    env.run_command(["git", "checkout", head_sha], cwd=repo_path)

    worktree_path = add_branch_and_get_worktree(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        extra_args="--base main",
    )

    expected_file = file_for_commit(worktree_path, commit_message)
    assert expected_file.exists()


def test_add_reuses_existing_branch(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` reuses an existing branch instead of creating a new one."""
    env = isolated_tmux_server
    branch_name = "feature-existing-branch"
    commit_message = "Existing branch changes"

    write_workmux_config(repo_path)

    # Remember the default branch so we can switch back after preparing the feature branch
    current_branch_result = env.run_command(
        ["git", "branch", "--show-current"], cwd=repo_path
    )
    default_branch = current_branch_result.stdout.strip()

    # Create and populate an existing branch
    env.run_command(["git", "checkout", "-b", branch_name], cwd=repo_path)
    create_commit(env, repo_path, commit_message)
    branch_head = env.run_command(
        ["git", "rev-parse", "HEAD"], cwd=repo_path
    ).stdout.strip()

    # Switch back to the default branch so workmux add runs from a typical state
    env.run_command(["git", "checkout", default_branch], cwd=repo_path)

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )
    expected_file = file_for_commit(worktree_path, commit_message)
    assert expected_file.exists()
    assert expected_file.read_text() == f"content for {commit_message}"

    # The branch should still point to the commit we created earlier
    branch_tip = env.run_command(
        ["git", "rev-parse", branch_name], cwd=repo_path
    ).stdout.strip()
    assert branch_tip == branch_head


def test_add_from_remote_branch(
    isolated_tmux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    remote_repo_path: Path,
):
    """When the branch exists only on the remote, workmux add should fetch and track it."""
    env = isolated_tmux_server
    remote_branch_path = "feature/remote-pr"
    remote_ref = f"origin/{remote_branch_path}"
    commit_message = "Remote PR work"

    write_workmux_config(repo_path)

    # Wire up the repo to a bare remote and push the default branch.
    env.run_command(
        ["git", "remote", "add", "origin", str(remote_repo_path)], cwd=repo_path
    )
    env.run_command(["git", "push", "-u", "origin", "main"], cwd=repo_path)

    # Create a branch with commits and push it to the remote.
    env.run_command(["git", "checkout", "-b", remote_branch_path], cwd=repo_path)
    create_commit(env, repo_path, commit_message)
    remote_tip = env.run_command(
        ["git", "rev-parse", remote_branch_path], cwd=repo_path
    ).stdout.strip()
    env.run_command(["git", "push", "-u", "origin", remote_branch_path], cwd=repo_path)

    # Remove the local branch and remote-tracking ref so the branch only exists on the remote.
    env.run_command(["git", "checkout", "main"], cwd=repo_path)
    env.run_command(["git", "branch", "-D", remote_branch_path], cwd=repo_path)
    env.run_command(
        ["git", "update-ref", "-d", f"refs/remotes/{remote_ref}"],
        cwd=repo_path,
    )

    worktree_path = add_branch_and_get_worktree(
        env,
        workmux_exe_path,
        repo_path,
        remote_branch_path,
        command_target=remote_ref,
    )
    expected_file = file_for_commit(worktree_path, commit_message)
    assert expected_file.exists()
    assert expected_file.read_text() == f"content for {commit_message}"

    # Local branch should point to the remote commit and track origin/<branch_name>.
    branch_tip = env.run_command(
        ["git", "rev-parse", remote_branch_path], cwd=repo_path
    ).stdout.strip()
    assert branch_tip == remote_tip

    upstream_tip = env.run_command(
        ["git", "rev-parse", f"{remote_branch_path}@{{upstream}}"], cwd=repo_path
    ).stdout.strip()
    assert upstream_tip == remote_tip

    origin_tip = env.run_command(
        ["git", "rev-parse", remote_ref], cwd=repo_path
    ).stdout.strip()
    assert origin_tip == remote_tip


def test_add_fails_when_worktree_exists(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` fails with a clear message if the worktree already exists."""
    env = isolated_tmux_server
    branch_name = "feature-existing-worktree"
    existing_worktree_path = repo_path.parent / "existing_worktree_dir"

    write_workmux_config(repo_path)

    # Create the branch and then return to the default branch
    env.run_command(["git", "checkout", "-b", branch_name], cwd=repo_path)
    env.run_command(["git", "checkout", "main"], cwd=repo_path)

    # Manually create a git worktree for the branch to simulate the pre-existing state
    env.run_command(
        ["git", "worktree", "add", str(existing_worktree_path), branch_name],
        cwd=repo_path,
    )

    with pytest.raises(AssertionError) as excinfo:
        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    stderr = str(excinfo.value)
    assert f"A worktree for branch '{branch_name}' already exists." in stderr
    assert "Use 'workmux open" in stderr


def test_add_copies_single_file(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` copies a single file to the worktree."""
    env = isolated_tmux_server
    branch_name = "feature-copy-file"

    # Create a file in the repo root to copy
    env_file = repo_path / ".env"
    env_file.write_text("SECRET_KEY=test123")

    # Commit the file to avoid uncommitted changes
    env.run_command(["git", "add", ".env"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add .env file"], cwd=repo_path)

    # Configure workmux to copy the .env file
    write_workmux_config(repo_path, files={"copy": [".env"]}, env=env)

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )
    assert_copied_file(worktree_path, ".env", "SECRET_KEY=test123")


def test_add_copies_multiple_files_with_glob(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` copies multiple files using glob patterns."""
    env = isolated_tmux_server
    branch_name = "feature-copy-glob"

    # Create multiple .local files in the repo root
    (repo_path / ".env.local").write_text("LOCAL_VAR=value1")
    (repo_path / ".secrets.local").write_text("API_KEY=secret")

    # Commit the files
    env.run_command(["git", "add", "*.local"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add local files"], cwd=repo_path)

    # Configure workmux to copy all .local files
    write_workmux_config(repo_path, files={"copy": ["*.local"]}, env=env)

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )
    assert_copied_file(worktree_path, ".env.local", "LOCAL_VAR=value1")
    assert_copied_file(worktree_path, ".secrets.local", "API_KEY=secret")


def test_add_copies_file_with_parent_directories(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` creates parent directories when copying nested files."""
    env = isolated_tmux_server
    branch_name = "feature-copy-nested"

    # Create a nested file structure
    config_dir = repo_path / "config"
    config_dir.mkdir()
    (config_dir / "app.conf").write_text("setting=value")

    # Commit the files
    env.run_command(["git", "add", "config/"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add config files"], cwd=repo_path)

    # Configure workmux to copy the nested file
    write_workmux_config(repo_path, files={"copy": ["config/app.conf"]}, env=env)

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )
    assert_copied_file(worktree_path, "config/app.conf", "setting=value")
    assert (worktree_path / "config").is_dir()


def test_add_symlinks_single_file(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` creates a symlink for a single file."""
    env = isolated_tmux_server
    branch_name = "feature-symlink-file"

    # Create a file in the repo root to symlink
    shared_file = repo_path / "shared.txt"
    shared_file.write_text("shared content")

    # Commit the file
    env.run_command(["git", "add", "shared.txt"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add shared file"], cwd=repo_path)

    # Configure workmux to symlink the file
    write_workmux_config(repo_path, files={"symlink": ["shared.txt"]}, env=env)

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )
    symlinked_file = assert_symlink_to(worktree_path, "shared.txt")
    assert symlinked_file.read_text() == "shared content"


def test_add_symlinks_directory(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` creates a symlink for a directory."""
    env = isolated_tmux_server
    branch_name = "feature-symlink-dir"

    # Create a directory in the repo root to symlink
    node_modules = repo_path / "node_modules"
    node_modules.mkdir()
    (node_modules / "package.json").write_text('{"name": "test"}')

    # Commit the directory
    env.run_command(["git", "add", "node_modules/"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add node_modules"], cwd=repo_path)

    # Configure workmux to symlink the directory
    write_workmux_config(repo_path, files={"symlink": ["node_modules"]}, env=env)

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )
    symlinked_dir = assert_symlink_to(worktree_path, "node_modules")
    assert (symlinked_dir / "package.json").exists()


def test_add_symlinks_multiple_items_with_glob(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` creates symlinks for multiple items using glob patterns."""
    env = isolated_tmux_server
    branch_name = "feature-symlink-glob"

    # Create multiple cache directories
    (repo_path / ".cache").mkdir()
    (repo_path / ".cache" / "data.txt").write_text("cache data")
    (repo_path / ".pnpm-store").mkdir()
    (repo_path / ".pnpm-store" / "index.txt").write_text("pnpm index")

    # Commit the directories
    env.run_command(["git", "add", ".cache/", ".pnpm-store/"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add cache dirs"], cwd=repo_path)

    # Configure workmux to symlink using glob patterns
    write_workmux_config(repo_path, files={"symlink": [".*-store", ".cache"]}, env=env)

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )
    cache_symlink = assert_symlink_to(worktree_path, ".cache")
    pnpm_symlink = assert_symlink_to(worktree_path, ".pnpm-store")
    assert (cache_symlink / "data.txt").exists()
    assert (pnpm_symlink / "index.txt").exists()


def test_add_symlinks_are_relative(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that created symlinks use relative paths, not absolute paths."""
    env = isolated_tmux_server
    branch_name = "feature-symlink-relative"

    # Create a file to symlink
    test_file = repo_path / "test.txt"
    test_file.write_text("test content")

    # Commit the file
    env.run_command(["git", "add", "test.txt"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add test file"], cwd=repo_path)

    # Configure workmux to symlink the file
    write_workmux_config(repo_path, files={"symlink": ["test.txt"]}, env=env)

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )
    symlinked_file = assert_symlink_to(worktree_path, "test.txt")

    # Verify the symlink points to the correct relative path
    source_file = repo_path / "test.txt"
    expected_target = os.path.relpath(source_file, symlinked_file.parent)
    link_target = os.readlink(symlinked_file)
    assert link_target == expected_target, (
        f"Symlink target incorrect. Expected: {expected_target}, Got: {link_target}"
    )


def test_add_symlink_replaces_existing_file(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that symlinking replaces an existing file at the destination."""
    env = isolated_tmux_server
    branch_name = "feature-symlink-replace"

    # Create a file to symlink
    source_file = repo_path / "source.txt"
    source_file.write_text("source content")

    # Commit the file
    env.run_command(["git", "add", "source.txt"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add source file"], cwd=repo_path)

    # Configure workmux to symlink the file
    write_workmux_config(repo_path, files={"symlink": ["source.txt"]}, env=env)

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
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
        env, workmux_exe_path, repo_path, branch_name_2
    )
    dest_file_2 = worktree_path_2 / "source.txt"
    assert dest_file_2.is_symlink()
    assert dest_file_2.read_text() == "source content"


def test_add_symlink_with_nested_structure(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that symlinking works with nested directory structures."""
    env = isolated_tmux_server
    branch_name = "feature-symlink-nested"

    # Create a nested directory structure
    nested_dir = repo_path / "lib" / "cache"
    nested_dir.mkdir(parents=True)
    (nested_dir / "data.db").write_text("database content")

    # Commit the structure
    env.run_command(["git", "add", "lib/"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add nested structure"], cwd=repo_path)

    # Configure workmux to symlink the nested directory
    write_workmux_config(repo_path, files={"symlink": ["lib/cache"]}, env=env)

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )
    symlinked_dir = assert_symlink_to(worktree_path, "lib/cache")
    assert (symlinked_dir / "data.db").read_text() == "database content"
    # Verify parent directory exists and is NOT a symlink
    assert (worktree_path / "lib").is_dir()
    assert not (worktree_path / "lib").is_symlink()


def test_add_combines_copy_and_symlink(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that copy and symlink operations can be used together."""
    env = isolated_tmux_server
    branch_name = "feature-combined-ops"

    # Create files for both copy and symlink
    (repo_path / ".env").write_text("SECRET=abc123")
    node_modules = repo_path / "node_modules"
    node_modules.mkdir()
    (node_modules / "package.json").write_text('{"name": "test"}')

    # Commit both
    env.run_command(["git", "add", ".env", "node_modules/"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add files"], cwd=repo_path)

    # Configure workmux to both copy and symlink
    write_workmux_config(
        repo_path, files={"copy": [".env"], "symlink": ["node_modules"]}, env=env
    )

    worktree_path = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch_name
    )

    assert_copied_file(worktree_path, ".env", "SECRET=abc123")
    symlinked_dir = assert_symlink_to(worktree_path, "node_modules")
    assert (symlinked_dir / "package.json").exists()


def test_add_file_operations_with_empty_config(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that workmux add works when files config is empty or missing."""
    env = isolated_tmux_server
    branch_name = "feature-no-files"

    # Configure workmux with no file operations
    write_workmux_config(repo_path, env=env)

    # Should succeed without errors
    add_branch_and_get_worktree(env, workmux_exe_path, repo_path, branch_name)


def test_add_file_operations_with_nonexistent_pattern(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that workmux handles glob patterns that match no files gracefully."""
    env = isolated_tmux_server
    branch_name = "feature-no-match"

    # Configure workmux with patterns that don't match anything
    write_workmux_config(
        repo_path,
        files={"copy": ["nonexistent-*.txt"], "symlink": ["missing-dir"]},
        env=env,
    )

    # Should succeed without errors (no matches is not an error)
    add_branch_and_get_worktree(env, workmux_exe_path, repo_path, branch_name)


def test_add_copy_with_path_traversal_fails(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` fails if a copy path attempts to traverse outside the repo."""
    env = isolated_tmux_server
    branch_name = "feature-copy-traversal"

    # Create a sensitive file outside the repository
    (repo_path.parent / "sensitive_file").write_text("secret")

    write_workmux_config(repo_path, files={"copy": ["../sensitive_file"]}, env=env)

    with pytest.raises(AssertionError) as excinfo:
        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # The error should indicate path traversal or invalid path
    stderr = str(excinfo.value)
    assert (
        "Path traversal" in stderr
        or "outside" in stderr
        or "No such file" in stderr
        or "pattern matched nothing" in stderr
    )


def test_add_symlink_with_path_traversal_fails(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add` fails if a symlink path attempts to traverse outside the repo."""
    env = isolated_tmux_server
    branch_name = "feature-symlink-traversal"

    # Create a directory outside the repository that will be matched by the glob
    (repo_path.parent / "some_dir").mkdir()
    (repo_path.parent / "some_dir" / "file.txt").write_text("outside repo")

    write_workmux_config(repo_path, files={"symlink": ["../some_dir"]}, env=env)

    with pytest.raises(AssertionError) as excinfo:
        run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # The error should indicate path traversal
    stderr = str(excinfo.value)
    assert "Path traversal" in stderr or "outside" in stderr


def test_add_symlink_overwrites_conflicting_file_from_git(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies a symlink operation overwrites a conflicting file checked out by git."""
    env = isolated_tmux_server
    branch_name = "feature-symlink-overwrite"

    # In main repo root, create the directory to be symlinked
    (repo_path / "node_modules").mkdir()
    (repo_path / "node_modules" / "dep.js").write_text("content")
    env.run_command(["git", "add", "node_modules/"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add real node_modules"], cwd=repo_path)

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


def test_add_background_creates_window_without_switching(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that `workmux add --background` creates window without switching to it."""
    env = isolated_tmux_server
    branch_name = "feature-background"
    initial_window = "initial"

    write_workmux_config(repo_path)

    # Create an initial window and remember it
    env.tmux(["new-window", "-n", initial_window])
    env.tmux(["select-window", "-t", initial_window])

    # Get current window before running add
    current_before = env.tmux(["display-message", "-p", "#{window_name}"])
    assert initial_window in current_before.stdout

    # Run workmux add with --background flag
    worktree_path = add_branch_and_get_worktree(
        env,
        workmux_exe_path,
        repo_path,
        branch_name,
        extra_args="--background",
    )

    # Verify worktree was created
    assert worktree_path.is_dir()

    # Verify the new window exists
    window_name = get_window_name(branch_name)
    assert_window_exists(env, window_name)

    # Verify we're still on the initial window (didn't switch)
    current_after = env.tmux(["display-message", "-p", "#{window_name}"])
    assert initial_window in current_after.stdout


def test_rescue_moves_uncommitted_changes_to_new_worktree(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_rescue_with_untracked_files(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_rescue_creates_tmux_window(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_rescue_fails_with_no_uncommitted_changes(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_rescue_with_both_staged_and_unstaged_changes(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_rescue_respects_no_hooks_flag(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_rescue_preserves_file_modes(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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
    import stat

    assert rescued_script.stat().st_mode & stat.S_IXUSR


def test_rescue_handles_modified_tracked_files(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that add --with-changes handles modifications to tracked files."""
    env = isolated_tmux_server
    branch_name = "feature-rescue-modified"

    write_workmux_config(repo_path)

    # Create and commit a file
    tracked_file = repo_path / "tracked.txt"
    tracked_file.write_text("original content")
    env.run_command(["git", "add", "tracked.txt"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add tracked file"], cwd=repo_path)

    # Modify the file
    tracked_file.write_text("modified content")

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


def test_rescue_executes_post_create_hooks(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_rescue_with_only_untracked_files(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_rescue_fails_with_only_untracked_files_without_u_flag(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
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


def test_rescue_with_gitignored_files(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that add --with-changes -u does NOT include gitignored files."""
    env = isolated_tmux_server
    branch_name = "feature-rescue-gitignored"

    write_workmux_config(repo_path)

    # Create .gitignore
    gitignore = repo_path / ".gitignore"
    gitignore.write_text("*.log\nignored_dir/\n")
    env.run_command(["git", "add", ".gitignore"], cwd=repo_path)
    env.run_command(["git", "commit", "-m", "Add gitignore"], cwd=repo_path)

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


# ==============================================================================
# --name flag tests
# ==============================================================================


def test_add_with_name_uses_custom_handle(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --name overrides the default handle for worktree directory and tmux window,
    while preserving the original git branch name."""
    from .conftest import slugify

    env = isolated_tmux_server
    branch_name = "feature/my-new-feature"
    custom_name = "my-feature"

    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add {branch_name} --name {custom_name}",
    )

    # Worktree should use the custom name (slugified)
    expected_handle = slugify(custom_name)
    worktree_path = repo_path.parent / f"{repo_path.name}__worktrees" / expected_handle
    assert worktree_path.is_dir(), f"Expected worktree at {worktree_path}"

    # Worktree should NOT exist at the default (branch-derived) path
    default_handle = slugify(branch_name)
    default_path = repo_path.parent / f"{repo_path.name}__worktrees" / default_handle
    assert not default_path.exists()

    # Tmux window should use the custom name
    expected_window = f"wm-{expected_handle}"
    assert_window_exists(env, expected_window)

    # Git branch should use the original name, not the handle
    result = env.run_command(
        ["git", "-C", str(worktree_path), "rev-parse", "--abbrev-ref", "HEAD"]
    )
    assert result.stdout.strip() == branch_name


def test_add_with_name_fails_with_multi_worktree_flags(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --name cannot be combined with multi-worktree generation flags."""
    env = isolated_tmux_server

    # Test with --count > 1
    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add my-feature --name custom -n 2",
        expect_fail=True,
    )
    assert "--name cannot be used with multi-worktree generation" in result.stderr

    # Test with --foreach
    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add my-feature --name custom --foreach 'platform:ios,android'",
        expect_fail=True,
    )
    assert "--name cannot be used with multi-worktree generation" in result.stderr

    # Test with multiple --agent flags
    result = run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        "add my-feature --name custom -a claude -a gemini",
        expect_fail=True,
    )
    assert "--name cannot be used with multi-worktree generation" in result.stderr


def test_add_with_name_works_with_rescue(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies --name works with --with-changes (rescue) flow."""
    from .conftest import slugify

    env = isolated_tmux_server
    branch_name = "rescue-feature"
    custom_name = "rescued"

    # Create uncommitted changes in the main repo
    test_file = repo_path / "uncommitted.txt"
    test_file.write_text("uncommitted content")

    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add --with-changes {branch_name} --name {custom_name} -u",
    )

    # Verify worktree uses custom name
    expected_handle = slugify(custom_name)
    worktree_path = repo_path.parent / f"{repo_path.name}__worktrees" / expected_handle
    assert worktree_path.is_dir()

    # Verify the changes were moved
    assert (worktree_path / "uncommitted.txt").exists()

    # Verify original worktree is clean
    assert not (repo_path / "uncommitted.txt").exists()


# ==============================================================================
# worktree_naming and worktree_prefix config tests
# ==============================================================================


def test_add_respects_basename_naming_strategy(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that worktree_naming: basename uses only the last part of the branch."""
    env = isolated_tmux_server
    branch_name = "feature/user-auth"
    expected_handle = "user-auth"

    write_workmux_config(repo_path, worktree_naming="basename")

    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Verify worktree directory uses basename
    worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
    assert (worktrees_dir / expected_handle).is_dir()

    # Verify tmux window uses basename
    assert_window_exists(env, f"wm-{expected_handle}")


def test_add_respects_worktree_prefix(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that worktree_prefix is prepended to the handle."""
    env = isolated_tmux_server
    branch_name = "api-fix"
    prefix = "backend-"
    expected_handle = f"{prefix}{branch_name}"

    write_workmux_config(repo_path, worktree_prefix=prefix)

    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
    assert (worktrees_dir / expected_handle).is_dir()
    assert_window_exists(env, f"wm-{expected_handle}")


def test_add_combines_basename_and_prefix(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that basename strategy and prefix work together."""
    env = isolated_tmux_server
    branch_name = "team/frontend/login"
    expected_handle = "web-login"

    write_workmux_config(
        repo_path,
        worktree_naming="basename",
        worktree_prefix="web-",
    )

    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"
    assert (worktrees_dir / expected_handle).is_dir()
    assert_window_exists(env, f"wm-{expected_handle}")


def test_explicit_name_overrides_naming_config(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """Verifies that --name overrides all config (naming strategy and prefix)."""
    env = isolated_tmux_server
    branch_name = "feature/complex-stuff"
    explicit_name = "simple-name"

    # Configure options that should be ignored when --name is used
    write_workmux_config(
        repo_path,
        worktree_naming="basename",
        worktree_prefix="ignored-",
    )

    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add {branch_name} --name {explicit_name}",
    )

    worktrees_dir = repo_path.parent / f"{repo_path.name}__worktrees"

    # Should be exactly what was passed in --name, ignoring prefix
    assert (worktrees_dir / explicit_name).is_dir()
    assert_window_exists(env, f"wm-{explicit_name}")

    # Verify the config was ignored
    assert not (worktrees_dir / "complex-stuff").exists()
    assert not (worktrees_dir / "ignored-simple-name").exists()


def test_post_create_hook_receives_workmux_handle_env_var(
    isolated_tmux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """
    Verifies that post_create hooks receive the WORKMUX_HANDLE environment variable
    with the derived handle (not the raw branch name).
    """
    env = isolated_tmux_server

    # Branch with prefix that will be stripped by basename
    branch_name = "feature/my-feature"
    expected_handle = "my-feature"  # basename of branch, slugified

    # Output file where the hook will write the env var
    handle_output_file = repo_path / "handle_from_hook.txt"

    # Configure basename naming and a hook that writes WORKMUX_HANDLE to a file
    write_workmux_config(
        repo_path,
        worktree_naming="basename",
        post_create=[f"echo $WORKMUX_HANDLE > {handle_output_file}"],
    )

    # Create the worktree
    run_workmux_add(env, workmux_exe_path, repo_path, branch_name)

    # Verify the hook was run and received the correct handle
    assert handle_output_file.exists(), "Hook should have created the output file"
    actual_handle = handle_output_file.read_text().strip()
    assert actual_handle == expected_handle, (
        f"WORKMUX_HANDLE should be '{expected_handle}' (derived handle), "
        f"not '{actual_handle}' (which might be the raw branch name)"
    )
