"""Agent ownership must exclude agents in nested worktrees."""

import json
import shlex
from pathlib import Path

import pytest

from .conftest import (
    TmuxEnvironment,
    get_window_name,
    poll_until,
    run_workmux_add,
    run_workmux_command,
    wait_for_window_ready,
    write_workmux_config,
)
from .support.agent_state import (
    build_status_cmd_with_marker,
    stabilize_tmux_pane,
)


@pytest.mark.tmux_only
@pytest.mark.parametrize("main_agent", [False, True])
def test_nested_worktree_agent_ownership(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    main_agent: bool,
):
    env = mux_server
    repo = mux_repo_path
    runner = env.tmux(["display-message", "-p", "#{window_id}"]).stdout.strip()
    write_workmux_config(repo, panes=[{"focus": True}])
    config = repo / ".workmux.yaml"
    config.write_text(config.read_text() + "\nworktree_dir: worktrees\n")
    run_workmux_add(env, workmux_exe_path, repo, "feature-a")
    nested_window = get_window_name("feature-a")
    wait_for_window_ready(env, nested_window)

    def register(window: str, directory: Path, status: str) -> str:
        pane = env.tmux(
            ["display-message", "-p", "-t", window, "#{pane_id}"]
        ).stdout.strip()
        marker = env.tmp_path / f"registered-{pane}"
        env.send_keys(window, f"cd {shlex.quote(str(directory))}")
        env.send_keys(
            window,
            build_status_cmd_with_marker(env, workmux_exe_path, status, marker),
        )
        assert poll_until(marker.exists, timeout=5)
        stabilize_tmux_pane(env, window)
        env.select_window(runner)
        return pane

    subdir = repo / "worktrees/feature-a/src"
    subdir.mkdir()
    nested_pane = register(nested_window, subdir, "working")
    main_pane = None
    if main_agent:
        env.tmux(["new-window", "-d", "-n", "main-agent", "-c", str(repo)])
        wait_for_window_ready(env, "main-agent")
        main_pane = register("main-agent", repo, "waiting")

    def command(args: str, *, fail: bool = False):
        return run_workmux_command(env, workmux_exe_path, repo, args, expect_fail=fail)

    rows = json.loads(command("list --json").stdout)
    by_branch = {row["branch"]: row for row in rows}
    assert by_branch["feature-a"]["agent_statuses"] == ["working"]
    expected_main = ["waiting"] if main_agent else []
    assert by_branch["main"]["agent_statuses"] == expected_main
    filtered = json.loads(command("list main --json").stdout)
    assert len(filtered) == 1
    assert filtered[0]["agent_statuses"] == expected_main

    status = json.loads(command("status --json").stdout)
    assert len(status["agents"]) == status["reconciled_agent_count"] == 1 + main_agent
    assert [a["pane_id"] for a in status["agents"] if a["branch"] == "feature-a"] == [
        nested_pane
    ]
    main_status = json.loads(command("status main --json").stdout)
    assert [a["pane_id"] for a in main_status["agents"]] == (
        [main_pane] if main_agent else []
    )
    capture = command("capture main", fail=not main_agent)
    if not main_agent:
        assert "No agent running" in capture.stderr
    result = command("wait main --status working --timeout 1", fail=True)
    assert result.exit_code == 1
    assert "Timeout" in result.stderr
    command("wait feature-a --status working --timeout 1")

    env.stop_server()
    env.start_server()
    result = command("resurrect --dry-run")
    assert "feature-a" in result.stdout
    assert "-> restoring" in result.stdout
    assert "would restore 1 worktree(s)" in result.stdout
    if main_agent:
        assert "skipping (main worktree)" in result.stdout
