import json
import os
import re
from pathlib import Path
from typing import Dict, List

from .conftest import (
    MuxEnvironment,
    create_commit,
    get_window_name,
    get_worktree_path,
    run_workmux_add,
    run_workmux_command,
    write_workmux_config,
)


def run_workmux_list(
    env: MuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    args: str = "",
) -> str:
    """
    Runs `workmux list` inside the multiplexer session and returns the output.
    """
    command = f"list {args}".strip()
    result = run_workmux_command(env, workmux_exe_path, repo_path, command)
    return result.stdout


def parse_list_output(output: str) -> List[Dict[str, str]]:
    """
    Parses the tabular output of `workmux list` into a list of dictionaries.
    This parser is robust to variable column widths.
    """
    lines = [line.rstrip() for line in output.strip().split("\n")]
    if len(lines) < 1:  # Header at minimum
        return []

    header = lines[0]
    # Use regex to find column headers, robust against extra spaces
    columns = re.split(r"\s{2,}", header.strip())
    columns = [c.strip() for c in columns if c.strip()]

    # Find the start index of each column in the header string
    indices = [header.find(col) for col in columns]

    results = []
    # Data rows start after the header (no separator line in blank style)
    for row_str in lines[1:]:
        if not row_str.strip():  # Skip empty lines
            continue
        row_data = {}
        for i, col_name in enumerate(columns):
            start = indices[i]
            # The last column goes to the end of the line
            end = indices[i + 1] if i + 1 < len(indices) else len(row_str)
            value = row_str[start:end].strip()
            row_data[col_name] = value
        results.append(row_data)

    return results


def install_fake_gh_pr_list_retry(env: MuxEnvironment, branch_name: str) -> None:
    """Install a fake gh that fails check rollup queries and serves PR metadata."""
    gh_path = env.fake_bin_dir / "gh"
    pr_json = json.dumps(
        [
            {
                "number": 77,
                "title": "Keep PR metadata available",
                "state": "OPEN",
                "isDraft": False,
                "headRefName": branch_name,
                "url": "https://github.example.test/test/repo/pull/77",
            }
        ]
    )
    gh_path.write_text(
        f"""#!/usr/bin/env python3
import sys

if sys.argv[1:3] == ["pr", "list"]:
    if any("statusCheckRollup" in arg for arg in sys.argv):
        print("upstream timed out while resolving statusCheckRollup", file=sys.stderr)
        sys.exit(1)
    print({pr_json!r})
    sys.exit(0)

print("unexpected gh command: " + " ".join(sys.argv[1:]), file=sys.stderr)
sys.exit(1)
"""
    )
    gh_path.chmod(0o755)


def write_agent_state_file(env: MuxEnvironment, worktree_path: Path, status: str):
    """
    Creates a fake agent state file in XDG_STATE_HOME to simulate an active agent.
    The state file uses the worktree path as the workdir so it gets matched
    by the list command.
    """
    state_dir = Path(env.env["XDG_STATE_HOME"]) / "workmux" / "agents"
    state_dir.mkdir(parents=True, exist_ok=True)

    # Use a unique pane key based on the worktree path to avoid collisions
    pane_id = f"%{abs(hash(str(worktree_path))) % 1000}"
    state = {
        "pane_key": {
            "backend": "tmux",
            "instance": "test",
            "pane_id": pane_id,
        },
        "workdir": str(worktree_path),
        "status": status,
        "status_ts": 1234567890,
        "pane_title": None,
        "pane_pid": 12345,
        "command": "node",
        "updated_ts": 1234567890,
    }

    # Percent-encode % in pane_id for filename safety
    safe_pane_id = pane_id.replace("%", "%25")
    filename = f"tmux__test__{safe_pane_id}.json"
    state_file = state_dir / filename
    state_file.write_text(json.dumps(state))
    return state_file


