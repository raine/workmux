"""Helpers for interactive `workmux setup` tests."""

import json
import shlex
from pathlib import Path

from ..conftest import (
    MuxEnvironment,
    get_scripts_dir,
    make_env_script,
    poll_until_file_has_content,
    wait_for_pane_output,
)


def write_claude_manual_status_hook(claude_dir: Path) -> None:
    claude_dir.mkdir(parents=True, exist_ok=True)
    plugin_path = Path(__file__).parents[2] / ".claude-plugin" / "plugin.json"
    plugin = json.loads(plugin_path.read_text())
    (claude_dir / "settings.json").write_text(json.dumps({"hooks": plugin["hooks"]}))


def install_opencode_version(env: MuxEnvironment, version: str) -> None:
    """Make setup tests independent of the host's installed OpenCode version."""
    env.install_script(
        env.fake_bin_dir / "opencode",
        f"#!/bin/sh\nprintf 'opencode {version}\\n'\n",
    )


def run_setup_interactive(env: MuxEnvironment, workmux_exe_path: Path) -> Path:
    scripts_dir = get_scripts_dir(env)
    exit_code_file = scripts_dir / "setup_exit_code.txt"
    if exit_code_file.exists():
        exit_code_file.unlink()

    script = make_env_script(
        env,
        (
            f"{shlex.quote(str(workmux_exe_path))} setup; "
            f"echo $? > {shlex.quote(str(exit_code_file))}"
        ),
        {
            "PATH": env.env["PATH"],
            "HOME": env.env.get("HOME", ""),
            "TMPDIR": env.env.get("TMPDIR", "/tmp"),
            "XDG_CONFIG_HOME": env.env.get("XDG_CONFIG_HOME", ""),
            "XDG_STATE_HOME": env.env.get("XDG_STATE_HOME", ""),
        },
    )
    env.send_keys("test:", script, enter=True)
    return exit_code_file


def run_setup_with_answers(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    *,
    hooks_answer: str = "y",
    skills_answer: str = "n",
    expected_output: tuple[str, ...] = (),
    timeout: float = 5.0,
) -> Path:
    exit_code_file = run_setup_interactive(env, workmux_exe_path)
    for text in expected_output:
        wait_for_pane_output(env, "test", text, timeout=timeout)
    wait_for_pane_output(
        env, "test", "Install or update status tracking hooks?", timeout=timeout
    )
    env.send_keys("test:", hooks_answer)
    wait_for_pane_output(env, "test", "Install bundled skills?", timeout=timeout)
    env.send_keys("test:", skills_answer)

    assert poll_until_file_has_content(exit_code_file, timeout=timeout)
    assert exit_code_file.read_text().strip() == "0"
    return exit_code_file
