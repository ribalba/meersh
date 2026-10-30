"""The stage of the tour video: a virtual screen, a browser window with the terminal,
meersh in a pseudo terminal, the recording and the camera.

make_video.py tells the story, this module films it. It works like meertex's tour video
(tools/video in meertex). The window lives on a screen of its own (Xvfb), so nothing
opens on your desktop. Chrome runs headed in kiosk mode, because only a headed browser
paints like the real thing, and ffmpeg records the screen with x11grab. The camera works
afterwards on the recording, which is larger than the video, so it can move closer
without blurring the text.
"""

import base64
import fcntl
import json
import math
import os
import random
import re
import signal
import struct
import subprocess
import termios
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
XTERM = HERE / "node_modules" / "@xterm" / "xterm"
WEBGL = HERE / "node_modules" / "@xterm" / "addon-webgl"

FPS = 30
# The browser lays the page out at 1536x864 CSS pixels and paints it at 5/3 the density.
# The video is 1920x1080, so the camera can come up to 4/3 closer before it has to
# enlarge anything.
CSS_W, CSS_H = 1536, 864
REC_W, REC_H = 2560, 1440
DPR = REC_W / CSS_W
OUT_W, OUT_H = 1920, 1080

# A clean environment for the browser on the virtual screen: no Wayland, no session bus
# and no desktop scale factor from the real session.
BASE_ENV = {
    "PATH": "/usr/local/bin:/usr/bin:/bin",
    "LANG": "en_US.UTF-8",
    "TZ": "Europe/Berlin",
}


