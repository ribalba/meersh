#!/usr/bin/env python3
"""Films the narrated tour of meersh for the website (website/video/README.md).

Everything in it is real: the meersh release build in a pseudo terminal, a made-up
project in a home of its own, Claude Code as the model and a coding agent that works
through meersh's MCP server. Google Chrome shows the terminal with xterm.js on a virtual
screen (stage.py). Kokoro speaks the narration (kokoro_say.py), and ffmpeg puts the clips
together with the voice, burned-in subtitles and the end card. publish.py
turns the result into the files of the website.

Every run starts from a fresh demo home, so it can run again at any time.
"""

import argparse
import array
import json
import os
import shutil
import subprocess
import sys
import time
import wave
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from pathlib import Path
from typing import Callable

from playwright.sync_api import sync_playwright

HERE = Path(__file__).resolve().parent
WEBSITE = HERE.parent
REPO = WEBSITE.parent
sys.path.insert(0, str(HERE))

import stage  # noqa: E402
from stage import FPS, OUT_H, OUT_W, Camera  # noqa: E402

MEERSH = REPO / "target" / "release" / "meersh"
ART = WEBSITE / "art"

# The narration voice, the same as in the meertex and meerora videos. narration.txt has
# the pronunciation lexicon that Kokoro speaks as phonemes.
KOKORO_VOICE = "af_heart"
KOKORO_SPEED = 0.95
KOKORO_MODELS = [Path.home() / ".cache" / "kokoro", Path.home() / ".cache" / "meerora" / "voice" / "kokoro"]
KOKORO_PYTHONS = [Path.home() / ".cache" / "meersh-kokoro" / "bin" / "python",
                  Path.home() / ".cache" / "meertex-kokoro" / "bin" / "python"]

GAP = 1.3  # seconds between two sentences in the same shot
SHOT_LEAD = 0.5  # seconds before the first sentence of a shot that follows a cut
SHOT_TAIL = 1.0  # seconds after the last sentence of a shot before a cut
XFADE = 0.5  # seconds of crossfade between two scenes
PREROLL = 0.8  # seconds the recorder runs before the action starts

# The demo user's Claude Code login is the real one. meersh calls `claude -p`, and this
# wrapper on the demo PATH gives it the real home, so the demo home needs no copy of it.
REAL_CLAUDE = shutil.which("claude") or str(Path.home() / ".local" / "bin" / "claude")

# Every line editor prompt starts by asking the terminal for the cursor position, so a
# position query followed by the prompt means meersh waits for the next line.
DONE = r"(?s)\x1b\[6n.*\x1b\[96m> "

ASK = "directories without changes for six months"
ACT = 'ls | llm "delete all files that are larger than 5mb"'
TITLE = "meersh  ~/project"


# --- the story ----------------------------------------------------------------------
#
# Each scene is one [section] of narration.txt. Each shot is filmed in one take:
# prepare() sets the terminal up before the recorder starts, act() plays while it runs,
# timed by the cue (the shot's sentences in seconds from its start).


class Cue:
    def __init__(self, t0: float, says: list[float], length: float):
        self.t0, self.say, self.length = t0, says, length

    def now(self) -> float:
        return time.monotonic() - self.t0

    def at(self, t: float):
        wait = self.t0 + t - time.monotonic()
        if wait > 0:
            time.sleep(wait)


@dataclass
class Shot:
    id: str
    says: int = 0  # how many sentences of the scene are spoken during this shot, in order
    hold: float = 4.5  # the shortest this shot may be, in seconds
    prepare: Callable | None = None
    act: Callable | None = None
    still: float = 0.5  # when in the shot its still is taken, as a share of its length
    frames: int = 0  # filled by plan()
    start: float = 0.0  # in its scene, filled by plan()
    cues: list[float] = field(default_factory=list)  # filled by plan()


@dataclass
class Scene:
    id: str
    shots: list[Shot]
    lead: float = 0.7  # seconds before the first sentence
    tail: float = 1.5  # seconds after the last sentence
    subtitles: bool = True
    sentences: list[tuple[str, Path, float]] = field(default_factory=list)  # filled by speak()
    offsets: list[float] = field(default_factory=list)  # filled by plan()
    frames: int = 0


def enter(t: stage.Terminal, line: str, cps: float = 15.0, timeout: float = 30.0):
    """Type a command line at a human pace, press return and wait for the next prompt."""
    t.type(line, cps=cps)
    time.sleep(0.3)
    since = t.mark()
    t.send("\r")
    t.wait_for(DONE, since, timeout)


def answer(t: stage.Terminal, keys: str, then: str = DONE, timeout: float = 30.0):
    """Answer a question of meersh, such as run it? or approve?."""
    t.type(keys, cps=6)
    time.sleep(0.35)
    since = t.mark()
    t.send("\r")
    t.wait_for(then, since, timeout)


