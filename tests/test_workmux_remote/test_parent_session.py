"""Remote relay placement and exact lifecycle targeting across tmux sessions."""

import json
import subprocess
from pathlib import Path

import pytest

from ..conftest import (
    TmuxEnvironment,
    get_window_name,
    poll_until,
    setup_git_repo,
    slugify,
)
from .conftest import LOOPBACK, Node, agent_dirs_env, commit_project, local_records, wm

pytestmark = pytest.mark.tmux_only


def windows(env: TmuxEnvironment, session: str) -> dict[str, str]:
    output = env.tmux(
        ["list-windows", "-t", f"={session}:", "-F", "#{window_name} #{window_id}"],
        check=False,
    )
    return dict(line.split() for line in output.stdout.splitlines())


def active(env: TmuxEnvironment, session: str) -> str:
    return env.tmux(
        ["display-message", "-p", "-t", f"={session}:", "#{window_id}:#{pane_id}"]
    ).stdout.strip()


@pytest.mark.parametrize("existing", [False, True])
def test_explicit_parent_lifecycle(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    existing: bool,
):
    env, exe, repo = mux_server, workmux_exe_path, mux_repo_path
    commit_project(
        repo,
        {"panes": [{"command": "echo READY"}, {"split": "horizontal", "zoom": True}]},
    )
    name = get_window_name("feat")
    env.tmux(["new-session", "-d", "-s", "Review-extra", "-n", name])
    env.tmux(["new-window", "-d", "-t", "=test:", "-n", name])
    decoys = windows(env, "Review-extra"), set(windows(env, "test").values())
    if existing:
        env.tmux(["new-session", "-d", "-s", "Review", "-n", "control"])
    before = active(env, "test")
    parent_before = active(env, "Review") if existing else None
    wm(
        env,
        exe,
        repo,
        "add feat --remote lo -b --parent-session Review",
        extra_env={"TMUX_PANE": ""},
    )
    assert active(env, "test") == before
    if existing:
        assert active(env, "Review") == parent_before
    assert windows(env, "Review-extra") == decoys[0]
    assert set(windows(env, "test").values()) == decoys[1]
    target = windows(env, "Review")[name]
    [record] = local_records(env)
    assert record["window_session"] == "Review"
    sessions = node.sessions()
    assert len(sessions) == 2

    rows = json.loads(wm(env, exe, repo, "list --json").stdout)
    assert next(row for row in rows if row.get("remote") == "lo")["is_open"]
    assert "Switched" in wm(env, exe, repo, "open feat").stdout
    assert windows(env, "Review")[name] == target

    # Linked panes retain membership in the recorded parent, even when another
    # session's listing reports the same pane IDs later.
    env.tmux(["link-window", "-s", target, "-t", "=Review-extra:"])
    rows = json.loads(wm(env, exe, repo, "list --json").stdout)
    assert next(row for row in rows if row.get("remote") == "lo")["is_open"]
    env.tmux(["unlink-window", "-t", f"=Review-extra:{target}"])

    wm(env, exe, repo, "close feat")
    assert name not in windows(env, "Review")
    assert node.sessions() == sessions
    assert windows(env, "Review-extra") == decoys[0]
    assert set(windows(env, "test").values()) == decoys[1]
    reopened = wm(env, exe, repo, "open feat")
    assert "Attached to the running agent" in reopened.stdout
    assert name in windows(env, "Review")
    assert local_records(env)[0]["execution"] == record["execution"]
    wm(env, exe, repo, "remove --force feat")
    assert name not in windows(env, "Review")
    assert windows(env, "Review-extra") == decoys[0]
    assert set(windows(env, "test").values()) == decoys[1]
    assert node.sessions() == []


def test_implicit_parent_cross_session_and_ambiguity(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path, node: Node
):
    env, exe, repo = mux_server, workmux_exe_path, mux_repo_path
    commit_project(repo, {"panes": [{"command": "echo READY"}]})
    wm(env, exe, repo, "add feat --remote lo -b")
    name = get_window_name("feat")
    target = windows(env, "test")[name]
    env.tmux(["new-session", "-d", "-s", "elsewhere"])
    env.tmux(["move-window", "-s", target, "-t", "=elsewhere:"])
    env.tmux(["new-window", "-d", "-t", "=test:", "-n", name + "-extra"])
    rows = json.loads(wm(env, exe, repo, "list --json").stdout)
    assert next(row for row in rows if row.get("remote") == "lo")["is_open"]
    assert "Switched" in wm(env, exe, repo, "open feat").stdout
    env.tmux(["new-window", "-d", "-t", "=test:", "-n", name])
    decoy = windows(env, "test")[name]
    for command in ["close feat", "open feat"]:
        refused = wm(env, exe, repo, command, expect_fail=True)
        assert "Ambiguous tmux window" in refused.stderr
        assert windows(env, "elsewhere")[name] == target
        assert windows(env, "test")[name] == decoy
    env.tmux(["kill-window", "-t", decoy])
    wm(env, exe, repo, "close feat")
    assert name not in windows(env, "elsewhere")
    assert name + "-extra" in windows(env, "test")
    assert node.sessions()


