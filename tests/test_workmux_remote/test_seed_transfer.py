"""Compressed file transfer through real add commands and an instrumented loopback node."""

import json
import shutil
import sys
from pathlib import Path

import pytest
import yaml

from ..conftest import TmuxEnvironment
from .conftest import Node, commit_project, git, wm

pytestmark = pytest.mark.tmux_only


@pytest.mark.parametrize("mode", ["warm", "race", "truncated", "oversize", "path"])
def test_seed_transfer(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
    mode: str,
):
    """Real adds preserve trees and cache behavior; bad uploads cannot provision."""
    log = tmp_path / "requests.jsonl"
    raced = tmp_path / "raced"
    wrapper = tmp_path / "node-wrapper"
    wrapper.write_text(
        f"#!{sys.executable}\n"
        "import gzip, json, os, pathlib, shutil, subprocess, sys\n"
        f"binary = {str(workmux_exe_path)!r}\n"
        "if sys.argv[1:3] != ['node', 'provision-workspace']:\n"
        "    os.execv(binary, [binary, *sys.argv[1:]])\n"
        "raw = sys.stdin.buffer.read()\n"
        "decoded = gzip.decompress(raw)\n"
        "length = int.from_bytes(decoded[:4], 'big')\n"
        "request = json.loads(decoded[4:4 + length])\n"
        "payload = request.get('payload') or {}\n"
        f"with open({str(log)!r}, 'a') as log:\n"
        "    log.write(json.dumps({'bytes': len(raw), 'seeds': len(payload.get('seeds', [])), "
        "'cached': payload.get('cached_seeds', []), 'payload': bool(payload)}) + '\\n')\n"
        f"if {mode!r} == 'race' and payload.get('cached_seeds') and not pathlib.Path({str(raced)!r}).exists():\n"
        f"    shutil.rmtree(pathlib.Path({str(node.root)!r}) / 'projects' / request['project_id'] / 'repo' / 'cache')\n"
        f"    pathlib.Path({str(raced)!r}).touch()\n"
        f"if {mode!r} in ('truncated', 'oversize', 'path') and payload and not pathlib.Path({str(raced)!r}).exists():\n"
        f"    pathlib.Path({str(raced)!r}).touch()\n"
        f"    if {mode!r} == 'truncated': raw = raw[:-5]\n"
        "    else:\n"
        f"        if {mode!r} == 'oversize': payload['copies'][0]['size'] = 64 * 1024 * 1024 + 1\n"
        "        else: payload['copies'][0]['path'] = '../escape'\n"
        "        metadata = json.dumps(request).encode()\n"
        "        raw = gzip.compress(len(metadata).to_bytes(4, 'big') + metadata + decoded[4 + length:])\n"
        "result = subprocess.run([binary, *sys.argv[1:]], input=raw, stdout=subprocess.PIPE)\n"
        "output = json.loads(result.stdout)\n"
        "print(json.dumps(output))\n"
        "sys.exit(result.returncode)\n"
    )
    wrapper.chmod(0o755)
    config_path = mux_server.home_path / ".config/workmux/config.yaml"
    config = yaml.safe_load(config_path.read_text())
    config["remotes"]["lo"]["binary"] = str(wrapper)
    config_path.write_text(yaml.dump(config))

    repo = mux_repo_path
    commit_project(
        repo,
        {
            "panes": [{"command": "echo ready"}],
            "post_create": ["echo hooked > hook-ran"],
            "remote_files": {"symlink": ["cache"], "copy": ["copy.txt", "tree"]},
        },
    )
    # Local-only ignore rules must also apply when the seed is a cache hit.
    with (repo / ".git/info/exclude").open("a") as exclude:
        exclude.write("\ncache\ncopy.txt\ntree\nhook-ran\n")
    (repo / "cache").mkdir()
    (repo / "cache/data").write_text("first" * 200_000)
    (repo / "copy.txt").write_text("first")
    (repo / "tree/empty").mkdir(parents=True)
    binary = bytes(range(256)) * 4096
    (repo / "tree/executable").write_bytes(binary)
    (repo / "tree/executable").chmod(0o751)
    (repo / "tree/link").symlink_to("executable")
    (repo / "tree/dangling").symlink_to("absent")
    if mode in ("truncated", "oversize", "path"):
        failed = wm(
            mux_server, workmux_exe_path, repo, "add cold --remote lo", expect_fail=True
        )
        assert "partly set up" in failed.stderr
        [pending] = node.workspaces()
        assert not pending["progress"]["ready"]
        assert not pending["progress"]["files"]
        assert not (Path(pending["repo_path"]) / "cache").exists()
        assert not (Path(pending["worktree_path"]) / "hook-ran").exists()
    wm(mux_server, workmux_exe_path, repo, "add cold --remote lo")
    [first] = node.workspaces()
    seed = Path(first["repo_path"]) / "cache"
    assert (seed / "data").read_text() == "first" * 200_000
    copied = Path(first["worktree_path"]) / "tree"
    assert (copied / "executable").read_bytes() == binary
    assert (copied / "executable").stat().st_mode & 0o777 == 0o751
    assert (copied / "empty").is_dir()
    assert (copied / "link").readlink() == Path("executable")
    assert (copied / "dangling").readlink() == Path("absent")

    (repo / "cache/data").write_text("second")
    (repo / "copy.txt").write_text("second")
    wm(mux_server, workmux_exe_path, repo, "add warm --remote lo")
    second = next(ws for ws in node.workspaces() if ws["handle"] == "warm")
    worktree = Path(second["worktree_path"])
    assert (worktree / "cache").is_symlink()
    assert (worktree / "copy.txt").read_text() == "second"
    assert git(worktree, "status", "--porcelain") == "?? hook-ran"
    expected = "second" if mode == "race" else "first" * 200_000
    assert (worktree / "cache/data").read_text() == expected
    requests = [json.loads(line) for line in log.read_text().splitlines()]
    payloads = [request for request in requests if request["payload"]]
    assert payloads[0]["seeds"] == 2
    assert payloads[0]["bytes"] < 40_000
    warm = payloads[-2] if mode == "race" else payloads[-1]
    assert warm["seeds"] == 0
    assert warm["cached"] == ["cache"]
    assert warm["bytes"] < 30_000
    if mode == "race":
        assert len(payloads) == 3
        assert payloads[2]["seeds"] == 2

    # Eviction before preparation is an ordinary miss, not a failed add.
    shutil.rmtree(seed)
    wm(mux_server, workmux_exe_path, repo, "add evicted --remote lo")
    assert (seed / "data").read_text() == "second"


