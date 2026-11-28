"""
Tests for desktop notifications on agent status changes.

Notifications go to the file named by WORKMUX_TEST_NOTIFICATION_LOG instead of
the desktop, so each test can read what would have been shown.
"""

import time
from pathlib import Path

from .conftest import (
    MuxEnvironment,
    get_window_name,
    get_worktree_path,
    poll_until,
    run_workmux_add,
    wait_for_window_ready,
    write_workmux_config,
)
from .support.agent_state import build_status_cmd, list_agent_state_files


def _write_global_notifications(env: MuxEnvironment, body: str) -> None:
    config_dir = Path(env.env["XDG_CONFIG_HOME"]) / "workmux"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.yaml").write_text(body)


def _setup_agent(
    env: MuxEnvironment, workmux_exe_path: Path, repo_path: Path, branch: str
) -> tuple[str, Path]:
    window = get_window_name(branch)
    write_workmux_config(repo_path, panes=[{"focus": True}])
    run_workmux_add(env, workmux_exe_path, repo_path, branch)
    wait_for_window_ready(env, window)
    log = env.tmp_path / f"notifications-{branch}.log"
    return window, log


def _report(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    window: str,
    log: Path,
    status: str,
) -> None:
    """Report `status` from the agent pane and wait until it is persisted."""
    marker = env.tmp_path / f"reported-{time.monotonic_ns()}"
    command = build_status_cmd(
        env,
        workmux_exe_path,
        status,
        {"WORKMUX_TEST_NOTIFICATION_LOG": str(log)},
    )
    env.send_keys(window, f"{command}; touch {marker}")
    assert poll_until(marker.exists, timeout=5.0), f"status {status} not reported"


def _messages(log: Path) -> list[str]:
    return log.read_text().splitlines() if log.exists() else []


def test_waiting_and_done_notify_once_per_transition(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    _write_global_notifications(
        env, "notifications:\n  desktop: [waiting, done]\n  delay_seconds: 0\n"
    )
    window, log = _setup_agent(env, workmux_exe_path, mux_repo_path, "notify-edges")

    _report(env, workmux_exe_path, window, log, "working")
    _report(env, workmux_exe_path, window, log, "waiting")
    assert poll_until(lambda: len(_messages(log)) == 1, timeout=5.0)
    assert _messages(log) == [f"{window} needs input"]

    # A repeated report is not a new transition.
    _report(env, workmux_exe_path, window, log, "waiting")
    _report(env, workmux_exe_path, window, log, "done")
    assert poll_until(lambda: len(_messages(log)) == 2, timeout=5.0)
    time.sleep(0.5)
    assert _messages(log) == [f"{window} needs input", f"{window} finished"]


def test_first_done_is_a_baseline(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    _write_global_notifications(
        env, "notifications:\n  desktop: [waiting, done]\n  delay_seconds: 0\n"
    )
    window, log = _setup_agent(env, workmux_exe_path, mux_repo_path, "notify-base")

    _report(env, workmux_exe_path, window, log, "done")
    assert poll_until(lambda: len(list_agent_state_files(env)) == 1, timeout=5.0)
    time.sleep(1.0)
    assert _messages(log) == []


def test_unlisted_events_and_missing_config_stay_silent(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    _write_global_notifications(
        env, "notifications:\n  desktop: [waiting]\n  delay_seconds: 0\n"
    )
    window, log = _setup_agent(env, workmux_exe_path, mux_repo_path, "notify-filter")

    _report(env, workmux_exe_path, window, log, "working")
    _report(env, workmux_exe_path, window, log, "done")
    time.sleep(1.0)
    assert _messages(log) == []

    # Project config cannot enable notifications.
    _write_global_notifications(env, "{}\n")
    project_config = get_worktree_path(mux_repo_path, "notify-filter") / ".workmux.yaml"
    project_config.write_text(
        "nerdfont: false\nnotifications:\n  desktop: [waiting]\n  delay_seconds: 0\n"
    )
    _report(env, workmux_exe_path, window, log, "waiting")
    time.sleep(1.0)
    assert _messages(log) == []


def test_delay_drops_transient_states_and_keeps_held_ones(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    _write_global_notifications(
        env, "notifications:\n  desktop: [waiting, done]\n  delay_seconds: 2\n"
    )
    window, log = _setup_agent(env, workmux_exe_path, mux_repo_path, "notify-delay")

    # Waiting that resolves within the delay never notifies.
    _report(env, workmux_exe_path, window, log, "working")
    _report(env, workmux_exe_path, window, log, "waiting")
    _report(env, workmux_exe_path, window, log, "working")
    time.sleep(3.0)
    assert _messages(log) == []

    # Waiting held past the delay notifies once, even when repeated: a
    # repeated report does not restart or cancel the pending notification.
    started = time.monotonic()
    _report(env, workmux_exe_path, window, log, "waiting")
    _report(env, workmux_exe_path, window, log, "waiting")
    assert poll_until(lambda: len(_messages(log)) == 1, timeout=6.0)
    assert time.monotonic() - started >= 1.5
    time.sleep(1.0)
    assert _messages(log) == [f"{window} needs input"]


def test_clear_cancels_pending_notification(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    env = mux_server
    _write_global_notifications(
        env, "notifications:\n  desktop: [waiting, done]\n  delay_seconds: 2\n"
    )
    window, log = _setup_agent(env, workmux_exe_path, mux_repo_path, "notify-clear")

    _report(env, workmux_exe_path, window, log, "waiting")
    _report(env, workmux_exe_path, window, log, "clear")
    time.sleep(3.0)
    assert _messages(log) == []
