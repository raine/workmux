import os
import re
import shlex
import subprocess
import tempfile
import time
import unicodedata
from pathlib import Path
from typing import Any, Callable, Dict, Generator, List, Optional

from dataclasses import dataclass

import pytest
import yaml


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


class TmuxEnvironment:
    """
    A helper class to manage the state of an isolated test environment.
    It controls a dedicated tmux server via a private socket file.
    """

    def __init__(self, tmp_path: Path):
        # The base directory for all temporary test files
        self.tmp_path = tmp_path

        # Create a dedicated home directory for the test to prevent
        # loading the user's real shell configuration (.zshrc, .bash_history, etc.)
        self.home_path = self.tmp_path / "test_home"
        self.home_path.mkdir()

        # Use a short socket path in /tmp to avoid macOS socket path length limits
        # Create a temporary file and use its name for the socket
        tmp_file = tempfile.NamedTemporaryFile(
            prefix="tmux_", suffix=".sock", delete=False
        )
        self.socket_path = Path(tmp_file.name)
        tmp_file.close()
        self.socket_path.unlink()  # Remove the file, we just want the path

        # Create a copy of the current environment variables
        self.env = os.environ.copy()

        # Ensure we never accidentally target the user's tmux server
        # This prevents any subprocess from connecting to the host tmux session
        self.env.pop("TMUX", None)

        # Force temporary directory to the isolated test path.
        # Rust's std::env::temp_dir() respects TMPDIR on Unix.
        self.env["TMPDIR"] = str(self.tmp_path)

        # Isolate the shell environment completely to prevent history pollution
        # and other side effects from user's shell configuration
        self.env["HOME"] = str(self.home_path)

        # Prevent tmux from loading the user's real ~/.tmux.conf file
        self.env["TMUX_CONF"] = "/dev/null"

        # Create a fake git editor for non-interactive commits
        # Git needs the commit message file to be modified, so we ensure it has content
        fake_editor_script = self.home_path / "fake_git_editor.sh"
        fake_editor_script.write_text(
            "#!/bin/sh\n"
            "# If the file is empty or only has comments, add a default message\n"
            'if ! grep -q "^[^#]" "$1" 2>/dev/null; then\n'
            '  echo "Test commit" > "$1"\n'
            "fi\n"
        )
        fake_editor_script.chmod(0o755)
        self.env["GIT_EDITOR"] = str(fake_editor_script)

    def run_command(
        self, cmd: list[str], check: bool = True, cwd: Optional[Path] = None
    ):
        """Runs a generic command within the isolated environment."""
        working_dir = cwd if cwd is not None else self.tmp_path
        return subprocess.run(
            cmd,
            cwd=working_dir,
            env=self.env,
            capture_output=True,
            text=True,
            check=check,
        )

    def tmux(self, tmux_args: list[str], check: bool = True):
        """
        Runs a tmux command targeting our isolated server.
        It explicitly uses the '-S' flag for clarity and robustness.
        """
        base_cmd = ["tmux", "-S", str(self.socket_path)]
        return self.run_command(base_cmd + tmux_args, check=check)


@pytest.fixture
def isolated_tmux_server(tmp_path: Path) -> Generator[TmuxEnvironment, None, None]:
    """
    A pytest fixture that provides a fully isolated tmux server for a single test.

    It performs the following steps:
    1. Creates a TmuxEnvironment instance.
    2. Starts a new, isolated tmux server process.
    3. Yields the environment manager to the test function.
    4. After the test runs, it kills the isolated tmux server for cleanup.
    """
    # 1. Setup
    test_env = TmuxEnvironment(tmp_path)

    # Start the dedicated tmux server with a new session
    # -d runs in detached mode (doesn't attach to the session)
    # -s names the session "test"
    test_env.tmux(["new-session", "-d", "-s", "test"], check=True)

    # 2. Yield control to the test function
    yield test_env

    # 3. Teardown
    # Kill the isolated server after the test is complete.
    # This will also clean up the socket file
    test_env.tmux(["kill-server"], check=False)

    # Clean up the socket file if it still exists
    if test_env.socket_path.exists():
        test_env.socket_path.unlink()


