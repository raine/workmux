"""Agent logins on a loopback node: `remote login` runs the agent's login
command and keeps a setup token private, `--import` copies credential
files into the node user's agent directories, and `node credentials`
reports them. Agents are fake scripts; nothing touches real accounts."""

import json
import os
import subprocess
from pathlib import Path

from .conftest import RemoteCli, write_script

TOKEN = "sk-ant-oat01-fake-token"


def configure(cli: RemoteCli, binary: Path, **extra) -> Path:
    """A loopback remote with `isolation: none`, fake agents on the login
    shell's PATH; returns the node root."""
    cli.write_config(
        {
            "lo": {
                "host": "localhost",
                "path": "~/node",
                "binary": str(binary),
                "isolation": "none",
            }
        },
        **extra,
    )
    fake_bin = cli.home / "fake-bin"
    fake_bin.mkdir()
    write_script(
        fake_bin / "claude",
        'echo "claude-ran $*" >> "$HOME/agent-calls"\necho "Long-lived token: (fake)"\n',
    )
    write_script(
        fake_bin / "codex",
        'echo "codex-ran $* NO_BROWSER=$NO_BROWSER" >> "$HOME/agent-calls"\n',
    )
    # The node reads the login environment from `$SHELL -l`.
    shell = write_script(
        cli.home / "login-shell",
        f"PATH={fake_bin}:/usr/bin:/bin\nexport PATH\nexec env -0\n",
    )
    cli.env["SHELL"] = str(shell)
    return cli.home / "node"


def calls(cli: RemoteCli) -> list:
    path = cli.home / "agent-calls"
    return path.read_text().splitlines() if path.exists() else []


def mode(path: Path) -> int:
    return path.stat().st_mode & 0o777


def test_login_runs_the_agents_login_with_extra_args(
    loopback_cli: RemoteCli, workmux_exe_path: Path
):
    configure(loopback_cli, workmux_exe_path)
    result = loopback_cli.run("remote", "login", "lo", "codex", "--", "--device-auth")
    assert result.returncode == 0, result.stderr
    assert calls(loopback_cli) == ["codex-ran login --device-auth NO_BROWSER=true"]
    assert (
        "Forwarding localhost port(s)" in result.stdout
        or "is in use on this machine" in result.stderr
    )


def test_login_rejects_unknown_agents(loopback_cli: RemoteCli, workmux_exe_path: Path):
    configure(loopback_cli, workmux_exe_path)
    result = loopback_cli.run("remote", "login", "lo", "cursor")
    assert result.returncode != 0
    assert "remote logins support claude, codex" in result.stderr
    result = loopback_cli.run("remote", "login", "lo", "codex", "--setup-token")
    assert result.returncode != 0
    assert "--setup-token is for claude" in result.stderr


def test_setup_token_is_kept_private_and_never_printed(
    loopback_cli: RemoteCli, workmux_exe_path: Path
):
    root = configure(loopback_cli, workmux_exe_path)
    result = loopback_cli.run(
        "remote", "login", "lo", "claude", "--setup-token", input=f"{TOKEN}\n"
    )
    assert result.returncode == 0, result.stderr
    assert calls(loopback_cli) == ["claude-ran setup-token"]
    assert "Saved" in result.stderr
    assert TOKEN not in result.stdout + result.stderr

    secrets = root / "secrets"
    assert mode(secrets) == 0o700
    assert mode(secrets / "node.json") == 0o600
    stored = json.loads((secrets / "node.json").read_text())
    assert stored == {"CLAUDE_CODE_OAUTH_TOKEN": TOKEN}

    # An empty answer keeps the token.
    result = loopback_cli.run(
        "remote", "login", "lo", "claude", "--setup-token", input="\n"
    )
    assert result.returncode == 0, result.stderr
    assert "Kept the current token" in result.stderr
    assert json.loads((secrets / "node.json").read_text()) == stored

    report = node_credentials(loopback_cli, root, ["claude"])
    assert report == [{"agent": "claude", "found": ["the setup token"]}]


def node_credentials(cli: RemoteCli, root: Path, agents: list) -> list:
    env = {**cli.env, "WORKMUX_NODE_ROOT": str(root)}
    root.mkdir(exist_ok=True)
    result = subprocess.run(
        [str(cli.exe), "node", "credentials", "--json"],
        env=env,
        input=json.dumps({"agents": agents}),
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    return json.loads(result.stdout)["agents"]


def test_import_copies_dummy_credentials_into_the_node_users_dirs(
    loopback_cli: RemoteCli, workmux_exe_path: Path
):
    laptop = loopback_cli.home / "laptop-creds"
    root = configure(
        loopback_cli,
        workmux_exe_path,
        sandbox={"agent_config_dir": str(laptop / "{agent}")},
    )
    home = loopback_cli.home
    assert node_credentials(loopback_cli, root, ["codex", "gemini", "omp"]) == [
        {"agent": "codex", "found": []},
        {"agent": "gemini", "found": []},
        {"agent": "omp", "found": []},
    ]

    (laptop / "codex").mkdir(parents=True)
    (laptop / "codex" / "auth.json").write_text('{"tokens": {"fake": true}}')
    (laptop / "codex" / "config.toml").write_text("model = 'x'\n")
    result = loopback_cli.run("remote", "login", "lo", "codex", "--import")
    assert result.returncode == 0, result.stderr
    assert "one-time copy" in result.stdout
    imported = home / ".codex" / "auth.json"
    assert imported.read_text() == '{"tokens": {"fake": true}}'
    assert mode(imported) == 0o600
    assert not (home / ".codex" / "config.toml").exists()

    (laptop / "gemini").mkdir()
    (laptop / "gemini" / "oauth_creds.json").write_text('{"refresh_token": "fake"}')
    result = loopback_cli.run("remote", "login", "lo", "gemini", "--import")
    assert result.returncode == 0, result.stderr
    settings = json.loads((home / ".gemini" / "settings.json").read_text())
    assert settings["security"]["auth"]["selectedType"] == "oauth-personal"
    assert mode(home / ".gemini" / "oauth_creds.json") == 0o600

    report = node_credentials(loopback_cli, root, ["codex", "gemini"])
    assert report[0]["found"] == [str(imported)]
    assert report[1]["found"] == [str(home / ".gemini" / "oauth_creds.json")]

    result = loopback_cli.run("remote", "login", "lo", "omp", "--import")
    assert result.returncode != 0
    assert "agent.db" in result.stderr
    assert "workmux remote login lo omp" in result.stderr

    result = loopback_cli.run("remote", "login", "lo", "pi", "--import")
    assert result.returncode != 0
    assert "no pi credentials" in result.stderr
    assert not os.path.exists(home / ".pi" / "agent" / "auth.json")


def test_put_credentials_accepts_only_the_agents_files(
    loopback_cli: RemoteCli, tmp_path: Path
):
    root = tmp_path / "root"
    root.mkdir()
    result = subprocess.run(
        [str(loopback_cli.exe), "node", "put-credentials", "--json"],
        env={**loopback_cli.env, "WORKMUX_NODE_ROOT": str(root)},
        input=json.dumps(
            {"agent": "codex", "files": [{"name": "../.bashrc", "data": "eA=="}]}
        ),
        capture_output=True,
        text=True,
    )
    assert result.returncode == 1
    assert json.loads(result.stdout)["error"]["code"] == "bad_request"
    assert not (loopback_cli.home / ".bashrc").exists()
