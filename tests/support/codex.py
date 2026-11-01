"""A Codex stand-in that submits a prompt only when it arrives as a
bracketed paste followed by Enter, the way Codex's composer needs it.

Run as `python3 SIMULATOR WORKMUX READY SUBMITTED INPUT`: it reports
`waiting` through WORKMUX, touches READY once it reads input, logs raw
input to INPUT, and writes the submitted prompt to SUBMITTED."""

CODEX_SIMULATOR = (
    """
import os
from pathlib import Path
import subprocess
import sys
import termios
import time
import tty

workmux, ready_path, submitted_path, input_path = sys.argv[1:]
subprocess.run([workmux, "set-window-status", "waiting"], check=True)
fd = sys.stdin.fileno()
old_settings = termios.tcgetattr(fd)
tty.setraw(fd)
os.write(sys.stdout.fileno(), b"\\x1b[?2004h")
time.sleep(0.1)
Path(ready_path).touch()

begin = b"\\x1b[200~"
end = b"\\x1b[201~"
buffer = bytearray()
prompt = bytearray()
in_paste = False
paste_completed = False
try:
    while True:
        received = os.read(fd, 4096)
        with Path(input_path).open("ab") as input_log:
            input_log.write(received)
        buffer.extend(received)
        while buffer:
            if (len(buffer) < len(begin) and begin.startswith(buffer)) or (
                len(buffer) < len(end) and end.startswith(buffer)
            ):
                break
            if buffer.startswith(begin):
                del buffer[:len(begin)]
                in_paste = True
                continue
            if buffer.startswith(end):
                del buffer[:len(end)]
                in_paste = False
                paste_completed = True
                continue
            byte = buffer.pop(0)
            if byte == 13:
                if paste_completed and not in_paste:
                    Path(submitted_path).write_bytes(prompt)
                    raise SystemExit(0)
                prompt.append(10)
            else:
                prompt.append(byte)
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old_settings)
""".strip()
    + "\n"
)