def setup_git_repo(path: Path, env_vars: Optional[dict] = None):
    """Initializes a git repository in the given path with an initial commit."""
    subprocess.run(
        ["git", "init"], cwd=path, check=True, capture_output=True, env=env_vars
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
def repo_path(isolated_tmux_server: "TmuxEnvironment") -> Path:
    """Initializes a git repo in the test env and returns its path."""
    path = isolated_tmux_server.tmp_path
    setup_git_repo(path, isolated_tmux_server.env)
    return path


@pytest.fixture
def remote_repo_path(isolated_tmux_server: "TmuxEnvironment") -> Path:
    """Creates a bare git repo to act as a remote."""
    parent = isolated_tmux_server.tmp_path.parent
    remote_path = Path(tempfile.mkdtemp(prefix="remote_repo_", dir=parent))
    subprocess.run(
        ["git", "init", "--bare"],
        cwd=remote_path,
        check=True,
        capture_output=True,
    )
    return remote_path


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
    files: Optional[Dict[str, List[str]]] = None,
    env: Optional[TmuxEnvironment] = None,
    window_prefix: Optional[str] = None,
    agent: Optional[str] = None,
    merge_strategy: Optional[str] = None,
    worktree_naming: Optional[str] = None,
    worktree_prefix: Optional[str] = None,
):
    """Creates a .workmux.yaml file from structured data and optionally commits it."""
    config: Dict[str, Any] = {}
    if panes is not None:
        config["panes"] = panes
    if post_create:
        config["post_create"] = post_create
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
    env: TmuxEnvironment,
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
    return f"wm-{handle}"


