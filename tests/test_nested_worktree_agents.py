"""Agent ownership must exclude agents in nested worktrees."""

import json
import shlex
import shutil
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
    mark_agent_state,
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


@pytest.fixture
def nested_agent(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
) -> str:
    """Start a tracked agent in a worktree inside the main checkout."""
    runner_window = mux_server.tmux(
        ["display-message", "-p", "#{window_id}"]
    ).stdout.strip()
    write_workmux_config(mux_repo_path, panes=[{"focus": True}])
    with (mux_repo_path / ".workmux.yaml").open("a") as config:
        config.write("\nworktree_dir: worktrees\n")
    branch = "nested-feature"
    run_workmux_add(mux_server, workmux_exe_path, mux_repo_path, branch)
    assert (mux_repo_path / "worktrees" / branch / ".git").is_file()
    window = get_window_name(branch)
    wait_for_window_ready(mux_server, window)
    mark_agent_state(mux_server, workmux_exe_path, window, "done")
    stabilize_tmux_pane(mux_server, window)
    mux_server.select_window(runner_window)
    return branch


@pytest.mark.tmux_only
@pytest.mark.parametrize("filtered", [False, True])
def test_list_main_excludes_nested_agent(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    nested_agent: str,
    filtered: bool,
):
    command = "list --json"
    if filtered:
        command += f" {mux_repo_path.name}"
    result = run_workmux_command(mux_server, workmux_exe_path, mux_repo_path, command)
    rows = json.loads(result.stdout)
    main = next(row for row in rows if row["is_main"])
    assert main["agent_statuses"] == []
    if not filtered:
        nested = next(row for row in rows if row["branch"] == nested_agent)
        assert nested["agent_statuses"] == ["done"]


@pytest.mark.tmux_only
@pytest.mark.parametrize("targeted", [False, True])
def test_status_main_excludes_nested_agent(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    nested_agent: str,
    targeted: bool,
):
    command = "status --json"
    if targeted:
        command += f" {mux_repo_path.name}"
    result = run_workmux_command(mux_server, workmux_exe_path, mux_repo_path, command)
    agents = json.loads(result.stdout)["agents"]
    assert [agent["worktree"] for agent in agents] == (
        [] if targeted else [nested_agent]
    )


@pytest.mark.tmux_only
def test_wait_main_does_not_complete_for_nested_agent(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    nested_agent: str,
):
    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        f"wait {mux_repo_path.name} --status done --timeout 1",
        expect_fail=True,
    )
    assert result.exit_code == 1
    assert "Timeout waiting for:" in result.stderr


@pytest.mark.tmux_only
def test_resurrect_restores_nested_agent_after_server_restart(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    nested_agent: str,
):
    mux_server.stop_server()
    mux_server.start_server()
    result = run_workmux_command(
        mux_server, workmux_exe_path, mux_repo_path, "resurrect --dry-run"
    )
    assert result.exit_code == 0
    assert f"{nested_agent:<20} -> restoring" in result.stdout
    assert "Dry run: would restore 1 worktree(s)" in result.stdout


@pytest.mark.tmux_only
@pytest.mark.parametrize("any_target", [False, True])
def test_wait_preserves_missing_worktree_lifecycle(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    nested_agent: str,
    any_target: bool,
):
    gone = mux_repo_path / "worktrees" / "gone"
    mux_server.run_command(
        ["git", "worktree", "add", "-b", "gone", str(gone)], cwd=mux_repo_path
    )
    shutil.rmtree(gone)
    registered = mux_server.run_command(
        ["git", "worktree", "list", "--porcelain"], cwd=mux_repo_path
    ).stdout
    assert "branch refs/heads/gone" in registered
    assert not gone.exists()

    targets = f"{nested_agent} gone --any" if any_target else "gone"
    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        f"wait {targets} --status done --timeout 1",
        expect_fail=not any_target,
    )
    if any_target:
        assert result.exit_code == 0, result.stderr
    else:
        assert result.exit_code == 1
        assert "Timeout waiting for: gone" in result.stderr
