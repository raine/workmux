"""An agent stand-in for `send --wait`.

Run as `python3 AGENT WORKMUX MODE READY`: it reports `done` through
WORKMUX, touches READY, then for each line typed into it acts by MODE:

- `turns`: reports `working` after a moment, then `done` a while later
- `silent`: reports nothing, like an agent whose Enter was swallowed
- `exit`: exits"""

FAKE_AGENT = """
import subprocess
import sys
import time
from pathlib import Path

workmux, mode, ready = sys.argv[1:]


def report(status):
    subprocess.run(
        [workmux, "set-window-status", status], check=True, stdin=subprocess.DEVNULL
    )


report("done")
Path(ready).touch()
for line in sys.stdin:
    if mode == "exit":
        sys.exit(0)
    if mode == "turns":
        time.sleep(1.5)
        report("working")
        time.sleep(1.5)
        report("done")
"""
