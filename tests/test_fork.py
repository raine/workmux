"""Tests for `workmux add --fork` conversation forking."""

import json
import shlex
import time
from pathlib import Path

import pytest


from .conftest import (
    MuxEnvironment,
    get_worktree_path,
    poll_until,
    run_workmux_command,
    write_workmux_config,
)


def create_fake_claude_session(
    claude_config_dir: Path,
    worktree_path: Path,
    session_id: str = "test-session-abc123",
    content: str = '{"type":"message"}',
    with_subdir: bool = False,
) -> Path:
    """Create a fake Claude conversation file for testing.

    Returns the path to the created .jsonl file.
    """
    # Encode path the same way Claude does: non-alphanumeric (except -) become -
    encoded = "".join(c if c.isalnum() or c == "-" else "-" for c in str(worktree_path))
    project_dir = claude_config_dir / "projects" / encoded
    project_dir.mkdir(parents=True, exist_ok=True)

    jsonl_path = project_dir / f"{session_id}.jsonl"
    jsonl_path.write_text(content)

    if with_subdir:
        subdir = project_dir / session_id
        subdir.mkdir(exist_ok=True)
        (subdir / "data.json").write_text("{}")

    return jsonl_path


def run_fork_command(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    command: str,
    claude_dir: Path,
    expect_fail: bool = False,
):
    """Run a workmux add command with CLAUDE_CONFIG_DIR set."""
    return run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"add {command}",
        expect_fail=expect_fail,
        pre_run_env={"CLAUDE_CONFIG_DIR": str(claude_dir)},
    )


class TestForkBasic:
    """Tests for --fork flag with workmux add."""

    def test_fork_no_conversations_errors(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path, tmp_path
    ):
        """--fork with no conversations in current worktree should fail."""
        env = mux_server
        write_workmux_config(mux_repo_path)

        claude_dir = tmp_path / "claude-empty"
        claude_dir.mkdir()

        result = run_fork_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "fork-test --fork",
            claude_dir,
            expect_fail=True,
        )
        assert "No conversations found" in result.stderr

    def test_fork_copies_conversation(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path, tmp_path
    ):
        """--fork should copy conversation files into the new worktree's project dir."""
        env = mux_server
        write_workmux_config(mux_repo_path)

        claude_dir = tmp_path / "claude"
        session_id = "session-fork-test"

        create_fake_claude_session(
            claude_dir, mux_repo_path, session_id=session_id, with_subdir=True
        )

        run_fork_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "fork-branch --fork",
            claude_dir,
        )

        # Verify worktree was created
        worktree_path = get_worktree_path(mux_repo_path, "fork-branch")
        assert worktree_path.is_dir()

        # Verify conversation was copied to the new worktree's project dir
        encoded_target = "".join(
            c if c.isalnum() or c == "-" else "-" for c in str(worktree_path)
        )
        target_project_dir = claude_dir / "projects" / encoded_target
        assert (target_project_dir / f"{session_id}.jsonl").exists()
        assert (target_project_dir / session_id / "data.json").exists()

    def test_fork_specific_session(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path, tmp_path
    ):
        """--fork=<session-id> should fork a specific conversation."""
        env = mux_server
        write_workmux_config(mux_repo_path)

        claude_dir = tmp_path / "claude"

        # Create two sessions with different mtimes
        create_fake_claude_session(claude_dir, mux_repo_path, session_id="old-session")
        time.sleep(0.1)
        create_fake_claude_session(
            claude_dir, mux_repo_path, session_id="specific-session"
        )

        run_fork_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "fork-specific --fork=specific-session",
            claude_dir,
        )

        worktree_path = get_worktree_path(mux_repo_path, "fork-specific")
        encoded_target = "".join(
            c if c.isalnum() or c == "-" else "-" for c in str(worktree_path)
        )
        target_project_dir = claude_dir / "projects" / encoded_target

        # Only the specific session should be copied
        assert (target_project_dir / "specific-session.jsonl").exists()
        assert not (target_project_dir / "old-session.jsonl").exists()

    def test_fork_unknown_session_errors(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path, tmp_path
    ):
        """--fork=<nonexistent> should fail with clear error."""
        env = mux_server
        write_workmux_config(mux_repo_path)

        claude_dir = tmp_path / "claude"
        create_fake_claude_session(
            claude_dir, mux_repo_path, session_id="existing-session"
        )

        result = run_fork_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "fork-missing --fork=nonexistent",
            claude_dir,
            expect_fail=True,
        )
        assert "No conversation matching 'nonexistent'" in result.stderr

    def test_fork_prefix_match(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path, tmp_path
    ):
        """--fork=<prefix> should match session by prefix."""
        env = mux_server
        write_workmux_config(mux_repo_path)

        claude_dir = tmp_path / "claude"
        create_fake_claude_session(
            claude_dir, mux_repo_path, session_id="abc123-def456-full-uuid"
        )

        run_fork_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "fork-prefix --fork=abc123",
            claude_dir,
        )

        worktree_path = get_worktree_path(mux_repo_path, "fork-prefix")
        encoded_target = "".join(
            c if c.isalnum() or c == "-" else "-" for c in str(worktree_path)
        )
        target_project_dir = claude_dir / "projects" / encoded_target
        assert (target_project_dir / "abc123-def456-full-uuid.jsonl").exists()