def test_list_output_format(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux list` produces correctly formatted table output."""
    env = mux_server
    branch_name = "feature-test"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    output = run_workmux_list(env, workmux_exe_path, mux_repo_path)
    worktree_path = get_worktree_path(mux_repo_path, branch_name)

    # Parse and verify the output contains the expected data
    parsed_output = parse_list_output(output)
    assert len(parsed_output) == 2

    # Verify header is present
    assert "BRANCH" in output
    assert "AGE" in output
    assert "AGENT" in output
    assert "MUX" in output
    assert "UNMERGED" in output
    assert "PATH" in output

    # Verify main branch entry - should show "(here)" when run from mux_repo_path
    main_entry = next((r for r in parsed_output if r["BRANCH"] == "main"), None)
    assert main_entry is not None
    assert main_entry["AGE"] == "-"
    assert main_entry["AGENT"] == "-"
    assert main_entry["MUX"] == "-"
    assert main_entry["UNMERGED"] == "-"
    assert main_entry["PATH"] == "(here)"

    # Verify feature branch entry - shows as relative path
    feature_entry = next((r for r in parsed_output if r["BRANCH"] == branch_name), None)
    assert feature_entry is not None
    assert feature_entry["AGE"] != ""  # age is populated (value depends on timing)
    assert feature_entry["AGENT"] == "-"
    assert feature_entry["MUX"] == "✓"
    assert feature_entry["UNMERGED"] == "-"
    # Convert relative path to absolute and compare
    expected_relative = os.path.relpath(worktree_path, mux_repo_path)
    assert feature_entry["PATH"] == expected_relative


def test_list_initial_state(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `workmux list` shows only the main branch in a new repo."""
    env = mux_server

    output = run_workmux_list(env, workmux_exe_path, mux_repo_path)
    parsed_output = parse_list_output(output)
    assert len(parsed_output) == 1

    main_entry = parsed_output[0]
    assert main_entry["BRANCH"] == "main"
    assert main_entry["AGE"] == "-"
    assert main_entry["AGENT"] == "-"
    assert main_entry["MUX"] == "-"
    assert main_entry["UNMERGED"] == "-"
    # When run from mux_repo_path, main branch shows as "(here)"
    assert main_entry["PATH"] == "(here)"


def test_list_with_active_worktree(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `list` shows an active worktree with a multiplexer window ('✓')."""
    env = mux_server
    branch_name = "feature-active"
    write_workmux_config(mux_repo_path)

    # Create the worktree and window
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    output = run_workmux_list(env, workmux_exe_path, mux_repo_path)
    parsed_output = parse_list_output(output)
    assert len(parsed_output) == 2

    worktree_entry = next(
        (r for r in parsed_output if r["BRANCH"] == branch_name), None
    )
    assert worktree_entry is not None
    assert worktree_entry["AGENT"] == "-"
    assert worktree_entry["MUX"] == "✓"
    assert worktree_entry["UNMERGED"] == "-"
    # Path shows as relative when run from mux_repo_path
    expected_path = get_worktree_path(mux_repo_path, branch_name)
    expected_relative = os.path.relpath(expected_path, mux_repo_path)
    assert worktree_entry["PATH"] == expected_relative


def test_list_pr_falls_back_without_status_check_rollup(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `list --pr` keeps PR metadata when check rollup fails."""
    env = mux_server
    branch_name = "feature-pr-fallback"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)
    install_fake_gh_pr_list_retry(env, branch_name)

    output = run_workmux_list(env, workmux_exe_path, mux_repo_path, "--pr")
    parsed_output = parse_list_output(output)

    worktree_entry = next(
        (r for r in parsed_output if r["BRANCH"] == branch_name), None
    )
    assert worktree_entry is not None
    assert worktree_entry["PR"].startswith("#77")


def test_list_with_unmerged_commits(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `list` shows a worktree with unmerged commits ('●')."""
    env = mux_server
    branch_name = "feature-unmerged"
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    # Create a commit only on the feature branch
    create_commit(env, worktree_path, "This commit is unmerged")

    output = run_workmux_list(env, workmux_exe_path, mux_repo_path)
    parsed_output = parse_list_output(output)
    worktree_entry = next(
        (r for r in parsed_output if r["BRANCH"] == branch_name), None
    )
    assert worktree_entry is not None
    assert worktree_entry["MUX"] == "✓"
    assert worktree_entry["UNMERGED"] == "●"


def test_list_with_detached_window(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies `list` shows a worktree whose window has been closed ('-')."""
    env = mux_server
    branch_name = "feature-detached"
    window_name = get_window_name(branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    # Kill the window manually
    env.kill_window(window_name)

    output = run_workmux_list(env, workmux_exe_path, mux_repo_path)
    parsed_output = parse_list_output(output)
    worktree_entry = next(
        (r for r in parsed_output if r["BRANCH"] == branch_name), None
    )
    assert worktree_entry is not None
    assert worktree_entry["MUX"] == "-"
    assert worktree_entry["UNMERGED"] == "-"


def test_list_alias_ls_works(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies that the `ls` alias for `list` works correctly."""
    env = mux_server

    # Run `ls` and verify it produces expected output
    result = run_workmux_command(env, workmux_exe_path, mux_repo_path, "ls")
    ls_output = result.stdout

    parsed_output = parse_list_output(ls_output)
    assert len(parsed_output) == 1
    assert parsed_output[0]["BRANCH"] == "main"
    # When run from mux_repo_path, main branch shows as "(here)"
    assert parsed_output[0]["PATH"] == "(here)"


def test_list_agent_status_no_agents(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies AGENT column shows '-' when no agents are running."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-no-agent")

    output = run_workmux_list(env, workmux_exe_path, mux_repo_path)
    parsed_output = parse_list_output(output)

    for entry in parsed_output:
        assert entry["AGENT"] == "-"


def test_list_agent_status_with_state_file(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies AGENT column shows status when a state file exists.

    Note: Since the test mux backend/instance won't match the fake state file's
    backend/instance, load_reconciled_agents filters them out. This test verifies
    the column is present and defaults to '-' for non-matching agents.
    """
    env = mux_server
    branch_name = "feature-with-agent"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    write_agent_state_file(env, worktree_path, "working")

    output = run_workmux_list(env, workmux_exe_path, mux_repo_path)
    parsed_output = parse_list_output(output)
    assert "AGENT" in output

    # Agent state file has a different backend/instance than the test environment,
    # so load_reconciled_agents won't match it. This verifies graceful fallback.
    feature_entry = next((r for r in parsed_output if r["BRANCH"] == branch_name), None)
    assert feature_entry is not None
    assert feature_entry["AGENT"] == "-"


def test_list_filter_by_branch(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies filtering list output by branch name."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-alpha")
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-beta")

    # Filter to only show feature-alpha
    output = run_workmux_list(env, workmux_exe_path, mux_repo_path, "feature-alpha")
    parsed_output = parse_list_output(output)

    assert len(parsed_output) == 1
    assert parsed_output[0]["BRANCH"] == "feature-alpha"


def test_list_filter_by_handle(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies filtering list output by worktree handle (directory name)."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-gamma")
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-delta")

    # Filter by handle (slugified branch name)
    output = run_workmux_list(env, workmux_exe_path, mux_repo_path, "feature-gamma")
    parsed_output = parse_list_output(output)

    assert len(parsed_output) == 1
    assert parsed_output[0]["BRANCH"] == "feature-gamma"


def test_list_filter_multiple(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies filtering with multiple branch names."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-one")
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-two")
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-three")

    # Filter to show two specific branches
    output = run_workmux_list(
        env, workmux_exe_path, mux_repo_path, "feature-one feature-three"
    )
    parsed_output = parse_list_output(output)

    assert len(parsed_output) == 2
    branches = {r["BRANCH"] for r in parsed_output}
    assert branches == {"feature-one", "feature-three"}


def test_list_filter_no_match(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies filtering with no matches shows 'No worktrees found'."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-exists")

    output = run_workmux_list(
        env, workmux_exe_path, mux_repo_path, "nonexistent-branch"
    )

    assert "No worktrees found" in output


def test_list_json_output(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies --json flag outputs valid JSON with expected fields."""
    env = mux_server
    branch_name = "feature-json"
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    output = run_workmux_list(env, workmux_exe_path, mux_repo_path, "--json")
    data = json.loads(output)

    assert isinstance(data, list)
    assert len(data) == 2

    # Verify all expected fields exist on each entry
    expected_fields = {
        "handle",
        "branch",
        "path",
        "is_main",
        "mode",
        "has_uncommitted_changes",
        "is_open",
        "created_at",
    }
    for entry in data:
        assert set(entry.keys()) == expected_fields

    # Verify main worktree entry
    main_entry = next(e for e in data if e["branch"] == "main")
    assert main_entry["is_main"] is True
    assert main_entry["mode"] == "window"
    # has_uncommitted_changes may be True due to config file written by write_workmux_config
    assert isinstance(main_entry["has_uncommitted_changes"], bool)
    assert main_entry["is_open"] is False

    # Verify feature worktree entry
    feature_entry = next(e for e in data if e["branch"] == branch_name)
    assert feature_entry["is_main"] is False
    assert feature_entry["mode"] == "window"
    assert feature_entry["has_uncommitted_changes"] is False
    assert feature_entry["is_open"] is True
    assert feature_entry["path"] == str(get_worktree_path(mux_repo_path, branch_name))


def test_list_json_empty(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies --json with no matches outputs empty JSON array."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-json-empty")

    output = run_workmux_list(
        env, workmux_exe_path, mux_repo_path, "--json nonexistent-branch"
    )
    data = json.loads(output)
    assert data == []


def test_list_json_with_uncommitted_changes(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies --json reports has_uncommitted_changes correctly."""
    env = mux_server
    branch_name = "feature-json-dirty"
    worktree_path = get_worktree_path(mux_repo_path, branch_name)
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, branch_name)

    # Create an uncommitted file in the worktree
    (worktree_path / "dirty-file.txt").write_text("uncommitted change")

    output = run_workmux_list(
        env, workmux_exe_path, mux_repo_path, f"--json {branch_name}"
    )
    data = json.loads(output)
    assert len(data) == 1
    assert data[0]["has_uncommitted_changes"] is True


def test_list_json_with_filter(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Verifies --json works with filters."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-json-a")
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-json-b")

    output = run_workmux_list(
        env, workmux_exe_path, mux_repo_path, "--json feature-json-a"
    )
    data = json.loads(output)
    assert len(data) == 1
    assert data[0]["branch"] == "feature-json-a"