class Screen:
    """A virtual X screen of its own."""

    def __init__(self, width: int, height: int):
        self.width, self.height = width, height
        read, write = os.pipe()
        self.proc = subprocess.Popen(
            ["Xvfb", "-displayfd", str(write), "-screen", "0", f"{width}x{height}x24", "-nolisten", "tcp"],
            pass_fds=(write,), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        os.close(write)
        number = b""
        with os.fdopen(read, "rb") as f:
            while not number.endswith(b"\n"):
                chunk = f.read(1)
                if not chunk:
                    raise RuntimeError("Xvfb did not start")
                number += chunk
        self.display = f":{number.decode().strip()}"

    def env(self, home: Path) -> dict:
        return {**BASE_ENV, "DISPLAY": self.display, "HOME": str(home)}

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()


def ease(t: float) -> float:
    """Ease in and out, 0 to 1."""
    t = min(1.0, max(0.0, t))
    return t * t * (3 - 2 * t)


class Window:
    """A Chrome window that fills its screen, in kiosk mode."""

    def __init__(self, pw, screen: Screen, profile: Path, home: Path, scale: float = DPR):
        self.screen = screen
        args = [
            "--ozone-platform=x11",
            "--kiosk",
            "--window-position=0,0",
            f"--window-size={round(screen.width / scale)},{round(screen.height / scale)}",
            f"--force-device-scale-factor={scale}",
            "--hide-scrollbars",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-infobars",
            "--disable-features=Translate,MediaRouter",
            "--password-store=basic",
        ]
        self.ctx = pw.chromium.launch_persistent_context(
            str(profile), channel="chrome", headless=False, env=screen.env(home), no_viewport=True,
            timezone_id="Europe/Berlin", locale="en-US", args=args,
        )
        self.page = self.ctx.pages[0] if self.ctx.pages else self.ctx.new_page()

    def close(self):
        self.ctx.close()


class Terminal:
    """A program in a pseudo terminal, shown with xterm.js in a Window.

    This serves terminal.html, xterm.js and the program's output as server-sent events.
    Keys go straight to the pseudo terminal, typed at a human pace. What xterm.js answers
    to the program's queries, such as the cursor position, comes back through /input."""

    def __init__(self, window: Window, command: list[str], cwd: Path, env: dict, port: int, title: str):
        self.window = window
        self.output = bytearray()
        self.cond = threading.Condition()
        self.events: list[tuple[str, str]] = []
        self.rng = random.Random(7)
        self.master = None
        self.server = ThreadingHTTPServer(("127.0.0.1", port), self._handler())
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

        page = window.page
        page.goto(f"http://127.0.0.1:{port}/terminal.html")
        page.wait_for_function("window.termReady === true")
        size = page.evaluate("window.termSize")
        self.cols, self.rows = size["cols"], size["rows"]
        self.set_title(title)

        self.master, slave = os.openpty()
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", self.rows, self.cols, 0, 0))
        env = {**env, "TERM": "xterm-256color", "COLUMNS": str(self.cols), "LINES": str(self.rows)}
        # setsid -c makes the pseudo terminal the controlling terminal of the program.
        self.proc = subprocess.Popen(
            ["setsid", "-c", *command], stdin=slave, stdout=slave, stderr=slave, cwd=cwd, env=env, close_fds=True,
        )
        os.close(slave)
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        while True:
            try:
                data = os.read(self.master, 65536)
            except OSError:
                data = b""
            with self.cond:
                if not data:
                    self.events.append(("end", ""))
                    self.cond.notify_all()
                    return
                self.output.extend(data)
                self.events.append(("out", base64.b64encode(data).decode()))
                self.cond.notify_all()

    def _handler(self):
        term = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
                if self.path == "/input" and term.master is not None:
                    os.write(term.master, body)
                self.send_response(204)
                self.end_headers()

            def do_GET(self):
                files = {
                    "/terminal.html": (HERE / "terminal.html", "text/html; charset=utf-8"),
                    "/xterm.js": (XTERM / "lib" / "xterm.js", "text/javascript"),
                    "/xterm.css": (XTERM / "css" / "xterm.css", "text/css"),
                    "/addon-webgl.js": (WEBGL / "lib" / "addon-webgl.js", "text/javascript"),
                }
                if self.path in files:
                    path, kind = files[self.path]
                    body = path.read_bytes()
                    self.send_response(200)
                    self.send_header("Content-Type", kind)
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                    return
                if self.path != "/stream":
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Cache-Control", "no-cache")
                self.end_headers()
                sent = 0
                try:
                    while True:
                        with term.cond:
                            while sent == len(term.events):
                                term.cond.wait(timeout=15)
                                if sent == len(term.events):
                                    self.wfile.write(b": ping\n\n")
                                    self.wfile.flush()
                            batch = term.events[sent:]
                            sent = len(term.events)
                        for kind, data in batch:
                            if kind == "end":
                                return
                            self.wfile.write(f"event: {kind}\ndata: {data}\n\n".encode())
                        self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    return

        return Handler

    def set_title(self, title: str):
        with self.cond:
            self.events.append(("title", title))
            self.cond.notify_all()

    def text(self, since: int = 0) -> str:
        with self.cond:
            return self.output[since:].decode("utf-8", "replace")

    def send(self, keys: str):
        os.write(self.master, keys.encode())

    def type(self, text: str, cps: float = 16.0):
        """Type text at about cps characters a second, a little uneven like a person."""
        for ch in text:
            os.write(self.master, ch.encode())
            pause = 1.0 / cps * self.rng.uniform(0.6, 1.5)
            if ch == " ":
                pause *= 1.3
            time.sleep(pause)

    def type_for(self, text: str, seconds: float):
        """Type text so that it takes about seconds."""
        self.type(text, cps=len(text) / seconds * 1.08)

    def wait_for(self, pattern: str, since: int = 0, timeout: float = 20.0) -> str:
        end = time.monotonic() + timeout
        rx = re.compile(pattern)
        while time.monotonic() < end:
            text = self.text(since)
            if rx.search(text):
                return text
            time.sleep(0.03)
        raise RuntimeError(f"the terminal did not show {pattern!r} within {timeout:.0f} s:\n{self.text(since)[-2000:]}")

    def mark(self) -> int:
        with self.cond:
            return len(self.output)

    def run(self, command: str, done: str, timeout: float = 30.0):
        """Run a command off camera and wait until its output matches done."""
        since = self.mark()
        self.send(command + "\r")
        self.wait_for(done, since, timeout)

    def stop(self):
        if self.proc.poll() is None:
            os.killpg(self.proc.pid, signal.SIGHUP)
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.server.shutdown()
        try:
            os.close(self.master)
        except OSError:
            pass


class Recording:
    """ffmpeg records a screen losslessly. It notes when its first frame was taken, so
    the clip can be cut at the moment the action started."""

    def __init__(self, screen: Screen, path: Path, seconds: float):
        self.path = path
        self.log = path.with_suffix(".log")
        with open(self.log, "w") as log:
            self.proc = subprocess.Popen(
                ["ffmpeg", "-hide_banner", "-y", "-f", "x11grab", "-draw_mouse", "0", "-framerate", str(FPS),
                 "-video_size", f"{screen.width}x{screen.height}", "-i", screen.display, "-t", f"{seconds:.3f}",
                 "-c:v", "utvideo", "-pix_fmt", "gbrp", str(path)],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=log,
            )

    def wait(self):
        self.proc.wait()
        if self.proc.returncode:
            raise RuntimeError(f"ffmpeg failed to record, see {self.log}")

    def started(self) -> float:
        """The wall-clock time of the first frame (x11grab stamps frames with it)."""
        for _ in range(100):
            m = re.search(r"start: ([0-9.]+)", self.log.read_text())
            if m:
                return float(m.group(1))
            time.sleep(0.05)
        raise RuntimeError(f"no start time in {self.log}")


# --- the camera -------------------------------------------------------------------


class Camera:
    """Where the camera looks over time: keyframes (seconds, zoom, (x, y)) with the
    centre in the page's CSS pixels. Between two keyframes it eases. A zoom of 1 shows
    the whole screen, 4/3 is as close as it gets without enlarging."""

    def __init__(self, keys):
        self.keys = sorted(keys, key=lambda k: k[0])

    @classmethod
    def still(cls, zoom: float = 1.0, at=(CSS_W / 2, CSS_H / 2)):
        return cls([(0.0, zoom, at)])

    def at(self, t: float):
        keys = self.keys
        if t <= keys[0][0]:
            return keys[0][1], keys[0][2]
        for (t0, z0, c0), (t1, z1, c1) in zip(keys, keys[1:]):
            if t <= t1:
                e = ease((t - t0) / (t1 - t0)) if t1 > t0 else 1.0
                # Zoom in log space, so in and out feel the same speed.
                z = math.exp(math.log(z0) + (math.log(z1) - math.log(z0)) * e)
                return z, (c0[0] + (c1[0] - c0[0]) * e, c0[1] + (c1[1] - c0[1]) * e)
        return keys[-1][1], keys[-1][2]

    def box(self, t: float):
        """The part of the recording (in recorded pixels) that fills the frame at t."""
        z, (cx, cy) = self.at(t)
        w, h = REC_W / z, REC_H / z
        x = min(max(cx * DPR - w / 2, 0.0), REC_W - w)
        y = min(max(cy * DPR - h / 2, 0.0), REC_H - h)
        return (x, y, x + w, y + h)


def develop(main: Path, skip: float, frames: int, camera: Camera, out: Path, still: Path | None = None,
            still_at: int = 0):
    """Turn the recording of a shot into its clip: cut it at the start of the action,
    move the camera and scale to the size of the video. The clip is lossless (FFV1), and
    frame still_at is saved as a PNG to still."""
    cmd = ["ffmpeg", "-v", "error", "-ss", f"{skip:.3f}", "-i", str(main), "-vf", "format=rgb24",
           "-frames:v", str(frames), "-f", "rawvideo", "-pix_fmt", "rgb24", "-"]
    reader = subprocess.Popen(cmd, stdout=subprocess.PIPE)
    writer = subprocess.Popen(
        ["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", f"{OUT_W}x{OUT_H}",
         "-framerate", str(FPS), "-i", "-", "-c:v", "ffv1", "-level", "3", "-pix_fmt", "yuv444p", str(out)],
        stdin=subprocess.PIPE,
    )
    size = REC_W * REC_H * 3
    last = None
    for n in range(frames):
        data = reader.stdout.read(size)
        if len(data) < size:
            # The recording ran a little short. Hold its last frame.
            if last is None:
                raise RuntimeError(f"{main} has no frames")
            frame = last
        else:
            frame = Image.frombuffer("RGB", (REC_W, REC_H), data, "raw", "RGB", 0, 1)
            last = frame
        shot = frame.resize((OUT_W, OUT_H), Image.LANCZOS, box=camera.box(n / FPS))
        writer.stdin.write(shot.tobytes())
        if still and n == still_at:
            shot.save(still)
    reader.stdout.close()
    reader.wait()
    writer.stdin.close()
    if writer.wait():
        raise RuntimeError(f"ffmpeg failed to write {out}")


def write_json(path: Path, data):
    path.write_text(json.dumps(data, indent=1, ensure_ascii=False))