def create_fake_codex_session(
    codex_home: Path,
    worktree_path: Path,
    session_id: str,
    day: str = "2026-08-12",
    source: str = "cli",
) -> Path:
    """Create a fake Codex rollout file for testing.

    Returns the path to the created .jsonl file.
    """
    year, month, day_of_month = day.split("-")
    day_dir = codex_home / "sessions" / year / month / day_of_month
    day_dir.mkdir(parents=True, exist_ok=True)

    rollout_path = day_dir / f"rollout-{day}T00-00-00-{session_id}.jsonl"
    meta = {
        "timestamp": f"{day}T00:00:00.000Z",
        "type": "session_meta",
        "payload": {
            "id": session_id,
            "session_id": session_id,
            "timestamp": f"{day}T00:00:00Z",
            "cwd": str(worktree_path),
            "originator": "codex",
            "cli_version": "0.147.0",
            "source": source,
        },
    }
    rollout_path.write_text(json.dumps(meta) + "\n")
    return rollout_path


def write_codex_shim(env: MuxEnvironment, bin_dir: Path) -> tuple[Path, Path]:
    """Create a `codex` stand-in that records the arguments it was launched with.

    Returns the shim path and the file it writes its arguments to.
    """
    bin_dir.mkdir(parents=True, exist_ok=True)
    argv_file = bin_dir / "codex-argv.txt"
    shim = bin_dir / "codex"
    env.install_script(shim, f'#!/bin/sh\nprintf "%s\\n" "$@" > {argv_file}\n')
    return shim, argv_file


def read_codex_argv(argv_file: Path, timeout: float = 10.0) -> list[str]:
    """Wait for the codex shim to record its arguments and return them."""
    assert poll_until(lambda: argv_file.exists(), timeout=timeout), (
        f"codex shim never ran (expected {argv_file})"
    )
    return argv_file.read_text().split()