def clear(st):
    st.term.run("clear", DONE)
    time.sleep(0.4)


# 1. intro: ls returns records.


def intro_act(st, cue):
    cue.at(cue.say[0] + 2.4)
    enter(st.term, "ls", cps=6)


# 2. objects: filters on typed columns.


def objects_act(st, cue):
    cue.at(cue.say[0] + 0.2)
    enter(st.term, "ls | where size > 1mb | sort-by size -r", cps=16)
    cue.at(cue.say[1] - 0.5)
    enter(st.term, "ls | where modified > ((date now) - 1wk)", cps=17)


# 3. methods: $f.hash() and $p.kill() on the devserver the studio started.


def methods_act(st, cue):
    t = st.term
    cue.at(cue.say[0] + 0.1)
    enter(t, "let f = (ls | where name == dump.sql | first)", cps=20)
    enter(t, "$f.hash()", cps=10)
    cue.at(cue.say[1] + 0.8)
    enter(t, "let p = (ps | where name == devserver | first)", cps=22)
    enter(t, "$p.kill()", cps=10)


# 4. legacy: git through its adapter.


def legacy_act(st, cue):
    cue.at(cue.say[0] + 0.4)
    enter(st.term, "^git log | select author date subject", cps=15)
    cue.at(cue.say[1] + 1.2)
    enter(st.term, "^git log | where author =~ Priya | select date subject", cps=18)


# 5. ask: the ? shorthand. The same request runs once off camera and is declined, so
# the answer is in meersh's response cache and the take does not wait for the model.


def ask_prepare(st):
    clear(st)
    t = st.term
    since = t.mark()
    t.send(f"? {ASK}\r")
    t.wait_for(r"run it\? \[Y/n/e\]", since, timeout=180)
    time.sleep(0.3)
    since = t.mark()
    t.send("n\r")
    t.wait_for(DONE, since)
    clear(st)


def ask_act(st, cue):
    t = st.term
    cue.at(cue.say[0] + 0.5)
    t.type(f"? {ASK}", cps=15)
    time.sleep(0.3)
    since = t.mark()
    t.send("\r")
    wait = time.monotonic()
    t.wait_for(r"run it\? \[Y/n/e\]", since, timeout=120)
    st.notes.append(f"ask: the proposal took {time.monotonic() - wait:.1f} s")
    cue.at(cue.say[3] + 1.0)
    answer(t, "y")


# 6. actor: the model acts on the objects of a pipeline. Warmed up like ask.


def actor_prepare(st):
    clear(st)
    t = st.term
    since = t.mark()
    t.send(ACT + "\r")
    t.wait_for(r"approve\?", since, timeout=240)
    time.sleep(0.3)
    since = t.mark()
    t.send("n\r")
    t.wait_for(DONE, since)
    clear(st)


def actor_act(st, cue):
    t = st.term
    cue.at(cue.say[0] + 0.3)
    t.type(ACT, cps=16)
    time.sleep(0.3)
    since = t.mark()
    t.send("\r")
    wait = time.monotonic()
    t.wait_for(r"approve\?", since, timeout=120)
    st.notes.append(f"actor: the plan took {time.monotonic() - wait:.1f} s")
    cue.at(cue.say[2] + 3.6)
    answer(t, "y")


# 7. undo: the journal and undo.


def undo_act(st, cue):
    cue.at(cue.say[0] + 0.4)
    enter(st.term, "journal | select principal action target", cps=17)
    cue.at(cue.say[0] + 4.4)
    enter(st.term, "journal | where principal == actor | undo", cps=18)


# 8. roles: the same request decided for every principal.

ROLES = "user script:cleanup.msh plan actor agent:claude"


def roles_act(st, cue):
    cue.at(cue.say[0] + 0.4)
    enter(st.term, f"[{ROLES}] | each {{ policy explain fs.delete notes.md --principal $it }} "
                   "| select principal decision because", cps=26)


# 9. ownership: what the session created, the user's files and a system path.


def ownership_act(st, cue):
    cue.at(cue.say[0] + 0.2)
    enter(st.term, "touch draft.txt", cps=14)
    enter(st.term, "[draft.txt notes.md /etc/hostname] | each { policy explain fs.delete $it --principal actor } "
                   "| select target decision because", cps=26)


# 10. confine: Landlock in a with block.


def confine_act(st, cue):
    cue.at(cue.say[0] + 0.3)
    enter(st.term, 'with {fs.write: ./build} { ^sh -c "date > build/log.txt; date > notes.md" }', cps=20)
    cue.at(cue.say[1] + 2.5)
    enter(st.term, "ls build", cps=10)


