import json
import os
import re
import shlex
import shutil
import subprocess
import tempfile
import time
import unicodedata
from abc import ABC, abstractmethod
from pathlib import Path
from typing import Any, Callable, Dict, Generator, List, Optional, Union

from dataclasses import dataclass, field

import pytest
import yaml


# =============================================================================
# Shell Testing Configuration
# =============================================================================

# Shell names to test - paths are discovered dynamically via shutil.which()
SHELL_NAMES = ["bash", "zsh", "fish", "nu"]


@dataclass
class ShellCommands:
    """Shell-specific command generation for multi-shell testing."""

    path: str

    @property
    def name(self) -> str:
        """Return shell name (e.g., 'zsh', 'bash', 'fish', 'nu')."""
        return Path(self.path).name

    @property
    def rc_filename(self) -> str:
        """Return the RC file path relative to HOME for this shell.

        Note: Bash uses .bash_profile because tmux spawns login shells (-l),
        which read .bash_profile, not .bashrc.
        """
        return {
            "bash": ".bash_profile",
            "zsh": ".zshrc",
            "fish": ".config/fish/config.fish",
            "nu": ".config/nushell/config.nu",
        }[self.name]

    def set_env(self, var: str, value: str) -> str:
        """Generate shell-specific environment variable export statement.

        Note: Do not use this for PATH - use prepend_path() instead.
        """
        match self.name:
            case "fish":
                return f"set -gx {var} '{value}'"
            case "nu":
                return f"$env.{var} = '{value}'"
            case _:
                return f"export {var}='{value}'"

    def env_ref(self, var: str) -> str:
        """Return shell-specific syntax for referencing an environment variable."""
        match self.name:
            case "nu":
                return f"$env.{var}"
            case _:
                return f"${var}"

    def prepend_path(self, dir_path: str) -> str:
        """Generate shell-specific command to prepend a directory to PATH.

        Fish and nushell treat PATH as a list, not a colon-separated string,
        so they need special handling.
        """
        match self.name:
            case "fish":
                return f"set -gx PATH '{dir_path}' $PATH"
            case "nu":
                return f"$env.PATH = ($env.PATH | prepend '{dir_path}')"
            case _:
                return f"export PATH='{dir_path}':$PATH"

    def alias(self, name: str, command: str) -> str:
        """Generate shell-specific alias definition.

        Note: Nushell aliases use `alias name = cmd` syntax. The ^ prefix
        forces nushell to call the external command rather than recursing.
        """
        match self.name:
            case "fish":
                return f"alias {name} '{command}'"
            case "nu":
                # Use ^ to call external command, avoiding infinite recursion
                return f"alias {name} = ^{command}"
            case _:
                return f"alias {name}='{command}'"

    def append_to_file(self, text: str, file_path: str) -> str:
        """Generate shell-specific command to append text to a file."""
        match self.name:
            case "nu":
                return f'"{text}" | save --append {file_path}'
            case _:
                return f"echo '{text}' >> {file_path}"


def get_shells_to_test() -> list[str]:
    """Return list of shell paths to test based on environment variables.

    Environment variables:
        TEST_SHELL: Test a specific shell only (e.g., "fish", "nu", "bash", "zsh")

    By default, tests run against all installed shells (bash, zsh, fish, nu).
    Uses shutil.which() to discover actual shell paths rather than hardcoding,
    ensuring portability across different systems (Linux, macOS, Homebrew, etc.).
    """
    # Test a specific shell
    if specific_shell := os.environ.get("TEST_SHELL"):
        path = shutil.which(specific_shell)
        if path:
            return [path]
        raise ValueError(f"Shell '{specific_shell}' not found")

    # Default: test all installed shells
    shells = [p for p in (shutil.which(name) for name in SHELL_NAMES) if p]
    return shells if shells else ["/bin/sh"]


# =============================================================================
# Backend Selection for Tests
# =============================================================================
#
# By default, tests run against tmux only (for CI compatibility).
# To run tests against WezTerm:
#   pytest --backend=wezterm tests/
#   WORKMUX_TEST_BACKEND=wezterm pytest tests/
#
# To run against both backends:
#   pytest --backend=tmux,wezterm tests/
#
# Tests automatically skip if the required backend is not installed.
# =============================================================================


# =============================================================================
# Multiplexer Environment Abstraction
# =============================================================================