class TestForkCodex:
    """Tests for --fork with Codex, which forks conversations natively."""

    def test_fork_launches_codex_fork_with_session_id(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path, tmp_path
    ):
        """--fork should launch `codex fork` pinned to the new worktree."""
        env = mux_server
        codex_home = tmp_path / "codex"
        shim, argv_file = write_codex_shim(mux_server, tmp_path / "bin")
        write_workmux_config(
            mux_repo_path, panes=[{"command": "<agent>"}], agent=str(shim)
        )

        session_id = "019ff7c5-a9d3-77b3-8cab-253e05f6f729"
        rollout = create_fake_codex_session(codex_home, mux_repo_path, session_id)
        original = rollout.read_text()

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add codex-fork-branch --fork",
            pre_run_env={"CODEX_HOME": str(codex_home)},
        )

        worktree_path = get_worktree_path(mux_repo_path, "codex-fork-branch")
        assert worktree_path.is_dir()
        # `-C` keeps the fork in the new worktree. Without it Codex asks which
        # directory to use and defaults to the one the parent session recorded.
        assert read_codex_argv(argv_file) == [
            "fork",
            "-C",
            str(worktree_path),
            session_id,
        ]
        # Codex writes the forked conversation itself, so the parent is untouched.
        assert rollout.read_text() == original

    def test_fork_specific_session(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path, tmp_path
    ):
        """--fork=<prefix> should select the matching Codex session."""
        env = mux_server
        codex_home = tmp_path / "codex"
        shim, argv_file = write_codex_shim(mux_server, tmp_path / "bin")
        write_workmux_config(
            mux_repo_path, panes=[{"command": "<agent>"}], agent=str(shim)
        )

        create_fake_codex_session(
            codex_home,
            mux_repo_path,
            "019ff7c5-1111-1111-1111-111111111111",
            day="2026-08-11",
        )
        time.sleep(0.1)
        create_fake_codex_session(
            codex_home,
            mux_repo_path,
            "019aaaaa-2222-2222-2222-222222222222",
            day="2026-08-12",
        )

        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add codex-fork-specific --fork=019ff7c5",
            pre_run_env={"CODEX_HOME": str(codex_home)},
        )

        worktree_path = get_worktree_path(mux_repo_path, "codex-fork-specific")
        assert read_codex_argv(argv_file) == [
            "fork",
            "-C",
            str(worktree_path),
            "019ff7c5-1111-1111-1111-111111111111",
        ]

    def test_fork_no_conversations_errors(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path, tmp_path
    ):
        """--fork with no Codex conversations should fail."""
        env = mux_server
        codex_home = tmp_path / "codex-empty"
        codex_home.mkdir()
        write_workmux_config(mux_repo_path, agent="codex")

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add codex-fork-empty --fork",
            expect_fail=True,
            pre_run_env={"CODEX_HOME": str(codex_home)},
        )
        assert "No conversations found" in result.stderr

    def test_fork_ignores_sessions_from_other_directories(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path, tmp_path
    ):
        """Codex sessions recorded elsewhere are not fork candidates."""
        env = mux_server
        codex_home = tmp_path / "codex"
        other_worktree = tmp_path / "other-project"
        other_worktree.mkdir()
        write_workmux_config(mux_repo_path, agent="codex")

        create_fake_codex_session(
            codex_home, other_worktree, "019ff7c5-3333-3333-3333-333333333333"
        )

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add codex-fork-other --fork",
            expect_fail=True,
            pre_run_env={"CODEX_HOME": str(codex_home)},
        )
        assert "No conversations found" in result.stderr

    def test_fork_ignores_non_interactive_sessions(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path, tmp_path
    ):
        """`codex exec` sessions cannot be forked, so they are skipped."""
        env = mux_server
        codex_home = tmp_path / "codex"
        write_workmux_config(mux_repo_path, agent="codex")

        create_fake_codex_session(
            codex_home,
            mux_repo_path,
            "019ff7c5-4444-4444-4444-444444444444",
            source="exec",
        )

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add codex-fork-exec --fork",
            expect_fail=True,
            pre_run_env={"CODEX_HOME": str(codex_home)},
        )
        assert "No conversations found" in result.stderr

    def test_fork_unknown_session_errors(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path, tmp_path
    ):
        """--fork=<nonexistent> should fail with a clear error."""
        env = mux_server
        codex_home = tmp_path / "codex"
        write_workmux_config(mux_repo_path, agent="codex")

        create_fake_codex_session(
            codex_home, mux_repo_path, "019ff7c5-5555-5555-5555-555555555555"
        )

        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            "add codex-fork-missing --fork=deadbeef",
            expect_fail=True,
            pre_run_env={"CODEX_HOME": str(codex_home)},
        )
        assert "No conversation matching 'deadbeef'" in result.stderr


