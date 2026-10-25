"""Node bootstrap through the loopback transport: the same scripts the SSH
transport runs on a remote, executed by the local `sh`."""

import json
import os
import shutil
import subprocess
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


def test_remote_cli_ignores_host_agent_installations(
    tmp_path: Path, workmux_exe_path: Path, monkeypatch: pytest.MonkeyPatch
):
    host = tmp_path / "host-agents"
    host.mkdir()
    write_script(host / "agy", "exit 1\n")
    config = host / "opencode.json"
    config.write_text("{}")
    for name in (
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "COPILOT_HOME",
        "COPILOT_CONFIG_DIR",
        "GEMINI_CLI_HOME",
        "GROK_HOME",
        "PI_CODING_AGENT_DIR",
        "PI_CONFIG_DIR",
        "OPENCODE_CONFIG_DIR",
    ):
        monkeypatch.setenv(name, str(host))
    monkeypatch.setenv("OPENCODE_CONFIG", str(config))
    monkeypatch.setenv("WORKMUX_TEST_AGY_DETECT", "1")
    monkeypatch.setenv("PATH", f"{host}:{os.environ['PATH']}")
    cli = RemoteCli(tmp_path, workmux_exe_path, loopback=True)
    (cli.home / "node").mkdir()
    cli.env["WORKMUX_NODE_ROOT"] = str(cli.home / "node")

    result = cli.run(
        "node",
        "status-hooks",
        "--json",
        input=json.dumps(
            {"isolation": "none", "install": True, "opencode_image": "unused"}
        ),
    )

    assert result.returncode == 0, result.stdout + result.stderr
    assert json.loads(result.stdout)["agents"] == []
    assert sorted(p.name for p in host.iterdir()) == ["agy", "opencode.json"]


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


def test_setup_installs_status_hooks_where_containers_mount_them(
    loopback_cli: RemoteCli, workmux_exe_path: Path
):
    configure_loopback(loopback_cli, workmux_exe_path)
    subprocess.run(["git", "init", "-q", str(loopback_cli.cwd)], check=True)
    (loopback_cli.cwd / ".workmux.yaml").write_text("sandbox:\n  enabled: true\n")
    home = loopback_cli.home

    result = loopback_cli.run("remote", "setup", "lo")

    assert result.returncode == 0, result.stderr
    assert "installed status hooks for Claude Code, Codex, Gemini CLI" in result.stdout
    for settings in [".claude/settings.json", ".gemini/settings.json"]:
        hooks = json.loads((home / settings).read_text())["hooks"]
        assert "workmux set-window-status" in json.dumps(hooks), settings
    assert "workmux set-window-status" in (home / ".codex/hooks.json").read_text()
    assert "hooks = true" in (home / ".codex/config.toml").read_text()
    for agent_dir in [".pi/agent", ".omp/agent"]:
        assert (home / agent_dir / "extensions/workmux-status.ts").is_file()
    claude = (home / ".claude/settings.json").stat()

    again = loopback_cli.run("remote", "setup", "lo")

    assert again.returncode == 0, again.stderr
    assert "status hooks are already installed for Claude Code" in again.stdout
    assert "installed status hooks" not in again.stdout
    assert (home / ".claude/settings.json").stat().st_mtime_ns == claude.st_mtime_ns


def test_doctor_reports_status_hooks(loopback_cli: RemoteCli, workmux_exe_path: Path):
    loopback_cli.write_config(
        {
            "lo": {
                "host": "localhost",
                "path": "~/node",
                "binary": str(workmux_exe_path),
                "isolation": "none",
            }
        }
    )
    settings = loopback_cli.home / ".claude" / "settings.json"
    settings.parent.mkdir()
    assert loopback_cli.run("remote", "setup", "lo").returncode == 0
    assert "workmux set-window-status" in settings.read_text()

    ready = loopback_cli.run("remote", "doctor", "lo")
    settings.write_text('{"theme": "dark"}\n')
    missing = loopback_cli.run("remote", "doctor", "lo")

    assert "✓ hooks      installed for Claude Code" in ready.stdout, ready.stdout
    assert "! hooks      missing for Claude Code" in missing.stdout, missing.stdout
    assert "fix: workmux remote setup lo" in missing.stdout
    assert loopback_cli.run("remote", "setup", "lo").returncode == 0
    assert json.loads(settings.read_text())["theme"] == "dark"
    assert "✓ hooks" in loopback_cli.run("remote", "doctor", "lo").stdout


@pytest.mark.parametrize(
    "program,expected,success",
    [
        (None, "skipped OpenCode status hooks: opencode is not installed", True),
        ("echo version-failed >&2\nexit 7\n", "opencode --version failed", False),
        ("echo 99.0.0\n", "Unsupported OpenCode version", False),
        ("echo 2.0.1\n", "installed status hooks for OpenCode", True),
    ],
)
def test_host_setup_opencode_requires_an_installed_version(
    loopback_cli: RemoteCli,
    workmux_exe_path: Path,
    program: str | None,
    expected: str,
    success: bool,
):
    configure_loopback(loopback_cli, workmux_exe_path)
    home = loopback_cli.home
    config_dir = home / ".config/opencode"
    config_dir.mkdir(parents=True)
    sentinel = config_dir / "user-config.json"
    sentinel.write_text('{"keep": true}')
    fake_bin = home / "host-bin"
    fake_bin.mkdir()
    shell = write_script(
        home / "login-shell",
        f'export PATH={fake_bin}:/usr/bin:/bin\nshift\nexec /bin/sh "$@"\n',
    )
    loopback_cli.env["SHELL"] = str(shell)
    if program is not None:
        write_script(fake_bin / "opencode", program)

    result = loopback_cli.run("remote", "setup", "lo")

    assert (result.returncode == 0) == success, result.stdout + result.stderr
    assert expected in result.stdout + result.stderr
    assert sentinel.read_text() == '{"keep": true}'
    if program is None:
        assert sorted(p.name for p in config_dir.iterdir()) == ["user-config.json"]
    elif success:
        assert (config_dir / "plugins/workmux-status/index.ts").is_file()
