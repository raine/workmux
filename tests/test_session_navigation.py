"""Headless checks for deferred session cleanup scripts and lifecycle.

PTY-attached navigation coverage lives in test_popup_navigation.py and
test_session_relocation.py.
"""

import shlex
import shutil
import time
from pathlib import Path

import pytest

from .conftest import (
    FakeAgentInstaller,
    TmuxEnvironment,
    WorkmuxCommandResult,
    assert_session_exists,
    assert_session_not_exists,
    get_scripts_dir,
    get_session_name,
    get_worktree_path,
    poll_until,
    poll_until_file_has_content,
    run_workmux_command,
    write_workmux_config,
)
from .test_workmux_add.conftest import add_branch_and_get_worktree

# Session navigation is tmux-specific (other backends don't support sessions)
pytestmark = pytest.mark.tmux_only


def run_workmux_in_session(
    env: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    session_name: str,
    command: str,
    working_dir: Path | None = None,
    rust_log: str | None = None,
) -> WorkmuxCommandResult:
    """Run a workmux command from inside a specific tmux session.

    Unlike run_workmux_command which runs from the test session, this sends
    the command to a pane inside the target session. This allows testing
    behavior that depends on being inside the session (e.g.,
    is_inside_matching_target detection in cleanup.rs).

    The command is executed via a script file to avoid tmux send-keys line
    length limits. Exit code, stdout, and stderr are captured to files and
    returned after the command completes.

    When rust_log is set, XDG_STATE_HOME is redirected to a temp directory
    so the log file can be read back. workmux writes tracing output to a
    log file (not stderr), so the log content is appended to stderr in the
    returned result.

    The script ignores the pane's hangup signal so it can capture the command's
    exit code after a deferred cleanup kills the session.
    """
    scripts_dir = get_scripts_dir(env)
    stdout_file = scripts_dir / "in_session_stdout.txt"
    stderr_file = scripts_dir / "in_session_stderr.txt"
    exit_code_file = scripts_dir / "in_session_exit.txt"
    script_file = scripts_dir / "in_session_run.sh"
    state_dir = scripts_dir / "xdg_state"
    state_dir.mkdir(exist_ok=True)

    for f in [stdout_file, stderr_file, exit_code_file]:
        if f.exists():
            f.unlink()

    # Clear any previous log file so we only see output from this run
    log_file = state_dir / "workmux" / "workmux.log"
    if log_file.exists():
        log_file.unlink()

    workdir = working_dir or repo_path

    rust_log_line = ""
    if rust_log:
        rust_log_line = f"export RUST_LOG={shlex.quote(rust_log)}"

    script_content = f"""#!/bin/sh
export PATH={shlex.quote(env.env["PATH"])}
export TMPDIR={shlex.quote(env.env.get("TMPDIR", "/tmp"))}
export HOME={shlex.quote(env.env.get("HOME", ""))}
export XDG_STATE_HOME={shlex.quote(str(state_dir))}
export WORKMUX_TEST=1
{rust_log_line}
cd {shlex.quote(str(workdir))}
{shlex.quote(str(workmux_exe_path))} {command} > {shlex.quote(str(stdout_file))} 2> {shlex.quote(str(stderr_file))}
status=$?
echo "$status" > {shlex.quote(str(exit_code_file))}
exit "$status"
"""
    script_file.write_text(script_content)

    # Ignore the pane hangup so deferred session removal cannot interrupt capture.
    env.send_keys(
        f"={session_name}:",
        f"nohup /bin/sh {shlex.quote(str(script_file))} >/dev/null 2>&1 &",
    )

    # Wait for completion (longer timeout to account for deferred scripts)
    if not poll_until_file_has_content(exit_code_file, timeout=10.0):
        pane_content = (
            env.tmux(
                ["capture-pane", "-p", "-t", f"={session_name}:"], check=False
            ).stdout
            or "(session may have been killed)"
        )
        raise AssertionError(
            f"Command did not complete in time (session: {session_name})\n"
            f"Pane content:\n{pane_content}"
        )

    stderr_content = stderr_file.read_text() if stderr_file.exists() else ""

    # Append workmux log file content to stderr when RUST_LOG was set.
    # workmux writes tracing output to $XDG_STATE_HOME/workmux/workmux.log
    # (not stderr), so we read it back and include it in the result.
    if rust_log and log_file.exists():
        stderr_content += log_file.read_text()

    return WorkmuxCommandResult(
        exit_code=int(exit_code_file.read_text().strip()),
        stdout=stdout_file.read_text() if stdout_file.exists() else "",
        stderr=stderr_content,
    )


