"""
Tests for `workmux status` command.

Tests output format, JSON mode, filtering, and behavior with real agent state.
"""

import json
import subprocess
from pathlib import Path
from typing import cast

import pytest

from .conftest import (
    MuxEnvironment,
    TmuxEnvironment,
    get_window_name,
    get_worktree_path,
    poll_until,
    run_workmux_add,
    run_workmux_command,
    setup_git_repo,
    wait_for_window_ready,
    write_workmux_config,
)
from .support.agent_state import (
    build_status_cmd_with_marker,
    get_agents_dir,
    list_agent_state_files,
    stabilize_tmux_agent,
    start_active_agent,
)


def test_status_no_agents(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status shows 'No active agents' when no agents are running."""
    result = run_workmux_command(mux_server, workmux_exe_path, mux_repo_path, "status")
    assert "No active agents" in result.stdout


def test_status_json_no_agents(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status --json reports an empty tracked-agent observation."""
    result = run_workmux_command(
        mux_server, workmux_exe_path, mux_repo_path, "status --json"
    )
    parsed = json.loads(result.stdout)
    assert parsed["agents"] == []
    assert parsed["state_files_total"] == 0
    assert parsed["state_files_invalid"] == 0
    assert parsed["state_files_invalid_unattributed"] == 0
    assert parsed["state_files_invalid_matching_context"] == 0
    assert parsed["state_files_matching_context"] == 0
    assert parsed["reconciled_agent_count"] == 0
    assert parsed["target_errors"] == []
    assert parsed["context"]["backend"] == mux_server.backend_name


def test_status_json_git_skips_enrichment_without_matching_agents(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Empty observations do not require repository history for Git enrichment."""
    empty_repo = mux_repo_path.with_name(f"{mux_repo_path.name}-empty")
    empty_repo.mkdir()
    subprocess.run(["git", "init", "-q", str(empty_repo)], check=True)

    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "status --json --git",
        working_dir=empty_repo,
    )
    parsed = json.loads(result.stdout)

    assert parsed["agents"] == []
    assert parsed["scope"]["repository"] == str(empty_repo)


def test_status_json_reports_context_mismatch(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status exposes state that belongs to another multiplexer instance."""
    env = mux_server
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-other-instance",
        status="working",
    )
    state_file = list_agent_state_files(env)[0]
    state = json.loads(state_file.read_text())
    state["pane_key"]["instance"] = "different-instance"
    state_file.write_text(json.dumps(state))

    result = run_workmux_command(env, workmux_exe_path, mux_repo_path, "status --json")
    parsed = json.loads(result.stdout)

    assert parsed["state_files_total"] == 1
    assert parsed["state_files_matching_context"] == 0
    assert parsed["reconciled_agent_count"] == 0
    assert parsed["agents"] == []


def test_status_json_does_not_delete_stale_state(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Observation excludes stale state without mutating the state store."""
    env = mux_server
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-read-only",
        status="working",
    )
    state_file = list_agent_state_files(env)[0]
    state = json.loads(state_file.read_text())
    state["pane_pid"] += 1
    state_file.write_text(json.dumps(state))

    result = run_workmux_command(env, workmux_exe_path, mux_repo_path, "status --json")
    parsed = json.loads(result.stdout)

    assert parsed["state_files_matching_context"] == 1
    assert parsed["reconciled_agent_count"] == 0
    assert state_file.exists()


def test_status_json_reports_agents_outside_repository_scope(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Reconciled count exposes agents omitted by repository scoping."""
    env = mux_server
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-other-repo",
        status="working",
    )
    other_repo = env.tmp_path / "other-repo"
    other_repo.mkdir()
    subprocess.run(["git", "init", "-q", str(other_repo)], check=True)

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "status --json",
        working_dir=other_repo,
    )
    parsed = json.loads(result.stdout)

    assert parsed["scope"]["repository"] == str(other_repo)
    assert parsed["reconciled_agent_count"] == 1
    assert parsed["agents"] == []