# 11. agents: what the agent session of Studio.agent() left behind.


def agents_act(st, cue):
    cue.at(cue.say[0] + 0.5)
    enter(st.term, "ledger | select object principal state", cps=17)


SCENES = [
    Scene("intro", lead=1.2, shots=[
        Shot("ls", says=2, hold=9.0, prepare=clear, act=intro_act),
    ]),
    Scene("objects", shots=[
        Shot("where", says=2, hold=11.0, prepare=clear, act=objects_act),
    ]),
    Scene("methods", shots=[
        Shot("methods", says=2, hold=14.5, prepare=clear, act=methods_act, still=0.9),
    ]),
    Scene("legacy", shots=[
        Shot("git", says=2, hold=11.0, prepare=clear, act=legacy_act, still=0.8),
    ]),
    Scene("ask", shots=[
        Shot("ask", says=4, hold=16.0, prepare=ask_prepare, act=ask_act, still=0.6),
    ]),
    Scene("actor", shots=[
        Shot("actor", says=3, hold=20.0, prepare=actor_prepare, act=actor_act, still=0.75),
    ]),
    Scene("undo", shots=[
        Shot("undo", says=2, hold=13.0, prepare=clear, act=undo_act, still=0.9),
    ]),
    Scene("roles", shots=[
        Shot("roles", says=3, hold=17.0, prepare=clear, act=roles_act, still=0.9),
    ]),
    Scene("ownership", shots=[
        Shot("ownership", says=2, hold=14.0, prepare=clear, act=ownership_act, still=0.9),
    ]),
    Scene("confine", shots=[
        Shot("confine", says=2, hold=13.0, prepare=clear, act=confine_act, still=0.9),
    ]),
    Scene("agents", shots=[
        Shot("agents", says=2, hold=11.0, prepare=clear, act=agents_act, still=0.9),
    ]),
    Scene("ending", lead=1.6, tail=4.0, subtitles=False, shots=[
        Shot("ending", says=3, prepare=clear),
    ]),
]


# --- the studio -----------------------------------------------------------------------


def ago(days: float, hour: int, minute: int) -> float:
    """A timestamp days before today at the given time of day."""
    day = datetime.now() - timedelta(days=days)
    return day.replace(hour=hour, minute=minute, second=0, microsecond=0).timestamp()