def create_fake_pi_session(session_dir: Path, cwd: Path, session_id: str) -> Path:
    """Write a Pi session header and a synthetic conversation entry."""
    session_dir.mkdir(parents=True, exist_ok=True)
    path = session_dir / f"2026-01-01T00-00-00-000Z_{session_id}.jsonl"
    header = {
        "type": "session",
        "version": 3,
        "id": session_id,
        "timestamp": "2026-01-01T00:00:00.000Z",
        "cwd": str(cwd),
    }
    path.write_text(json.dumps(header) + '\n{"type":"message"}\n')
    return path


class TestForkPi:
    """Pi receives a source path and owns creation of the forked session."""

    @pytest.mark.parametrize("fork_flag", ["--fork", "--fork=older"])
    @pytest.mark.parametrize("custom_store", [False, True])
    def test_launches_native_fork(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path,
        mux_repo_path,
        tmp_path,
        fork_flag,
        custom_store,
    ):
        env = mux_server
        agent_dir = tmp_path / "pi agent's data"
        session_dir = (
            tmp_path / "custom sessions"
            if custom_store
            else agent_dir / "sessions" / "--project--"
        )
        older = create_fake_pi_session(session_dir, mux_repo_path, "older-session")
        time.sleep(0.1)
        latest = create_fake_pi_session(session_dir, mux_repo_path, "latest-session")
        # Shared stores must not select a newer conversation from another project.
        create_fake_pi_session(session_dir, tmp_path / "other", "other-session")
        selected = older if "=" in fork_flag else latest
        original = selected.read_bytes()
        argv_file = tmp_path / "pi-argv.txt"
        cwd_file = tmp_path / "pi-cwd.txt"
        shim = tmp_path / "bin" / "pi"
        shim.parent.mkdir()
        env.install_script(
            shim,
            "#!/usr/bin/env bash\nset -euo pipefail\n"
            f'printf "%s\\n" "$@" > {shlex.quote(str(argv_file))}\n'
            f"pwd -P > {shlex.quote(str(cwd_file))}\n",
        )
        write_workmux_config(
            mux_repo_path, panes=[{"command": "<agent>"}], agent=str(shim)
        )
        run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"add pi-fork {fork_flag}",
            pre_run_env={
                "PI_CODING_AGENT_DIR": str(agent_dir),
                "PI_CODING_AGENT_SESSION_DIR": str(session_dir) if custom_store else "",
            },
        )
        assert poll_until(lambda: cwd_file.exists(), timeout=10)
        assert argv_file.read_text().splitlines() == ["--fork", str(selected.resolve())]
        target = get_worktree_path(mux_repo_path, "pi-fork")
        assert Path(cwd_file.read_text().strip()) == target.resolve()
        assert selected.read_bytes() == original
        assert len(list(agent_dir.rglob("*.jsonl"))) == (0 if custom_store else 3)

    @pytest.mark.parametrize("fork_flag", ["--fork", "--fork=missing"])
    def test_missing_session_errors(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path,
        mux_repo_path,
        tmp_path,
        fork_flag,
    ):
        write_workmux_config(mux_repo_path, agent="pi")
        result = run_workmux_command(
            mux_server,
            workmux_exe_path,
            mux_repo_path,
            f"add pi-missing {fork_flag}",
            expect_fail=True,
            pre_run_env={
                "PI_CODING_AGENT_DIR": str(tmp_path / "empty"),
                "PI_CODING_AGENT_SESSION_DIR": "",
            },
        )
        expected = (
            "No conversation matching" if "=" in fork_flag else "No conversations found"
        )
        assert expected in result.stderr
        assert not get_worktree_path(mux_repo_path, "pi-missing").exists()
