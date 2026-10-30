# The meersh website

The marketing site. A static page and a stylesheet, served by nginx. Nothing to build.

```bash
cd website
docker compose -f docker-compose.dev.yml up --build -d      # http://127.0.0.1:8085
```

The landing styles are the meercal site's, which are the meerail site's, so the sites read
as one family. Keep the shared part of `public/css/site.css` identical to theirs and put
anything specific under the `===== meersh additions =====` heading at the foot of the file,
so a fix to one site can be copied to the others without a diff to read.

## Terminal windows

A shell has no screenshots worth the name, so the page shows terminal windows. Their content
is real output, captured from a running meersh, not written by hand.

```bash
cargo build --release
python3 terminal/capture.py --meersh ../target/release/meersh          # no model needed
python3 terminal/capture.py --meersh ../target/release/meersh --llm    # also the actor, needs Ollama
```

The script builds a small demo project with fixed file dates in a temporary directory, runs
the commands with their own ledger, cache and trash, and writes the output between the
`<!-- capture:<name> -->` markers of `public/index.html`.

## Images

The sources are in `art/`. `meersh-full.png` is the meerkat holding a terminal. It is the hero
image and part of the social preview. `meersh-peek.png` is the meerkat peeking over a terminal. It
is the square logo in the top bar, the favicon and the logo in the repository README. `og.svg` holds
the text of the social preview. The files in `public/img` are rendered from them.

```bash
cd public/img
magick ../../art/meersh-full.png -resize 640x -strip logo.png
magick ../../art/meersh-peek.png -resize 512x512 -strip logo-square.png
for s in 32 64 180; do magick ../../art/meersh-peek.png -resize ${s}x${s} -strip favicon-$s.png; done
rsvg-convert -w 1200 -h 630 ../../art/og.svg -o og.png
magick og.png \( ../../art/meersh-full.png -resize x480 \) -geometry +90+40 -composite -strip og.png
```

## Tour video

The tour in `public/video/` is filmed from the real shell by `video/make_video.py` and
built for the page by `video/publish.py`. [video/README.md](video/README.md) says what
it needs and how a run works.

## Deploying

`docker-compose.yml` is for Coolify, the way the meerpic and meerora sites are deployed. The
image is plain nginx with `public/` copied in, so anything that runs a container will do.

```bash
docker build -t meersh-website .
docker run -p 80:80 meersh-website
```

Put TLS in front of it. Nothing here is dynamic and nothing is stored, so the container is
disposable. The other sites load an umami script for statistics. This one has none until a
website id exists for it.
