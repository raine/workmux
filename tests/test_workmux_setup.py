"""Tests for `workmux setup` command."""

import json
from pathlib import Path

import pytest

from .conftest import (
    MuxEnvironment,
    run_workmux_command,
)
from .support.setup import (
    install_opencode_version,
    run_setup_with_answers,
    write_claude_manual_status_hook,
)

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
        """OpenCode V1 with the original plugin shows all-configured message."""
        install_opencode_version(mux_server, "v1.18.29")
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

    def test_opencode_v2_plugin_configured(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """The V2 package files are recognized without prompting."""
        install_opencode_version(mux_server, "v2.0.18")
        plugin_dir = (
            mux_server.home_path / ".config" / "opencode" / "plugins" / "workmux-status"
        )
        plugin_dir.mkdir(parents=True)
        bundled = (
            Path(__file__).parent.parent
            / "resources"
            / "opencode"
            / "v2"
            / "workmux-status"
        )
        for name in ("index.ts", "tui.ts", "status.ts"):
            (plugin_dir / name).write_text((bundled / name).read_text())

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
        install_opencode_version(mux_server, "v1.18.29")
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
        install_opencode_version(mux_server, "v1.18.29")
        plugin_dir = mux_server.home_path / ".config" / "opencode" / "plugins"
        plugin_dir.mkdir(parents=True)
        historical_plugin = (
            Path(__file__).parent.parent
            / "tests"
            / "fixtures"
            / "opencode"
            / "v1-0b0c3875.ts"
        ).read_text()
        (plugin_dir / "workmux-status.ts").write_text(historical_plugin)

        # This historical version produces a long diff; its trailing added
        # handler remains visible when the confirmation prompt is displayed.
        run_setup_with_answers(
            mux_server,
            workmux_exe_path,
            expected_output=("case 'session.deleted':",),
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
        """Accepting installs the original plugin only for OpenCode V1."""
        install_opencode_version(mux_server, "v1.18.29")
        opencode_dir = mux_server.home_path / ".config" / "opencode"
        opencode_dir.mkdir(parents=True)

        run_setup_with_answers(mux_server, workmux_exe_path, hooks_answer="y")

        plugin_path = opencode_dir / "plugins" / "workmux-status.ts"
        assert plugin_path.exists()
        assert not (opencode_dir / "package.json").exists()
        plugin_text = plugin_path.read_text()
        assert "workmux register-agent" in plugin_text

    @pytest.mark.parametrize(
        "v1_source",
        [
            "resources/opencode/plugins/workmux-status.ts",
            "tests/fixtures/opencode/v1-0b0c3875.ts",
        ],
        ids=["current-v1", "historical-v1"],
    )
    def test_opencode_v2_install_replaces_v1_entrypoint(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
        v1_source: str,
    ):
        """V2 setup removes only the conflicting workmux V1 entrypoint."""
        install_opencode_version(mux_server, "v2.0.18")
        opencode_dir = mux_server.home_path / ".config" / "opencode"
        plugins = opencode_dir / "plugins"
        plugins.mkdir(parents=True)
        bundled_v1 = Path(__file__).parent.parent / v1_source
        (plugins / "workmux-status.ts").write_text(bundled_v1.read_text())
        (plugins / "custom.ts").write_text("// keep me")

        run_setup_with_answers(mux_server, workmux_exe_path, hooks_answer="y")

        bundled_v2 = (
            Path(__file__).parent.parent
            / "resources"
            / "opencode"
            / "v2"
            / "workmux-status"
        )
        installed_v2 = plugins / "workmux-status"
        for name in ("index.ts", "tui.ts", "status.ts"):
            assert (installed_v2 / name).read_text() == (bundled_v2 / name).read_text()
        assert not (plugins / "workmux-status.ts").exists()
        assert (plugins / "custom.ts").read_text() == "// keep me"

        result = run_workmux_command(
            mux_server, workmux_exe_path, repo_path, "setup --hooks"
        )
        assert "All agents have status tracking configured" in result.stdout

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
        assert '["set-window-status", status]' in extension_text
        assert 'pi.on("session_start"' in extension_text
        assert '["register-agent"]' in extension_text
        assert 'pi.on("message_end"' not in extension_text
        assert 'event.toolName === "ask"' in extension_text
        assert 'setStatus("waiting")' in extension_text

    def test_both_agents_install_accept(
        self,
        mux_server: MuxEnvironment,
        workmux_exe_path: Path,
        repo_path: Path,
    ):
        """Accepting installs both agents' hooks."""
        install_opencode_version(mux_server, "v1.18.29")
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
