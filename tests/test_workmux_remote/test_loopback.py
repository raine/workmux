"""Node bootstrap through the loopback transport: the same scripts the SSH
transport runs on a remote, executed by the local `sh`."""

import json
import os
import shutil
import time
from pathlib import Path

import pytest

from .conftest import RemoteCli, dev_pin, write_script

FAKE_PROTOCOL_999 = (
    'printf \'%s\\n\' \'{"protocol":999,"version":"9.9.9","os":"linux",'
    '"arch":"aarch64","capabilities":["hello"]}\'\n'
)


def configure_loopback(cli: RemoteCli, binary: Path, name: str = "lo") -> Path:
    """Configure one loopback remote; returns its node `bin/` directory."""
    cli.write_config(
        {name: {"host": "localhost", "path": "~/node", "binary": str(binary)}}
    )
    return cli.home / "node" / "bin"


def test_node_hello_reports_protocol_version_and_platform(
    loopback_cli: RemoteCli,
):
    result = loopback_cli.run("node", "hello", "--json")
    assert result.returncode == 0, result.stderr
    assert result.stderr == ""
    hello = json.loads(result.stdout)
    version = loopback_cli.run("--version").stdout.split()[-1]
    assert hello["protocol"] == 1
    assert hello["version"] == version
    assert hello["os"] and hello["arch"]
    assert "hello" in hello["capabilities"]


def test_node_hello_accepts_request_object(loopback_cli: RemoteCli):
    result = loopback_cli.run("node", "hello", "--json", input='{"client": "x"}')
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout)["protocol"] == 1


def test_node_rejects_non_object_request(loopback_cli: RemoteCli):
    result = loopback_cli.run("node", "hello", "--json", input="[1]")
    assert result.returncode == 1
    error = json.loads(result.stdout)["error"]
    assert error["code"] == "bad_request"
    assert "JSON object" in error["message"]


def test_remote_list_without_remotes(loopback_cli: RemoteCli):
    result = loopback_cli.run("remote")
    assert result.returncode == 0, result.stderr
    assert "No remotes configured" in result.stdout


def test_setup_installs_pinned_binary(loopback_cli: RemoteCli, workmux_exe_path: Path):
    bin_dir = configure_loopback(loopback_cli, workmux_exe_path)

    result = loopback_cli.run("remote", "setup", "lo")

    assert result.returncode == 0, result.stderr
    assert "installed workmux" in result.stdout
    pinned = bin_dir / dev_pin(workmux_exe_path)
    assert pinned.read_bytes() == workmux_exe_path.read_bytes()
    assert pinned.stat().st_mode & 0o777 == 0o755
    assert sorted(p.name for p in bin_dir.iterdir()) == [pinned.name]


def test_setup_rerun_is_noop(loopback_cli: RemoteCli, workmux_exe_path: Path):
    bin_dir = configure_loopback(loopback_cli, workmux_exe_path)
    assert loopback_cli.run("remote", "setup", "lo").returncode == 0
    pinned = bin_dir / dev_pin(workmux_exe_path)
    before = pinned.stat()

    result = loopback_cli.run("remote", "setup", "lo")

    assert result.returncode == 0, result.stderr
    assert "already installed" in result.stdout
    after = pinned.stat()
    assert (after.st_ino, after.st_mtime_ns) == (before.st_ino, before.st_mtime_ns)
    assert sorted(p.name for p in bin_dir.iterdir()) == [pinned.name]


def test_setup_recovers_from_interrupted_installs(
    loopback_cli: RemoteCli, workmux_exe_path: Path
):
    """Leftovers of interrupted installs: an old partial stage file is removed,
    a recent one (possibly another install in progress) is kept, and a broken
    file under the pinned name is replaced."""
    bin_dir = configure_loopback(loopback_cli, workmux_exe_path)
    bin_dir.mkdir(parents=True)
    stale = bin_dir / ".workmux.0123456789abcdef"
    stale.write_bytes(workmux_exe_path.read_bytes()[:4096])
    two_hours_ago = time.time() - 2 * 3600
    os.utime(stale, (two_hours_ago, two_hours_ago))
    fresh = bin_dir / ".workmux.fedcba9876543210"
    fresh.write_bytes(b"partial")
    pinned = bin_dir / dev_pin(workmux_exe_path)
    write_script(pinned, "exit 3\n")

    result = loopback_cli.run("remote", "setup", "lo")

    assert result.returncode == 0, result.stderr
    assert "installed workmux" in result.stdout
    assert pinned.read_bytes() == workmux_exe_path.read_bytes()
    assert not stale.exists()
    assert fresh.exists()


def test_setup_rejects_protocol_mismatch(loopback_cli: RemoteCli, tmp_path: Path):
    fake = write_script(tmp_path / "workmux-999", FAKE_PROTOCOL_999)
    bin_dir = configure_loopback(loopback_cli, fake)

    result = loopback_cli.run("remote", "setup", "lo")

    assert result.returncode == 1
    assert "speaks protocol 999 (workmux 9.9.9)" in result.stderr
    assert "speaks protocol 1" in result.stderr
    assert "remotes.lo.binary" in result.stderr
    assert list(bin_dir.iterdir()) == []