def test_status_json_outside_repository_reports_all_reconciled_agents(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """A fleet query outside Git keeps the multiplexer observation available."""
    env = mux_server
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-no-repo",
        status="working",
    )
    outside_repo = mux_repo_path.with_name(f"{mux_repo_path.name}-outside")
    outside_repo.mkdir()

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "status --json",
        working_dir=outside_repo,
    )
    parsed = json.loads(result.stdout)

    assert parsed["scope"]["repository"] is None
    assert parsed["scope"]["all"] is False
    assert parsed["reconciled_agent_count"] == 1
    assert len(parsed["agents"]) == 1


@pytest.mark.tmux_only
def test_status_all_json_reports_agents_across_repositories(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status --all bypasses the current repository scope."""
    env = cast(TmuxEnvironment, mux_server)
    runner_window = env.tmux(["display-message", "-p", "#{window_id}"]).stdout.strip()
    first = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-all-first",
        status="working",
    )
    stabilize_tmux_agent(env, first)
    env.select_window(runner_window)
    second_repo = env.tmp_path.parent / "status-all-second-repo"
    second_repo.mkdir()
    setup_git_repo(second_repo, env.env)
    second = start_active_agent(
        env,
        workmux_exe_path,
        second_repo,
        "feature-status-all-second",
        status="waiting",
    )
    stabilize_tmux_agent(env, second)
    env.select_window(runner_window)

    result = run_workmux_command(
        env, workmux_exe_path, mux_repo_path, "status --all --json"
    )
    parsed = json.loads(result.stdout)

    by_workdir = {entry["workdir"]: entry for entry in parsed["agents"]}
    assert set(by_workdir) == {str(first.worktree), str(second.worktree)}
    assert parsed["scope"] == {"repository": None, "all": True, "targets": []}
    assert by_workdir[str(first.worktree)]["project"] == mux_repo_path.name
    assert by_workdir[str(first.worktree)]["project_path"] == str(mux_repo_path)
    assert by_workdir[str(second.worktree)]["project"] == second_repo.name
    assert by_workdir[str(second.worktree)]["project_path"] == str(second_repo)


@pytest.mark.tmux_only
def test_status_project_selector_round_trips_json_and_completions(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """JSON project names address agents from outside any repository."""
    env = cast(TmuxEnvironment, mux_server)
    runner = env.tmux(["display-message", "-p", "#{window_id}"]).stdout.strip()
    agent = start_active_agent(
        env, workmux_exe_path, mux_repo_path, "feature-project-selector"
    )
    stabilize_tmux_agent(env, agent)
    env.select_window(runner)
    outside = mux_repo_path.parent

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"status --json {agent.branch}",
        working_dir=outside,
    )
    entry = json.loads(result.stdout)["agents"][0]
    assert entry["project"] == mux_repo_path.name
    selector = f"{entry['project']}:{entry['worktree']}"
    alias = f"{agent.worktree.parent.name}:{entry['worktree']}"

    for target in [selector, alias]:
        result = run_workmux_command(
            env,
            workmux_exe_path,
            mux_repo_path,
            f"status --json {target}",
            working_dir=outside,
        )
        parsed = json.loads(result.stdout)
        assert parsed["target_errors"] == []
        assert len(parsed["agents"]) == 1
        assert parsed["agents"][0]["workdir"] == entry["workdir"]
        assert parsed["agents"][0]["project_path"] == entry["project_path"]

    completions = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "_complete-agent-targets",
        working_dir=outside,
    ).stdout.splitlines()
    assert selector in completions
    assert alias not in completions
    assert entry["worktree"] in completions

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"send {selector} selector-smoke-test",
        working_dir=outside,
    )
    assert result.exit_code == 0


def test_status_json_fails_on_invalid_state(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Invalid state for the selected context fails without deleting evidence."""
    start_active_agent(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-invalid-state",
        status="working",
    )
    invalid_file = list_agent_state_files(mux_server)[0]
    invalid_file.write_text("{")

    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "status --json",
        expect_fail=True,
    )

    assert result.stdout == ""
    assert invalid_file.exists()
    assert "invalid agent state file" in result.stderr


