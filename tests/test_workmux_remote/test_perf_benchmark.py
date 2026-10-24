"""Opt-in isolated release benchmark; see WORKMUX_PERF_* below."""

import json
import os
import random
import shutil
import sys
import time
from pathlib import Path

import pytest
import yaml

from ..conftest import TmuxEnvironment
from .conftest import Node, commit_project, git, wm

pytestmark = [
    pytest.mark.tmux_only,
    pytest.mark.skipif(
        not os.environ.get("WORKMUX_PERF_CLIENT"), reason="opt-in benchmark"
    ),
]


@pytest.fixture(scope="session")
def workmux_exe_path() -> Path:
    return Path(os.environ["WORKMUX_PERF_CLIENT"])


def test_remote_transfer_benchmark(
    mux_server: TmuxEnvironment,
    workmux_exe_path: Path,
    mux_repo_path: Path,
    node: Node,
    tmp_path: Path,
):
    """CLIENT/NODE select binaries; RATE caps each upload stream in bytes/sec."""
    binary = os.environ.get("WORKMUX_PERF_NODE", str(workmux_exe_path))
    rate = int(os.environ.get("WORKMUX_PERF_RATE", "0"))
    log = tmp_path / "requests.jsonl"
    wire = tmp_path / "git-wire.jsonl"
    wrapper = tmp_path / "node-wrapper"
    wrapper.write_text(
        f"#!{sys.executable}\n"
        "import json, os, subprocess, sys, time\n"
        f"binary = {binary!r}\n"
        "if sys.argv[1:3] != ['node', 'provision-workspace']:\n"
        "    os.execv(binary, [binary, *sys.argv[1:]])\n"
        "chunks = []; size = 0; start = time.monotonic()\n"
        "while True:\n"
        "    chunk = sys.stdin.buffer.read1(65536)\n"
        "    if not chunk: break\n"
        "    chunks.append(chunk); size += len(chunk)\n"
        f"    if {rate}: time.sleep(max(0, size / {rate or 1} - (time.monotonic() - start)))\n"
        "raw = b''.join(chunks)\n"
        f"with open({str(log)!r}, 'a') as log: log.write(json.dumps(size) + '\\n')\n"
        "sys.exit(subprocess.run([binary, *sys.argv[1:]], input=raw).returncode)\n"
    )
    wrapper.chmod(0o755)
    bindir = tmp_path / "bin"
    bindir.mkdir()
    receiver = bindir / "git-receive-pack"
    real_receiver = shutil.which("git-receive-pack")
    assert real_receiver
    receiver.write_text(
        f"#!{sys.executable}\n"
        "import json, subprocess, sys, threading, time\n"
        f"p = subprocess.Popen([{real_receiver!r}, *sys.argv[1:]], stdin=subprocess.PIPE)\n"
        "size = 0; prefix = bytearray()\n"
        "def pump():\n"
        "    global size\n"
        "    start = time.monotonic()\n"
        "    while True:\n"
        "        chunk = sys.stdin.buffer.read1(65536)\n"
        "        if not chunk: break\n"
        "        if len(prefix) < 4096: prefix.extend(chunk[:4096 - len(prefix)])\n"
        "        size += len(chunk)\n"
        f"        if {rate}: time.sleep(max(0, size / {rate or 1} - (time.monotonic() - start)))\n"
        "        p.stdin.write(chunk); p.stdin.flush()\n"
        "    p.stdin.close()\n"
        "threading.Thread(target=pump, daemon=True).start()\n"
        "code = p.wait()\n"
        f"with open({str(wire)!r}, 'a') as log: log.write(json.dumps(dict(bytes=size, pack_bytes=size - prefix.index(b'PACK') if b'PACK' in prefix else 0)) + '\\n')\n"
        "sys.exit(code)\n"
    )
    receiver.chmod(0o755)
    # Git prepends its exec-path to PATH before invoking GIT_SSH_COMMAND.
    git_shim = bindir / "git"
    git_shim.write_text(
        f"#!{sys.executable}\n"
        "import os, sys\n"
        f"os.execv({shutil.which('git')!r}, ['git', '--exec-path={bindir}', *sys.argv[1:]])\n"
    )
    git_shim.chmod(0o755)
    config_path = mux_server.home_path / ".config/workmux/config.yaml"
    config = yaml.safe_load(config_path.read_text())
    config["remotes"]["lo"]["binary"] = str(wrapper)
    config_path.write_text(yaml.dump(config))
    repo = mux_repo_path
    commit_project(
        repo,
        {
            "panes": [{"command": "echo READY"}],
            "files": {"symlink": ["history", "tests/venv"]},
        },
    )
    with (repo / ".git/info/exclude").open("a") as exclude:
        exclude.write("\nhistory\ntests/venv\n")
    rng = random.Random(330)
    for name, count in [("history", 80), ("tests/venv", 480)]:
        path = repo / name
        path.mkdir(parents=True)
        for i in range(count):
            (path / str(i)).write_bytes(rng.randbytes(65536))
    (repo / "blob").write_bytes(rng.randbytes(30 * 1024 * 1024))
    git(repo, "add", "blob")
    git(repo, "commit", "-m", "large history")
    for i in range(40):
        (repo / "revision").write_text(f"revision {i}\n")
        git(repo, "add", "revision")
        git(repo, "commit", "-m", f"revision {i}")
    env = {"PATH": f"{bindir}:{mux_server.env['PATH']}"}

    def run(command: str):
        return wm(
            mux_server, workmux_exe_path, repo, command, extra_env=env, timeout=180
        )

    def total(path: Path, key: str = "bytes") -> int:
        return (
            sum(
                (value[key] if isinstance(value := json.loads(line), dict) else value)
                for line in path.read_text().splitlines()
            )
            if path.exists()
            else 0
        )

    def add(label: str, handle: str):
        before_pack = total(wire, "pack_bytes")
        before_request, before_wire = total(log), total(wire)
        started = time.monotonic()
        run(f"add {handle} --remote lo")
        seconds = time.monotonic() - started
        assert total(log) > before_request
        assert total(wire) > before_wire, "Git transfer instrumentation was bypassed"
        workspace = next(w for w in node.workspaces() if w["handle"] == handle)
        remote = Path(workspace["repo_path"])
        assert git(Path(workspace["worktree_path"]), "rev-parse", "HEAD") == git(
            repo, "rev-parse", "HEAD"
        )
        git(remote, "config", "receive.unpackLimit", "1")
        print(
            json.dumps(
                {
                    "phase": label,
                    "seconds": round(seconds, 3),
                    "request_bytes": total(log) - before_request,
                    "git_input_bytes": total(wire) - before_wire,
                    "pack_bytes": total(wire, "pack_bytes") - before_pack,
                    "rate": rate,
                }
            ),
            flush=True,
        )

    add("cold", "cold")
    add("warm", "warm")
    run("remove --force cold warm")
    add("readd", "again")
    run("remove --force again")
    (repo / "revision").write_text("advanced\n")
    git(repo, "add", "revision")
    git(repo, "commit", "-m", "advance")
    add("advanced", "advanced")
    run("remove --force advanced")
    assert node.workspaces() == []
