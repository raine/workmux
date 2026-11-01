"""Remote agent lifecycle over the loopback transport: `add --remote`, the
attach relay in a real tmux pane, `close`, `open`, `list` and `remove`.

The node runs on this machine under a temporary root with `isolation: none`,
driven by the same scripts SSH would run on a remote."""

import json
import os
import subprocess
from pathlib import Path
from typing import List

import pytest
import yaml

from ..conftest import (
    TmuxEnvironment,
    assert_window_exists,
    assert_window_not_exists,
    get_window_name,
    get_worktree_path,
    poll_until,
    setup_git_repo,
    slugify,
    wait_for_pane_output,
    write_workmux_config,
)
from .conftest import LOOPBACK, Node, commit_project, git, local_records, wm

pytestmark = pytest.mark.tmux_only


def setup_project(repo: Path) -> str:
    """Commit a project config locally (never pushed anywhere) and leave an
    ignored file for `remote_files.copy`; returns the commit."""
    project = {
        "nerdfont": False,
        "panes": [
            {"command": "echo AGENT-READY"},
            {"command": "echo SECOND-PANE", "split": "horizontal"},
        ],
        "remote_files": {"copy": [".env.local"]},
        "post_create": ["echo hooked > hook-ran.txt"],
    }
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    with open(repo / ".gitignore", "a") as gitignore:
        gitignore.write(".env.local\n")
    git(repo, "add", ".workmux.yaml", ".gitignore")
    git(repo, "commit", "-m", "configure remote panes")
    (repo / ".env.local").write_text("SECRET=fake\n")
    return git(repo, "rev-parse", "HEAD")


def pane_commands(env: TmuxEnvironment, window: str) -> List[str]:
    result = env.tmux(["list-panes", "-t", window, "-F", "#{pane_start_command}"])
    return result.stdout.splitlines()


def wait_for_startup_and_relay(
    env: TmuxEnvironment, node: Node, window: str, ex_id: str
) -> None:
    session = f"={ex_id}-0"
    target = f"{session}:"
    # Attaching a narrower viewer can reflow startup output into node history.
    # Check execution there, then check fresh output through the live relay.
    assert poll_until(
        lambda: "AGENT-READY"
        in node.tmux("capture-pane", "-p", "-S", "-", "-t", target).stdout,
        timeout=20,
    )
    assert poll_until(
        lambda: bool(
            node.tmux(
                "list-clients", "-t", session, "-F", "#{client_pid}"
            ).stdout.strip()
        ),
        timeout=20,
    )
    node.tmux("send-keys", "-t", target, "printf 'RELAY-%s\\n' READY", "Enter")
    wait_for_pane_output(env, window, "RELAY-READY", timeout=20)


