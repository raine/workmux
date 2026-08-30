"""Tests for `workmux setup` command."""

import json
from pathlib import Path

from .conftest import (
    MuxEnvironment,
    run_workmux_command,
)
from .support.setup import run_setup_with_answers, write_claude_manual_status_hook


# ---------------------------------------------------------------------------
# Non-interactive tests (no prompt expected)
# ---------------------------------------------------------------------------


class TestSetupNoPrompt:
    """Tests where setup exits without prompting for input."""

    def test_no_agents_detected(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Prints message when no agent directories exist."""
        result = run_workmux_command(mux_server, workmux_exe_path, repo_path, "setup")
        assert "No agents detected" in result.stdout

    def test_claude_hooks_already_configured(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Claude with manual hooks shows all-configured message."""
        write_claude_manual_status_hook(mux_server.home_path / ".claude")

        result = run_workmux_command(
            mux_server, workmux_exe_path, repo_path, "setup --hooks"
        )
        assert "All agents have status tracking configured" in result.stdout

    def test_claude_plugin_enabled(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Claude with enabled plugin shows all-configured message."""
        claude_dir = mux_server.home_path / ".claude"
        claude_dir.mkdir()
        settings = {"enabledPlugins": {"workmux-status@workmux": True}}
        (claude_dir / "settings.json").write_text(json.dumps(settings))

        result = run_workmux_command(
            mux_server, workmux_exe_path, repo_path, "setup --hooks"
        )
        assert "All agents have status tracking configured" in result.stdout

    def test_claude_plugin_disabled_uses_manual_hook_status(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """A disabled plugin does not hide a complete manual integration."""
        claude_dir = mux_server.home_path / ".claude"
        write_claude_manual_status_hook(claude_dir)
        settings_path = claude_dir / "settings.json"
        settings = json.loads(settings_path.read_text())
        settings["enabledPlugins"] = {"workmux-status@workmux": False}
        settings_path.write_text(json.dumps(settings))

        result = run_workmux_command(
            mux_server, workmux_exe_path, repo_path, "setup --hooks"
        )
        assert "All agents have status tracking configured" in result.stdout

    def test_opencode_plugin_configured(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """OpenCode with the bundled plugin shows all-configured message."""
        plugin_dir = mux_server.home_path / ".config" / "opencode" / "plugins"
        plugin_dir.mkdir(parents=True)
        bundled_plugin = (
            Path(__file__).parent.parent
            / "resources"
            / "opencode"
            / "plugins"
            / "workmux-status.ts"
        )
        (plugin_dir / "workmux-status.ts").write_text(bundled_plugin.read_text())

        result = run_workmux_command(
            mux_server, workmux_exe_path, repo_path, "setup --hooks"
        )
        assert "All agents have status tracking configured" in result.stdout

    def test_omp_extension_configured(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """OMP with extension file shows all-configured message."""
        extension_dir = mux_server.home_path / ".omp" / "agent" / "extensions"
        extension_dir.mkdir(parents=True)
        bundled_extension = (
            Path(__file__).parent.parent
            / "resources"
            / "omp"
            / "extensions"
            / "workmux-status.ts"
        )
        (extension_dir / "workmux-status.ts").write_text(bundled_extension.read_text())

        result = run_workmux_command(
            mux_server, workmux_exe_path, repo_path, "setup --hooks"
        )
        assert "All agents have status tracking configured" in result.stdout

    def test_both_agents_configured(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Both agents configured shows all-configured message."""
        write_claude_manual_status_hook(mux_server.home_path / ".claude")
        plugin_dir = mux_server.home_path / ".config" / "opencode" / "plugins"
        plugin_dir.mkdir(parents=True)
        bundled_plugin = (
            Path(__file__).parent.parent
            / "resources"
            / "opencode"
            / "plugins"
            / "workmux-status.ts"
        )
        (plugin_dir / "workmux-status.ts").write_text(bundled_plugin.read_text())

        result = run_workmux_command(
            mux_server, workmux_exe_path, repo_path, "setup --hooks"
        )
        assert "All agents have status tracking configured" in result.stdout

    def test_requires_interactive_terminal(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Fails when stdin is piped (not a terminal)."""
        result = run_workmux_command(
            mux_server,
            workmux_exe_path,
            repo_path,
            "setup",
            stdin_input="y\n",
            expect_fail=True,
        )
        assert "interactive terminal" in result.stderr


# ---------------------------------------------------------------------------
# Interactive tests (prompt for Y/n)
# ---------------------------------------------------------------------------


class TestSetupInstall:
    """Tests that exercise the interactive install prompt."""

    def test_outdated_claude_hooks_explain_update_prompt(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
    ):
        claude_dir = mux_server.home_path / ".claude"
        claude_dir.mkdir()
        (claude_dir / "settings.json").write_text(
            json.dumps(
                {
                    "hooks": {
                        "UserPromptSubmit": [
                            {
                                "hooks": [
                                    {
                                        "type": "command",
                                        "command": "workmux set-window-status working",
                                    }
                                ]
                            }
                        ],
                        "Notification": [
                            {
                                "matcher": "permission_prompt|elicitation_dialog",
                                "hooks": [
                                    {
                                        "type": "command",
                                        "command": "workmux set-window-status waiting",
                                    }
                                ],
                            }
                        ],
                        "PostToolUse": [
                            {
                                "hooks": [
                                    {
                                        "type": "command",
                                        "command": "workmux set-window-status working",
                                    }
                                ]
                            }
                        ],
                        "Stop": [
                            {
                                "hooks": [
                                    {
                                        "type": "command",
                                        "command": "workmux set-window-status done",
                                    }
                                ]
                            }
                        ],
                    }
                }
            )
        )

        run_setup_with_answers(
            mux_server,
            workmux_exe_path,
            expected_output=("workmux register-agent",),
        )

    def test_outdated_opencode_plugin_explains_update_prompt(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
    ):
        plugin_dir = mux_server.home_path / ".config" / "opencode" / "plugins"
        plugin_dir.mkdir(parents=True)
        bundled_plugin = (
            Path(__file__).parent.parent
            / "resources"
            / "opencode"
            / "plugins"
            / "workmux-status.ts"
        ).read_text()
        registration = """  try {
    await $`workmux register-agent`.quiet();
  } catch {
    // Status tracking remains available when registration cannot reach workmux.
  }

"""
        (plugin_dir / "workmux-status.ts").write_text(
            bundled_plugin.replace(registration, "")
        )

        run_setup_with_answers(
            mux_server,
            workmux_exe_path,
            expected_output=("workmux register-agent",),
        )

        assert (
            "workmux register-agent" in (plugin_dir / "workmux-status.ts").read_text()
        )

    def test_claude_install_accept(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Answering 'y' installs hooks into settings.json."""
        claude_dir = mux_server.home_path / ".claude"
        claude_dir.mkdir()

        run_setup_with_answers(mux_server, workmux_exe_path, hooks_answer="y")

        settings_path = claude_dir / "settings.json"
        assert settings_path.exists()
        settings = json.loads(settings_path.read_text())
        assert "hooks" in settings
        assert "UserPromptSubmit" in settings["hooks"]
        assert "Notification" in settings["hooks"]
        assert "PostToolUse" in settings["hooks"]
        assert "Stop" in settings["hooks"]

    def test_claude_install_default_enter(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Pressing Enter accepts installation (default is Y)."""
        claude_dir = mux_server.home_path / ".claude"
        claude_dir.mkdir()

        run_setup_with_answers(mux_server, workmux_exe_path, hooks_answer="")

        settings_path = claude_dir / "settings.json"
        assert settings_path.exists()
        settings = json.loads(settings_path.read_text())
        assert "hooks" in settings

    def test_claude_install_decline(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Answering 'n' skips installation."""
        claude_dir = mux_server.home_path / ".claude"
        claude_dir.mkdir()

        run_setup_with_answers(mux_server, workmux_exe_path, hooks_answer="n")

        settings_path = claude_dir / "settings.json"
        assert not settings_path.exists()

    def test_opencode_install_accept(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Accepting installs the OpenCode plugin file."""
        opencode_dir = mux_server.home_path / ".config" / "opencode"
        opencode_dir.mkdir(parents=True)

        run_setup_with_answers(mux_server, workmux_exe_path, hooks_answer="y")

        plugin_path = opencode_dir / "plugins" / "workmux-status.ts"
        assert plugin_path.exists()
        assert not (opencode_dir / "package.json").exists()
        plugin_text = plugin_path.read_text()
        assert "workmux register-agent" in plugin_text

    def test_omp_install_accept(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Accepting installs OMP extension file."""
        omp_dir = mux_server.home_path / ".omp" / "agent"
        omp_dir.mkdir(parents=True)

        run_setup_with_answers(mux_server, workmux_exe_path, hooks_answer="y")

        extension_path = omp_dir / "extensions" / "workmux-status.ts"
        assert extension_path.exists()
        extension_text = extension_path.read_text()
        assert "@oh-my-pi/pi-coding-agent" in extension_text
        assert 'workmux", ["set-window-status' in extension_text
        assert 'pi.on("session_start"' in extension_text
        assert '["register-agent"]' in extension_text
        assert 'pi.on("message_end"' in extension_text
        assert '"role" in event.message' in extension_text
        assert 'event.message.role === "assistant"' in extension_text
        assert 'event.toolName === "ask"' in extension_text
        assert 'setStatus("waiting")' in extension_text

    def test_both_agents_install_accept(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Accepting installs both agents' hooks."""
        claude_dir = mux_server.home_path / ".claude"
        claude_dir.mkdir()
        opencode_dir = mux_server.home_path / ".config" / "opencode"
        opencode_dir.mkdir(parents=True)

        run_setup_with_answers(mux_server, workmux_exe_path, hooks_answer="y")

        settings_path = claude_dir / "settings.json"
        assert settings_path.exists()
        settings = json.loads(settings_path.read_text())
        assert "hooks" in settings
        assert "SessionStart" in settings["hooks"]
        assert "Stop" in settings["hooks"]

        plugin_path = opencode_dir / "plugins" / "workmux-status.ts"
        assert plugin_path.exists()
        assert not (opencode_dir / "package.json").exists()

    def test_claude_preserves_existing_settings(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Installing hooks preserves existing settings.json content."""
        claude_dir = mux_server.home_path / ".claude"
        claude_dir.mkdir()
        existing = {
            "permissions": {"allow": ["Bash"]},
            "hooks": {
                "Stop": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": "afplay /System/Library/Sounds/Glass.aiff",
                            }
                        ]
                    }
                ]
            },
        }
        (claude_dir / "settings.json").write_text(json.dumps(existing, indent=2))

        run_setup_with_answers(mux_server, workmux_exe_path, hooks_answer="y")

        settings = json.loads((claude_dir / "settings.json").read_text())
        assert "permissions" in settings
        assert settings["permissions"]["allow"] == ["Bash"]
        stop_commands = [
            hook.get("command", "")
            for group in settings["hooks"]["Stop"]
            for hook in group.get("hooks", [])
        ]
        assert "afplay /System/Library/Sounds/Glass.aiff" in stop_commands
        assert "workmux set-window-status done" in stop_commands
        assert settings["hooks"]["SessionStart"][0]["matcher"] == (
            "startup|resume|clear|fork"
        )
        assert settings["hooks"]["SessionStart"][0]["hooks"][0]["command"] == (
            "workmux register-agent"
        )
        assert "UserPromptSubmit" in settings["hooks"]
        assert "Notification" in settings["hooks"]
        assert "PostToolUse" in settings["hooks"]