def test_config_relative_ignore_classification(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
):
    repo = mux_repo_path
    app = repo / "app"
    app.mkdir()
    (repo / ".gitignore").write_text("*.secret\n!keep.secret\n")
    (app / "tracked.secret").write_text("tracked")
    git(repo, "add", ".gitignore")
    git(repo, "add", "-f", "app/tracked.secret")
    commit_project(
        app,
        {
            "panes": [{"command": "echo ready"}],
            "remote_files": {"copy": ["*.secret", "local*", "global*"]},
        },
    )
    with (repo / ".git/info/exclude").open("a") as exclude:
        exclude.write("\nlocal*\n")
    global_exclude = mux_server.home_path / "global-ignore"
    global_exclude.write_text("global*\n")
    git(repo, "config", "core.excludesFile", str(global_exclude))
    for name in [
        "ignored.secret",
        "keep.secret",
        "local space",
        'local"quote',
        "local\ttab",
        "global file",
    ]:
        (app / name).write_text(name)
    wm(mux_server, workmux_exe_path, app, "add classified --remote lo")
    [workspace] = node.workspaces()
    tree = Path(workspace["worktree_path"])
    for name in [
        "tracked.secret",
        "ignored.secret",
        "keep.secret",
        "local space",
        'local"quote',
        "local\ttab",
        "global file",
    ]:
        assert (tree / "app" / name).read_text() == (app / name).read_text()
    assert git(tree, "status", "--porcelain") == "?? app/keep.secret"
    exclude = Path(
        git(tree, "rev-parse", "--path-format=absolute", "--git-path", "info/exclude")
    ).read_text()
    assert "/app/local space\n" in exclude
    assert "/app/global file\n" in exclude
    for name in ["tracked.secret", "keep.secret", "ignored.secret"]:
        assert f"/app/{name}\n" not in exclude


@pytest.mark.parametrize("selection", [None, {}, {"copy": [], "symlink": []}])
def test_local_files_do_not_upload_or_reuse_remote_seeds(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    selection,
):
    repo = mux_repo_path
    (repo / ".gitignore").write_text("cache\nCLAUDE.local.md\n")
    git(repo, "add", ".gitignore")
    (repo / "cache").mkdir()
    (repo / "cache/data").write_text("shared")
    (repo / "CLAUDE.local.md").write_text("local instructions")
    project = {
        "panes": [{"command": "echo ready"}],
        "files": {"symlink": ["cache"], "copy": ["CLAUDE.local.md"]},
        "remote_files": {"symlink": ["cache"]},
    }
    commit_project(repo, project)
    wm(mux_server, workmux_exe_path, repo, "add seeded --remote lo")
    [seeded] = node.workspaces()
    seed = Path(seeded["repo_path"]) / "cache"
    assert (seed / "data").read_text() == "shared"
    assert not (Path(seeded["worktree_path"]) / "CLAUDE.local.md").exists()

    if selection is None:
        del project["remote_files"]
    else:
        project["remote_files"] = selection
    commit_project(repo, project)
    result = wm(mux_server, workmux_exe_path, repo, "add unselected --remote lo")
    workspace = next(ws for ws in node.workspaces() if ws["handle"] == "unselected")
    tree = Path(workspace["worktree_path"])
    assert not (tree / "cache").exists()
    assert not (tree / "cache").is_symlink()
    assert not (tree / "CLAUDE.local.md").exists()
    assert (seed / "data").read_text() == "shared"
    assert ("files: local only" in result.stderr) == (
        selection != {"copy": [], "symlink": []}
    )

    wm(mux_server, workmux_exe_path, repo, "add local --local")
    local = repo.parent / f"{repo.name}__worktrees" / "local"
    assert (local / "cache").is_symlink()
    assert (local / "CLAUDE.local.md").read_text() == "local instructions"
