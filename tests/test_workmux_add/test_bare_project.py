"""Configuration project names are independent of repository path anchors."""

from pathlib import Path

import pytest
import yaml

from ..conftest import (
    MuxEnvironment,
    assert_window_exists,
    run_workmux_command,
)


@pytest.mark.parametrize(
    "layout,bare,project",
    [
        ("northwind", False, "northwind"),
        (".northwind", False, ".northwind"),
        ("northwind.git", False, "northwind.git"),
        ("northwind", True, "northwind"),
        ("northwind/.bare", True, "northwind"),
        ("northwind/.anything", True, "northwind"),
        ("northwind/.git", True, "northwind"),
        ("northwind.git", True, "northwind"),
        ("northwind.git/.git", True, "northwind.git"),
        (".northwind/.git", True, ".northwind"),
    ],
)
def test_project_templates_preserve_path_anchors(
    mux_server: MuxEnvironment,
    workmux_exe_path: Path,
    layout: str,
    bare: bool,
    project: str,
):
    env = mux_server
    repo = env.tmp_path / "layouts" / layout
    env.run_command(
        ["git", "init", "-b", "main", *(["--bare"] if bare else []), str(repo)]
    )
    env.run_command(["git", "config", "user.name", "Test"], cwd=repo)
    env.run_command(["git", "config", "user.email", "test@example.com"], cwd=repo)
    seed = env.tmp_path / "seed" if bare else repo
    if bare:
        env.run_command(["git", "worktree", "add", "-b", "main", str(seed)], cwd=repo)
    env.run_command(["git", "commit", "--allow-empty", "-m", "Initial"], cwd=seed)
    linked = env.tmp_path / "linked"
    env.run_command(["git", "worktree", "add", "-b", "linked", str(linked)], cwd=seed)
    nested = linked / "nested"
    nested.mkdir()

    anchor = repo.parent if bare and repo.name == ".git" else repo
    config = env.home_path / ".config" / "workmux" / "config.yaml"
    config.parent.mkdir(parents=True, exist_ok=True)

    def configure(template: str | None):
        settings = {
            "window_prefix": "{project}/",
            "main_branch": "main",
            "nerdfont": False,
            "panes": [{"command": ""}],
        }
        if template is not None:
            settings["worktree_dir"] = template
        config.write_text(yaml.safe_dump(settings))

    for template, expected_base in [
        ("../out/{project}", anchor.parent / "out" / project),
        ("../", anchor.parent),
        (None, anchor.parent / f"{anchor.name}__worktrees"),
    ]:
        configure(template)
        for cwd in (repo, linked, nested):
            result = run_workmux_command(
                env, workmux_exe_path, cwd, "add feature --dry-run --base main"
            )
            assert f"Worktree: {expected_base / 'feature'}" in result.stdout
            assert f"Target:   {project}/feature (window)" in result.stdout

    configure("../out/{project}")
    run_workmux_command(
        env, workmux_exe_path, linked, "add feature --base main --background"
    )
    worktree = anchor.parent / "out" / project / "feature"
    assert worktree.is_dir()
    assert_window_exists(env, f"{project}/feature")
    run_workmux_command(env, workmux_exe_path, worktree, "close feature")
    assert f"{project}/feature" not in env.list_windows()