class Studio:
    """The demo home with its project, the agent session, the screen, the browser window
    and meersh in the terminal."""

    def __init__(self, args, work: Path, pw):
        self.args, self.work, self.pw = args, work, pw
        self.home = work / "home"
        self.project = self.home / "project"
        self.notes: list[str] = []
        self.screen = self.window = self.term = self.devserver = None

    def start(self):
        self.make_home()
        self.agent()
        bin_dir = self.home / ".local" / "bin"
        self.devserver = subprocess.Popen([str(bin_dir / "devserver"), "3600"], cwd=self.project,
                                          stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
        profile = self.work / "profile"
        shutil.rmtree(profile, ignore_errors=True)
        self.screen = stage.Screen(stage.REC_W, stage.REC_H)
        self.window = stage.Window(self.pw, self.screen, profile, self.home)
        self.term = stage.Terminal(self.window, [str(self.args.meersh)], self.project, self.env(),
                                   self.args.port, TITLE)
        self.term.wait_for(DONE)

    def env(self) -> dict:
        return {
            **stage.BASE_ENV,
            "HOME": str(self.home),
            "PATH": f"{self.home / '.local' / 'bin'}:/usr/local/bin:/usr/bin:/bin",
        }

    def make_home(self):
        """A home of its own: the made-up project with fixed ages relative to today, a git
        history by three made-up people, a config that sends the assistant and the actor
        to Claude, the claude wrapper and a copy of sleep that runs as devserver."""
        if self.home.exists():
            shutil.rmtree(self.home)
        bin_dir = self.home / ".local" / "bin"
        bin_dir.mkdir(parents=True)
        claude = bin_dir / "claude"
        claude.write_text(f'#!/bin/sh\nexec env HOME="{Path.home()}" "{REAL_CLAUDE}" "$@"\n')
        claude.chmod(0o755)
        shutil.copy("/usr/bin/sleep", bin_dir / "devserver")
        config = self.home / ".config" / "meersh"
        config.mkdir(parents=True)
        (config / "config.toml").write_text(
            '[llm]\ndefault = "claude"\nassistant = "claude"\n\n'
            '[llm.backends.claude]\nkind = "claude-code"\nmodel = "haiku"\n'
        )

        p = self.project
        p.mkdir()
        files = {
            "README.md": ("# photo-sync\n\n" + "Keeps photo folders on several machines in step.\n" * 40, ago(9, 9, 14)),
            "notes.md": ("- ask Tomás about the thumbnail cache\n" * 9, ago(1, 17, 2)),
            "dump.sql": ("INSERT INTO photos VALUES (1, 'IMG_0001.jpg');\n" * 46000, ago(18, 8, 30)),
            "build-2025.tar": (7_340_032, ago(270, 15, 44)),
            "screen-recording.mp4": (48_234_496, ago(5, 19, 31)),
        }
        for name, (content, when) in files.items():
            path = p / name
            if isinstance(content, str):
                path.write_text(content)
            else:
                with open(path, "wb") as f:
                    f.truncate(content)
            os.utime(path, (when, when))

        git = {**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=p, env=git, check=True)
        commits = [
            ("Lena Fischer", "lena@example.org", 3, "Add the export to CSV"),
            ("Tomás Ruiz", "tomas@example.org", 2, "Fix the date parser for ISO weeks"),
            ("Priya Nair", "priya@example.org", 1, "Speed up the thumbnail cache"),
            ("Priya Nair", "priya@example.org", 0.6, "Keep thumbnails of deleted photos for a day"),
        ]
        changelog = p / "CHANGELOG.md"
        for author, email, days, subject in commits:
            with open(changelog, "a") as f:
                f.write(f"- {subject}\n")
            when = datetime.fromtimestamp(ago(days, 10 + round(days * 3) % 8, 12)).isoformat()
            person = {"GIT_AUTHOR_NAME": author, "GIT_AUTHOR_EMAIL": email, "GIT_AUTHOR_DATE": when,
                      "GIT_COMMITTER_NAME": author, "GIT_COMMITTER_EMAIL": email, "GIT_COMMITTER_DATE": when}
            subprocess.run(["git", "add", "-A"], cwd=p, env=git, check=True)
            subprocess.run(["git", "commit", "-q", "-m", subject], cwd=p, env={**git, **person}, check=True)
        os.utime(changelog, (ago(0.6, 12, 30),) * 2)

        for name, when in [("src", ago(1, 11, 20)), ("assets", ago(28, 10, 0)), ("build", ago(2, 16, 5)),
                           ("old-drafts", ago(230, 12, 0)), ("vendor", ago(345, 9, 0))]:
            d = p / name
            d.mkdir()
            (d / ".keep").write_text("")
            os.utime(d / ".keep", (when, when))
            os.utime(d, (when, when))

    def agent(self):
        """Claude Code's part, off camera: an agent session of meersh's MCP server writes
        a file and asks to delete one of the user's. The ledger and the policy show both
        on camera."""
        proc = subprocess.Popen([str(self.args.meersh), "mcp", "--agent", "claude"], cwd=self.project,
                                env=self.env(), stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

        def call(n: int, method: str, params: dict) -> dict:
            proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n")
            proc.stdin.flush()
            return json.loads(proc.stdout.readline())

        call(1, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                               "clientInfo": {"name": "make_video", "version": "1"}})
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        steps = [
            ("write_file", {"path": "src/thumbs.rs", "content": "pub fn keep_for_a_day() {}\n"}),
            ("run", {"pipeline": "rm notes.md"}),
        ]
        for n, (tool, arguments) in enumerate(steps, 2):
            result = call(n, "tools/call", {"name": tool, "arguments": arguments})["result"]
            say(f"agent {tool} {arguments}: {result['content'][0]['text'][:160]}")
        proc.stdin.close()
        proc.wait(timeout=10)
        if not (self.project / "notes.md").exists():
            raise SystemExit("the agent deleted notes.md, which the policy should have stopped")

    def stop(self):
        if self.term:
            self.term.stop()
        if self.window:
            self.window.close()
        if self.screen:
            self.screen.stop()
        if self.devserver and self.devserver.poll() is None:
            self.devserver.kill()

    # --- filming ------------------------------------------------------------------

    def film(self, scene: Scene, shot: Shot):
        say(f"filming {shot.id}: {shot.frames / FPS:.1f} s")
        if shot.prepare:
            shot.prepare(self)
        length = shot.frames / FPS
        raw = self.work / "raw"
        raw.mkdir(exist_ok=True)
        rec = stage.Recording(self.screen, raw / f"{shot.id}.mkv", PREROLL + length + 0.6)
        time.sleep(PREROLL)
        t0, wall = time.monotonic(), time.time()
        cue = Cue(t0, shot.cues, length)
        if shot.act:
            shot.act(self, cue)
        late = cue.now() - length
        if late > 0:
            self.notes.append(f"{shot.id}: the action ran {late:.1f} s past the end of the shot")
        rec.wait()
        clip = self.work / "clips" / f"{shot.id}.mkv"
        still = self.work / "stills" / f"{shot.id}.png"
        stage.develop(rec.path, wall - rec.started(), shot.frames, Camera.still(), clip, still,
                      round((shot.frames - 1) * shot.still))
        if not self.args.keep_raw:
            rec.path.unlink(missing_ok=True)


# --- narration and timing ---------------------------------------------------------------


def say(msg: str):
    print(f"[video] {msg}", flush=True)


def read_narration() -> tuple[dict[str, list[tuple[str, str]]], dict[str, str]]:
    """The sentences of every scene as (subtitle, spoken) and the lexicon of
    [pronounce]. A line `subtitle | spoken` speaks other words than it shows."""
    scenes: dict[str, list[tuple[str, str]]] = {}
    lexicon: dict[str, str] = {}
    current = None
    for line in (HERE / "narration.txt").read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            current = line[1:-1]
            if current != "pronounce":
                scenes[current] = []
        elif current == "pronounce":
            word, _, phonemes = line.partition("=")
            lexicon[word.strip()] = phonemes.strip()
        elif current:
            shown, _, spoken = line.partition("|")
            scenes[current].append((shown.strip(), (spoken or shown).strip()))
    return scenes, lexicon


def duration(path: Path) -> float:
    out = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", str(path)],
                         capture_output=True, text=True, check=True)
    return float(out.stdout.strip())


