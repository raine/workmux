"""`workmux add` placement: remote placement fails before creating anything,
`--local` and configurations without remotes create normal local worktrees."""

from pathlib import Path
from typing import Any, Dict, Optional

import yaml

from ..conftest import (
    MuxEnvironment,
    assert_window_exists,
    get_window_name,
    get_worktree_path,
    run_workmux_command,
)

REMOTES = {"hetzner": {"host": "wm-remote-test"}, "box": {"host": "dev@box"}}


def configure(
    env: MuxEnvironment,
    repo_path: Path,
    remotes: Optional[Dict[str, Any]],
    placement: Optional[str],
) -> None:
    global_config: Dict[str, Any] = {"nerdfont": False}
    if remotes is not None:
        global_config["remotes"] = remotes
    config_dir = env.home_path / ".config" / "workmux"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.yaml").write_text(yaml.dump(global_config))
    project: Dict[str, Any] = {"nerdfont": False, "panes": [{"command": "true"}]}
    if placement is not None:
        project["placement"] = placement
    (repo_path / ".workmux.yaml").write_text(yaml.dump(project))


def assert_not_created(repo_path: Path, branch: str) -> None:
    assert not get_worktree_path(repo_path, branch).exists()
    branches = (repo_path / ".git" / "refs" / "heads").iterdir()
    assert branch not in [b.name for b in branches]


def test_remote_placement_fails_before_creating(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    configure(mux_server, mux_repo_path, REMOTES, placement="hetzner")
    result = run_workmux_command(
        mux_server, workmux_exe_path, mux_repo_path, "add feat", expect_fail=True
    )
    assert "remote agents are not available yet (remote 'hetzner')" in result.stderr
    assert "--local" in result.stderr
    assert_not_created(mux_repo_path, "feat")


def test_named_remote_flag_fails_clearly(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    configure(mux_server, mux_repo_path, REMOTES, placement=None)
    result = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "add feat --remote box",
        expect_fail=True,
    )
    assert "remote agents are not available yet (remote 'box')" in result.stderr
    assert_not_created(mux_repo_path, "feat")


def test_ambiguous_and_unknown_remote_flags(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    configure(mux_server, mux_repo_path, REMOTES, placement=None)
    ambiguous = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "add feat --remote",
        expect_fail=True,
    )
    assert "--remote needs a name" in ambiguous.stderr
    assert "box, hetzner" in ambiguous.stderr
    unknown = run_workmux_command(
        mux_server,
        workmux_exe_path,
        mux_repo_path,
        "add --remote feat other",
        expect_fail=True,
    )
    assert "Unknown remote 'feat'" in unknown.stderr
    assert "branch name" in unknown.stderr
    assert_not_created(mux_repo_path, "feat")
    assert_not_created(mux_repo_path, "other")


def test_local_flag_overrides_remote_placement(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    configure(mux_server, mux_repo_path, REMOTES, placement="hetzner")
    run_workmux_command(mux_server, workmux_exe_path, mux_repo_path, "add feat --local")
    assert get_worktree_path(mux_repo_path, "feat").is_dir()
    assert_window_exists(mux_server, get_window_name("feat"))


def test_placement_without_remotes_is_ignored(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """A project that sets `placement` must not affect users who configured
    no remotes."""
    configure(mux_server, mux_repo_path, remotes=None, placement="hetzner")
    run_workmux_command(mux_server, workmux_exe_path, mux_repo_path, "add feat")
    assert get_worktree_path(mux_repo_path, "feat").is_dir()
    assert_window_exists(mux_server, get_window_name("feat"))


def test_project_remotes_are_ignored(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """`remotes` is global-only: a project cannot define an SSH destination."""
    configure(mux_server, mux_repo_path, remotes=None, placement="evil")
    project = yaml.safe_load((mux_repo_path / ".workmux.yaml").read_text())
    project["remotes"] = {"evil": {"host": "attacker.example"}}
    (mux_repo_path / ".workmux.yaml").write_text(yaml.dump(project))
    run_workmux_command(mux_server, workmux_exe_path, mux_repo_path, "add feat")
    assert get_worktree_path(mux_repo_path, "feat").is_dir()
