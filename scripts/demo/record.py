#!/usr/bin/env python3
"""Run a scripted mtr-ng session in a private tmux server and record it.

Every tick the pane is captured with its colours and cursor, and the script's
captions and key presses are logged with the same clock, so that render.py can
turn the run into a video. Usage: record.py BIN_DIR OUT_DIR
"""

import json
import os
import pathlib
import random
import subprocess
import sys
import threading
import time

SOCKET = "mtr-ng-demo"  # a server of its own, so no other tmux session is touched
SESSION = "demo"
COLS, ROWS = 120, 38
FPS = 10


def tmux(*args, check=True):
    return subprocess.run(
        ["tmux", "-L", SOCKET, *args], capture_output=True, text=True, check=check
    ).stdout


class Recorder:
    def __init__(self, bin_dir, out):
        self.out = out
        self.events = []
        self.frames = open(out / "frames.jsonl", "w")
        tmux("kill-server", check=False)
        # An empty ZDOTDIR keeps the user's zsh startup files, and their PATH, out
        zdotdir = out / "zdotdir"
        zdotdir.mkdir(exist_ok=True)
        env = {
            "ZDOTDIR": str(zdotdir),
            "PATH": f"{bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
            "HOME": os.environ["HOME"],  # the geolocation database lives under it
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "LANG": "en_US.UTF-8",
            "PROMPT": "%F{green}$%f ",
        }
        tmux("-f", "/dev/null", "new-session", "-d", "-s", SESSION,
             "-x", str(COLS), "-y", str(ROWS),
             *[a for k, v in env.items() for a in ("-e", f"{k}={v}")],
             "/bin/zsh", "-f")
        tmux("set", "-g", "status", "off")
        time.sleep(0.5)
        path = f"{bin_dir}:/usr/bin:/bin"
        tmux("send-keys", "-t", SESSION, f"export PATH={path}; clear", "Enter")
        time.sleep(0.5)
        self.start = time.monotonic()
        self.running = True
        self.thread = threading.Thread(target=self.capture_loop)
        self.thread.start()

    def now(self):
        return round(time.monotonic() - self.start, 3)

    def capture_loop(self):
        tick = 0
        while self.running:
            t = self.now()
            out = tmux("capture-pane", "-p", "-e", "-N", "-t", SESSION, ";",
                       "display-message", "-p", "-t", SESSION,
                       "#{cursor_x} #{cursor_y} #{cursor_flag}", check=False)
            *screen, cursor = out.rstrip("\n").split("\n")
            x, y, shown = (int(v) for v in cursor.split())
            self.frames.write(json.dumps(
                {"t": t, "cursor": [x, y, shown], "screen": screen}) + "\n")
            tick += 1
            time.sleep(max(0.0, self.start + tick / FPS - time.monotonic()))

    def log(self, kind, **data):
        self.events.append({"t": self.now(), "kind": kind, **data})

    # The script's vocabulary
    def say(self, text):
        """Show a caption until the next one, or until hush(). [key] draws a keycap."""
        self.log("say", text=text)

    def hush(self):
        self.log("hush")

    def wait(self, seconds):
        time.sleep(seconds)

    def key(self, name, label=None):
        self.log("key", key=label or name)
        tmux("send-keys", "-t", SESSION, name)

    def type(self, text):
        for ch in text:
            tmux("send-keys", "-t", SESSION, "-l", ch)
            time.sleep(random.uniform(0.045, 0.11) + (0.12 if ch == " " else 0))

    def stop(self):
        self.log("end")
        self.running = False
        self.thread.join()
        self.frames.close()
        tmux("kill-server", check=False)
        (self.out / "events.json").write_text(json.dumps(self.events, indent=1))
        (self.out / "meta.json").write_text(json.dumps({"cols": COLS, "rows": ROWS, "fps": FPS}))


def script(r):
    r.wait(1.0)
    r.say("Tracing news.ycombinator.com. --privacy-hops 3 hides the first three hops, my own network")
    r.type("mtr-ng -g --privacy-hops 3 news.ycombinator.com")
    r.wait(0.8)
    r.key("Enter", "Enter")
    r.wait(1.0)
    r.say("A probe goes to every hop each second, and the table fills in as the replies arrive")
    r.wait(7)
    r.say("Each hop keeps its loss and round-trip times; the sparkline shows its recent replies")
    r.wait(6)
    r.say("[e] shows the MPLS labels that routers quote in their replies")
    r.key("e")
    r.wait(5)
    r.key("e")
    r.wait(0.4)
    r.say("[↓] selects a hop, and [Enter] opens everything known about it")
    for _ in range(6):
        r.key("Down", "↓")
        r.wait(0.25)
    r.wait(0.8)
    r.key("Enter")
    r.wait(6.5)
    r.key("Escape", "Esc")
    r.wait(0.4)
    r.say("[v] turns the sparklines into a heatmap of colour alone")
    r.wait(0.8)
    r.key("v")
    r.wait(4)
    r.key("v")
    r.wait(0.4)
    r.say("[m] puts the located hops on a world map, joined along great circles")
    r.wait(0.8)
    r.key("m")
    r.wait(6.5)
    r.key("m")
    r.wait(0.4)
    r.say("[o] opens the column chooser: [Space] adds jitter and drops EMA, and [←] moves jitter next to Last")
    r.wait(0.8)
    r.key("o")
    r.wait(1.2)
    for _ in range(18):  # down to "Last jitter"
        r.key("Down", "↓")
        r.wait(0.15)
    r.wait(0.6)
    r.key("Space")
    r.wait(0.8)
    for _ in range(11):  # after "Last RTT"
        r.key("Left", "←")
        r.wait(0.15)
    r.wait(0.6)
    for _ in range(2):  # "Smoothed RTT (EMA)", whose width the sparklines need back
        r.key("Down", "↓")
        r.wait(0.3)
    r.key("Space")
    r.wait(1.0)
    r.key("Escape", "Esc")
    r.wait(3)
    r.say("[a] lists what leaves this machine, and whose data is on screen")
    r.wait(0.8)
    r.key("a")
    r.wait(6)
    r.key("Escape", "Esc")
    r.wait(0.4)
    r.say("[q] quits")
    r.wait(1.2)
    r.key("q")
    r.wait(1.0)
    r.say("mtr-ng: github.com/gedejong/mtr-ng")
    r.wait(3.5)
    r.hush()


if __name__ == "__main__":
    bin_dir, out = pathlib.Path(sys.argv[1]).resolve(), pathlib.Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    rec = Recorder(bin_dir, out)
    try:
        script(rec)
    finally:
        rec.stop()
