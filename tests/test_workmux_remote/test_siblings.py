"""Agents that spawn agents on their remote: `workmux add` inside a remote
execution creates a sibling workspace on the same node, which the laptop
adopts the next time it looks at the node."""

import json
import time
from pathlib import Path

import pytest
import yaml

from ..conftest import (
    TmuxEnvironment,
    assert_window_exists,
    assert_window_not_exists,
    get_window_name,
    poll_until,
    wait_for_pane_output,
)
from .conftest import Node, commit_project, git, local_records, observation_path, wm

pytestmark = pytest.mark.tmux_only


def setup_project(repo: Path) -> None:
    commit_project(
        repo, {"nerdfont": False, "panes": [{"command": "echo AGENT-READY"}]}
    )


def spawn(node: Node, ex_id: str, args: str) -> None:
    """Type `workmux add <args>` into the node session of `ex_id`, the way
    an agent without a sandbox would run it."""
    wrapper = node.root / "run" / "bin" / "workmux"
    node.tmux("send-keys", "-t", f"{ex_id}-0:", f"{wrapper} add {args}", "Enter")


def node_pane(node: Node, ex_id: str) -> str:
    return node.tmux("capture-pane", "-p", "-t", f"{ex_id}-0:").stdout


def by_branch(node: Node, branch: str):
    found = [ws for ws in node.workspaces() if ws["branch"] == branch]
    return found[0] if found else None


def running(node: Node, branch: str) -> bool:
    ws = by_branch(node, branch)
    return bool(ws and ws.get("execution") and ws["execution"]["state"] == "running")


def active_window(env: TmuxEnvironment) -> str:
    return env.tmux(["display-message", "-p", "#{window_name}"]).stdout.strip()


AGENT = """#!/bin/sh
# Applies commands written to agent-control in its worktree's git dir.
control="$(git rev-parse --absolute-git-dir)/agent-control"
echo FAKE-AGENT-READY
while :; do
  if [ -f "$control" ]; then
    command=$(cat "$control")
    rm -f "$control"
    case $command in
      spawn\\ *) workmux add ${command#spawn } </dev/null; echo "spawn-exit-$?" ;;
      *) workmux set-window-status "$command" </dev/null && echo "applied-$command" ;;
    esac
  fi
  sleep 0.1
done
"""


def setup_agent_project(repo: Path, agent: Path) -> None:
    agent.write_text(AGENT)
    agent.chmod(0o755)
    commit_project(
        repo,
        {"nerdfont": False, "agent": str(agent), "panes": [{"command": "<agent>"}]},
    )


def tell(worktree: Path, command: str) -> None:
    control = Path(git(worktree, "rev-parse", "--absolute-git-dir")) / "agent-control"
    tmp = control.with_suffix(".tmp")
    tmp.write_text(command)
    tmp.rename(control)


