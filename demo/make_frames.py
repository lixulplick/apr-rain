#!/usr/bin/env python3
"""Render the apr-rain demo video as PNG frames (1280x720, 30fps).

Scenes (63s total):
  0.0-4.2   terminal lead-in: prompt, typing ./target/release/apr-rain
  4.2-56.5  the effect — frames captured live from the real binary (capture.py)
  56.5-63.0 outro card (title, repo link, music credit)

The effect frames are the actual ANSI output of the release binary, replayed
in a GNOME-style terminal mock — same approach as the lkdn demo video.
"""
import bisect
import os
import pickle
import re
import sys
from PIL import Image, ImageDraw, ImageFont

W, H = 1280, 720
FPS = 30
COLS, ROWS = 128, 34
FONT_PATH = "/usr/share/fonts/adwaita-mono-fonts/AdwaitaMono-Regular.ttf"
FONT_SIZE = 16
FONT = ImageFont.truetype(FONT_PATH, FONT_SIZE)
CHAR_W = FONT.getlength("M")  # 9.59375 — AdwaitaMono is monospaced, katakana included
LINE_H = 19                    # ~1:2 cell aspect, like a real terminal
X0 = (W - COLS * CHAR_W) / 2
Y0 = 36 + (H - 36 - ROWS * LINE_H) / 2

BG = (16, 17, 26)
FG = (208, 211, 216)
GREEN = (133, 220, 153)
CYAN = (129, 200, 245)
DIM = (120, 125, 138)

PROMPT = "lxp@myhost:~/proj/apr-rain$ "
CMD = "./target/release/apr-rain"

T_TYPE = 1.5          # typing starts
T_RUN = 3.4           # pause after command
T_EFFECT = 4.2        # effect starts (binary took over the screen)
T_OUTRO = 56.5        # outro card
T_END = 63.0

FRAMES_DIR = "/home/lxp/proj/apr-rain/demo/frames"
CAPTURE = "/home/lxp/proj/apr-rain/demo/captured.pkl"

START = int(sys.argv[1]) if len(sys.argv) > 1 else 0
os.makedirs(FRAMES_DIR, exist_ok=True)
if START == 0:
    for f in os.listdir(FRAMES_DIR):
        os.remove(os.path.join(FRAMES_DIR, f))

# ---------------------------------------------------------------------------
# captured effect frames
# ---------------------------------------------------------------------------
with open(CAPTURE, "rb") as f:
    cap = pickle.load(f)
cap_times = [t for t, _ in cap["frames"]]
assert cap["cols"] == COLS and cap["rows"] == ROWS, (
    f"capture grid {cap['cols']}x{cap['rows']} != render grid {COLS}x{ROWS}"
)

SGR = re.compile(r"\x1b\[([0-9;]*)m")

def parse_line(line):
    """ANSI line -> [(text, color)] segments (truecolor 38;2 only)."""
    segs = []
    color = FG
    pos = 0
    for m in SGR.finditer(line):
        text = line[pos : m.start()]
        if text:
            segs.append((text, color))
        toks = (m.group(1) or "0").split(";")
        i = 0
        while i < len(toks):
            if toks[i] == "38" and i + 3 < len(toks) + 1 and toks[i + 1] == "2":
                color = tuple(int(toks[i + k]) for k in (2, 3, 4))
                i += 4
                continue
            if toks[i] in ("", "0", "39"):
                color = FG
            i += 1
        pos = m.end()
    text = line[pos:]
    if text:
        segs.append((text, color))
    return segs

def captured_at(t):
    """Effect frame displayed at capture-relative time t (None before first)."""
    i = bisect.bisect_right(cap_times, t) - 1
    if i < 0:
        return None
    return cap["frames"][i][1].split("\r\n")

# ---------------------------------------------------------------------------
# rendering
# ---------------------------------------------------------------------------
def chrome(img, d):
    """GNOME-style headerbar."""
    d.rectangle([0, 0, W, 34], fill=(43, 39, 55))
    d.rectangle([0, 34, W, 36], fill=(28, 26, 38))
    title = "lxp@myhost: ~/proj/apr-rain"
    tw = FONT.getlength(title)
    d.ellipse([W // 2 - tw // 2 - 22, 12, W // 2 - tw // 2 - 8, 26], fill=(233, 84, 32))
    d.text((W // 2 - tw // 2, 9), title, font=FONT, fill=(222, 221, 218))
    d.ellipse([W - 34, 9, W - 10, 33], outline=(120, 118, 130), width=2)
    d.line([W - 28, 15, W - 16, 27], fill=(200, 198, 210), width=2)
    d.line([W - 28, 27, W - 16, 15], fill=(200, 198, 210), width=2)

def new_frame():
    img = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(img)
    chrome(img, d)
    return img, d

def draw_terminal_line(d, row, segs):
    y = Y0 + row * LINE_H
    x = X0
    for text, color in segs:
        if text.strip():
            d.text((x, y), text, font=FONT, fill=color)
        x += FONT.getlength(text)

def draw_cursor(d, x, y):
    d.rectangle([x, y, x + 9, y + 14], fill=(170, 175, 190))

def frame_prompt(d, typed, cursor_on):
    draw_terminal_line(d, 0, [(PROMPT, GREEN), (typed, FG)])
    if cursor_on:
        x = X0 + FONT.getlength(PROMPT + typed)
        draw_cursor(d, x, Y0)

def frame_effect(d, lines):
    for row, line in enumerate(lines[:ROWS]):
        draw_terminal_line(d, row, parse_line(line))

OUTRO = [
    [("", FG)],
    [("", FG)],
    [("  apr-rain", CYAN, )],
    [("  matrix rain that reveals Alejandro Revilla's headshot", FG)],
    [("", FG)],
    [("  founder of jPOS and Transactility, rendered in falling katakana", DIM)],
    [("", FG)],
    [("  cargo build --release && ./target/release/apr-rain", DIM)],
    [("", FG)],
    [("  https://github.com/lixulplick/apr-rain", CYAN)],
    [("", FG)],
    [("", FG)],
    [("  ♪  Rob Dougan — Clubbed to Death", DIM)],
    [("", FG)],
]

def frame_outro(d):
    for row, segs in enumerate(OUTRO):
        draw_terminal_line(d, row, segs)

# ---------------------------------------------------------------------------
# main timeline
# ---------------------------------------------------------------------------
total_frames = int(T_END * FPS)
n = 0
for fi in range(START, total_frames):
    t = fi / FPS
    img, d = new_frame()
    if t < T_RUN:
        # typing scene
        if t < T_TYPE:
            typed = ""
        else:
            k = int((t - T_TYPE) * 15)  # ~15 chars/sec
            typed = CMD[: min(len(CMD), k)]
        frame_prompt(d, typed, cursor_on=(t % 1.0) < 0.55)
    elif t < T_EFFECT:
        frame_prompt(d, CMD, cursor_on=(t % 1.0) < 0.55)
    elif t < T_OUTRO:
        lines = captured_at(t - T_EFFECT)
        if lines is None:
            frame_prompt(d, CMD, cursor_on=(t % 1.0) < 0.55)
        else:
            frame_effect(d, lines)
    else:
        frame_outro(d)

    img.save(os.path.join(FRAMES_DIR, f"{fi:05d}.png"))
    n += 1
    if fi % 150 == 0:
        print(f"frame {fi}/{total_frames} (t={t:.1f}s)", flush=True)

print(f"done: {n} frames, {n / FPS:.2f}s at {FPS}fps")