def test_installed_protocol_mismatch_is_reported(
    loopback_cli: RemoteCli, tmp_path: Path
):
    fake = write_script(tmp_path / "workmux-999", FAKE_PROTOCOL_999)
    bin_dir = configure_loopback(loopback_cli, fake)
    bin_dir.mkdir(parents=True)
    pinned = bin_dir / dev_pin(fake)
    write_script(pinned, FAKE_PROTOCOL_999)

    setup = loopback_cli.run("remote", "setup", "lo")
    listing = loopback_cli.run("remote")

    assert setup.returncode == 1
    assert "speaks protocol 999" in setup.stderr
    assert pinned.read_text().endswith(FAKE_PROTOCOL_999)
    assert "protocol mismatch" in listing.stdout


def test_setup_rejects_binary_that_is_not_a_node(
    loopback_cli: RemoteCli, tmp_path: Path
):
    fake = write_script(tmp_path / "not-workmux", "echo hello world\n")
    bin_dir = configure_loopback(loopback_cli, fake)

    result = loopback_cli.run("remote", "setup", "lo")

    assert result.returncode == 1
    assert "does not run as workmux" in result.stderr
    assert "hello world" in result.stderr
    assert list(bin_dir.iterdir()) == []


def test_setup_rejects_workmux_without_node_command(
    loopback_cli: RemoteCli, tmp_path: Path
):
    fake = write_script(
        tmp_path / "old-workmux",
        "echo \"error: unrecognized subcommand 'node'\" >&2\nexit 2\n",
    )
    bin_dir = configure_loopback(loopback_cli, fake)

    result = loopback_cli.run("remote", "setup", "lo")

    assert result.returncode == 1
    assert "predates remote nodes" in result.stderr
    assert list(bin_dir.iterdir()) == []


def test_setup_reports_unwritable_node_root(
    loopback_cli: RemoteCli, workmux_exe_path: Path
):
    if os.geteuid() == 0:
        pytest.skip("root can write anywhere")
    locked = loopback_cli.home / "locked"
    locked.mkdir(mode=0o555)
    loopback_cli.write_config(
        {
            "lo": {
                "host": "localhost",
                "path": "~/locked/node",
                "binary": str(workmux_exe_path),
            }
        }
    )

    result = loopback_cli.run("remote", "setup", "lo")

    assert result.returncode == 1
    assert "Failed to stage the upload in ~/locked/node/bin on lo" in result.stderr
    assert "binary must be" not in result.stderr
    assert not (locked / "node").exists()


def test_setup_refuses_binary_replaced_during_setup(
    loopback_cli: RemoteCli, tmp_path: Path
):
    """The pin name is the hash of the configured binary when setup starts;
    a binary rebuilt before the upload must not be installed under it."""
    hello_v1 = (
        'printf \'%s\\n\' \'{"protocol":1,"version":"VERSION","os":"linux",'
        '"arch":"aarch64","capabilities":["hello"]}\'\n'
    )
    first = write_script(tmp_path / "workmux", hello_v1.replace("VERSION", "1.0.0"))
    rebuilt = write_script(tmp_path / "rebuilt", hello_v1.replace("VERSION", "1.0.1"))
    original_pin = dev_pin(first)
    bin_dir = configure_loopback(loopback_cli, first)
    # setup runs `uname` on the remote after computing the pin and before the
    # upload; replace the binary there, as `just remote-dev-binary` does.
    fake_bin = tmp_path / "fake-bin"
    fake_bin.mkdir()
    real_uname = shutil.which("uname")
    write_script(
        fake_bin / "uname",
        f'cp "{rebuilt}" "{first}.tmp" && mv -f "{first}.tmp" "{first}"\n'
        f'exec "{real_uname}" "$@"\n',
    )
    loopback_cli.env["PATH"] = f"{fake_bin}:{loopback_cli.env['PATH']}"

    result = loopback_cli.run("remote", "setup", "lo")

    assert result.returncode == 1
    assert "remotes.lo.binary changed while setup was running" in result.stderr
    assert not (bin_dir / original_pin).exists()


def test_remote_list_reports_each_remote(
    loopback_cli: RemoteCli, workmux_exe_path: Path
):
    loopback_cli.write_config(
        {
            "ready": {
                "host": "localhost",
                "path": "~/a",
                "binary": str(workmux_exe_path),
            },
            "fresh": {
                "host": "localhost",
                "path": "~/b",
                "binary": str(workmux_exe_path),
            },
            "broken": {"host": "-oProxyCommand=x"},
        },
        placement="ready",
    )
    assert loopback_cli.run("remote", "setup", "ready").returncode == 0

    result = loopback_cli.run("remote")

    assert result.returncode == 0, result.stderr
    lines = {line.split()[0]: line for line in result.stdout.splitlines()[1:]}
    assert "ready (workmux" in lines["ready"]
    assert "(default)" in lines["ready"]
    assert "not set up" in lines["fresh"]
    assert "invalid config" in lines["broken"]


def test_setup_unknown_remote(loopback_cli: RemoteCli, workmux_exe_path: Path):
    configure_loopback(loopback_cli, workmux_exe_path)
    result = loopback_cli.run("remote", "setup", "nope")
    assert result.returncode == 1
    assert "Unknown remote 'nope' (configured: lo)" in result.stderr
