"""Returning the client to a configured session when a workmux session closes.

Session mode sends the client to whichever session it was in previously once
the workmux session it displays is destroyed. `default_session` names a session
to prefer instead, so closing the worktree you are working in puts you back
where you started rather than wherever you happened to be last.

These use a real PTY-attached client so the assertions are about where the
client actually lands, not about the commands workmux generates.
"""

from pathlib import Path

import pytest

from .conftest import (
    TmuxEnvironment,
    get_session_name,
    poll_until,
    write_workmux_config,
)
from .test_popup_navigation import attached_client
from .test_workmux_add.conftest import add_branch_and_get_worktree

pytestmark = pytest.mark.tmux_only

HOME_SESSION = "home-base"


def client_session(env: TmuxEnvironment, client: str) -> str | None:
    """The session a given client currently displays."""
    for line in env.tmux(
        ["list-clients", "-F", "#{client_name}\t#{session_name}"], check=False
    ).stdout.splitlines():
        name, _, session = line.partition("\t")
        if name == client:
            return session
    return None


def session_gone(env: TmuxEnvironment, session: str) -> bool:
    """Whether the named session has been destroyed."""
    return env.tmux(["has-session", "-t", f"={session}"], check=False).returncode != 0


def prepare(
    env: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch: str,
    default_session: str | None,
) -> tuple[Path, str]:
    """Create a session-mode worktree, with `default_session` configured."""
    write_workmux_config(
        repo_path, panes=[{"command": "/bin/sh"}], default_session=default_session
    )
    worktree = add_branch_and_get_worktree(
        env, workmux_exe_path, repo_path, branch, extra_args="--session --background"
    )
    return worktree, get_session_name(branch)


def run_in_session(env: TmuxEnvironment, session: str, command: str) -> None:
    env.send_keys(f"={session}:", command)


@pytest.mark.parametrize("operation", ["remove", "close"])
def test_client_returns_to_configured_session(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    operation: str,
):
    """A client viewing the closing session lands on `default_session`."""
    env = mux_server
    env.tmux(["new-session", "-d", "-s", HOME_SESSION])
    worktree, source = prepare(
        env, workmux_exe_path, repo_path, f"cfg-{operation}", HOME_SESSION
    )
    command = f"cd {worktree}; {workmux_exe_path} {operation}"
    if operation == "remove":
        command += " -f"

    with attached_client(env) as (_master, client):
        env.tmux(["switch-client", "-c", client, "-t", f"={source}:"])
        assert client_session(env, client) == source
        run_in_session(env, source, command)

        assert poll_until(lambda: session_gone(env, source), timeout=10), (
            "source session should be destroyed"
        )
        assert poll_until(
            lambda: client_session(env, client) == HOME_SESSION, timeout=10
        ), f"client should return to {HOME_SESSION}, got {client_session(env, client)}"


def test_unknown_configured_session_falls_back_to_previous(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """A configured session that does not exist leaves the client in tmux."""
    env = mux_server
    worktree, source = prepare(
        env, workmux_exe_path, repo_path, "cfg-missing", "nowhere-at-all"
    )

    with attached_client(env) as (_master, client):
        assert client_session(env, client) == "test"
        env.tmux(["switch-client", "-c", client, "-t", f"={source}:"])
        run_in_session(env, source, f"cd {worktree}; {workmux_exe_path} remove -f")

        assert poll_until(lambda: session_gone(env, source), timeout=10)
        assert poll_until(lambda: client_session(env, client) == "test", timeout=10), (
            f"client should fall back to its previous session, got "
            f"{client_session(env, client)}"
        )


def test_managed_target_wins_over_configured_session(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    """A workmux session for the destination branch takes precedence."""
    env = mux_server
    env.tmux(["new-session", "-d", "-s", HOME_SESSION])
    managed_main = get_session_name("main")
    env.tmux(["new-session", "-d", "-s", managed_main])
    worktree, source = prepare(
        env, workmux_exe_path, repo_path, "cfg-precedence", HOME_SESSION
    )

    with attached_client(env) as (_master, client):
        env.tmux(["switch-client", "-c", client, "-t", f"={source}:"])
        run_in_session(env, source, f"cd {worktree}; {workmux_exe_path} remove -f")

        assert poll_until(lambda: session_gone(env, source), timeout=10)
        assert poll_until(
            lambda: client_session(env, client) == managed_main, timeout=10
        ), (
            f"managed session {managed_main} should win over {HOME_SESSION}, got "
            f"{client_session(env, client)}"
        )