def test_agent_spawns_sibling(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    repo = mux_repo_path
    setup_agent_project(repo, tmp_path / "fake-agent")
    project = yaml.safe_load((repo / ".workmux.yaml").read_text())
    project["files"] = {"copy": ["copied-binary"]}
    commit_project(repo, project)
    with (repo / ".git/info/exclude").open("a") as exclude:
        exclude.write("\ncopied-binary\n")
    binary = bytes(range(256)) * 1024
    (repo / "copied-binary").write_bytes(binary)
    (repo / "copied-binary").chmod(0o751)
    added = wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    assert "Successfully created remote workspace" in added.stdout, added.stderr
    [parent] = node.workspaces()
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    wait_for_pane_output(
        mux_server, get_window_name("feat"), "FAKE-AGENT-READY", timeout=20
    )

    # The parent commits; its sibling starts from that commit.
    worktree = Path(parent["worktree_path"])
    # Siblings replay the saved recipe, not either mutable source tree.
    (repo / "copied-binary").unlink()
    (worktree / "copied-binary").write_text("changed by parent")
    (worktree / "parent.txt").write_text("from the parent\n")
    git(worktree, "add", "parent.txt")
    git(worktree, "commit", "-m", "parent work")
    tip = git(worktree, "rev-parse", "HEAD")

    # While the parent's window relays it, the relay's observer adopts the
    # sibling and opens its window without focus.
    focused = active_window(mux_server)
    tell(worktree, "spawn sib -b -p sibling-task")
    assert poll_until(lambda: running(node, "sib"), timeout=20), node_pane(node, ex_id)
    assert "spawn-exit-0" in node_pane(node, ex_id)
    sibling = by_branch(node, "sib")
    assert sibling is not None
    assert sibling["origin"]["parent"] == parent["ws_id"]
    assert sibling["origin"]["background"] is True
    assert sibling["base_sha"] == tip
    assert sibling["project_id"] == parent["project_id"]
    sib_tree = Path(sibling["worktree_path"])
    assert (sib_tree / "parent.txt").is_file()
    assert (sib_tree / "copied-binary").read_bytes() == binary
    assert (sib_tree / "copied-binary").stat().st_mode & 0o777 == 0o751
    assert git(sib_tree, "branch", "--show-current") == "sib"
    prompt = sib_tree / ".workmux" / "PROMPT-sib.md"
    assert prompt.read_text().strip() == "sibling-task"

    assert poll_until(
        lambda: any(r["branch"] == "sib" for r in local_records(mux_server)),
        timeout=20,
    )
    window = get_window_name("sib")
    assert poll_until(lambda: window in mux_server.list_windows(), timeout=10)
    wait_for_pane_output(mux_server, window, "FAKE-AGENT-READY", timeout=20)
    assert active_window(mux_server) == focused
    [adopted] = [r for r in local_records(mux_server) if r["branch"] == "sib"]
    assert adopted["state"] == "ready"
    assert adopted["base_label"] == "feat"
    assert not adopted.get("window_pending")

    # A branch that exists is refused, and so is a spawn without a name.
    tell(worktree, "spawn sib -b -p again")
    assert poll_until(
        lambda: "already exists on the node" in node_pane(node, ex_id), timeout=20
    ), node_pane(node, ex_id)
    tell(worktree, "spawn -A -p unnamed")
    assert poll_until(
        lambda: "remote agents must name the branch" in node_pane(node, ex_id),
        timeout=20,
    ), node_pane(node, ex_id)
    assert len(node.workspaces()) == 2

    # The sibling shows in `list` and `status`, and closes, opens and goes
    # like any remote workspace.
    tell(sib_tree, "working")
    assert poll_until(
        lambda: "applied-working"
        in node.tmux(
            "capture-pane", "-p", "-t", f"{sibling['execution']['ex_id']}-0:"
        ).stdout,
        timeout=20,
    )
    listing = wm(mux_server, workmux_exe_path, repo, "list")
    assert "sib" in listing.stdout and "remote:lo" in listing.stdout

    def sib_status():
        status = json.loads(
            wm(mux_server, workmux_exe_path, repo, "status --json").stdout
        )
        return [
            a for a in status["agents"] if a.get("remote") and a["worktree"] == "sib"
        ]

    assert poll_until(
        lambda: [a["status"] for a in sib_status()] == ["working"], timeout=20
    ), sib_status()
    wm(mux_server, workmux_exe_path, repo, "close sib")
    assert_window_not_exists(mux_server, window)
    opened = wm(mux_server, workmux_exe_path, repo, "open sib")
    assert "Attached to the running agent" in opened.stdout, opened.stderr
    observed = observation_path(mux_server)
    before = json.loads(observed.read_text())
    assert any(ws.get("sibling") for ws in before["workspaces"])
    removed = wm(mux_server, workmux_exe_path, repo, "remove --force sib")
    assert "Removed remote workspace 'sib'" in removed.stdout, removed.stderr
    assert by_branch(node, "sib") is None

    # An observation from before the removal does not bring it back.
    observed.write_text(json.dumps({**before, "last_seen": int(time.time())}))
    listing = wm(mux_server, workmux_exe_path, repo, "list")
    assert "adopted" not in listing.stderr, listing.stderr
    assert [r["branch"] for r in local_records(mux_server)] == ["feat"]


def test_sibling_spawned_while_nobody_watches(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    setup_project(repo)
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    wait_for_pane_output(mux_server, get_window_name("feat"), "AGENT-READY", timeout=20)
    # Without a window nothing on the laptop observes the node.
    wm(mux_server, workmux_exe_path, repo, "close feat")

    spawn(node, ex_id, "later -p 'while away'")
    assert poll_until(lambda: running(node, "later"), timeout=20), node_pane(
        node, ex_id
    )
    assert not any(r["branch"] == "later" for r in local_records(mux_server))

    # The next command that looks at the node adopts it; spawned moments
    # ago without -b, its window takes focus.
    listing = wm(mux_server, workmux_exe_path, repo, "list")
    assert "later" in listing.stdout, listing.stderr
    window = get_window_name("later")
    assert_window_exists(mux_server, window)
    assert active_window(mux_server) == window
    wait_for_pane_output(mux_server, window, "AGENT-READY", timeout=20)


def test_forced_remove_of_a_sibling_stays_forgotten(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    setup_project(repo)
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    wait_for_pane_output(mux_server, get_window_name("feat"), "AGENT-READY", timeout=20)
    wm(mux_server, workmux_exe_path, repo, "close feat")
    spawn(node, ex_id, "gone -b -p 'left behind'")
    assert poll_until(lambda: running(node, "gone"), timeout=20), node_pane(node, ex_id)
    wm(mux_server, workmux_exe_path, repo, "list")
    assert any(r["branch"] == "gone" for r in local_records(mux_server))

    # Without the remote's config the node cannot be reached, and --force
    # forgets the workspace on the laptop only.
    config = mux_server.home_path / ".config" / "workmux" / "config.yaml"
    saved = config.read_text()
    config.write_text("nerdfont: false\n")
    removed = wm(mux_server, workmux_exe_path, repo, "remove --force gone")
    assert "forgot remote workspace 'gone'" in removed.stderr, removed.stderr
    config.write_text(saved)

    # Later looks at the node, which still runs it, leave it for gc.
    time.sleep(2)
    for _ in range(2):
        listing = wm(mux_server, workmux_exe_path, repo, "list")
        assert "adopted" not in listing.stderr, listing.stderr
    assert [r["branch"] for r in local_records(mux_server)] == ["feat"]
    assert running(node, "gone")
    gc = wm(mux_server, workmux_exe_path, repo, "remote gc lo --dry-run")
    assert "workspace 'gone'" in gc.stdout, gc.stdout + gc.stderr


def test_sibling_refuses_symlinks_from_the_parent(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    repo = mux_repo_path
    setup_project(repo)
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    [parent] = node.workspaces()
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    wait_for_pane_output(mux_server, get_window_name("feat"), "AGENT-READY", timeout=20)

    # The agent commits a symlink where the node writes the prompt file.
    outside = tmp_path / "outside"
    outside.mkdir()
    worktree = Path(parent["worktree_path"])
    (worktree / ".workmux").symlink_to(outside)
    git(worktree, "add", ".workmux")
    git(worktree, "commit", "-m", "point .workmux elsewhere")

    spawn(node, ex_id, "evil -p 'payload'")
    assert poll_until(
        lambda: "is a symlink in the checkout" in node_pane(node, ex_id), timeout=20
    ), node_pane(node, ex_id)
    assert list(outside.iterdir()) == []
    assert by_branch(node, "evil") is None
    assert [ws["branch"] for ws in node.workspaces()] == ["feat"]


def session_windows(
    env: TmuxEnvironment, session: str, field: str = "window_name"
) -> list:
    """`(<field>, active)` of the windows of `session`, in order."""
    out = env.tmux(
        ["list-windows", "-t", session, "-F", f"#{{{field}}} #{{window_active}}"]
    ).stdout
    return [(line.split()[0], line.split()[1] == "1") for line in out.splitlines()]


def test_sibling_opens_next_to_its_parent_in_another_session(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    repo = mux_repo_path
    setup_agent_project(repo, tmp_path / "fake-agent")
    # The first relay observes the node for every window.
    wm(mux_server, workmux_exe_path, repo, "add obs --remote lo")
    wait_for_pane_output(
        mux_server, get_window_name("obs"), "FAKE-AGENT-READY", timeout=20
    )
    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    wait_for_pane_output(
        mux_server, get_window_name("feat"), "FAKE-AGENT-READY", timeout=20
    )
    parent = by_branch(node, "feat")
    assert parent is not None

    mux_server.tmux(["new-session", "-d", "-s", "other"])
    feat_id = mux_server.tmux(
        [
            "display-message",
            "-p",
            "-t",
            f"test:{get_window_name('feat')}",
            "#{window_id}",
        ]
    ).stdout.strip()
    mux_server.tmux(["move-window", "-s", feat_id, "-t", "other:"])
    # Ids, not names: tmux renames the runner window after its command.
    before = session_windows(mux_server, "test", "window_id")

    tell(Path(parent["worktree_path"]), "spawn sib -p elsewhere")
    window = get_window_name("sib")
    assert poll_until(
        lambda: window in [name for name, _ in session_windows(mux_server, "other")],
        timeout=20,
    ), session_windows(mux_server, "other")
    names = [name for name, _ in session_windows(mux_server, "other")]
    assert names.index(window) == names.index(get_window_name("feat")) + 1
    # Opened where its parent is, without taking focus from another session.
    assert session_windows(mux_server, "test", "window_id") == before
    assert (window, False) in session_windows(mux_server, "other")