def test_parent_collision_and_provisioning_retry(
    mux_server: TmuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path, node: Node
):
    env, exe, repo = mux_server, workmux_exe_path, mux_repo_path
    allow = node.root / "allow"
    commit_project(
        repo,
        {"panes": [{"command": "echo READY"}], "post_create": [f"test -e {allow}"]},
    )
    env.tmux(["new-session", "-d", "-s", "Review", "-n", get_window_name("feat")])
    refused = wm(
        env,
        exe,
        repo,
        "add feat --name feat --remote lo --parent-session Review",
        expect_fail=True,
    )
    assert "already exists" in refused.stderr
    assert node.workspaces() == []
    failed = wm(
        env,
        exe,
        repo,
        "add feat --remote lo --parent-session Review -b",
        expect_fail=True,
    )
    assert "again to resume" in failed.stderr
    [record] = local_records(env)
    assert record["handle"] == f"feat-{slugify(repo.name)}"
    assert record["window_session"] == "Review"
    refused = wm(
        env,
        exe,
        repo,
        "add feat --remote lo --parent-session Different",
        expect_fail=True,
    )
    assert "Cannot change --parent-session" in refused.stderr
    allow.touch()
    wm(env, exe, repo, "add feat --remote lo -b")
    assert local_records(env)[0]["ws_id"] == record["ws_id"]
    assert get_window_name(record["handle"]) in windows(env, "Review")


def test_parent_wait_tracks_window_id(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    env, exe, repo = mux_server, workmux_exe_path, mux_repo_path
    commit_project(repo, {"panes": [{"command": "echo READY"}]})
    name = get_window_name("feat")
    env.tmux(["new-window", "-d", "-t", "=test:", "-n", name])
    process_env = {
        **env.env,
        **LOOPBACK,
        **agent_dirs_env(env.home_path),
        "TMUX": f"{env.socket_path},0,0",
    }
    with (tmp_path / "wait.log").open("w+") as log:
        process = subprocess.Popen(
            [
                str(exe),
                "add",
                "feat",
                "--remote",
                "lo",
                "-b",
                "--parent-session",
                "Review",
                "--wait",
            ],
            cwd=repo,
            env=process_env,
            stdout=log,
            stderr=log,
        )
        try:

            def waiting():
                log.seek(0)
                return "Waiting for window" in log.read()

            assert poll_until(waiting, timeout=60)
            assert process.poll() is None
            target = windows(env, "Review")[name]
            env.tmux(["kill-window", "-t", target])
            assert process.wait(timeout=10) == 0
            assert name in windows(env, "test")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=10)


def test_explicit_add_does_not_make_an_implicit_workspace_ambiguous(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    env, exe, repo = mux_server, workmux_exe_path, mux_repo_path
    commit_project(repo, {"panes": [{"command": "echo READY"}]})
    wm(env, exe, repo, "add feat --remote lo -b")
    other = tmp_path / "other-repo"
    other.mkdir()
    setup_git_repo(other, env.env)
    commit_project(other, {"panes": [{"command": "echo READY"}]})
    refused = wm(
        env,
        exe,
        other,
        "add feat --name feat --remote lo --parent-session Review -b",
        expect_fail=True,
    )
    assert "already exists" in refused.stderr
    wm(env, exe, other, "add feat --remote lo --parent-session Review -b")
    assert get_window_name(f"feat-{slugify(other.name)}") in windows(env, "Review")
    rows = json.loads(wm(env, exe, repo, "list --json").stdout)
    assert next(row for row in rows if row.get("remote") == "lo")["is_open"]
    wm(env, exe, repo, "close feat")
    assert get_window_name("feat") not in windows(env, "test")
    assert get_window_name(f"feat-{slugify(other.name)}") in windows(env, "Review")