def kokoro_models(given: str | None) -> Path:
    for folder in ([Path(given).expanduser()] if given else KOKORO_MODELS):
        if (folder / "kokoro-v1.0.onnx").exists() and (folder / "voices-v1.0.bin").exists():
            return folder
    raise SystemExit("Kokoro's model files are missing. Put kokoro-v1.0.onnx and voices-v1.0.bin into "
                     f"{KOKORO_MODELS[0]} or pass --kokoro-models (see kokoro_say.py).")


def kokoro_python(given: str | None) -> str | None:
    if given:
        return given
    return next((str(p) for p in KOKORO_PYTHONS if p.exists()), None)


def speak(audio: Path, python: str | None, models: str | None):
    """Speaks every sentence into its own WAV file. A sentence keeps its recording while
    its text and voice stay the same, because Kokoro varies a little from run to run and
    the shots must keep fitting."""
    scenes, lexicon = read_narration()
    audio.mkdir(parents=True, exist_ok=True)
    jobs = []
    for scene in SCENES:
        scene.sentences = []
        for i, (shown, spoken) in enumerate(scenes[scene.id]):
            path = audio / f"{scene.id}-{i + 1}.wav"
            used = {k: v for k, v in lexicon.items() if k.lower() in spoken.lower()}
            key = {"voice": KOKORO_VOICE, "speed": KOKORO_SPEED, "text": spoken, "lexicon": used}
            said = path.with_suffix(".json")
            if not (path.exists() and said.exists() and json.loads(said.read_text()) == key):
                jobs.append((path, said, key))
            scene.sentences.append((shown, path, 0.0))
    if jobs:
        if not python:
            raise SystemExit("Kokoro needs a Python with kokoro-onnx: pass --kokoro-python or set "
                             "MEERSH_VIDEO_KOKORO_PYTHON (see kokoro_say.py).")
        spec = {
            "models": str(kokoro_models(models)),
            "voice": KOKORO_VOICE,
            "speed": KOKORO_SPEED,
            "lexicon": lexicon,
            "jobs": [{"text": key["text"], "out": str(path)} for path, _, key in jobs],
        }
        spec_path = audio / "kokoro-jobs.json"
        stage.write_json(spec_path, spec)
        say(f"speaking {len(jobs)} sentences with Kokoro ({KOKORO_VOICE}, speed {KOKORO_SPEED})")
        out = subprocess.run([python, "-I", str(HERE / "kokoro_say.py"), str(spec_path)],
                             capture_output=True, text=True, check=True).stdout
        spoken_as = {json.loads(line)["out"]: json.loads(line)["phonemes"] for line in out.splitlines() if line.startswith("{")}
        for path, said, key in jobs:
            said.write_text(json.dumps(key, ensure_ascii=False))
            path.with_suffix(".phonemes").write_text(spoken_as.get(str(path), "") + "\n")
    for scene in SCENES:
        scene.sentences = [(shown, path, duration(path)) for shown, path, _ in scene.sentences]


