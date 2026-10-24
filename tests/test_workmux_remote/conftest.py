"""Fixtures for `workmux remote` and `workmux node` tests."""

import hashlib
import os
import subprocess
from pathlib import Path
from typing import Any, Dict, Optional

import pytest
import yaml


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
            }
        )
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
