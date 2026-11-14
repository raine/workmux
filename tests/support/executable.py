"""PATH test doubles share one executable inode to avoid cold script launches.

Harness-owned scripts should instead be invoked through their interpreter.
Payloads see their .script path as $0 or sys.argv[0], not the command symlink.
"""

from pathlib import Path

SCRIPT_RUNNER = Path(__file__).with_name("script-runner")


def install_script(path: Path, body: str) -> Path:
    """Install a shared entry point and a non-executable interpreter payload.

    The real command name remains on PATH, so production command discovery and
    subprocess execution are exercised. Only the test double's launch changes.
    """
    if not body.startswith("#!"):
        raise ValueError("Test scripts require an explicit interpreter")
    payload = path.with_name(path.name + ".script")
    payload.write_text(body)
    if path.exists() or path.is_symlink():
        path.unlink()
    path.symlink_to(SCRIPT_RUNNER)
    return path