class MuxEnvironment(ABC):
    """
    Abstract base class for multiplexer test environments.

    Provides a common interface for running tests against different
    terminal multiplexer backends (tmux, WezTerm).
    """

    def __init__(self, tmp_path: Path):
        self.tmp_path = tmp_path
        self._scripts_dir: Optional[Path] = None  # Lazily created by get_scripts_dir()

        # Create isolated home directory
        self.home_path = self.tmp_path / "test_home"
        self.home_path.mkdir()

        # Prevent shell config issues
        (self.home_path / ".zshrc").touch()

        # Base environment setup
        self.env = os.environ.copy()
        self.env["TMPDIR"] = str(self.tmp_path)
        self.env["HOME"] = str(self.home_path)
        # Explicitly set XDG_STATE_HOME to ensure state files are isolated
        # (config uses $HOME/.config/ directly, so HOME isolation handles that)
        self.env["XDG_STATE_HOME"] = str(self.home_path / ".local" / "state")
        self.env["XDG_CONFIG_HOME"] = str(self.home_path / ".config")

        # Create fake git editor
        fake_editor_script = self.home_path / "fake_git_editor.sh"
        fake_editor_script.write_text(
            "#!/bin/sh\n"
            'if ! grep -q "^[^#]" "$1" 2>/dev/null; then\n'
            '  echo "Test commit" > "$1"\n'
            "fi\n"
        )
        fake_editor_script.chmod(0o755)
        self.env["GIT_EDITOR"] = str(fake_editor_script)

    @property
    @abstractmethod
    def backend_name(self) -> str:
        """Return the backend name ('tmux' or 'wezterm')."""
        pass

    @abstractmethod
    def start_server(self) -> None:
        """Start the multiplexer server with an initial session."""
        pass

    @abstractmethod
    def stop_server(self) -> None:
        """Stop the multiplexer server and clean up resources."""
        pass

    def run_command(
        self, cmd: list[str], check: bool = True, cwd: Optional[Path] = None
    ):
        """Run a generic command within the isolated environment."""
        working_dir = cwd if cwd is not None else self.tmp_path
        result = subprocess.run(
            cmd,
            cwd=working_dir,
            env=self.env,
            capture_output=True,
            text=True,
            check=False,
        )
        if check and result.returncode != 0:
            raise subprocess.CalledProcessError(
                result.returncode, cmd, result.stdout, result.stderr
            )
        return result

    @abstractmethod
    def mux_command(
        self, args: list[str], check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        """
        Run a backend-specific command.

        For tmux: `tmux -S <socket> <args>`
        For WezTerm: `wezterm cli <args>`
        """
        pass

    @abstractmethod
    def list_windows(self) -> list[str]:
        """Return a list of all window/tab names."""
        pass

    @abstractmethod
    def capture_pane(self, window_name: str) -> Optional[str]:
        """Capture the content of a pane in the specified window."""
        pass

    @abstractmethod
    def send_keys(self, target: str, text: str, enter: bool = True) -> None:
        """
        Send text to a pane.

        Args:
            target: Window/pane identifier
            text: Text to send
            enter: Whether to send Enter key after text
        """
        pass

    @abstractmethod
    def run_shell_background(self, script: str) -> None:
        """
        Run a shell script in the background.

        Used for commands that may kill their own window (like merge/remove).
        """
        pass

    @abstractmethod
    def set_session_env(self, key: str, value: str) -> None:
        """Set an environment variable in the multiplexer session."""
        pass

    @abstractmethod
    def kill_window(self, window_name: str) -> None:
        """Kill/close a specific window by name."""
        pass

    @abstractmethod
    def get_current_window(self) -> Optional[str]:
        """Get the name of the currently focused window."""
        pass

    @abstractmethod
    def select_window(self, window_name: str) -> None:
        """Switch focus to a specific window by name."""
        pass

    @abstractmethod
    def new_window(self, name: Optional[str] = None) -> None:
        """Create a new window/tab with optional name."""
        pass

    @abstractmethod
    def configure_default_shell(self, shell: str) -> None:
        """Configure the default shell for new panes.

        For tmux: sets the default-shell option.
        For WezTerm: sets SHELL env var (workmux already starts with -l).
        """
        pass


class TmuxEnvironment(MuxEnvironment):
    """
    Tmux-specific test environment.

    Uses a private socket file for complete isolation.
    """

    def __init__(self, tmp_path: Path):
        super().__init__(tmp_path)

        # Use short socket path to avoid macOS length limits
        tmp_file = tempfile.NamedTemporaryFile(
            prefix="tmux_", suffix=".sock", delete=False
        )
        self.socket_path = Path(tmp_file.name)
        tmp_file.close()
        self.socket_path.unlink()

        # Ensure we don't accidentally target user's tmux or WezTerm
        self.env.pop("TMUX", None)
        self.env.pop("WEZTERM_PANE", None)
        self.env["TMUX_CONF"] = "/dev/null"

    @property
    def backend_name(self) -> str:
        return "tmux"

    def start_server(self) -> None:
        """Start isolated tmux server with a 'test' session."""
        self.mux_command(["new-session", "-d", "-s", "test"])

    def stop_server(self) -> None:
        """Kill the tmux server and clean up socket."""
        self.mux_command(["kill-server"], check=False)
        if self.socket_path.exists():
            self.socket_path.unlink()

    def mux_command(
        self, args: list[str], check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        """Run tmux command with private socket."""
        base_cmd = ["tmux", "-S", str(self.socket_path)]
        return self.run_command(base_cmd + args, check=check)

    # Alias for backward compatibility with existing tests
    def tmux(self, args: list[str], check: bool = True):
        """Alias for mux_command (backward compatibility)."""
        return self.mux_command(args, check=check)

    def list_windows(self) -> list[str]:
        """List all tmux window names."""
        result = self.mux_command(["list-windows", "-F", "#{window_name}"])
        return [w for w in result.stdout.strip().split("\n") if w]

    def capture_pane(self, window_name: str) -> Optional[str]:
        """Capture pane content from a tmux window."""
        result = self.mux_command(
            ["capture-pane", "-p", "-t", window_name], check=False
        )
        if result.returncode == 0:
            return result.stdout
        return None

    def send_keys(self, target: str, text: str, enter: bool = True) -> None:
        """Send keys to a tmux pane."""
        args = ["send-keys", "-t", target, text]
        if enter:
            args.append("C-m")
        self.mux_command(args)

    def run_shell_background(self, script: str) -> None:
        """Run script in background using tmux run-shell."""
        self.mux_command(["run-shell", "-b", script])

    def set_session_env(self, key: str, value: str) -> None:
        """Set environment variable in tmux session."""
        self.mux_command(["set-environment", "-g", key, value])

    def kill_window(self, window_name: str) -> None:
        """Kill a tmux window by name. Raises if window doesn't exist."""
        result = self.mux_command(["kill-window", "-t", window_name], check=False)
        if result.returncode != 0:
            raise RuntimeError(f"Window '{window_name}' not found: {result.stderr}")

    def get_current_window(self) -> Optional[str]:
        """Get the name of the currently active tmux window."""
        result = self.mux_command(
            ["display-message", "-p", "#{window_name}"], check=False
        )
        if result.returncode == 0:
            return result.stdout.strip()
        return None

    def select_window(self, window_name: str) -> None:
        """Switch focus to a tmux window by name. Raises if window doesn't exist."""
        result = self.mux_command(["select-window", "-t", window_name], check=False)
        if result.returncode != 0:
            raise RuntimeError(f"Window '{window_name}' not found: {result.stderr}")

    def new_window(self, name: Optional[str] = None) -> None:
        """Create a new tmux window with optional name."""
        args = ["new-window"]
        if name:
            args.extend(["-n", name])
        self.mux_command(args)

    def configure_default_shell(self, shell: str) -> None:
        """Configure tmux to use the specified shell for new panes."""
        self.mux_command(["set-option", "-g", "default-shell", shell])


class WezTermEnvironment(MuxEnvironment):
    """
    WezTerm-specific test environment.

    Uses WezTerm's CLI with a dedicated workspace for isolation.
    Note: WezTerm doesn't support fully isolated servers like tmux sockets,
    so we rely on workspace isolation and careful cleanup.
    """

    def __init__(self, tmp_path: Path):
        super().__init__(tmp_path)

        import uuid

        self.workspace_name = f"workmux_test_{uuid.uuid4().hex[:8]}"
        self._created_pane_ids: list[str] = []

        # Remove TMUX env var to ensure we use WezTerm
        self.env.pop("TMUX", None)

    @property
    def backend_name(self) -> str:
        return "wezterm"

    def start_server(self) -> None:
        """Create a new tab in the test workspace."""
        result = self.run_command(
            [
                "wezterm",
                "cli",
                "spawn",
                "--new-window",
                "--workspace",
                self.workspace_name,
                "--cwd",
                str(self.tmp_path),
            ],
            check=True,
        )
        pane_id = result.stdout.strip()
        self._created_pane_ids.append(pane_id)

        # Set tab title to "test" for consistency with tmux
        self.run_command(
            ["wezterm", "cli", "set-tab-title", "--pane-id", pane_id, "test"]
        )

    def stop_server(self) -> None:
        """Clean up all panes in the test workspace.

        Workmux commands spawn additional tabs/panes that aren't tracked
        in _created_pane_ids, so we clean up everything in our workspace.
        """
        # Get all panes in our workspace (includes those created by workmux)
        for pane in self._list_panes():
            pane_id = str(pane["pane_id"])
            self.run_command(
                ["wezterm", "cli", "kill-pane", "--pane-id", pane_id],
                check=False,
            )
        self._created_pane_ids.clear()

    def mux_command(
        self, args: list[str], check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        """Run wezterm cli command."""
        return self.run_command(["wezterm", "cli"] + args, check=check)

    def _list_panes(self) -> list[dict]:
        """Get all panes in our workspace as parsed JSON."""
        result = self.mux_command(["list", "--format", "json"])
        all_panes = json.loads(result.stdout)
        return [p for p in all_panes if p.get("workspace") == self.workspace_name]

    def list_windows(self) -> list[str]:
        """List all tab titles in our workspace."""
        panes = self._list_panes()
        seen = set()
        result = []
        for p in panes:
            title = p.get("tab_title", "")
            if title and title not in seen:
                seen.add(title)
                result.append(title)
        return result

    def _find_pane_by_tab_title(self, tab_title: str) -> Optional[dict]:
        """Find a pane by its tab title."""
        panes = self._list_panes()
        for p in panes:
            if p.get("tab_title") == tab_title:
                return p
        return None

    def capture_pane(self, window_name: str) -> Optional[str]:
        """Capture pane content by tab title."""
        pane = self._find_pane_by_tab_title(window_name)
        if not pane:
            return None

        pane_id = str(pane["pane_id"])
        result = self.mux_command(
            ["get-text", "--pane-id", pane_id, "--escapes"], check=False
        )
        if result.returncode == 0:
            return result.stdout
        return None

    def send_keys(self, target: str, text: str, enter: bool = True) -> None:
        """Send text to a pane identified by tab title or 'test:' session prefix."""
        if target == "test:":
            panes = self._list_panes()
            if panes:
                pane_id = str(panes[0]["pane_id"])
            else:
                raise RuntimeError("No panes in test workspace")
        else:
            pane = self._find_pane_by_tab_title(target)
            if not pane:
                raise RuntimeError(f"Pane with tab_title '{target}' not found")
            pane_id = str(pane["pane_id"])

        self.mux_command(["send-text", "--pane-id", pane_id, "--no-paste", text])
        if enter:
            self.mux_command(["send-text", "--pane-id", pane_id, "--no-paste", "\r"])

    def run_shell_background(self, script: str) -> None:
        """Run script in background via nohup."""
        bg_script = f"nohup sh -c {repr(script)} >/dev/null 2>&1 &"
        self.send_keys("test:", bg_script, enter=True)

    def set_session_env(self, key: str, value: str) -> None:
        """Set environment variable via shell export."""
        self.send_keys("test:", f"export {key}={repr(value)}", enter=True)

    def kill_window(self, window_name: str) -> None:
        """Kill a WezTerm tab by its title. Kills ALL panes in the tab."""
        panes = self._list_panes()
        matching_panes = [p for p in panes if p.get("tab_title") == window_name]

        if not matching_panes:
            raise RuntimeError(f"Window '{window_name}' not found in workspace")

        # Kill all panes in reverse order (last pane first, like Rust code)
        for pane in reversed(matching_panes):
            self.mux_command(["kill-pane", "--pane-id", str(pane["pane_id"])])

    def get_current_window(self) -> Optional[str]:
        """Get the tab title of the currently focused pane in our workspace.

        Uses wezterm cli list-clients to find the focused pane, then maps
        it to a tab title if it belongs to our test workspace.
        """
        # Get the globally focused pane ID
        result = self.run_command(
            ["wezterm", "cli", "list-clients", "--format", "json"], check=False
        )
        if result.returncode != 0 or not result.stdout.strip():
            return None

        clients = json.loads(result.stdout)
        if not clients:
            return None

        focused_pane_id = clients[0].get("focused_pane_id")
        if focused_pane_id is None:
            return None

        # Check if focused pane is in our workspace and get its tab title
        for pane in self._list_panes():
            if pane.get("pane_id") == focused_pane_id:
                return pane.get("tab_title")

        # Focused pane is not in our workspace
        return None

    def select_window(self, window_name: str) -> None:
        """Switch focus to a WezTerm tab by title. Raises if tab doesn't exist."""
        pane = self._find_pane_by_tab_title(window_name)
        if not pane:
            raise RuntimeError(f"Window '{window_name}' not found in workspace")
        pane_id = str(pane["pane_id"])
        self.mux_command(["activate-pane", "--pane-id", pane_id])

    def new_window(self, name: Optional[str] = None) -> None:
        """Create a new WezTerm tab in the test workspace."""
        # Find window_id from existing pane in our workspace
        panes = self._list_panes()
        if not panes:
            raise RuntimeError("No existing panes in test workspace to add tab to")
        window_id = str(panes[0]["window_id"])

        result = self.run_command(
            [
                "wezterm",
                "cli",
                "spawn",
                "--window-id",
                window_id,
                "--cwd",
                str(self.tmp_path),
            ],
            check=True,
        )
        pane_id = result.stdout.strip()
        if name:
            self.run_command(
                ["wezterm", "cli", "set-tab-title", "--pane-id", pane_id, name]
            )

    def configure_default_shell(self, shell: str) -> None:
        """Configure the default shell for WezTerm panes.

        WezTerm doesn't have a session-level default-shell option like tmux.
        Instead, workmux already starts shells with -l flag for login shell behavior.
        We just set the SHELL env var so subprocesses know which shell to use.
        """
        self.env["SHELL"] = shell


def skip_if_backend_unavailable(backend: str):
    """
    Skip test if the required multiplexer backend is not available.

    For tmux: Checks binary exists and `tmux -V` succeeds.
    For WezTerm: Checks binary exists and `wezterm cli list` succeeds
                (requires WezTerm to be running).

    This ensures CI environments without WezTerm can still run tmux tests.
    """
    if backend == "tmux":
        if not shutil.which("tmux"):
            pytest.skip("tmux not installed")
        result = subprocess.run(
            ["tmux", "-V"], capture_output=True, text=True, check=False
        )
        if result.returncode != 0:
            pytest.skip("tmux not available")
    elif backend == "wezterm":
        if not shutil.which("wezterm"):
            pytest.skip("wezterm not installed")
        result = subprocess.run(
            ["wezterm", "cli", "list"], capture_output=True, text=True, check=False
        )
        if result.returncode != 0:
            pytest.skip("wezterm not running or not available")


# Type alias for tests that accept either backend
MuxEnv = Union[TmuxEnvironment, WezTermEnvironment]

# Default window prefix - must match src/config.rs window_prefix() default
DEFAULT_WINDOW_PREFIX = "wm-"

# Type alias for backward compatibility - tests can use either
MuxEnv = Union[TmuxEnvironment, WezTermEnvironment]

# =============================================================================
# Shared Assertion Helpers
# =============================================================================


def assert_window_exists(env: MuxEnvironment, window_name: str) -> None:
    """Ensure a window/tab with the provided name exists."""
    existing_windows = env.list_windows()
    assert window_name in existing_windows, (
        f"Window {window_name!r} not found. Existing: {existing_windows!r}"
    )


def assert_session_exists(env: "TmuxEnvironment", session_name: str) -> None:
    """Ensure a tmux session with the provided name exists."""
    result = env.tmux(["list-sessions", "-F", "#{session_name}"])
    existing_sessions = [s for s in result.stdout.strip().split("\n") if s]
    assert session_name in existing_sessions, (
        f"Session {session_name!r} not found. Existing: {existing_sessions!r}"
    )


def assert_session_not_exists(env: "TmuxEnvironment", session_name: str) -> None:
    """Ensure a tmux session with the provided name does NOT exist."""
    result = env.tmux(["list-sessions", "-F", "#{session_name}"], check=False)
    if result.returncode != 0:
        # No sessions exist at all
        return
    existing_sessions = [s for s in result.stdout.strip().split("\n") if s]
    assert session_name not in existing_sessions, (
        f"Session {session_name!r} should not exist but was found. Existing: {existing_sessions!r}"
    )


def assert_window_not_exists(env: "TmuxEnvironment", window_name: str) -> None:
    """Ensure a tmux window with the provided name does NOT exist."""
    result = env.tmux(["list-windows", "-F", "#{window_name}"], check=False)
    if result.returncode != 0:
        # No windows exist at all
        return
    existing_windows = [w for w in result.stdout.strip().split("\n") if w]
    assert window_name not in existing_windows, (
        f"Window {window_name!r} should not exist but was found. Existing: {existing_windows!r}"
    )


def assert_copied_file(
    worktree_path: Path, relative_path: str, expected_text: str | None = None
) -> Path:
    """Assert that a copied file exists in the worktree and is not a symlink."""
    file_path = worktree_path / relative_path
    assert file_path.exists(), f"Expected copied file {relative_path} to exist"
    assert not file_path.is_symlink(), (
        f"Expected {relative_path} to be a regular file, but found a symlink"
    )
    if expected_text is not None:
        assert file_path.read_text() == expected_text
    return file_path


def assert_symlink_to(worktree_path: Path, relative_path: str) -> Path:
    """Assert that a symlink exists in the worktree and return the path."""
    symlink_path = worktree_path / relative_path
    assert symlink_path.exists(), f"Expected symlink {relative_path} to exist"
    assert symlink_path.is_symlink(), f"Expected {relative_path} to be a symlink"
    return symlink_path


# =============================================================================
# Polling & Wait Helpers
# =============================================================================


def wait_for_pane_output(
    env: MuxEnvironment, window_name: str, text: str, timeout: float = 2.0
) -> None:
    """Poll until the specified text appears in the pane."""

    final_content = f"Pane for window '{window_name}' was not captured."

    def _has_output() -> bool:
        nonlocal final_content
        content = env.capture_pane(window_name)
        if content is not None:
            final_content = content
            return text in final_content
        final_content = f"Error capturing pane for window '{window_name}'"
        return False

    if not poll_until(_has_output, timeout=timeout):
        assert False, (
            f"Expected output {text!r} not found in window {window_name!r} within {timeout}s.\n"
            f"--- FINAL PANE CONTENT ---\n"
            f"{final_content}\n"
            f"--------------------------"
        )


def wait_for_file(
    env: MuxEnvironment,
    file_path: Path,
    timeout: float = 5.0,
    *,
    window_name: str | None = None,
    worktree_path: Path | None = None,
    debug_log_path: Path | None = None,
) -> None:
    """
    Poll for a file to exist. On timeout, fail with diagnostics about panes, worktrees, and logs.
    """

    def _file_exists() -> bool:
        return file_path.exists()

    if poll_until(_file_exists, timeout=timeout):
        return

    diagnostics: list[str] = [f"Target file: {file_path}"]

    if worktree_path is not None:
        diagnostics.append(f"Worktree path: {worktree_path}")
        if worktree_path.exists():
            try:
                files = sorted(p.name for p in worktree_path.iterdir())
                diagnostics.append(f"Worktree files: {files}")
            except Exception as exc:  # pragma: no cover - best effort diagnostics
                diagnostics.append(f"Error listing worktree files: {exc}")
        else:
            diagnostics.append("Worktree directory not found.")

    if debug_log_path is not None:
        if debug_log_path.exists():
            diagnostics.append(
                f"Debug log '{debug_log_path.name}':\n{debug_log_path.read_text()}"
            )
        else:
            diagnostics.append(f"Debug log '{debug_log_path.name}' not found.")

    if window_name is not None:
        pane_content = env.capture_pane(window_name)
        if pane_content is None:
            pane_content = f"Could not capture pane for window '{window_name}'."
        diagnostics.append(f"Pane '{window_name}' content:\n{pane_content}")

    diag_str = "\n".join(diagnostics)
    assert False, (
        f"File not found after {timeout}s: {file_path}\n\n"
        f"-- Diagnostics --\n{diag_str}\n-----------------"
    )


# =============================================================================
# Path & Naming Helpers
# =============================================================================


def prompt_file_for_branch(worktree_path: Path, branch_name: str) -> Path:
    """Return the path to the prompt file for the given branch.

    Prompt files are now stored in <worktree>/.workmux/PROMPT-<sanitized-branch>.md
    Branch names with slashes are sanitized to dashes.
    """
    sanitized_branch = branch_name.replace("/", "-")
    return worktree_path / ".workmux" / f"PROMPT-{sanitized_branch}.md"


def assert_prompt_file_contents(
    env: MuxEnvironment,
    branch_name: str,
    expected_text: str,
    worktree_path: Optional[Path] = None,
) -> None:
    """Assert that a prompt file exists for the branch and matches the expected text."""
    if worktree_path is None:
        raise ValueError(
            "worktree_path is required - prompt files are now in <worktree>/.workmux/"
        )
    prompt_file = prompt_file_for_branch(worktree_path, branch_name)
    assert prompt_file.exists(), f"Prompt file not found at {prompt_file}"
    actual_text = prompt_file.read_text()
    assert actual_text == expected_text, (
        f"Content mismatch for prompt file: {prompt_file}"
    )


def file_for_commit(worktree_path: Path, commit_message: str) -> Path:
    """Return the expected file path generated by create_commit for a message."""
    sanitized = commit_message.replace(" ", "_").replace(":", "")
    return worktree_path / f"file_for_{sanitized}.txt"


def configure_default_shell(shell: str | None = None) -> list[list[str]]:
    """Return tmux commands that configure the default shell for panes."""
    shell_path = shell or os.environ.get("SHELL", "/bin/zsh")
    return [["set-option", "-g", "default-shell", shell_path]]


# =============================================================================
# RepoBuilder - Declarative Git Repository Setup
# =============================================================================


@dataclass
class RepoBuilder:
    """Builder pattern for setting up git repositories declaratively in tests."""

    env: MuxEnvironment
    path: Path
    _files_to_add: list[str] = field(default_factory=list)

    def with_file(self, relative_path: str, content: str) -> "RepoBuilder":
        """Create a file with the given content."""
        file_path = self.path / relative_path
        file_path.parent.mkdir(parents=True, exist_ok=True)
        file_path.write_text(content)
        self._files_to_add.append(relative_path)
        return self

    def with_files(self, files: dict[str, str]) -> "RepoBuilder":
        """Create multiple files from a dict of path -> content."""
        for rel_path, content in files.items():
            self.with_file(rel_path, content)
        return self

    def with_dir(self, relative_path: str) -> "RepoBuilder":
        """Create an empty directory."""
        dir_path = self.path / relative_path
        dir_path.mkdir(parents=True, exist_ok=True)
        return self

    def with_executable(self, relative_path: str, content: str) -> "RepoBuilder":
        """Create an executable file."""
        file_path = self.path / relative_path
        file_path.parent.mkdir(parents=True, exist_ok=True)
        file_path.write_text(content)
        file_path.chmod(0o755)
        self._files_to_add.append(relative_path)
        return self

    def commit(self, message: str = "Update files") -> "RepoBuilder":
        """Stage all pending files and commit."""
        if self._files_to_add:
            self.env.run_command(["git", "add"] + self._files_to_add, cwd=self.path)
            self._files_to_add.clear()
        else:
            self.env.run_command(["git", "add", "."], cwd=self.path)
        self.env.run_command(["git", "commit", "-m", message], cwd=self.path)
        return self

    def add_to_gitignore(self, patterns: list[str]) -> "RepoBuilder":
        """Append patterns to .gitignore."""
        gitignore_path = self.path / ".gitignore"
        with gitignore_path.open("a") as f:
            for pattern in patterns:
                f.write(f"{pattern}\n")
        return self


@pytest.fixture
def repo_builder(mux_server: MuxEnvironment, repo_path: Path) -> RepoBuilder:
    """Provides a RepoBuilder for declarative git setup in tests."""
    return RepoBuilder(env=mux_server, path=repo_path)


# =============================================================================
# Fake Agent Installation
# =============================================================================


@dataclass
class FakeAgentInstaller:
    """Factory for installing fake agent commands in tests."""

    env: MuxEnvironment
    _bin_dir: Path | None = None

    @property
    def bin_dir(self) -> Path:
        if self._bin_dir is None:
            self._bin_dir = self.env.tmp_path / "agents-bin"
            self._bin_dir.mkdir(exist_ok=True)
        return self._bin_dir

    def install(self, name: str, script_body: str) -> Path:
        """Creates a fake agent command in PATH so panes can invoke it."""
        script_path = self.bin_dir / name
        script_path.write_text(script_body)
        script_path.chmod(0o755)

        new_path = f"{self.bin_dir}:{self.env.env.get('PATH', '')}"
        self.env.env["PATH"] = new_path
        self.env.set_session_env("PATH", new_path)
        return script_path


@pytest.fixture
def fake_agent_installer(mux_server: MuxEnvironment) -> FakeAgentInstaller:
    """Provides a factory for installing fake agent commands."""
    return FakeAgentInstaller(env=mux_server)


def slugify(text: str) -> str:
    """
    Convert text to a slug, matching the behavior of the Rust `slug` crate.

    - Converts to lowercase
    - Replaces non-alphanumeric characters with dashes
    - Removes leading/trailing dashes
    - Collapses multiple dashes to single dash
    """
    # Normalize unicode characters (e.g., é -> e)
    text = unicodedata.normalize("NFKD", text)
    text = text.encode("ascii", "ignore").decode("ascii")

    # Convert to lowercase
    text = text.lower()

    # Replace non-alphanumeric characters with dashes
    text = re.sub(r"[^a-z0-9]+", "-", text)

    # Remove leading/trailing dashes and collapse multiple dashes
    text = re.sub(r"-+", "-", text)
    text = text.strip("-")

    return text


def pytest_addoption(parser):
    """Add --backend option to pytest."""
    parser.addoption(
        "--backend",
        action="store",
        default=None,
        help="Multiplexer backend to test: tmux, wezterm, or both (comma-separated)",
    )


def pytest_configure(config):
    """Register custom markers."""
    config.addinivalue_line(
        "markers",
        "tmux_only: mark test as tmux-specific (skipped for other backends)",
    )


def pytest_xdist_auto_num_workers(config) -> int | None:
    """Limit parallel workers for WezTerm to avoid overwhelming the mux-server.

    WezTerm's GUI mux-server can't handle many parallel test workers spawning
    panes simultaneously - causes race conditions in window mapping.
    """
    backends = get_test_backends(config)
    if "wezterm" in backends:
        return 8  # Single worker to avoid WezTerm crashes
    return None  # Let xdist use its default (usually CPU count)


def get_test_backends(config) -> list[str]:
    """
    Determine which backends to test based on config/environment.

    Priority:
    1. --backend command line option
    2. WORKMUX_TEST_BACKEND environment variable
    3. Default to tmux
    """
    # Check command line option
    backend_opt = config.getoption("--backend") if config else None
    if backend_opt:
        return [b.strip() for b in backend_opt.split(",")]

    # Check environment variable
    env_backend = os.environ.get("WORKMUX_TEST_BACKEND")
    if env_backend:
        return [b.strip() for b in env_backend.split(",")]

    # Default to tmux
    return ["tmux"]


def pytest_generate_tests(metafunc):
    """Dynamically parametrize mux_server fixture based on config.

    Tests marked with @pytest.mark.tmux_only will only run with tmux backend.
    """
    if "mux_server" in metafunc.fixturenames:
        # Check if test is marked tmux_only
        tmux_only = metafunc.definition.get_closest_marker("tmux_only")
        if tmux_only:
            # Force tmux-only for this test
            backends = ["tmux"]
        else:
            backends = get_test_backends(metafunc.config)
        metafunc.parametrize("mux_server", backends, indirect=True)


@pytest.fixture
def mux_server(request, tmp_path: Path) -> Generator[MuxEnvironment, None, None]:
    """
    Parameterized fixture for running tests with different multiplexer backends.

    Configure via:
    - Command line: pytest --backend=wezterm
    - Environment: WORKMUX_TEST_BACKEND=wezterm pytest
    - Multiple backends: --backend=tmux,wezterm or WORKMUX_TEST_BACKEND=tmux,wezterm

    Tests using this fixture will run once per enabled backend.
    """
    backend = request.param
    skip_if_backend_unavailable(backend)

    if backend == "tmux":
        test_env = TmuxEnvironment(tmp_path)
    else:
        test_env = WezTermEnvironment(tmp_path)

    test_env.start_server()
    yield test_env
    test_env.stop_server()
    # Clean up scripts directory if it was created
    if test_env._scripts_dir is not None and test_env._scripts_dir.exists():
        shutil.rmtree(test_env._scripts_dir, ignore_errors=True)


@pytest.fixture(params=get_shells_to_test(), ids=lambda s: Path(s).name)
def shell_cmd(request) -> ShellCommands:
    """
    Fixture providing shell-specific command helpers.

    By default, tests run with zsh only. To test all available shells:
        TEST_ALL_SHELLS=1 pytest tests/

    Tests using this fixture will run once per enabled shell.
    """
    shell_path = request.param
    if not os.path.exists(shell_path):
        pytest.skip(f"Shell {shell_path} not available")
    return ShellCommands(shell_path)


def setup_git_repo(path: Path, env_vars: Optional[dict] = None):
    """Initializes a git repository in the given path with an initial commit."""
    subprocess.run(
        ["git", "init", "-b", "main"],
        cwd=path,
        check=True,
        capture_output=True,
        env=env_vars,
    )
    # Configure git user for commits
    subprocess.run(
        ["git", "config", "user.name", "Test User"],
        cwd=path,
        check=True,
        capture_output=True,
        env=env_vars,
    )
    subprocess.run(
        ["git", "config", "user.email", "test@example.com"],
        cwd=path,
        check=True,
        capture_output=True,
        env=env_vars,
    )
    # Ignore test_home directory and test output files to prevent uncommitted changes
    gitignore_path = path / ".gitignore"
    gitignore_path.write_text(
        "test_home/\nworkmux_*.txt\n"  # Test helper output files
    )
    subprocess.run(
        ["git", "add", ".gitignore"],
        cwd=path,
        check=True,
        capture_output=True,
        env=env_vars,
    )
    subprocess.run(
        ["git", "commit", "--allow-empty", "-m", "Initial commit"],
        cwd=path,
        check=True,
        capture_output=True,
        env=env_vars,
    )


@pytest.fixture
def repo_path(mux_server: MuxEnvironment) -> Path:
    """Initializes a git repo in the test env and returns its path.

    This fixture is backend-agnostic and will run tests against all
    configured backends (tmux by default, or --backend=wezterm).
    """
    path = mux_server.tmp_path
    setup_git_repo(path, mux_server.env)
    return path


# Backward compatibility alias - tests using mux_repo_path will continue to work
@pytest.fixture
def mux_repo_path(repo_path: Path) -> Path:
    """Alias for repo_path - for backward compatibility."""
    return repo_path


@pytest.fixture
def remote_repo_path(mux_server: MuxEnvironment) -> Path:
    """Creates a bare git repo to act as a remote.

    This fixture is backend-agnostic.
    """
    parent = mux_server.tmp_path.parent
    remote_path = Path(tempfile.mkdtemp(prefix="remote_repo_", dir=parent))
    subprocess.run(
        ["git", "init", "--bare"],
        cwd=remote_path,
        check=True,
        capture_output=True,
    )
    return remote_path


# Backward compatibility alias
@pytest.fixture
def mux_remote_repo_path(remote_repo_path: Path) -> Path:
    """Alias for remote_repo_path - for backward compatibility."""
    return remote_repo_path


def poll_until(
    condition: Callable[[], bool],
    timeout: float = 5.0,
    poll_interval: float = 0.1,
) -> bool:
    """
    Poll until a condition is met or timeout is reached.

    Args:
        condition: A callable that returns True when the condition is met
        timeout: Maximum time to wait in seconds
        poll_interval: Time to wait between checks in seconds

    Returns:
        True if condition was met, False if timeout was reached
    """
    start_time = time.time()
    while time.time() - start_time < timeout:
        if condition():
            return True
        time.sleep(poll_interval)
    return False


def poll_until_file_has_content(file_path: Path, timeout: float = 5.0) -> bool:
    """
    Poll until a file exists and has non-empty content.

    This avoids race conditions where a file is created but not yet written to.
    Shell redirection like `echo $? > file` may create an empty file before
    writing the actual content.

    Args:
        file_path: Path to the file to check
        timeout: Maximum time to wait in seconds

    Returns:
        True if file exists with content, False if timeout was reached
    """

    def has_content() -> bool:
        if not file_path.exists():
            return False
        try:
            return bool(file_path.read_text().strip())
        except (IOError, OSError):
            return False

    return poll_until(has_content, timeout=timeout)


@dataclass
class WorkmuxCommandResult:
    """Represents the result of running a workmux command inside tmux."""

    exit_code: int
    stdout: str
    stderr: str


@pytest.fixture(scope="session")
def workmux_exe_path() -> Path:
    """
    Returns the path to the local workmux build for testing.
    """
    local_path = Path(__file__).parent.parent / "target/debug/workmux"
    if not local_path.exists():
        pytest.fail("Could not find workmux executable. Run 'cargo build' first.")
    return local_path


def write_workmux_config(
    repo_path: Path,
    panes: Optional[List[Dict[str, Any]]] = None,
    post_create: Optional[List[str]] = None,
    pre_merge: Optional[List[str]] = None,
    pre_remove: Optional[List[str]] = None,
    files: Optional[Dict[str, List[str]]] = None,
    env: Optional[MuxEnvironment] = None,
    window_prefix: Optional[str] = None,
    agent: Optional[str] = None,
    merge_strategy: Optional[str] = None,
    worktree_naming: Optional[str] = None,
    worktree_prefix: Optional[str] = None,
):
    """Creates a .workmux.yaml file from structured data and optionally commits it."""
    # Disable nerdfonts by default to ensure consistent "wm-" prefix in tests,
    # regardless of user's global config
    config: Dict[str, Any] = {"nerdfont": False}
    if panes is not None:
        config["panes"] = panes
    if post_create:
        config["post_create"] = post_create
    if pre_merge:
        config["pre_merge"] = pre_merge
    if pre_remove:
        config["pre_remove"] = pre_remove
    if files:
        config["files"] = files
    if window_prefix:
        config["window_prefix"] = window_prefix
    if agent:
        config["agent"] = agent
    if merge_strategy:
        config["merge_strategy"] = merge_strategy
    if worktree_naming:
        config["worktree_naming"] = worktree_naming
    if worktree_prefix:
        config["worktree_prefix"] = worktree_prefix
    (repo_path / ".workmux.yaml").write_text(yaml.dump(config))

    # If env is provided, commit the config file to avoid uncommitted changes in merge tests
    if env:
        subprocess.run(
            ["git", "add", ".workmux.yaml"], cwd=repo_path, check=True, env=env.env
        )
        subprocess.run(
            ["git", "commit", "-m", "Add workmux config"],
            cwd=repo_path,
            check=True,
            env=env.env,
        )


def write_global_workmux_config(
    env: MuxEnvironment,
    panes: Optional[List[Dict[str, Any]]] = None,
    post_create: Optional[List[str]] = None,
    files: Optional[Dict[str, List[str]]] = None,
    window_prefix: Optional[str] = None,
) -> Path:
    """Creates the global ~/.config/workmux/config.yaml file within the isolated HOME."""
    config: Dict[str, Any] = {}
    if panes is not None:
        config["panes"] = panes
    if post_create is not None:
        config["post_create"] = post_create
    if files is not None:
        config["files"] = files
    if window_prefix is not None:
        config["window_prefix"] = window_prefix

    config_dir = env.home_path / ".config" / "workmux"
    config_dir.mkdir(parents=True, exist_ok=True)
    config_path = config_dir / "config.yaml"
    config_path.write_text(yaml.dump(config))
    return config_path


def get_worktree_path(repo_path: Path, branch_name: str) -> Path:
    """Returns the expected path for a worktree directory.

    The directory name is the slugified version of the branch name,
    matching the Rust workmux behavior.
    """
    handle = slugify(branch_name)
    return repo_path.parent / f"{repo_path.name}__worktrees" / handle


def get_window_name(branch_name: str) -> str:
    """Returns the expected tmux window name for a worktree.

    The window name uses the slugified version of the branch name,
    matching the Rust workmux behavior.
    """
    handle = slugify(branch_name)
    return f"{DEFAULT_WINDOW_PREFIX}{handle}"


# Global counter to generate unique script names
_script_counter = 0


def get_scripts_dir(env: MuxEnvironment) -> Path:
    """Get the directory for test helper scripts.

    Uses a directory in /tmp (outside the git repo) to avoid being
    affected by git operations like `git stash -u` which would stash
    untracked files in the repo.

    The scripts dir is cached on the env object to ensure consistent paths
    across multiple calls within the same test. Uses tempfile.mkdtemp for
    guaranteed uniqueness in parallel test runs.
    """
    # Cache the scripts dir on the env object for consistent paths
    if env._scripts_dir is None:
        # Use mkdtemp for guaranteed uniqueness (avoids hash collisions)
        env._scripts_dir = Path(tempfile.mkdtemp(prefix="workmux_scripts_"))
    return env._scripts_dir


def make_env_script(env: MuxEnvironment, command: str, env_vars: dict[str, str]) -> str:
    """Create a script file that sets environment variables and runs a command.

    This avoids tmux send-keys line length limits when env vars or paths are long.

    Args:
        env: The multiplexer environment (provides tmp_path)
        command: The shell command to run
        env_vars: Environment variables to set (e.g., {"XDG_STATE_HOME": "/path"})

    Returns:
        Path to the script file (as string) that can be passed to send_keys
    """
    global _script_counter
    _script_counter += 1
    script_file = get_scripts_dir(env) / f"env_cmd_{_script_counter}.sh"

    exports = "\n".join(f"export {k}={shlex.quote(v)}" for k, v in env_vars.items())
    script_content = f"""#!/bin/sh
{exports}
{command}
"""
    script_file.write_text(script_content)
    script_file.chmod(0o755)
    return str(script_file)


def get_session_name(branch_name: str) -> str:
    """Returns the expected tmux session name for a worktree created with --session.

    The session name uses the slugified version of the branch name,
    matching the Rust workmux behavior.
    """
    handle = slugify(branch_name)
    return f"{DEFAULT_WINDOW_PREFIX}{handle}"


def run_workmux_command(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    command: str,
    pre_run_mux_cmds: Optional[List[List[str]]] = None,
    expect_fail: bool = False,
    working_dir: Optional[Path] = None,
    stdin_input: Optional[str] = None,
) -> WorkmuxCommandResult:
    """
    Helper to run a workmux command inside the isolated multiplexer session.

    Allows tests to optionally expect failure while still capturing stdout/stderr.

    Args:
        env: The isolated multiplexer environment
        workmux_exe_path: Path to the workmux executable
        repo_path: Path to the git repository
        command: The workmux command to run (e.g., "add feature-branch")
        pre_run_mux_cmds: Optional list of mux commands to run before the command
        expect_fail: Whether the command is expected to fail (non-zero exit)
        working_dir: Optional directory to run the command from (defaults to repo_path)
        stdin_input: Optional text to pipe to the command's stdin
    """
    scripts_dir = get_scripts_dir(env)
    stdout_file = scripts_dir / "workmux_stdout.txt"
    stderr_file = scripts_dir / "workmux_stderr.txt"
    exit_code_file = scripts_dir / "workmux_exit_code.txt"
    script_file = scripts_dir / "workmux_run.sh"

    for f in [stdout_file, stderr_file, exit_code_file]:
        if f.exists():
            f.unlink()

    if pre_run_mux_cmds:
        for cmd_args in pre_run_mux_cmds:
            env.mux_command(cmd_args)

    workdir = working_dir if working_dir is not None else repo_path

    # Handle stdin piping via printf
    pipe_cmd = ""
    if stdin_input is not None:
        pipe_cmd = f"printf %s {shlex.quote(stdin_input)} | "

    # Write the command to a script file to avoid tmux send-keys line length limits.
    # The PATH can be very long in test environments, causing command truncation.
    script_content = f"""#!/bin/sh
trap 'echo $? > {shlex.quote(str(exit_code_file))}' EXIT
export PATH={shlex.quote(env.env["PATH"])}
export TMPDIR={shlex.quote(env.env.get("TMPDIR", "/tmp"))}
export HOME={shlex.quote(env.env.get("HOME", ""))}
export WORKMUX_TEST=1
cd {shlex.quote(str(workdir))}
{pipe_cmd}{shlex.quote(str(workmux_exe_path))} {command} > {shlex.quote(str(stdout_file))} 2> {shlex.quote(str(stderr_file))}
"""
    script_file.write_text(script_content)
    script_file.chmod(0o755)

    # Execute the script - this keeps the send_keys command short
    env.send_keys("test:", str(script_file), enter=True)

    if not poll_until_file_has_content(exit_code_file, timeout=10.0):
        # Capture pane content for debugging
        pane_content = env.capture_pane("test") or "(empty)"
        raise AssertionError(
            f"workmux command did not complete in time\nPane content:\n{pane_content}"
        )

    result = WorkmuxCommandResult(
        exit_code=int(exit_code_file.read_text().strip()),
        stdout=stdout_file.read_text() if stdout_file.exists() else "",
        stderr=stderr_file.read_text() if stderr_file.exists() else "",
    )

    if expect_fail:
        if result.exit_code == 0:
            raise AssertionError(
                f"workmux {command} was expected to fail but succeeded.\nStdout:\n{result.stdout}"
            )
    else:
        if result.exit_code != 0:
            raise AssertionError(
                f"workmux {command} failed with exit code {result.exit_code}\n{result.stderr}"
            )

    return result


def run_workmux_add(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch_name: str,
    pre_run_mux_cmds: Optional[List[List[str]]] = None,
    *,
    base: Optional[str] = None,
    background: bool = False,
) -> None:
    """
    Helper to run `workmux add` command inside the isolated multiplexer session.

    Asserts that the command completes successfully.

    Args:
        env: The isolated multiplexer environment
        workmux_exe_path: Path to the workmux executable
        repo_path: Path to the git repository
        branch_name: Name of the branch/worktree to create
        pre_run_mux_cmds: Optional list of mux commands to run before workmux add
        base: Optional base branch for the new worktree (passed as `--base`)
        background: If True, pass `--background` so the window is created without focus
    """
    args = ["add", branch_name]
    if base:
        args.extend(["--base", base])
    if background:
        args.append("--background")

    command = " ".join(args)

    run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        command,
        pre_run_mux_cmds=pre_run_mux_cmds,
    )


def run_workmux_open(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch_name: Optional[str] = None,
    *,
    run_hooks: bool = False,
    force_files: bool = False,
    new_window: bool = False,
    session: bool = False,
    prompt: Optional[str] = None,
    prompt_file: Optional[Path] = None,
    pre_run_mux_cmds: Optional[List[List[str]]] = None,
    expect_fail: bool = False,
    working_dir: Optional[Path] = None,
) -> WorkmuxCommandResult:
    """
    Helper to run `workmux open` command inside the isolated multiplexer session.

    Returns the command result so tests can assert on stdout/stderr.

    Args:
        branch_name: Worktree name to open (optional with --new, uses current directory)
        new_window: If True, pass --new to force opening a new window (creates suffix like -2, -3)
        session: If True, pass -s to force opening as a tmux session
        prompt: Inline prompt text to pass via -p
        prompt_file: Path to a prompt file to pass via -P
        working_dir: Optional directory to run the command from (defaults to repo_path)
    """
    flags: List[str] = []
    if run_hooks:
        flags.append("--run-hooks")
    if force_files:
        flags.append("--force-files")
    if new_window:
        flags.append("--new")
    if session:
        flags.append("-s")
    if prompt:
        flags.append(f"-p {shlex.quote(prompt)}")
    if prompt_file:
        flags.append(f"-P {shlex.quote(str(prompt_file))}")

    flag_str = f" {' '.join(flags)}" if flags else ""
    name_part = f" {branch_name}" if branch_name else ""
    return run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"open{name_part}{flag_str}",
        pre_run_mux_cmds=pre_run_mux_cmds,
        expect_fail=expect_fail,
        working_dir=working_dir,
    )


def create_commit(env: MuxEnvironment, path: Path, message: str):
    """Creates and commits a file within the test env at a specific path."""
    (path / f"file_for_{message.replace(' ', '_').replace(':', '')}.txt").write_text(
        f"content for {message}"
    )
    env.run_command(["git", "add", "."], cwd=path)
    env.run_command(["git", "commit", "-m", message], cwd=path)


def create_dirty_file(path: Path, filename: str = "dirty.txt"):
    """Creates an uncommitted file."""
    (path / filename).write_text("uncommitted changes")


def run_workmux_remove(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch_name: Optional[str] = None,
    force: bool = False,
    keep_branch: bool = False,
    gone: bool = False,
    all: bool = False,
    user_input: Optional[str] = None,
    expect_fail: bool = False,
    from_window: Optional[str] = None,
) -> None:
    """
    Helper to run `workmux remove` command inside the isolated multiplexer session.

    Uses background execution to avoid hanging when remove kills its own window.
    Asserts that the command completes successfully unless expect_fail is True.

    Args:
        env: The isolated multiplexer environment
        workmux_exe_path: Path to the workmux executable
        repo_path: Path to the git repository
        branch_name: Optional name of the branch/worktree to remove (omit to auto-detect from current branch)
        force: Whether to use -f flag to skip confirmation
        keep_branch: Whether to use --keep-branch flag to keep the local branch
        gone: Whether to use --gone flag to remove worktrees with deleted upstreams
        all: Whether to use --all flag to remove all worktrees
        user_input: Optional string to pipe to stdin (e.g., 'y' for confirmation)
        expect_fail: If True, asserts the command fails (non-zero exit code)
        from_window: Optional window name to run the command from (useful for testing remove from within worktree window)
    """
    scripts_dir = get_scripts_dir(env)
    stdout_file = scripts_dir / "workmux_remove_stdout.txt"
    stderr_file = scripts_dir / "workmux_remove_stderr.txt"
    exit_code_file = scripts_dir / "workmux_remove_exit_code.txt"

    # Clean up any previous files
    for f in [stdout_file, stderr_file, exit_code_file]:
        if f.exists():
            f.unlink()

    force_flag = "-f " if force else ""
    keep_branch_flag = "--keep-branch " if keep_branch else ""
    gone_flag = "--gone " if gone else ""
    all_flag = "--all " if all else ""
    branch_arg = branch_name if branch_name else ""
    input_cmd = f"echo '{user_input}' | " if user_input else ""

    # If from_window is specified, we need to change to that window's working directory
    if from_window:
        worktree_path = get_worktree_path(
            repo_path, from_window.replace(DEFAULT_WINDOW_PREFIX, "")
        )
        remove_script = (
            f"cd {worktree_path} && "
            f"{input_cmd}"
            f"{workmux_exe_path} remove {force_flag}{keep_branch_flag}{gone_flag}{all_flag}{branch_arg} "
            f"> {stdout_file} 2> {stderr_file}; "
            f"echo $? > {exit_code_file}"
        )
    else:
        remove_script = (
            f"cd {repo_path} && "
            f"{input_cmd}"
            f"{workmux_exe_path} remove {force_flag}{keep_branch_flag}{gone_flag}{all_flag}{branch_arg} "
            f"> {stdout_file} 2> {stderr_file}; "
            f"echo $? > {exit_code_file}"
        )

    env.run_shell_background(remove_script)

    # Wait for command to complete (longer timeout for --gone which runs git fetch)
    assert poll_until_file_has_content(exit_code_file, timeout=15.0), (
        "workmux remove did not complete in time"
    )

    exit_code = int(exit_code_file.read_text().strip())
    stderr = stderr_file.read_text() if stderr_file.exists() else ""

    if expect_fail:
        if exit_code == 0:
            raise AssertionError(
                f"workmux remove was expected to fail but succeeded.\nStderr:\n{stderr}"
            )
    else:
        if exit_code != 0:
            raise AssertionError(
                f"workmux remove failed with exit code {exit_code}\nStderr:\n{stderr}"
            )


def run_workmux_merge(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch_name: Optional[str] = None,
    ignore_uncommitted: bool = False,
    rebase: bool = False,
    squash: bool = False,
    keep: bool = False,
    into: Optional[str] = None,
    no_verify: bool = False,
    no_hooks: bool = False,
    notification: bool = False,
    expect_fail: bool = False,
    from_window: Optional[str] = None,
) -> None:
    """
    Helper to run `workmux merge` command inside the isolated multiplexer session.

    Uses background execution to avoid hanging when merge kills its own window.
    Asserts that the command completes successfully unless expect_fail is True.

    Args:
        env: The isolated multiplexer environment
        workmux_exe_path: Path to the workmux executable
        repo_path: Path to the git repository
        branch_name: Optional name of the branch to merge (omit to auto-detect from current branch)
        ignore_uncommitted: Whether to use --ignore-uncommitted flag
        rebase: Whether to use --rebase flag
        squash: Whether to use --squash flag
        keep: Whether to use --keep flag
        into: Optional target branch to merge into (instead of main)
        no_verify: Whether to use --no-verify flag (skip pre-merge hooks)
        notification: Whether to use --notification flag (show system notification)
        expect_fail: If True, asserts the command fails (non-zero exit code)
        from_window: Optional window name to run the command from
    """
    scripts_dir = get_scripts_dir(env)
    stdout_file = scripts_dir / "workmux_merge_stdout.txt"
    stderr_file = scripts_dir / "workmux_merge_stderr.txt"
    exit_code_file = scripts_dir / "workmux_merge_exit_code.txt"

    for f in [stdout_file, stderr_file, exit_code_file]:
        if f.exists():
            f.unlink()

    flags = []
    if ignore_uncommitted:
        flags.append("--ignore-uncommitted")
    if rebase:
        flags.append("--rebase")
    if squash:
        flags.append("--squash")
    if keep:
        flags.append("--keep")
    if into:
        flags.append(f"--into {into}")
    if no_verify:
        flags.append("--no-verify")
    if no_hooks:
        flags.append("--no-hooks")
    if notification:
        flags.append("--notification")

    branch_arg = branch_name if branch_name else ""
    flags_str = " ".join(flags)

    if from_window:
        from_branch = from_window.replace(DEFAULT_WINDOW_PREFIX, "")
        worktree_path = get_worktree_path(repo_path, from_branch)
        workdir = worktree_path
    else:
        workdir = repo_path

    # Create a simple editor script for non-interactive git commits
    editor_script = scripts_dir / "git_editor.sh"
    editor_script.write_text('#!/bin/sh\necho "Auto commit from test" > "$1"\n')
    editor_script.chmod(0o755)

    merge_script = (
        f"export GIT_EDITOR={shlex.quote(str(editor_script))} && "
        f"cd {workdir} && "
        f"{workmux_exe_path} merge {flags_str} {branch_arg} "
        f"> {stdout_file} 2> {stderr_file}; "
        f"echo $? > {exit_code_file}"
    )

    env.run_shell_background(merge_script)

    assert poll_until_file_has_content(exit_code_file, timeout=10.0), (
        "workmux merge did not complete in time"
    )

    exit_code = int(exit_code_file.read_text().strip())
    stderr = stderr_file.read_text() if stderr_file.exists() else ""

    if expect_fail:
        if exit_code == 0:
            raise AssertionError(
                f"workmux merge was expected to fail but succeeded.\nStderr:\n{stderr}"
            )
    else:
        if exit_code != 0:
            raise AssertionError(
                f"workmux merge failed with exit code {exit_code}\nStderr:\n{stderr}"
            )


def install_fake_gh_cli(
    env: MuxEnvironment,
    pr_number: int,
    json_response: Optional[Dict[str, Any]] = None,
    stderr: str = "",
    exit_code: int = 0,
):
    """
    Creates a fake 'gh' command that responds to 'pr view <number> --json' with controlled output.

    Args:
        env: The isolated multiplexer environment
        pr_number: The PR number to respond to
        json_response: Dict containing the PR data to return as JSON (or None to return error)
        stderr: Error message to output to stderr
        exit_code: Exit code for the fake gh command (0 for success, non-zero for error)
    """
    import json

    # Create a bin directory in the test home
    bin_dir = env.home_path / "bin"
    bin_dir.mkdir(exist_ok=True)

    # Create the fake gh script
    gh_script = bin_dir / "gh"

    # Build the script content
    json_output = json.dumps(json_response) if json_response else ""

    # Escape single quotes in JSON for shell script
    json_output_escaped = json_output.replace("'", "'\\''")

    script_content = f"""#!/bin/sh
# Fake gh CLI for testing

# Check if this is a 'pr view' command for our PR number
# The command will be: gh pr view {pr_number} --json fields...
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "{pr_number}" ]; then
    if [ {exit_code} -ne 0 ]; then
        echo "{stderr}" >&2
        exit {exit_code}
    fi
    echo '{json_output_escaped}'
    exit 0
fi

# For any other command, fail
echo "gh: command not implemented in fake" >&2
exit 1
"""

    gh_script.write_text(script_content)
    gh_script.chmod(0o755)

    # Add the bin directory to PATH
    new_path = f"{bin_dir}:{env.env.get('PATH', '')}"
    env.env["PATH"] = new_path
    # Set PATH in the multiplexer session so workmux can find the fake gh
    env.set_session_env("PATH", new_path)


def pytest_report_teststatus(report):
    """Suppress progress dots when running in Claude Code."""
    import os

    if os.environ.get("CLAUDECODE") and report.when == "call" and report.passed:
        return report.outcome, "", report.outcome.upper()