def test_first_bash_pane_keeps_startup_output(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    # A prompt wider than the viewer exercises reflow on the first attach.
    prompt = "node:" + "x" * 100 + "$ "
    commit_project(
        mux_repo_path,
        {
            "nerdfont": False,
            "panes": [
                {
                    "command": f"PS1='{prompt}'; "
                    "echo ran >> command-runs; echo FIRST-PANE-READY"
                }
            ],
        },
    )
    assert node.sessions() == []
    wm(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "add first --remote lo",
        extra_env={"SHELL": "/bin/bash"},
    )
    [workspace] = node.workspaces()
    marker = Path(workspace["worktree_path"]) / "command-runs"
    assert poll_until(marker.exists, timeout=10)
    assert marker.read_text() == "ran\n"
    wait_for_pane_output(
        mux_server, get_window_name("first"), "FIRST-PANE-READY", timeout=20
    )


def test_remote_lifecycle(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    head = setup_project(repo)
    window = get_window_name("feat")

    result = wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    assert "Successfully created remote workspace" in result.stdout, result.stderr
    assert "Pushing" in result.stdout

    # The node has the worktree at the unpushed commit, with files and hooks.
    [workspace] = node.workspaces()
    worktree = Path(workspace["worktree_path"])
    assert worktree.is_relative_to(node.root)
    assert git(worktree, "rev-parse", "HEAD") == head
    assert git(worktree, "branch", "--show-current") == "feat"
    assert (worktree / ".env.local").read_text() == "SECRET=fake\n"
    assert (worktree / "hook-ran.txt").read_text().strip() == "hooked"
    assert not get_worktree_path(repo, "feat").exists()

    # One session per pane, relayed into a local window with the same layout.
    [record] = local_records(mux_server)
    assert record["state"] == "ready"
    ex_id = record["execution"]["ex_id"]
    assert node.sessions() == [f"{ex_id}-0", f"{ex_id}-1"]
    assert_window_exists(mux_server, window)
    commands = pane_commands(mux_server, window)
    assert len(commands) == 2
    assert all("remote attach" in command for command in commands)
    wait_for_startup_and_relay(mux_server, node, window, ex_id)

    # Keystrokes reach the remote shell.
    mux_server.tmux(["send-keys", "-t", window, "echo typed-$((40+2))", "Enter"])
    wait_for_pane_output(mux_server, window, "typed-42", timeout=10)

    listing = wm(mux_server, workmux_exe_path, repo, "list")
    assert "remote:lo" in listing.stdout
    rows = json.loads(wm(mux_server, workmux_exe_path, repo, "list --json").stdout)
    [remote_row] = [row for row in rows if row.get("remote") == "lo"]
    assert remote_row["handle"] == "feat"
    assert remote_row["is_open"] is True

    # Closing the window only detaches.
    wm(mux_server, workmux_exe_path, repo, "close feat")
    assert_window_not_exists(mux_server, window)
    assert node.sessions() == [f"{ex_id}-0", f"{ex_id}-1"]

    # Open rejoins the running execution instead of starting another.
    opened = wm(mux_server, workmux_exe_path, repo, "open feat")
    assert "Attached to the running agent" in opened.stdout, opened.stderr
    assert node.sessions() == [f"{ex_id}-0", f"{ex_id}-1"]
    assert_window_exists(mux_server, window)
    wait_for_pane_output(mux_server, window, "typed-42", timeout=20)
    second = wm(mux_server, workmux_exe_path, repo, "open feat")
    assert "Switched to remote workspace" in second.stdout
    assert len(pane_commands(mux_server, window)) == 2

    # A bare close inside the window closes it.
    pane = mux_server.tmux(["list-panes", "-t", window, "-F", "#{pane_id}"])
    current = {"TMUX_PANE": pane.stdout.split()[0]}
    closed = wm(mux_server, workmux_exe_path, repo, "close", extra_env=current)
    assert "Closed the window of 'feat'" in closed.stdout, closed.stderr
    assert_window_not_exists(mux_server, window)
    assert node.sessions() == [f"{ex_id}-0", f"{ex_id}-1"]

    # Session mode is refused as add --remote refuses it.
    config = repo / ".workmux.yaml"
    window_mode = config.read_text()
    config.write_text(window_mode + "mode: session\n")
    refused = wm(mux_server, workmux_exe_path, repo, "open feat", expect_fail=True)
    assert "Session mode cannot be used with remote workspace 'feat'" in refused.stderr
    assert "--mode window" in refused.stderr
    opened = wm(mux_server, workmux_exe_path, repo, "open feat --mode window")
    assert "Attached to the running agent" in opened.stdout, opened.stderr
    config.write_text(window_mode)

    # Commands that cannot act on remote workspaces yet say so.
    for command in ["path feat"]:
        refused = wm(mux_server, workmux_exe_path, repo, command, expect_fail=True)
        assert "is a remote workspace on 'lo'" in refused.stderr, command

    # The hook output is unsaved work: without a terminal, remove refuses.
    refused = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "remove feat",
        expect_fail=True,
        stdin_input="",
    )
    assert "--force" in refused.stderr
    assert worktree.is_dir()
    assert len(local_records(mux_server)) == 1

    removed = wm(mux_server, workmux_exe_path, repo, "remove --force feat")
    assert "Removed remote workspace 'feat'" in removed.stdout, removed.stderr
    assert not worktree.exists()
    assert node.workspaces() == []
    assert node.sessions() == []
    node_repo = Path(workspace["repo_path"])
    assert git(node_repo, "branch", "--list", "feat") == ""
    assert git(node_repo, "for-each-ref", "--format=%(refname)", "refs/workmux") == (
        "refs/workmux/project/transfer"
    )
    assert git(node_repo, "rev-parse", "refs/workmux/project/transfer") == head
    assert local_records(mux_server) == []
    assert_window_not_exists(mux_server, window)


def test_attach_relay_reconnects_and_closes_with_the_session(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    setup_project(repo)
    window = get_window_name("relay")
    wm(mux_server, workmux_exe_path, repo, "add relay --remote lo")
    [record] = local_records(mux_server)
    ex_id = record["execution"]["ex_id"]
    wait_for_startup_and_relay(mux_server, node, window, ex_id)

    def client_pids() -> List[str]:
        result = node.tmux("list-clients", "-t", f"={ex_id}-0", "-F", "#{client_pid}")
        return result.stdout.split()

    # A lost connection shows a status line and comes back by itself. The
    # reconnect replaces clients that lost connections leave on the node.
    [pid] = client_pids()
    socket = node.root / "run" / "tmux.sock"
    leftover_command = f"env -u TMUX tmux -S {socket} attach -t '={ex_id}-0'"
    mux_server.tmux(["new-window", "-d", "-n", "leftover", leftover_command])
    assert poll_until(lambda: len(client_pids()) == 2, timeout=10)
    os.kill(int(pid), 9)
    wait_for_pane_output(mux_server, window, "retry in", timeout=10)
    assert poll_until(lambda: len(client_pids()) == 1, timeout=15)

    def leftover_open() -> bool:
        result = mux_server.tmux(["list-windows", "-F", "#{window_name}"])
        return "leftover" in result.stdout.split()

    assert poll_until(lambda: not leftover_open(), timeout=5)
    mux_server.tmux(["send-keys", "-t", window, "echo back-$((1+1))", "Enter"])
    wait_for_pane_output(mux_server, window, "back-2", timeout=10)

    # A detach waits for the user; keys other than r and q are dropped.
    node.tmux("detach-client", "-s", f"={ex_id}-0")
    # tmux prints "detached" before the relay handles its exit and accepts
    # reconnect keys. Wait for the relay's own prompt.
    wait_for_pane_output(mux_server, window, "r reconnect", timeout=10)
    mux_server.tmux(["send-keys", "-t", window, "-l", "echo dropped"])
    mux_server.tmux(["send-keys", "-t", window, "r"])
    assert poll_until(lambda: len(client_pids()) == 1, timeout=15)
    mux_server.tmux(["send-keys", "-t", window, "echo rejoined", "Enter"])
    wait_for_pane_output(mux_server, window, "rejoined", timeout=10)
    content = mux_server.capture_pane(window) or ""
    assert "echo droppedecho" not in content

    # Exit messages of earlier attaches don't pile up and scroll the screen.
    node.tmux("detach-client", "-s", f"={ex_id}-0")
    wait_for_pane_output(mux_server, window, "r reconnect", timeout=10)
    content = mux_server.capture_pane(window) or ""
    assert content.count("[detached") == 1, content
    mux_server.tmux(["send-keys", "-t", window, "r"])
    assert poll_until(lambda: len(client_pids()) == 1, timeout=15)

    # When the remote session ends, its local pane closes.
    def local_panes() -> int:
        result = mux_server.tmux(["list-panes", "-t", window], check=False)
        return len(result.stdout.splitlines()) if result.returncode == 0 else 0

    assert local_panes() == 2
    node.tmux("kill-session", "-t", f"={ex_id}-0")
    assert poll_until(lambda: local_panes() == 1, timeout=10)


def test_add_resumes_interrupted_provisioning(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    """A failed `add --remote` is resumed by running it again: the same
    workspace continues where it stopped with the config as it is now, and
    finished hooks do not rerun."""
    repo = mux_repo_path
    allow = node.root / "allow-second-hook"
    project = {
        "nerdfont": False,
        "panes": [{"command": "echo AGENT-READY"}],
        "post_create": ["echo ran >> hooks.log", f"test -e {allow}"],
    }
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "configure hooks")

    failed = wm(
        mux_server, workmux_exe_path, repo, "add feat --remote lo", expect_fail=True
    )
    assert "test -e" in failed.stderr, failed.stderr
    assert "again to resume it" in failed.stderr
    [record] = local_records(mux_server)
    assert record["state"] == "provisioning"
    assert_window_not_exists(mux_server, get_window_name("feat"))

    # The user fixes the hook and adds a pane before trying again.
    project["post_create"] = ["echo ran >> hooks.log", "true"]
    project["panes"].append({"command": "echo SECOND-PANE", "split": "horizontal"})
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    resumed = wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    assert "Resuming remote workspace 'feat'" in resumed.stdout, resumed.stderr
    [workspace] = node.workspaces()
    assert workspace["ws_id"] == record["ws_id"]
    worktree = Path(workspace["worktree_path"])
    assert (worktree / "hooks.log").read_text() == "ran\n"
    [ready] = local_records(mux_server)
    assert ready["state"] == "ready"
    ex_id = ready["execution"]["ex_id"]
    assert node.sessions() == [f"{ex_id}-0", f"{ex_id}-1"]
    assert len(pane_commands(mux_server, get_window_name("feat"))) == 2
    wait_for_startup_and_relay(mux_server, node, get_window_name("feat"), ex_id)


def test_add_suffixes_a_name_another_repository_holds(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    """Like a local add, `add --remote` whose window name another
    repository's workspace holds takes the project-suffixed name, and a rerun
    of an interrupted add resumes that name. An explicit or taken suffixed
    name is refused."""
    repo = mux_repo_path
    other = mux_server.tmp_path.parent / "other-repo"
    other.mkdir(parents=True, exist_ok=True)
    setup_git_repo(other, mux_server.env)
    write_workmux_config(other)
    for branch in ["dup", "named", "twice"]:
        wm(mux_server, workmux_exe_path, other, f"add {branch}", working_dir=other)

    allow = node.root / "allow-hook"
    project = {
        "nerdfont": False,
        "panes": [{"command": "echo AGENT-READY"}],
        "post_create": [f"test -e {allow}"],
    }
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "configure hooks")
    suffixed = f"dup-{slugify(repo.name)}"

    failed = wm(
        mux_server, workmux_exe_path, repo, "add dup --remote lo", expect_fail=True
    )
    assert (
        f"window '{get_window_name('dup')}' exists in another repository, using "
        f"'{get_window_name(suffixed)}'" in failed.stderr
    ), failed.stderr
    [record] = local_records(mux_server)
    assert (record["handle"], record["state"]) == (suffixed, "provisioning")

    allow.touch()
    resumed = wm(mux_server, workmux_exe_path, repo, "add dup --remote lo")
    assert f"Resuming remote workspace '{suffixed}'" in resumed.stdout, resumed.stderr
    [workspace] = node.workspaces()
    assert (workspace["ws_id"], workspace["handle"]) == (record["ws_id"], suffixed)
    wait_for_pane_output(
        mux_server, get_window_name(suffixed), "AGENT-READY", timeout=20
    )
    assert_window_exists(mux_server, get_window_name("dup"))

    refused = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "add named --name named --remote lo",
        expect_fail=True,
    )
    assert (
        f"A tmux window named '{get_window_name('named')}' already exists"
        in refused.stderr
    ), refused.stderr

    mux_server.tmux(
        [
            "new-window",
            "-d",
            "-n",
            get_window_name(f"twice-{slugify(repo.name)}"),
        ]
    )
    refused = wm(
        mux_server, workmux_exe_path, repo, "add twice --remote lo", expect_fail=True
    )
    assert (
        f"A tmux window named '{get_window_name(f'twice-{slugify(repo.name)}')}' "
        "already exists" in refused.stderr
    ), refused.stderr
    assert [record["handle"] for record in local_records(mux_server)] == [suffixed]


def test_names_resolve_to_one_side(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    """A branch belongs to one workspace, and a local handle outranks a
    remote branch, so a name never reaches the wrong side."""
    repo = mux_repo_path
    setup_project(repo)
    wm(mux_server, workmux_exe_path, repo, "add x --name r1 --remote lo")

    for command in ["add x", "add x --name r2 --remote lo"]:
        refused = wm(mux_server, workmux_exe_path, repo, command, expect_fail=True)
        assert "Branch 'x' belongs to remote workspace 'r1' on 'lo'" in refused.stderr

    # A local worktree named `x` on another branch is reached by `x`.
    wm(mux_server, workmux_exe_path, repo, "add y --name x")
    local = get_worktree_path(repo, "x")
    path = wm(mux_server, workmux_exe_path, repo, "path x")
    assert Path(path.stdout.strip()).resolve() == local.resolve()
    wm(mux_server, workmux_exe_path, repo, "remove --force x")
    assert not local.exists()
    assert [ws["handle"] for ws in node.workspaces()] == ["r1"]
    assert [record["handle"] for record in local_records(mux_server)] == ["r1"]


def test_laptop_only_ignores_apply_on_the_node(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    """A copied file the laptop ignores through rules the node lacks stays
    ignored there, so an untouched workspace is clean and removes without
    --force."""
    repo = mux_repo_path
    project = {
        "nerdfont": False,
        "panes": [{"command": "echo AGENT-READY"}],
        "remote_files": {"copy": [".env.local"]},
    }
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "copy env")
    exclude = repo / ".git" / "info" / "exclude"
    exclude.parent.mkdir(parents=True, exist_ok=True)
    with open(exclude, "a") as rules:
        rules.write(".env.local\n")
    (repo / ".env.local").write_text("SECRET=fake\n")

    wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    [workspace] = node.workspaces()
    worktree = Path(workspace["worktree_path"])
    assert (worktree / ".env.local").read_text() == "SECRET=fake\n"
    assert git(worktree, "status", "--porcelain") == ""

    removed = wm(mux_server, workmux_exe_path, repo, "remove feat", stdin_input="")
    assert "Removed remote workspace 'feat'" in removed.stdout, removed.stderr
    assert node.workspaces() == []


def test_host_only_workspaces_never_need_docker(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    """`isolation: none` starts and removes without docker, even where a
    docker CLI exists but cannot reach its daemon."""
    stub_dir = tmp_path / "bin"
    stub_dir.mkdir()
    stub = stub_dir / "docker"
    stub.write_text(
        "#!/bin/sh\necho 'Cannot connect to the Docker daemon' >&2\nexit 1\n"
    )
    stub.chmod(0o755)
    broken_docker = {"PATH": f"{stub_dir}:{os.environ['PATH']}"}
    repo = mux_repo_path
    setup_project(repo)

    added = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "add feat --remote lo",
        extra_env=broken_docker,
    )
    assert "Successfully created remote workspace" in added.stdout, added.stderr
    removed = wm(
        mux_server,
        workmux_exe_path,
        repo,
        "remove --force feat",
        extra_env=broken_docker,
    )
    assert "Removed remote workspace 'feat'" in removed.stdout, removed.stderr
    assert node.workspaces() == []


def test_setup_token_reaches_claude_panes_on_the_host(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    """With `isolation: none` the node-wide setup token is in the session
    environment of a Claude workspace's panes."""
    token = "sk-ant-oat01-fake-token"
    (mux_server.fake_bin_dir / "claude").write_text(
        "#!/bin/sh\n"
        '[ "$1" = setup-token ] && { echo "fake setup-token flow"; exit 0; }\n'
        'echo "TOKEN-IN-PANE:${CLAUDE_CODE_OAUTH_TOKEN:-none}"\n'
        "exec sh\n"
    )
    (mux_server.fake_bin_dir / "claude").chmod(0o755)
    login = subprocess.run(
        [str(workmux_exe_path), "remote", "login", "lo", "claude", "--setup-token"],
        cwd=mux_repo_path,
        env={**mux_server.env, **LOOPBACK},
        input=f"{token}\n",
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert login.returncode == 0, login.stderr
    assert (node.root / "secrets" / "node.json").stat().st_mode & 0o777 == 0o600

    repo = mux_repo_path
    (repo / ".workmux.yaml").write_text(
        yaml.dump(
            {"nerdfont": False, "agent": "claude", "panes": [{"command": "<agent>"}]}
        )
    )
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "claude panes")
    added = wm(mux_server, workmux_exe_path, repo, "add feat --remote lo")
    assert "Successfully created remote workspace" in added.stdout, added.stderr
    wait_for_pane_output(
        mux_server, get_window_name("feat"), f"TOKEN-IN-PANE:{token}", timeout=20
    )
    assert list((node.root / "secrets").glob("*.tmux")) == []


@pytest.mark.parametrize("inherited", ["", "256color"])
def test_remote_panes_advertise_truecolor(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    inherited: str,
):
    commit_project(
        mux_repo_path,
        {
            "panes": [
                {"command": "printf 'COLOR=%s\\n' \"$COLORTERM\""},
                {
                    "command": "printf 'COLOR=%s\\n' \"$COLORTERM\"",
                    "split": "horizontal",
                },
            ]
        },
    )
    for handle in ["cold", "warm"]:
        wm(
            mux_server,
            workmux_exe_path,
            mux_repo_path,
            f"add {handle} --remote lo",
            extra_env={"COLORTERM": inherited},
        )
        workspace = next(ws for ws in node.workspaces() if ws["handle"] == handle)
        ex_id = workspace["execution"]["ex_id"]
        for index in [0, 1]:
            assert poll_until(
                lambda: "COLOR=truecolor"
                in node.tmux(
                    "capture-pane", "-p", "-S", "-", "-t", f"{ex_id}-{index}:"
                ).stdout
            )
        # A stale server environment must not control newly created sessions.
        node.tmux("set-environment", "-g", "COLORTERM", "256color")