def run_workmux_command(
    env: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    command: str,
    pre_run_tmux_cmds: Optional[List[List[str]]] = None,
    expect_fail: bool = False,
    working_dir: Optional[Path] = None,
) -> WorkmuxCommandResult:
    """
    Helper to run a workmux command inside the isolated tmux session.

    Allows tests to optionally expect failure while still capturing stdout/stderr.

    Args:
        env: The isolated tmux environment
        workmux_exe_path: Path to the workmux executable
        repo_path: Path to the git repository
        command: The workmux command to run (e.g., "add feature-branch")
        pre_run_tmux_cmds: Optional list of tmux commands to run before the command
        expect_fail: Whether the command is expected to fail (non-zero exit)
        working_dir: Optional directory to run the command from (defaults to repo_path)
    """
    stdout_file = env.tmp_path / "workmux_stdout.txt"
    stderr_file = env.tmp_path / "workmux_stderr.txt"
    exit_code_file = env.tmp_path / "workmux_exit_code.txt"

    for f in [stdout_file, stderr_file, exit_code_file]:
        if f.exists():
            f.unlink()

    if pre_run_tmux_cmds:
        for cmd_args in pre_run_tmux_cmds:
            env.tmux(cmd_args)

    workdir = working_dir if working_dir is not None else repo_path
    workdir_str = shlex.quote(str(workdir))
    exe_str = shlex.quote(str(workmux_exe_path))
    stdout_str = shlex.quote(str(stdout_file))
    stderr_str = shlex.quote(str(stderr_file))
    exit_code_str = shlex.quote(str(exit_code_file))

    # Prepend the updated PATH to ensure fake commands (like gh) are found.
    # The shell in the existing tmux pane does not automatically pick up
    # changes from `tmux set-environment -g`.
    path_str = shlex.quote(env.env["PATH"])

    workmux_cmd = (
        f"cd {workdir_str} && "
        f"env PATH={path_str} {exe_str} {command} "
        f"> {stdout_str} 2> {stderr_str}; "
        f"echo $? > {exit_code_str}"
    )

    env.tmux(["send-keys", "-t", "test:", workmux_cmd, "C-m"])

    assert poll_until(exit_code_file.exists, timeout=5.0), (
        "workmux command did not complete in time"
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
    env: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch_name: str,
    pre_run_tmux_cmds: Optional[List[List[str]]] = None,
    *,
    base: Optional[str] = None,
    background: bool = False,
) -> None:
    """
    Helper to run `workmux add` command inside the isolated tmux session.

    Asserts that the command completes successfully.

    Args:
        env: The isolated tmux environment
        workmux_exe_path: Path to the workmux executable
        repo_path: Path to the git repository
        branch_name: Name of the branch/worktree to create
        pre_run_tmux_cmds: Optional list of tmux commands to run before workmux add
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
        pre_run_tmux_cmds=pre_run_tmux_cmds,
    )


def run_workmux_open(
    env: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch_name: str,
    *,
    run_hooks: bool = False,
    force_files: bool = False,
    pre_run_tmux_cmds: Optional[List[List[str]]] = None,
    expect_fail: bool = False,
) -> WorkmuxCommandResult:
    """
    Helper to run `workmux open` command inside the isolated tmux session.

    Returns the command result so tests can assert on stdout/stderr.
    """
    flags: List[str] = []
    if run_hooks:
        flags.append("--run-hooks")
    if force_files:
        flags.append("--force-files")

    flag_str = f" {' '.join(flags)}" if flags else ""
    return run_workmux_command(
        env,
        workmux_exe_path,
        repo_path,
        f"open {branch_name}{flag_str}",
        pre_run_tmux_cmds=pre_run_tmux_cmds,
        expect_fail=expect_fail,
    )


def create_commit(env: TmuxEnvironment, path: Path, message: str):
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
    env: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch_name: Optional[str] = None,
    force: bool = False,
    keep_branch: bool = False,
    user_input: Optional[str] = None,
    expect_fail: bool = False,
    from_window: Optional[str] = None,
) -> None:
    """
    Helper to run `workmux remove` command inside the isolated tmux session.

    Uses tmux run-shell -b to avoid hanging when remove kills its own window.
    Asserts that the command completes successfully unless expect_fail is True.

    Args:
        env: The isolated tmux environment
        workmux_exe_path: Path to the workmux executable
        repo_path: Path to the git repository
        branch_name: Optional name of the branch/worktree to remove (omit to auto-detect from current branch)
        force: Whether to use -f flag to skip confirmation
        keep_branch: Whether to use --keep-branch flag to keep the local branch
        user_input: Optional string to pipe to stdin (e.g., 'y' for confirmation)
        expect_fail: If True, asserts the command fails (non-zero exit code)
        from_window: Optional tmux window name to run the command from (useful for testing remove from within worktree window)
    """
    stdout_file = env.tmp_path / "workmux_remove_stdout.txt"
    stderr_file = env.tmp_path / "workmux_remove_stderr.txt"
    exit_code_file = env.tmp_path / "workmux_remove_exit_code.txt"

    # Clean up any previous files
    for f in [stdout_file, stderr_file, exit_code_file]:
        if f.exists():
            f.unlink()

    force_flag = "-f " if force else ""
    keep_branch_flag = "--keep-branch " if keep_branch else ""
    branch_arg = branch_name if branch_name else ""
    input_cmd = f"echo '{user_input}' | " if user_input else ""

    # If from_window is specified, we need to change to that window's working directory
    if from_window:
        worktree_path = get_worktree_path(repo_path, from_window.replace("wm-", ""))
        remove_script = (
            f"cd {worktree_path} && "
            f"{input_cmd}"
            f"{workmux_exe_path} remove {force_flag}{keep_branch_flag}{branch_arg} "
            f"> {stdout_file} 2> {stderr_file}; "
            f"echo $? > {exit_code_file}"
        )
    else:
        remove_script = (
            f"cd {repo_path} && "
            f"{input_cmd}"
            f"{workmux_exe_path} remove {force_flag}{keep_branch_flag}{branch_arg} "
            f"> {stdout_file} 2> {stderr_file}; "
            f"echo $? > {exit_code_file}"
        )

    env.tmux(["run-shell", "-b", remove_script])

    # Wait for command to complete
    assert poll_until(exit_code_file.exists, timeout=5.0), (
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
    env: TmuxEnvironment,
    workmux_exe_path: Path,
    repo_path: Path,
    branch_name: Optional[str] = None,
    ignore_uncommitted: bool = False,
    rebase: bool = False,
    squash: bool = False,
    keep: bool = False,
    into: Optional[str] = None,
    expect_fail: bool = False,
    from_window: Optional[str] = None,
) -> None:
    """
    Helper to run `workmux merge` command inside the isolated tmux session.

    Uses tmux run-shell -b to avoid hanging when merge kills its own window.
    Asserts that the command completes successfully unless expect_fail is True.

    Args:
        env: The isolated tmux environment
        workmux_exe_path: Path to the workmux executable
        repo_path: Path to the git repository
        branch_name: Optional name of the branch to merge (omit to auto-detect from current branch)
        ignore_uncommitted: Whether to use --ignore-uncommitted flag
        rebase: Whether to use --rebase flag
        squash: Whether to use --squash flag
        keep: Whether to use --keep flag
        into: Optional target branch to merge into (instead of main)
        expect_fail: If True, asserts the command fails (non-zero exit code)
        from_window: Optional tmux window name to run the command from
    """
    stdout_file = env.tmp_path / "workmux_merge_stdout.txt"
    stderr_file = env.tmp_path / "workmux_merge_stderr.txt"
    exit_code_file = env.tmp_path / "workmux_merge_exit_code.txt"

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

    branch_arg = branch_name if branch_name else ""
    flags_str = " ".join(flags)

    if from_window:
        from_branch = from_window.replace("wm-", "")
        worktree_path = get_worktree_path(repo_path, from_branch)
        script_dir = worktree_path
    else:
        script_dir = repo_path

    merge_script = (
        f"cd {script_dir} && "
        f"{workmux_exe_path} merge {flags_str} {branch_arg} "
        f"> {stdout_file} 2> {stderr_file}; "
        f"echo $? > {exit_code_file}"
    )

    env.tmux(["run-shell", "-b", merge_script])

    assert poll_until(exit_code_file.exists, timeout=10.0), (
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
    env: TmuxEnvironment,
    pr_number: int,
    json_response: Optional[Dict[str, Any]] = None,
    stderr: str = "",
    exit_code: int = 0,
):
    """
    Creates a fake 'gh' command that responds to 'pr view <number> --json' with controlled output.

    Args:
        env: The isolated tmux environment
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
    # CRITICAL: Also set PATH in the tmux session so workmux can find the fake gh
    env.tmux(["set-environment", "-g", "PATH", new_path])