def plan():
    """Lays out every scene shot by shot. A shot speaks its sentences with GAP seconds
    between them, ends SHOT_TAIL after its last sentence (the scene's tail for the last
    shot) and never runs shorter than its hold."""
    for scene in SCENES:
        scene.offsets = []
        t, i = 0.0, 0
        for n, shot in enumerate(scene.shots):
            start = t
            last = n == len(scene.shots) - 1
            cursor = start + (scene.lead if n == 0 else SHOT_LEAD)
            shot.cues = []
            for _ in range(shot.says):
                scene.offsets.append(cursor)
                shot.cues.append(cursor - start)
                cursor += scene.sentences[i][2] + GAP
                i += 1
            end = start + shot.hold
            if shot.says:
                end = max(end, cursor - GAP + (scene.tail if last else SHOT_TAIL))
            elif last:
                end = max(end, start + scene.tail)
            shot.start = start
            shot.frames = round((end - start) * FPS)
            t = start + shot.frames / FPS
        if i != len(scene.sentences):
            raise SystemExit(f"scene {scene.id} has {len(scene.sentences)} sentences in narration.txt, but its shots speak {i}")
        scene.frames = sum(s.frames for s in scene.shots)


def scene_starts() -> list[float]:
    starts, t = [], 0.0
    for scene in SCENES:
        starts.append(t)
        t += scene.frames / FPS - XFADE
    return starts


def total_seconds() -> float:
    return sum(s.frames for s in SCENES) / FPS - XFADE * (len(SCENES) - 1)


def storyboard() -> str:
    rows = ["| # | Scene | Shots | Seconds |", "| --- | --- | --- | --- |"]
    for i, (start, scene) in enumerate(zip(scene_starts(), SCENES), 1):
        shot_list = ", ".join(f"{s.id} ({s.frames / FPS:.1f} s)" for s in scene.shots)
        rows.append(f"| {i} | {scene.id} | {shot_list} | {start:.1f} to {start + scene.frames / FPS:.1f} |")
    return "\n".join(rows)


# --- sound, subtitles, the end card and the final video ----------------------------------


def narration_track(audio: Path) -> Path:
    """Every sentence at its place on one track."""
    rate = None
    total = total_seconds()
    samples = None
    for start, scene in zip(scene_starts(), SCENES):
        for offset, (_, path, _) in zip(scene.offsets, scene.sentences):
            with wave.open(str(path)) as f:
                if rate is None:
                    rate = f.getframerate()
                    samples = array.array("h", bytes(2 * int(total * rate + rate)))
                assert f.getframerate() == rate and f.getnchannels() == 1 and f.getsampwidth() == 2
                data = array.array("h", f.readframes(f.getnframes()))
            at_sample = int((start + offset) * rate)
            end = min(len(samples), at_sample + len(data))
            samples[at_sample:end] = data[: end - at_sample]
    path = audio / "narration.wav"
    with wave.open(str(path), "wb") as f:
        f.setnchannels(1)
        f.setsampwidth(2)
        f.setframerate(rate)
        f.writeframes(samples.tobytes())
    return path


def ass_time(t: float) -> str:
    cs = round(t * 100)
    return f"{cs // 360000}:{cs // 6000 % 60:02d}:{cs // 100 % 60:02d}.{cs % 100:02d}"


def subtitles(work: Path) -> Path:
    """Burned-in subtitles: white Noto Sans on a panel of the website's text colour
    (#1f2328), in the band below the terminal window."""
    head = f"""[Script Info]
ScriptType: v4.00+
PlayResX: {OUT_W}
PlayResY: {OUT_H}
WrapStyle: 0
ScaledBorderAndShadow: yes

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Meersh,Noto Sans SemiBold,40,&H00FFFFFF,&H00FFFFFF,&H1A28231F,&H00000000,0,0,0,0,100,100,0,0,3,14,0,2,180,180,26,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"""
    rows = []
    for start, scene in zip(scene_starts(), SCENES):
        if not scene.subtitles:
            continue
        for offset, (text, _, d) in zip(scene.offsets, scene.sentences):
            begin = start + offset - 0.05
            end = start + offset + d + 0.5
            rows.append(f"Dialogue: 0,{ass_time(begin)},{ass_time(end)},Meersh,,0,0,0,,{text}")
    path = work / "subtitles.ass"
    path.write_text(head + "\n".join(rows) + "\n")
    return path


NOTO = Path("/usr/share/fonts/google-noto")
BLUE = "#2185d0"  # the website's theme colour


