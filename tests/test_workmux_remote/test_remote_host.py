"""End-to-end node bootstrap over real SSH.

Runs only when WORKMUX_REMOTE_TEST_HOST names an ssh destination that logs in
without prompting. WORKMUX_REMOTE_TEST_BINARY must be a Linux workmux built
from this tree for that host (`just remote-dev-binary <host>`). Each test
uses its own node root under the remote home and removes it afterwards.
"""

import os
import signal
import subprocess
import time
import uuid
from pathlib import Path
from typing import Iterator

import pytest
import yaml

from .conftest import RemoteCli, dev_pin, write_script

HOST = os.environ.get("WORKMUX_REMOTE_TEST_HOST")
BINARY = os.environ.get("WORKMUX_REMOTE_TEST_BINARY")

pytestmark = [
    pytest.mark.skipif(not HOST, reason="WORKMUX_REMOTE_TEST_HOST is not set"),
    pytest.mark.skipif(
        bool(HOST) and not BINARY,
        reason="WORKMUX_REMOTE_TEST_BINARY is not set; build one with "
        "`just remote-dev-binary $WORKMUX_REMOTE_TEST_HOST`",
    ),
]


def remote_sh(command: str, timeout: float = 60) -> subprocess.CompletedProcess[str]:
    assert HOST
    return subprocess.run(
        ["ssh", "-o", "BatchMode=yes", "--", HOST, command],
        capture_output=True,
        text=True,
        timeout=timeout,
        stdin=subprocess.DEVNULL,
    )


class Node:
    def __init__(self, cli: RemoteCli, root: str, binary: Path):
        self.cli = cli
        self.root = root
        self.binary = binary
        self.pin = f"{root}/bin/{dev_pin(binary)}"

    def configure(self, binary: Path) -> None:
        assert HOST
        self.cli.write_config(
            {
                "e2e": {
                    "host": HOST,
                    "path": f"~/{self.root}",
                    "binary": str(binary),
                    "isolation": "container",
                }
            }
        )

    def ls_bin(self) -> list[str]:
        result = remote_sh(f"ls -A {self.root}/bin 2>/dev/null || true")
        return sorted(result.stdout.split())


@pytest.fixture
def node(tmp_path: Path, workmux_exe_path: Path) -> Iterator[Node]:
    assert BINARY
    binary = Path(BINARY)
    root = f".workmux-node-e2e-{uuid.uuid4().hex[:8]}"
    cli = RemoteCli(tmp_path, workmux_exe_path, loopback=False)
    node = Node(cli, root, binary)
    node.configure(binary)
    try:
        yield node
    finally:
        remote_sh(f"rm -rf -- {root}")


def test_setup_installs_then_reruns_as_noop(node: Node):
    first = node.cli.run("remote", "setup", "e2e")
    assert first.returncode == 0, first.stderr
    assert "installed workmux" in first.stdout
    before = remote_sh(f"stat -c '%i %Y' {node.pin}").stdout

    second = node.cli.run("remote", "setup", "e2e")

    assert second.returncode == 0, second.stderr
    assert "already installed" in second.stdout
    assert remote_sh(f"stat -c '%i %Y' {node.pin}").stdout == before
    assert node.ls_bin() == [Path(node.pin).name]
    hello = remote_sh(f"{node.pin} node hello --json </dev/null")
    assert '"protocol":1' in hello.stdout


@pytest.mark.parametrize("host_mode", [False, True])
def test_doctor_is_green_after_setup(node: Node, host_mode: bool):
    assert node.cli.run("remote", "setup", "e2e").returncode == 0

    if host_mode:
        config_path = node.cli.home / ".config/workmux/config.yaml"
        config = yaml.safe_load(config_path.read_text())
        del config["remotes"]["e2e"]["isolation"]
        config_path.write_text(yaml.safe_dump(config))

    result = node.cli.run("remote", "doctor", "e2e", timeout=600)

    assert result.returncode == 0, result.stdout + result.stderr
    checks = [
        "ssh",
        "arch",
        "node",
        "git",
        "tmux",
        "linger",
        "agents",
        "hooks",
    ]
    for check in checks:
        assert f"✓ {check}" in result.stdout
    if host_mode:
        assert "✓ agent " in result.stdout
        assert "run on host (sandbox.enabled is off)" in result.stdout
        assert "not needed for host agents" in result.stdout
        assert "image " not in result.stdout
    else:
        assert "✓ container" in result.stdout
        assert "✓ image" in result.stdout


def test_listing_shows_ready_node(node: Node):
    assert node.cli.run("remote", "setup", "e2e").returncode == 0
    result = node.cli.run("remote")
    assert result.returncode == 0, result.stderr
    assert "ready (workmux" in result.stdout


def test_protocol_mismatch_is_clear(node: Node, tmp_path: Path):
    fake = write_script(
        tmp_path / "workmux-999",
        'printf \'%s\\n\' \'{"protocol":999,"version":"9.9.9","os":"linux",'
        '"arch":"x","capabilities":[]}\'\n',
    )
    node.configure(fake)

    result = node.cli.run("remote", "setup", "e2e")

    assert result.returncode == 1
    assert "speaks protocol 999 (workmux 9.9.9)" in result.stderr
    assert node.ls_bin() == []


def test_interrupted_setup_recovers(node: Node):
    proc = subprocess.Popen(
        [str(node.cli.exe), "remote", "setup", "e2e"],
        cwd=node.cli.cwd,
        env=node.cli.env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL,
        start_new_session=True,
    )
    # Interrupt like Ctrl-C (the whole process group) once the upload started.
    deadline = time.time() + 60
    while time.time() < deadline and proc.poll() is None:
        if any(name.startswith(".workmux.") for name in node.ls_bin()):
            break
        time.sleep(0.05)
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGINT)
    proc.communicate(timeout=60)
    assert proc.returncode != 0, "setup finished before it could be interrupted"
    assert Path(node.pin).name not in node.ls_bin()

    result = node.cli.run("remote", "setup", "e2e")

    assert result.returncode == 0, result.stderr
    assert "installed workmux" in result.stdout
    sums = remote_sh(f"sha256sum {node.pin}").stdout.split()
    assert sums and sums[0].startswith(
        dev_pin(node.binary).removeprefix("workmux-dev-")
    )
