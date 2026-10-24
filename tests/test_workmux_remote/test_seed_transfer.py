"""Seed negotiation through real add commands and an instrumented loopback node."""

import json
import shutil
import sys
from pathlib import Path

import pytest
import yaml

from ..conftest import TmuxEnvironment
from .conftest import Node, commit_project, git, wm

pytestmark = pytest.mark.tmux_only


@pytest.mark.parametrize("mode", ["warm", "race", "legacy"])
def test_seed_transfer_negotiation(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
    mode: str,
):
    """Warm adds omit seeds; cache eviction retries and old nodes transfer fully."""
    log = tmp_path / "requests.jsonl"
    raced = tmp_path / "raced"
    wrapper = tmp_path / "node-wrapper"
    wrapper.write_text(
        f"#!{sys.executable}\n"
        "import json, os, pathlib, shutil, subprocess, sys\n"
        f"binary = {str(workmux_exe_path)!r}\n"
        "if sys.argv[1:3] != ['node', 'provision-workspace']:\n"
        "    os.execv(binary, [binary, *sys.argv[1:]])\n"
        "raw = sys.stdin.buffer.read()\n"
        "request = json.loads(raw)\n"
        "payload = request.get('payload') or {}\n"
        f"with open({str(log)!r}, 'a') as log:\n"
        "    log.write(json.dumps({'bytes': len(raw), 'seeds': len(payload.get('seeds', [])), "
        "'cached': payload.get('cached_seeds', []), 'payload': bool(payload)}) + '\\n')\n"
        f"if {mode!r} == 'race' and payload.get('cached_seeds') and not pathlib.Path({str(raced)!r}).exists():\n"
        f"    shutil.rmtree(pathlib.Path({str(node.root)!r}) / 'projects' / request['project_id'] / 'repo' / 'cache')\n"
        f"    pathlib.Path({str(raced)!r}).touch()\n"
        "result = subprocess.run([binary, *sys.argv[1:]], input=raw, stdout=subprocess.PIPE)\n"
        "output = json.loads(result.stdout)\n"
        f"if {mode!r} == 'legacy':\n"
        "    output.pop('cached_seeds', None)\n"
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
            "files": {"symlink": ["cache"], "copy": ["copy.txt"]},
        },
    )
    # Local-only ignore rules must also apply when the seed is a cache hit.
    with (repo / ".git/info/exclude").open("a") as exclude:
        exclude.write("\ncache\ncopy.txt\n")
    (repo / "cache").mkdir()
    (repo / "cache/data").write_text("first" * 200_000)
    (repo / "copy.txt").write_text("first")
    wm(mux_server, workmux_exe_path, repo, "add cold --remote lo")
    [first] = node.workspaces()
    seed = Path(first["repo_path"]) / "cache"
    assert (seed / "data").read_text() == "first" * 200_000

    (repo / "cache/data").write_text("second")
    (repo / "copy.txt").write_text("second")
    wm(mux_server, workmux_exe_path, repo, "add warm --remote lo")
    second = next(ws for ws in node.workspaces() if ws["handle"] == "warm")
    worktree = Path(second["worktree_path"])
    assert (worktree / "cache").is_symlink()
    assert (worktree / "copy.txt").read_text() == "second"
    assert git(worktree, "status", "--porcelain") == ""
    expected = "second" if mode == "race" else "first" * 200_000
    assert (worktree / "cache/data").read_text() == expected
    requests = [json.loads(line) for line in log.read_text().splitlines()]
    payloads = [request for request in requests if request["payload"]]
    assert payloads[0]["seeds"] == 2
    assert payloads[0]["bytes"] > 1_000_000
    if mode == "legacy":
        assert payloads[1]["seeds"] == 2
        assert payloads[1]["cached"] == []
    else:
        assert payloads[1]["seeds"] == 0
        assert payloads[1]["cached"] == ["cache"]
        assert payloads[1]["bytes"] < 10_000
        if mode == "race":
            assert len(payloads) == 3
            assert payloads[2]["seeds"] == 2

    # Eviction before preparation is an ordinary miss, not a failed add.
    shutil.rmtree(seed)
    wm(mux_server, workmux_exe_path, repo, "add evicted --remote lo")
    assert (seed / "data").read_text() == "second"
