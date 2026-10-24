"""Fixtures for `workmux remote` and `workmux node` tests."""

import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any, Dict, Iterator, List, Optional

import pytest
import yaml

from ..conftest import TmuxEnvironment, run_workmux_command


def agent_dirs_env(home: Path) -> Dict[str, str]:
    """Agent directory overrides inside `home`, so a loopback node installing
    status hooks never writes to the real agent config the environment names."""
    return {
        "CLAUDE_CONFIG_DIR": str(home / ".claude"),
        "CODEX_HOME": str(home / ".codex"),
        "GEMINI_CLI_HOME": str(home),
        "PI_CODING_AGENT_DIR": str(home / ".pi" / "agent"),
        "OPENCODE_CONFIG_DIR": str(home / ".config" / "opencode"),
    }


class RemoteCli:
    """Runs workmux with isolated HOME/XDG dirs and a generated global config."""

    def __init__(self, tmp_path: Path, exe: Path, loopback: bool):
        self.exe = exe
        self.home = tmp_path / "home"
        self.cwd = tmp_path / "work"
        self.home.mkdir()
        self.cwd.mkdir()
        self.env = os.environ.copy()
        self.env.update(
            {
                "HOME": str(self.home),
                "XDG_CONFIG_HOME": str(self.home / ".config"),
                "XDG_STATE_HOME": str(self.home / ".local" / "state"),
                "XDG_CACHE_HOME": str(self.home / ".cache"),
                **agent_dirs_env(self.home),
            }
        )
        # Containers mount pi's and omp's default directories, which
        # PI_CODING_AGENT_DIR would make one.
        for name in ("PI_CODING_AGENT_DIR", "PI_CONFIG_DIR"):
            self.env.pop(name, None)
        self.env.pop("TMUX", None)
        self.env.pop("TMUX_PANE", None)
        if loopback:
            self.env["WORKMUX_REMOTE_LOOPBACK"] = "1"
        else:
            self.env.pop("WORKMUX_REMOTE_LOOPBACK", None)

    def write_config(
        self,
        remotes: Dict[str, Dict[str, Any]],
        placement: Optional[str] = None,
        **extra: Any,
    ) -> None:
        config: Dict[str, Any] = {"nerdfont": False, "remotes": remotes, **extra}
        if placement is not None:
            config["placement"] = placement
        config_dir = self.home / ".config" / "workmux"
        config_dir.mkdir(parents=True, exist_ok=True)
        (config_dir / "config.yaml").write_text(yaml.dump(config))

    def run(
        self, *args: str, input: str = "", timeout: float = 120
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(self.exe), *args],
            cwd=self.cwd,
            env=self.env,
            input=input,
            capture_output=True,
            text=True,
            timeout=timeout,
        )


def dev_pin(binary: Path) -> str:
    """Pinned file name `workmux remote setup` uses for an uploaded binary."""
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    return f"workmux-dev-{digest[:12]}"


def write_script(path: Path, body: str) -> Path:
    path.write_text("#!/bin/sh\n" + body)
    path.chmod(0o755)
    return path


@pytest.fixture
def loopback_cli(tmp_path: Path, workmux_exe_path: Path) -> RemoteCli:
    return RemoteCli(tmp_path, workmux_exe_path, loopback=True)


LOOPBACK = {"WORKMUX_REMOTE_LOOPBACK": "1"}


class Node:
    """The loopback node: its root, records and tmux server."""

    def __init__(self, root: Path):
        self.root = root

    def tmux(self, *args: str) -> subprocess.CompletedProcess[str]:
        socket = self.root / "run" / "tmux.sock"
        return subprocess.run(
            ["tmux", "-S", str(socket), *args], capture_output=True, text=True
        )

    def sessions(self) -> List[str]:
        result = self.tmux("list-sessions", "-F", "#{session_name}")
        return sorted(result.stdout.split()) if result.returncode == 0 else []

    def workspaces(self) -> List[dict]:
        directory = self.root / "state" / "workspaces"
        if not directory.is_dir():
            return []
        return [
            json.loads(path.read_text())
            for path in sorted(directory.glob("w*.json"))
            if "." not in path.stem
        ]


@pytest.fixture
def node(mux_server: TmuxEnvironment, workmux_exe_path: Path) -> Iterator[Node]:
    # A short root keeps the node's tmux socket under the root.
    root = Path(tempfile.mkdtemp(prefix="wmn-", dir="/tmp")).resolve()
    config = {
        "nerdfont": False,
        "remotes": {
            "lo": {
                "host": "localhost",
                "path": str(root),
                "binary": str(workmux_exe_path),
                "isolation": "none",
            }
        },
    }
    config_dir = mux_server.home_path / ".config" / "workmux"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.yaml").write_text(yaml.dump(config))
    node = Node(root)
    yield node
    node.tmux("kill-server")
    shutil.rmtree(root, ignore_errors=True)


def git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=cwd, check=True, capture_output=True, text=True
    ).stdout.strip()


def commit_project(repo: Path, project: Dict[str, Any]) -> None:
    """Commit `project` as the repository's `.workmux.yaml`."""
    (repo / ".workmux.yaml").write_text(yaml.dump(project))
    git(repo, "add", ".workmux.yaml")
    git(repo, "commit", "-m", "configure the project")


def local_records(env: TmuxEnvironment) -> List[dict]:
    directory = env.home_path / ".local/state/workmux/remote/lo/workspaces"
    if not directory.is_dir():
        return []
    return [json.loads(p.read_text()) for p in sorted(directory.glob("*.json"))]


def wm(
    env: TmuxEnvironment,
    exe: Path,
    repo: Path,
    command: str,
    extra_env: Optional[Dict[str, str]] = None,
    **kwargs,
):
    """Run workmux from the runner pane; commands that focus a remote window
    would otherwise type the next command into the remote shell."""
    assert env.runner_pane_id is not None
    env.tmux(["select-window", "-t", env.runner_pane_id])
    kwargs.setdefault("timeout", 60)
    pre_run_env = {**LOOPBACK, **agent_dirs_env(env.home_path), **(extra_env or {})}
    return run_workmux_command(
        env, exe, repo, command, pre_run_env=pre_run_env, **kwargs
    )
