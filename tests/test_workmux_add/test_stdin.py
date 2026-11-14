"""Tests for stdin input support in `workmux add`."""

import shlex
from pathlib import Path

from ..conftest import (
    DEFAULT_WINDOW_PREFIX,
    MuxEnvironment,
    assert_window_exists,
    get_scripts_dir,
    get_worktree_path,
    poll_until_file_has_content,
    run_workmux_command,
    slugify,
    write_workmux_config,
)


class TestStdinInput:
    """Tests for piping input to workmux add via stdin."""

    def test_open_empty_stdin_pipe_does_not_block(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that an open stdin pipe with no data is ignored."""
        env = mux_server
        # Stdin handling does not depend on split-pane shell handshakes.
        write_workmux_config(mux_repo_path, panes=[{"focus": True}])

        scripts_dir = get_scripts_dir(env)
        stdout_file = scripts_dir / "empty_stdin_stdout.txt"
        stderr_file = scripts_dir / "empty_stdin_stderr.txt"
        exit_code_file = scripts_dir / "empty_stdin_exit_code.txt"
        script_file = scripts_dir / "empty_stdin_run.py"

        for path in [stdout_file, stderr_file, exit_code_file]:
            if path.exists():
                path.unlink()

        script_file.write_text(
            "import os\n"
            "import subprocess\n"
            "import sys\n"
            "read_fd, write_fd = os.pipe()\n"
            "try:\n"
            "    with os.fdopen(read_fd, 'rb', closefd=True) as stdin:\n"
            "        proc = subprocess.Popen([sys.argv[1], 'add', 'topic'], cwd=sys.argv[2], stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)\n"
            "        try:\n"
            "            stdout, stderr = proc.communicate(timeout=3)\n"
            "        except subprocess.TimeoutExpired:\n"
            "            proc.kill()\n"
            "            proc.communicate()\n"
            "            raise AssertionError('workmux add blocked on open empty stdin pipe')\n"
            "finally:\n"
            "    os.close(write_fd)\n"
            "open(sys.argv[3], 'w').write(stdout)\n"
            "open(sys.argv[4], 'w').write(stderr)\n"
            "open(sys.argv[5], 'w').write(str(proc.returncode))\n"
        )

        env.send_keys(
            "test:",
            " ".join(
                shlex.quote(str(arg))
                for arg in [
                    "python3",
                    script_file,
                    workmux_exe_path,
                    mux_repo_path,
                    stdout_file,
                    stderr_file,
                    exit_code_file,
                ]
            ),
            enter=True,
        )

        assert poll_until_file_has_content(exit_code_file, timeout=5.0), (
            env.capture_pane("test:")
        )
        stdout = stdout_file.read_text()
        stderr = stderr_file.read_text()

        assert int(exit_code_file.read_text()) == 0, stderr
        assert "Successfully created worktree" in stdout
        assert get_worktree_path(mux_repo_path, "topic").is_dir()
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}topic")

    def test_stdin_creates_multiple_worktrees(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that piping lines to stdin creates multiple worktrees.

        Stdin lines become the 'input' variable in foreach_vars, which is
        appended to the base branch name by the default branch template.
        """
        env = mux_server
        input_data = "feature-a\nfeature-b"

        write_workmux_config(mux_repo_path)

        # Pipe input - the default branch template appends foreach_vars (input) to base_name
        # So "topic" + "feature-a" -> "topic-feature-a"
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add topic",
            stdin_input=input_data,
        )

        # Verify all expected worktrees and windows exist
        for item in ["feature-a", "feature-b"]:
            expected_handle = slugify(f"topic-{item}")
            worktree_path = (
                mux_repo_path.parent
                / f"{mux_repo_path.name}__worktrees"
                / expected_handle
            )
            assert worktree_path.is_dir(), f"Expected worktree at {worktree_path}"

            expected_window = f"{DEFAULT_WINDOW_PREFIX}{expected_handle}"
            assert_window_exists(env, expected_window)

    def test_stdin_with_custom_branch_template(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that stdin input works with custom branch templates."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        # Use custom branch template that puts input first
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add base --branch-template '{{ input }}-feature'",
            stdin_input="api\nauth",
        )

        # Verify worktrees were created with custom template
        for item in ["api", "auth"]:
            expected_handle = slugify(f"{item}-feature")
            worktree_path = (
                mux_repo_path.parent
                / f"{mux_repo_path.name}__worktrees"
                / expected_handle
            )
            assert worktree_path.is_dir(), f"Expected worktree at {worktree_path}"
            assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}{expected_handle}")

    def test_stdin_conflicts_with_foreach_flag(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that using stdin and --foreach simultaneously fails."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-branch --foreach 'region:us,eu'",
            stdin_input="input-data",
            expect_fail=True,
        )

        assert "Cannot use --foreach when piping input from stdin" in result.stderr

    def test_stdin_conflicts_with_name_flag(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that using stdin and --name simultaneously fails (multi-worktree constraint)."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add my-branch --name custom-name",
            stdin_input="input-data",
            expect_fail=True,
        )

        assert "--name cannot be used with multi-worktree generation" in result.stderr

    def test_stdin_conflicts_with_prompt_editor_and_auto_name(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that stdin cannot be used with interactive prompt editor when using -A."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add -A --prompt-editor",
            stdin_input="input-data",
            expect_fail=True,
        )

        assert "Cannot use interactive prompt editor when piping input" in result.stderr

    def test_stdin_overrides_frontmatter_foreach(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that stdin input takes precedence over 'foreach' defined in prompt frontmatter."""
        env = mux_server

        # Create a prompt file with foreach in frontmatter
        # Use -p for inline prompt (empty) so we don't trigger agent requirement
        prompt_file = mux_repo_path / "prompt_with_matrix.md"
        prompt_file.write_text("""---
foreach:
  env: [dev, prod]
---
Task for {{ input }}
""")

        # Configure with an agent pane to satisfy the prompt requirement
        write_workmux_config(mux_repo_path, panes=[{"command": "<agent>"}])

        # Pipe 'api' via stdin. The prompt has 'foreach env', but stdin should override it
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add deploy -P prompt_with_matrix.md",
            stdin_input="api",
        )

        # Warning should be shown
        assert "stdin input overrides prompt frontmatter" in result.stderr

        # Should create 'deploy-api' (base_name + input), ignoring the 'dev/prod' matrix from frontmatter
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}deploy-api")

        # Ensure the frontmatter expansion did NOT happen
        window_list = env.list_windows()
        assert "deploy-dev" not in window_list
        assert "deploy-prod" not in window_list

    def test_empty_stdin_lines_are_filtered(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that empty lines in stdin are ignored."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        # Include empty lines and whitespace-only lines
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add task",
            stdin_input="first\n\n  \nsecond\n",
        )

        # Should only create two worktrees (empty lines filtered)
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}task-first")
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}task-second")

        # Verify no window for empty input
        window_list = env.list_windows()
        # Ensure we only have the expected windows plus the test session window
        window_names = [w for w in window_list if w.startswith(DEFAULT_WINDOW_PREFIX)]
        assert len(window_names) == 2

    def test_stdin_with_whitespace_trimmed(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that whitespace is trimmed from stdin lines."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        # Include lines with leading/trailing whitespace
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add item",
            stdin_input="  padded  \n",
        )

        # Should create worktree with trimmed name: item-padded
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}item-padded")

    def test_stdin_json_lines_expose_keys_as_variables(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that JSON lines are parsed and keys become template variables."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        # Pipe JSON lines - each key should become a template variable
        json_lines = '{"name":"workmux","id":"1"}\n{"name":"tmux-tools","id":"2"}'

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add analyze --branch-template '{{ base_name }}-{{ name }}'",
            stdin_input=json_lines,
        )

        # Verify worktrees created with names from JSON 'name' key
        for name in ["workmux", "tmux-tools"]:
            expected_handle = slugify(f"analyze-{name}")
            worktree_path = (
                mux_repo_path.parent
                / f"{mux_repo_path.name}__worktrees"
                / expected_handle
            )
            assert worktree_path.is_dir(), f"Expected worktree at {worktree_path}"
            assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}{expected_handle}")

    def test_stdin_json_lines_preserve_input_variable(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        mux_repo_path: Path,
    ):
        """Verifies that {{ input }} contains the raw JSON line."""
        env = mux_server

        write_workmux_config(mux_repo_path)

        # Use {{ input }} in template - should get the raw JSON string (slugified)
        json_line = '{"name":"test"}'

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add task --branch-template '{{ base_name }}-{{ index }}'",
            stdin_input=json_line,
        )

        # {{ index }} should be 1 for single item
        assert_window_exists(env, f"{DEFAULT_WINDOW_PREFIX}task-1")
