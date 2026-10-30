# The tour video

The website (`public/index.html`) shows a narrated tour of about three minutes, filmed
from the real shell. `make_video.py` films it and puts it together, and `publish.py`
turns it into the files of the page in `public/video/`. It works like the tour video of
meertex (`tools/video` in meertex), with the same voice.

Nothing in the video is drawn by hand except the end card and the frame of the terminal
window. The rest is real:

- The meersh release build in a pseudo terminal, shown by xterm.js in Google Chrome. Chrome
  runs headed in kiosk mode on a virtual screen (Xvfb), so nothing opens on your desktop.
- A made-up project in a home of its own (`~/.cache/meersh-video/home`), with file ages
  relative to the day of filming and a git history by three made-up people.
- Claude Code (`claude -p`, model haiku) for the `?` shorthand and for the model that acts
  on objects. A wrapper on the demo PATH runs it with your real home, so your login works
  and the demo home needs no copy of it. meersh starts it with `--safe-mode`, so your
  CLAUDE.md, hooks and MCP servers stay out of the answers.
- A coding agent session of `meersh mcp --agent claude`, driven over JSON-RPC before the
  first shot. It writes a file and asks to delete one of yours, which the policy stops.

## Running it

It needs these things once:

1. The meersh release build: `cargo build --release` in the repository.
2. ffmpeg with libopenh264, libass, libsvtav1 and libopus, Xvfb, git, Google Chrome, node
   and a logged-in Claude Code.
3. xterm.js and its WebGL renderer for the terminal window: `npm ci` in this folder.
4. A virtualenv with Playwright and Pillow. meertex's `~/.cache/meertex-pw` works too.

   ```sh
   python3 -m venv ~/.cache/meersh-pw
   ~/.cache/meersh-pw/bin/pip install playwright pillow
   ```

5. Kokoro for the voice: a virtualenv with kokoro-onnx, the system's espeak-ng, and
   `kokoro-v1.0.onnx` and `voices-v1.0.bin` from the kokoro-onnx GitHub releases
   (model-files-v1.0) in `~/.cache/kokoro`. meerora's copy in
   `~/.cache/meerora/voice/kokoro` is found too, and so is the virtualenv
   `~/.cache/meertex-kokoro` of meertex.

   ```sh
   python3 -m venv ~/.cache/meersh-kokoro
   ~/.cache/meersh-kokoro/bin/pip install kokoro-onnx
   ```

Then, from the repository's root:

```sh
~/.cache/meersh-pw/bin/python website/video/make_video.py           # writes ~/Videos/meersh-tour.mp4
~/.cache/meersh-pw/bin/python website/video/publish.py ~/Videos/meersh-tour.mp4
```

A run films in real time and then cuts every frame, so it takes about ten minutes, and
`publish.py` a few more. The work folder `~/.cache/meersh-video` keeps the demo home, the
spoken sentences, one lossless clip per shot and a still from every shot. It needs a few
GB while it films, so it must not be on a tmpfs.

| Option | |
| --- | --- |
| `--only ls,ask` | Film only these shots and stop, to look at their clips and stills. `undo` needs `actor` in the same run. |
| `--reuse` | Film only the shots that have no clip yet, then put the video together. |
| `--plan` | Speak the narration, print the storyboard with the times and stop. |
| `--out FILE` | Another file for the video. An existing file is never replaced, `-2` is added instead. |
| `--work DIR` | Another work folder. |
| `--meersh FILE` | Another meersh binary. |
| `--keep-raw` | Keep the screen recordings in `WORK/raw`. |
| `--port N` | Another port for the terminal page (8099). |

`publish.py` takes `--poster SHOT` for another poster, `--mp4-rate` and `--no-webm`.

## The story

`narration.txt` holds the sentences, one scene per section, and how Kokoro says meersh.
`make_video.py` holds the shots. Each shot is one take. `prepare()` sets the terminal up,
the recorder starts, and `act()` types while it runs, timed by the shot's sentences. A
shot is as long as its sentences with 1.3 seconds between them, and never shorter than
its hold.

| Scene | What the narration says | What the terminal shows |
| --- | --- | --- |
| intro | Commands pass objects, columns have types. | `ls` |
| objects | Filters work on columns, sizes and dates compare as values. | `ls \| where size > 1mb \| sort-by size -r` and a filter on the date |
| methods | Objects carry their identity and methods. | `$f.hash()` on a file and `$p.kill()` on the devserver process |
| legacy | Other programs run with `^`, adapters make records of their output. | `^git log` through the git adapter |
| ask | The `?` shorthand writes a meersh pipeline from plain words. | `? directories without changes for six months`, the proposal, and its result after `y` |
| actor | A model acts on objects, sees only their type, nothing happens without approval. | `ls \| llm "delete all files that are larger than 5mb"`, the plan and `y` |
| undo | The trash and the journal, undo for the model's actions. | `journal` and `journal \| where principal == actor \| undo` |
| roles | A new security model. meersh asks who wrote a command, and every role has its own rights. | `policy explain` for deleting `notes.md` as user, script, plan, actor and agent |
| ownership | Rights follow ownership. A model may delete what the session created, must ask for your files and is refused for system paths. | `touch draft.txt`, then `policy explain` for `draft.txt`, `notes.md` and `/etc/hostname` |
| confine | Landlock holds programs to the folders a `with` block allows. | `with {fs.write: ./build} { ^sh -c "..." }` writes `build/log.txt` and is refused for `notes.md` |
| agents | Coding agents use meersh over MCP under the same rules. | `ledger` with the file the agent wrote |
| ending | "A shell for the 21st century", the Meerverse, GitHub. | The end card over an empty prompt. |

The `?` and actor shots each run their command once before the recorder starts and
decline it. The answer then sits in meersh's response cache, so the take does not wait
for the model and shows the same answer it got a moment before.

## How it is made

- The browser lays the page out at 1536 by 864 CSS pixels and paints it at 5/3 the
  density, 2560 by 1440, and the video scales that to 1920 by 1080. The camera does not
  move, because text in motion is hard to read. Instead the terminal window is large
  and its font is 19 CSS pixels, about 24 pixels in the video. `stage.Camera` could
  still ease between keyframes, as in the meertex video.
- x11grab stamps the first frame with the wall clock, so each recording is cut at the
  moment its action started.
- The terminal is xterm.js with its WebGL renderer, which draws the box drawing
  characters of meersh tables itself, so the frames join. meersh's line editor asks the
  terminal for the cursor position, and xterm.js's answer goes back into the pseudo
  terminal through the page server (`terminal.html`, `stage.Terminal`).
- The narration is Kokoro v1.0 (82M, Apache 2.0), voice `af_heart` at speed 0.95, one WAV
  file per sentence. A sentence keeps its recording while its text stays the same.
- The sound is the voice alone, normalised to about -17 LUFS.
- Subtitles are burned in with libass, white Noto Sans on a panel of the website's text
  colour, in the band below the terminal window. The ending has none, the end card
  carries the text.
- The clips are lossless (FFV1). The video is H.264 at 12 Mbit/s with AAC, and
  `publish.py` makes the page's smaller AV1 and H.264 versions from it.
