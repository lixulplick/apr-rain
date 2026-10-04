#!/usr/bin/env python3
"""Capture apr-rain's live ANSI output under a pty, with frame timestamps.

The video renderer (make_frames.py) replays these frames in a GNOME-style
terminal mock, so the demo shows the real binary's output, byte-faithful.

apr-rain redraws the full screen every frame: each frame starts with
\\x1b[H and ends with \\x1b[0m. We record (wall-clock time, frame) pairs so
the renderer can pick the frame that was on screen at any given moment.
"""
import os
import pty
import select
import struct
import fcntl
import termios
import time
import pickle

COLS, ROWS = 128, 34
FPS = 30

OUT = "/home/lxp/proj/apr-rain/demo/captured.pkl"

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.chdir("/home/lxp/proj/apr-rain")
    os.execv(
        "/home/lxp/proj/apr-rain/target/release/apr-rain",
        ["apr-rain", "--fps", str(FPS), "--reveal-secs", "8", "--hold-secs", "9", "--loops", "3"],
    )

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
os.set_blocking(fd, False)

start = time.monotonic()
buf = b""
frames = []  # (t_seconds, frame_str) — frame without the leading \x1b[H

# The binary exits on its own after --loops 3 (~61s of animation).
while True:
    r, _, _ = select.select([fd], [], [], 5.0)
    if not r:
        break
    try:
        data = os.read(fd, 262144)
    except OSError:
        break
    if not data:
        break
    buf += data
    # frames are delimited by the home-cursor escape at the START of each frame
    while True:
        i = buf.find(b"\x1b[H")
        if i < 0:
            # keep at most a tail (unterminated partial frame)
            buf = buf[-8192:] if len(buf) > 8192 else buf
            break
        j = buf.find(b"\x1b[H", i + 3)
        end = j if j >= 0 else len(buf)
        frame = buf[i + 3 : end]
        if j >= 0:
            frames.append((time.monotonic() - start, frame.decode("utf-8", "replace")))
            buf = buf[: i] + buf[j:]
        else:
            break

try:
    os.waitpid(pid, 0)
except ChildProcessError:
    pass

with open(OUT, "wb") as f:
    pickle.dump({"fps": FPS, "cols": COLS, "rows": ROWS, "frames": frames}, f)

deltas = [b - a for (a, _), (b, _) in zip(frames, frames[1:])]
print(
    f"captured {len(frames)} frames over {frames[-1][0]:.1f}s "
    f"(mean inter-frame {sum(deltas)/max(1,len(deltas)):.3f}s)"
)
