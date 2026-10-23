"""Origin-based prefixes for existing worktrees."""

import json
from pathlib import Path

import pytest
import yaml

from .conftest import (
    TmuxEnvironment,
    assert_session_exists,
    assert_session_not_exists,
    run_workmux_command,
    write_global_workmux_config,
    write_workmux_config,
)
from .test_popup_navigation import attached_client

pytestmark = pytest.mark.tmux_only


@pytest.mark.parametrize("bare_layout", [False, True])
def test_open_reuses_origin_named_session(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    bare_layout: bool,
):
    env = mux_server
    main = anchor = repo_path
    if bare_layout:
        project = env.tmp_path / "local-project"
        project.mkdir()
        anchor = project / ".bare"
        main = project / "main"
        env.run_command(["git", "clone", "--bare", str(repo_path), str(anchor)])
        (project / ".git").write_text("gitdir: ./.bare\n")
        env.run_command(["git", "worktree", "add", str(main), "main"], cwd=anchor)
    env.run_command(
        ["git", "config", "remote.origin.url", "git@host:group/example-repo.git"],
        cwd=anchor,
    )
    feature = env.tmp_path / "feature"
    env.run_command(
        ["git", "worktree", "add", "-b", "feature", str(feature), "main"],
        cwd=anchor,
    )
    config = write_global_workmux_config(env)
    config.write_text(
        yaml.safe_dump(
            {
                "mode": "session",
                "window_prefix": "{origin_name}-",
                "main_branch": "main",
                "nerdfont": False,
                "panes": [{"command": "/bin/sh"}],
            }
        )
    )
    expected = f"example-repo-{main.name}"
    with attached_client(env) as (_, client):
        for attempt, cwd in enumerate([main, anchor, feature]):
            env.tmux(["switch-client", "-c", client, "-t", "=test"])
            result = run_workmux_command(env, workmux_exe_path, cwd, "open main")
            if attempt:
                assert "Switched to existing" in result.stdout
            assert_session_exists(env, expected)
            sessions = env.tmux(["list-sessions", "-F", "#{session_name}"]).stdout
            assert sessions.splitlines().count(expected) == 1
        listing = run_workmux_command(
            env, workmux_exe_path, feature, "list --json main"
        )
        assert json.loads(listing.stdout)[0]["is_open"] is True
        run_workmux_command(env, workmux_exe_path, feature, "close main")
        assert_session_not_exists(env, expected)
    assert main.is_dir()
    assert feature.is_dir()


def test_origin_prefix_in_window_mode(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, repo_path: Path
):
    env = mux_server
    env.run_command(
        ["git", "config", "remote.origin.url", "https://host/group/example-repo.git"],
        cwd=repo_path,
    )
    write_workmux_config(
        repo_path, window_prefix="{origin_name}-", panes=[{"command": "/bin/sh"}]
    )
    run_workmux_command(env, workmux_exe_path, repo_path, "open main --mode window")
    assert "example-repo-" + repo_path.name in env.list_windows()