def test_status_human_fails_on_invalid_state(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Human status preserves the same invalid-state failure contract."""
    start_active_agent(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-invalid-human",
        status="working",
    )
    invalid_file = list_agent_state_files(mux_server)[0]
    invalid_file.write_text("{")

    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "status",
        expect_fail=True,
    )

    assert result.stdout == ""
    assert invalid_file.exists()
    assert "invalid agent state file" in result.stderr


def test_status_json_reports_invalid_state_for_other_context(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Invalid state for another context is visible without failing the query."""
    agents_dir = get_agents_dir(mux_server)
    agents_dir.mkdir(parents=True, exist_ok=True)
    invalid_file = agents_dir / "wezterm__other__1.json"
    invalid_file.write_text("{")

    result = run_workmux_command(
        mux_server, workmux_exe_path, mux_repo_path, "status --json"
    )
    parsed = json.loads(result.stdout)

    assert parsed["state_files_total"] == 1
    assert parsed["state_files_invalid"] == 1
    assert parsed["state_files_invalid_matching_context"] == 0
    assert invalid_file.exists()


def test_status_json_rejects_invalid_backend_override(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """An invalid explicit backend cannot fall through to an empty result."""
    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "status --json",
        pre_run_env={"WORKMUX_BACKEND": "invalid"},
        expect_fail=True,
    )

    assert result.stdout == ""
    assert "invalid WORKMUX_BACKEND" in result.stderr


def test_status_json_fails_when_multiplexer_is_unavailable(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """An unavailable selected instance cannot appear as an empty result."""
    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "status --json",
        pre_run_env={"TMUX": "/tmp/workmux-missing-tmux-socket,1,0"},
        expect_fail=True,
    )

    assert result.stdout == ""
    assert "tmux query failed" in result.stderr


def test_status_with_active_agent(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status shows agent info when an agent is active."""
    env = mux_server
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-active",
        status="working",
    )

    result = run_workmux_command(env, workmux_exe_path, mux_repo_path, "status")
    assert "working" in result.stdout
    assert "WORKTREE" in result.stdout
    assert "STATUS" in result.stdout


def test_status_json_with_active_agent(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status --json returns agent data when an agent is active."""
    env = mux_server
    agent = start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-json",
        status="done",
    )

    result = run_workmux_command(env, workmux_exe_path, mux_repo_path, "status --json")
    parsed = json.loads(result.stdout)
    assert isinstance(parsed, dict)
    assert parsed["context"]["backend"] == env.backend_name
    assert parsed["state_files_total"] >= 1
    assert parsed["state_files_matching_context"] >= 1
    assert parsed["reconciled_agent_count"] >= 1
    assert len(parsed["agents"]) >= 1

    entry = parsed["agents"][0]
    assert entry["worktree"] == agent.worktree.name
    assert entry["branch"] == agent.branch
    assert entry["status"] == "done"
    assert entry["pane_id"]
    assert entry["workdir"] == str(agent.worktree)
    assert entry["agent_kind"] is None
    assert entry["session"]
    assert entry["window_name"] == agent.window
    assert isinstance(entry["updated_ts"], int)


@pytest.mark.tmux_only
def test_status_json_attributes_multiple_agents(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status --json identifies each agent sharing a worktree."""
    env = cast(TmuxEnvironment, mux_server)
    branch_name = "feature-status-multi"
    window_name = get_window_name(branch_name)
    write_workmux_config(
        mux_repo_path,
        panes=[
            {"focus": True},
            {"split": "horizontal"},
        ],
    )
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    wait_for_window_ready(env, window_name)

    pane_ids = env.tmux(
        ["list-panes", "-t", window_name, "-F", "#{pane_id}"]
    ).stdout.splitlines()
    assert len(pane_ids) == 2

    expected = {}
    for index, (pane_id, status, agent_kind) in enumerate(
        zip(pane_ids, ["working", "done"], ["claude", None], strict=True)
    ):
        marker = env.tmp_path / f"status-multi-{index}"
        env.send_keys(
            pane_id,
            build_status_cmd_with_marker(env, workmux_exe_path, status, marker),
        )
        assert poll_until(marker.exists, timeout=5.0)
        expected[pane_id] = {"status": status, "agent_kind": agent_kind}

    for pane_id in pane_ids:
        env.send_keys(pane_id, "exec sleep 30")
    assert poll_until(
        lambda: env.tmux(
            ["list-panes", "-t", window_name, "-F", "#{pane_current_command}"]
        ).stdout.splitlines()
        == ["sleep", "sleep"],
        timeout=5.0,
    )

    assert poll_until(lambda: len(list_agent_state_files(env)) == 2, timeout=5.0)
    live_panes = {
        pane_id: (command, int(pid))
        for pane_id, command, pid in (
            line.split("|", 2)
            for line in env.tmux(
                [
                    "list-panes",
                    "-t",
                    window_name,
                    "-F",
                    "#{pane_id}|#{pane_current_command}|#{pane_pid}",
                ]
            ).stdout.splitlines()
        )
    }
    for state_file in list_agent_state_files(env):
        state = json.loads(state_file.read_text())
        pane_id = state["pane_key"]["pane_id"]
        state["command"], state["pane_pid"] = live_panes[pane_id]
        state["workdir"] = str(get_worktree_path(mux_repo_path, branch_name))
        state["agent_kind"] = expected[pane_id]["agent_kind"]
        state_file.write_text(json.dumps(state))

    runner_window = next(name for name in env.list_windows() if name != window_name)
    env.select_window(runner_window)
    result = run_workmux_command(env, workmux_exe_path, mux_repo_path, "status --json")
    payload = json.loads(result.stdout)
    entries = [entry for entry in payload["agents"] if entry["branch"] == branch_name]
    assert payload["reconciled_agent_count"] >= 2

    assert len(entries) == 2
    assert {
        entry["pane_id"]: {
            "status": entry["status"],
            "agent_kind": entry["agent_kind"],
        }
        for entry in entries
    } == expected
    assert {entry["workdir"] for entry in entries} == {
        str(get_worktree_path(mux_repo_path, branch_name))
    }
    assert {entry["window_name"] for entry in entries} == {window_name}


def test_status_filter_by_worktree(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status filters to show only the specified worktree."""
    env = mux_server
    branch_name = "feature-status-filt"
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        status="working",
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"status --json {branch_name}",
    )
    parsed = json.loads(result.stdout)
    assert parsed["scope"]["targets"] == [branch_name]
    assert len(parsed["agents"]) >= 1
    for entry in parsed["agents"]:
        assert entry["branch"] == branch_name
        assert {
            "workdir",
            "agent_kind",
            "session",
            "window_name",
            "updated_ts",
        } <= entry.keys()


def test_status_json_returns_partial_results_for_unresolved_target(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """One unresolved target does not hide agents from resolved targets."""
    env = mux_server
    branch_name = "feature-status-partial"
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        status="working",
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"status --json {branch_name} nonexistent-worktree",
        expect_fail=True,
    )
    parsed = json.loads(result.stdout)

    assert [entry["branch"] for entry in parsed["agents"]] == [branch_name]
    assert parsed["target_errors"] == [
        {
            "target": "nonexistent-worktree",
            "code": "target_resolution_failed",
            "message": "No agent found matching 'nonexistent-worktree'",
        }
    ]


def test_status_json_resolves_existing_worktree_without_agent(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """A known worktree with no tracked agent is a successful empty target."""
    branch_name = "feature-status-no-agent"
    write_workmux_config(mux_repo_path)
    run_workmux_add(mux_server, workmux_exe_path, mux_repo_path, branch_name)

    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        f"status --json {branch_name}",
    )
    parsed = json.loads(result.stdout)

    assert parsed["agents"] == []
    assert parsed["target_errors"] == []


def test_status_filter_no_match_fails(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status fails when a requested worktree cannot be resolved."""
    env = mux_server
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-exists",
        status="working",
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "status nonexistent-worktree",
        expect_fail=True,
    )
    assert result.stdout == ""
    assert "No agent found matching 'nonexistent-worktree'" in result.stderr