class TestRemoveFromInsideSession:
    """Session removal defers cleanup and lets tmux relocate attached clients."""

    def test_remove_uses_guarded_navigation_with_native_fallback(
        self, mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
    ):
        """Session closure changes only the source's destruction behavior."""
        env = mux_server
        branch_name = "feature-session-nav-switch"
        session_name = get_session_name(branch_name)

        write_workmux_config(repo_path)

        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--session --background",
        )
        assert_session_exists(env, session_name)

        # Run remove from INSIDE the session with debug logging to capture
        # the deferred script content
        result = run_workmux_in_session(
            env,
            workmux_exe_path,
            repo_path,
            session_name,
            "remove -f",
            working_dir=worktree_path,
            rust_log="workmux=debug",
        )

        assert result.exit_code == 0, f"Remove failed: {result.stderr}"

        assert "detach-on-destroy off" in result.stderr
        assert "if-shell -F" in result.stderr
        assert "switch-client -c" in result.stderr
        assert "switch-client -l" not in result.stderr

        # Wait for deferred script to complete (session killed, worktree removed)
        assert poll_until(lambda: not worktree_path.exists(), timeout=5.0), (
            "Worktree should be removed by deferred cleanup"
        )
        assert_session_not_exists(env, session_name)


class TestCreateAndSwitch:
    """Tests for `workmux add --session` without --background.

    When creating a session-mode worktree without --background, workmux
    should attempt to switch the client to the new session. In a headless
    test environment, switch-client will fail (no PTY-attached client),
    but we can verify the session is created regardless.

    Full switch verification requires manual testing with an attached
    terminal. The key behavior to verify headlessly is that the session
    is created even when the switch attempt fails.
    """

    def test_add_session_creates_session_despite_switch_failure(
        self, mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
    ):
        """Verify --session without --background creates the session.

        In a headless environment, the switch-client call fails because
        there's no attached client. The session should still be created
        and the worktree should still be set up correctly.
        """
        env = mux_server
        branch_name = "feature-session-no-bg"
        session_name = get_session_name(branch_name)
        worktree_path = get_worktree_path(repo_path, branch_name)

        write_workmux_config(repo_path)

        # Run without --background; may fail due to switch-client
        result = run_workmux_command(
            env,
            workmux_exe_path,
            repo_path,
            f"add {branch_name} --session",
            expect_fail=True,
        )

        # The session should be created even if the switch failed
        sessions_result = env.tmux(
            ["list-sessions", "-F", "#{session_name}"], check=False
        )
        existing_sessions = [s for s in sessions_result.stdout.strip().split("\n") if s]

        assert session_name in existing_sessions, (
            f"Session {session_name!r} should exist after 'add --session' "
            f"even when switch-client fails.\n"
            f"Existing sessions: {existing_sessions!r}\n"
            f"Command stderr: {result.stderr}"
        )

        # Worktree should also be created
        assert worktree_path.is_dir(), (
            f"Worktree at {worktree_path} should exist after 'add --session'"
        )


class TestStableSessionCleanupIdentity:
    def test_session_rename_does_not_release_deferred_cleanup(
        self,
        mux_server: TmuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        fake_agent_installer: FakeAgentInstaller,
    ):
        env = mux_server
        branch_name = "stable-session-id"
        session_name = get_session_name(branch_name)
        renamed_session = f"{session_name}-renamed"
        write_workmux_config(repo_path)
        worktree_path = add_branch_and_get_worktree(
            env,
            workmux_exe_path,
            repo_path,
            branch_name,
            extra_args="--session --background",
        )
        session_id = env.tmux(
            ["display-message", "-p", "-t", f"={session_name}:", "#{session_id}"]
        ).stdout.strip()
        assert session_id.startswith("$")

        real_tmux = shutil.which("tmux")
        assert real_tmux is not None
        fake_tmux = fake_agent_installer.install(
            "tmux",
            "#!/bin/sh\n"
            'for arg in "$@"; do\n'
            '  if [ "$arg" = kill-session ]; then exit 0; fi\n'
            "done\n"
            f'exec {shlex.quote(real_tmux)} "$@"\n',
        )

        scripts_dir = get_scripts_dir(env)
        output = scripts_dir / "stable-session-output"
        script = scripts_dir / "stable-session-remove.sh"
        script.write_text(
            "#!/bin/sh\n"
            f"export PATH={shlex.quote(env.env['PATH'])}\n"
            f"export HOME={shlex.quote(env.env['HOME'])}\n"
            f"export XDG_STATE_HOME={shlex.quote(env.env['XDG_STATE_HOME'])}\n"
            f"cd {shlex.quote(str(worktree_path))}\n"
            f"exec {shlex.quote(str(workmux_exe_path))} remove --force "
            f">{shlex.quote(str(output))} 2>&1\n"
        )
        env.send_keys(f"={session_name}:", f"/bin/sh {shlex.quote(str(script))}")
        assert poll_until(
            lambda: output.exists() and "Scheduled removal" in output.read_text()
        )
        env.tmux(["rename-session", "-t", session_id, renamed_session])

        time.sleep(0.8)
        live_ids = env.tmux(
            ["list-sessions", "-F", "#{session_id}"]
        ).stdout.splitlines()
        assert session_id in live_ids
        assert worktree_path.is_dir()

        env.run_command(
            [real_tmux, "-S", str(env.socket_path), "kill-session", "-t", session_id]
        )
        assert poll_until(lambda: not worktree_path.exists(), timeout=10.0)
        fake_tmux.unlink()