def card(work: Path) -> tuple[Path, Path]:
    """The end card in two layers, white like the website: the meerkat, the name and the
    line of the website, then "Part of the Meerverse" and the address, which fade in
    later, as on the end cards of meertex and meerora."""
    from PIL import Image, ImageDraw, ImageFont

    first, second = work / "card.png", work / "card-address.png"
    bold = ImageFont.truetype(str(NOTO / "NotoSans-Bold.ttf"), 104)
    line = ImageFont.truetype(str(NOTO / "NotoSans-SemiBold.ttf"), 46)
    label = ImageFont.truetype(str(NOTO / "NotoSansMono-Medium.ttf"), 25)
    address = ImageFont.truetype(str(NOTO / "NotoSans-SemiBold.ttf"), 46)

    img = Image.new("RGBA", (OUT_W, OUT_H), (255, 255, 255, 240))
    meerkat = Image.open(ART / "meersh-full.png").convert("RGBA")
    height = 380
    meerkat = meerkat.resize((round(meerkat.width * height / meerkat.height), height), Image.LANCZOS)
    img.alpha_composite(meerkat, ((OUT_W - meerkat.width) // 2, 78))
    draw = ImageDraw.Draw(img)
    draw.text((OUT_W / 2, 476), "meersh", font=bold, fill="#1f2328", anchor="mt")
    a, b = "A shell for ", "the 21st century."
    wa, wb = draw.textlength(a, font=line), draw.textlength(b, font=line)
    x = (OUT_W - wa - wb) / 2
    draw.text((x, 626), a, font=line, fill="#1f2328", anchor="lt")
    draw.text((x + wa, 626), b, font=line, fill=BLUE, anchor="lt")
    img.save(first)

    img = Image.new("RGBA", (OUT_W, OUT_H), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    draw.rectangle(((OUT_W - 72) / 2, 730, (OUT_W + 72) / 2, 732), fill="#d1d9e0")
    text = "PART OF THE MEERVERSE"
    spaced = sum(draw.textlength(c, font=label) for c in text) + 3 * (len(text) - 1)
    x = (OUT_W - spaced) / 2
    for c in text:
        draw.text((x, 758), c, font=label, fill="#59636e", anchor="lt")
        x += draw.textlength(c, font=label) + 3
    draw.text((OUT_W / 2, 802), "github.com/ribalba/meersh", font=address, fill=BLUE, anchor="mt")
    img.save(second)
    return first, second


def assemble(work: Path, audio: Path, out: Path):
    say("putting the video together")
    total = total_seconds()
    starts = scene_starts()
    inputs: list[str] = []
    graph: list[str] = []
    k = 0
    for n, scene in enumerate(SCENES):
        labels = []
        for shot in scene.shots:
            inputs += ["-i", str(work / "clips" / f"{shot.id}.mkv")]
            labels.append(f"[{k}:v]")
            k += 1
        norm = f"fps={FPS},format=yuv444p,setsar=1,settb=AVTB"
        if len(labels) == 1:
            graph.append(f"{labels[0]}{norm}[s{n}]")
        else:
            graph.append(f"{''.join(labels)}concat=n={len(labels)}:v=1:a=0,{norm}[s{n}]")
    prev = "s0"
    for n in range(1, len(SCENES)):
        offset = sum(s.frames for s in SCENES[:n]) / FPS - XFADE * n
        graph.append(f"[{prev}][s{n}]xfade=transition=fade:duration={XFADE}:offset={offset:.4f}[x{n}]")
        prev = f"x{n}"
    (card_png, address_png), narration = card(work), narration_track(audio)
    loop = ["-loop", "1", "-framerate", str(FPS), "-t", f"{total:.3f}"]
    inputs += [*loop, "-i", str(card_png), *loop, "-i", str(address_png), "-i", str(narration)]
    card_in, address_in, narr_in = k, k + 1, k + 2
    ending = SCENES[-1]
    card_at = starts[-1] + ending.lead - 0.6
    # "Part of the Meerverse" comes in with the sentence that says it.
    said = next((i for i, (text, _, _) in enumerate(ending.sentences) if "Meerverse" in text), len(ending.offsets) - 1)
    address_at = starts[-1] + ending.offsets[said] - 0.3
    subs = subtitles(work)
    graph.append(f"[{card_in}:v]format=rgba,fade=t=in:st={card_at:.3f}:d=0.9:alpha=1[card]")
    graph.append(f"[{address_in}:v]format=rgba,fade=t=in:st={address_at:.3f}:d=0.8:alpha=1[address]")
    graph.append(
        f"[{prev}]format=rgba[base];[base][card]overlay=0:0:shortest=1[carded];[carded][address]overlay=0:0:shortest=1,"
        f"ass={subs}:fontsdir={NOTO},"
        f"fade=t=in:st=0:d=0.8,fade=t=out:st={total - 1.0:.3f}:d=1.0,format=yuv420p[v]"
    )
    # The voice alone. A pad of low sine tones under it came across as a hum.
    graph.append(
        f"[{narr_in}:a]aresample=48000,aformat=channel_layouts=stereo,apad,atrim=0:{total:.3f},"
        # Loudness of web video, about -17 LUFS, with the peaks kept below -1.5 dBTP.
        f"loudnorm=I=-17:TP=-1.5:LRA=11,aresample=48000,afade=t=out:st={total - 1.2:.3f}:d=1.2[a]"
    )
    tmp = out.with_name(out.stem + ".part.mp4")
    cmd = ["ffmpeg", "-v", "error", "-stats", "-y", *inputs, "-filter_complex", ";".join(graph), "-map", "[v]", "-map", "[a]"]
    cmd += ["-c:v", "libopenh264", "-profile:v", "high", "-b:v", "12M", "-maxrate", "18M", "-g", "60", "-pix_fmt", "yuv420p"]
    cmd += ["-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", "-t", f"{total:.3f}", str(tmp)]
    (work / "ffmpeg-assemble.txt").write_text(" ".join(cmd) + "\n")
    subprocess.run(cmd, check=True)
    tmp.rename(out)


def free_name(path: Path) -> Path:
    if not path.exists():
        return path
    for n in range(2, 100):
        candidate = path.with_name(f"{path.stem}-{n}{path.suffix}")
        if not candidate.exists():
            return candidate
    raise SystemExit(f"no free name next to {path}")


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--work", type=Path, default=Path.home() / ".cache" / "meersh-video",
                   help="folder for the demo home, the recordings, the clips, the audio and the stills "
                        "(~/.cache/meersh-video). It needs a few GB, so not a tmpfs")
    p.add_argument("--out", type=Path, default=Path.home() / "Videos" / "meersh-tour.mp4",
                   help="the video. An existing file is never replaced, -2 is added instead")
    p.add_argument("--only", help="film only these shots, comma separated, and skip the final video")
    p.add_argument("--reuse", action="store_true", help="keep the clips in WORK/clips and film only the missing shots")
    p.add_argument("--plan", action="store_true", help="speak the narration, print the storyboard and stop")
    p.add_argument("--meersh", type=Path, default=MEERSH, help=f"the meersh binary ({MEERSH.relative_to(REPO)})")
    p.add_argument("--kokoro-python", default=os.environ.get("MEERSH_VIDEO_KOKORO_PYTHON"),
                   help="a Python with kokoro-onnx, also MEERSH_VIDEO_KOKORO_PYTHON")
    p.add_argument("--kokoro-models", help=f"the folder with Kokoro's model files (default: {KOKORO_MODELS[0]})")
    p.add_argument("--port", type=int, default=8099, help="the port of the terminal page (8099)")
    p.add_argument("--keep-raw", action="store_true", help="keep the screen recordings in WORK/raw")
    args = p.parse_args()

    for tool in ("Xvfb", "ffmpeg", "ffprobe", "setsid", "git"):
        if not shutil.which(tool):
            raise SystemExit(f"{tool} is missing")
    if not args.meersh.exists():
        raise SystemExit(f"{args.meersh} is missing. Run cargo build --release in {REPO}.")
    if not (stage.XTERM / "lib" / "xterm.js").exists() or not (stage.WEBGL / "lib" / "addon-webgl.js").exists():
        raise SystemExit(f"xterm.js is missing. Run npm ci in {HERE}.")
    work = args.work.expanduser().resolve()
    for d in ("clips", "stills", "audio"):
        (work / d).mkdir(parents=True, exist_ok=True)
    say(f"working in {work}")

    speak(work / "audio", kokoro_python(args.kokoro_python), args.kokoro_models)
    plan()
    print(storyboard(), flush=True)
    say(f"{total_seconds():.1f} s in total")
    if args.plan:
        return

    all_shots = [(scene, shot) for scene in SCENES for shot in scene.shots]
    only = set(args.only.split(",")) if args.only else None
    unknown = (only or set()) - {s.id for _, s in all_shots}
    if unknown:
        raise SystemExit(f"no such shot: {', '.join(sorted(unknown))}")
    if args.reuse and not only:
        only = {s.id for _, s in all_shots if not (work / "clips" / f"{s.id}.mkv").exists()}
    todo = [(scene, shot) for scene, shot in all_shots if only is None or shot.id in only]
    # undo takes back what the actor shot trashed, so it needs that shot in the same take.
    ids = {shot.id for _, shot in todo}
    if "undo" in ids and "actor" not in ids:
        raise SystemExit("the undo shot needs the actor shot in the same run, add actor to --only")

    if todo:
        with sync_playwright() as pw:
            studio = Studio(args, work, pw)
            try:
                studio.start()
                for scene, shot in todo:
                    studio.film(scene, shot)
            finally:
                studio.stop()
                for note in studio.notes:
                    say(note)
    if args.only:
        return
    out = free_name(args.out.expanduser())
    out.parent.mkdir(parents=True, exist_ok=True)
    assemble(work, work / "audio", out)
    say(f"wrote {out} ({duration(out):.1f} s)")


if __name__ == "__main__":
    main()
