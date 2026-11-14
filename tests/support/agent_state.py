"""Helpers for agent state file setup in command tests."""

import json
import shlex
from dataclasses import dataclass
from pathlib import Path

from ..conftest import (
    MuxEnvironment,
    TmuxEnvironment,
    get_window_name,
    get_worktree_path,
    make_env_script,
    poll_until,
    run_workmux_add,
    wait_for_window_ready,
    write_workmux_config,
)


@dataclass(frozen=True)
class ActiveAgent:
    branch: str
    window: str
    worktree: Path


def get_state_dir(env: MuxEnvironment) -> Path:
    return Path(env.env["XDG_STATE_HOME"]) / "workmux"


def get_agents_dir(env: MuxEnvironment) -> Path:
    return get_state_dir(env) / "agents"


def list_agent_state_files(env: MuxEnvironment) -> list[Path]:
    agents_dir = get_agents_dir(env)
    if not agents_dir.exists():
        return []
    return list(agents_dir.glob("*.json"))


def read_agent_state(path: Path) -> dict:
    return json.loads(path.read_text())


def build_status_cmd(
    env: MuxEnvironment,
    workmux_exe: Path,
    status: str,
    env_vars: dict[str, str] | None = None,
) -> str:
    command = f"{workmux_exe} set-window-status {status}"
    script_env = {"XDG_STATE_HOME": env.env["XDG_STATE_HOME"]}
    if env_vars:
        script_env.update(env_vars)
    return make_env_script(env, command, script_env)


def build_status_cmd_with_marker(
    env: MuxEnvironment,
    workmux_exe: Path,
    status: str,
    marker_path: Path,
    env_vars: dict[str, str] | None = None,
) -> str:
    command = f"{workmux_exe} set-window-status {status}; touch {shlex.quote(str(marker_path))}"
    script_env = {"XDG_STATE_HOME": env.env["XDG_STATE_HOME"]}
    if env_vars:
        script_env.update(env_vars)
    return make_env_script(env, command, script_env)


def mark_agent_state(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    window_name: str,
    status: str,
    *,
    timeout: float = 5.0,
) -> None:
    status_cmd = build_status_cmd(env, workmux_exe_path, status)
    env.send_keys(window_name, status_cmd)
    assert poll_until(lambda: len(list_agent_state_files(env)) > 0, timeout=timeout), (
        "Agent state file not created"
    )


def start_active_agent(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch: str,
    *,
    status: str = "working",
    timeout: float = 5.0,
) -> ActiveAgent:
    window_name = get_window_name(branch)
    write_workmux_config(repo_path, panes=[{"focus": True}])
    run_workmux_add(env, workmux_exe_path, repo_path, branch)
    wait_for_window_ready(env, window_name)

    mark_agent_state(env, workmux_exe_path, window_name, status, timeout=timeout)

    return ActiveAgent(
        branch=branch,
        window=window_name,
        worktree=get_worktree_path(repo_path, branch),
    )


def stabilize_tmux_agent(env: TmuxEnvironment, agent: ActiveAgent) -> None:
    """Keep a synthetic tracked agent live across subsequent test commands."""
    stabilize_tmux_pane(env, agent.window)


def stabilize_tmux_pane(env: TmuxEnvironment, target: str) -> None:
    """Keep the synthetic tracked agent of pane `target` live across
    subsequent test commands."""
    env.send_keys(target, "exec sleep 30")

    def pane() -> list[str]:
        return (
            env.tmux(
                [
                    "display-message",
                    "-p",
                    "-t",
                    target,
                    "#{pane_id}|#{pane_current_command}|#{pane_pid}",
                ]
            )
            .stdout.strip()
            .split("|", 2)
        )

    assert poll_until(lambda: pane()[1] == "sleep", timeout=5.0)
    pane_id, command, pane_pid = pane()
    for state_file in list_agent_state_files(env):
        state = read_agent_state(state_file)
        if state["pane_key"]["pane_id"] == pane_id:
            state["command"] = command
            state["pane_pid"] = int(pane_pid)
            state_file.write_text(json.dumps(state))
            return
    raise AssertionError(f"No state file found for pane {pane_id}")
